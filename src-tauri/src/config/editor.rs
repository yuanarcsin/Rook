//! 配置文本编辑器后端（M4-2）：revision 冲突检测 + 原子保存。
//!
//! 与 [`super::load`] 的关系：
//! - revision 是**文件字节**的 SHA-256 前 16 字节 hex（与 spike-s6 一致），
//!   不依赖 mtime，也不受"编辑器改了又改回"影响；
//! - 保存前先重读文件并比对 revision，不一致返回 [`EditorError::Conflict`]，
//!   **绝不写文件**；
//! - 内容先写同目录临时文件，再以该临时文件为 `--config` 走完整的
//!   [`super::load`]（含结构校验），结构错误不落盘；任务级 issue 允许保存
//!   （与 docs/02 §6.1 的严重级别一致）；
//! - 校验通过后 `sync_all` + `rename` 原子替换，失败保留原文件并清理临时文件；
//! - 替换前把上一版复制为 `<文件名>.bak`（只保留一份；备份失败只告警）；
//! - 内容与当前文件字节相同则 no-op，避免"没改也触发重载"。
//!
//! 本模块只做纯逻辑 + 文件操作：不依赖 Tauri，也不接受前端传入的路径。

use std::fmt;
use std::fmt::Write as _;
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use sha2::{Digest, Sha256};

use super::error::LoadError;
use super::{LoadedConfig, Overrides, load};

/// revision 取 SHA-256 摘要的前 16 字节（32 个 hex 字符）。
const REVISION_BYTES: usize = 16;

/// 同一进程内的保存串行化：避免两个 UI 请求同时走 read-modify-write。
///
/// 文本保存与结构化草稿（[`super::task_draft`]）共用这一把锁：两者都会
/// "读文件 → 算 revision → 改内容 → 原子替换"，并发会互相覆盖。
static SAVE_LOCK: Mutex<()> = Mutex::new(());
/// 临时文件名序号：保证同目录内多次保存不撞名。
static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// 编辑器读取结果：文件内容 + 内容 revision。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigText {
    pub path: PathBuf,
    pub text: String,
    pub revision: String,
}

/// 保存成功的结果。
#[derive(Debug, Clone, PartialEq)]
pub struct SaveOutcome {
    /// 落盘后文件内容的 revision。
    pub revision: String,
    /// `false` 表示内容与当前文件相同，未写盘。
    pub changed: bool,
    /// 本次内容的任务级问题/警告（结构错误不会走到这里）。
    pub issues: Vec<super::ConfigIssue>,
}

/// 编辑器错误；`stage` 与命令错误 DTO 的取值一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorError {
    /// 读取当前配置文件失败。
    Read { path: PathBuf, message: String },
    /// 写临时文件/替换原文件失败。
    Write { path: PathBuf, message: String },
    /// 内容未通过结构校验（语法、未知字段、version 等）。
    Parse {
        path: PathBuf,
        message: String,
        line: Option<usize>,
        col: Option<usize>,
    },
    /// 文件已被外部修改：`current_revision` 是磁盘上的最新 revision。
    Conflict { current_revision: String },
    /// 没有可用的 `.bak`（从未成功保存过，或备份已被删除）。
    BackupMissing { path: PathBuf },
}

impl EditorError {
    /// 稳定英文阶段标识：`read` / `write` / `parse` / `conflict` / `backup`。
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Write { .. } => "write",
            Self::Parse { .. } => "parse",
            Self::Conflict { .. } => "conflict",
            Self::BackupMissing { .. } => "backup",
        }
    }

    fn read(path: &Path, error: io::Error) -> Self {
        Self::Read {
            path: path.to_path_buf(),
            message: format!("读取配置文件 {} 失败：{error}", path.display()),
        }
    }

    fn write(path: &Path, error: io::Error) -> Self {
        Self::Write {
            path: path.to_path_buf(),
            message: format!("写入配置文件 {} 失败：{error}", path.display()),
        }
    }
}

impl fmt::Display for EditorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { message, .. } | Self::Write { message, .. } => f.write_str(message),
            Self::Parse {
                path,
                message,
                line,
                col,
            } => {
                write!(f, "{}: ", path.display())?;
                if let (Some(line), Some(col)) = (line, col) {
                    write!(f, "{line}:{col}: ")?;
                }
                f.write_str(message)
            }
            Self::Conflict { current_revision } => {
                write!(f, "文件已被外部修改（当前 revision {current_revision}）")
            }
            Self::BackupMissing { path } => {
                write!(f, "没有可用的配置备份：{}", path.display())
            }
        }
    }
}

impl std::error::Error for EditorError {}

/// 计算内容 revision：SHA-256 前 16 字节的 hex。
pub fn revision_of(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(REVISION_BYTES * 2);
    for byte in &digest[..REVISION_BYTES] {
        // 写入 String 不会失败。
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 在保存锁内执行 `f`：串行化所有"读-改-写"配置的路径。
pub(crate) fn with_save_lock<T>(f: impl FnOnce() -> T) -> T {
    let _guard = SAVE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    f()
}

/// 把已经生成好的内容落盘：临时文件 → 完整加载校验 → 备份旧版 → 原子替换。
///
/// 调用方必须已持有 [`with_save_lock`] 的锁，并已完成 revision 冲突检测与
/// "内容未变"判定——这里只负责"写临时文件、校验、备份、替换、失败清理"。
///
/// 结构错误返回 [`EditorError::Parse`]（原文件不受影响）；成功返回
/// `(任务级问题/警告, 落盘后文件内容的 revision)`，由调用方决定如何呈现。
///
/// C5：写入前若 `version == 1`，用 `toml_edit` 把它升到 2（保留注释与其它字段），
/// 因此落盘字节可能与入参不同，调用方**必须**使用返回的 revision，而不是自己
/// 对入参字节重新计算。
pub(crate) fn commit(
    path: &Path,
    bytes: &[u8],
    data_dir: Option<&Path>,
) -> Result<(Vec<super::ConfigIssue>, String), EditorError> {
    let bytes = bump_schema_version(bytes)?;
    let revision = revision_of(&bytes);
    let temp = TempFile::create(path, &bytes)?;
    let loaded = validate(temp.path(), path, data_dir)?;
    // 只有新内容通过完整校验才备份：避免把"写坏的尝试"覆盖掉最后一份好配置。
    backup_previous(path);
    temp.persist(path)?;
    Ok((loaded.issues, revision))
}

/// 把 `version = 1` 的字节升级为 2；其它情况原样返回。
fn bump_schema_version(bytes: &[u8]) -> Result<Vec<u8>, EditorError> {
    let text = std::str::from_utf8(bytes).map_err(|error| EditorError::Parse {
        path: PathBuf::new(),
        message: format!("配置文件必须是 UTF-8 编码：{error}"),
        line: None,
        col: None,
    })?;
    let mut doc: toml_edit::DocumentMut = text.parse().map_err(|error| EditorError::Parse {
        path: PathBuf::new(),
        message: format!("配置文件不是合法 TOML：{error}"),
        line: None,
        col: None,
    })?;
    if doc.get("version").and_then(|item| item.as_integer()) == Some(1) {
        doc["version"] = toml_edit::value(super::SCHEMA_VERSION);
        return Ok(doc.to_string().into_bytes());
    }
    Ok(bytes.to_vec())
}

/// 覆盖保存前的备份路径：同目录 `<文件名>.bak`，只保留一份。
pub fn backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak");
    path.with_file_name(name)
}

/// 恢复备份时"被替换掉的坏文件"的落点：同目录 `<文件名>.bak.bad`。
///
/// 与 [`backup_path`] 分开命名，避免恢复流程把坏内容写回唯一的一份好备份。
pub fn bad_backup_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".bak.bad");
    path.with_file_name(name)
}

/// 尽力保留上一版配置：失败只告警，不阻止本次保存。
///
/// 恢复入口见 [`restore_backup`]：`.bak` 保存的是"最近一次成功保存前的版本"，
/// 用户手改坏配置后可以一键回滚。
fn backup_previous(path: &Path) {
    if !path.exists() {
        return;
    }
    let backup = backup_path(path);
    if let Err(error) = fs::copy(path, &backup) {
        tracing::warn!(
            config = %path.display(),
            backup = %backup.display(),
            %error,
            "保存前备份配置失败（继续保存）"
        );
    }
}

/// 读取配置文本与其 revision。
pub fn read_config_text(path: &Path) -> Result<ConfigText, EditorError> {
    let bytes = fs::read(path).map_err(|error| EditorError::read(path, error))?;
    let text = String::from_utf8(bytes.clone()).map_err(|error| EditorError::Read {
        path: path.to_path_buf(),
        message: format!("配置文件必须是 UTF-8 编码：{error}"),
    })?;
    Ok(ConfigText {
        path: path.to_path_buf(),
        text,
        revision: revision_of(&bytes),
    })
}

/// 保存配置文本。
///
/// - `expected_revision` 与磁盘当前 revision 不一致 → [`EditorError::Conflict`]，不写盘；
/// - 内容与磁盘字节相同 → `changed = false`，不写盘；
/// - 结构错误 → [`EditorError::Parse`]，不写盘；
/// - 成功 → 临时文件 `sync_all` 后 `rename` 原子替换，返回新 revision。
///
/// `data_dir` 是 `--data-dir` 覆盖项：校验临时文件时与正常加载使用同一数据目录，
/// 避免校验过程凭空创建/使用另一个目录。
pub fn save_config_text(
    path: &Path,
    text: &str,
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<SaveOutcome, EditorError> {
    with_save_lock(|| {
        let current = fs::read(path).map_err(|error| EditorError::read(path, error))?;
        let current_revision = revision_of(&current);
        if current_revision != expected_revision {
            return Err(EditorError::Conflict { current_revision });
        }

        let bytes = text.as_bytes();
        if bytes == current.as_slice() {
            return Ok(SaveOutcome {
                revision: current_revision,
                changed: false,
                issues: Vec::new(),
            });
        }

        let (issues, revision) = commit(path, bytes, data_dir)?;

        Ok(SaveOutcome {
            revision,
            changed: true,
            issues,
        })
    })
}

/// 用 `<文件名>.bak` 覆盖当前配置（一键回滚到上一版）。
///
/// 与保存同一套校验：备份内容先写临时文件并走完整加载，**内容不合法则主文件
/// 不变**；通过后把当前文件（可能已被手改坏）另存为 `<文件名>.bak.bad`，
/// 再原子替换。这里刻意**不调用** `backup_previous`——那会把坏文件覆盖到
/// 唯一的一份好备份上。
pub fn restore_backup(path: &Path, data_dir: Option<&Path>) -> Result<SaveOutcome, EditorError> {
    with_save_lock(|| {
        let backup = backup_path(path);
        let bytes = match fs::read(&backup) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(EditorError::BackupMissing { path: backup });
            }
            Err(error) => return Err(EditorError::read(&backup, error)),
        };

        let temp = TempFile::create(path, &bytes)?;
        let loaded = validate(temp.path(), path, data_dir)?;

        // 坏文件是排障现场：另存一份，失败也不阻止恢复。
        let bad = bad_backup_path(path);
        if let Err(error) = fs::copy(path, &bad) {
            tracing::warn!(
                config = %path.display(),
                bad_backup = %bad.display(),
                %error,
                "恢复备份时另存当前配置失败（继续恢复）"
            );
        }

        temp.persist(path)?;

        Ok(SaveOutcome {
            revision: revision_of(&bytes),
            changed: true,
            issues: loaded.issues,
        })
    })
}

/// 以临时文件为 `--config` 走完整加载：结构错误转 [`EditorError::Parse`]。
fn validate(
    temp: &Path,
    target: &Path,
    data_dir: Option<&Path>,
) -> Result<LoadedConfig, EditorError> {
    let overrides = Overrides {
        config: Some(temp),
        data_dir,
    };
    match load(&overrides) {
        Ok(loaded) => Ok(loaded),
        Err(LoadError::Invalid(error)) => {
            let mut message = error.message.clone();
            if !error.path.is_empty() {
                message.push_str(&format!("（字段 {}）", error.path));
            }
            Err(EditorError::Parse {
                path: target.to_path_buf(),
                message,
                line: Some(error.line),
                col: Some(error.col),
            })
        }
        // 校验阶段的 I/O 失败（临时文件不可读等）按写阶段报错。
        Err(LoadError::Io { source, .. }) => Err(EditorError::Write {
            path: target.to_path_buf(),
            message: format!("校验临时配置失败：{source}"),
        }),
    }
}

/// 同目录临时文件：创建即 armed，`Drop` 时清理；成功 rename 后解除。
struct TempFile {
    path: PathBuf,
    armed: bool,
}

impl TempFile {
    fn create(target: &Path, bytes: &[u8]) -> Result<Self, EditorError> {
        let dir = target
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let name = target
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "config.toml".to_owned());

        let mut last_error = None;
        for _ in 0..8 {
            let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!(".{name}.{}.{seq}.tmp", std::process::id()));
            let opened = OpenOptions::new().write(true).create_new(true).open(&path);
            let mut file = match opened {
                Ok(file) => file,
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    last_error = Some(error);
                    continue;
                }
                Err(error) => return Err(EditorError::write(&path, error)),
            };

            // 失败时 TempFile 的 Drop 会删掉半截临时文件。
            let temp = Self { path, armed: true };
            file.write_all(bytes)
                .and_then(|()| file.sync_all())
                .map_err(|error| EditorError::write(&temp.path, error))?;
            return Ok(temp);
        }

        Err(EditorError::write(
            dir,
            last_error.unwrap_or_else(|| {
                io::Error::new(io::ErrorKind::AlreadyExists, "无法创建临时配置文件")
            }),
        ))
    }

    fn path(&self) -> &Path {
        &self.path
    }

    /// 原子替换目标文件；失败时临时文件由 `Drop` 清理，原文件不受影响。
    fn persist(mut self, target: &Path) -> Result<(), EditorError> {
        fs::rename(&self.path, target).map_err(|error| EditorError::write(target, error))?;
        self.armed = false;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if self.armed {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MINIMAL: &str = "version = 2\n";

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
            revision_of(&fs::read(&self.path).expect("读配置文件"))
        }

        fn save(&self, text: &str, expected: &str) -> Result<SaveOutcome, EditorError> {
            save_config_text(&self.path, text, expected, Some(&self.data_dir))
        }

        /// 目录里残留的临时文件（保存流程必须清理干净）。
        fn temp_leftovers(&self) -> Vec<String> {
            fs::read_dir(self.path.parent().expect("父目录"))
                .expect("列目录")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect()
        }
    }

    #[test]
    fn revision_is_stable_and_changes_with_content() {
        let a = revision_of(b"version = 1\n");
        assert_eq!(a, revision_of(b"version = 1\n"));
        assert_eq!(a.len(), 32);
        assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a, revision_of(b"version = 2\n"));
    }

    #[test]
    fn read_returns_text_and_revision() {
        let fx = Fx::new(MINIMAL);
        let view = read_config_text(&fx.path).expect("读取");
        assert_eq!(view.path, fx.path);
        assert_eq!(view.text, MINIMAL);
        assert_eq!(view.revision, fx.revision());
    }

    #[test]
    fn read_missing_file_reports_read_stage() {
        let fx = Fx::new(MINIMAL);
        let missing = fx.path.with_file_name("nope.toml");
        let error = read_config_text(&missing).expect_err("应当失败");
        assert_eq!(error.stage(), "read");
    }

    #[test]
    fn conflict_refuses_to_write() {
        let fx = Fx::new(MINIMAL);
        let opened = fx.revision();
        // 外部修改（模拟另一个编辑器）。
        let external = "version = 1\n# external edit\n";
        fs::write(&fx.path, external).expect("外部写入");
        let external_revision = fx.revision();
        assert_ne!(opened, external_revision);

        // 即使草稿本身也不合法，冲突也必须先返回。
        let error = fx.save("version = ", &opened).expect_err("应当冲突");
        match error {
            EditorError::Conflict { current_revision } => {
                assert_eq!(current_revision, external_revision);
            }
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
        assert_eq!(fx.read(), external, "外部修改必须完好保留");
        assert!(fx.temp_leftovers().is_empty());
    }

    #[test]
    fn invalid_toml_refuses_to_write() {
        let fx = Fx::new(MINIMAL);
        let error = fx.save("version = ", &fx.revision()).expect_err("应当失败");
        assert_eq!(error.stage(), "parse");
        assert_eq!(fx.read(), MINIMAL);
        assert!(fx.temp_leftovers().is_empty());
    }

    #[test]
    fn structural_error_refuses_to_write() {
        let fx = Fx::new(MINIMAL);
        // version=3 是结构错误（整份配置不可用）。
        let error = fx
            .save("version = 3\n", &fx.revision())
            .expect_err("应当失败");
        assert_eq!(error.stage(), "parse");
        assert_eq!(fx.read(), MINIMAL);
        assert!(fx.temp_leftovers().is_empty());
    }

    #[test]
    fn task_level_issue_still_saves() {
        let fx = Fx::new(MINIMAL);
        let draft = "version = 2\n\n[[tasks]]\nid = \"ghost\"\nname = \"幽灵任务\"\n\
                     command = \"rook-no-such-command\"\n\
                     working_dir = \"D:/rook-no-such-dir\"\n";
        let outcome = fx
            .save(draft, &fx.revision())
            .expect("任务级问题应允许保存");
        assert!(outcome.changed);
        assert!(!outcome.issues.is_empty(), "应带回任务级 issue");
        assert_eq!(fx.read(), draft);
        assert_eq!(outcome.revision, fx.revision());
        assert!(fx.temp_leftovers().is_empty());
    }

    #[test]
    fn save_replaces_file_atomically() {
        let fx = Fx::new(MINIMAL);
        let draft = "version = 2\n\n[app]\nstart_minimized = false\n";
        let outcome = fx.save(draft, &fx.revision()).expect("保存成功");
        assert!(outcome.changed);
        assert_eq!(fx.read(), draft);
        assert_eq!(outcome.revision, revision_of(draft.as_bytes()));
        assert_eq!(outcome.revision, fx.revision());
        assert!(fx.temp_leftovers().is_empty());

        // 新 revision 可以继续保存。
        let next = "version = 2\n\n[app]\nstart_minimized = true\n";
        let outcome = fx.save(next, &outcome.revision).expect("二次保存成功");
        assert!(outcome.changed);
        assert_eq!(fx.read(), next);
    }

    #[test]
    fn save_upgrades_v1_to_v2_preserving_comments() {
        let fx = Fx::new("version = 1\n# 注释\n\n[app]\nconfirm_exit = true\n");
        let draft = "version = 1\n# 注释\n\n[app]\nconfirm_exit = false\n";
        let outcome = fx.save(draft, &fx.revision()).expect("保存成功");
        assert!(outcome.changed);
        let text = fx.read();
        assert!(text.contains("version = 2"), "{text}");
        assert!(text.contains("# 注释"), "{text}");
        assert!(text.contains("confirm_exit = false"), "{text}");
        // 返回 revision 必须与落盘字节一致，紧接着二次保存不误判冲突。
        assert_eq!(outcome.revision, fx.revision());
    }

    #[test]
    fn identical_content_is_noop() {
        let fx = Fx::new(MINIMAL);
        let outcome = fx.save(MINIMAL, &fx.revision()).expect("no-op");
        assert!(!outcome.changed);
        assert_eq!(outcome.revision, fx.revision());
        assert_eq!(fx.read(), MINIMAL);
        assert!(fx.temp_leftovers().is_empty());
    }

    /// 每次成功保存前保留上一版：只留一份 `.bak`，内容始终是"上一个成功版本"。
    #[test]
    fn save_keeps_previous_version_as_bak() {
        let fx = Fx::new(MINIMAL);
        assert!(!backup_path(&fx.path).exists());

        let first = "version = 2\n# first\n";
        let outcome = fx.save(first, &fx.revision()).expect("首次保存");
        let backup = backup_path(&fx.path);
        assert_eq!(fs::read_to_string(&backup).expect("读备份"), MINIMAL);

        let second = "version = 2\n# second\n";
        fx.save(second, &outcome.revision).expect("二次保存");
        assert_eq!(fs::read_to_string(&backup).expect("读备份"), first);
        // 只保留一份，不产生 .bak.1 之类的额外文件。
        let backups = fs::read_dir(fx.path.parent().expect("父目录"))
            .expect("列目录")
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".bak"))
            .count();
        assert_eq!(backups, 1);
    }

    /// 校验失败不写盘，也不能覆盖已有的备份。
    #[test]
    fn failed_save_leaves_bak_untouched() {
        let fx = Fx::new(MINIMAL);
        let good = "version = 2\n# good\n";
        let outcome = fx.save(good, &fx.revision()).expect("先写一版好配置");
        let backup_before = fs::read_to_string(backup_path(&fx.path)).expect("读备份");

        let broken = "version = 1\n[app\n";
        assert!(fx.save(broken, &outcome.revision).is_err());
        assert_eq!(fx.read(), good);
        assert_eq!(
            fs::read_to_string(backup_path(&fx.path)).expect("读备份"),
            backup_before
        );
    }

    /// 手改坏配置后一键恢复：主文件回到 `.bak`，坏内容另存为 `.bak.bad`。
    #[test]
    fn restore_backup_rolls_back_and_keeps_bad_copy() {
        let fx = Fx::new(MINIMAL);
        let good = "version = 1\n# good\n";
        fx.save(good, &fx.revision()).expect("先写一版好配置");
        assert_eq!(
            fs::read_to_string(backup_path(&fx.path)).expect("读备份"),
            MINIMAL
        );

        let broken = "version = 1\n[app\nstart_minimized = \n";
        fs::write(&fx.path, broken).expect("手改坏配置");

        let outcome = restore_backup(&fx.path, Some(&fx.data_dir)).expect("恢复成功");
        assert!(outcome.changed);
        assert_eq!(outcome.revision, revision_of(MINIMAL.as_bytes()));
        // 主文件回到备份内容，备份本身没被写过。
        assert_eq!(fx.read(), MINIMAL);
        assert_eq!(
            fs::read_to_string(backup_path(&fx.path)).expect("读备份"),
            MINIMAL
        );
        // 坏文件保留下来供排障。
        assert_eq!(
            fs::read_to_string(bad_backup_path(&fx.path)).expect("读坏文件副本"),
            broken
        );
        assert!(fx.temp_leftovers().is_empty());
    }

    /// 没有 `.bak` 时返回 `BackupMissing`，主文件不受影响。
    #[test]
    fn restore_without_backup_reports_backup_stage() {
        let fx = Fx::new(MINIMAL);
        let error = restore_backup(&fx.path, Some(&fx.data_dir)).expect_err("应当失败");
        assert_eq!(error.stage(), "backup");
        match error {
            EditorError::BackupMissing { path } => assert_eq!(path, backup_path(&fx.path)),
            other => panic!("期望 BackupMissing，实际 {other:?}"),
        }
        assert_eq!(fx.read(), MINIMAL);
        assert!(!bad_backup_path(&fx.path).exists());
    }

    /// 备份内容本身不合法：拒绝覆盖主文件，也不产生 `.bak.bad`。
    #[test]
    fn invalid_backup_does_not_overwrite_config() {
        let fx = Fx::new(MINIMAL);
        fs::write(backup_path(&fx.path), "version = 1\n[app\n").expect("写坏备份");

        let error = restore_backup(&fx.path, Some(&fx.data_dir)).expect_err("应当失败");
        assert_eq!(error.stage(), "parse");
        assert_eq!(fx.read(), MINIMAL);
        assert!(!bad_backup_path(&fx.path).exists());
        assert!(fx.temp_leftovers().is_empty());
    }
}
