//! 界面只读视图（M1-4）：把 [`LoadedConfig`] 或加载错误转换成前端可直接渲染的快照。
//!
//! 本模块刻意不依赖 Tauri，也不做任何 I/O：输入是已经加载好的配置或错误，
//! 输出是纯数据。命令层（`engine`）负责在启动/重载时调用这里并保存结果。
//!
//! 序列化统一使用 camelCase，字段名即前端契约（见 `src/lib/api.ts`）。

use std::path::Path;

use serde::Serialize;

use crate::config::{
    ConfigError, ConfigIssue, ConfigWatchView, LoadError, LoadedConfig, TaskConfig,
};
use crate::state_store::TaskState;

/// 一次配置加载的完整只读视图。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    /// 应用版本（`CARGO_PKG_VERSION`）。
    pub app_version: String,
    /// 实际使用的配置文件路径。
    pub config_path: String,
    /// 当前配置内容的 revision（与 `read_config_text`/保存命令同一套算法）。
    ///
    /// 配置不可用（结构/I/O 错误）时为空串；前端保存草稿可直接带它做冲突检测，
    /// 无需先读整份 TOML（B6）。
    pub config_revision: String,
    /// 实际使用的数据目录；配置加载失败且没有 `--data-dir` 时为 `None`。
    pub data_dir: Option<String>,
    /// 结构错误：整份配置不可用。任务级问题不会出现在这里。
    pub config_error: Option<ConfigErrorView>,
    /// 全部任务级问题（错误 + 警告），含 `taskId`。
    pub issues: Vec<IssueView>,
    /// 配置中的任务；结构错误时为空。
    pub tasks: Vec<TaskSnapshot>,
    /// 退出前是否弹确认框（`[app].confirm_exit`；配置不可用时为 true）。
    pub confirm_exit: bool,
    /// 下次打开窗口是否需要展示一次“仍在托盘运行”的提示。
    pub tray_hint_pending: bool,
    /// 配置热重载监听状态（只读；由 Engine 在 refresh 时填入）。
    pub config_watch: ConfigWatchView,
}

/// 结构错误视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigErrorView {
    pub file: String,
    /// 行号（1 起）。
    pub line: usize,
    /// 列号（1 起）。
    pub col: usize,
    /// 字段路径；TOML/serde 层错误可能为空。
    pub path: String,
    pub message: String,
}

/// 任务级问题视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IssueView {
    /// `"error"` 或 `"warning"`。
    pub severity: String,
    pub task_id: Option<String>,
    pub path: String,
    pub message: String,
    pub line: Option<usize>,
    pub col: Option<usize>,
}

/// 插件来源视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginView {
    pub id: String,
    /// 清单绝对路径。
    pub manifest_path: String,
    /// 导入时的版本；清单升级后由界面提示同步。
    pub version: Option<String>,
    /// C8：清单版本高于已存 `version`（配置加载时比对，只提示、不自动同步）。
    ///
    /// 由 `Engine` 在配置重载时填好；`Snapshot::from_loaded` 一律初始化为 false。
    pub update_available: bool,
}

/// 每周运行时段视图。
///
/// 2026-11-05 语义修正：`schedule.enabled` 是唯一开关，进入窗口自动启动一次、
/// 离开窗口无条件停止。v2（2026-11-15）：已废弃的 `start_in_window` /
/// `stop_outside` / `schedule_started` 三个字段从 DTO 删除；旧前端读不到它们
/// 时会退回固定语义。
#[derive(Debug, Clone, PartialEq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleView {
    pub enabled: bool,
    pub windows: Vec<ScheduleWindowView>,
    /// 当前是否处于某个时段内（Engine 刷新时计算）。
    pub in_window: bool,
    /// 下次进入时段的时刻（Unix epoch 毫秒）。
    pub next_start_at: Option<i64>,
    /// 当前/下次时段结束的时刻（Unix epoch 毫秒）。
    pub next_end_at: Option<i64>,
}

/// 停止方式视图（2026-11-05）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StopView {
    /// `"graceful"` / `"terminate"`。
    pub method: String,
    /// 优雅停止宽限（毫秒）。
    pub grace_ms: u64,
    /// 停止总预算（毫秒）。
    pub timeout_ms: u64,
}

/// 一个运行时段视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleWindowView {
    /// `mon`..`sun`。
    pub days: Vec<String>,
    /// `HH:MM`。
    pub start: String,
    /// `HH:MM`。
    pub end: String,
}

impl ScheduleView {
    /// 从配置生成静态视图；动态字段由 Engine 在 refresh 时填入。
    pub fn from_config(schedule: &crate::config::ScheduleConfig) -> Self {
        let now = crate::schedule::now_local();
        let plan = crate::schedule::Schedule {
            windows: schedule.windows.clone(),
        };
        let active = plan.active(now);
        Self {
            enabled: schedule.enabled,
            windows: schedule
                .windows
                .iter()
                .map(|window| ScheduleWindowView {
                    days: window
                        .days
                        .iter()
                        .map(|day| day.as_str().to_owned())
                        .collect(),
                    start: crate::schedule::format_time(window.start),
                    end: crate::schedule::format_time(window.end),
                })
                .collect(),
            in_window: schedule.enabled && active.is_some(),
            next_start_at: plan
                .next_start(now)
                .and_then(crate::schedule::to_epoch_millis),
            next_end_at: plan
                .next_end(now)
                .and_then(crate::schedule::to_epoch_millis),
        }
    }
}

/// 单个任务的只读视图。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TaskSnapshot {
    pub id: String,
    pub name: String,
    pub enabled: bool,
    pub command: String,
    pub args: Vec<String>,
    pub working_dir: String,
    pub auto_start: bool,
    pub elevated: bool,
    pub notify: bool,
    /// `"http"` / `"heartbeat"` / `"process"`。
    pub health_type: String,
    pub ports: Vec<u16>,
    pub panel_url: Option<String>,
    /// 插件来源（清单导入的任务）；手工任务为 None。
    pub plugin: Option<PluginView>,
    /// 每周运行时段（2026-11-04）：静态窗口 + 当前是否在窗内/下次起止。
    pub schedule: ScheduleView,
    /// 停止方式（2026-11-05）：graceful/terminate、宽限与总预算。
    pub stop: StopView,
    /// 该任务没有任务级错误。
    pub valid: bool,
    /// 仅属于该任务的问题。
    pub issues: Vec<IssueView>,
    /// 运行时状态；M2-4b 起由 Engine 填充。配置结构错误或尚未创建实例时为 `None`。
    pub runtime: Option<RuntimeView>,
    /// M3-4a：从 state.json 恢复的监督状态（用户意图/失败预算/paused/needs_human）。
    ///
    /// 未加载状态文件或首次运行时为默认值；Engine 在 refresh 时填入。
    pub supervisor: SupervisorView,
    /// A3：后端按权威规则算出的展示状态（界面/托盘只做文案映射，不再自行推导）。
    ///
    /// 取值：`invalid` / `unknown` / `starting` / `running` / `stopping` /
    /// `stopped` / `paused` / `needs_human` / `backoff` / `disabled`。
    pub display_status: String,
    /// A3：后端算出的可执行动作，界面与托盘只读该字段。
    pub allowed_actions: AllowedActions,
}

/// 任务可执行动作（A3）。
///
/// 序列化为 `{ "start": bool, "stop": bool, "restart": bool }`；语义与
/// [`crate::runtime::coordinator::manual_start_allowed`] /
/// [`crate::runtime::coordinator::manual_restart_allowed`] 以及运行状态机一致。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllowedActions {
    pub start: bool,
    pub stop: bool,
    pub restart: bool,
}

/// 展示状态（纯函数，可单测）；顺序即优先级。
///
/// 与历史实现保持一致：`runtime=running` 但监督层仍 `starting` 时显示"启动中"；
/// 运行时不在 `starting/running/stopping` 时依次回落到监督状态。
pub fn compute_display_status(
    valid: bool,
    enabled: bool,
    runtime: Option<&str>,
    last: Option<&str>,
    needs_human: bool,
    paused: bool,
) -> &'static str {
    if !valid {
        return "invalid";
    }
    match runtime {
        Some("running") if last == Some("starting") => return "starting",
        Some("starting") => return "starting",
        Some("running") => return "running",
        Some("stopping") => return "stopping",
        _ => {}
    }
    if needs_human {
        return "needs_human";
    }
    if paused {
        return "paused";
    }
    if last == Some("backoff") {
        return "backoff";
    }
    if !enabled {
        return "disabled";
    }
    if runtime.is_none() {
        return "unknown";
    }
    "stopped"
}

/// 可执行动作（纯函数，可单测）。
///
/// - `start`：仅 `stopped`（`starting` 期间启动会被运行状态机拒绝）；
/// - `stop`：`running` / `stopping`（`stopping` 允许重复 stop 等待收尾）；
/// - `restart`：`stopped` / `running`；`enabled=false` 不可启动/重启，
///   但 Stop 仍允许收敛运行中实例（docs/02 §4）。
pub fn compute_allowed_actions(
    valid: bool,
    enabled: bool,
    runtime: Option<&str>,
) -> AllowedActions {
    AllowedActions {
        start: valid && enabled && runtime == Some("stopped"),
        stop: valid && matches!(runtime, Some("running" | "stopping")),
        restart: valid && enabled && matches!(runtime, Some("stopped" | "running")),
    }
}

/// 恢复的监督状态视图（state.json 的非敏感投影）。
///
/// 与 [`crate::state_store::TaskState`] 对应但只暴露界面需要的稳定字段：
/// 枚举统一为字符串，时间统一为 Unix epoch 毫秒。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SupervisorView {
    /// `"stopped"` / `"paused"` / `None`。
    pub desired_override: Option<String>,
    /// 连续失败次数（重启应用后保留）。
    pub consecutive_failures: u32,
    /// 累计启动次数。
    pub total_starts: u64,
    /// 累计自动重启次数。
    pub total_restarts: u64,
    /// `"crash_loop"` / `"needs_human"` / ... / `None`。
    pub paused_reason: Option<String>,
    /// 最近一次观察到的监督状态。
    pub last_status: Option<String>,
    /// 下次退避重试时刻（Unix epoch 毫秒）；不接调度时只用于展示。
    pub next_retry_at: Option<i64>,
    /// 需要人工处理标志。
    pub needs_human: NeedsHumanView,
    /// M3-6a：配置已修改但进程仍在用旧配置（`apply_changes=next_start`），
    /// 下一次 Restart 后生效。由 Engine 在 refresh 时填入，不落盘。
    pub restart_pending: bool,
}

/// `needs_human` 标志视图。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct NeedsHumanView {
    pub active: bool,
    pub source: Option<String>,
    pub message: Option<String>,
    /// 置位时刻（Unix epoch 毫秒）。
    pub since: Option<i64>,
}

impl SupervisorView {
    /// 把 state.json 的任务状态转成界面视图。
    pub fn capture(state: &TaskState) -> Self {
        Self {
            desired_override: state
                .desired_override
                .map(|value| value.as_str().to_owned()),
            consecutive_failures: state.consecutive_failures,
            total_starts: state.total_starts,
            total_restarts: state.total_restarts,
            paused_reason: state
                .paused_reason
                .as_ref()
                .map(|value| value.as_str().to_owned()),
            last_status: state
                .last_status
                .as_ref()
                .map(|value| value.as_str().to_owned()),
            next_retry_at: state.next_retry_at.map(epoch_millis_of),
            needs_human: NeedsHumanView {
                active: state.needs_human.active,
                source: state.needs_human.source.clone(),
                message: state.needs_human.message.clone(),
                since: state.needs_human.since.map(epoch_millis_of),
            },
            // 待重启标记属于 Engine 的运行时判断，不来自 state.json。
            restart_pending: false,
        }
    }
}

/// `Timestamp` → Unix epoch 毫秒。
fn epoch_millis_of(timestamp: crate::state_store::Timestamp) -> i64 {
    timestamp.datetime().timestamp_millis()
}

/// 运行时状态视图（M2-4b）。
///
/// 与 [`crate::runtime::RuntimeSnapshot`] 一一对应，但只包含前端需要的稳定字段：
/// 时间统一为 Unix epoch 毫秒，枚举统一为字符串。本类型不依赖 Tauri。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeView {
    /// `"stopped"` / `"starting"` / `"running"` / `"stopping"`。
    pub status: String,
    /// 当前根进程 PID；`stopped` 时为 `None`。
    pub pid: Option<u32>,
    /// 最近一次启动分配的实例 id。
    pub instance_id: Option<String>,
    /// 最近一次成功启动的时间（Unix epoch 毫秒）。
    pub started_at: Option<i64>,
    /// 最近一次实例的根进程退出码。
    pub exit_code: Option<u32>,
    /// `"natural"` / `"user_stop"`。
    pub exit_reason: Option<String>,
    /// 最近一次启动失败的原因。
    pub last_error: Option<RuntimeErrorView>,
    /// 退出时 Job 内是否已无活动进程。
    pub tree_empty: Option<bool>,
    /// 是否因树未空而强制终止了进程树。
    pub forced_kill: bool,
    /// 当前活动日志文件（`<dir>/current.log`）。
    pub log_path: String,
    /// 日志采集统计。
    pub log_stats: LogStatsView,
}

/// 运行时错误视图：阶段用稳定英文标识，说明面向用户。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeErrorView {
    /// `"port_check"` / `"spawn"` / `"logs"` / `"watch"` / `"stop"` / `"invalid_state"`。
    pub stage: String,
    /// 底层错误码（Win32）；没有系统码时为 `None`。
    pub code: Option<u32>,
    pub message: String,
}

/// 日志采集统计视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogStatsView {
    /// 从管道读到的字节数。
    pub read_bytes: u64,
    /// 成功落盘的字节数。
    pub written_bytes: u64,
    /// 已读未落盘的字节数。
    pub dropped_bytes: u64,
    /// 文件打开/写入/重命名失败次数。
    pub io_errors: u64,
    /// 管道读取/查询失败次数。
    pub read_errors: u64,
    /// 成功轮转次数。
    pub rotations: u64,
    /// 因队列满丢弃的块数。
    pub queue_drops: u64,
}

#[cfg(windows)]
impl RuntimeView {
    /// 从运行时快照生成前端视图；`started_at` 转为 Unix epoch 毫秒。
    pub fn capture(snapshot: &crate::runtime::RuntimeSnapshot) -> Self {
        Self {
            status: snapshot.status.as_str().to_owned(),
            pid: snapshot.pid,
            instance_id: snapshot.instance_id.clone(),
            started_at: snapshot.started_at.and_then(epoch_millis),
            exit_code: snapshot.exit_code,
            exit_reason: snapshot
                .exit_reason
                .map(|reason| reason.as_str().to_owned()),
            last_error: snapshot.last_error.as_ref().map(RuntimeErrorView::capture),
            tree_empty: snapshot.tree_empty,
            forced_kill: snapshot.forced_kill,
            log_path: snapshot.log_path.to_string_lossy().into_owned(),
            log_stats: LogStatsView::capture(snapshot.logs),
        }
    }
}

#[cfg(windows)]
impl RuntimeErrorView {
    fn capture(error: &crate::runtime::RuntimeError) -> Self {
        Self {
            stage: error.stage().key().to_owned(),
            code: error.code(),
            message: error.message().to_owned(),
        }
    }
}

#[cfg(windows)]
impl LogStatsView {
    fn capture(stats: crate::logs::LogStats) -> Self {
        Self {
            read_bytes: stats.read_bytes,
            written_bytes: stats.written_bytes,
            dropped_bytes: stats.dropped_bytes,
            io_errors: stats.io_errors,
            read_errors: stats.read_errors,
            rotations: stats.rotations,
            queue_drops: stats.queue_drops,
        }
    }
}

#[cfg(windows)]
fn epoch_millis(time: std::time::SystemTime) -> Option<i64> {
    let millis = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(millis).ok()
}

impl Snapshot {
    /// 从成功加载的配置生成快照。
    pub fn from_loaded(loaded: &LoadedConfig) -> Self {
        let issues: Vec<IssueView> = loaded.issues.iter().map(IssueView::from).collect();
        // C7：按 `order` 稳定排序（`sort_by_key` 是稳定排序，相同 order 保持文件顺序）。
        let mut ordered: Vec<&TaskConfig> = loaded.config.tasks.iter().collect();
        ordered.sort_by_key(|task| task.order);
        let tasks = ordered
            .into_iter()
            .map(|task| TaskSnapshot {
                id: task.id.clone(),
                name: task.name.clone(),
                enabled: task.enabled,
                command: task.command.clone(),
                args: task.args.clone(),
                working_dir: task.working_dir.to_string_lossy().into_owned(),
                auto_start: task.auto_start,
                elevated: task.elevated,
                notify: task.notify,
                health_type: task.health.kind.as_str().to_owned(),
                ports: task.ports.clone(),
                panel_url: task.panel_url.clone(),
                plugin: task.plugin.as_ref().map(|plugin| PluginView {
                    id: plugin.id.clone(),
                    manifest_path: plugin.manifest.to_string_lossy().into_owned(),
                    version: plugin.version.clone(),
                    // C8：可更新标记由 Engine 在配置加载时比对清单版本后填入。
                    update_available: false,
                }),
                schedule: ScheduleView::from_config(&task.schedule),
                stop: StopView {
                    method: task.stop.method.as_str().to_owned(),
                    grace_ms: task.stop.grace_ms,
                    timeout_ms: task.stop.timeout_ms,
                },
                valid: loaded.is_task_valid(&task.id),
                issues: loaded
                    .issues
                    .iter()
                    .filter(|issue| issue.task_id.as_deref() == Some(task.id.as_str()))
                    .map(IssueView::from)
                    .collect(),
                runtime: None,
                supervisor: SupervisorView::default(),
                display_status: compute_display_status(
                    loaded.is_task_valid(&task.id),
                    task.enabled,
                    None,
                    None,
                    false,
                    false,
                )
                .to_owned(),
                allowed_actions: compute_allowed_actions(
                    loaded.is_task_valid(&task.id),
                    task.enabled,
                    None,
                ),
            })
            .collect();

        Self {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            config_path: loaded.config_path.to_string_lossy().into_owned(),
            config_revision: loaded.revision.clone(),
            data_dir: Some(loaded.data_dir.to_string_lossy().into_owned()),
            config_error: None,
            issues,
            tasks,
            confirm_exit: loaded.config.app.confirm_exit,
            tray_hint_pending: false,
            config_watch: ConfigWatchView::default(),
        }
    }

    /// 从加载失败生成"错误态"快照：任务列表为空，只展示结构错误。
    ///
    /// I/O 错误（读不到文件、数据目录创建失败）同样收敛到这里，行/列用 1 占位，
    /// 因为界面上仍需要一个统一的错误横幅。
    pub fn from_load_error(error: &LoadError, data_dir: Option<&Path>) -> Self {
        let (file, line, col, path, message) = match error {
            LoadError::Invalid(ConfigError {
                file,
                line,
                col,
                path,
                message,
            }) => (
                file.to_string_lossy().into_owned(),
                *line,
                *col,
                path.clone(),
                message.clone(),
            ),
            LoadError::Io { path, source } => (
                path.to_string_lossy().into_owned(),
                1,
                1,
                String::new(),
                format!("配置 I/O 失败：{source}"),
            ),
        };

        Self {
            app_version: env!("CARGO_PKG_VERSION").to_owned(),
            config_path: file.clone(),
            // 配置不可用：没有可信的 revision，保存命令应先让用户读文本。
            config_revision: String::new(),
            data_dir: data_dir.map(|dir| dir.to_string_lossy().into_owned()),
            config_error: Some(ConfigErrorView {
                file,
                line,
                col,
                path,
                message,
            }),
            issues: Vec::new(),
            tasks: Vec::new(),
            // 配置不可用时保守地保留确认行为。
            confirm_exit: true,
            tray_hint_pending: false,
            config_watch: ConfigWatchView::default(),
        }
    }
}

impl From<&ConfigIssue> for IssueView {
    fn from(issue: &ConfigIssue) -> Self {
        Self {
            severity: issue.severity.as_str().to_owned(),
            task_id: issue.task_id.clone(),
            path: issue.path.clone(),
            message: issue.message.clone(),
            line: issue.line,
            col: issue.col,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::*;
    use crate::config::{LoadContext, MapEnv, load_file};

    /// 临时目录 + 假 PATH + 假 LOCALAPPDATA 的测试夹具。
    struct Fx {
        _tmp: tempfile::TempDir,
        root: PathBuf,
        bin: PathBuf,
        env: MapEnv,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().expect("创建临时目录");
            let root = tmp.path().to_path_buf();
            let bin = root.join("bin");
            fs::create_dir_all(&bin).expect("创建 bin");
            let env = MapEnv::new()
                .with("PATH", bin.to_string_lossy().to_string())
                .with("PATHEXT", ".COM;.EXE;.BAT;.CMD");
            Self {
                _tmp: tmp,
                root,
                bin,
                env,
            }
        }

        fn exe(&self, name: &str) {
            fs::write(self.bin.join(name), b"").expect("写假可执行文件");
        }

        fn dir(&self, relative: &str) -> PathBuf {
            let path = self.root.join(relative);
            fs::create_dir_all(&path).expect("创建目录");
            path
        }

        /// TOML 里用正斜杠，避免反斜杠被当成转义。
        fn toml_path(&self, path: &Path) -> String {
            path.to_string_lossy().replace('\\', "/")
        }

        fn load(&self, source: &str) -> Result<LoadedConfig, LoadError> {
            let path = self.root.join("Rook.toml");
            fs::write(&path, source).expect("写配置文件");
            let local = self.root.join("local");
            load_file(&LoadContext {
                config_path: &path,
                data_dir_override: None,
                local_appdata: Some(&local),
                env: &self.env,
            })
        }
    }

    #[test]
    fn tasks_sorted_by_order_with_stable_ties() {
        let fx = Fx::new();
        fx.exe("node.exe");
        let dir = fx.toml_path(&fx.dir("work"));
        let source = format!(
            r#"version = 1

[[tasks]]
id = "third"
name = "丙"
command = "node"
working_dir = "{dir}"
order = 2

[[tasks]]
id = "first"
name = "甲"
command = "node"
working_dir = "{dir}"
order = 0

[[tasks]]
id = "tie-b"
name = "乙"
command = "node"
working_dir = "{dir}"
order = 1

[[tasks]]
id = "tie-a"
name = "丁"
command = "node"
working_dir = "{dir}"
order = 1
"#
        );

        let loaded = fx.load(&source).expect("配置应可加载");
        let snapshot = Snapshot::from_loaded(&loaded);
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        // 相同 order 保持文件顺序（稳定排序）。
        assert_eq!(ids, ["first", "tie-b", "tie-a", "third"]);
    }

    #[test]
    fn missing_order_defaults_to_file_index() {
        let fx = Fx::new();
        fx.exe("node.exe");
        let dir = fx.toml_path(&fx.dir("work"));
        let source = format!(
            r#"version = 1

[[tasks]]
id = "alpha"
name = "甲"
command = "node"
working_dir = "{dir}"

[[tasks]]
id = "beta"
name = "乙"
command = "node"
working_dir = "{dir}"
"#
        );

        let loaded = fx.load(&source).expect("配置应可加载");
        assert_eq!(loaded.config.task("alpha").expect("alpha").order, 0);
        assert_eq!(loaded.config.task("beta").expect("beta").order, 1);

        let snapshot = Snapshot::from_loaded(&loaded);
        let ids: Vec<&str> = snapshot.tasks.iter().map(|task| task.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "beta"]);
    }

    #[test]
    fn valid_config_with_two_tasks_maps_all_fields() {
        let fx = Fx::new();
        fx.exe("node.exe");
        let alpha = fx.toml_path(&fx.dir("alpha"));
        let beta = fx.toml_path(&fx.dir("beta"));
        let source = format!(
            r#"
version = 1

[[tasks]]
id = "alpha"
name = "甲任务"
command = "node"
args = ["a.js", "--port", "3000"]
working_dir = "{alpha}"
auto_start = true
elevated = true
notify = false
ports = [3000, 3001]
panel_url = "http://127.0.0.1:3000"

  [tasks.health]
  type = "http"
  url = "http://127.0.0.1:3000/health"

[[tasks]]
id = "beta"
name = "乙任务"
command = "node"
working_dir = "{beta}"
auto_start = false

  [tasks.health]
  type = "heartbeat"
  max_age_ms = 30000
"#
        );

        let loaded = fx.load(&source).expect("配置应可加载");
        let snapshot = Snapshot::from_loaded(&loaded);

        assert_eq!(snapshot.app_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(
            snapshot.config_path,
            loaded.config_path.to_string_lossy().as_ref()
        );
        assert_eq!(
            snapshot.data_dir.as_deref(),
            Some(loaded.data_dir.to_string_lossy().as_ref())
        );
        assert!(snapshot.config_error.is_none());
        // 配置合法（loaded、任务 valid），只有 B5 的预留字段提示：elevated 未实现。
        assert_eq!(snapshot.issues.len(), 1, "{:?}", snapshot.issues);
        assert_eq!(snapshot.issues[0].path, "tasks[0].elevated");
        assert_eq!(snapshot.issues[0].severity, "warning");
        assert_eq!(snapshot.issues[0].task_id.as_deref(), Some("alpha"));

        assert_eq!(snapshot.tasks.len(), 2);
        let alpha_task = &snapshot.tasks[0];
        assert_eq!(alpha_task.id, "alpha");
        assert_eq!(alpha_task.name, "甲任务");
        assert_eq!(alpha_task.command, "node");
        assert_eq!(alpha_task.args, ["a.js", "--port", "3000"]);
        assert_eq!(alpha_task.working_dir, alpha);
        assert!(alpha_task.auto_start);
        assert!(alpha_task.elevated);
        assert!(!alpha_task.notify);
        assert_eq!(alpha_task.health_type, "http");
        assert_eq!(alpha_task.ports, [3000, 3001]);
        assert_eq!(
            alpha_task.panel_url.as_deref(),
            Some("http://127.0.0.1:3000")
        );
        assert!(alpha_task.valid);
        // 任务级问题只有 elevated 的提示性 Warning。
        assert_eq!(alpha_task.issues.len(), 1, "{:?}", alpha_task.issues);
        assert_eq!(alpha_task.issues[0].path, "tasks[0].elevated");
        assert_eq!(alpha_task.issues[0].severity, "warning");

        let beta_task = &snapshot.tasks[1];
        assert_eq!(beta_task.health_type, "heartbeat");
        assert!(!beta_task.auto_start);
        assert!(beta_task.valid);
        assert!(beta_task.issues.is_empty(), "{:?}", beta_task.issues);
    }

    #[test]
    fn supervisor_view_captures_restored_state() {
        use crate::state_store::{
            DesiredOverride, LastStatus, NeedsHumanState, PausedReason, TaskState, Timestamp,
        };

        let since = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
        let retry_at = Timestamp::parse("2026-11-02T04:50:00Z").expect("合法时间");
        let state = TaskState {
            desired_override: Some(DesiredOverride::Paused),
            consecutive_failures: 3,
            total_starts: 12,
            total_restarts: 2,
            paused_reason: Some(PausedReason::CrashLoop),
            next_retry_at: Some(retry_at),
            last_status: Some(LastStatus::Paused),
            needs_human: NeedsHumanState {
                active: true,
                source: Some("heartbeat".to_owned()),
                message: Some("login_required".to_owned()),
                since: Some(since),
            },
            ..TaskState::default()
        };

        let view = SupervisorView::capture(&state);
        assert_eq!(view.desired_override.as_deref(), Some("paused"));
        assert_eq!(view.consecutive_failures, 3);
        assert_eq!(view.total_starts, 12);
        assert_eq!(view.total_restarts, 2);
        assert_eq!(view.paused_reason.as_deref(), Some("crash_loop"));
        assert_eq!(view.last_status.as_deref(), Some("paused"));
        assert_eq!(
            view.next_retry_at,
            Some(retry_at.datetime().timestamp_millis())
        );
        assert!(view.needs_human.active);
        assert_eq!(view.needs_human.source.as_deref(), Some("heartbeat"));
        assert_eq!(view.needs_human.message.as_deref(), Some("login_required"));
        assert_eq!(
            view.needs_human.since,
            Some(since.datetime().timestamp_millis())
        );

        let value = serde_json::to_value(&view).expect("序列化监督视图");
        assert_eq!(value["desiredOverride"], "paused");
        assert_eq!(value["consecutiveFailures"], 3);
        assert_eq!(value["pausedReason"], "crash_loop");
        assert_eq!(value["needsHuman"]["active"], true);
        assert!(value.get("desired_override").is_none());

        // 默认值：没有恢复状态时不伪造任何覆盖/暂停/人工标志。
        let empty = SupervisorView::capture(&TaskState::default());
        assert!(empty.desired_override.is_none());
        assert!(empty.paused_reason.is_none());
        assert!(empty.last_status.is_none());
        assert!(empty.next_retry_at.is_none());
        assert!(!empty.needs_human.active);
        // M3-6a：restart_pending 由 Engine 填充，state.json 里没有对应字段。
        assert!(!empty.restart_pending);
    }

    #[test]
    fn supervisor_view_roundtrips_through_state_store() {
        use crate::state_store::{
            DesiredOverride, LastStatus, NeedsHumanState, PausedReason, PersistedState, StateStore,
            TaskState, Timestamp,
        };

        let dir = tempfile::tempdir().expect("临时目录");
        let store = StateStore::in_dir(dir.path());
        let since = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
        let retry_at = Timestamp::parse("2026-11-02T04:50:00Z").expect("合法时间");
        let mut persisted = PersistedState::default();
        persisted.tasks.insert(
            "alpha".to_owned(),
            TaskState {
                desired_override: Some(DesiredOverride::Paused),
                consecutive_failures: 3,
                total_starts: 12,
                total_restarts: 2,
                paused_reason: Some(PausedReason::CrashLoop),
                next_retry_at: Some(retry_at),
                last_status: Some(LastStatus::Paused),
                needs_human: NeedsHumanState {
                    active: true,
                    source: Some("heartbeat".to_owned()),
                    message: Some("login_required".to_owned()),
                    since: Some(since),
                },
                ..TaskState::default()
            },
        );

        assert_eq!(
            store.save(&persisted).expect("保存应成功"),
            crate::state_store::SaveOutcome::Written
        );
        let loaded = store.load();
        assert_eq!(loaded.status, crate::state_store::LoadStatus::Loaded);
        let view = SupervisorView::capture(loaded.state.task("alpha").expect("恢复的任务"));

        assert_eq!(view.desired_override.as_deref(), Some("paused"));
        assert_eq!(view.consecutive_failures, 3);
        assert_eq!(view.total_starts, 12);
        assert_eq!(view.total_restarts, 2);
        assert_eq!(view.paused_reason.as_deref(), Some("crash_loop"));
        assert_eq!(view.last_status.as_deref(), Some("paused"));
        assert_eq!(
            view.next_retry_at,
            Some(retry_at.datetime().timestamp_millis())
        );
        assert!(view.needs_human.active);
        assert_eq!(view.needs_human.source.as_deref(), Some("heartbeat"));
    }

    #[test]
    fn structural_error_becomes_error_snapshot() {
        let fx = Fx::new();
        let error = fx.load("version = 3\n").expect_err("不支持的版本应被拒绝");
        assert!(error.structural().is_some());

        let data_dir = fx.root.join("data");
        let snapshot = Snapshot::from_load_error(&error, Some(&data_dir));

        assert!(snapshot.tasks.is_empty());
        assert!(snapshot.issues.is_empty());
        assert_eq!(
            snapshot.data_dir.as_deref(),
            Some(data_dir.to_string_lossy().as_ref())
        );

        let config_error = snapshot.config_error.expect("应包含结构错误");
        assert!(config_error.file.ends_with("Rook.toml"));
        assert_eq!(snapshot.config_path, config_error.file);
        assert!(config_error.line >= 1);
        assert!(config_error.col >= 1);
        assert!(
            config_error.message.contains("配置版本"),
            "{}",
            config_error.message
        );
    }

    #[test]
    fn missing_command_marks_task_invalid() {
        let fx = Fx::new();
        let workdir = fx.toml_path(&fx.dir("broken"));
        let source = format!(
            r#"
version = 1

[[tasks]]
id = "broken"
name = "坏命令"
command = "definitely-not-installed-xyz"
working_dir = "{workdir}"
"#
        );

        let loaded = fx.load(&source).expect("任务级错误不应影响整份配置");
        let snapshot = Snapshot::from_loaded(&loaded);

        assert_eq!(snapshot.tasks.len(), 1);
        let task = &snapshot.tasks[0];
        assert!(!task.valid);
        // 命令错误 + process 健康检查的粗粒度警告。
        assert_eq!(task.issues.len(), 2);
        let error = task
            .issues
            .iter()
            .find(|issue| issue.severity == "error")
            .expect("应包含命令错误");
        assert_eq!(error.task_id.as_deref(), Some("broken"));
        assert_eq!(error.path, "tasks[0].command");
        assert!(error.line.is_some());
        assert!(task.issues.iter().any(|issue| issue.severity == "warning"));

        assert_eq!(snapshot.issues.len(), 2);
        assert!(
            snapshot
                .issues
                .iter()
                .any(|issue| issue.severity == "error")
        );
    }

    #[test]
    fn serializes_camel_case_field_names() {
        let fx = Fx::new();
        fx.exe("node.exe");
        let workdir = fx.toml_path(&fx.dir("wd"));
        let source = format!(
            r#"
version = 1

[[tasks]]
id = "t"
name = "任务"
command = "node"
working_dir = "{workdir}"
auto_start = true
"#
        );

        let loaded = fx.load(&source).expect("配置应可加载");
        let value = serde_json::to_value(Snapshot::from_loaded(&loaded)).expect("序列化快照");

        for key in [
            "appVersion",
            "configPath",
            "dataDir",
            "configError",
            "issues",
            "tasks",
            "confirmExit",
            "trayHintPending",
            "configWatch",
        ] {
            assert!(value.get(key).is_some(), "缺少字段 {key}");
        }
        assert!(value.get("app_version").is_none());
        assert!(value.get("config_path").is_none());

        let task = &value["tasks"][0];
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
            "healthType",
            "ports",
            "panelUrl",
            "schedule",
            "stop",
            "valid",
            "issues",
            "runtime",
            "supervisor",
            "displayStatus",
            "allowedActions",
        ] {
            assert!(task.get(key).is_some(), "任务缺少字段 {key}");
        }
        let supervisor = &task["supervisor"];
        for key in [
            "desiredOverride",
            "consecutiveFailures",
            "totalStarts",
            "totalRestarts",
            "pausedReason",
            "lastStatus",
            "nextRetryAt",
            "needsHuman",
            "restartPending",
        ] {
            assert!(supervisor.get(key).is_some(), "监督视图缺少字段 {key}");
        }
        assert_eq!(supervisor["consecutiveFailures"], 0);
        assert_eq!(supervisor["needsHuman"]["active"], false);
        assert_eq!(supervisor["restartPending"], false);
        assert!(supervisor["desiredOverride"].is_null());
        assert!(task.get("working_dir").is_none());
        assert!(task.get("auto_start").is_none());
        assert!(task.get("health_type").is_none());
        assert!(task.get("panel_url").is_none());
        assert_eq!(task["healthType"], "process");
        assert_eq!(task["stop"]["method"], "graceful");
        assert_eq!(task["stop"]["graceMs"], 2500);
        assert_eq!(task["stop"]["timeoutMs"], 8000);
        assert!(task["runtime"].is_null());
        // A3：直接由配置生成的快照还没有运行时，按"未知"占位；动作全禁用。
        assert_eq!(task["displayStatus"], "unknown");
        assert_eq!(task["allowedActions"]["start"], false);
        assert_eq!(task["allowedActions"]["stop"], false);
        assert_eq!(task["allowedActions"]["restart"], false);
        assert!(task["allowedActions"].get("start").is_some());
        assert!(value["configError"].is_null());
        // 直接由配置生成的快照还没有监听状态，Engine refresh 时再填入。
        assert_eq!(value["configWatch"]["state"], "disabled");
        assert_eq!(value["configWatch"]["enabled"], false);
    }
}

#[cfg(all(test, windows))]
mod runtime_view_tests {
    use std::path::PathBuf;
    use std::time::{Duration, UNIX_EPOCH};

    use super::*;
    use crate::logs::LogStats;
    use crate::runtime::{ExitReason, RuntimeError, RuntimeSnapshot, RuntimeStage, TaskStatus};

    fn snapshot(status: TaskStatus) -> RuntimeSnapshot {
        RuntimeSnapshot {
            id: "alpha".to_owned(),
            status,
            pid: Some(4242),
            instance_id: Some("alpha-1-abc".to_owned()),
            started_at: Some(UNIX_EPOCH + Duration::from_millis(1_700_000_000_123)),
            exit_code: None,
            exit_reason: None,
            tree_empty: None,
            forced_kill: false,
            last_error: None,
            logs: LogStats {
                read_bytes: 100,
                written_bytes: 90,
                dropped_bytes: 10,
                io_errors: 1,
                read_errors: 2,
                rotations: 3,
                queue_drops: 4,
            },
            log_path: PathBuf::from("C:/data/logs/alpha/current.log"),
        }
    }

    /// 运行中实例：状态/pid/实例 id/epoch 毫秒/日志统计全部落到 camelCase 字段。
    #[test]
    fn capture_maps_running_snapshot_to_camel_case_view() {
        let view = RuntimeView::capture(&snapshot(TaskStatus::Running));
        assert_eq!(view.status, "running");
        assert_eq!(view.pid, Some(4242));
        assert_eq!(view.instance_id.as_deref(), Some("alpha-1-abc"));
        assert_eq!(view.started_at, Some(1_700_000_000_123));
        assert_eq!(view.exit_code, None);
        assert_eq!(view.exit_reason, None);
        assert!(view.last_error.is_none());
        assert!(!view.forced_kill);
        assert!(view.log_path.ends_with("current.log"));

        let value = serde_json::to_value(&view).expect("序列化运行时视图");
        for key in [
            "status",
            "pid",
            "instanceId",
            "startedAt",
            "exitCode",
            "exitReason",
            "lastError",
            "treeEmpty",
            "forcedKill",
            "logPath",
            "logStats",
        ] {
            assert!(value.get(key).is_some(), "缺少字段 {key}");
        }
        assert!(value.get("instance_id").is_none());
        assert!(value.get("started_at").is_none());
        assert!(value.get("log_path").is_none());

        let stats = &value["logStats"];
        for key in [
            "readBytes",
            "writtenBytes",
            "droppedBytes",
            "ioErrors",
            "readErrors",
            "rotations",
            "queueDrops",
        ] {
            assert!(stats.get(key).is_some(), "日志统计缺少字段 {key}");
        }
        assert_eq!(stats["readBytes"], 100);
        assert_eq!(stats["queueDrops"], 4);
        assert_eq!(value["startedAt"], 1_700_000_000_123u64);
    }

    /// 启动失败 + 自然退出：错误阶段/错误码/退出原因都要能序列化。
    #[test]
    fn capture_maps_error_and_exit_fields() {
        let mut failed = snapshot(TaskStatus::Stopped);
        failed.pid = None;
        failed.started_at = None;
        failed.last_error = Some(RuntimeError::new(
            RuntimeStage::PortCheck,
            Some(10048),
            "端口 3000 已被监听".to_owned(),
        ));

        let view = RuntimeView::capture(&failed);
        let error = view.last_error.as_ref().expect("应包含 lastError");
        assert_eq!(error.stage, "port_check");
        assert_eq!(error.code, Some(10048));
        assert!(error.message.contains("3000"));

        let value = serde_json::to_value(&view).expect("序列化失败视图");
        assert_eq!(value["status"], "stopped");
        assert_eq!(value["lastError"]["stage"], "port_check");
        assert_eq!(value["lastError"]["code"], 10048);
        assert!(value["lastError"].get("message").is_some());

        let mut exited = snapshot(TaskStatus::Stopped);
        exited.pid = None;
        exited.exit_code = Some(0);
        exited.exit_reason = Some(ExitReason::UserStop);
        exited.tree_empty = Some(true);
        exited.forced_kill = true;

        let view = RuntimeView::capture(&exited);
        assert_eq!(view.exit_code, Some(0));
        assert_eq!(view.exit_reason.as_deref(), Some("user_stop"));
        assert_eq!(view.tree_empty, Some(true));
        assert!(view.forced_kill);
    }

    /// 结构错误快照仍然没有任务、也没有运行时。
    #[test]
    fn load_error_snapshot_has_no_tasks_or_runtime() {
        let error = LoadError::Io {
            path: PathBuf::from("C:/missing/Rook.toml"),
            source: std::io::Error::new(std::io::ErrorKind::NotFound, "文件不存在"),
        };
        let snapshot = Snapshot::from_load_error(&error, None);
        assert!(snapshot.tasks.is_empty());
        assert!(snapshot.config_error.is_some());
        let value = serde_json::to_value(&snapshot).expect("序列化错误快照");
        assert_eq!(value["tasks"].as_array().map(Vec::len), Some(0));
        assert!(value["configError"].is_object());
    }
}

/// A3：`displayStatus` / `allowedActions` 的纯函数测试。
///
/// 这两组规则此前分别散落在 `tray.rs` 与 `task-utils.ts`，这里固定语义，
/// 并保证与 `runtime::coordinator` 的手动启停判定一致。
#[cfg(test)]
mod action_rules_tests {
    use super::*;

    #[test]
    fn display_invalid_wins_over_runtime() {
        // 配置无效：即使运行时有状态（极端情况）也显示"无效"。
        assert_eq!(
            compute_display_status(false, true, Some("running"), None, false, false),
            "invalid"
        );
        assert_eq!(
            compute_display_status(false, false, None, None, false, false),
            "invalid"
        );
    }

    #[test]
    fn display_starting_when_runtime_running_but_supervisor_starting() {
        assert_eq!(
            compute_display_status(true, true, Some("running"), Some("starting"), false, false),
            "starting"
        );
        assert_eq!(
            compute_display_status(true, true, Some("starting"), None, false, false),
            "starting"
        );
    }

    #[test]
    fn display_passes_through_running_and_stopping() {
        assert_eq!(
            compute_display_status(true, true, Some("running"), Some("healthy"), false, false),
            "running"
        );
        assert_eq!(
            compute_display_status(true, true, Some("stopping"), None, false, false),
            "stopping"
        );
    }

    #[test]
    fn display_needs_human_beats_paused_and_backoff() {
        assert_eq!(
            compute_display_status(true, true, Some("stopped"), Some("backoff"), true, true),
            "needs_human"
        );
    }

    #[test]
    fn display_paused_beats_backoff() {
        assert_eq!(
            compute_display_status(true, true, Some("stopped"), Some("backoff"), false, true),
            "paused"
        );
        assert_eq!(
            compute_display_status(true, false, Some("stopped"), None, false, true),
            "paused"
        );
    }

    #[test]
    fn display_backoff_before_disabled() {
        assert_eq!(
            compute_display_status(true, false, Some("stopped"), Some("backoff"), false, false),
            "backoff"
        );
    }

    #[test]
    fn display_disabled_and_unknown_and_stopped() {
        assert_eq!(
            compute_display_status(true, false, Some("stopped"), None, false, false),
            "disabled"
        );
        // 没有运行时（配置结构错误前 / 尚未创建实例）：未知而不是无效。
        assert_eq!(
            compute_display_status(true, true, None, None, false, false),
            "unknown"
        );
        assert_eq!(
            compute_display_status(true, true, Some("stopped"), Some("stopped"), false, false),
            "stopped"
        );
        // 未识别的运行时状态按已停止处理，不伪造展示状态。
        assert_eq!(
            compute_display_status(true, true, Some("weird"), None, false, false),
            "stopped"
        );
    }

    #[test]
    fn allowed_actions_follow_runtime_state() {
        let stopped = compute_allowed_actions(true, true, Some("stopped"));
        assert_eq!(
            stopped,
            AllowedActions {
                start: true,
                stop: false,
                restart: true
            }
        );

        let running = compute_allowed_actions(true, true, Some("running"));
        assert_eq!(
            running,
            AllowedActions {
                start: false,
                stop: true,
                restart: true
            }
        );

        // starting 与 stopping：只有 stopping 允许 stop（收敛实例）。
        let starting = compute_allowed_actions(true, true, Some("starting"));
        assert_eq!(
            starting,
            AllowedActions {
                start: false,
                stop: false,
                restart: false
            }
        );
        let stopping = compute_allowed_actions(true, true, Some("stopping"));
        assert_eq!(
            stopping,
            AllowedActions {
                start: false,
                stop: true,
                restart: false
            }
        );
    }

    #[test]
    fn allowed_actions_respect_enabled_and_valid() {
        let disabled = compute_allowed_actions(true, false, Some("stopped"));
        assert!(!disabled.start);
        assert!(!disabled.restart);
        assert!(!disabled.stop);

        // 禁用任务的运行中实例仍必须能停止。
        let disabled_running = compute_allowed_actions(true, false, Some("running"));
        assert!(disabled_running.stop);
        assert!(!disabled_running.start);
        assert!(!disabled_running.restart);

        let invalid = compute_allowed_actions(false, true, Some("running"));
        assert_eq!(invalid, AllowedActions::default());
    }

    #[test]
    fn allowed_actions_ignore_paused_and_needs_human() {
        // paused / needs_human 只影响展示：用户显式 Start 会清除它们。
        assert!(compute_allowed_actions(true, true, Some("stopped")).start);
    }

    #[test]
    fn allowed_actions_without_runtime_are_all_false() {
        let actions = compute_allowed_actions(true, true, None);
        assert_eq!(actions, AllowedActions::default());
    }
}
