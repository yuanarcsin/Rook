//! 每周运行时段的专用编辑器（2026-11-04）。
//!
//! 与 `task_draft` 分开：结构化任务表单暂不承载可变的时段窗口，UI 用
//! `get_schedule/save_schedule` 单独编辑 `[tasks.schedule]`。保存仍复用
//! 文本编辑器的 revision 冲突检测、临时文件校验与原子替换语义，确保：
//! - 只改目标任务里的 `schedule` 表，保留注释与其他任务；
//! - 结构错误不落盘；
//! - 内容未变时 no-op。
//!
//! 本模块不依赖 Tauri；命令层负责从 Engine 取路径/快照。

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, Item, Table, value};

use super::ConfigIssue;
use super::editor::{self, EditorError};
use super::model::ScheduleConfig;
use crate::schedule::{Weekday, format_time, parse_time};

/// 运行时段草稿（JSON camelCase）。
///
/// v2（2026-11-15）：删除已废弃的 `start_in_window` / `stop_outside`；旧载荷里
/// 多余的同名键会被 serde 忽略，不影响保存。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleDraft {
    pub enabled: bool,
    pub windows: Vec<ScheduleWindowDraft>,
}

/// 一个运行时段草稿。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleWindowDraft {
    /// `mon`..`sun`。
    pub days: Vec<String>,
    /// `HH:MM`。
    pub start: String,
    /// `HH:MM`。
    pub end: String,
}

impl ScheduleDraft {
    /// 从已校验的配置生成草稿。
    pub fn from_config(schedule: &ScheduleConfig) -> Self {
        Self {
            enabled: schedule.enabled,
            windows: schedule
                .windows
                .iter()
                .map(|window| ScheduleWindowDraft {
                    days: window
                        .days
                        .iter()
                        .map(|day| day.as_str().to_owned())
                        .collect(),
                    start: format_time(window.start),
                    end: format_time(window.end),
                })
                .collect(),
        }
    }

    /// 校验草稿并转换为配置模型；错误消息面向用户。
    pub fn validate(&self) -> Result<(), String> {
        if self.windows.len() > 64 {
            return Err("运行时段最多 64 段".to_owned());
        }
        for (index, window) in self.windows.iter().enumerate() {
            let label = format!("第 {} 段", index + 1);
            let mut days = Vec::with_capacity(window.days.len());
            for raw in &window.days {
                let Some(day) = Weekday::parse(raw) else {
                    return Err(format!(
                        "{label} 的星期 \"{raw}\" 非法（只接受 mon/tue/wed/thu/fri/sat/sun）"
                    ));
                };
                if !days.contains(&day) {
                    days.push(day);
                }
            }
            if days.is_empty() {
                return Err(format!("{label} 至少选择一天"));
            }
            let Some(start) = parse_time(&window.start) else {
                return Err(format!("{label} 的开始时间非法（HH:MM）"));
            };
            let Some(end) = parse_time(&window.end) else {
                return Err(format!("{label} 的结束时间非法（HH:MM）"));
            };
            if start == end {
                return Err(format!(
                    "{label} 的开始与结束不能相同（跨午夜请让 end 早于 start）"
                ));
            }
        }
        if self.enabled && self.windows.is_empty() {
            return Err("启用运行时段时至少需要一段".to_owned());
        }
        Ok(())
    }

    fn is_default(&self) -> bool {
        !self.enabled && self.windows.is_empty()
    }
}

/// 保存成功的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SaveScheduleOutcome {
    pub revision: String,
    pub changed: bool,
    pub issues: Vec<ConfigIssue>,
}

/// 编辑器错误；stage 与命令 DTO 对齐。
#[derive(Debug)]
pub enum ScheduleEditError {
    Read {
        path: PathBuf,
        message: String,
    },
    Write {
        path: PathBuf,
        message: String,
    },
    Parse {
        message: String,
        line: Option<usize>,
        col: Option<usize>,
    },
    Conflict {
        current_revision: String,
    },
    InvalidTask {
        task_id: String,
        message: String,
    },
    Invalid {
        message: String,
    },
}

impl ScheduleEditError {
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Parse { .. } => "parse",
            Self::Conflict { .. } => "conflict",
            Self::InvalidTask { .. } => "invalid_task",
            Self::Invalid { .. } => "invalid",
        }
    }

    pub fn current_revision(&self) -> Option<&str> {
        match self {
            Self::Conflict { current_revision } => Some(current_revision),
            _ => None,
        }
    }

    pub fn line_col(&self) -> (Option<usize>, Option<usize>) {
        match self {
            Self::Parse { line, col, .. } => (*line, *col),
            _ => (None, None),
        }
    }
}

impl fmt::Display for ScheduleEditError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { path, message } => {
                write!(f, "读取配置文件 {} 失败：{message}", path.display())
            }
            Self::Write { path, message } => {
                write!(f, "写入配置文件 {} 失败：{message}", path.display())
            }
            Self::Parse { message, .. } => f.write_str(message),
            Self::Conflict { current_revision } => {
                write!(f, "文件已被外部修改（当前 revision {current_revision}）")
            }
            Self::InvalidTask { task_id, message } => {
                write!(f, "任务 {task_id} 不存在或不可编辑：{message}")
            }
            Self::Invalid { message } => f.write_str(message),
        }
    }
}

impl std::error::Error for ScheduleEditError {}

impl From<EditorError> for ScheduleEditError {
    fn from(error: EditorError) -> Self {
        match error {
            EditorError::Read { path, message } => Self::Read { path, message },
            EditorError::Write { path, message } => Self::Write { path, message },
            EditorError::Parse {
                message, line, col, ..
            } => Self::Parse { message, line, col },
            EditorError::Conflict { current_revision } => Self::Conflict { current_revision },
            // 只有恢复备份会产生该错误；保存路径不会走到这里。
            EditorError::BackupMissing { path } => Self::Read {
                message: format!("没有可用的配置备份：{}", path.display()),
                path,
            },
        }
    }
}

/// 读取配置文件文本并返回 revision（供 get_schedule 填 DTO）。
pub fn config_revision(path: &Path) -> Result<String, ScheduleEditError> {
    editor::read_config_text(path)
        .map(|view| view.revision)
        .map_err(Into::into)
}

/// 保存某个任务的运行时段。`draft` 已由调用方转换为 UI 草稿。
pub fn save_schedule(
    config_path: &Path,
    task_id: &str,
    draft: &ScheduleDraft,
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<SaveScheduleOutcome, ScheduleEditError> {
    draft
        .validate()
        .map_err(|message| ScheduleEditError::Invalid { message })?;

    editor::with_save_lock(|| {
        let current = fs::read(config_path).map_err(|error| ScheduleEditError::Read {
            path: config_path.to_path_buf(),
            message: error.to_string(),
        })?;
        let current_revision = editor::revision_of(&current);
        if current_revision != expected_revision {
            return Err(ScheduleEditError::Conflict { current_revision });
        }
        let text = String::from_utf8(current.clone()).map_err(|error| ScheduleEditError::Read {
            path: config_path.to_path_buf(),
            message: format!("配置文件必须是 UTF-8 编码：{error}"),
        })?;
        let mut doc = text
            .parse::<DocumentMut>()
            .map_err(|error| ScheduleEditError::Parse {
                message: format!("配置文件不是合法 TOML：{error}"),
                line: None,
                col: None,
            })?;
        let task =
            find_task_mut(&mut doc, task_id).ok_or_else(|| ScheduleEditError::InvalidTask {
                task_id: task_id.to_owned(),
                message: "配置文件中找不到该任务".to_owned(),
            })?;
        if draft.is_default() {
            if task.get("schedule").is_none() {
                return Ok(SaveScheduleOutcome {
                    revision: current_revision,
                    changed: false,
                    issues: Vec::new(),
                });
            }
            task.remove("schedule");
        } else {
            task.insert("schedule", Item::Table(build_schedule_table(draft)));
        }
        let next = doc.to_string();
        if next.as_bytes() == current.as_slice() {
            return Ok(SaveScheduleOutcome {
                revision: current_revision,
                changed: false,
                issues: Vec::new(),
            });
        }
        let (issues, revision) = editor::commit(config_path, next.as_bytes(), data_dir)?;
        Ok(SaveScheduleOutcome {
            revision,
            changed: true,
            issues,
        })
    })
}

fn find_task_mut<'a>(doc: &'a mut DocumentMut, task_id: &str) -> Option<&'a mut Table> {
    let tasks = doc.get_mut("tasks")?.as_array_of_tables_mut()?;
    tasks
        .iter_mut()
        .find(|task| task.get("id").and_then(Item::as_str) == Some(task_id))
}

fn build_schedule_table(draft: &ScheduleDraft) -> Table {
    let mut table = Table::new();
    table.insert("enabled", value(draft.enabled));
    let mut windows = ArrayOfTables::new();
    for window in &draft.windows {
        let mut item = Table::new();
        let mut days = Array::new();
        for day in &window.days {
            days.push(day.as_str());
        }
        item.insert("days", value(days));
        item.insert("start", value(window.start.as_str()));
        item.insert("end", value(window.end.as_str()));
        windows.push(item);
    }
    table.insert("windows", Item::ArrayOfTables(windows));
    table
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fx {
        _tmp: tempfile::TempDir,
        path: PathBuf,
        data_dir: PathBuf,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("临时目录");
            let root = tmp.path().to_path_buf();
            let data_dir = root.join("data");
            fs::create_dir_all(&data_dir).expect("数据目录");
            let path = root.join("Rook.toml");
            fs::write(
                &path,
                "version = 1\n# 保留这句注释\n\n[[tasks]]\nid = \"alpha\"\nname = \"甲\"\ncommand = \"cmd.exe\"\nworking_dir = \".\"\n\n[[tasks]]\nid = \"beta\"\nname = \"乙\"\ncommand = \"cmd.exe\"\nworking_dir = \".\"\n",
            )
            .expect("写配置");
            Self {
                _tmp: tmp,
                path,
                data_dir,
            }
        }

        fn revision(&self) -> String {
            editor::revision_of(&fs::read(&self.path).expect("读配置"))
        }

        fn save(
            &self,
            draft: &ScheduleDraft,
            revision: &str,
        ) -> Result<SaveScheduleOutcome, ScheduleEditError> {
            save_schedule(&self.path, "alpha", draft, revision, Some(&self.data_dir))
        }
    }

    fn draft() -> ScheduleDraft {
        ScheduleDraft {
            enabled: true,
            windows: vec![ScheduleWindowDraft {
                days: vec!["mon".to_owned(), "wed".to_owned()],
                start: "07:50".to_owned(),
                end: "21:30".to_owned(),
            }],
        }
    }

    #[test]
    fn save_writes_schedule_and_preserves_comments_and_other_tasks() {
        let fx = Fx::new();
        let revision = fx.revision();
        let outcome = fx.save(&draft(), &revision).expect("保存");
        assert!(outcome.changed);
        assert_ne!(outcome.revision, revision);

        let text = fs::read_to_string(&fx.path).expect("读配置");
        assert!(text.contains("# 保留这句注释"), "注释必须保留\n{text}");
        assert!(text.contains("id = \"beta\""), "其他任务必须保留");
        let doc = text.parse::<DocumentMut>().expect("TOML");
        let tasks = doc
            .get("tasks")
            .and_then(Item::as_array_of_tables)
            .expect("tasks");
        let alpha = tasks
            .iter()
            .find(|task| task.get("id").and_then(Item::as_str) == Some("alpha"))
            .expect("alpha");
        let schedule = alpha
            .get("schedule")
            .and_then(Item::as_table)
            .expect("schedule");
        assert_eq!(schedule.get("enabled").and_then(Item::as_bool), Some(true));
        // v2：废弃字段不再写盘。
        assert!(!schedule.contains_key("start_in_window"));
        assert!(!schedule.contains_key("stop_outside"));
        let windows = schedule
            .get("windows")
            .and_then(Item::as_array_of_tables)
            .expect("windows");
        assert_eq!(windows.len(), 1);
        let days = windows
            .get(0)
            .and_then(|window| window.get("days"))
            .and_then(Item::as_array)
            .expect("days");
        let days: Vec<_> = days.iter().filter_map(|value| value.as_str()).collect();
        assert_eq!(days, vec!["mon", "wed"]);
    }

    #[test]
    fn identical_draft_is_noop() {
        let fx = Fx::new();
        let first = fx.save(&draft(), &fx.revision()).expect("首次保存");
        let second = fx.save(&draft(), &first.revision).expect("二次保存");
        assert!(!second.changed);
        assert_eq!(second.revision, first.revision);
    }

    #[test]
    fn default_draft_removes_schedule_table() {
        let fx = Fx::new();
        let first = fx.save(&draft(), &fx.revision()).expect("先写入");
        let default = ScheduleDraft::default();
        let second = fx.save(&default, &first.revision).expect("再删除");
        assert!(second.changed);
        let text = fs::read_to_string(&fx.path).expect("读配置");
        assert!(!text.contains("[tasks.schedule]"), "{text}");
        assert!(text.contains("# 保留这句注释"));
    }

    #[test]
    fn conflict_refuses_to_write() {
        let fx = Fx::new();
        let revision = fx.revision();
        let external = format!(
            "{}\n# 外部修改\n",
            fs::read_to_string(&fx.path).expect("读配置")
        );
        fs::write(&fx.path, &external).expect("外部写入");
        let error = fx.save(&draft(), &revision).expect_err("应当冲突");
        assert_eq!(error.stage(), "conflict");
        assert!(error.current_revision().is_some());
        assert_eq!(fs::read_to_string(&fx.path).expect("读配置"), external);
    }

    #[test]
    fn invalid_draft_is_rejected_before_writing() {
        let fx = Fx::new();
        let revision = fx.revision();
        let mut invalid = draft();
        invalid.windows[0].start = "25:00".to_owned();
        let error = fx.save(&invalid, &revision).expect_err("应当拒绝");
        assert_eq!(error.stage(), "invalid");
        assert_eq!(fx.revision(), revision, "不能写盘");
    }
}
