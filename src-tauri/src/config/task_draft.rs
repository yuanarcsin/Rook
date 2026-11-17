//! 结构化任务草稿（M4-4a）：从当前配置生成草稿、按草稿改单个 `[[tasks]]`、删除任务。
//!
//! 与 [`super::editor`] 的关系（共用同一套保存语义）：
//! - 同一把保存锁、同一套 revision（文件字节 SHA-256 前 16 字节 hex）；
//! - 冲突检测先于任何修改：revision 与磁盘不一致时**绝不写文件**；
//! - 内容先写同目录临时文件，再以该临时文件为 `--config` 走完整 [`super::load`]
//!   （结构错误不落盘），成功后才 `sync_all` + `rename` 原子替换；
//! - 生成的内容与磁盘字节相同 → no-op，避免"没改也触发重载"。
//!
//! 与 [`super::editor`] 的区别：这里不接收整份文本，只接收结构化草稿，写回时用
//! `toml_edit` **只改草稿涉及的值**，保留其他字段、键顺序、注释与格式。改标量前
//! 先搬运旧的 `Decor`，否则行内注释会被静默清空（见
//! `docs/research/spike-s6-toml-edit.md` §3.2）。与默认值相同的字段不写回，
//! 因此"取出草稿原样保存"是逐字节 no-op。
//!
//! 路径只由调用方（Engine，来自启动时的配置路径解析）传入，本模块不接受前端路径。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use super::SaveOutcome;
use super::editor::{self, EditorError};

// ---------------------------------------------------------------- 草稿 DTO

/// 一个任务的结构化草稿（JSON 字段为 camelCase）。
///
/// 字段集合与 `docs/02-task-model-and-config.md` §4 的常用字段一一对应；
/// 枚举一律用字符串承载（`env_proxy` / `health.kind` / `restart.mode` /
/// `stop.method` / `logs.rotation` / `logs.encoding`），非法取值由落盘前的
/// 完整校验拒绝，错误信息带期望取值。
///
/// 缺省字段按文档默认值填充：草稿里的"默认值"表示"文件里不写这个键"，
/// 因此取出草稿原样保存不会往文件里塞默认值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskDraft {
    /// 任务 id（必填，不可变；用于定位现有 `[[tasks]]`，找不到则新建）。
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    /// 文件里怎么写就怎么返回（相对路径保持相对，不做展开）。
    pub working_dir: String,
    pub auto_start: bool,
    pub elevated: bool,
    pub notify: bool,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<u32>,
    pub env_proxy: String,
    /// `None` / 空串表示不写该键。
    pub panel_url: Option<String>,
    pub health: TaskDraftHealth,
    pub restart: TaskDraftRestart,
    pub stop: TaskDraftStop,
    pub logs: TaskDraftLogs,
    pub power: TaskDraftPower,
}

/// `[tasks.health]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskDraftHealth {
    /// TOML 键名是 `type`，草稿里叫 `kind`（与 [`super::HealthConfig`] 一致）。
    pub kind: String,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub startup_grace_ms: u64,
    pub failure_threshold: u32,
    /// 心跳文件路径；`None` / 空串表示用 `<data>/heartbeats/<id>.hb`。
    pub path: Option<String>,
    /// 心跳最大年龄；`None` 表示不写（heartbeat 模式必填）。
    pub max_age_ms: Option<u64>,
    pub needs_human_status: Vec<String>,
    /// 仅 http 模式；`None` / 空串表示不写该键。
    pub url: Option<String>,
    pub expect_status: Vec<u32>,
    pub expect_body_contains: Option<String>,
}

/// `[tasks.restart]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskDraftRestart {
    pub mode: String,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub multiplier: f64,
    pub jitter: f64,
    pub max_consecutive_failures: u32,
    pub pause_on_exit_codes: Vec<u32>,
    pub stable_after_ms: u64,
    pub window_ms: u64,
    pub max_starts_in_window: u32,
}

/// `[tasks.stop]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskDraftStop {
    pub method: String,
    pub grace_ms: u64,
    pub timeout_ms: u64,
}

/// `[tasks.logs]`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskDraftLogs {
    /// 日志目录；`None` / 空串表示用 `<data>/logs/<id>`。
    pub dir: Option<String>,
    pub rotation: String,
    pub max_file_mb: u32,
    pub max_files: u32,
    pub max_days: u32,
    pub encoding: String,
}

/// `[tasks.power]`
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TaskDraftPower {
    /// `None` 表示继承 `[supervisor].resume_grace_ms`。
    pub resume_grace_ms: Option<u64>,
}

impl Default for TaskDraft {
    fn default() -> Self {
        Self {
            id: String::new(),
            name: String::new(),
            enabled: true,
            command: String::new(),
            args: Vec::new(),
            working_dir: String::new(),
            auto_start: false,
            elevated: false,
            notify: true,
            env: BTreeMap::new(),
            ports: Vec::new(),
            env_proxy: super::EnvProxy::BypassLocalhost.as_str().to_owned(),
            panel_url: None,
            health: TaskDraftHealth::default(),
            restart: TaskDraftRestart::default(),
            stop: TaskDraftStop::default(),
            logs: TaskDraftLogs::default(),
            power: TaskDraftPower::default(),
        }
    }
}

impl Default for TaskDraftHealth {
    fn default() -> Self {
        let defaults = super::HealthConfig::default();
        Self {
            kind: defaults.kind.as_str().to_owned(),
            interval_ms: defaults.interval_ms,
            timeout_ms: defaults.timeout_ms,
            startup_grace_ms: defaults.startup_grace_ms,
            failure_threshold: defaults.failure_threshold,
            path: None,
            max_age_ms: None,
            needs_human_status: Vec::new(),
            url: None,
            expect_status: defaults
                .expect_status
                .iter()
                .map(|code| u32::from(*code))
                .collect(),
            expect_body_contains: None,
        }
    }
}

impl Default for TaskDraftRestart {
    fn default() -> Self {
        let defaults = super::RestartConfig::default();
        Self {
            mode: defaults.mode.as_str().to_owned(),
            initial_delay_ms: defaults.initial_delay_ms,
            max_delay_ms: defaults.max_delay_ms,
            multiplier: defaults.multiplier,
            jitter: defaults.jitter,
            max_consecutive_failures: defaults.max_consecutive_failures,
            pause_on_exit_codes: Vec::new(),
            stable_after_ms: defaults.stable_after_ms,
            window_ms: defaults.window_ms,
            max_starts_in_window: defaults.max_starts_in_window,
        }
    }
}

impl Default for TaskDraftStop {
    fn default() -> Self {
        let defaults = super::StopConfig::default();
        Self {
            method: defaults.method.as_str().to_owned(),
            grace_ms: defaults.grace_ms,
            timeout_ms: defaults.timeout_ms,
        }
    }
}

impl Default for TaskDraftLogs {
    fn default() -> Self {
        Self {
            dir: None,
            rotation: super::Rotation::Size.as_str().to_owned(),
            max_file_mb: 10,
            max_files: 5,
            max_days: 7,
            encoding: super::Encoding::Auto.as_str().to_owned(),
        }
    }
}

// ---------------------------------------------------------------- 错误

/// 草稿读写错误；`stage` 与命令错误 DTO 的取值一一对应。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DraftError {
    /// 读取配置文件失败。
    Read { path: PathBuf, message: String },
    /// 现有配置文件无法解析（语法错误），带行/列。
    Parse {
        path: PathBuf,
        message: String,
        line: Option<usize>,
        col: Option<usize>,
    },
    /// 文件已被外部修改：`current_revision` 是磁盘上的最新 revision。
    Conflict { current_revision: String },
    /// 配置状态不允许这次操作（配置里没有任务、任务不存在、`tasks` 不是数组表）。
    Config { message: String },
    /// 草稿本身未通过结构校验（行列对应"将要写入的内容"）。
    Validate {
        path: PathBuf,
        message: String,
        line: Option<usize>,
        col: Option<usize>,
    },
    /// 写临时文件/替换原文件失败。
    Write { path: PathBuf, message: String },
}

impl DraftError {
    /// 稳定英文阶段标识：`read` / `parse` / `conflict` / `config` / `validate` / `write`。
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Parse { .. } => "parse",
            Self::Conflict { .. } => "conflict",
            Self::Config { .. } => "config",
            Self::Validate { .. } => "validate",
            Self::Write { .. } => "write",
        }
    }

    fn config(message: impl Into<String>) -> Self {
        Self::Config {
            message: message.into(),
        }
    }
}

impl fmt::Display for DraftError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read { message, .. } | Self::Write { message, .. } => f.write_str(message),
            Self::Config { message } => f.write_str(message),
            Self::Parse {
                path,
                message,
                line,
                col,
            }
            | Self::Validate {
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

impl std::error::Error for DraftError {}

impl From<EditorError> for DraftError {
    /// [`EditorError::Parse`] 在保存路径上只可能来自"新内容的完整校验"，
    /// 因此映射为 [`DraftError::Validate`]；读取现有文件失败仍是 `Read`。
    fn from(error: EditorError) -> Self {
        match error {
            EditorError::Read { path, message } => Self::Read { path, message },
            EditorError::Write { path, message } => Self::Write { path, message },
            EditorError::Parse {
                path,
                message,
                line,
                col,
            } => Self::Validate {
                path,
                message,
                line,
                col,
            },
            EditorError::Conflict { current_revision } => Self::Conflict { current_revision },
            // 只有恢复备份会产生该错误；保存路径不会走到这里。
            EditorError::BackupMissing { path } => Self::Read {
                message: format!("没有可用的配置备份：{}", path.display()),
                path,
            },
        }
    }
}

// ---------------------------------------------------------------- 读取草稿

/// 读取 `id` 对应任务的结构化草稿。
///
/// 值来自文件原文（不做路径展开、不补默认值的字面量）：缺省字段填文档默认值，
/// 因此把返回的草稿原样交给 [`save_task_draft`] 不会产生任何字节变化。
pub fn get_task_draft(path: &Path, id: &str) -> Result<TaskDraft, DraftError> {
    let bytes = fs::read(path).map_err(|error| read_error(path, error))?;
    let text = String::from_utf8(bytes).map_err(|error| {
        read_error(
            path,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("配置文件必须是 UTF-8 编码：{error}"),
            ),
        )
    })?;
    let doc = parse_document(path, &text)?;
    let table =
        find_task(&doc, id)?.ok_or_else(|| DraftError::config(format!("任务 {id} 不存在")))?;
    Ok(draft_of(table))
}

fn read_error(path: &Path, error: std::io::Error) -> DraftError {
    DraftError::Read {
        path: path.to_path_buf(),
        message: format!("读取配置文件 {} 失败：{error}", path.display()),
    }
}

fn parse_document(path: &Path, text: &str) -> Result<DocumentMut, DraftError> {
    text.parse::<DocumentMut>()
        .map_err(|error| DraftError::Parse {
            path: path.to_path_buf(),
            message: error.message().to_owned(),
            line: error.span().map(|span| line_col(text, span.start).0),
            col: error.span().map(|span| line_col(text, span.start).1),
        })
}

/// 1 起算的行/列；`byte` 是字符边界（解析器给出的 span 一定落在边界上）。
pub(super) fn line_col(text: &str, byte: usize) -> (usize, usize) {
    let mut line = 1;
    let mut col = 1;
    for (index, ch) in text.char_indices() {
        if index >= byte {
            break;
        }
        if ch == '\n' {
            line += 1;
            col = 1;
        } else {
            col += 1;
        }
    }
    (line, col)
}

/// 定位 `id` 对应的 `[[tasks]]` 表；配置里没有该任务时返回 `Ok(None)`。
fn find_task<'a>(doc: &'a DocumentMut, id: &str) -> Result<Option<&'a Table>, DraftError> {
    let Some(index) = task_index(doc, id)? else {
        return Ok(None);
    };
    Ok(doc
        .as_table()
        .get("tasks")
        .and_then(Item::as_array_of_tables)
        .and_then(|aot| aot.get(index)))
}

// ---------------------------------------------------------------- 保存草稿

/// 按草稿写入配置：只改草稿涉及的值，保留其他字段、注释与格式。
///
/// - `expected_revision` 与磁盘当前 revision 不一致 → [`DraftError::Conflict`]，不写盘；
/// - 生成的内容与磁盘字节相同 → `changed = false`，不写盘、不触发重载；
/// - 结构错误（含草稿里的非法取值）→ [`DraftError::Validate`]，不写盘；
/// - 成功 → 临时文件校验后原子替换，返回新 revision 与本次内容的问题列表。
///
/// `draft.id` 找不到现有任务时追加一个新的 `[[tasks]]`（改名一律视为删除+新建）。
pub fn save_task_draft(
    path: &Path,
    draft: &TaskDraft,
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<SaveOutcome, DraftError> {
    editor::with_save_lock(|| {
        let current = open(path, expected_revision)?;

        if draft.working_dir.trim().is_empty() {
            // 空 working_dir 会被加载层当成"配置目录"，必须在写盘前拦下。
            return Err(DraftError::Validate {
                path: path.to_path_buf(),
                message: "working_dir 不能为空（必填）".to_owned(),
                line: None,
                col: None,
            });
        }

        let mut doc = current.doc;
        let table = task_table_mut(&mut doc, &draft.id)?;
        apply_draft(table, draft);
        commit(path, current.bytes, doc, data_dir)
    })
}

/// 删除 `id` 对应的 `[[tasks]]` 表（含其子表与表头注释），保留其他任务。
///
/// 冲突检测与原子保存语义同 [`save_task_draft`]。
pub fn delete_task(
    path: &Path,
    id: &str,
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<SaveOutcome, DraftError> {
    editor::with_save_lock(|| {
        let current = open(path, expected_revision)?;
        let mut doc = current.doc;

        let Some(index) = task_index(&doc, id)? else {
            return Err(DraftError::config(format!("任务 {id} 不存在")));
        };
        let aot = doc
            .as_table_mut()
            .get_mut("tasks")
            .and_then(Item::as_array_of_tables_mut)
            .ok_or_else(|| DraftError::config("tasks 不是 [[tasks]] 数组表，无法编辑"))?;
        aot.remove(index);

        commit(path, current.bytes, doc, data_dir)
    })
}

/// 已读入的当前配置：解析结果与原始字节一起保留，供"内容未变"判定使用。
struct Current {
    bytes: Vec<u8>,
    doc: DocumentMut,
}

/// 读文件 → 冲突检测 → UTF-8 → 解析。冲突优先于任何其他错误。
fn open(path: &Path, expected_revision: &str) -> Result<Current, DraftError> {
    let bytes = fs::read(path).map_err(|error| read_error(path, error))?;
    let revision = editor::revision_of(&bytes);
    if revision != expected_revision {
        return Err(DraftError::Conflict {
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
    let doc = parse_document(path, &text)?;
    Ok(Current { bytes, doc })
}

/// 内容未变 → no-op；否则走临时文件校验 + 原子替换。
fn commit(
    path: &Path,
    bytes: Vec<u8>,
    doc: DocumentMut,
    data_dir: Option<&Path>,
) -> Result<SaveOutcome, DraftError> {
    let rendered = doc.to_string();
    if rendered.as_bytes() == bytes.as_slice() {
        return Ok(SaveOutcome {
            revision: editor::revision_of(&bytes),
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
}

/// 取 `id` 对应的任务表，不存在则追加一个新的 `[[tasks]]`。
fn task_table_mut<'a>(doc: &'a mut DocumentMut, id: &str) -> Result<&'a mut Table, DraftError> {
    if !doc.as_table().contains_key("tasks") {
        doc.as_table_mut()
            .insert("tasks", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    let aot = doc
        .as_table_mut()
        .get_mut("tasks")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| DraftError::config("tasks 不是 [[tasks]] 数组表，无法编辑"))?;

    let index = aot
        .iter()
        .position(|table| text_of(table, "id").as_deref() == Some(id));
    let index = match index {
        Some(index) => index,
        None => {
            let mut table = Table::new();
            // 与上一个任务之间留空行，保持文件可读。
            table.decor_mut().set_prefix("\n");
            aot.push(table);
            aot.len() - 1
        }
    };
    aot.get_mut(index)
        .ok_or_else(|| DraftError::config("任务表定位失败"))
}

fn task_index(doc: &DocumentMut, id: &str) -> Result<Option<usize>, DraftError> {
    let Some(item) = doc.as_table().get("tasks") else {
        return Ok(None);
    };
    let aot = item
        .as_array_of_tables()
        .ok_or_else(|| DraftError::config("tasks 不是 [[tasks]] 数组表，无法编辑"))?;
    Ok(aot
        .iter()
        .position(|table| text_of(table, "id").as_deref() == Some(id)))
}

// ---------------------------------------------------------------- 草稿 → 文件

/// 把草稿写进任务表：只改有差异的字段，与默认值相同的字段不落盘。
fn apply_draft(table: &mut Table, draft: &TaskDraft) {
    set_str(table, "id", &draft.id, None);
    set_str(table, "name", &draft.name, None);
    set_bool(table, "enabled", draft.enabled, true);
    set_str(table, "command", &draft.command, None);
    set_strings(table, "args", &draft.args);
    set_str(table, "working_dir", &draft.working_dir, None);
    set_bool(table, "auto_start", draft.auto_start, false);
    set_bool(table, "elevated", draft.elevated, false);
    set_bool(table, "notify", draft.notify, true);
    set_env(table, "env", &draft.env);
    set_ints(table, "ports", &draft.ports, &[]);
    set_str(
        table,
        "env_proxy",
        &draft.env_proxy,
        Some(super::EnvProxy::BypassLocalhost.as_str()),
    );
    set_optional(table, "panel_url", draft.panel_url.as_deref());

    with_sub_table(table, "health", |health| {
        set_str(
            health,
            "type",
            &draft.health.kind,
            Some(super::HealthKind::Process.as_str()),
        );
        set_int(health, "interval_ms", draft.health.interval_ms, 10_000);
        set_int(health, "timeout_ms", draft.health.timeout_ms, 2000);
        set_int(
            health,
            "failure_threshold",
            u64::from(draft.health.failure_threshold),
            3,
        );
        set_int(
            health,
            "startup_grace_ms",
            draft.health.startup_grace_ms,
            20_000,
        );
        set_optional(health, "url", draft.health.url.as_deref());
        set_ints(health, "expect_status", &draft.health.expect_status, &[200]);
        set_optional(
            health,
            "expect_body_contains",
            draft.health.expect_body_contains.as_deref(),
        );
        set_optional(health, "path", draft.health.path.as_deref());
        set_optional_int(health, "max_age_ms", draft.health.max_age_ms);
        set_strings(
            health,
            "needs_human_status",
            &draft.health.needs_human_status,
        );
    });

    with_sub_table(table, "restart", |restart| {
        set_str(
            restart,
            "mode",
            &draft.restart.mode,
            Some(super::RestartMode::OnFailure.as_str()),
        );
        set_int(
            restart,
            "initial_delay_ms",
            draft.restart.initial_delay_ms,
            3000,
        );
        set_int(restart, "max_delay_ms", draft.restart.max_delay_ms, 60_000);
        set_float(restart, "multiplier", draft.restart.multiplier, 2.0);
        set_float(restart, "jitter", draft.restart.jitter, 0.2);
        set_int(
            restart,
            "max_consecutive_failures",
            u64::from(draft.restart.max_consecutive_failures),
            5,
        );
        set_ints(
            restart,
            "pause_on_exit_codes",
            &draft.restart.pause_on_exit_codes,
            &[],
        );
        set_int(
            restart,
            "stable_after_ms",
            draft.restart.stable_after_ms,
            60_000,
        );
        set_int(restart, "window_ms", draft.restart.window_ms, 600_000);
        set_int(
            restart,
            "max_starts_in_window",
            u64::from(draft.restart.max_starts_in_window),
            6,
        );
    });

    with_sub_table(table, "stop", |stop| {
        set_str(
            stop,
            "method",
            &draft.stop.method,
            Some(super::StopMethod::Graceful.as_str()),
        );
        set_int(stop, "grace_ms", draft.stop.grace_ms, 2500);
        set_int(stop, "timeout_ms", draft.stop.timeout_ms, 8000);
    });

    with_sub_table(table, "logs", |logs| {
        set_optional(logs, "dir", draft.logs.dir.as_deref());
        set_str(
            logs,
            "rotation",
            &draft.logs.rotation,
            Some(super::Rotation::Size.as_str()),
        );
        set_int(logs, "max_file_mb", u64::from(draft.logs.max_file_mb), 10);
        set_int(logs, "max_files", u64::from(draft.logs.max_files), 5);
        set_int(logs, "max_days", u64::from(draft.logs.max_days), 7);
        set_str(
            logs,
            "encoding",
            &draft.logs.encoding,
            Some(super::Encoding::Auto.as_str()),
        );
    });

    with_sub_table(table, "power", |power| {
        set_optional_int(power, "resume_grace_ms", draft.power.resume_grace_ms);
    });
}

/// 在子表上执行 `edit`；子表原本不存在且最终没写入任何字段时不留下空表。
fn with_sub_table(table: &mut Table, key: &str, edit: impl FnOnce(&mut Table)) {
    let created = table.get(key).map(|item| !item.is_table()).unwrap_or(true);
    if created {
        let mut sub = Table::new();
        // 与任务自身的键值之间留空行，和文档示例一致。
        sub.decor_mut().set_prefix("\n");
        table.insert(key, Item::Table(sub));
    }

    let empty = match table.get_mut(key).and_then(Item::as_table_mut) {
        Some(sub) => {
            edit(sub);
            sub.is_empty()
        }
        None => false,
    };
    if created && empty {
        table.remove(key);
    }
}

/// 写标量前先搬运旧的 `Decor`：`Table::insert` / 直接换 `Value` 都会清空行内注释。
fn write_scalar(table: &mut Table, key: &str, value: Value) {
    match table.get_mut(key) {
        Some(item) => {
            let decor = item.as_value().map(|old| old.decor().clone());
            let mut next = value;
            if let Some(decor) = decor {
                *next.decor_mut() = decor;
            }
            *item = Item::Value(next);
        }
        None => {
            table.insert(key, Item::Value(value));
        }
    }
}

fn set_str(table: &mut Table, key: &str, value: &str, default: Option<&str>) {
    match table.get(key) {
        Some(item) if item.as_str() == Some(value) => {}
        Some(_) => write_scalar(table, key, Value::from(value)),
        None => {
            if default != Some(value) {
                table.insert(key, Item::Value(Value::from(value)));
            }
        }
    }
}

/// 可选字符串：`None` / 空串表示"不写这个键"，已有键会被删除（回到默认语义）。
fn set_optional(table: &mut Table, key: &str, value: Option<&str>) {
    match value.filter(|text| !text.trim().is_empty()) {
        Some(text) => set_str(table, key, text, None),
        None => {
            table.remove(key);
        }
    }
}

fn set_bool(table: &mut Table, key: &str, value: bool, default: bool) {
    match table.get(key) {
        Some(item) if item.as_bool() == Some(value) => {}
        Some(_) => write_scalar(table, key, Value::from(value)),
        None => {
            if value != default {
                table.insert(key, Item::Value(Value::from(value)));
            }
        }
    }
}

fn set_int(table: &mut Table, key: &str, value: u64, default: u64) {
    set_int_with(table, key, value, Some(default));
}

/// 可选整数：`None` 表示不写该键（如 `power.resume_grace_ms` 继承全局）。
fn set_optional_int(table: &mut Table, key: &str, value: Option<u64>) {
    match value {
        Some(value) => set_int_with(table, key, value, None),
        None => {
            table.remove(key);
        }
    }
}

/// `default = None` 表示"没有默认值"：键不存在时总要写入。
fn set_int_with(table: &mut Table, key: &str, value: u64, default: Option<u64>) {
    let value = i64::try_from(value).unwrap_or(i64::MAX);
    match table.get(key) {
        Some(item) if item.as_integer() == Some(value) => {}
        Some(_) => write_scalar(table, key, Value::from(value)),
        None => {
            let is_default =
                default.and_then(|fallback| i64::try_from(fallback).ok()) == Some(value);
            if !is_default {
                table.insert(key, Item::Value(Value::from(value)));
            }
        }
    }
}

fn set_float(table: &mut Table, key: &str, value: f64, default: f64) {
    let same = table.get(key).is_some_and(|item| {
        item.as_float() == Some(value) || item.as_integer().map(|int| int as f64) == Some(value)
    });
    if same {
        return;
    }
    if table.contains_key(key) {
        write_scalar(table, key, Value::from(value));
    } else if value != default {
        table.insert(key, Item::Value(Value::from(value)));
    }
}

fn set_strings(table: &mut Table, key: &str, values: &[String]) {
    let same = match table.get(key) {
        Some(item) => match item.as_array() {
            Some(array) => {
                array.len() == values.len()
                    && array
                        .iter()
                        .zip(values)
                        .all(|(value, want)| value.as_str() == Some(want.as_str()))
            }
            None => false,
        },
        None => values.is_empty(),
    };
    if same {
        return;
    }
    if values.is_empty() && !table.contains_key(key) {
        return;
    }
    write_array(
        table,
        key,
        values
            .iter()
            .map(|text| Value::from(text.as_str()))
            .collect(),
    );
}

fn set_ints(table: &mut Table, key: &str, values: &[u32], default: &[u32]) {
    let wanted: Vec<i64> = values.iter().map(|value| i64::from(*value)).collect();
    let same = match table.get(key) {
        Some(item) => match item.as_array() {
            Some(array) => {
                array.len() == wanted.len()
                    && array
                        .iter()
                        .zip(&wanted)
                        .all(|(value, want)| value.as_integer() == Some(*want))
            }
            // `expect_status = 200` 与 `[200]` 语义相同，不为此改写文件。
            None => wanted.len() == 1 && item.as_integer() == wanted.first().copied(),
        },
        None => wanted.is_empty(),
    };
    if same {
        return;
    }
    let is_default = values.len() == default.len()
        && values
            .iter()
            .zip(default)
            .all(|(value, want)| value == want);
    if table.contains_key(key) || !is_default {
        write_array(table, key, wanted.into_iter().map(Value::from).collect());
    }
}

/// 写入数组：长度不变时逐元素替换（保留每个元素的前缀/行内注释），
/// 否则按原布局重建（多行数组保持一行一元素）。
fn write_array(table: &mut Table, key: &str, values: Vec<Value>) {
    let existing = table.get(key).and_then(Item::as_array);
    let multiline = existing
        .map(|array| array.to_string().contains('\n'))
        .unwrap_or(false);
    let same_len = existing
        .map(|array| array.len() == values.len())
        .unwrap_or(false);

    if same_len
        && let Some(array) = table
            .get_mut(key)
            .and_then(Item::as_value_mut)
            .and_then(Value::as_array_mut)
    {
        for (index, value) in values.into_iter().enumerate() {
            // `Array::replace` 由上游主动保留元素 decor。
            array.replace(index, value);
        }
        return;
    }

    let mut array = Array::new();
    if multiline {
        array.set_trailing_comma(true);
        array.set_trailing("\n");
        for value in values {
            let mut value = value;
            value.decor_mut().set_prefix("\n  ");
            array.push_formatted(value);
        }
    } else {
        for value in values {
            array.push(value);
        }
        array.fmt();
    }
    write_scalar(table, key, Value::Array(array));
}

/// 写 `env`：优先就地更新已有的行内表（保留键序），否则整表替换。
fn set_env(table: &mut Table, key: &str, env: &BTreeMap<String, String>) {
    let same = match table.get(key).and_then(Item::as_inline_table) {
        Some(inline) => {
            inline.len() == env.len()
                && env.iter().all(|(name, value)| {
                    inline.get(name).and_then(Value::as_str) == Some(value.as_str())
                })
        }
        None => env.is_empty() && !table.contains_key(key),
    };
    if same {
        return;
    }

    if env.is_empty() {
        // 空 env 与不写等价，直接删键，避免留下 `env = {}`。
        table.remove(key);
        return;
    }

    if let Some(inline) = table.get_mut(key).and_then(Item::as_inline_table_mut) {
        let stale: Vec<String> = inline
            .iter()
            .map(|(name, _)| name.to_owned())
            .filter(|name| !env.contains_key(name))
            .collect();
        for name in stale {
            inline.remove(&name);
        }
        for (name, value) in env {
            inline.insert(name.as_str(), Value::from(value.as_str()));
        }
        return;
    }

    let mut inline = InlineTable::new();
    for (name, value) in env {
        inline.insert(name.as_str(), Value::from(value.as_str()));
    }
    inline.fmt();
    write_scalar(table, key, Value::InlineTable(inline));
}

// ---------------------------------------------------------------- 文件 → 草稿

fn text_of(table: &Table, key: &str) -> Option<String> {
    table.get(key).and_then(Item::as_str).map(str::to_owned)
}

fn bool_of(table: &Table, key: &str) -> Option<bool> {
    table.get(key).and_then(Item::as_bool)
}

fn int_of(table: &Table, key: &str) -> Option<i64> {
    table.get(key).and_then(Item::as_integer)
}

fn float_of(table: &Table, key: &str) -> Option<f64> {
    table.get(key).and_then(|item| {
        item.as_float()
            .or_else(|| item.as_integer().map(|int| int as f64))
    })
}

fn strings_of(table: &Table, key: &str) -> Option<Vec<String>> {
    table.get(key).and_then(Item::as_array).map(|array| {
        array
            .iter()
            .filter_map(|value| value.as_str().map(str::to_owned))
            .collect()
    })
}

/// 整数列表：`200` 与 `[200]` 都接受（与加载层的 `de_opt_int_list` 一致）。
fn ints_of(table: &Table, key: &str) -> Option<Vec<i64>> {
    match table.get(key)? {
        item if item.is_integer() => item.as_integer().map(|value| vec![value]),
        item => item.as_array().map(|array| {
            array
                .iter()
                .filter_map(|value| value.as_integer())
                .collect()
        }),
    }
}

fn env_of(table: &Table, key: &str) -> Option<BTreeMap<String, String>> {
    table
        .get(key)
        .and_then(Item::as_inline_table)
        .map(|inline| {
            inline
                .iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|text| (name.to_owned(), text.to_owned()))
                })
                .collect()
        })
}

fn draft_of(table: &Table) -> TaskDraft {
    let defaults = TaskDraft::default();
    TaskDraft {
        id: text_of(table, "id").unwrap_or_default(),
        name: text_of(table, "name").unwrap_or_default(),
        enabled: bool_of(table, "enabled").unwrap_or(defaults.enabled),
        command: text_of(table, "command").unwrap_or_default(),
        args: strings_of(table, "args").unwrap_or_default(),
        working_dir: text_of(table, "working_dir").unwrap_or_default(),
        auto_start: bool_of(table, "auto_start").unwrap_or(defaults.auto_start),
        elevated: bool_of(table, "elevated").unwrap_or(defaults.elevated),
        notify: bool_of(table, "notify").unwrap_or(defaults.notify),
        env: env_of(table, "env").unwrap_or_default(),
        ports: ints_of(table, "ports")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|port| u32::try_from(port).ok())
            .collect(),
        env_proxy: text_of(table, "env_proxy").unwrap_or(defaults.env_proxy),
        panel_url: text_of(table, "panel_url"),
        health: health_of(table, &defaults.health),
        restart: restart_of(table, &defaults.restart),
        stop: stop_of(table, &defaults.stop),
        logs: logs_of(table, &defaults.logs),
        power: power_of(table),
    }
}

fn sub_table<'a>(table: &'a Table, key: &str) -> Option<&'a Table> {
    table.get(key).and_then(Item::as_table)
}

fn health_of(table: &Table, defaults: &TaskDraftHealth) -> TaskDraftHealth {
    let Some(health) = sub_table(table, "health") else {
        return defaults.clone();
    };
    TaskDraftHealth {
        kind: text_of(health, "type").unwrap_or_else(|| defaults.kind.clone()),
        interval_ms: int_of(health, "interval_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.interval_ms),
        timeout_ms: int_of(health, "timeout_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.timeout_ms),
        startup_grace_ms: int_of(health, "startup_grace_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.startup_grace_ms),
        failure_threshold: int_of(health, "failure_threshold")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.failure_threshold),
        path: text_of(health, "path"),
        max_age_ms: int_of(health, "max_age_ms").and_then(|value| u64::try_from(value).ok()),
        needs_human_status: strings_of(health, "needs_human_status").unwrap_or_default(),
        url: text_of(health, "url"),
        expect_status: ints_of(health, "expect_status")
            .map(|codes| {
                codes
                    .into_iter()
                    .filter_map(|code| u32::try_from(code).ok())
                    .collect()
            })
            .unwrap_or_else(|| defaults.expect_status.clone()),
        expect_body_contains: text_of(health, "expect_body_contains"),
    }
}

fn restart_of(table: &Table, defaults: &TaskDraftRestart) -> TaskDraftRestart {
    let Some(restart) = sub_table(table, "restart") else {
        return defaults.clone();
    };
    TaskDraftRestart {
        mode: text_of(restart, "mode").unwrap_or_else(|| defaults.mode.clone()),
        initial_delay_ms: int_of(restart, "initial_delay_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.initial_delay_ms),
        max_delay_ms: int_of(restart, "max_delay_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.max_delay_ms),
        multiplier: float_of(restart, "multiplier").unwrap_or(defaults.multiplier),
        jitter: float_of(restart, "jitter").unwrap_or(defaults.jitter),
        max_consecutive_failures: int_of(restart, "max_consecutive_failures")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.max_consecutive_failures),
        pause_on_exit_codes: ints_of(restart, "pause_on_exit_codes")
            .unwrap_or_default()
            .into_iter()
            .filter_map(|code| u32::try_from(code).ok())
            .collect(),
        stable_after_ms: int_of(restart, "stable_after_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.stable_after_ms),
        window_ms: int_of(restart, "window_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.window_ms),
        max_starts_in_window: int_of(restart, "max_starts_in_window")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.max_starts_in_window),
    }
}

fn stop_of(table: &Table, defaults: &TaskDraftStop) -> TaskDraftStop {
    let Some(stop) = sub_table(table, "stop") else {
        return defaults.clone();
    };
    TaskDraftStop {
        method: text_of(stop, "method").unwrap_or_else(|| defaults.method.clone()),
        grace_ms: int_of(stop, "grace_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.grace_ms),
        timeout_ms: int_of(stop, "timeout_ms")
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or(defaults.timeout_ms),
    }
}

fn logs_of(table: &Table, defaults: &TaskDraftLogs) -> TaskDraftLogs {
    let Some(logs) = sub_table(table, "logs") else {
        return defaults.clone();
    };
    TaskDraftLogs {
        dir: text_of(logs, "dir"),
        rotation: text_of(logs, "rotation").unwrap_or_else(|| defaults.rotation.clone()),
        max_file_mb: int_of(logs, "max_file_mb")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.max_file_mb),
        max_files: int_of(logs, "max_files")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.max_files),
        max_days: int_of(logs, "max_days")
            .and_then(|value| u32::try_from(value).ok())
            .unwrap_or(defaults.max_days),
        encoding: text_of(logs, "encoding").unwrap_or_else(|| defaults.encoding.clone()),
    }
}

fn power_of(table: &Table) -> TaskDraftPower {
    TaskDraftPower {
        resume_grace_ms: sub_table(table, "power")
            .and_then(|power| int_of(power, "resume_grace_ms"))
            .and_then(|value| u64::try_from(value).ok()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Overrides, load};

    /// 临时目录 + 假可执行文件 + 独立数据目录的测试夹具。
    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        path: PathBuf,
        data_dir: PathBuf,
        exe: PathBuf,
    }

    impl Fx {
        fn new(source: &str) -> Self {
            let tmp = tempfile::tempdir().expect("创建临时目录");
            let root = tmp.path().to_path_buf();
            let data_dir = root.join("data");
            fs::create_dir_all(&data_dir).expect("创建数据目录");
            let bin = root.join("bin");
            fs::create_dir_all(&bin).expect("创建 bin");
            let exe = bin.join("fake.exe");
            fs::write(&exe, b"").expect("写假可执行文件");
            let path = root.join("Rook.toml");
            fs::write(&path, source).expect("写配置文件");
            Self {
                _tmp: tmp,
                root,
                path,
                data_dir,
                exe,
            }
        }

        /// TOML 里用正斜杠，避免反斜杠被当成转义。
        fn slash(&self, path: &Path) -> String {
            path.to_string_lossy().replace('\\', "/")
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.path).expect("读配置文件")
        }

        fn revision(&self) -> String {
            editor::revision_of(&fs::read(&self.path).expect("读配置文件"))
        }

        fn get(&self, id: &str) -> Result<TaskDraft, DraftError> {
            get_task_draft(&self.path, id)
        }

        fn save(&self, draft: &TaskDraft, expected: &str) -> Result<SaveOutcome, DraftError> {
            save_task_draft(&self.path, draft, expected, Some(&self.data_dir))
        }

        fn delete(&self, id: &str, expected: &str) -> Result<SaveOutcome, DraftError> {
            delete_task(&self.path, id, expected, Some(&self.data_dir))
        }

        /// 目录里残留的临时文件（保存流程必须清理干净）。
        fn temps(&self) -> Vec<String> {
            fs::read_dir(&self.root)
                .expect("列目录")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect()
        }
    }

    /// 两个任务 + 注释 + 行内注释 + 子表，覆盖"只改一处、其他原样"的场景。
    fn fixture(exe: &str, root: &str) -> String {
        format!(
            r#"# 顶层注释：Rook
version = 2

# 第一个任务：网关
[[tasks]]
id = "alpha"
name = "甲"            # 行内注释：名称
command = "{exe}"
args = ["a.js", "--port", "3000"]
working_dir = "{root}"
auto_start = true

  # 健康探测
  [tasks.health]
  type = "http"
  url = "http://127.0.0.1:3000/health"
  expect_status = 200
  interval_ms = 10000

  [tasks.restart]
  max_consecutive_failures = 5   # 熔断阈值

  [tasks.power]
  resume_grace_ms = 60000

# 第二个任务：同步
[[tasks]]
id = "beta"
name = "乙"
command = "{exe}"
working_dir = "{root}"
enabled = false
"#
        )
    }

    fn build(fx: &Fx) -> String {
        fixture(&fx.slash(&fx.exe), &fx.slash(&fx.root))
    }

    fn new_fx() -> Fx {
        // 先建夹具拿到路径，再写真正的配置内容。
        let fx = Fx::new("version = 1\n");
        let source = build(&fx);
        fs::write(&fx.path, &source).expect("写配置");
        fx
    }

    #[test]
    fn draft_reflects_file_values() {
        let fx = new_fx();
        let draft = fx.get("alpha").expect("应能取到草稿");

        assert_eq!(draft.id, "alpha");
        assert_eq!(draft.name, "甲");
        assert!(draft.enabled);
        assert!(draft.auto_start);
        assert_eq!(draft.args, ["a.js", "--port", "3000"]);
        assert_eq!(draft.env_proxy, "bypass_localhost");
        assert_eq!(draft.health.kind, "http");
        assert_eq!(
            draft.health.url.as_deref(),
            Some("http://127.0.0.1:3000/health")
        );
        // 单值写法 `expect_status = 200` 读成列表。
        assert_eq!(draft.health.expect_status, [200]);
        assert_eq!(draft.restart.max_consecutive_failures, 5);
        assert_eq!(draft.restart.multiplier, 2.0);
        assert_eq!(draft.power.resume_grace_ms, Some(60_000));
        // 缺省字段填默认值，而不是空值。
        assert_eq!(draft.logs.rotation, "size");
        assert_eq!(draft.logs.max_files, 5);
        assert!(draft.logs.dir.is_none());
        assert!(draft.health.path.is_none());

        let beta = fx.get("beta").expect("第二个任务");
        assert!(!beta.enabled);
        assert_eq!(beta.health.kind, "process");
    }

    #[test]
    fn unknown_task_reports_config_stage() {
        let fx = new_fx();
        let error = fx.get("nope").expect_err("任务不存在应报错");
        assert_eq!(error.stage(), "config");
        assert!(error.to_string().contains("nope"), "{error}");
    }

    #[test]
    fn roundtrip_draft_is_byte_identical_noop() {
        let fx = new_fx();
        let before = fx.read();
        let revision = fx.revision();
        let draft = fx.get("alpha").expect("草稿");

        let outcome = fx.save(&draft, &revision).expect("原样保存应成功");
        assert!(!outcome.changed, "内容未变必须 no-op");
        assert_eq!(outcome.revision, revision);
        assert_eq!(fx.read(), before, "文件必须逐字节不变");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn single_field_edit_keeps_comments_and_other_tasks() {
        let fx = new_fx();
        let before = fx.read();
        let revision = fx.revision();
        let mut draft = fx.get("alpha").expect("草稿");
        draft.name = "甲2".to_owned();

        let outcome = fx.save(&draft, &revision).expect("保存成功");
        assert!(outcome.changed);

        let after = fx.read();
        assert!(after.contains("# 顶层注释：Rook"), "{after}");
        assert!(after.contains("# 第一个任务：网关"), "{after}");
        assert!(after.contains("# 健康探测"), "{after}");
        // 行内注释与列对齐都保留。
        assert!(
            after.contains("name = \"甲2\"            # 行内注释：名称"),
            "{after}"
        );
        assert!(
            after.contains("max_consecutive_failures = 5   # 熔断阈值"),
            "{after}"
        );

        // 第二个任务整块不变。
        let beta_of =
            |text: &str| text[text.find("# 第二个任务").expect("第二个任务")..].to_owned();
        assert_eq!(beta_of(&after), beta_of(&before));
        // 只有一行发生变化。
        let changed: Vec<_> = before
            .lines()
            .zip(after.lines())
            .filter(|(old, new)| old != new)
            .collect();
        assert_eq!(changed.len(), 1, "{changed:?}");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn save_draft_keeps_order_field() {
        // C7：草稿只有已知键，用户写的 `order` 不在其中，必须原样保留。
        let fx = Fx::new("version = 1\n");
        let source = format!(
            r#"version = 1

[[tasks]]
id = "alpha"
name = "甲"
command = "{exe}"
working_dir = "{root}"
order = 5
"#,
            exe = fx.slash(&fx.exe),
            root = fx.slash(&fx.root)
        );
        fs::write(&fx.path, &source).expect("写配置");
        let revision = fx.revision();

        let mut draft = fx.get("alpha").expect("草稿");
        draft.name = "甲2".to_owned();
        fx.save(&draft, &revision).expect("保存成功");

        let after = fx.read();
        assert!(after.contains("order = 5"), "order 必须保留：{after}");
        let loaded = load(&Overrides {
            config: Some(&fx.path),
            data_dir: Some(&fx.data_dir),
        })
        .expect("配置应可加载");
        assert_eq!(loaded.config.task("alpha").expect("任务").order, 5);
    }

    #[test]
    fn new_task_is_appended_with_required_fields() {
        let fx = new_fx();
        let revision = fx.revision();
        let mut draft = TaskDraft {
            id: "gamma".to_owned(),
            name: "丙".to_owned(),
            command: fx.slash(&fx.exe),
            working_dir: fx.slash(&fx.root),
            auto_start: true,
            ..TaskDraft::default()
        };
        draft.health.kind = "http".to_owned();
        draft.health.url = Some("http://127.0.0.1:3100/health".to_owned());
        draft.health.expect_body_contains = Some("{\"ok\":true".to_owned());
        draft.env.insert("PORT".to_owned(), "3100".to_owned());
        draft.ports = vec![3100];

        let outcome = fx.save(&draft, &revision).expect("新增应成功");
        assert!(outcome.changed);

        let after = fx.read();
        assert!(after.contains("# 顶层注释：Rook"), "原有注释不能丢");
        assert!(after.contains("# 第二个任务：同步"), "{after}");
        assert!(after.contains("[[tasks]]\nid = \"gamma\""), "{after}");
        assert!(after.contains("env = { PORT = \"3100\" }"), "{after}");
        assert!(after.contains("ports = [3100]"), "{after}");
        assert!(after.contains("[tasks.health]"), "{after}");
        assert!(after.contains("expect_body_contains"), "{after}");
        assert!(after.contains("ok"), "{after}");
        // 默认值不落盘。
        assert!(!after.contains("elevated"), "{after}");
        assert!(!after.contains("stable_after_ms"), "{after}");
        assert!(!after.contains("window_ms"), "{after}");
        assert!(!after.contains("max_starts_in_window"), "{after}");

        let draft = fx.get("gamma").expect("新任务应可读回");
        assert_eq!(draft.name, "丙");
        assert_eq!(draft.health.kind, "http");
        assert_eq!(
            draft.health.expect_body_contains.as_deref(),
            Some("{\"ok\":true")
        );
        assert_eq!(draft.env.get("PORT").map(String::as_str), Some("3100"));
        assert_eq!(draft.ports, [3100]);
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn delete_removes_only_target_task() {
        let fx = new_fx();
        let revision = fx.revision();

        let outcome = fx.delete("beta", &revision).expect("删除应成功");
        assert!(outcome.changed);

        let after = fx.read();
        assert!(!after.contains("id = \"beta\""), "{after}");
        assert!(
            !after.contains("# 第二个任务：同步"),
            "表头注释应随表一起删除：{after}"
        );
        assert!(after.contains("id = \"alpha\""), "{after}");
        assert!(after.contains("# 第一个任务：网关"), "{after}");
        assert!(after.contains("# 行内注释：名称"), "{after}");

        let loaded = load(&Overrides {
            config: Some(&fx.path),
            data_dir: Some(&fx.data_dir),
        })
        .expect("删除后配置仍应可加载");
        assert_eq!(loaded.config.tasks.len(), 1);
        assert_eq!(loaded.config.tasks[0].id, "alpha");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn delete_unknown_task_reports_config_stage() {
        let fx = new_fx();
        let before = fx.read();
        let error = fx.delete("nope", &fx.revision()).expect_err("应报错");
        assert_eq!(error.stage(), "config");
        assert_eq!(fx.read(), before);
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn restart_window_fields_roundtrip() {
        let fx = new_fx();
        let revision = fx.revision();
        let mut draft = fx.get("alpha").expect("草稿");
        // 缺省值来自 RestartConfig::default()，读写都不应凭空空写配置。
        assert_eq!(draft.restart.window_ms, 600_000);
        assert_eq!(draft.restart.max_starts_in_window, 6);

        draft.restart.window_ms = 120_000;
        draft.restart.max_starts_in_window = 3;
        let outcome = fx.save(&draft, &revision).expect("保存成功");
        assert!(outcome.changed);

        let after = fx.read();
        assert!(after.contains("window_ms = 120000"), "{after}");
        assert!(after.contains("max_starts_in_window = 3"), "{after}");

        let reloaded = fx.get("alpha").expect("读回");
        assert_eq!(reloaded.restart.window_ms, 120_000);
        assert_eq!(reloaded.restart.max_starts_in_window, 3);

        // 原样保存不产生字节变化。
        let again = fx.save(&reloaded, &outcome.revision).expect("二次保存");
        assert!(!again.changed);
    }

    #[test]
    fn env_and_arrays_are_rewritten() {
        let fx = new_fx();
        let revision = fx.revision();
        let mut draft = fx.get("alpha").expect("草稿");
        draft.env.insert("PORT".to_owned(), "3000".to_owned());
        draft.env.insert("DEBUG".to_owned(), "1".to_owned());
        draft.ports = vec![3000, 3001];
        draft.args = vec!["b.js".to_owned()];
        draft.health.expect_status = vec![200, 204];
        draft.restart.pause_on_exit_codes = vec![2];

        let outcome = fx.save(&draft, &revision).expect("保存成功");
        assert!(outcome.changed);

        let after = fx.read();
        assert!(
            after.contains("env = { DEBUG = \"1\", PORT = \"3000\" }"),
            "{after}"
        );
        assert!(after.contains("ports = [3000, 3001]"), "{after}");
        assert!(after.contains("args = [\"b.js\"]"), "{after}");
        assert!(after.contains("expect_status = [200, 204]"), "{after}");
        assert!(after.contains("pause_on_exit_codes = [2]"), "{after}");

        let reloaded = fx.get("alpha").expect("读回");
        assert_eq!(reloaded.env.len(), 2);
        assert_eq!(reloaded.ports, [3000, 3001]);
        assert_eq!(reloaded.args, ["b.js"]);
        assert_eq!(reloaded.health.expect_status, [200, 204]);
        assert_eq!(reloaded.restart.pause_on_exit_codes, [2]);

        // 清空 env / 可选字符串会删除对应键，回到默认语义。
        let revision = fx.revision();
        let mut draft = fx.get("alpha").expect("草稿");
        draft.env.clear();
        draft.panel_url = None;
        draft.health.expect_body_contains = None;
        fx.save(&draft, &revision).expect("保存成功");
        let after = fx.read();
        assert!(!after.contains("env ="), "{after}");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn invalid_draft_is_not_written() {
        let fx = new_fx();
        let before = fx.read();
        let revision = fx.revision();
        let mut draft = fx.get("alpha").expect("草稿");
        // 探测周期下限 1000ms：5 是结构错误。
        draft.health.interval_ms = 5;

        let error = fx.save(&draft, &revision).expect_err("应拒绝保存");
        assert_eq!(error.stage(), "validate");
        assert_eq!(fx.read(), before, "原文件必须完好");
        assert_eq!(fx.revision(), revision);
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn empty_working_dir_is_rejected() {
        let fx = new_fx();
        let before = fx.read();
        let mut draft = fx.get("alpha").expect("草稿");
        draft.working_dir = "   ".to_owned();

        let error = fx.save(&draft, &fx.revision()).expect_err("应拒绝保存");
        assert_eq!(error.stage(), "validate");
        assert_eq!(fx.read(), before);
    }

    #[test]
    fn conflict_refuses_to_write() {
        let fx = new_fx();
        let opened = fx.revision();
        // 模拟外部编辑器改文件。
        let external = format!("{}\n# external edit\n", fx.read());
        fs::write(&fx.path, &external).expect("外部写入");
        let current = fx.revision();
        assert_ne!(opened, current);

        let mut draft = fx.get("alpha").expect("草稿");
        draft.name = "不该写入".to_owned();
        match fx.save(&draft, &opened).expect_err("应冲突") {
            DraftError::Conflict { current_revision } => assert_eq!(current_revision, current),
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
        assert_eq!(fx.read(), external, "外部修改必须完好保留");
        assert!(fx.temps().is_empty());

        // 删除同样先做冲突检测。
        let error = fx.delete("alpha", &opened).expect_err("应冲突");
        assert_eq!(error.stage(), "conflict");
        assert_eq!(fx.read(), external);
    }

    #[test]
    fn draft_serializes_camel_case() {
        let draft = TaskDraft::default();
        let value = serde_json::to_value(&draft).expect("序列化草稿");
        for key in [
            "id",
            "name",
            "enabled",
            "command",
            "args",
            "workingDir",
            "autoStart",
            "elevated",
            "notify",
            "env",
            "ports",
            "envProxy",
            "panelUrl",
            "health",
            "restart",
            "stop",
            "logs",
            "power",
        ] {
            assert!(value.get(key).is_some(), "缺少字段 {key}");
        }
        assert!(value.get("working_dir").is_none());

        let health = &value["health"];
        for key in [
            "kind",
            "intervalMs",
            "timeoutMs",
            "startupGraceMs",
            "failureThreshold",
            "path",
            "maxAgeMs",
            "needsHumanStatus",
            "url",
            "expectStatus",
            "expectBodyContains",
        ] {
            assert!(health.get(key).is_some(), "health 缺少字段 {key}");
        }
        assert_eq!(health["kind"], "process");
        assert_eq!(health["expectStatus"][0], 200);

        let restart = &value["restart"];
        assert_eq!(restart["mode"], "on-failure");
        assert_eq!(restart["initialDelayMs"], 3000);
        assert_eq!(restart["maxDelayMs"], 60000);
        assert_eq!(restart["maxConsecutiveFailures"], 5);
        assert_eq!(restart["stableAfterMs"], 60000);
        assert!(
            restart["pauseOnExitCodes"]
                .as_array()
                .is_some_and(Vec::is_empty)
        );

        assert_eq!(value["stop"]["method"], "graceful");
        assert_eq!(value["stop"]["graceMs"], 2500);
        assert_eq!(value["stop"]["timeoutMs"], 8000);
        assert_eq!(value["logs"]["rotation"], "size");
        assert_eq!(value["logs"]["maxFileMb"], 10);
        assert_eq!(value["logs"]["maxDays"], 7);
        assert_eq!(value["logs"]["encoding"], "auto");
        assert!(value["power"]["resumeGraceMs"].is_null());

        // 反序列化：缺字段走默认值，显式字段覆盖默认值。
        let parsed: TaskDraft = serde_json::from_value(serde_json::json!({
            "id": "x",
            "health": { "kind": "heartbeat", "maxAgeMs": 30000 },
            "logs": { "maxFiles": 3 }
        }))
        .expect("反序列化草稿");
        assert_eq!(parsed.id, "x");
        assert!(parsed.enabled);
        assert_eq!(parsed.health.kind, "heartbeat");
        assert_eq!(parsed.health.max_age_ms, Some(30_000));
        assert_eq!(parsed.health.interval_ms, 10_000);
        assert_eq!(parsed.logs.max_files, 3);
    }
}
