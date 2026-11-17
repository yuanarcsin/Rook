//! 任务展示顺序（C7）：只改 `[[tasks]]` 的 `order` 字段，保留注释与其他键。
//!
//! 语义：
//! - 传入的 `ids` 必须与配置文件里现有任务的 id **集合完全一致**（顺序即目标顺序）；
//!   重复、缺失、多余一律拒绝，不写盘——排序不能顺带增删任务；
//! - 每个任务表的 `order` 写成 `0..n`（与 `ids` 下标一致），其他字段/注释/空行原样保留；
//! - 冲突检测与原子保存复用 [`super::editor`]：读到的 revision 与 `expected_revision`
//!   不一致直接返回 [`ReorderError::Conflict`]，绝不写文件；
//! - 渲染结果与磁盘字节相同 → `changed = false`，不写盘、不触发重载。
//!
//! 快照按 `order` 稳定排序（见 [`crate::snapshot::Snapshot::from_loaded`]），
//! 所以写盘后的顺序才是界面上看到的顺序。

use std::collections::{BTreeSet, HashMap};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use toml_edit::{DocumentMut, Item, value};

use super::editor::{self, EditorError, SaveOutcome};

/// 排序失败的原因；`stage` 与命令错误 DTO 的取值一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReorderError {
    /// 文件已被外部修改：`current_revision` 是磁盘上的最新 revision。
    Conflict { current_revision: String },
    /// 请求不合法：id 集合与配置文件不一致、`tasks` 不是数组表等。
    Invalid { message: String },
    /// 现有配置文件无法解析（语法错误），带行/列。
    Parse {
        path: PathBuf,
        message: String,
        line: Option<usize>,
        col: Option<usize>,
    },
    /// 读/写文件失败。
    Write { path: PathBuf, message: String },
}

impl ReorderError {
    /// 稳定英文阶段标识：`conflict` / `invalid` / `parse` / `write`。
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Conflict { .. } => "conflict",
            Self::Invalid { .. } => "invalid",
            Self::Parse { .. } => "parse",
            Self::Write { .. } => "write",
        }
    }
}

impl fmt::Display for ReorderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid { message } | Self::Write { message, .. } => f.write_str(message),
            Self::Parse {
                path,
                message,
                line,
                col,
            } => {
                if !path.as_os_str().is_empty() {
                    write!(f, "{}: ", path.display())?;
                }
                if let (Some(line), Some(col)) = (line, col) {
                    write!(f, "{line}:{col}: ")?;
                }
                f.write_str(message)
            }
            Self::Conflict { current_revision } => {
                write!(f, "文件已被外部修改（当前 revision {current_revision}）")
            }
        }
    }
}

impl std::error::Error for ReorderError {}

impl From<EditorError> for ReorderError {
    fn from(error: EditorError) -> Self {
        match error {
            EditorError::Conflict { current_revision } => Self::Conflict { current_revision },
            EditorError::Parse {
                path,
                message,
                line,
                col,
            } => Self::Parse {
                path,
                message,
                line,
                col,
            },
            EditorError::Read { path, message } | EditorError::Write { path, message } => {
                Self::Write { path, message }
            }
            // 只有恢复备份会产生该错误；排序路径不会走到这里。
            EditorError::BackupMissing { path } => Self::Write {
                message: format!("没有可用的配置备份：{}", path.display()),
                path,
            },
        }
    }
}

/// 按 `ids` 的顺序重写所有任务的 `order`。
pub fn reorder_tasks(
    path: &Path,
    ids: &[String],
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<SaveOutcome, ReorderError> {
    editor::with_save_lock(|| {
        let bytes = fs::read(path).map_err(|error| read_error(path, error))?;
        let revision = editor::revision_of(&bytes);
        if revision != expected_revision {
            return Err(ReorderError::Conflict {
                current_revision: revision,
            });
        }
        let text = String::from_utf8(bytes.clone()).map_err(|error| {
            read_error(
                path,
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("配置文件必须是 UTF-8 编码：{error}"),
                ),
            )
        })?;
        let mut doc = text
            .parse::<DocumentMut>()
            .map_err(|error| ReorderError::Parse {
                path: path.to_path_buf(),
                message: error.message().to_owned(),
                line: error
                    .span()
                    .map(|span| super::task_draft::line_col(&text, span.start).0),
                col: error
                    .span()
                    .map(|span| super::task_draft::line_col(&text, span.start).1),
            })?;

        let existing = existing_ids(&doc)?;
        let positions = order_positions(&existing, ids)?;
        apply_order(&mut doc, &positions)?;

        let rendered = doc.to_string();
        if rendered.as_bytes() == bytes.as_slice() {
            return Ok(SaveOutcome {
                revision,
                changed: false,
                issues: Vec::new(),
            });
        }

        let (issues, revision) = editor::commit(path, rendered.as_bytes(), data_dir)?;
        Ok(SaveOutcome {
            revision,
            changed: true,
            issues,
        })
    })
}

fn read_error(path: &Path, error: std::io::Error) -> ReorderError {
    ReorderError::Write {
        path: path.to_path_buf(),
        message: format!("读取配置文件 {} 失败：{error}", path.display()),
    }
}

fn invalid(message: impl Into<String>) -> ReorderError {
    ReorderError::Invalid {
        message: message.into(),
    }
}

/// 配置文件里现有任务的 id（保持文件顺序）。
fn existing_ids(doc: &DocumentMut) -> Result<Vec<String>, ReorderError> {
    let Some(item) = doc.as_table().get("tasks") else {
        return Ok(Vec::new());
    };
    let aot = item
        .as_array_of_tables()
        .ok_or_else(|| invalid("tasks 不是 [[tasks]] 数组表，无法调整顺序"))?;
    aot.iter()
        .map(|table| {
            table
                .get("id")
                .and_then(Item::as_str)
                .map(str::to_owned)
                .ok_or_else(|| invalid("有任务缺少 id，无法调整顺序"))
        })
        .collect()
}

/// 校验 `ids` 与现有集合一致，返回 `id -> 目标下标`。
fn order_positions(
    existing: &[String],
    ids: &[String],
) -> Result<HashMap<String, usize>, ReorderError> {
    let mut duplicates: Vec<&str> = Vec::new();
    let mut seen: BTreeSet<&str> = BTreeSet::new();
    for id in ids {
        if !seen.insert(id.as_str()) && !duplicates.contains(&id.as_str()) {
            duplicates.push(id.as_str());
        }
    }
    if !duplicates.is_empty() {
        return Err(invalid(format!(
            "排序列表里有重复的任务 id：{}",
            duplicates.join("、")
        )));
    }

    let requested: BTreeSet<&str> = ids.iter().map(String::as_str).collect();
    let current: BTreeSet<&str> = existing.iter().map(String::as_str).collect();
    let missing: Vec<&str> = current.difference(&requested).copied().collect();
    let extra: Vec<&str> = requested.difference(&current).copied().collect();
    if !missing.is_empty() || !extra.is_empty() {
        let mut parts = Vec::new();
        if !missing.is_empty() {
            parts.push(format!("缺少 {}", missing.join("、")));
        }
        if !extra.is_empty() {
            parts.push(format!("多出 {}", extra.join("、")));
        }
        return Err(invalid(format!(
            "排序列表与配置文件中的任务不一致：{}",
            parts.join("；")
        )));
    }

    Ok(ids
        .iter()
        .enumerate()
        .map(|(position, id)| (id.clone(), position))
        .collect())
}

/// 按目标下标写 `order`；只动这一个键，注释与其他字段保持原样。
fn apply_order(
    doc: &mut DocumentMut,
    positions: &HashMap<String, usize>,
) -> Result<(), ReorderError> {
    let aot = doc
        .as_table_mut()
        .get_mut("tasks")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| invalid("tasks 不是 [[tasks]] 数组表，无法调整顺序"))?;
    for table in aot.iter_mut() {
        let Some(id) = table.get("id").and_then(Item::as_str) else {
            return Err(invalid("有任务缺少 id，无法调整顺序"));
        };
        let Some(position) = positions.get(id) else {
            return Err(invalid(format!("任务 {id} 不在排序列表里")));
        };
        table["order"] = value(i64::try_from(*position).unwrap_or(i64::MAX));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时目录 + 配置文件 + 独立数据目录。
    struct Fx {
        _tmp: tempfile::TempDir,
        path: PathBuf,
        data_dir: PathBuf,
    }

    impl Fx {
        fn new(source: &str) -> Self {
            let tmp = tempfile::tempdir().expect("创建临时目录");
            let root = tmp.path().to_path_buf();
            let data_dir = root.join("data");
            fs::create_dir_all(&data_dir).expect("创建数据目录");
            let path = root.join("Rook.toml");
            fs::write(&path, source).expect("写配置文件");
            Self {
                _tmp: tmp,
                path,
                data_dir,
            }
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.path).expect("读配置文件")
        }

        fn revision(&self) -> String {
            editor::revision_of(&fs::read(&self.path).expect("读配置文件"))
        }

        fn reorder(&self, ids: &[&str], expected: &str) -> Result<SaveOutcome, ReorderError> {
            let ids: Vec<String> = ids.iter().map(|id| (*id).to_owned()).collect();
            reorder_tasks(&self.path, &ids, expected, Some(&self.data_dir))
        }

        fn load(&self) -> super::super::LoadedConfig {
            super::super::load(&super::super::Overrides {
                config: Some(&self.path),
                data_dir: Some(&self.data_dir),
            })
            .expect("配置应可加载")
        }

        /// 配置目录里残留的临时文件（保存流程必须清理干净）。
        fn temps(&self) -> Vec<String> {
            fs::read_dir(self.path.parent().expect("父目录"))
                .expect("列目录")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect()
        }
    }

    /// 三个任务 + 顶层/任务注释 + 既有 order。
    const THREE_TASKS: &str = r#"# 顶层注释：Rook
version = 1

# 任务 A：网关
[[tasks]]
id = "alpha"
name = "甲"        # 行内注释
command = "cmd.exe"
order = 2

# 任务 B：同步
[[tasks]]
id = "beta"
name = "乙"
command = "cmd.exe"

# 任务 C：报表
[[tasks]]
id = "gamma"
name = "丙"
command = "cmd.exe"
order = 0
"#;

    #[test]
    fn reorder_persists_order_and_keeps_comments() {
        let fx = Fx::new(THREE_TASKS);
        let outcome = fx
            .reorder(&["gamma", "alpha", "beta"], &fx.revision())
            .expect("排序应成功");
        assert!(outcome.changed);
        assert_eq!(outcome.revision, fx.revision());

        let after = fx.read();
        for comment in [
            "# 顶层注释：Rook",
            "# 任务 A：网关",
            "# 任务 B：同步",
            "# 任务 C：报表",
            "name = \"甲\"        # 行内注释",
        ] {
            assert!(after.contains(comment), "注释必须保留：{comment}\n{after}");
        }

        // 每个任务都写上了 0..n，且与目标顺序一致。
        let loaded = fx.load();
        assert_eq!(loaded.config.task("gamma").expect("gamma").order, 0);
        assert_eq!(loaded.config.task("alpha").expect("alpha").order, 1);
        assert_eq!(loaded.config.task("beta").expect("beta").order, 2);

        // 快照按 order 展示：重启后仍是同一顺序。
        let snapshot = crate::snapshot::Snapshot::from_loaded(&loaded);
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["gamma", "alpha", "beta"]);
    }

    #[test]
    fn reorder_to_current_order_is_noop() {
        let fx = Fx::new(THREE_TASKS);
        // 现有文件里 gamma 已是最小 order，其余同文件顺序。
        let before = fx.read();
        let outcome = fx
            .reorder(&["gamma", "alpha", "beta"], &fx.revision())
            .expect("首次排序成功");
        assert!(outcome.changed);
        let after = fx.read();
        assert_ne!(after, before);

        let outcome = fx
            .reorder(&["gamma", "alpha", "beta"], &outcome.revision)
            .expect("同序再排应 no-op");
        assert!(!outcome.changed, "内容未变必须 no-op");
        assert_eq!(fx.read(), after);
    }

    #[test]
    fn duplicate_ids_are_rejected() {
        let fx = Fx::new(THREE_TASKS);
        let before = fx.read();
        let error = fx
            .reorder(&["alpha", "alpha", "beta"], &fx.revision())
            .expect_err("重复 id 应拒绝");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("alpha"), "{error}");
        assert_eq!(fx.read(), before);
    }

    #[test]
    fn missing_and_extra_ids_report_difference() {
        let fx = Fx::new(THREE_TASKS);
        let before = fx.read();
        let error = fx
            .reorder(&["alpha", "beta", "delta"], &fx.revision())
            .expect_err("集合不一致应拒绝");
        assert_eq!(error.stage(), "invalid");
        let message = error.to_string();
        assert!(message.contains("gamma"), "{message}");
        assert!(message.contains("delta"), "{message}");
        assert_eq!(fx.read(), before, "拒绝时不能写盘");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn conflict_does_not_write() {
        let fx = Fx::new(THREE_TASKS);
        let opened = fx.revision();
        let external = format!("{THREE_TASKS}\n# external edit\n");
        fs::write(&fx.path, &external).expect("外部写入");
        let external_revision = fx.revision();
        assert_ne!(opened, external_revision);

        let error = fx
            .reorder(&["beta", "alpha", "gamma"], &opened)
            .expect_err("应冲突");
        match error {
            ReorderError::Conflict { current_revision } => {
                assert_eq!(current_revision, external_revision);
            }
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
        assert_eq!(fx.read(), external, "外部修改必须完好保留");
    }
}
