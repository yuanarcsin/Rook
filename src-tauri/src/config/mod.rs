//! 配置子系统（M1-3）：模型、加载、校验、路径解析、默认模板；
//! M4-2 起包含文本编辑器后端（revision 冲突检测 + 原子保存，见 [`editor`]）；
//! M4-4a 起包含结构化任务草稿（`toml_edit` 保注释 + revision + 原子保存，见 [`task_draft`]）。
//!
//! 只做纯后端逻辑：不依赖 Tauri、不注册命令、不启动/监督进程。
//!
//! 错误分层：
//! - **结构错误**（TOML 语法、未知字段、类型错误、version、重复 id、枚举取值、
//!   数值范围与关系）使整份配置不可用，返回 [`LoadError::Invalid`]，带文件/行/列。
//! - **任务级错误**（命令找不到、工作目录不存在、health 必填缺失、保留环境变量前缀、
//!   ports 越界、panel_url 非法、路径展开失败）只让该任务 invalid：配置照常加载，
//!   问题记在 [`LoadedConfig::issues`]（`task_id` 标明归属）。
//! - **警告**（process 粗粒度监督、字段配错模式）不影响加载。
//!
//! 配置路径优先级：`--config` > `<exe_dir>/Rook.toml` > `%APPDATA%/Rook/config.toml`
//! （不存在则创建并写入默认模板，绝不覆盖）。数据目录：`--data-dir` > `[app].data_dir` >
//! `%LOCALAPPDATA%/Rook`。相对路径一律相对配置文件所在目录解析。

pub mod editor;
mod error;
mod expand;
pub mod migrate;
mod model;
mod paths;
pub mod plugin;
mod raw;
mod reorder;
pub mod schedule_edit;
mod task_draft;
mod template;
mod validate;
pub mod watch;

#[cfg(test)]
mod tests;

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub use editor::{
    ConfigText, EditorError, SaveOutcome, read_config_text, revision_of, save_config_text,
};
pub use error::{ConfigError, ConfigIssue, LoadError, Severity};
pub use expand::{EnvVars, MapEnv, SystemEnv};
pub use model::*;
pub use paths::{
    APP_DIR_NAME, INSTALLED_CONFIG_FILE, PORTABLE_CONFIG_FILE, resolve_config_path,
    resolve_data_dir,
};
pub use reorder::{ReorderError, reorder_tasks};
pub use schedule_edit::{
    SaveScheduleOutcome, ScheduleDraft, ScheduleEditError, ScheduleWindowDraft, config_revision,
    save_schedule,
};
pub use task_draft::{
    DraftError, TaskDraft, TaskDraftHealth, TaskDraftLogs, TaskDraftPower, TaskDraftRestart,
    TaskDraftStop, delete_task, get_task_draft, save_task_draft,
};
pub use template::{DEFAULT_CONFIG_TEMPLATE, default_config_template, write_default_config};
pub use watch::{
    ConfigWatchErrorView, ConfigWatchHandle, ConfigWatchState, ConfigWatchView, WatchSpec,
};

use raw::RawConfig;
use validate::Validator;

/// 加载配置所需的上下文（路径与错误都显式传入，便于测试）。
pub struct LoadContext<'a> {
    /// 已经解析好的配置文件路径。
    pub config_path: &'a Path,
    /// `--data-dir`。
    pub data_dir_override: Option<&'a Path>,
    /// `%LOCALAPPDATA%`，`None` 表示环境里没有（此时必须给 `--data-dir` 或 `[app].data_dir`）。
    pub local_appdata: Option<&'a Path>,
    pub env: &'a dyn EnvVars,
}

/// 命令行覆盖项。
#[derive(Debug, Default, Clone, Copy)]
pub struct Overrides<'a> {
    /// `--config <path>`
    pub config: Option<&'a Path>,
    /// `--data-dir <path>`
    pub data_dir: Option<&'a Path>,
}

/// 加载结果：可用配置 + 任务级问题。
#[derive(Debug, Clone, PartialEq)]
pub struct LoadedConfig {
    pub config: Config,
    pub issues: Vec<ConfigIssue>,
    /// 实际使用的配置文件绝对/相对路径（供 `CAMPUS_SENTRY_CONFIG` 注入）。
    pub config_path: PathBuf,
    /// 实际使用的数据目录（供 `CAMPUS_SENTRY_DATA_DIR` 注入）。
    pub data_dir: PathBuf,
    /// 本次加载所用文件字节的 revision（SHA-256 前 16 字节 hex）。
    ///
    /// 配置监听用它做“自身保存抑制”：UI 保存后已经 reload，监听器再收到
    /// 文件事件时 revision 相同就直接跳过，不重复 reload/日志/reconcile。
    pub revision: String,
}

impl LoadedConfig {
    /// 该任务是否可用（没有任务级错误）。
    pub fn is_task_valid(&self, task_id: &str) -> bool {
        !self.issues.iter().any(|issue| {
            issue.severity == Severity::Error && issue.task_id.as_deref() == Some(task_id)
        })
    }

    /// 只取错误（不含警告）。
    pub fn errors(&self) -> impl Iterator<Item = &ConfigIssue> {
        self.issues
            .iter()
            .filter(|issue| issue.severity == Severity::Error)
    }

    /// 只取警告。
    pub fn warnings(&self) -> impl Iterator<Item = &ConfigIssue> {
        self.issues
            .iter()
            .filter(|issue| issue.severity == Severity::Warning)
    }
}

/// 按进程环境加载配置：解析 `--config` / `--data-dir` 对应的路径后读取。
pub fn load(overrides: &Overrides<'_>) -> Result<LoadedConfig, LoadError> {
    let env = SystemEnv;
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."));
    let appdata = env.var("APPDATA").map(PathBuf::from);
    let local_appdata = env.var("LOCALAPPDATA").map(PathBuf::from);

    let config_path = resolve_config_path(overrides.config, &exe_dir, appdata.as_deref())?;
    load_file(&LoadContext {
        config_path: &config_path,
        data_dir_override: overrides.data_dir,
        local_appdata: local_appdata.as_deref(),
        env: &env,
    })
}

/// 从指定文件加载（不做 `--config` 之类的路径决策）。
pub fn load_file(ctx: &LoadContext<'_>) -> Result<LoadedConfig, LoadError> {
    let path = ctx.config_path;
    let bytes = fs::read(path).map_err(|error| LoadError::io(path, error))?;
    load_bytes(ctx, &bytes)
}

/// 从已经读取的字节加载。
///
/// 配置监听先读一次字节、算 revision 做自身保存抑制，再用同一份字节走完整
/// 校验，避免“读两次文件”带来的 TOCTOU 与重复 I/O。
pub fn load_bytes(ctx: &LoadContext<'_>, bytes: &[u8]) -> Result<LoadedConfig, LoadError> {
    let path = ctx.config_path;
    let revision = editor::revision_of(bytes);
    let text = std::str::from_utf8(bytes).map_err(|error| {
        LoadError::io(
            path,
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("配置文件必须是 UTF-8 编码：{error}"),
            ),
        )
    })?;

    let raw: RawConfig = toml::from_str(text)
        .map_err(|error| LoadError::Invalid(validate::describe_toml_error(path, text, &error)))?;

    // C5：v1 → v2 内存迁移。v2 与 v1 字段相同、仅版本号不同，迁移只改 version；
    // 原始文件不被改写，下次通过编辑器保存时才落成 v2。迁移前备份 `<config>.v1.bak`。
    let raw = if raw.version == Some(1) {
        migrate::backup_before_migrate(path, 1);
        let value: toml::Value = toml::from_str(text).map_err(|error| {
            LoadError::Invalid(validate::describe_toml_error(path, text, &error))
        })?;
        let migrated = migrate::migrate_value(value, 1).map_err(|message| {
            LoadError::Invalid(ConfigError {
                file: path.to_path_buf(),
                line: 1,
                col: 1,
                path: "version".to_owned(),
                message,
            })
        })?;
        migrated.try_into().map_err(|error: toml::de::Error| {
            LoadError::Invalid(ConfigError {
                file: path.to_path_buf(),
                line: 1,
                col: 1,
                path: "version".to_owned(),
                message: format!("迁移后的配置无法解析：{error}"),
            })
        })?
    } else {
        raw
    };

    let config_dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let validator = Validator::new(text, path, config_dir, ctx.env);

    let document = validator.document(&raw)?;
    let data_dir = paths::resolve_data_dir(
        ctx.data_dir_override,
        document.app.data_dir.as_deref(),
        ctx.local_appdata,
    )?;
    let mut issues = validator.reserved_issues(&raw);
    let (tasks, task_issues) =
        validator.tasks(&raw, &data_dir, document.supervisor.resume_grace_ms)?;
    issues.extend(task_issues);

    Ok(LoadedConfig {
        config: Config {
            version: document.version,
            app: document.app,
            supervisor: document.supervisor,
            autostart: document.autostart,
            notifications: document.notifications,
            logs: document.logs,
            tasks,
        },
        issues,
        config_path: path.to_path_buf(),
        data_dir,
        revision,
    })
}
