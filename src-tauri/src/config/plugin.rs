//! 脚本插件清单（2026-11-04 用户决策）：脚本目录放 rook.plugin.toml（旧名 campussentry.plugin.toml 仍接受），
//! 应用读取清单 → 预览 → 导入为普通 [[tasks]]；之后由现有引擎管理进程生命周期。
//!
//! 设计边界（与 docs/15-plugin-spec.md 一致）：
//! - 清单只描述"如何启动/探测/展示这个脚本"，不承载业务逻辑；
//! - 凭据仍由脚本自己持有（应用不把 uuid/Cookie 写进配置）；
//! - 导入后任务配置是唯一运行时事实源；sync_plugin 只更新清单拥有的字段，
//!   保留用户改过的 enabled/auto_start/schedule/logs/stop/power/notify/elevated/name；
//! - [logs] / [schedule] 只在首次导入时作为初始值；
//! - 本模块不依赖 Tauri；命令层负责把结果转成快照/错误 DTO。
//!
//! 本文件由 backend-core 实现完整逻辑与测试；接口保持稳定。

use std::collections::BTreeMap;
use std::fs;
use std::path::{Component, Path, PathBuf};

use chrono::NaiveTime;
use serde::{Deserialize, Deserializer, Serialize};
use toml_edit::{Array, ArrayOfTables, DocumentMut, InlineTable, Item, Table, Value};

use super::editor::{self, EditorError};
use super::{Encoding, EnvProxy, HealthKind, RestartMode, Rotation};
use crate::schedule::{Weekday, format_time, parse_time};

/// 脚本根目录下的清单文件名（新名）。
pub const MANIFEST_NAME: &str = crate::brand::PLUGIN_MANIFEST;
/// 旧版清单文件名：仍接受，便于已有脚本平滑迁移。
pub const LEGACY_MANIFEST_NAME: &str = crate::brand::LEGACY_PLUGIN_MANIFEST;
/// 当前支持的清单 schema 版本。
pub const MANIFEST_SCHEMA: i64 = 1;
/// 清单文件大小上限（docs/15 §3.1）。
const MAX_MANIFEST_BYTES: usize = 64 * 1024;

// ---------------------------------------------------------------- 预览 DTO

/// 导入前预览；字段与 src/lib/api.ts 的 PluginPreview 一一对应。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginPreview {
    pub manifest_path: String,
    pub root_dir: String,
    pub plugin_id: String,
    pub name: String,
    pub version: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: String,
    pub ports: Vec<u16>,
    pub env: BTreeMap<String, String>,
    pub env_proxy: String,
    pub panel_url: Option<String>,
    pub health_type: String,
    pub health_url: Option<String>,
    pub schedule: Option<PluginSchedulePreview>,
    /// 建议的任务 id（当前等于 plugin.id）。
    pub task_id: String,
    /// 配置里已有同 id 任务时给出，导入前由用户选择"更新/换 id"。
    pub existing_task_id: Option<String>,
    /// 不影响导入的提醒（例如工作目录不存在、未声明健康探测）。
    pub warnings: Vec<String>,
}

/// 清单里的运行时段默认值。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginSchedulePreview {
    pub enabled: bool,
    pub windows: Vec<PluginWindowPreview>,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginWindowPreview {
    pub days: Vec<String>,
    pub start: String,
    pub end: String,
}

/// 导入/同步成功后的结果（命令层用它重新加载 Engine）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginWriteOutcome {
    pub task_id: String,
    /// 文件内容是否真的变化。
    pub changed: bool,
    /// 落盘后的配置 revision。
    pub revision: String,
}

// ---------------------------------------------------------------- 错误

/// 清单解析/导入/同步失败。
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    #[error("读取插件清单失败：{message}")]
    Read { message: String },
    #[error("插件清单不是合法 TOML：{message}")]
    Parse { message: String },
    #[error("插件清单不合法：{message}")]
    Invalid { message: String },
    #[error("插件清单调用的文件不存在：{path}")]
    MissingPath { path: String },
    #[error("配置文件中找不到任务 {task_id}")]
    TaskNotFound { task_id: String },
    #[error("配置文件已被外部修改，请重新加载后再试")]
    Conflict { current_revision: String },
    #[error("写入配置失败：{message}")]
    Write { message: String },
    #[error("插件功能尚未接线")]
    NotImplemented,
}

impl PluginError {
    /// 稳定阶段标识；命令层 DTO 使用。
    pub fn stage(&self) -> &'static str {
        match self {
            Self::Read { .. } => "read",
            Self::Parse { .. } => "parse",
            Self::Invalid { .. } => "invalid",
            Self::MissingPath { .. } => "missing_path",
            Self::TaskNotFound { .. } => "task_not_found",
            Self::Conflict { .. } => "conflict",
            Self::Write { .. } => "write",
            Self::NotImplemented => "not_implemented",
        }
    }
}

fn invalid(message: impl Into<String>) -> PluginError {
    PluginError::Invalid {
        message: message.into(),
    }
}

// ---------------------------------------------------------------- 清单原始结构

/// 清单顶层：全部字段可缺失，逐项给出中文错误；未知字段直接拒绝（serde）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawManifest {
    schema: Option<i64>,
    plugin: Option<RawPlugin>,
    run: Option<RawRun>,
    health: Option<RawHealth>,
    restart: Option<super::raw::RawRestart>,
    logs: Option<RawLogs>,
    ui: Option<RawUi>,
    schedule: Option<RawSchedule>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawPlugin {
    id: Option<String>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    homepage: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawRun {
    command: Option<String>,
    args: Option<Vec<String>>,
    working_dir: Option<String>,
    env: Option<BTreeMap<String, String>>,
    env_proxy: Option<String>,
    ports: Option<Vec<i64>>,
    auto_start: Option<bool>,
    elevated: Option<bool>,
}

/// [health]：与配置里的 RawHealth 相比**不接受 path**（心跳路径由应用管理）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawHealth {
    #[serde(rename = "type")]
    kind: Option<String>,
    interval_ms: Option<i64>,
    timeout_ms: Option<i64>,
    failure_threshold: Option<i64>,
    startup_grace_ms: Option<i64>,
    url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_int_list")]
    expect_status: Option<Vec<i64>>,
    expect_body_contains: Option<String>,
    max_age_ms: Option<i64>,
    needs_human_status: Option<Vec<String>>,
}

/// [logs]：只提供轮转偏好，**不接受 dir**（日志目录由应用管理）。
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawLogs {
    rotation: Option<String>,
    max_file_mb: Option<i64>,
    max_files: Option<i64>,
    max_days: Option<i64>,
    encoding: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawUi {
    panel_url: Option<String>,
    status_url: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawSchedule {
    enabled: Option<bool>,
    start_in_window: Option<bool>,
    stop_outside: Option<bool>,
    #[serde(default)]
    windows: Vec<RawScheduleWindow>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawScheduleWindow {
    days: Option<Vec<String>>,
    start: Option<String>,
    end: Option<String>,
}

/// 接受整数或整数数组（expect_status = 200 / = [200]）。
fn de_opt_int_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<i64>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(i64),
        Many(Vec<i64>),
    }

    Ok(
        Option::<OneOrMany>::deserialize(deserializer)?.map(|value| match value {
            OneOrMany::One(single) => vec![single],
            OneOrMany::Many(many) => many,
        }),
    )
}

// ---------------------------------------------------------------- 校验后的清单

/// 通过完整校验的清单；导入/同步只使用这一层，不再碰原始字段。
struct Manifest {
    /// 清单文件的 canonical 绝对路径。
    manifest_path: PathBuf,
    /// 脚本根目录（清单所在目录）。
    root_dir: PathBuf,
    plugin: PluginMeta,
    run: RunSpec,
    health: Option<HealthSpec>,
    restart: Option<RestartSpec>,
    logs: Option<LogsSpec>,
    ui: UiSpec,
    schedule: Option<ScheduleSpec>,
}

struct PluginMeta {
    id: String,
    name: String,
    version: String,
    description: Option<String>,
    homepage: Option<String>,
}

struct RunSpec {
    /// 已解析为绝对路径（路径形态的 command）或保持裸命令名。
    command: String,
    args: Vec<String>,
    working_dir: PathBuf,
    env: BTreeMap<String, String>,
    env_proxy: EnvProxy,
    ports: Vec<u16>,
    auto_start: bool,
    elevated: bool,
}

/// [health]；只保存清单声明过的字段，未声明的交给配置默认值。
struct HealthSpec {
    kind: HealthKind,
    interval_ms: Option<u64>,
    timeout_ms: Option<u64>,
    failure_threshold: Option<u32>,
    startup_grace_ms: Option<u64>,
    url: Option<String>,
    expect_status: Option<Vec<u16>>,
    expect_body_contains: Option<String>,
    max_age_ms: Option<u64>,
    needs_human_status: Vec<String>,
    /// 清单里声明了、但当前 type 不生效的字段（预览提醒用）。
    ignored: Vec<&'static str>,
}

/// [restart]；只保存清单声明过的字段。
struct RestartSpec {
    mode: Option<RestartMode>,
    initial_delay_ms: Option<u64>,
    max_delay_ms: Option<u64>,
    multiplier: Option<f64>,
    jitter: Option<f64>,
    max_consecutive_failures: Option<u32>,
    stable_after_ms: Option<u64>,
    pause_on_exit_codes: Vec<u32>,
}

/// [logs]；只保存清单声明过的字段（同步时保留用户偏好，不覆盖）。
struct LogsSpec {
    rotation: Option<Rotation>,
    max_file_mb: Option<u32>,
    max_files: Option<u32>,
    max_days: Option<u32>,
    encoding: Option<Encoding>,
}

struct UiSpec {
    panel_url: Option<String>,
}

/// [schedule]。
///
/// v1 清单兼容字段 `start_in_window` / `stop_outside` 自 2026-11-15 起被忽略：
/// 保留两个 `Option<bool>` 只为让旧清单仍能通过 `deny_unknown_fields` 解析，
/// **不要**把它们写进任务配置或清单模板。
struct ScheduleSpec {
    enabled: bool,
    #[allow(dead_code)]
    start_in_window: Option<bool>,
    #[allow(dead_code)]
    stop_outside: Option<bool>,
    windows: Vec<ScheduleWindow>,
}

struct ScheduleWindow {
    days: Vec<Weekday>,
    start: NaiveTime,
    end: NaiveTime,
}

impl Manifest {
    /// 读取并校验清单；路径可以是清单文件，也可以是脚本目录。
    fn load(manifest_path: &Path) -> Result<Self, PluginError> {
        let path = resolve_manifest(manifest_path)?;
        let bytes = fs::read(&path).map_err(|error| PluginError::Read {
            message: format!("读取插件清单 {} 失败：{error}", path.display()),
        })?;
        if bytes.len() > MAX_MANIFEST_BYTES {
            return Err(PluginError::Read {
                message: format!(
                    "插件清单 {} 超过 64 KiB（当前 {} 字节）",
                    path.display(),
                    bytes.len()
                ),
            });
        }
        let text = String::from_utf8(bytes).map_err(|error| PluginError::Read {
            message: format!("插件清单 {} 必须是 UTF-8 编码：{error}", path.display()),
        })?;
        let raw: RawManifest = toml::from_str(&text).map_err(|error| PluginError::Parse {
            message: format!("解析插件清单 {} 失败：{error}", path.display()),
        })?;
        Self::from_raw(path, raw)
    }

    fn from_raw(path: PathBuf, raw: RawManifest) -> Result<Self, PluginError> {
        let schema = raw
            .schema
            .ok_or_else(|| invalid(format!("缺少 schema（当前清单版本为 {MANIFEST_SCHEMA}）")))?;
        if schema != MANIFEST_SCHEMA {
            return Err(invalid(format!(
                "不支持的清单 schema 版本 schema={schema}（当前仅支持 schema={MANIFEST_SCHEMA}，请升级 {} 或改用 schema=1）",
                crate::brand::DISPLAY_NAME
            )));
        }

        let root_dir = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();

        let plugin = match raw.plugin {
            Some(plugin) => PluginMeta::from_raw(plugin)?,
            None => return Err(invalid("缺少 [plugin] 表（id/name/version 必填）")),
        };
        let run = match raw.run {
            Some(run) => RunSpec::from_raw(run, &root_dir)?,
            None => return Err(invalid("缺少 [run] 表（command 必填）")),
        };
        let health = raw.health.as_ref().map(HealthSpec::from_raw).transpose()?;
        let restart = raw
            .restart
            .as_ref()
            .map(RestartSpec::from_raw)
            .transpose()?;
        let logs = raw.logs.as_ref().map(LogsSpec::from_raw).transpose()?;
        let ui = UiSpec::from_raw(raw.ui)?;
        let schedule = raw
            .schedule
            .as_ref()
            .map(ScheduleSpec::from_raw)
            .transpose()?;

        Ok(Self {
            manifest_path: path,
            root_dir,
            plugin,
            run,
            health,
            restart,
            logs,
            ui,
            schedule,
        })
    }

    fn preview(&self, existing_task_ids: &[String]) -> PluginPreview {
        let existing_task_id = existing_task_ids
            .iter()
            .any(|id| id == &self.plugin.id)
            .then(|| self.plugin.id.clone());

        PluginPreview {
            manifest_path: path_text(&self.manifest_path),
            root_dir: path_text(&self.root_dir),
            plugin_id: self.plugin.id.clone(),
            name: self.plugin.name.clone(),
            version: self.plugin.version.clone(),
            description: self.plugin.description.clone(),
            homepage: self.plugin.homepage.clone(),
            command: self.run.command.clone(),
            args: self.run.args.clone(),
            working_dir: path_text(&self.run.working_dir),
            ports: self.run.ports.clone(),
            env: self.run.env.clone(),
            env_proxy: self.run.env_proxy.as_str().to_owned(),
            panel_url: self.ui.panel_url.clone(),
            health_type: self
                .health
                .as_ref()
                .map(|health| health.kind.as_str().to_owned())
                .unwrap_or_else(|| HealthKind::Process.as_str().to_owned()),
            health_url: self.health.as_ref().and_then(|health| health.url.clone()),
            schedule: self
                .schedule
                .as_ref()
                .map(|schedule| PluginSchedulePreview {
                    enabled: schedule.enabled,
                    windows: schedule
                        .windows
                        .iter()
                        .map(|window| PluginWindowPreview {
                            days: window
                                .days
                                .iter()
                                .map(|day| day.as_str().to_owned())
                                .collect(),
                            start: format_time(window.start),
                            end: format_time(window.end),
                        })
                        .collect(),
                }),
            task_id: self.plugin.id.clone(),
            existing_task_id,
            warnings: self.warnings(),
        }
    }

    /// 不影响导入的提醒；真正的错误在解析阶段就返回了。
    fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        if !looks_like_path(&self.run.command) && !command_in_path(&self.run.command) {
            warnings.push(format!(
                "在 PATH 中找不到命令 \"{}\"：启动时会再校验（PATH 可能变化）",
                self.run.command
            ));
        }
        match &self.health {
            None => warnings.push("清单未声明 [health]：导入后按进程级监督（粗粒度）".to_owned()),
            Some(health) if health.kind == HealthKind::Process => {
                warnings.push("health.type = process：只做进程级监督（粗粒度）".to_owned());
            }
            Some(health) => {
                for field in &health.ignored {
                    warnings.push(format!("{field} 仅在其它 health.type 下生效，已忽略"));
                }
            }
        }
        if self.run.elevated {
            warnings.push(
                "elevated=true：当前版本未实现提权，运行时仍为普通权限（建议保持 false）"
                    .to_owned(),
            );
        }
        if let Some(panel_url) = &self.ui.panel_url
            && !url_host_is_loopback(panel_url)
        {
            warnings.push(format!(
                "ui.panel_url 的主机不是本机（localhost/127.0.0.1）：{panel_url}"
            ));
        }
        if !is_semver_like(&self.plugin.version) {
            warnings.push(format!(
                "plugin.version \"{}\" 不是 semver（仅用于版本提示）",
                self.plugin.version
            ));
        }
        warnings
    }
}

impl PluginMeta {
    fn from_raw(raw: RawPlugin) -> Result<Self, PluginError> {
        let id = plugin_id(raw.id.as_deref())?;
        let name = required_text(raw.name.as_deref(), "plugin.name")?;
        let name_len = name.chars().count();
        if name_len == 0 || name_len > 64 {
            return Err(invalid("plugin.name 长度必须为 1–64 字符"));
        }
        let version = required_text(raw.version.as_deref(), "plugin.version")?;
        let version_len = version.chars().count();
        if version_len == 0 || version_len > 32 {
            return Err(invalid("plugin.version 长度必须为 1–32 字符"));
        }
        let description = optional_text(raw.description.as_deref());
        if let Some(description) = &description
            && description.chars().count() > 256
        {
            return Err(invalid("plugin.description 最多 256 字符"));
        }
        let homepage = match optional_text(raw.homepage.as_deref()) {
            Some(homepage) => Some(validate_url(&homepage, "plugin.homepage")?),
            None => None,
        };
        Ok(Self {
            id,
            name,
            version,
            description,
            homepage,
        })
    }
}

impl RunSpec {
    fn from_raw(raw: RawRun, root_dir: &Path) -> Result<Self, PluginError> {
        let command_text = required_text(raw.command.as_deref(), "run.command")?;
        ensure_text_len(&command_text, 1024, "run.command")?;
        let command_text = expand_manifest_text(&command_text, "run.command")?;
        if command_text.contains('"') {
            return Err(invalid("run.command 不能包含引号；请把参数放进 run.args"));
        }
        let command = resolve_command(root_dir, &command_text)?;

        let args = raw.args.unwrap_or_default();
        if args.len() > 64 {
            return Err(invalid(format!(
                "run.args 最多 64 个（当前 {} 个）",
                args.len()
            )));
        }
        for (index, arg) in args.iter().enumerate() {
            ensure_clean_text(arg, &format!("run.args[{index}]"))?;
            ensure_text_len(arg, 1024, &format!("run.args[{index}]"))?;
        }

        let working_dir_text =
            optional_text(raw.working_dir.as_deref()).unwrap_or_else(|| ".".to_owned());
        ensure_text_len(&working_dir_text, 1024, "run.working_dir")?;
        let working_dir_text = expand_manifest_text(&working_dir_text, "run.working_dir")?;
        let working_dir = resolve_path(root_dir, &working_dir_text, "run.working_dir")?;
        if !working_dir.is_dir() {
            return Err(PluginError::MissingPath {
                path: path_text(&working_dir),
            });
        }

        let raw_env = raw.env.unwrap_or_default();
        if raw_env.len() > 64 {
            return Err(invalid(format!(
                "run.env 最多 64 项（当前 {} 项）",
                raw_env.len()
            )));
        }
        let env = validate_env(raw_env)?;

        let env_proxy = match optional_text(raw.env_proxy.as_deref()) {
            None => EnvProxy::BypassLocalhost,
            Some(text) => EnvProxy::parse(&text).ok_or_else(|| {
                invalid(format!(
                    "run.env_proxy 未知取值 \"{text}\"（期望 {}）",
                    EnvProxy::VARIANTS.join(" | ")
                ))
            })?,
        };
        let ports = validate_ports(raw.ports.as_deref().unwrap_or(&[]))?;
        Ok(Self {
            command,
            args,
            working_dir,
            env,
            env_proxy,
            ports,
            auto_start: raw.auto_start.unwrap_or(false),
            elevated: raw.elevated.unwrap_or(false),
        })
    }
}

impl HealthSpec {
    fn from_raw(raw: &RawHealth) -> Result<Self, PluginError> {
        // 写了 [health] 但没写 type：按文档默认 process。
        let kind = match optional_text(raw.kind.as_deref()) {
            None => HealthKind::Process,
            Some(text) => HealthKind::parse(&text).ok_or_else(|| {
                invalid(format!(
                    "health.type 未知取值 \"{text}\"（期望 {}）",
                    HealthKind::VARIANTS.join(" | ")
                ))
            })?,
        };

        let mut spec = Self {
            kind,
            interval_ms: optional_int(raw.interval_ms, 1000, 300_000, "health.interval_ms")?,
            timeout_ms: optional_int(raw.timeout_ms, 100, 30_000, "health.timeout_ms")?,
            failure_threshold: optional_int(
                raw.failure_threshold,
                1,
                10,
                "health.failure_threshold",
            )?
            .map(|value| value as u32),
            startup_grace_ms: optional_int(
                raw.startup_grace_ms,
                0,
                i64::MAX,
                "health.startup_grace_ms",
            )?,
            url: None,
            expect_status: None,
            expect_body_contains: None,
            max_age_ms: None,
            needs_human_status: Vec::new(),
            ignored: Vec::new(),
        };

        match kind {
            HealthKind::Http => {
                let raw_url = required_text(raw.url.as_deref(), "health.url")?;
                if !raw_url.to_ascii_lowercase().starts_with("http://") {
                    return Err(invalid(format!(
                        "health.url 必须以 http:// 开头（v1 不支持 https）：{raw_url}"
                    )));
                }
                spec.url = Some(validate_url(&raw_url, "health.url")?);
                spec.expect_status = Some(validate_status_codes(raw.expect_status.as_deref())?);
                spec.expect_body_contains =
                    validate_body_contains(raw.expect_body_contains.as_deref())?;
                if raw.max_age_ms.is_some() {
                    spec.ignored.push("health.max_age_ms");
                }
                if raw.needs_human_status.is_some() {
                    spec.ignored.push("health.needs_human_status");
                }
            }
            HealthKind::Heartbeat => {
                let max_age = raw
                    .max_age_ms
                    .ok_or_else(|| invalid("health.type = \"heartbeat\" 时必须声明 max_age_ms"))?;
                spec.max_age_ms = optional_int(Some(max_age), 1000, i64::MAX, "health.max_age_ms")?;
                spec.needs_human_status =
                    validate_status_strings(raw.needs_human_status.as_deref())?;
                if raw.url.is_some() {
                    spec.ignored.push("health.url");
                }
                if raw.expect_status.is_some() {
                    spec.ignored.push("health.expect_status");
                }
                if raw.expect_body_contains.is_some() {
                    spec.ignored.push("health.expect_body_contains");
                }
            }
            HealthKind::Process => {
                if raw.url.is_some() {
                    spec.ignored.push("health.url");
                }
                if raw.expect_status.is_some() {
                    spec.ignored.push("health.expect_status");
                }
                if raw.expect_body_contains.is_some() {
                    spec.ignored.push("health.expect_body_contains");
                }
                if raw.max_age_ms.is_some() {
                    spec.ignored.push("health.max_age_ms");
                }
                if raw.needs_human_status.is_some() {
                    spec.ignored.push("health.needs_human_status");
                }
            }
        }
        Ok(spec)
    }
}

impl RestartSpec {
    fn from_raw(raw: &super::raw::RawRestart) -> Result<Self, PluginError> {
        let mode = match optional_text(raw.mode.as_deref()) {
            Some(text) => Some(RestartMode::parse(&text).ok_or_else(|| {
                invalid(format!(
                    "restart.mode 未知取值 \"{text}\"（期望 {}）",
                    RestartMode::VARIANTS.join(" | ")
                ))
            })?),
            None => None,
        };
        let initial_delay_ms =
            optional_int(raw.initial_delay_ms, 0, 600_000, "restart.initial_delay_ms")?;
        let max_delay_ms = optional_int(raw.max_delay_ms, 0, 600_000, "restart.max_delay_ms")?;
        // 与配置校验一致：按"缺省值补全后"比较，避免导入出结构错误。
        let effective_initial = initial_delay_ms.unwrap_or(3000);
        let effective_max = max_delay_ms.unwrap_or(60_000);
        if effective_max < effective_initial {
            return Err(invalid(format!(
                "restart.max_delay_ms（{effective_max}）不能小于 initial_delay_ms（{effective_initial}）"
            )));
        }
        Ok(Self {
            mode,
            initial_delay_ms,
            max_delay_ms,
            multiplier: optional_float(raw.multiplier, 1.0, 10.0, "restart.multiplier")?,
            jitter: optional_float(raw.jitter, 0.0, 0.5, "restart.jitter")?,
            max_consecutive_failures: optional_int(
                raw.max_consecutive_failures,
                0,
                1000,
                "restart.max_consecutive_failures",
            )?
            .map(|value| value as u32),
            stable_after_ms: optional_int(
                raw.stable_after_ms,
                0,
                i64::MAX,
                "restart.stable_after_ms",
            )?,
            pause_on_exit_codes: validate_exit_codes(raw.pause_on_exit_codes.as_deref())?,
        })
    }
}

impl LogsSpec {
    fn from_raw(raw: &RawLogs) -> Result<Self, PluginError> {
        Ok(Self {
            rotation: optional_enum(raw.rotation.as_deref(), "logs.rotation", Rotation::parse)?,
            max_file_mb: optional_int(raw.max_file_mb, 1, 1024, "logs.max_file_mb")?
                .map(|value| value as u32),
            max_files: optional_int(raw.max_files, 1, 50, "logs.max_files")?
                .map(|value| value as u32),
            max_days: optional_int(raw.max_days, 1, 365, "logs.max_days")?
                .map(|value| value as u32),
            encoding: optional_enum(raw.encoding.as_deref(), "logs.encoding", Encoding::parse)?,
        })
    }
}

impl UiSpec {
    fn from_raw(raw: Option<RawUi>) -> Result<Self, PluginError> {
        let Some(raw) = raw else {
            return Ok(Self { panel_url: None });
        };
        let panel_url = optional_text(raw.panel_url.as_deref())
            .map(|url| validate_url(&url, "ui.panel_url"))
            .transpose()?;
        // status_url 仅预览展示；当前任务模型/预览 DTO 无该字段，校验后丢弃。
        if let Some(status_url) = optional_text(raw.status_url.as_deref()) {
            validate_url(&status_url, "ui.status_url")?;
        }
        Ok(Self { panel_url })
    }
}

impl ScheduleSpec {
    fn from_raw(raw: &RawSchedule) -> Result<Self, PluginError> {
        // 文档默认 enabled=false（导入后属用户偏好，同步保留）。
        let enabled = raw.enabled.unwrap_or(false);
        if raw.windows.len() > 16 {
            return Err(invalid(format!(
                "schedule.windows 最多 16 个（当前 {} 个）",
                raw.windows.len()
            )));
        }
        let mut windows = Vec::new();
        for (index, window) in raw.windows.iter().enumerate() {
            let field = format!("schedule.windows[{index}]");
            let raw_days = window.days.as_deref().unwrap_or(&[]);
            if raw_days.is_empty() {
                return Err(invalid(format!(
                    "{field}.days 不能为空（至少一个 mon..sun）"
                )));
            }
            let mut days = Vec::new();
            for raw_day in raw_days {
                let Some(day) = Weekday::parse(raw_day) else {
                    return Err(invalid(format!(
                        "{field}.days 含未知星期 \"{raw_day}\"（只接受 mon/tue/wed/thu/fri/sat/sun）"
                    )));
                };
                if !days.contains(&day) {
                    days.push(day);
                }
            }
            // 与配置校验保持一致：按 mon..sun 排序。
            days.sort_by_key(|day| {
                Weekday::ALL
                    .iter()
                    .position(|candidate| candidate == day)
                    .unwrap_or(usize::MAX)
            });
            let start = parse_clock(window.start.as_deref(), &format!("{field}.start"))?;
            let end = parse_clock(window.end.as_deref(), &format!("{field}.end"))?;
            if start == end {
                return Err(invalid(format!(
                    "{field}.start 与 end 不能相同（如需全天请拆成两段）"
                )));
            }
            windows.push(ScheduleWindow { days, start, end });
        }
        if enabled && windows.is_empty() {
            return Err(invalid(
                "schedule.enabled=true 但没有可用窗口：请添加 [[schedule.windows]] 或把 enabled 设为 false",
            ));
        }
        Ok(Self {
            enabled,
            // v1 清单兼容字段：只读入、不落盘（v2 起语义固定）。
            start_in_window: raw.start_in_window,
            stop_outside: raw.stop_outside,
            windows,
        })
    }
}

// ---------------------------------------------------------------- 校验辅助

fn resolve_manifest(manifest_path: &Path) -> Result<PathBuf, PluginError> {
    let candidate = if manifest_path.is_dir() {
        let primary = manifest_path.join(MANIFEST_NAME);
        if primary.is_file() {
            primary
        } else {
            // 兼容旧名 campussentry.plugin.toml
            manifest_path.join(LEGACY_MANIFEST_NAME)
        }
    } else {
        manifest_path.to_path_buf()
    };
    if !candidate.is_file() {
        return Err(PluginError::MissingPath {
            path: path_text(&candidate),
        });
    }
    let canonical = candidate
        .canonicalize()
        .map_err(|error| PluginError::Read {
            message: format!("解析插件清单路径 {} 失败：{error}", candidate.display()),
        })?;
    Ok(simplify_path(canonical))
}

/// 去掉 Windows canonicalize 返回的 verbatim 前缀，便于展示与写回配置。
fn simplify_path(path: PathBuf) -> PathBuf {
    let separator = char::from(92);
    let verbatim = format!("{separator}{separator}?{separator}");
    let verbatim_unc = format!("{verbatim}UNC{separator}");
    let text = path.to_string_lossy().into_owned();
    if let Some(rest) = text.strip_prefix(&verbatim_unc) {
        return PathBuf::from(format!("{separator}{separator}{rest}"));
    }
    if let Some(rest) = text.strip_prefix(&verbatim) {
        return PathBuf::from(rest.to_owned());
    }
    path
}

fn required_text(raw: Option<&str>, field: &str) -> Result<String, PluginError> {
    match raw.map(str::trim) {
        None | Some("") => Err(invalid(format!("缺少 {field}（必填）"))),
        Some(text) => Ok(text.to_owned()),
    }
}

fn optional_text(raw: Option<&str>) -> Option<String> {
    raw.map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
}

fn plugin_id(raw: Option<&str>) -> Result<String, PluginError> {
    let id = required_text(raw, "plugin.id")?;
    let bytes = id.as_bytes();
    // 与配置层的任务 id 规则一致（docs/15 §3.3）：小写字母/数字开头，
    // 之后可含小写字母、数字、_、-。
    let valid = bytes.len() <= 64
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit())
        && bytes.iter().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
        });
    if !valid {
        return Err(invalid(
            "plugin.id 只能使用小写字母、数字、_ 和 -（以字母或数字开头，长度 1–64）",
        ));
    }
    Ok(id)
}

fn ensure_clean_text(value: &str, field: &str) -> Result<(), PluginError> {
    if value.chars().any(char::is_control) {
        return Err(invalid(format!("{field} 不能包含 NUL/控制字符")));
    }
    Ok(())
}

fn ensure_text_len(value: &str, max: usize, field: &str) -> Result<(), PluginError> {
    if value.chars().count() > max {
        return Err(invalid(format!("{field} 最多 {max} 字符")));
    }
    Ok(())
}

/// 清单里的路径先做 %VAR% 展开（docs/15 §3.4）；未知变量是导入错误。
fn expand_manifest_text(value: &str, field: &str) -> Result<String, PluginError> {
    super::expand::expand(value, &super::SystemEnv)
        .map_err(|error| invalid(format!("{field} {error}")))
}

fn optional_int(
    raw: Option<i64>,
    min: i64,
    max: i64,
    field: &str,
) -> Result<Option<u64>, PluginError> {
    let Some(value) = raw else {
        return Ok(None);
    };
    if value < min || value > max {
        let hint = if max == i64::MAX {
            format!("不能小于 {min}")
        } else {
            format!("超出范围 {min}–{max}")
        };
        return Err(invalid(format!("{field} 取值 {value} {hint}")));
    }
    Ok(Some(value as u64))
}

fn optional_float(
    raw: Option<f64>,
    min: f64,
    max: f64,
    field: &str,
) -> Result<Option<f64>, PluginError> {
    let Some(value) = raw else {
        return Ok(None);
    };
    if !value.is_finite() || !(min..=max).contains(&value) {
        return Err(invalid(format!(
            "{field} 取值 {value} 超出范围 {min}–{max}"
        )));
    }
    Ok(Some(value))
}

fn optional_enum<T: Copy>(
    raw: Option<&str>,
    field: &str,
    parse: impl Fn(&str) -> Option<T>,
) -> Result<Option<T>, PluginError> {
    match optional_text(raw) {
        None => Ok(None),
        Some(text) => parse(&text)
            .map(Some)
            .ok_or_else(|| invalid(format!("{field} 未知取值 \"{text}\""))),
    }
}

fn validate_ports(raw: &[i64]) -> Result<Vec<u16>, PluginError> {
    if raw.len() > 32 {
        return Err(invalid(format!(
            "run.ports 最多 32 个（当前 {} 个）",
            raw.len()
        )));
    }
    let mut ports = Vec::new();
    for value in raw {
        if !(1..=65535).contains(value) {
            return Err(invalid(format!("run.ports 端口 {value} 超出范围 1–65535")));
        }
        let port = *value as u16;
        if !ports.contains(&port) {
            ports.push(port);
        }
    }
    Ok(ports)
}

fn validate_env(raw: BTreeMap<String, String>) -> Result<BTreeMap<String, String>, PluginError> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    for (key, value) in raw {
        let key = key.trim();
        if key.is_empty() {
            return Err(invalid("run.env 的环境变量名不能为空"));
        }
        ensure_clean_text(key, "run.env 变量名")?;
        ensure_text_len(key, 128, "run.env 变量名")?;
        ensure_text_len(&value, 2048, "run.env 变量值")?;
        if key.to_ascii_uppercase().starts_with("CAMPUS_SENTRY_") {
            return Err(invalid(format!(
                "run.env 不能设置保留变量 {key}（CAMPUS_SENTRY_ 前缀由核心注入）"
            )));
        }
        if env
            .keys()
            .any(|existing| existing.eq_ignore_ascii_case(key))
        {
            continue;
        }
        env.insert(key.to_owned(), value);
    }
    Ok(env)
}

fn validate_status_codes(raw: Option<&[i64]>) -> Result<Vec<u16>, PluginError> {
    let mut codes = Vec::new();
    for value in raw.unwrap_or(&[]) {
        if !(100..=599).contains(value) {
            return Err(invalid(format!(
                "health.expect_status 状态码 {value} 超出范围 100–599"
            )));
        }
        if codes.len() >= 32 {
            return Err(invalid("health.expect_status 最多 32 个"));
        }
        let code = *value as u16;
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    if codes.is_empty() {
        codes.push(200);
    }
    Ok(codes)
}

fn validate_body_contains(raw: Option<&str>) -> Result<Option<String>, PluginError> {
    let Some(body) = optional_text(raw) else {
        return Ok(None);
    };
    if body.chars().count() > 256 {
        return Err(invalid("health.expect_body_contains 最多 256 字符"));
    }
    Ok(Some(body))
}

fn validate_status_strings(raw: Option<&[String]>) -> Result<Vec<String>, PluginError> {
    let mut statuses = Vec::new();
    for value in raw.unwrap_or(&[]) {
        let value = value.trim();
        if value.is_empty() {
            return Err(invalid("health.needs_human_status 不能包含空字符串"));
        }
        if statuses.len() >= 16 {
            return Err(invalid("health.needs_human_status 最多 16 个"));
        }
        if !statuses.iter().any(|item| item == value) {
            statuses.push(value.to_owned());
        }
    }
    Ok(statuses)
}

fn validate_exit_codes(raw: Option<&[i64]>) -> Result<Vec<u32>, PluginError> {
    let mut codes = Vec::new();
    for value in raw.unwrap_or(&[]) {
        if !(0..=255).contains(value) {
            return Err(invalid(format!(
                "restart.pause_on_exit_codes 退出码 {value} 超出范围 0–255"
            )));
        }
        if codes.len() >= 32 {
            return Err(invalid("restart.pause_on_exit_codes 最多 32 个"));
        }
        let code = *value as u32;
        if !codes.contains(&code) {
            codes.push(code);
        }
    }
    Ok(codes)
}

fn validate_url(url: &str, field: &str) -> Result<String, PluginError> {
    ensure_clean_text(url, field)?;
    if url.chars().count() > 2048 {
        return Err(invalid(format!("{field} 最多 2048 字符")));
    }
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .ok_or_else(|| invalid(format!("{field} 必须以 http:// 或 https:// 开头")))?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err(invalid(format!("{field} 缺少主机名")));
    }
    if authority.contains('@') {
        return Err(invalid(format!("{field} 不能包含用户名/密码")));
    }
    if authority.contains(' ') || authority.contains('\\') {
        return Err(invalid(format!("{field} 主机名包含非法字符")));
    }
    Ok(url.to_owned())
}

/// URL 主机是否为本机地址（预览提醒用）。
fn url_host_is_loopback(url: &str) -> bool {
    let lower = url.to_ascii_lowercase();
    let Some(rest) = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
    else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host = authority.split(':').next().unwrap_or("");
    matches!(host, "localhost" | "127.0.0.1" | "[::1]" | "::1")
}

fn is_semver_like(version: &str) -> bool {
    let core = version.split(['-', '+']).next().unwrap_or("");
    let parts: Vec<&str> = core.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()))
}

fn parse_clock(raw: Option<&str>, field: &str) -> Result<NaiveTime, PluginError> {
    let text = optional_text(raw).ok_or_else(|| invalid(format!("缺少 {field}（HH:MM）")))?;
    parse_time(&text)
        .ok_or_else(|| invalid(format!("{field} 不是合法时间 \"{text}\"（期望 HH:MM）")))
}

// ---------------------------------------------------------------- 路径

fn resolve_command(root_dir: &Path, command: &str) -> Result<String, PluginError> {
    if looks_like_path(command) {
        let path = resolve_path(root_dir, command, "run.command")?;
        if !path.is_file() {
            return Err(PluginError::MissingPath {
                path: path_text(&path),
            });
        }
        Ok(path_text(&path))
    } else {
        ensure_clean_text(command, "run.command")?;
        Ok(command.to_owned())
    }
}

fn resolve_path(base: &Path, value: &str, field: &str) -> Result<PathBuf, PluginError> {
    validate_path_text(value, field)?;
    let path = Path::new(value);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    };
    Ok(normalize_path(&joined))
}

/// 只接受本地路径（docs/15 §3.4）：拒绝 UNC/设备路径、盘符相对路径、控制字符。
fn validate_path_text(value: &str, field: &str) -> Result<(), PluginError> {
    if value.chars().any(char::is_control) {
        return Err(invalid(format!("{field} 不能包含 NUL/控制字符")));
    }
    if value.starts_with("\\\\") || value.starts_with("//") {
        return Err(invalid(format!(
            "{field} 不支持 UNC/设备路径（只支持本地绝对路径或相对路径）：{value}"
        )));
    }
    if has_drive_prefix(value) {
        let rest = &value[2..];
        if !(rest.starts_with('\\') || rest.starts_with('/')) {
            return Err(invalid(format!(
                "{field} 不支持盘符相对路径（请写成 D:\\\\...）：{value}"
            )));
        }
    }
    Ok(())
}

/// 纯词法归一化（去掉 . 、处理 ..）；不要求路径存在。
fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(component.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn path_text(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn looks_like_path(command: &str) -> bool {
    command.contains('/') || command.contains('\\') || has_drive_prefix(command)
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// 裸命令名是否在 PATH 中（预览 warning；启动时还会校验）。
fn command_in_path(name: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    let extensions = std::env::var("PATHEXT").unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_owned());
    for dir in std::env::split_paths(&path) {
        if dir.join(name).is_file() {
            return true;
        }
        for extension in extensions.split(';').filter(|item| !item.is_empty()) {
            if dir.join(format!("{name}{extension}")).is_file() {
                return true;
            }
        }
    }
    false
}

// ---------------------------------------------------------------- 预览入口

/// 读取并校验清单；existing_task_ids 用于标记 id 冲突。
pub fn load_preview(
    manifest_path: &Path,
    existing_task_ids: &[String],
) -> Result<PluginPreview, PluginError> {
    let manifest = Manifest::load(manifest_path)?;
    let mut preview = manifest.preview(existing_task_ids);
    if preview.existing_task_id.is_some() {
        preview.warnings.push(format!(
            "配置中已存在 id \"{}\" 的任务：导入会失败，请改用同步更新或修改清单里的 plugin.id",
            preview.task_id
        ));
    }
    Ok(preview)
}

// ---------------------------------------------------------------- 版本提示

/// 只读取清单里的 `plugin.version`（C8 可更新提示）。
///
/// 复用完整校验：清单损坏/工作目录不存在时返回错误，调用方只做 debug 记录，
/// 不让一份坏清单影响其余任务或快照生成。
pub fn manifest_version(path: &Path) -> Result<String, PluginError> {
    let manifest = Manifest::load(path)?;
    Ok(manifest.plugin.version.clone())
}

/// 清单版本是否比已存版本新（纯函数）。
///
/// - `current` 为 `None`/空：已存版本未知，提示同步（返回 true）；
/// - 两侧都能解析为 `.` 分隔的十进制数字段：逐段比较、缺段补 0，
///   `candidate` 严格更大才算更新（`1.10 > 1.2`，`2.0.0 == 2.0`）；
/// - 任一段为空或非数字（`1.0-beta`、`v1.2` 等）：返回 false，避免误报。
pub fn version_is_newer(candidate: &str, current: Option<&str>) -> bool {
    let Some(current) = current.map(str::trim).filter(|text| !text.is_empty()) else {
        return true;
    };
    let (Some(candidate), Some(current)) = (version_segments(candidate), version_segments(current))
    else {
        return false;
    };
    let len = candidate.len().max(current.len());
    for index in 0..len {
        let left = candidate.get(index).copied().unwrap_or(0);
        let right = current.get(index).copied().unwrap_or(0);
        if left != right {
            return left > right;
        }
    }
    false
}

/// 版本号拆成十进制数字段；任一段为空/非数字/溢出时返回 `None`。
fn version_segments(version: &str) -> Option<Vec<u64>> {
    let version = version.trim();
    if version.is_empty() {
        return None;
    }
    version
        .split('.')
        .map(|part| {
            let part = part.trim();
            if part.is_empty() || !part.bytes().all(|byte| byte.is_ascii_digit()) {
                return None;
            }
            part.parse::<u64>().ok()
        })
        .collect()
}

// ---------------------------------------------------------------- 导入 / 同步

/// 把清单导入为新的 [[tasks]]；任务已存在时返回 Invalid（由 UI 引导同步）。
pub fn import_plugin(
    config_path: &Path,
    manifest_path: &Path,
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<PluginWriteOutcome, PluginError> {
    let manifest = Manifest::load(manifest_path)?;
    editor::with_save_lock(|| {
        let ConfigState {
            text,
            revision,
            mut doc,
        } = open_config(config_path, expected_revision)?;
        ensure_tasks(&mut doc)?;
        if find_task_index(&doc, &manifest.plugin.id)?.is_some() {
            return Err(invalid(format!(
                "任务 id \"{}\" 已存在；请使用同步更新，或修改清单里的 plugin.id",
                manifest.plugin.id
            )));
        }
        tasks_mut(&mut doc)?.push(task_table(&manifest));
        finish(
            config_path,
            &text,
            &revision,
            doc,
            data_dir,
            &manifest.plugin.id,
        )
    })
}

/// 用清单更新已有任务的"清单拥有字段"，保留用户偏好字段。
pub fn sync_plugin(
    config_path: &Path,
    task_id: &str,
    expected_revision: &str,
    data_dir: Option<&Path>,
) -> Result<PluginWriteOutcome, PluginError> {
    editor::with_save_lock(|| {
        let ConfigState {
            text,
            revision,
            mut doc,
        } = open_config(config_path, expected_revision)?;
        let index = find_task_index(&doc, task_id)?.ok_or_else(|| PluginError::TaskNotFound {
            task_id: task_id.to_owned(),
        })?;
        let (stored_id, stored_manifest) = {
            let table = task_at(&doc, index)?;
            (
                table
                    .get("plugin_id")
                    .and_then(Item::as_str)
                    .map(str::to_owned),
                table
                    .get("plugin_manifest")
                    .and_then(Item::as_str)
                    .map(str::to_owned),
            )
        };
        let Some(manifest_path) = stored_manifest else {
            return Err(invalid(format!(
                "任务 {task_id} 不是由插件清单导入的（缺少 plugin_manifest）"
            )));
        };
        let Some(stored_id) = stored_id else {
            return Err(invalid(format!(
                "任务 {task_id} 缺少 plugin_id，无法与清单对应"
            )));
        };
        let manifest = Manifest::load(Path::new(&manifest_path))?;
        if manifest.plugin.id != stored_id {
            return Err(invalid(format!(
                "清单 plugin.id（{}）与任务记录的 plugin_id（{stored_id}）不一致",
                manifest.plugin.id
            )));
        }
        apply_manifest(task_at_mut(&mut doc, index)?, &manifest);
        finish(config_path, &text, &revision, doc, data_dir, task_id)
    })
}

// ---------------------------------------------------------------- 配置读写

struct ConfigState {
    text: String,
    revision: String,
    doc: DocumentMut,
}

fn open_config(config_path: &Path, expected_revision: &str) -> Result<ConfigState, PluginError> {
    let current = editor::read_config_text(config_path).map_err(map_editor)?;
    if current.revision != expected_revision {
        return Err(PluginError::Conflict {
            current_revision: current.revision,
        });
    }
    let doc = current
        .text
        .parse::<DocumentMut>()
        .map_err(|error| PluginError::Parse {
            message: format!("配置文件 {} 不是合法 TOML：{error}", config_path.display()),
        })?;
    Ok(ConfigState {
        text: current.text,
        revision: current.revision,
        doc,
    })
}

/// 渲染 → 未变则 no-op → 否则走 editor::commit（临时文件 + 完整校验 + 原子替换）。
fn finish(
    config_path: &Path,
    original_text: &str,
    revision: &str,
    doc: DocumentMut,
    data_dir: Option<&Path>,
    task_id: &str,
) -> Result<PluginWriteOutcome, PluginError> {
    let rendered = doc.to_string();
    if rendered == original_text {
        return Ok(PluginWriteOutcome {
            task_id: task_id.to_owned(),
            changed: false,
            revision: revision.to_owned(),
        });
    }
    let (_, revision) =
        editor::commit(config_path, rendered.as_bytes(), data_dir).map_err(map_editor)?;
    Ok(PluginWriteOutcome {
        task_id: task_id.to_owned(),
        changed: true,
        revision,
    })
}

fn map_editor(error: EditorError) -> PluginError {
    match error {
        EditorError::Read { message, .. } => PluginError::Read { message },
        EditorError::Write { message, .. } => PluginError::Write { message },
        // 生成的配置未通过完整校验：按清单内容问题呈现，提示用户改清单。
        EditorError::Parse { message, .. } => PluginError::Invalid { message },
        EditorError::Conflict { current_revision } => PluginError::Conflict { current_revision },
        // 只有恢复备份会产生该错误；插件写入路径不会走到这里。
        EditorError::BackupMissing { path } => PluginError::Read {
            message: format!("没有可用的配置备份：{}", path.display()),
        },
    }
}

fn ensure_tasks(doc: &mut DocumentMut) -> Result<(), PluginError> {
    if !doc.as_table().contains_key("tasks") {
        doc.as_table_mut()
            .insert("tasks", Item::ArrayOfTables(ArrayOfTables::new()));
    }
    if doc
        .as_table()
        .get("tasks")
        .and_then(Item::as_array_of_tables)
        .is_none()
    {
        return Err(invalid("tasks 不是 [[tasks]] 数组表，无法编辑"));
    }
    Ok(())
}

fn tasks_mut(doc: &mut DocumentMut) -> Result<&mut ArrayOfTables, PluginError> {
    doc.as_table_mut()
        .get_mut("tasks")
        .and_then(Item::as_array_of_tables_mut)
        .ok_or_else(|| invalid("tasks 不是 [[tasks]] 数组表，无法编辑"))
}

fn find_task_index(doc: &DocumentMut, id: &str) -> Result<Option<usize>, PluginError> {
    let Some(item) = doc.as_table().get("tasks") else {
        return Ok(None);
    };
    let tasks = item
        .as_array_of_tables()
        .ok_or_else(|| invalid("tasks 不是 [[tasks]] 数组表，无法编辑"))?;
    Ok(tasks
        .iter()
        .position(|table| table.get("id").and_then(Item::as_str) == Some(id)))
}

fn task_at(doc: &DocumentMut, index: usize) -> Result<&Table, PluginError> {
    doc.as_table()
        .get("tasks")
        .and_then(Item::as_array_of_tables)
        .and_then(|tasks| tasks.get(index))
        .ok_or_else(|| invalid("任务表定位失败"))
}

fn task_at_mut(doc: &mut DocumentMut, index: usize) -> Result<&mut Table, PluginError> {
    doc.as_table_mut()
        .get_mut("tasks")
        .and_then(Item::as_array_of_tables_mut)
        .and_then(|tasks| tasks.get_mut(index))
        .ok_or_else(|| invalid("任务表定位失败"))
}

// ---------------------------------------------------------------- 清单 → 任务表

/// 导入用的新 [[tasks]] 表：清单拥有字段 + 插件来源元数据。
fn task_table(manifest: &Manifest) -> Table {
    let mut table = Table::new();
    // 与上一个任务之间留空行，保持文件可读。
    table.decor_mut().set_prefix("\n");
    insert_str(&mut table, "id", &manifest.plugin.id);
    insert_str(&mut table, "name", &manifest.plugin.name);
    insert_bool(&mut table, "enabled", true);
    apply_manifest(&mut table, manifest);
    insert_str(&mut table, "plugin_id", &manifest.plugin.id);
    insert_str(
        &mut table,
        "plugin_manifest",
        &path_text(&manifest.manifest_path),
    );
    if manifest.run.auto_start {
        insert_bool(&mut table, "auto_start", true);
    }
    if manifest.run.elevated {
        insert_bool(&mut table, "elevated", true);
    }
    if let Some(logs) = &manifest.logs {
        let logs = logs_table(logs);
        if !logs.is_empty() {
            table.insert("logs", Item::Table(logs));
        }
    }
    if let Some(schedule) = &manifest.schedule {
        table.insert("schedule", Item::Table(schedule_table(schedule)));
    }
    table
}

/// 同步：只改清单拥有字段；enabled/auto_start/notify/elevated/schedule/logs/stop/power
/// 与 name 一律不碰（保留用户偏好）。
fn apply_manifest(table: &mut Table, manifest: &Manifest) {
    set_str(table, "command", &manifest.run.command);
    set_string_array(table, "args", &manifest.run.args);
    set_str(table, "working_dir", &path_text(&manifest.run.working_dir));
    merge_env(table, "env", &manifest.run.env);
    set_defaulted_str(
        table,
        "env_proxy",
        manifest.run.env_proxy.as_str(),
        EnvProxy::BypassLocalhost.as_str(),
    );
    set_ports(table, "ports", &manifest.run.ports);
    set_optional_str(table, "panel_url", manifest.ui.panel_url.as_deref());

    // health.path 永远由应用决定：同步时保留任务里已有的 path（docs/15 §6.3）。
    let existing_path = table
        .get("health")
        .and_then(Item::as_table)
        .and_then(|health| health.get("path"))
        .cloned();
    let health = match (&manifest.health, existing_path) {
        (None, None) => None,
        (Some(spec), None) => Some(health_table(spec)),
        (Some(spec), Some(path)) => {
            let mut table = health_table(spec);
            table.insert("path", path);
            Some(table)
        }
        (None, Some(path)) => {
            let mut table = Table::new();
            table.decor_mut().set_prefix("\n");
            table.insert("path", path);
            Some(table)
        }
    };
    set_sub_table(table, "health", health);

    // 启动窗口限速字段归用户所有：清单不声明它们，同步时必须保留任务里已有的
    // `window_ms` / `max_starts_in_window`。即使清单没有 `[restart]` 表，也要
    // 保留一个只含这两个字段的表，避免同步把用户的安全设置删掉。
    let preserved_window = table
        .get("restart")
        .and_then(Item::as_table)
        .map(|restart| {
            ["window_ms", "max_starts_in_window"]
                .iter()
                .filter_map(|key| restart.get(key).cloned().map(|item| (*key, item)))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut restart = manifest
        .restart
        .as_ref()
        .map(restart_table)
        .filter(|table| !table.is_empty());
    if !preserved_window.is_empty() {
        let target = restart.get_or_insert_with(|| {
            let mut table = Table::new();
            table.decor_mut().set_prefix("\n");
            table
        });
        for (key, item) in preserved_window {
            target.insert(key, item);
        }
    }
    set_sub_table(table, "restart", restart);

    set_str(table, "plugin_version", &manifest.plugin.version);
}

fn health_table(spec: &HealthSpec) -> Table {
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    insert_str(&mut table, "type", spec.kind.as_str());
    if let Some(value) = spec.interval_ms {
        insert_int(&mut table, "interval_ms", value);
    }
    if let Some(value) = spec.timeout_ms {
        insert_int(&mut table, "timeout_ms", value);
    }
    if let Some(value) = spec.failure_threshold {
        insert_int(&mut table, "failure_threshold", u64::from(value));
    }
    if let Some(value) = spec.startup_grace_ms {
        insert_int(&mut table, "startup_grace_ms", value);
    }
    if let Some(url) = &spec.url {
        insert_str(&mut table, "url", url);
    }
    if let Some(codes) = &spec.expect_status {
        insert_int_array(
            &mut table,
            "expect_status",
            codes.iter().map(|code| i64::from(*code)).collect(),
        );
    }
    if let Some(body) = &spec.expect_body_contains {
        insert_str(&mut table, "expect_body_contains", body);
    }
    if let Some(value) = spec.max_age_ms {
        insert_int(&mut table, "max_age_ms", value);
    }
    if !spec.needs_human_status.is_empty() {
        insert_str_array(&mut table, "needs_human_status", &spec.needs_human_status);
    }
    table
}

fn restart_table(spec: &RestartSpec) -> Table {
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    if let Some(mode) = spec.mode {
        insert_str(&mut table, "mode", mode.as_str());
    }
    if let Some(value) = spec.initial_delay_ms {
        insert_int(&mut table, "initial_delay_ms", value);
    }
    if let Some(value) = spec.max_delay_ms {
        insert_int(&mut table, "max_delay_ms", value);
    }
    if let Some(value) = spec.multiplier {
        insert_float(&mut table, "multiplier", value);
    }
    if let Some(value) = spec.jitter {
        insert_float(&mut table, "jitter", value);
    }
    if let Some(value) = spec.max_consecutive_failures {
        insert_int(&mut table, "max_consecutive_failures", u64::from(value));
    }
    if let Some(value) = spec.stable_after_ms {
        insert_int(&mut table, "stable_after_ms", value);
    }
    if !spec.pause_on_exit_codes.is_empty() {
        insert_int_array(
            &mut table,
            "pause_on_exit_codes",
            spec.pause_on_exit_codes
                .iter()
                .map(|code| i64::from(*code))
                .collect(),
        );
    }
    table
}

fn logs_table(spec: &LogsSpec) -> Table {
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    if let Some(rotation) = spec.rotation {
        insert_str(&mut table, "rotation", rotation.as_str());
    }
    if let Some(value) = spec.max_file_mb {
        insert_int(&mut table, "max_file_mb", u64::from(value));
    }
    if let Some(value) = spec.max_files {
        insert_int(&mut table, "max_files", u64::from(value));
    }
    if let Some(value) = spec.max_days {
        insert_int(&mut table, "max_days", u64::from(value));
    }
    if let Some(encoding) = spec.encoding {
        insert_str(&mut table, "encoding", encoding.as_str());
    }
    table
}

fn schedule_table(spec: &ScheduleSpec) -> Table {
    let mut table = Table::new();
    table.decor_mut().set_prefix("\n");
    insert_bool(&mut table, "enabled", spec.enabled);
    // v2（2026-11-15）：start_in_window / stop_outside 不再写入（v1 清单里出现
    // 只做兼容解析，语义固定，不落盘）。
    if !spec.windows.is_empty() {
        let mut windows = ArrayOfTables::new();
        for window in &spec.windows {
            let mut entry = Table::new();
            entry.decor_mut().set_prefix("\n");
            let mut days = Array::new();
            for day in &window.days {
                days.push(Value::from(day.as_str()));
            }
            days.fmt();
            entry.insert("days", Item::Value(Value::Array(days)));
            entry.insert("start", Item::Value(Value::from(format_time(window.start))));
            entry.insert("end", Item::Value(Value::from(format_time(window.end))));
            windows.push(entry);
        }
        table.insert("windows", Item::ArrayOfTables(windows));
    }
    table
}

// ---------------------------------------------------------------- 表写入辅助

/// 写标量前先搬运旧的 decor：直接换 Value 会清空行内注释。
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

fn insert_str(table: &mut Table, key: &str, value: &str) {
    table.insert(key, Item::Value(Value::from(value)));
}

fn insert_bool(table: &mut Table, key: &str, value: bool) {
    table.insert(key, Item::Value(Value::from(value)));
}

fn insert_int(table: &mut Table, key: &str, value: u64) {
    let value = i64::try_from(value).unwrap_or(i64::MAX);
    table.insert(key, Item::Value(Value::from(value)));
}

fn insert_float(table: &mut Table, key: &str, value: f64) {
    table.insert(key, Item::Value(Value::from(value)));
}

fn insert_str_array(table: &mut Table, key: &str, values: &[String]) {
    let mut array = Array::new();
    for value in values {
        array.push(Value::from(value.as_str()));
    }
    array.fmt();
    table.insert(key, Item::Value(Value::Array(array)));
}

fn insert_int_array(table: &mut Table, key: &str, values: Vec<i64>) {
    let mut array = Array::new();
    for value in values {
        array.push(Value::from(value));
    }
    array.fmt();
    table.insert(key, Item::Value(Value::Array(array)));
}

fn set_str(table: &mut Table, key: &str, value: &str) {
    match table.get(key) {
        Some(item) if item.as_str() == Some(value) => {}
        Some(_) => write_scalar(table, key, Value::from(value)),
        None => {
            table.insert(key, Item::Value(Value::from(value)));
        }
    }
}

/// 可选字符串：None / 空串表示"不写这个键"，已有键会被删除（回到默认语义）。
fn set_optional_str(table: &mut Table, key: &str, value: Option<&str>) {
    match value.map(str::trim).filter(|text| !text.is_empty()) {
        Some(text) => set_str(table, key, text),
        None => {
            table.remove(key);
        }
    }
}

/// 与默认值相同则不写（已有键删除），保证导入内容干净。
fn set_defaulted_str(table: &mut Table, key: &str, value: &str, default: &str) {
    if value == default {
        table.remove(key);
    } else {
        set_str(table, key, value);
    }
}

fn set_string_array(table: &mut Table, key: &str, values: &[String]) {
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
    if values.is_empty() {
        table.remove(key);
        return;
    }
    write_array(
        table,
        key,
        values
            .iter()
            .map(|value| Value::from(value.as_str()))
            .collect(),
    );
}

fn set_ports(table: &mut Table, key: &str, ports: &[u16]) {
    let wanted: Vec<i64> = ports.iter().map(|port| i64::from(*port)).collect();
    let same = match table.get(key) {
        Some(item) => match item.as_array() {
            Some(array) => {
                array.len() == wanted.len()
                    && array
                        .iter()
                        .zip(&wanted)
                        .all(|(value, want)| value.as_integer() == Some(*want))
            }
            None => false,
        },
        None => wanted.is_empty(),
    };
    if same {
        return;
    }
    if wanted.is_empty() {
        table.remove(key);
        return;
    }
    write_array(table, key, wanted.into_iter().map(Value::from).collect());
}

/// 写入数组：长度不变时逐元素替换（保留元素 decor），否则按原布局重建。
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

/// 写 env（2026-11-05 静态种子归属修正）：清单只覆盖它声明的键。
///
/// - 首次导入（任务还没有 env）：按清单 env 初始值创建；
/// - 同步：以任务已有 env 为底，manifest 声明的同名键覆盖，其它键原样保留
///   （用户静态种子 YKT_ACCOUNT/PASSWORD、ZHS_UUID 等不能被同步清掉）；
/// - 清单未来不再声明的键转用户所有，不删除；
/// - 原本无 env 且清单 env 为空：保持无 env；
/// - 内联表 `env = { ... }` 与 `[tasks.env]` 子表两种写法都支持就地合并。
fn merge_env(table: &mut Table, key: &str, manifest_env: &BTreeMap<String, String>) {
    // 收集已有 env：内联表与子表都按表处理；非表形态退回旧的整块写入语义
    //（配置校验本应拦截非表 env）。
    let existing = match table.get(key) {
        None => Some(BTreeMap::new()),
        Some(item) => item.as_table_like().map(|env| {
            env.iter()
                .filter_map(|(name, value)| {
                    value
                        .as_str()
                        .map(|text| (name.to_owned(), text.to_owned()))
                })
                .collect::<BTreeMap<String, String>>()
        }),
    };
    let Some(existing) = existing else {
        set_env(table, key, manifest_env);
        return;
    };

    let mut merged = existing.clone();
    for (name, value) in manifest_env {
        merged.insert(name.clone(), value.clone());
    }
    if merged.is_empty() || merged == existing {
        // 没有可写的差异：保持文件逐字节不变（同步 no-op 依赖这一点）。
        return;
    }

    // 已有 env（内联表/子表）：就地覆盖清单声明的键，用户键保持不动。
    if let Some(env) = table.get_mut(key).and_then(Item::as_table_like_mut) {
        for (name, value) in manifest_env {
            env.insert(name, Item::Value(Value::from(value.as_str())));
        }
        return;
    }

    // 原本没有 env：按清单初始值创建行内表（保持与历史导入输出一致）。
    let mut inline = InlineTable::new();
    for (name, value) in manifest_env {
        inline.insert(name.as_str(), Value::from(value.as_str()));
    }
    inline.fmt();
    write_scalar(table, key, Value::InlineTable(inline));
}

/// 写 env：优先就地更新已有的行内表（保留键序），否则整表替换。
///
/// 2026-11-05 起同步路径改用 [`merge_env`]；本函数只作为非表形态兜底。
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

/// 整块替换子表（health/restart）：保留原表头注释与空行，None 表示删除。
fn set_sub_table(table: &mut Table, key: &str, replacement: Option<Table>) {
    match replacement {
        Some(next) => match table.get_mut(key).and_then(Item::as_table_mut) {
            Some(existing) => {
                let mut next = next;
                *next.decor_mut() = existing.decor().clone();
                *existing = next;
            }
            None => {
                table.insert(key, Item::Table(next));
            }
        },
        None => {
            table.remove(key);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Overrides, load};

    fn strs(values: &[&str]) -> Vec<String> {
        values.iter().map(|value| (*value).to_owned()).collect()
    }

    /// 按行前缀替换：清单里的 command / working_dir 可能是绝对路径。
    fn set_line(source: &str, prefix: &str, replacement: &str) -> String {
        let mut out = String::new();
        for line in source.lines() {
            if line.trim_start().starts_with(prefix) {
                out.push_str(replacement);
            } else {
                out.push_str(line);
            }
            out.push('\n');
        }
        out
    }

    /// 临时目录 + 假可执行文件 + 清单目录 + 独立数据目录的测试夹具。
    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        config_path: PathBuf,
        data_dir: PathBuf,
        exe: PathBuf,
        plugin_dir: PathBuf,
        manifest_path: PathBuf,
    }

    impl Fx {
        fn new(config_source: &str) -> Self {
            let tmp = tempfile::tempdir().expect("创建临时目录");
            // 统一成与生产路径解析相同的规范路径，避免 Windows 8.3 短名
            // （RUNNER~1）与 canonicalize 后的长名在同一断言里不相等。
            let root = simplify_path(tmp.path().canonicalize().expect("规范化临时目录"));
            let data_dir = root.join("data");
            fs::create_dir_all(&data_dir).expect("创建数据目录");
            let bin = root.join("bin");
            fs::create_dir_all(&bin).expect("创建 bin");
            let exe = bin.join("fake.exe");
            fs::write(&exe, b"").expect("写假可执行文件");
            let plugin_dir = root.join("plugins").join("demo");
            fs::create_dir_all(&plugin_dir).expect("创建清单目录");
            let config_path = root.join("Rook.toml");
            fs::write(&config_path, config_source).expect("写配置文件");
            let manifest_path = plugin_dir.join(MANIFEST_NAME);
            Self {
                _tmp: tmp,
                root,
                config_path,
                data_dir,
                exe,
                plugin_dir,
                manifest_path,
            }
        }

        fn slash(&self, path: &Path) -> String {
            path.to_string_lossy().replace('\\', "/")
        }

        fn config_source(&self) -> String {
            format!(
                r#"# 顶层注释：用户配置
version = 1

# 既有任务：不能被动到
[[tasks]]
id = "alpha"
name = "甲"
command = '{exe}'
working_dir = '{root}'
enabled = false
"#,
                exe = self.slash(&self.exe),
                root = self.slash(&self.root)
            )
        }

        fn write_manifest(&self, source: &str) {
            fs::write(&self.manifest_path, source).expect("写清单");
        }

        fn manifest_source(&self) -> String {
            // 清单目录是 <root>/plugins/demo，可执行文件在 <root>/bin。
            let exe = self.slash(&self.plugin_dir.join("../../bin/fake.exe"));
            format!(
                r#"# 脚本插件清单：雨课堂网关
schema = 1

[plugin]
id = "ykt-gateway"
name = "雨课堂网关"
version = "1.2.0"
description = "课堂签到网关"
homepage = "http://127.0.0.1:3300"

[run]
command = '{exe}'
args = ["server/index.js", "--port", "3300"]
working_dir = "."
env = {{ PORT = "3300" }}
env_proxy = "bypass_localhost"
ports = [3300]
auto_start = false
elevated = false

[health]
type = "http"
interval_ms = 5000
timeout_ms = 2000
failure_threshold = 3
startup_grace_ms = 20000
url = "http://127.0.0.1:3300/api/healthz"
expect_status = [200, 204]
expect_body_contains = '"ok":true'

[restart]
mode = "on-failure"
max_consecutive_failures = 5

[logs]
rotation = "daily"
max_files = 5

[ui]
panel_url = "http://127.0.0.1:3300/"

[schedule]
enabled = true
start_in_window = true
stop_outside = true

[[schedule.windows]]
days = ["mon", "tue", "wed", "thu", "fri"]
start = "07:50"
end = "08:20"
"#
            )
        }

        fn read(&self) -> String {
            fs::read_to_string(&self.config_path).expect("读配置文件")
        }

        fn revision(&self) -> String {
            editor::revision_of(&fs::read(&self.config_path).expect("读配置文件"))
        }

        fn preview(&self) -> Result<PluginPreview, PluginError> {
            load_preview(&self.manifest_path, &[])
        }

        fn import(
            &self,
            expected: &str,
            data_dir: Option<&Path>,
        ) -> Result<PluginWriteOutcome, PluginError> {
            import_plugin(&self.config_path, &self.manifest_path, expected, data_dir)
        }

        fn sync(
            &self,
            task_id: &str,
            expected: &str,
            data_dir: Option<&Path>,
        ) -> Result<PluginWriteOutcome, PluginError> {
            sync_plugin(&self.config_path, task_id, expected, data_dir)
        }

        fn load(&self) -> crate::config::LoadedConfig {
            load(&Overrides {
                config: Some(&self.config_path),
                data_dir: Some(&self.data_dir),
            })
            .expect("配置应可加载")
        }

        /// 配置目录里残留的临时文件（保存流程必须清理干净）。
        fn temps(&self) -> Vec<String> {
            fs::read_dir(&self.root)
                .expect("列目录")
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .filter(|name| name.ends_with(".tmp"))
                .collect()
        }
    }

    fn new_fx() -> Fx {
        let fx = Fx::new("version = 1\n");
        fx.write_manifest(&fx.manifest_source());
        fx
    }

    fn user_config_source(fx: &Fx) -> String {
        let logs = fx.slash(&fx.root.join("logs"));
        let manifest = fx.slash(&fx.manifest_path);
        let root = fx.slash(&fx.root);
        let heartbeat = fx.slash(&fx.root.join("heartbeats").join("ykt-gateway.hb"));
        format!(
            r#"# 用户改过的配置：同步不能覆盖这些字段
version = 1

[[tasks]]
id = "ykt-gateway"
name = "我自己改的名字"
enabled = false
command = "old-command.exe"
args = ["old.js"]
working_dir = '{root}'
auto_start = true
elevated = true
notify = false
env = {{ PORT = "9999", OLD = "1", YKT_PASSWORD = "seed" }}
ports = [9999]
env_proxy = "strip"
panel_url = "http://127.0.0.1:9999/"
plugin_id = "ykt-gateway"
plugin_manifest = '{manifest}'
plugin_version = "0.9.0"

[tasks.health]
type = "process"
path = '{heartbeat}'

[tasks.restart]
mode = "always"
max_consecutive_failures = 9
window_ms = 120000
max_starts_in_window = 3

[tasks.logs]
dir = '{logs}'
rotation = "daily"
max_files = 9

[tasks.stop]
timeout_ms = 12345

[tasks.power]
resume_grace_ms = 4321

[tasks.schedule]
enabled = false
start_in_window = false
stop_outside = false

[[tasks.schedule.windows]]
days = ["sat"]
start = "10:00"
end = "11:00"
"#,
            root = root,
            manifest = manifest,
            logs = logs,
            heartbeat = heartbeat
        )
    }

    #[test]
    fn manifest_version_reads_plugin_version() {
        let fx = new_fx();
        fx.write_manifest(&fx.manifest_source());
        assert_eq!(
            manifest_version(&fx.manifest_path).expect("应读到清单版本"),
            "1.2.0"
        );
        // 清单缺失时返回错误，不 panic：C8 只在 debug 日志里记录。
        assert!(manifest_version(&fx.plugin_dir.join("missing.toml")).is_err());
    }

    #[test]
    fn version_compare_uses_numeric_segments() {
        assert!(version_is_newer("1.10", Some("1.2")));
        assert!(!version_is_newer("1.2", Some("1.10")));
        assert!(version_is_newer("2.0.1", Some("2.0.0")));
        assert!(version_is_newer("2.0", Some("1.9.9")));
    }

    #[test]
    fn version_compare_pads_missing_segments() {
        assert!(!version_is_newer("2.0", Some("2.0.0")));
        assert!(version_is_newer("2.0.1", Some("2.0")));
        assert!(!version_is_newer("1.0.0.0", Some("1")));
    }

    #[test]
    fn version_compare_equal_is_not_newer() {
        assert!(!version_is_newer("1.2.0", Some("1.2.0")));
        assert!(!version_is_newer(" 1.2 ", Some("1.2")));
    }

    #[test]
    fn version_compare_unknown_current_is_newer() {
        assert!(version_is_newer("1.0.0", None));
        assert!(version_is_newer("1.0.0", Some("")));
        assert!(version_is_newer("1.0.0", Some("   ")));
    }

    #[test]
    fn version_compare_rejects_non_numeric() {
        // 任一段非数字：不判断，避免误报"可更新"。
        assert!(!version_is_newer("1.0.0-beta", Some("1.0.0")));
        assert!(!version_is_newer("v1.2.0", Some("1.0.0")));
        assert!(!version_is_newer("1.0.0", Some("latest")));
        assert!(!version_is_newer("1..0", Some("1.0.0")));
        assert!(!version_is_newer("", Some("1.0.0")));
    }

    #[test]
    fn preview_reads_manifest_and_resolves_paths() {
        let fx = new_fx();
        let preview = fx.preview().expect("预览应成功");

        assert_eq!(preview.plugin_id, "ykt-gateway");
        assert_eq!(preview.task_id, "ykt-gateway");
        assert_eq!(preview.name, "雨课堂网关");
        assert_eq!(preview.version, "1.2.0");
        assert_eq!(preview.description.as_deref(), Some("课堂签到网关"));
        assert_eq!(preview.homepage.as_deref(), Some("http://127.0.0.1:3300"));
        // 路径形态的 command / working_dir 相对清单目录解析为绝对路径。
        assert_eq!(Path::new(&preview.command), fx.exe.as_path());
        assert_eq!(preview.args, strs(&["server/index.js", "--port", "3300"]));
        assert_eq!(PathBuf::from(&preview.working_dir), fx.plugin_dir);
        assert_eq!(preview.ports, vec![3300u16]);
        assert_eq!(preview.env.get("PORT").map(String::as_str), Some("3300"));
        assert_eq!(preview.env_proxy, "bypass_localhost");
        assert_eq!(preview.panel_url.as_deref(), Some("http://127.0.0.1:3300/"));
        assert_eq!(preview.health_type, "http");
        assert_eq!(
            preview.health_url.as_deref(),
            Some("http://127.0.0.1:3300/api/healthz")
        );
        assert!(preview.existing_task_id.is_none());
        assert!(preview.warnings.is_empty(), "{:?}", preview.warnings);

        let schedule = preview.schedule.expect("应带运行时段预览");
        assert!(schedule.enabled);
        assert_eq!(schedule.windows.len(), 1);
        assert_eq!(
            schedule.windows[0].days,
            strs(&["mon", "tue", "wed", "thu", "fri"])
        );
        assert_eq!(schedule.windows[0].start, "07:50");
        assert_eq!(schedule.windows[0].end, "08:20");
    }

    #[test]
    fn preview_accepts_directory_path() {
        let fx = new_fx();
        let preview = load_preview(&fx.plugin_dir, &[]).expect("传目录应自动补清单名");
        assert_eq!(preview.plugin_id, "ykt-gateway");
        assert!(Path::new(&preview.manifest_path).is_file());
    }

    #[test]
    fn preview_marks_existing_task_id() {
        let fx = new_fx();
        let preview =
            load_preview(&fx.manifest_path, &["ykt-gateway".to_owned()]).expect("预览应成功");
        assert_eq!(preview.existing_task_id.as_deref(), Some("ykt-gateway"));
        assert!(
            preview
                .warnings
                .iter()
                .any(|warning| warning.contains("已存在")),
            "{:?}",
            preview.warnings
        );
    }

    #[test]
    fn preview_rejects_missing_manifest() {
        let fx = new_fx();
        let missing = fx.plugin_dir.join("does-not-exist.toml");
        let error = load_preview(&missing, &[]).expect_err("清单不存在应报错");
        assert_eq!(error.stage(), "missing_path");
    }

    #[test]
    fn preview_rejects_missing_working_dir() {
        let fx = new_fx();
        let manifest = set_line(
            &fx.manifest_source(),
            "working_dir = ",
            "working_dir = \"missing-dir\"",
        );
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("工作目录不存在应报错");
        assert_eq!(error.stage(), "missing_path");
        assert!(error.to_string().contains("missing-dir"), "{error}");
    }

    #[test]
    fn preview_rejects_missing_command_path() {
        let fx = new_fx();
        let manifest = set_line(
            &fx.manifest_source(),
            "command = ",
            "command = \"../../bin/missing.exe\"",
        );
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("command 路径不存在应报错");
        assert_eq!(error.stage(), "missing_path");
        assert!(error.to_string().contains("missing.exe"), "{error}");
    }

    #[test]
    fn preview_rejects_unc_working_dir() {
        let fx = new_fx();
        let manifest = set_line(
            &fx.manifest_source(),
            "working_dir = ",
            "working_dir = \"//server/share\"",
        );
        fx.write_manifest(&manifest);
        assert_eq!(fx.preview().expect_err("UNC 路径应报错").stage(), "invalid");
    }

    #[test]
    fn preview_rejects_unknown_env_var_in_path() {
        let fx = new_fx();
        let manifest = set_line(
            &fx.manifest_source(),
            "working_dir = ",
            "working_dir = \"%CAMPUS_SENTRY_MISSING_VAR_XYZ%\"",
        );
        fx.write_manifest(&manifest);
        assert_eq!(
            fx.preview().expect_err("未知环境变量应报错").stage(),
            "invalid"
        );
    }

    #[test]
    fn preview_rejects_bad_schema() {
        let fx = new_fx();
        let manifest = fx.manifest_source().replace("schema = 1", "schema = 2");
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("schema 不支持应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("schema"), "{error}");
    }

    #[test]
    fn preview_rejects_unknown_field() {
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("[plugin]", "[plugin]\nunexpected = 1");
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("未知字段应报错");
        assert_eq!(error.stage(), "parse");
    }

    #[test]
    fn preview_rejects_health_path_field() {
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("[health]", "[health]\npath = 'custom.hb'");
        fx.write_manifest(&manifest);

        let error = fx
            .preview()
            .expect_err("清单不允许 health.path（由应用管理）");
        assert_eq!(error.stage(), "parse");
    }

    #[test]
    fn preview_rejects_logs_dir_field() {
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("[logs]", "[logs]\ndir = 'custom-logs'");
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("清单不允许 logs.dir（由应用管理）");
        assert_eq!(error.stage(), "parse");
    }

    #[test]
    fn preview_rejects_invalid_plugin_id() {
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("id = \"ykt-gateway\"", "id = \"Ykt_Gateway\"");
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("非法 plugin.id 应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("plugin.id"), "{error}");
    }

    #[test]
    fn preview_accepts_underscore_plugin_id() {
        // docs/15 §3.3 的 id 规则与任务 id 一致，允许下划线。
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("id = \"ykt-gateway\"", "id = \"campus_net\"");
        fx.write_manifest(&manifest);

        let preview = fx.preview().expect("下划线 id 应被接受");
        assert_eq!(preview.plugin_id, "campus_net");
        assert_eq!(preview.task_id, "campus_net");
    }

    #[test]
    fn preview_rejects_empty_command() {
        let fx = new_fx();
        let manifest = set_line(&fx.manifest_source(), "command = ", "command = \"\"");
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("空 command 应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("run.command"), "{error}");
    }

    #[test]
    fn preview_rejects_too_long_version() {
        let fx = new_fx();
        let manifest = set_line(
            &fx.manifest_source(),
            "version = ",
            &format!("version = \"{}\"", "1".repeat(40)),
        );
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("version 过长应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("plugin.version"), "{error}");
    }

    #[test]
    fn preview_rejects_env_reserved_prefix() {
        let fx = new_fx();
        let manifest = fx.manifest_source().replace(
            "env = { PORT = \"3300\" }",
            "env = { CAMPUS_SENTRY_TASK_ID = \"x\" }",
        );
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("保留前缀应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("CAMPUS_SENTRY_"), "{error}");
    }

    #[test]
    fn preview_warns_when_elevated() {
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("elevated = false", "elevated = true");
        fx.write_manifest(&manifest);

        let preview = fx.preview().expect("elevated 只是提醒");
        assert!(
            preview
                .warnings
                .iter()
                .any(|warning| warning.contains("提权")),
            "{:?}",
            preview.warnings
        );
    }

    #[test]
    fn preview_rejects_bad_schedule() {
        let fx = new_fx();

        // 非法星期
        let mut manifest = fx.manifest_source().replace(
            "days = [\"mon\", \"tue\", \"wed\", \"thu\", \"fri\"]",
            "days = [\"funday\"]",
        );
        fx.write_manifest(&manifest);
        assert_eq!(fx.preview().expect_err("非法星期").stage(), "invalid");

        // start == end（配置校验与 docs 都拒绝）
        manifest = fx
            .manifest_source()
            .replace("start = \"07:50\"", "start = \"08:20\"");
        fx.write_manifest(&manifest);
        assert_eq!(fx.preview().expect_err("start == end").stage(), "invalid");

        // 非法时间
        manifest = fx
            .manifest_source()
            .replace("start = \"07:50\"", "start = \"25:00\"");
        fx.write_manifest(&manifest);
        assert_eq!(fx.preview().expect_err("非法时间").stage(), "invalid");
    }

    #[test]
    fn preview_rejects_empty_schedule_windows_when_enabled() {
        let fx = new_fx();
        let schedule_block = "[schedule]\nenabled = true\nstart_in_window = true\nstop_outside = true\n\n[[schedule.windows]]\ndays = [\"mon\", \"tue\", \"wed\", \"thu\", \"fri\"]\nstart = \"07:50\"\nend = \"08:20\"\n";
        let manifest = fx
            .manifest_source()
            .replace(schedule_block, "[schedule]\nenabled = true\n");
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("启用但无窗口应拒绝");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("没有可用窗口"), "{error}");
    }

    #[test]
    fn preview_rejects_health_without_url_or_max_age() {
        let fx = new_fx();

        let mut manifest = fx
            .manifest_source()
            .replace("url = \"http://127.0.0.1:3300/api/healthz\"\n", "");
        fx.write_manifest(&manifest);
        assert_eq!(fx.preview().expect_err("http 缺 url").stage(), "invalid");

        manifest = fx
            .manifest_source()
            .replace("type = \"http\"", "type = \"heartbeat\"");
        fx.write_manifest(&manifest);
        assert_eq!(
            fx.preview().expect_err("heartbeat 缺 max_age").stage(),
            "invalid"
        );
    }

    #[test]
    fn preview_reads_heartbeat_manifest() {
        let fx = new_fx();
        let manifest = fx.manifest_source().replace(
            r#"[health]
type = "http"
interval_ms = 5000
timeout_ms = 2000
failure_threshold = 3
startup_grace_ms = 20000
url = "http://127.0.0.1:3300/api/healthz"
expect_status = [200, 204]
expect_body_contains = '"ok":true'"#,
            r#"[health]
type = "heartbeat"
interval_ms = 5000
max_age_ms = 30000
startup_grace_ms = 20000
needs_human_status = ["login_required", "blocked"]"#,
        );
        let manifest = manifest.replace(
            "[ui]\npanel_url = \"http://127.0.0.1:3300/\"",
            "[ui]\npanel_url = \"http://127.0.0.1:3300/\"\nstatus_url = \"http://127.0.0.1:3300/api/sentry/status\"",
        );
        fx.write_manifest(&manifest);

        let preview = fx.preview().expect("heartbeat 清单应可预览");
        assert_eq!(preview.health_type, "heartbeat");
        assert!(preview.health_url.is_none());
        assert!(preview.warnings.is_empty(), "{:?}", preview.warnings);
    }

    #[test]
    fn import_writes_task_and_keeps_comments_and_other_tasks() {
        let fx = new_fx();
        fs::write(&fx.config_path, fx.config_source()).expect("写配置");
        let revision = fx.revision();

        let outcome = fx
            .import(&revision, Some(&fx.data_dir))
            .expect("导入应成功");
        assert!(outcome.changed);
        assert_eq!(outcome.task_id, "ykt-gateway");
        assert_eq!(outcome.revision, fx.revision());

        let after = fx.read();
        // 注释与既有任务原样保留。
        assert!(after.contains("# 顶层注释：用户配置"), "{after}");
        assert!(after.contains("# 既有任务：不能被动到"), "{after}");
        assert!(after.contains("id = \"alpha\""), "{after}");
        assert!(
            !after.contains("# 脚本插件清单：雨课堂网关"),
            "清单注释不写回配置"
        );

        // 期望字段。
        assert!(after.contains("id = \"ykt-gateway\""), "{after}");
        assert!(after.contains("name = \"雨课堂网关\""), "{after}");
        assert!(after.contains("enabled = true"), "{after}");
        assert!(after.contains("plugin_id = \"ykt-gateway\""), "{after}");
        assert!(after.contains("plugin_version = \"1.2.0\""), "{after}");
        assert!(after.contains("plugin_manifest = "), "{after}");
        assert!(after.contains("[tasks.health]"), "{after}");
        assert!(after.contains("type = \"http\""), "{after}");
        assert!(after.contains("[tasks.logs]"), "{after}");
        assert!(after.contains("[[tasks.schedule.windows]]"), "{after}");
        assert!(after.contains("start = \"07:50\""), "{after}");
        // v2：清单里的 start_in_window/stop_outside 只做兼容解析，不再写入任务配置。
        assert!(!after.contains("start_in_window"), "{after}");
        assert!(!after.contains("stop_outside"), "{after}");

        // 完整加载：结构合法、字段归一化正确。
        let loaded = fx.load();
        assert!(
            loaded.errors().next().is_none(),
            "不应有任务级错误：{:?}",
            loaded.issues
        );
        let alpha = loaded.config.task("alpha").expect("既有任务仍在");
        assert!(!alpha.enabled);
        let task = loaded.config.task("ykt-gateway").expect("新任务");
        let plugin = task.plugin.as_ref().expect("插件来源字段");
        assert_eq!(plugin.id, "ykt-gateway");
        assert_eq!(plugin.version.as_deref(), Some("1.2.0"));
        assert!(plugin.manifest.is_file());
        assert!(task.enabled);
        assert!(!task.auto_start);
        assert!(!task.elevated);
        assert_eq!(Path::new(&task.command), fx.exe.as_path());
        assert_eq!(task.args, strs(&["server/index.js", "--port", "3300"]));
        assert_eq!(task.working_dir, fx.plugin_dir);
        assert_eq!(task.env.get("PORT").map(String::as_str), Some("3300"));
        assert_eq!(task.ports, vec![3300u16]);
        assert_eq!(task.env_proxy, EnvProxy::BypassLocalhost);
        assert_eq!(task.panel_url.as_deref(), Some("http://127.0.0.1:3300/"));
        assert_eq!(task.health.kind, HealthKind::Http);
        assert_eq!(
            task.health.url.as_deref(),
            Some("http://127.0.0.1:3300/api/healthz")
        );
        assert_eq!(task.health.expect_status, vec![200u16, 204]);
        assert_eq!(
            task.health.expect_body_contains.as_deref(),
            Some("\"ok\":true")
        );
        assert_eq!(task.restart.mode, RestartMode::OnFailure);
        assert_eq!(task.restart.max_consecutive_failures, 5);
        assert_eq!(task.logs.rotation, Rotation::Daily);
        assert_eq!(task.logs.max_files, 5);
        assert!(task.schedule.enabled);
        assert_eq!(task.schedule.windows.len(), 1);
        assert_eq!(
            task.schedule.windows[0].days,
            vec![
                Weekday::Mon,
                Weekday::Tue,
                Weekday::Wed,
                Weekday::Thu,
                Weekday::Fri
            ]
        );
        assert_eq!(
            task.schedule.windows[0].start,
            parse_time("07:50").expect("时间")
        );
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn import_creates_tasks_table_when_missing() {
        let fx = new_fx();
        let revision = fx.revision();
        let outcome = fx
            .import(&revision, Some(&fx.data_dir))
            .expect("导入应成功");
        assert!(outcome.changed);

        let after = fx.read();
        assert!(after.contains("[[tasks]]"), "{after}");
        assert!(after.contains("id = \"ykt-gateway\""), "{after}");
        let loaded = fx.load();
        assert!(loaded.config.task("ykt-gateway").is_some());
    }

    #[test]
    fn import_conflict_does_not_write() {
        let fx = new_fx();
        fs::write(&fx.config_path, fx.config_source()).expect("写配置");
        let opened = fx.revision();

        // 外部修改。
        let external = "version = 1\n# external edit\n";
        fs::write(&fx.config_path, external).expect("外部写入");

        let error = fx
            .import(&opened, Some(&fx.data_dir))
            .expect_err("应报告冲突");
        match error {
            PluginError::Conflict { current_revision } => {
                assert_eq!(current_revision, fx.revision());
            }
            other => panic!("期望 Conflict，实际 {other:?}"),
        }
        assert_eq!(fx.read(), external, "外部修改必须完好保留");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn import_existing_task_id_is_invalid() {
        let fx = new_fx();
        fs::write(&fx.config_path, fx.config_source()).expect("写配置");
        let revision = fx.revision();
        fx.import(&revision, Some(&fx.data_dir)).expect("首次导入");
        let before = fx.read();
        let revision = fx.revision();

        let error = fx
            .import(&revision, Some(&fx.data_dir))
            .expect_err("重复导入应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("已存在"), "{error}");
        assert_eq!(fx.read(), before, "失败不能改文件");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn import_rejects_oversized_manifest() {
        let fx = new_fx();
        let mut manifest = fx.manifest_source();
        manifest.push_str(&"#".repeat(70 * 1024));
        fx.write_manifest(&manifest);

        let error = fx.preview().expect_err("超过 64KiB 应报错");
        assert_eq!(error.stage(), "read");
    }

    #[test]
    fn sync_updates_owned_fields_and_preserves_user_fields() {
        let fx = new_fx();
        fs::write(&fx.config_path, user_config_source(&fx)).expect("写用户配置");
        let revision = fx.revision();

        let outcome = fx
            .sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect("同步应成功");
        assert!(outcome.changed);
        assert_eq!(outcome.task_id, "ykt-gateway");
        assert_eq!(outcome.revision, fx.revision());

        let after = fx.read();
        assert!(
            after.contains("# 用户改过的配置：同步不能覆盖这些字段"),
            "{after}"
        );
        // health.path 由应用/用户掌握，同步必须保留。
        assert!(after.contains("path = "), "{after}");

        let loaded = fx.load();
        assert!(
            loaded.errors().next().is_none(),
            "不应有任务级错误：{:?}",
            loaded.issues
        );
        let task = loaded.config.task("ykt-gateway").expect("任务");

        // 清单拥有字段已更新。
        assert_eq!(Path::new(&task.command), fx.exe.as_path());
        assert_eq!(task.args, strs(&["server/index.js", "--port", "3300"]));
        assert_eq!(task.working_dir, fx.plugin_dir);
        // 清单声明的 PORT 被更新；用户额外键（含静态种子）必须保留。
        assert_eq!(task.env.get("PORT").map(String::as_str), Some("3300"));
        assert_eq!(
            task.env.get("OLD").map(String::as_str),
            Some("1"),
            "清单未声明的用户键不能被同步删除"
        );
        assert_eq!(
            task.env.get("YKT_PASSWORD").map(String::as_str),
            Some("seed"),
            "用户静态种子必须保留给用户"
        );
        assert_eq!(task.ports, vec![3300u16]);
        assert_eq!(task.env_proxy, EnvProxy::BypassLocalhost);
        assert_eq!(task.panel_url.as_deref(), Some("http://127.0.0.1:3300/"));
        assert_eq!(
            task.plugin
                .as_ref()
                .and_then(|plugin| plugin.version.as_deref()),
            Some("1.2.0")
        );
        assert_eq!(task.health.kind, HealthKind::Http);
        assert_eq!(
            task.health.url.as_deref(),
            Some("http://127.0.0.1:3300/api/healthz")
        );
        assert_eq!(task.restart.mode, RestartMode::OnFailure);
        assert_eq!(task.restart.max_consecutive_failures, 5);
        // 清单不拥有窗口限速字段：同步必须保留用户设置。
        assert_eq!(task.restart.window_ms, 120_000);
        assert_eq!(task.restart.max_starts_in_window, 3);

        // 用户偏好字段原样保留。
        assert_eq!(task.name, "我自己改的名字");
        assert!(!task.enabled);
        assert!(task.auto_start);
        assert!(task.elevated);
        assert!(!task.notify);
        assert_eq!(task.stop.timeout_ms, 12345);
        assert_eq!(task.logs.max_files, 9);
        assert_eq!(task.logs.dir, fx.root.join("logs"));
        assert_eq!(task.power.resume_grace_ms, 4321);
        assert!(!task.schedule.enabled);
        // v2：废弃字段不再进入模型；同步不碰用户 schedule 表，旧键原样留在文本里
        // （加载时只发 warning）。
        assert!(fx.read().contains("start_in_window = false"));
        assert_eq!(task.schedule.windows.len(), 1);
        assert_eq!(task.schedule.windows[0].days, vec![Weekday::Sat]);
        assert_eq!(
            task.schedule.windows[0].start,
            parse_time("10:00").expect("时间")
        );
        assert!(fx.temps().is_empty());
    }

    /// 2026-11-05：清单没有 env 时同步不能删除用户 env（静态种子归用户）。
    #[test]
    fn sync_without_manifest_env_keeps_user_env() {
        let fx = new_fx();
        let manifest = fx
            .manifest_source()
            .replace("env = { PORT = \"3300\" }\n", "");
        fx.write_manifest(&manifest);
        fs::write(&fx.config_path, user_config_source(&fx)).expect("写用户配置");
        let revision = fx.revision();

        fx.sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect("同步应成功");

        let loaded = fx.load();
        assert!(
            loaded.errors().next().is_none(),
            "不应有任务级错误：{:?}",
            loaded.issues
        );
        let task = loaded.config.task("ykt-gateway").expect("任务");
        // 用户 env 原样保留（清单未声明任何键）。
        assert_eq!(task.env.get("OLD").map(String::as_str), Some("1"));
        assert_eq!(
            task.env.get("YKT_PASSWORD").map(String::as_str),
            Some("seed")
        );
        // 清单拥有的其它字段仍然更新。
        assert_eq!(task.ports, vec![3300u16]);
        assert_eq!(Path::new(&task.command), fx.exe.as_path());
        assert!(fx.temps().is_empty());
    }

    /// 清单没有 `[restart]` 表时，同步也不能删除用户设置的窗口限速。
    #[test]
    fn sync_without_manifest_restart_keeps_window_fields() {
        let fx = new_fx();
        let manifest = fx.manifest_source().replace(
            "[restart]\nmode = \"on-failure\"\nmax_consecutive_failures = 5\n\n",
            "",
        );
        fx.write_manifest(&manifest);
        fs::write(&fx.config_path, user_config_source(&fx)).expect("写用户配置");
        let revision = fx.revision();

        fx.sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect("同步应成功");

        let loaded = fx.load();
        assert!(
            loaded.errors().next().is_none(),
            "不应有任务级错误：{:?}",
            loaded.issues
        );
        let task = loaded.config.task("ykt-gateway").expect("任务");
        assert_eq!(task.restart.window_ms, 120_000);
        assert_eq!(task.restart.max_starts_in_window, 3);
        assert!(fx.temps().is_empty());
    }

    /// `[tasks.env]` 子表形式：同步只覆盖清单声明的键，用户键保留。
    #[test]
    fn sync_merges_manifest_env_into_user_env_subtable() {
        let fx = new_fx();
        // 用户把 env 写成子表（等价语法），并放了静态种子。
        let source = user_config_source(&fx).replace(
            "env = { PORT = \"9999\", OLD = \"1\", YKT_PASSWORD = \"seed\" }",
            "",
        ) + "\n[tasks.env]\nPORT = \"9999\"\nOLD = \"1\"\nYKT_PASSWORD = \"seed\"\n";
        fs::write(&fx.config_path, source).expect("写用户配置");
        let revision = fx.revision();

        fx.sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect("同步应成功");

        let loaded = fx.load();
        assert!(
            loaded.errors().next().is_none(),
            "不应有任务级错误：{:?}",
            loaded.issues
        );
        let task = loaded.config.task("ykt-gateway").expect("任务");
        assert_eq!(task.env.get("PORT").map(String::as_str), Some("3300"));
        assert_eq!(task.env.get("OLD").map(String::as_str), Some("1"));
        assert_eq!(
            task.env.get("YKT_PASSWORD").map(String::as_str),
            Some("seed")
        );
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn sync_is_noop_when_manifest_unchanged() {
        let fx = new_fx();
        fs::write(&fx.config_path, fx.config_source()).expect("写配置");
        let revision = fx.revision();
        fx.import(&revision, Some(&fx.data_dir)).expect("导入");
        let before = fx.read();
        let revision = fx.revision();

        let outcome = fx
            .sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect("同步应成功");
        assert!(!outcome.changed, "清单未变时同步必须 no-op");
        assert_eq!(outcome.revision, revision);
        assert_eq!(fx.read(), before, "文件必须逐字节不变");
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn sync_keeps_order_field() {
        // C7：`order` 不属于清单拥有字段，同步必须原样保留。
        let fx = new_fx();
        let source = format!(
            r#"version = 1

[[tasks]]
id = "ykt-gateway"
name = "网关"
command = "old.exe"
working_dir = "{root}"
order = 7
plugin_id = "ykt-gateway"
plugin_manifest = "{manifest}"
plugin_version = "0.9.0"
"#,
            root = fx.slash(&fx.root),
            manifest = fx.slash(&fx.manifest_path)
        );
        fs::write(&fx.config_path, &source).expect("写配置");
        let revision = fx.revision();

        fx.sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect("同步应成功");

        let after = fx.read();
        assert!(after.contains("order = 7"), "order 必须保留：{after}");
        assert_eq!(fx.load().config.task("ykt-gateway").expect("任务").order, 7);
    }

    #[test]
    fn sync_unknown_task_reports_task_not_found() {
        let fx = new_fx();
        fs::write(&fx.config_path, fx.config_source()).expect("写配置");
        let revision = fx.revision();

        let error = fx
            .sync("nope", &revision, Some(&fx.data_dir))
            .expect_err("任务不存在应报错");
        assert_eq!(error.stage(), "task_not_found");
        assert!(error.to_string().contains("nope"), "{error}");
        assert_eq!(fx.revision(), revision, "失败不能改文件");
    }

    #[test]
    fn sync_task_without_manifest_is_invalid() {
        let fx = new_fx();
        fs::write(&fx.config_path, fx.config_source()).expect("写配置");
        let revision = fx.revision();

        let error = fx
            .sync("alpha", &revision, Some(&fx.data_dir))
            .expect_err("手工任务不能同步");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("plugin_manifest"), "{error}");
        assert_eq!(fx.revision(), revision);
    }

    #[test]
    fn sync_conflict_does_not_write() {
        let fx = new_fx();
        fs::write(&fx.config_path, user_config_source(&fx)).expect("写用户配置");
        let opened = fx.revision();
        let external = "version = 1\n# external\n";
        fs::write(&fx.config_path, external).expect("外部写入");

        let error = fx
            .sync("ykt-gateway", &opened, Some(&fx.data_dir))
            .expect_err("应报告冲突");
        assert_eq!(error.stage(), "conflict");
        assert_eq!(fx.read(), external);
        assert!(fx.temps().is_empty());
    }

    #[test]
    fn sync_manifest_id_mismatch_is_invalid() {
        let fx = new_fx();
        fs::write(&fx.config_path, user_config_source(&fx)).expect("写用户配置");
        // 清单改成另一个 plugin.id。
        let manifest = fx
            .manifest_source()
            .replace("id = \"ykt-gateway\"", "id = \"zhs-gateway\"");
        fx.write_manifest(&manifest);
        let revision = fx.revision();

        let error = fx
            .sync("ykt-gateway", &revision, Some(&fx.data_dir))
            .expect_err("清单 id 不一致应报错");
        assert_eq!(error.stage(), "invalid");
        assert!(error.to_string().contains("不一致"), "{error}");
        assert_eq!(fx.revision(), revision);
    }
}
