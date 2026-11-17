//! state.json 的数据模型（docs/03 §10）。
//!
//! 这里只有可序列化的值类型，不做 I/O、不读时钟；加载/保存见 [`super::store`]。
//!
//! 磁盘格式约定：
//!
//! - 字段名与 docs/03 §10 的示例一致（snake_case），与前端快照的 camelCase 无关；
//! - 所有字段都允许缺失（缺失 = 默认值），未知字段一律忽略；旧版本读取新版本
//!   多写的字段不应失败；
//! - `desired_override` 是安全关键字段，只接受 `"stopped"` / `"paused"`；
//!   其他取值说明文件不可信，加载层按“损坏”处理并向调用方报告；
//! - `paused_reason` / `last_status` 是恢复提示与诊断信息，未知取值保留为
//!   [`PausedReason::Other`] / [`LastStatus::Other`]，避免一个未来的枚举值
//!   让整份状态不可用；
//! - 时间统一为 UTC RFC-3339（如 `2026-11-02T04:40:00Z`）；加载时接受任意
//!   时区偏移并归一化为 UTC，保存时固定输出 `Z` 后缀；
//! - 只保存“重启后恢复监督语义”所需的持久状态与窗口偏好；进程/PID/句柄/
//!   日志统计等易变或可重建的信息不落盘（docs/03 §10）。
//!
//! 本模块不依赖 Tauri，也不访问真实用户目录。

use std::collections::BTreeMap;
use std::fmt;

use chrono::{DateTime, SecondsFormat, Utc};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

/// 当前支持的 state.json schema 版本（与 docs/03 §10 对齐）。
pub const SCHEMA_VERSION: i64 = 1;

/// UTC 时间戳。
///
/// 序列化为 RFC-3339 字符串；保存时使用 `Z` 后缀并按需附带毫秒精度，
/// 例如 `2026-11-02T04:40:00Z`、`2026-11-02T04:40:00.123Z`。
/// 校准/测试可以注入任意时间：[`Timestamp::now`] 只供调用方使用，
/// 模型与存储层内部不会读取时钟。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(DateTime<Utc>);

impl Timestamp {
    /// 包装一个 UTC 时间。
    pub fn new(datetime: DateTime<Utc>) -> Self {
        Self(datetime)
    }

    /// 当前 UTC 时间（仅供调用方/测试构造数据，存储层不调用）。
    pub fn now() -> Self {
        Self(Utc::now())
    }

    /// 解析 RFC-3339 字符串；带偏移的时间会归一化为 UTC。
    pub fn parse(text: &str) -> Result<Self, chrono::ParseError> {
        Ok(Self(
            DateTime::parse_from_rfc3339(text)?.with_timezone(&Utc),
        ))
    }

    /// 内部 UTC 时间。
    pub fn datetime(self) -> DateTime<Utc> {
        self.0
    }

    /// 磁盘上的字符串形式（RFC-3339、UTC、`Z` 后缀）。
    pub fn as_rfc3339(self) -> String {
        self.0.to_rfc3339_opts(SecondsFormat::AutoSi, true)
    }
}

impl fmt::Display for Timestamp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.as_rfc3339())
    }
}

impl From<DateTime<Utc>> for Timestamp {
    fn from(datetime: DateTime<Utc>) -> Self {
        Self::new(datetime)
    }
}

impl Serialize for Timestamp {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.as_rfc3339())
    }
}

impl<'de> Deserialize<'de> for Timestamp {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).map_err(|error| {
            serde::de::Error::custom(format!("时间不是合法的 RFC-3339 字符串：{text}（{error}）"))
        })
    }
}

/// 用户显式意图：`desired_override`（docs/03 §1）。
///
/// `null` = 没有覆盖（由 `auto_start` / 监督器决定）；`stopped` / `paused`
/// 是用户/熔断留下的持久意图。只接受这两种取值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DesiredOverride {
    /// 用户显式停止：任何自动路径都不得再启动。
    Stopped,
    /// 用户暂停或熔断：等待人工处理。
    Paused,
}

impl DesiredOverride {
    /// 稳定英文标识（state.json / 日志共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Paused => "paused",
        }
    }

    /// 解析字符串取值；未知取值返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "stopped" => Some(Self::Stopped),
            "paused" => Some(Self::Paused),
            _ => None,
        }
    }
}

impl fmt::Display for DesiredOverride {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for DesiredOverride {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for DesiredOverride {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "未知 desired_override 取值：{text}（只接受 stopped/paused）"
            ))
        })
    }
}

/// 进入 `paused` 的原因（诊断 + 恢复提示）。
///
/// 已知取值来自 docs/03；未知取值保留为 [`PausedReason::Other`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PausedReason {
    /// 连续失败达到 `max_consecutive_failures`。
    CrashLoop,
    /// 启动窗口内启动次数超过 `max_starts_in_window`。
    RestartBudget,
    /// 需要人工处理（心跳 status / 退出码 / 已持久化）。
    NeedsHuman,
    /// `restart.mode = never` 时进程退出。
    CrashNoRestart,
    /// 用户显式暂停。
    UserPause,
    /// 提权任务注册被取消。
    ElevationUnavailable,
    /// 未知/未来版本的取值，原样保留。
    Other(String),
}

impl PausedReason {
    /// 稳定英文标识。
    pub fn as_str(&self) -> &str {
        match self {
            Self::CrashLoop => "crash_loop",
            Self::RestartBudget => "restart_budget",
            Self::NeedsHuman => "needs_human",
            Self::CrashNoRestart => "crash_no_restart",
            Self::UserPause => "user_pause",
            Self::ElevationUnavailable => "elevation_unavailable",
            Self::Other(text) => text,
        }
    }

    /// 解析已知取值；未知取值保留为 [`PausedReason::Other`]。
    pub fn parse(text: &str) -> Self {
        match text {
            "crash_loop" => Self::CrashLoop,
            "restart_budget" => Self::RestartBudget,
            "needs_human" => Self::NeedsHuman,
            "crash_no_restart" => Self::CrashNoRestart,
            "user_pause" => Self::UserPause,
            "elevation_unavailable" => Self::ElevationUnavailable,
            _ => Self::Other(text.to_owned()),
        }
    }
}

impl fmt::Display for PausedReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for PausedReason {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for PausedReason {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// 监督器最后一次观察到的状态（docs/03 §2）。
///
/// 未知取值保留为 [`LastStatus::Other`]；恢复策略由 Engine 决定，
/// 本模块只负责如实保存。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LastStatus {
    Stopped,
    Starting,
    Running,
    Stopping,
    Backoff,
    Paused,
    /// 未知/未来版本的取值，原样保留。
    Other(String),
}

impl LastStatus {
    /// 稳定英文标识。
    pub fn as_str(&self) -> &str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Backoff => "backoff",
            Self::Paused => "paused",
            Self::Other(text) => text,
        }
    }

    /// 解析已知取值；未知取值保留为 [`LastStatus::Other`]。
    pub fn parse(text: &str) -> Self {
        match text {
            "stopped" => Self::Stopped,
            "starting" => Self::Starting,
            "running" => Self::Running,
            "stopping" => Self::Stopping,
            "backoff" => Self::Backoff,
            "paused" => Self::Paused,
            _ => Self::Other(text.to_owned()),
        }
    }
}

impl fmt::Display for LastStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Serialize for LastStatus {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LastStatus {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        Ok(Self::parse(&String::deserialize(deserializer)?))
    }
}

/// `needs_human` 注意标志（docs/03 §7.1）。
///
/// 它是正交标志，不是第三种意图；`active = false` 时其余字段应为空。
/// 本模块不做一致性校验，只原样保存。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NeedsHumanState {
    /// 是否已置位。
    pub active: bool,
    /// 来源：`"heartbeat"` / `"exit_code"` / `"restored"` 等（开放字符串）。
    pub source: Option<String>,
    /// 面向用户/日志的说明。
    pub message: Option<String>,
    /// 置位时刻。
    pub since: Option<Timestamp>,
}

/// 单个任务的持久状态（docs/03 §10）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskState {
    /// 用户显式意图；`None` = 没有覆盖。
    pub desired_override: Option<DesiredOverride>,
    /// 连续失败次数（重启后必须保留，避免绕过熔断）。
    pub consecutive_failures: u32,
    /// 累计启动次数（统计用）。
    pub total_starts: u64,
    /// 累计自动重启次数（统计用）。
    pub total_restarts: u64,
    /// 最近一次实例的根进程退出码。
    pub last_exit_code: Option<u32>,
    /// 最近一次退出时刻。
    pub last_exit_at: Option<Timestamp>,
    /// 最近一次启动时刻。
    pub last_start_at: Option<Timestamp>,
    /// 最近一次迁移原因（稳定英文标识，如 `clean_exit`）。
    pub last_reason: Option<String>,
    /// `paused` 的原因；非 paused 时为 `None`。
    pub paused_reason: Option<PausedReason>,
    /// 进入 `backoff` 时算好的下次重试时刻（重启后按剩余延迟恢复）。
    pub next_retry_at: Option<Timestamp>,
    /// 监督器最后观察到的状态。
    pub last_status: Option<LastStatus>,
    /// 需要人工处理标志。
    pub needs_human: NeedsHumanState,
    /// 最近一次观测到的运行时段 key（如 `2026-11-04T07:50`）；用于区分
    /// “新窗口”与“同一窗口内重启应用”，不参与监督决策。
    pub schedule_window: Option<String>,
    /// 用户在本时段内手动停止时记下当前 key；同一窗口内不再自动拉起。
    pub schedule_suppressed: Option<String>,
}

/// 主窗口偏好（非敏感）。
///
/// 默认值来自 docs/05 §4（默认 1040×700，最小 880×560）；这里不做范围钳制，
/// 由窗口层在应用时钳制到最小/可见区域。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WindowState {
    /// 窗口宽度（逻辑像素）。
    pub width: u32,
    /// 窗口高度（逻辑像素）。
    pub height: u32,
    /// 是否最大化。
    pub maximized: bool,
}

impl Default for WindowState {
    fn default() -> Self {
        Self {
            width: 1040,
            height: 700,
            maximized: false,
        }
    }
}

/// 界面偏好（非敏感）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiState {
    /// 上次选中的任务 id。
    pub last_selected: Option<String>,
    /// 首次关窗提示是否已经展示过（一次性标记）。
    pub tray_hint_shown: bool,
    /// 窗口刚关闭、下次打开时应展示一次“仍在托盘运行”提示。
    pub tray_hint_pending: bool,
}

/// 应用级会话标记（B1/B3）。
///
/// 向后兼容：旧 state.json 没有 `app_session` 时按 [`Default`] 读取；
/// 首次运行（state 文件缺失）不会被当作异常退出。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSession {
    /// 本次会话启动时刻。
    pub started_at: Option<Timestamp>,
    /// 上次退出是否干净；默认 `false`，但只有 `started_at` 也存在时才据此报告异常退出。
    pub clean_exit: bool,
    /// 上次退出原因（`user` / `session_end`）。
    pub last_exit_reason: Option<String>,
    /// 上次退出时刻。
    pub last_exit_at: Option<Timestamp>,
    /// 上次运行的应用版本（用于异常退出诊断）。
    pub last_version: Option<String>,
}

/// state.json 的根对象。
///
/// 删除该文件 = 重置计数器/覆盖状态/窗口偏好，不影响配置与日志（docs/03 §10）。
/// 任务 id 是否仍存在于配置中由 Engine 判断；本模块不依赖配置、不丢弃未知 id。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PersistedState {
    /// schema 版本；保存时必须是 [`SCHEMA_VERSION`]。
    pub schema: i64,
    /// 任务 id → 持久状态（有序 map，保证序列化稳定、便于 diff）。
    pub tasks: BTreeMap<String, TaskState>,
    /// 主窗口偏好。
    pub window: WindowState,
    /// 界面偏好。
    pub ui: UiState,
    /// 应用级会话标记（干净退出/异常退出识别）。
    pub app_session: AppSession,
}

impl Default for PersistedState {
    fn default() -> Self {
        Self {
            schema: SCHEMA_VERSION,
            tasks: BTreeMap::new(),
            window: WindowState::default(),
            ui: UiState::default(),
            app_session: AppSession::default(),
        }
    }
}

impl PersistedState {
    /// 按 id 读取任务状态。
    pub fn task(&self, id: &str) -> Option<&TaskState> {
        self.tasks.get(id)
    }

    /// 按 id 取任务状态；不存在时插入默认值。
    pub fn task_mut(&mut self, id: &str) -> &mut TaskState {
        self.tasks.entry(id.to_owned()).or_default()
    }

    /// 删除任务状态（配置删除任务时清理）。
    pub fn remove_task(&mut self, id: &str) -> Option<TaskState> {
        self.tasks.remove(id)
    }
}
