//! 归一化后的配置模型：默认值已应用、路径已展开并解析为绝对路径。
//!
//! 原始（可缺失字段的）反序列化结构见 [`super::raw`]，两者刻意分开：
//! `raw` 负责"用户写了什么"，本模块负责"最终生效的是什么"。

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::schedule::Window;

/// 当前支持的 schema 版本。
pub const SCHEMA_VERSION: i64 = 2;

/// 生成字符串枚举：`parse` 用于把 TOML 里的字符串转成强类型。
macro_rules! str_enum {
    ($(#[$meta:meta])* $name:ident { $($variant:ident => $text:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub enum $name {
            $($variant),+
        }

        impl $name {
            /// 全部合法取值（用于错误提示）。
            pub const VARIANTS: &'static [&'static str] = &[$($text),+];

            /// 配置/界面中的字符串取值。
            pub fn as_str(self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }

            /// 解析字符串取值；未知取值返回 `None`。
            pub fn parse(text: &str) -> Option<Self> {
                match text {
                    $($text => Some(Self::$variant),)+
                    _ => None,
                }
            }
        }
    };
}

/// 解析字符串枚举的统一接口，供校验器泛型调用。
pub(crate) trait StrEnum: Copy {
    const VARIANTS: &'static [&'static str];
    fn parse(text: &str) -> Option<Self>;
}

macro_rules! impl_str_enum {
    ($($name:ident),+ $(,)?) => {
        $(impl StrEnum for $name {
            const VARIANTS: &'static [&'static str] = $name::VARIANTS;
            fn parse(text: &str) -> Option<Self> {
                $name::parse(text)
            }
        })+
    };
}

str_enum!(Language { ZhCn => "zh-CN", EnUs => "en-US" });
str_enum!(Theme { System => "system", Light => "light", Dark => "dark" });
str_enum!(ApplyChanges { Restart => "restart", NextStart => "next_start" });
str_enum!(AutostartMode {
    RunKey => "run_key",
    ScheduledTask => "scheduled_task",
    Disabled => "disabled",
});
str_enum!(CoreLevel {
    Error => "error",
    Warn => "warn",
    Info => "info",
    Debug => "debug",
    Trace => "trace",
});
str_enum!(EnvProxy {
    Inherit => "inherit",
    Strip => "strip",
    BypassLocalhost => "bypass_localhost",
});
str_enum!(HealthKind {
    Http => "http",
    Heartbeat => "heartbeat",
    Process => "process",
});
str_enum!(RestartMode {
    OnFailure => "on-failure",
    Always => "always",
    Never => "never",
});
str_enum!(StopMethod {
    Terminate => "terminate",
    Graceful => "graceful",
});
str_enum!(Rotation { Size => "size", Daily => "daily", None => "none" });
str_enum!(Encoding {
    Auto => "auto",
    Utf8 => "utf-8",
    Gb18030 => "gb18030",
});

impl_str_enum!(
    Language,
    Theme,
    ApplyChanges,
    AutostartMode,
    CoreLevel,
    EnvProxy,
    HealthKind,
    RestartMode,
    StopMethod,
    Rotation,
    Encoding,
);

/// 整份配置。
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    pub version: u32,
    pub app: AppConfig,
    pub supervisor: SupervisorConfig,
    pub autostart: AutostartConfig,
    pub notifications: NotificationsConfig,
    pub logs: LogsConfig,
    /// 顺序与文件中的 `[[tasks]]` 一致；含校验失败的任务（用 issues 区分）。
    pub tasks: Vec<TaskConfig>,
}

impl Config {
    /// 按 id 查任务。
    pub fn task(&self, id: &str) -> Option<&TaskConfig> {
        self.tasks.iter().find(|task| task.id == id)
    }
}

/// `[app]`
#[derive(Debug, Clone, PartialEq)]
pub struct AppConfig {
    pub language: Language,
    pub theme: Theme,
    pub confirm_exit: bool,
    /// `None` = 使用 `%LOCALAPPDATA%/Rook`。
    pub data_dir: Option<PathBuf>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            language: Language::ZhCn,
            theme: Theme::System,
            confirm_exit: true,
            data_dir: None,
        }
    }
}

/// `[supervisor]`
#[derive(Debug, Clone, PartialEq)]
pub struct SupervisorConfig {
    pub start_stagger_ms: u64,
    pub shutdown_timeout_ms: u64,
    pub config_poll_interval_ms: u64,
    /// 配置热重载：是否监听配置文件（默认 true）。
    ///
    /// 关闭时监听状态为 disabled，不创建监听线程；修改后需重启应用生效。
    pub watch_config: bool,
    pub apply_changes: ApplyChanges,
    pub resume_grace_ms: u64,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            start_stagger_ms: 800,
            shutdown_timeout_ms: 8000,
            config_poll_interval_ms: 1000,
            watch_config: true,
            apply_changes: ApplyChanges::Restart,
            resume_grace_ms: 20_000,
        }
    }
}

/// `[autostart]`
#[derive(Debug, Clone, PartialEq)]
pub struct AutostartConfig {
    pub enabled: bool,
    pub mode: AutostartMode,
}

impl Default for AutostartConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            mode: AutostartMode::RunKey,
        }
    }
}

/// `[notifications]`
#[derive(Debug, Clone, PartialEq)]
pub struct NotificationsConfig {
    pub enabled: bool,
    pub crash_loop: bool,
    pub health_degraded: bool,
    pub recovered: bool,
}

impl Default for NotificationsConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            crash_loop: true,
            health_degraded: true,
            recovered: false,
        }
    }
}

/// 全局 `[logs]`（核心自身日志）。
#[derive(Debug, Clone, PartialEq)]
pub struct LogsConfig {
    pub core_level: CoreLevel,
    pub retention_days: u32,
    pub max_total_mb: u32,
}

impl Default for LogsConfig {
    fn default() -> Self {
        Self {
            core_level: CoreLevel::Info,
            retention_days: 7,
            max_total_mb: 500,
        }
    }
}

/// 一个被监督的任务。
#[derive(Debug, Clone, PartialEq)]
pub struct TaskConfig {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    /// 展示顺序：配置里显式写了 `order` 就用它，否则取 `[[tasks]]` 的序号（C7）。
    ///
    /// 只影响列表展示与排序，不影响监督、日志或端口语义。
    pub order: i64,
    pub command: String,
    pub args: Vec<String>,
    /// 已展开 `%VAR%` 并相对配置文件目录解析。
    pub working_dir: PathBuf,
    pub auto_start: bool,
    pub elevated: bool,
    pub notify: bool,
    /// 键按大小写不敏感去重，保留首次出现的写法。
    pub env: BTreeMap<String, String>,
    pub ports: Vec<u16>,
    pub env_proxy: EnvProxy,
    /// 插件清单导入后保留的来源信息；手工任务为 None。
    pub plugin: Option<PluginRef>,
    /// 每周运行时段；默认禁用。
    pub schedule: ScheduleConfig,
    pub panel_url: Option<String>,
    pub health: HealthConfig,
    pub restart: RestartConfig,
    pub stop: StopConfig,
    pub logs: TaskLogsConfig,
    pub power: PowerConfig,
    /// 资源守卫（C6）；两项都为 0 时引擎完全不做采样。
    pub limits: LimitsConfig,
}

/// 插件来源信息（`[[tasks]]` 上由导入器写入）。
#[derive(Debug, Clone, PartialEq)]
pub struct PluginRef {
    /// 清单里的 `plugin.id`。
    pub id: String,
    /// 清单绝对路径（已展开）。
    pub manifest: PathBuf,
    /// 导入时的 `plugin.version`；用于界面提示“清单有更新”。
    pub version: Option<String>,
}

/// `[tasks.schedule]`：每周运行时段（2026-11-04 用户决策；2026-11-05 修正）。
///
/// `enabled` 是唯一开关：进入窗口自动启动一次，离开窗口无条件停止任何存活
/// 实例（见 `crate::schedule::window`）。
///
/// v2（2026-11-15）：`start_in_window` / `stop_outside` 已从模型删除；旧配置里
/// 出现这两个键仍会被兼容解析（`config::raw`），校验层只发 Warning。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ScheduleConfig {
    pub enabled: bool,
    pub windows: Vec<Window>,
}

impl ScheduleConfig {
    /// 快照/界面用的星期列表（`mon`..`sun`）。
    pub fn window_days(&self) -> Vec<Vec<&'static str>> {
        self.windows
            .iter()
            .map(|window| window.days.iter().map(|day| day.as_str()).collect())
            .collect()
    }

    /// 是否有可用的时段。
    pub fn has_windows(&self) -> bool {
        !self.windows.is_empty()
    }

    /// 校验用：解析后的星期集合是否非空（空列表时段永不生效）。
    pub fn is_valid(&self) -> bool {
        self.windows
            .iter()
            .all(|window| !window.days.is_empty() && window.start != window.end)
    }
}

/// 一周中的一天（从 schedule 模块重导出，配置层只依赖纯类型）。
pub use crate::schedule::Weekday as ScheduleWeekday;

/// `[tasks.health]`
#[derive(Debug, Clone, PartialEq)]
pub struct HealthConfig {
    pub kind: HealthKind,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    pub failure_threshold: u32,
    pub startup_grace_ms: u64,
    /// 仅 `http` 模式；必须是 `http://`。
    pub url: Option<String>,
    pub expect_status: Vec<u16>,
    pub expect_body_contains: Option<String>,
    /// 仅 `heartbeat` 模式；缺省为 `<data>/heartbeats/<id>.hb`。
    pub path: Option<PathBuf>,
    /// 仅 `heartbeat` 模式。
    pub max_age_ms: Option<u64>,
    pub needs_human_status: Vec<String>,
}

impl Default for HealthConfig {
    fn default() -> Self {
        Self {
            kind: HealthKind::Process,
            interval_ms: 10_000,
            timeout_ms: 2000,
            failure_threshold: 3,
            startup_grace_ms: 20_000,
            url: None,
            expect_status: vec![200],
            expect_body_contains: None,
            path: None,
            max_age_ms: None,
            needs_human_status: Vec::new(),
        }
    }
}

/// `[tasks.restart]`
#[derive(Debug, Clone, PartialEq)]
pub struct RestartConfig {
    pub mode: RestartMode,
    pub initial_delay_ms: u64,
    pub max_delay_ms: u64,
    pub multiplier: f64,
    pub jitter: f64,
    pub max_consecutive_failures: u32,
    pub stable_after_ms: u64,
    /// 启动窗口长度（毫秒）；`0` = 不限。
    ///
    /// 与 `consecutive_failures` 无关：即使每次崩溃间隔都长于 `stable_after_ms`
    /// （计数被清零），窗口内启动次数超限仍会 paused(restart_budget)。
    pub window_ms: u64,
    /// 窗口内最多启动次数；`0` = 不限。
    pub max_starts_in_window: u32,
    pub pause_on_exit_codes: Vec<u32>,
}

impl Default for RestartConfig {
    fn default() -> Self {
        Self {
            mode: RestartMode::OnFailure,
            initial_delay_ms: 3000,
            max_delay_ms: 60_000,
            multiplier: 2.0,
            jitter: 0.2,
            max_consecutive_failures: 5,
            stable_after_ms: 60_000,
            // 10 分钟内最多 6 次启动（C1）。
            window_ms: 600_000,
            max_starts_in_window: 6,
            pause_on_exit_codes: Vec::new(),
        }
    }
}

/// `[tasks.stop]`
#[derive(Debug, Clone, PartialEq)]
pub struct StopConfig {
    pub method: StopMethod,
    /// 优雅停止宽限（毫秒）：发 `CTRL_BREAK` 后等脚本自行清理的上限。
    pub grace_ms: u64,
    /// 停止总预算：含宽限 + 兜底强杀后的确认等待。
    pub timeout_ms: u64,
}

impl Default for StopConfig {
    fn default() -> Self {
        Self {
            method: StopMethod::Graceful,
            grace_ms: 2500,
            timeout_ms: 8000,
        }
    }
}

/// `[tasks.logs]`
#[derive(Debug, Clone, PartialEq)]
pub struct TaskLogsConfig {
    /// 已解析的绝对路径；缺省为 `<data>/logs/<id>`。
    pub dir: PathBuf,
    pub rotation: Rotation,
    pub max_file_mb: u32,
    pub max_files: u32,
    pub max_days: u32,
    pub encoding: Encoding,
}

/// `[tasks.power]`
#[derive(Debug, Clone, PartialEq)]
pub struct PowerConfig {
    pub resume_grace_ms: u64,
}

impl Default for PowerConfig {
    fn default() -> Self {
        Self {
            resume_grace_ms: 20_000,
        }
    }
}

/// `[tasks.limits]`：资源守卫（C6）。
///
/// 两项都是 `0 = 不限制`；超限判定由 `crate::process::limits::LimitTracker`
/// 做“连续 3 次采样”确认，避免一次瞬时峰值就杀掉常驻任务。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LimitsConfig {
    /// 工作集（RSS）上限，单位 MB；0 = 不限。
    pub max_rss_mb: u32,
    /// 单核百分比上限（100 = 占满一个核）；0 = 不限。
    pub max_cpu_percent: u32,
}
