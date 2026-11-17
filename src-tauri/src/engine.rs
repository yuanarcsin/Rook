//! 运行时引擎（M1-4/M1-5 配置快照 → M2-4b 任务运行时接线）。
//!
//! 启动时解析 `--config` / `--data-dir` / `--profile` 并加载配置。结构错误不会终止应用：
//! 快照里记录 `configError`，托盘与窗口照常可用，用户修好文件后点"刷新"即可。
//!
//! 日志路径在 [`crate::logging`] 初始化时就已经确定（provisional 数据目录），
//! 配置里的 `[logs].core_level` / `retention_days` 只影响级别与清理，不改变文件位置。
//!
//! # 运行时生命周期（M2-4b / M3-4a）
//!
//! Engine 为每个**通过任务级校验**的任务持有 `Arc<TaskRuntime>`（与 `enabled` 无关：
//! `enabled=false` 的任务仍出现在快照里、可 Stop，但 Start/Restart 会被拒绝，
//! 托盘/界面同时禁用；见 docs/02 §4）。配置加载/重载时：
//!
//! - 新任务 → 创建 `stopped` 实例；
//! - 已有任务且实例处于 `stopped` → 用新配置替换（下一次 `start` 生效）；
//! - 已有任务处于 `starting` / `running` / `stopping` → 按 docs/02 §7.3 的
//!   reconcile 语义处理（M3-6a，见下节）；
//! - 任务从配置删除或校验失败 → `stopped` 实例直接丢弃；仍在运行/停止中的实例保留
//!   在表里，用户仍可用 `stop_task` 收敛，但不再出现在快照的任务列表中。
//!
//! 配置结构错误时不新建/替换任何实例，快照为空任务列表 + `configError`；已存在的
//! 运行中实例保持原样（本里程碑不做自动停止）。
//!
//! # 配置热重载 reconcile（M3-6a）
//!
//! `reload` 在替换 `self.loaded` 之前保存上一版任务配置（`previous_tasks`），
//! 再按字段分类（[crate::runtime::classify_config_change]）：
//!
//! - `name` / `notify` / `logs.encoding` / `logs.rotation`+`max_*` / `panel_url`：
//!   立即生效，无需动作；
//! - `health.*` / `restart.*` / `power.resume_grace_ms`：立即重建运行中实例的
//!   HealthMonitor 与 RestartMachine（失败计数与探测调度重置，
//!   needs_human/暂停保留）；
//! - `command` / `args` / `working_dir` / `env` / `elevated` / `ports` /
//!   `env_proxy` / `logs.dir` / `stop.*`：按 `[supervisor].apply_changes`
//!   —— `restart` 停止旧实例并用新配置启动（reason=config_changed、不计失败、
//!   清零失败计数），`next_start` 保持旧进程并标记 `restartPending`；
//! - 删除 / 失效 / `enabled=false`：沿用 B3/B13 的停止收敛。
//!
//! 用户停止/暂停/需要人工处理时绝不自动拉起：此时只标记 `restartPending`。
//! 标记在旧进程退出（下一次启动本来就会用当前配置）或任务被删除/禁用后清除。
//!
//! # 监督接线（M3-4b）
//!
//! - 每任务一个 [crate::restart::RestartMachine]（退避/熔断/needs_human）与
//!   当前实例的 [crate::health::HealthMonitor]；状态从 state.json 恢复
//!   （Q1/Q5/Q9/Q12），并回写到 state.json 的 supervisor 视图。
//! - 后台调度线程 250ms tick + 事件/命令唤醒；健康探测、进程启停都在 Engine
//!   锁外执行，回锁后按 instance_id/当前实例校验结果，迟到事件一律丢弃。
//! - 退出分类（Q3）：用户 Stop/Restart 的旧实例退出不计数；自然退出按
//!   clean=code==0 交给 RestartMachine；健康失败停止后按 HealthFailed，启动
//!   宽限到期按 StartFailed；spawn/端口/心跳目录等启动前失败同样 failure++。
//! - 自动启动按 Q5 过滤后错峰排期（start_stagger_ms）；恢复的 backoff 等到
//!   next_retry_at 到期，过期则至少 initial_delay；paused/needs_human 永不自动启动。
//! - 启动前创建心跳父目录并验证可写，失败按启动前检查失败处理（Q11）。
//!
//! # reload/start 竞态（M3-4a）
//!
//! 启停命令先在 Engine 锁内登记 in-flight 并记录配置代际，再释放锁执行阻塞操作。
//! 期间发生的 reload 会保留 in-flight 任务的旧 runtime（不替换/不丢弃）；命令完成
//! 后若代际变化，Engine 补一次 sync_runtimes。因此不会出现“旧 runtime 被命令
//! 启动后立刻被新表替换成孤儿进程”的情况。
//!
//! # 休眠/唤醒与时钟跳变（M3-6b）
//!
//! - Windows 侧用 `PowerRegisterSuspendResumeNotification` +
//!   `DEVICE_NOTIFY_CALLBACK`（无窗口、无消息循环）注册；回调只写
//!   [`crate::runtime::PowerSignalSink`] 的原子标志，注册失败只 warn 并退化为
//!   时钟跳变检测；
//! - 每轮 tick 先处理 suspend → resume → 时钟跳变，再决定是否推进计时器；
//!   suspend 期间整轮调度直接返回（不探测、不退避到期、不自动启动、不算启动超时）；
//! - resume 时按单调时钟实际走过的时长平移 `started_instant` /
//!   `next_health_at` / `auto_start_at` / `stop_retry_at` / 启动宽限与退避定时器，
//!   并按任务 `power.resume_grace_ms` 重新给健康宽限（睡眠不计失败）；
//! - 时钟跳变（墙钟与单调增量偏差 > 5s）只重置心跳新鲜度基线并给宽限，不重启；
//! - 冻结/跳变后递增基线代际，跨事件的迟到探测结果一律丢弃。
//!
//! # 线程模型
//!
//! ```text
//! start_task/stop_task/restart_task ──spawn_blocking──► Engine 锁（取 Arc）──► 运行时
//! 运行时事件 ──mpsc──► 转发线程（Weak<Engine>）──► 状态机/快照 ──► emit snapshot:changed
//! 调度线程 ──250ms tick/唤醒──► 锁内取计划 ──► 锁外探测/启停 ──► 回锁应用
//! ```
//!
//! 命令不在 UI 线程上执行阻塞操作：先取到 `Arc<TaskRuntime>` 再释放 Engine 锁，然后用
//! `tauri::async_runtime::spawn_blocking` 跑 `start/stop/restart`。转发线程只持有
//! `Weak<Mutex<Engine>>`，应用退出后升级失败即自行结束，不会在 AppHandle 失效后继续发事件。

// 测试构建只编译 core（宿主层 cfg(not(test))），部分入口在测试里没有调用方。
#![cfg_attr(test, allow(dead_code))]

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(test)]
use std::sync::mpsc;
use std::sync::mpsc::Sender;
use std::time::{Duration, Instant, SystemTime};

use crate::cli::CliArgs;
use crate::config::{
    self, ApplyChanges, ConfigWatchHandle, ConfigWatchState, HealthKind, LoadedConfig, Overrides,
    PowerConfig, TaskConfig, WatchSpec,
};
use crate::data_dir;
use crate::health::{HealthMonitor, HealthState, HealthVerdict};
use crate::ipc_error::IpcError;
use crate::logging::Logging;
use crate::logs;
use crate::notifications::{
    NotificationCenter, NotificationKind, RaisedNotification, TaskNotice, now_millis,
};
use crate::process::limits::{self, LimitTracker, ProcessSample};
use crate::restart::{
    Decision, Reason, RestartEvent, RestartMachine, RestartPolicy, RestartSeed, RestartSnapshot,
    RestartStatus, SystemRandom,
};
use crate::runtime::{
    AutoStartInput, CLOCK_JUMP_THRESHOLD, ClockJump, ConfigReconcile, InstanceContext, PowerStamp,
    PowerTracker, PowerWiring, ReadinessPlan, ReconcileDecision, ReloadGate, RuntimeError,
    RuntimeEvent, RuntimeSnapshot, StartReason, TaskRuntime, TaskSpec, TaskStatus, UserAction,
    alive_status_for_rebuild, apply_runtime_snapshot, apply_user_intent, build_health_monitor,
    classify_config_change, clock_skew_between, count_restart_success, decide_reconcile,
    health_interval, instance_matches, machine_tick_due, manual_restart_allowed,
    manual_start_allowed, plan_auto_start, probe_due, readiness_deadline_reached,
    readiness_first_healthy, readiness_plan, reconcile_action, replay_alive_machine, restore_seed,
    resume_deadline, running_instance_accepted, shift_timer, should_log_stop_retry,
    starting_instance_accepted, stop_retry_delay, sync_state, update_baseline,
};
use crate::schedule::window::{
    PersistedWindowChange, WindowAction, WindowRuntime, WindowTickInput, may_supervisor_autostart,
    plan_window_tick,
};
use crate::schedule::{Schedule, now_local, window_end_rfc3339};
use crate::shutdown::{ExitReason, ShutdownCoordinator, ShutdownOutcome};
use crate::snapshot::{RuntimeView, Snapshot, SupervisorView};
use crate::state_store::{
    DesiredOverride, LoadStatus, PersistedState, StateStore, TaskState, Timestamp,
};

/// 后台调度线程的 tick 间隔（要求 200–500ms；事件/命令会额外唤醒）。
pub(crate) const SCHEDULER_TICK: Duration = Duration::from_millis(250);
/// 运行中日志清理的最长间隔：启动时清理一次，之后至少每 24 小时一次。
const LOG_CLEANUP_INTERVAL: Duration = Duration::from_secs(24 * 60 * 60);
/// C6：资源守卫的采样周期（全局最多每 5s 采样一次所有受限任务）。
const LIMIT_SAMPLE_INTERVAL: Duration = Duration::from_secs(5);

/// 用户/托盘触发的启停动作（core 与宿主层共用）。
#[derive(Debug, Clone, Copy)]
pub(crate) enum Action {
    Start,
    Stop,
    Restart,
}

impl Action {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }

    pub(crate) fn parse(action: &str) -> Option<Self> {
        match action {
            "start" => Some(Self::Start),
            "stop" => Some(Self::Stop),
            "restart" => Some(Self::Restart),
            _ => None,
        }
    }
}

impl From<Action> for UserAction {
    fn from(action: Action) -> Self {
        match action {
            Action::Start => Self::Start,
            Action::Stop => Self::Stop,
            Action::Restart => Self::Restart,
        }
    }
}

/// 本次实例停止的来源：决定退出事件按"用户停止/健康失败/自然退出"哪条迁移处理。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum StopCause {
    /// 用户 Stop（旧实例退出不参与重启计数）。
    User,
    /// 用户 Restart：旧实例退出后立即用新配置启动新实例（M3-6a）。
    UserRestart,
    /// 健康失败触发的停止（退出后按 HealthFailed/StartFailed 处理）。
    Health,
    /// 配置 enabled=true→false：停止但保留配置条目/state（B13）。
    ConfigDisabled,
    /// 配置删除/失效：停止后删除 runtime/supervisor/state/心跳（B3）。
    ConfigRemoved,
    /// 配置进程字段变化（apply_changes=restart）：停止后按新配置重新启动（M3-6a）。
    ConfigRestart,
    /// 每周运行时段结束触发的停止（不写用户 override、不计失败）。
    Schedule,
    /// 应用退出（托盘/窗口/会话结束）触发的停止：不计失败、不重启。
    Shutdown,
}

impl StopCause {
    /// 事件日志中的稳定原因值。
    fn as_str(self) -> &'static str {
        match self {
            Self::User => "user_stop",
            Self::UserRestart => "user_restart",
            Self::Health => "health",
            Self::ConfigDisabled => "config_disabled",
            Self::ConfigRemoved => "config_removed",
            Self::ConfigRestart => "config_restart",
            Self::Schedule => "schedule_stop",
            Self::Shutdown => "shutdown",
        }
    }
}

// 2026-11-05：单任务的运行时段观测状态移到 `crate::schedule::window::WindowRuntime`
// （纯逻辑、可单测）；`schedule_started` gating 已删除——离开窗口无条件停止任何
// 存活实例，不再区分谁启动。窗口 key 仍写入 state.json 的 `schedule_window`，
// 但只用于诊断/跨重启对照，不参与监督决策。

/// 单任务的监督状态（M3-4b）。
pub(crate) struct TaskSupervisor {
    /// 重启/退避/熔断决策机。
    machine: RestartMachine,
    /// 当前实例的健康监视器；无进程时为 None。
    monitor: Option<HealthMonitor>,
    /// 当前实例 id（用于迟到事件过滤）。
    instance_id: Option<String>,
    /// 最近结束/被替换的实例 id（N1：清空 `instance_id` 前保存）。
    ///
    /// 旧 Starting 事件积压时 `instance_id` 可能已是 `None`，仅靠
    /// `current != event` 无法区分旧/新实例；previous 用于拒绝这类旧事件。
    previous_instance_id: Option<String>,
    /// 实例进程创建时刻（startup_grace 基线）。
    started_instant: Option<Instant>,
    /// 下一次健康探测的单调时刻。
    next_health_at: Option<Instant>,
    /// 健康探测间隔。
    health_interval: Duration,
    /// 自动启动排期（错峰/退避恢复）。
    auto_start_at: Option<Instant>,
    /// 状态机已要求立即启动，等待调度线程执行。
    pub(crate) pending_start: bool,
    /// 当前停止动作的来源（事件到达前设置）。
    stop_cause: Option<StopCause>,
    /// 本次停止是否由启动宽限到期触发（退出后按 StartFailed 处理，Q3）。
    health_startup_failure: bool,
    /// 已决定停止实例，等待取得 in-flight 门/重试到期（B2/B3）。
    pending_stop: Option<StopCause>,
    /// 停止失败后的下次重试时刻（B2）。
    stop_retry_at: Option<Instant>,
    /// 停止失败重试次数（日志限流/升级观测）。
    stop_retry_count: u32,
    /// 配置删除/禁用收敛请求；停止完成后执行清理（B3/B13）。
    cleanup_on_stop: Option<ConfigReconcile>,
    /// 删除任务时需要清理的心跳文件（启动实例上下文提供）。
    cleanup_heartbeat: Option<PathBuf>,
    /// 已发出启动、等待对应 Starting 事件（N1）。
    expect_start: bool,
    /// 启动就绪策略；None = 尚未收到 Running 或已就绪。
    readiness: Option<ReadinessPlan>,
    /// 本次自动重启成功启动后才计入 total_restarts（N6）。
    restart_count_pending: bool,
    /// C6：连续超限计数（连续 3 次才停止）。
    limit_tracker: LimitTracker,
    /// C6：上一次采样的基线；用于算 CPU 增量与实例切换检测。
    last_limit_sample: Option<ProcessSample>,
}

impl TaskSupervisor {
    fn new(machine: RestartMachine, interval: Duration) -> Self {
        Self {
            machine,
            monitor: None,
            instance_id: None,
            previous_instance_id: None,
            started_instant: None,
            next_health_at: None,
            health_interval: interval,
            auto_start_at: None,
            pending_start: false,
            stop_cause: None,
            health_startup_failure: false,
            pending_stop: None,
            stop_retry_at: None,
            stop_retry_count: 0,
            cleanup_on_stop: None,
            cleanup_heartbeat: None,
            expect_start: false,
            readiness: None,
            restart_count_pending: false,
            limit_tracker: LimitTracker::default(),
            last_limit_sample: None,
        }
    }

    /// 清空当前实例 id，并把旧值记录为 previous（N1）。
    ///
    /// 所有"实例已结束/被替换"的路径都必须走这里，保证迟到的旧 Starting
    /// 能被 previous 拒绝，而不是在 `instance_id == None` 时被误接受。
    fn clear_instance(&mut self) {
        if let Some(previous) = self.instance_id.take() {
            self.previous_instance_id = Some(previous);
        }
    }
}

/// 调度线程一轮工作：探针/启动/停止任务均已在锁内登记 in-flight。
#[derive(Default)]
pub(crate) struct SchedulerWork {
    pub probes: Vec<ProbeJob>,
    pub starts: Vec<StartJob>,
    pub stops: Vec<StopJob>,
    pub changed: bool,
}

/// 一次启动失败的分类（供 state.json last_reason / 命令错误共用）。
pub(crate) struct StartFailure {
    pub stage: &'static str,
    pub message: String,
}

/// 取出探测时的电源/时钟基线；冻结、唤醒或跳变后旧结果一律丢弃。
#[derive(Debug, Clone, Copy)]
pub(crate) struct ProbeCapture {
    stamp: PowerStamp,
    started_mono: Instant,
    started_wall: SystemTime,
}

/// 调度线程从 Engine 取出的健康探测任务（锁外执行，回锁按 instance_id 校验）。
pub(crate) struct ProbeJob {
    pub task_id: String,
    pub instance_id: String,
    pub monitor: HealthMonitor,
    pub capture: ProbeCapture,
}

/// 调度线程执行的启动任务（自动启动/退避重启）。
pub(crate) struct StartJob {
    pub task_id: String,
    pub runtime: Arc<TaskRuntime>,
    pub context: InstanceContext,
    pub generation: u64,
}

/// 调度线程执行的停止任务（健康失败/资源超限触发）。
pub(crate) struct StopJob {
    pub task_id: String,
    pub runtime: Arc<TaskRuntime>,
    pub timeout: Duration,
    pub generation: u64,
    /// 停止来源（`"health"` / `"resource_limit"`），只用于日志诊断。
    pub reason: &'static str,
}

/// 退出流程要停止的一个存活实例。
pub(crate) struct ShutdownTask {
    pub id: String,
    pub runtime: Arc<TaskRuntime>,
    /// 该任务自己的 `[tasks.stop].timeout_ms`。
    pub timeout: Duration,
}

/// 退出流程的停止计划：在 Engine 锁内生成，阻塞停止在锁外执行。
pub(crate) struct ShutdownPlan {
    /// 退出预算截止时刻；执行时按“剩余预算与任务超时取较小值”停止。
    pub deadline: Instant,
    pub tasks: Vec<ShutdownTask>,
}

/// 一次手动命令的执行计划（在 Engine 锁内准备，锁外执行）。
pub(crate) struct CommandPlan {
    /// 要启动/停止的运行时（Restart 时是已按当前配置新建的实例）。
    pub runtime: Arc<TaskRuntime>,
    /// Restart 运行中任务时需要先停止的旧实例；停止成功后才会换表。
    pub previous: Option<Arc<TaskRuntime>>,
    pub timeout: Duration,
    /// 命令开始时看到的配置代际（判断期间是否发生 reload）。
    pub generation: u64,
    /// 启动上下文；Stop 时为 `None`。
    pub context: Option<InstanceContext>,
}

/// 引擎状态：最近一次加载结果 + 任务运行时 + 供界面使用的快照。
pub(crate) struct Engine {
    pub(crate) args: CliArgs,
    /// 启动时的工作目录：dev profile 的 `.dev-data` 相对它解析。
    pub(crate) cwd: PathBuf,
    pub(crate) logging: Logging,
    /// 成功加载的配置；结构错误时为 `None`。
    pub(crate) loaded: Option<LoadedConfig>,
    /// 最近一次生成的快照（含运行时视图）。
    pub(crate) snapshot: Snapshot,
    /// 每个有效任务一个运行时；`Arc` 便于命令与转发线程共享。
    pub(crate) runtimes: HashMap<String, Arc<TaskRuntime>>,
    /// 运行时事件发送端；每次新建实例都注册一份克隆。
    pub(crate) events: Sender<RuntimeEvent>,
    /// state.json 的内存副本（启动/重载时加载，命令/事件后按需落盘）。
    pub(crate) state: PersistedState,
    /// 状态文件句柄；配置结构错误时为 `None`，不读写任何状态文件。
    pub(crate) state_store: Option<StateStore>,
    /// reload/start 竞态门：配置代际 + 每任务 in-flight 标记。
    pub(crate) gate: ReloadGate,
    /// 每任务监督状态（健康/重启/退避/自动启动排期）。
    pub(crate) supervisors: HashMap<String, TaskSupervisor>,
    /// 每任务运行时段观测状态（2026-11-05 纯逻辑模块）。
    pub(crate) schedule_states: HashMap<String, WindowRuntime>,
    /// 2026-11-05：本进程内用户显式手动 Start/Restart 过的 schedule 任务。
    ///
    /// 窗口外仍允许监督器自动重启这些任务（“窗口外手动启动不用管”）；手动 Stop
    /// 清除。只存内存：state.json 的 `desired_override` 只有 stopped/paused，
    /// 没有 started；应用重启后不存在存活实例，窗口外也不会自动开工。
    pub(crate) manual_started_schedule: HashSet<String>,
    /// 上一次成功加载的任务配置（按 id）：重载时与新版逐字段比较（M3-6a）。
    pub(crate) previous_tasks: HashMap<String, TaskConfig>,
    /// `apply_changes=next_start` 下已改配置、但仍在用旧配置运行的任务。
    ///
    /// 只在内存中保存：进程重启后旧实例已不存在，标记自然失效。
    pub(crate) restart_pending: HashSet<String>,
    /// 插件导入后的首次 reload 抑制自动启动：导入动作本身不启动任务，
    /// 清单里的 auto_start 只影响下一次应用启动（docs/15 §6.5）。
    pub(crate) suppress_auto_start_once: HashSet<String>,
    /// C8：清单版本高于任务里已存版本的插件任务 id（配置加载时算一次）。
    pub(crate) plugin_updates: HashSet<String>,
    /// 通知节流/去重（M3-5b）：只在需要人介入的迁移上生成，锁外投递。
    pub(crate) notifications: NotificationCenter,
    /// 唤醒后台调度线程（tick 之外的即时唤醒）。
    pub(crate) wake: Sender<()>,
    /// 休眠/唤醒/时钟跳变跟踪（M3-6b，全局一份）。
    pub(crate) power: PowerTracker,
    /// Windows 休眠/唤醒通知接线；注册失败时只退化为时钟跳变检测。
    pub(crate) power_wiring: PowerWiring,
    /// 电源/时钟基线代际：冻结或跳变后递增，用于丢弃跨事件的迟到探测结果。
    pub(crate) power_epoch: u64,
    /// 上次日志清理时刻（启动时清理一次，之后至少每天一次）。
    pub(crate) last_log_cleanup: Option<Instant>,
    /// C6：上次资源采样时刻（全局节流，最多每 `LIMIT_SAMPLE_INTERVAL` 一次）。
    pub(crate) last_limit_check: Option<Instant>,
    /// 退出流程是否已开始；调度线程在此期间不再自动拉起任务。
    pub(crate) shutting_down: bool,
    /// 退出流程幂等门（托盘/窗口/会话结束共用）。
    pub(crate) shutdown: Arc<ShutdownCoordinator>,
    /// 启动时 state.json 的加载结果（用于识别“上次未干净退出”）。
    pub(crate) state_load_status: Option<LoadStatus>,
    /// 配置热重载监听状态（只读投影进快照）。
    pub(crate) watch_state: Arc<ConfigWatchState>,
    /// 配置监听线程句柄；None = 未启动/已停止。
    pub(crate) watch_handle: Option<ConfigWatchHandle>,
}

impl Engine {
    pub(crate) fn new(
        args: CliArgs,
        cwd: PathBuf,
        logging: Logging,
        events: Sender<RuntimeEvent>,
        wake: Sender<()>,
        power_wiring: PowerWiring,
        watch_state: Arc<ConfigWatchState>,
    ) -> Self {
        let (loaded, snapshot) = load_config(&args, &cwd, &logging);
        // 监听开关只在启动时读取；修改 watch_config 后需重启应用生效。
        if let Some(loaded) = &loaded {
            watch_state.configure(
                loaded.config.supervisor.watch_config,
                watch_poll_interval(loaded),
            );
            watch_state.set_applied_revision(loaded.revision.clone());
        }
        let (state_store, state, state_load_status) = load_persisted_state(loaded.as_ref());
        // M5-1：按配置对齐 HKCU Run 开机自启。只在启动时执行一次；
        // dev profile 跳过（避免开发实例覆盖生产 Run 键），失败只 warn。
        if let Some(loaded) = &loaded {
            crate::autostart::reconcile_for_startup(&loaded.config.autostart, args.is_dev());
        }
        if let Some(error) = power_wiring.error() {
            tracing::warn!(
                error = %error,
                "注册休眠/唤醒通知失败，退化为时钟跳变检测（不影响监督）"
            );
        } else {
            tracing::info!("已注册休眠/唤醒通知（DEVICE_NOTIFY_CALLBACK，无窗口）");
        }
        let mut engine = Self {
            args,
            cwd,
            logging,
            loaded,
            snapshot,
            runtimes: HashMap::new(),
            events,
            state,
            state_store,
            gate: ReloadGate::new(),
            supervisors: HashMap::new(),
            schedule_states: HashMap::new(),
            manual_started_schedule: HashSet::new(),
            previous_tasks: HashMap::new(),
            restart_pending: HashSet::new(),
            suppress_auto_start_once: HashSet::new(),
            plugin_updates: HashSet::new(),
            notifications: NotificationCenter::default(),
            wake,
            power: PowerTracker::default(),
            power_wiring,
            power_epoch: 0,
            last_log_cleanup: None,
            last_limit_check: None,
            shutting_down: false,
            shutdown: Arc::new(ShutdownCoordinator::new()),
            state_load_status,
            watch_state,
            watch_handle: None,
        };
        // 启动时就清理已从配置删除任务的状态；只有确实删除时才落盘，
        // 避免把损坏/未知版本恢复出的默认状态立刻覆盖原文件。
        if engine.prune_state_tasks() {
            engine.persist();
        }
        engine.sync_runtimes();
        let now = Instant::now();
        let wall_now = Timestamp::now();
        // 启动时清理一次（核心/事件归档 + 所有任务日志归档）；失败只记录。
        engine.cleanup_expired_logs(now);
        engine.sync_supervisors(now, wall_now);
        engine.schedule_auto_start(now, wall_now, None);
        engine.previous_tasks = engine.current_task_configs();
        // C8：启动时也算一次，避免"清单升级后重启应用仍看不到徽标"。
        engine.refresh_plugin_updates();
        engine.refresh();
        engine.log_restored_state();
        // B1：识别上次未干净退出，并把本次会话标记为“未完成”，正常退出时再翻成 true。
        engine.begin_app_session();
        engine.wake();
        engine
    }

    /// 测试夹具：用给定配置装配一个最小可用的 Engine。
    ///
    /// 与 [`Engine::new`] 的差别只在“副作用”：不读磁盘配置、不写 state.json
    /// （`state_store=None`）、不碰注册表/电源通知（[`PowerWiring::for_test`]）。
    /// 运行时段、监督器、快照仍走与生产相同的 `sync_*` / `refresh` 路径。
    #[cfg(test)]
    pub(crate) fn for_test(loaded: LoadedConfig) -> Self {
        let snapshot = Snapshot::from_loaded(&loaded);
        let (events, _events_rx) = mpsc::channel();
        let (wake, _wake_rx) = mpsc::channel();
        let mut engine = Self {
            args: crate::cli::parse_args(std::iter::empty::<String>()),
            cwd: PathBuf::from("."),
            logging: crate::logging::Logging::for_test(),
            loaded: Some(loaded),
            snapshot,
            runtimes: HashMap::new(),
            events,
            state: PersistedState::default(),
            state_store: None,
            gate: ReloadGate::new(),
            supervisors: HashMap::new(),
            schedule_states: HashMap::new(),
            manual_started_schedule: HashSet::new(),
            previous_tasks: HashMap::new(),
            restart_pending: HashSet::new(),
            suppress_auto_start_once: HashSet::new(),
            plugin_updates: HashSet::new(),
            notifications: NotificationCenter::default(),
            wake,
            power: PowerTracker::default(),
            power_wiring: PowerWiring::for_test(),
            power_epoch: 0,
            last_log_cleanup: None,
            last_limit_check: None,
            shutting_down: false,
            shutdown: Arc::new(ShutdownCoordinator::new()),
            state_load_status: None,
            watch_state: Arc::new(ConfigWatchState::default()),
            watch_handle: None,
        };
        engine.sync_runtimes();
        let now = Instant::now();
        engine.sync_supervisors(now, Timestamp::now());
        engine.refresh_plugin_updates();
        engine.refresh();
        engine.previous_tasks = engine.current_task_configs();
        engine
    }

    /// 重新加载配置并更新状态；返回新快照（失败时是错误态快照）。
    pub(crate) fn reload(&mut self) -> Snapshot {
        let (loaded, snapshot) = load_config(&self.args, &self.cwd, &self.logging);
        self.apply_load_result(loaded, snapshot)
    }

    /// 把一次加载结果应用到引擎；成功/失败都走现有 reconcile 语义。
    ///
    /// 配置监听在**校验成功**后调用这里（失败走 [`Self::record_watch_error`]），
    /// 因此非法 TOML 不会把 `loaded` 清空、不会停任务、不会触发重启。
    fn apply_load_result(
        &mut self,
        loaded: Option<LoadedConfig>,
        mut snapshot: Snapshot,
    ) -> Snapshot {
        // M3-6a：替换 self.loaded 前保存上一版任务配置，供 docs/02 §7.3 的
        // 字段级比较使用；加载失败时保留它，等用户修好文件后仍能比较。
        let previous = std::mem::take(&mut self.previous_tasks);
        let loaded_ok = loaded.is_some();
        if !loaded_ok {
            // 配置不可用：快照不能携带旧 revision，否则前端会拿它做冲突检测
            // 并得到误导性的 `conflict`（docs/01 §6.2 的 configRevision）。
            snapshot.config_revision.clear();
        }
        if let Some(loaded) = &loaded {
            // 自身保存抑制：记录本次真正落盘内容的 revision；监听器随后收到
            // 同内容事件时直接跳过，不重复 reload/日志/reconcile。
            self.watch_state
                .set_applied_revision(loaded.revision.clone());
            self.watch_state.record_reload(config::watch::now_millis());
            // 运行中关闭 watch_config 时只请求停止、不 join（当前可能持有 Engine 锁）；
            // 重新开启需重启应用（启动时才会创建监听线程）。
            if let Some(handle) = &self.watch_handle
                && !handle.is_stopped()
            {
                let enabled = loaded.config.supervisor.watch_config;
                self.watch_state
                    .configure(enabled, watch_poll_interval(loaded));
                if !enabled {
                    handle.request_stop();
                }
            }
        }
        self.loaded = loaded;
        self.snapshot = snapshot;
        // C8：配置重载时算一次插件版本提示（不在每 tick 读清单文件）。
        self.refresh_plugin_updates();
        // M3-5b：配置结构错误只在"进入错误"时通知一次（同一错误反复重载不重复）。
        // 启动时的初始加载不通知：用户刚启动应用，托盘变红已足够。
        self.note_config_error();
        // 代际在成功/失败都递增：in-flight 命令完成后会补一次 sync。
        self.gate.reload();
        self.sync_state_store();
        self.prune_state_tasks();
        let now = Instant::now();
        let wall_now = Timestamp::now();
        // B3/B13/M3-6a：先对删除/失效/禁用任务发出停止收敛、对进程字段变化
        // 的任务发出重启（必须在 sync_runtimes 丢弃已停止 runtime 之前，才能
        // 清理心跳文件）。
        self.reconcile_config_changes(now, wall_now, &previous);
        self.sync_runtimes();
        let added = self.sync_supervisors(now, wall_now);
        // 新任务若满足 auto_start 条件，按错峰排期；已有 stopped 任务不因
        // 一次 reload 被重新拉起（用户 Stop 的 override 另有限制）。
        self.schedule_auto_start(now, wall_now, Some(&added));
        if loaded_ok {
            // 命令执行中的任务保留旧基线：in-flight 期间 reconcile 跳过了它，
            // 命令结束后 resync_after_reload 仍要能发现本次重载的变化。
            self.previous_tasks = update_baseline(
                &previous,
                self.current_task_configs(),
                self.gate.in_flight(),
            );
        } else {
            self.previous_tasks = previous;
        }
        self.refresh();
        self.persist();
        self.wake();
        self.snapshot.clone()
    }

    /// 应用一次“监听器校验成功”的配置。
    ///
    /// 复用 [`Self::apply_load_result`] 的完整 reconcile 语义；额外记录
    /// `config.watch_reload`，成功重载仍由 `log_loaded_config` 写 `config.reload`。
    pub(crate) fn apply_watched_loaded(&mut self, loaded: LoadedConfig) -> Snapshot {
        log_loaded_config(&loaded, &self.logging);
        tracing::info!(
            event = "config.watch_reload",
            revision = %loaded.revision,
            config = %loaded.config_path.display(),
            "检测到配置文件变化，已自动重新加载"
        );
        let snapshot = Snapshot::from_loaded(&loaded);
        self.apply_load_result(Some(loaded), snapshot)
    }

    /// 监听器读到非法 TOML/校验错误：只更新监听状态，保留当前配置与任务。
    ///
    /// 返回 `(最新快照, 错误是否有变化)`；同一错误重复出现时不重复广播/日志。
    pub(crate) fn record_watch_error(&mut self, error: &config::LoadError) -> (Snapshot, bool) {
        let changed = self.watch_state.record_load_error(error);
        if changed {
            tracing::warn!(
                event = "config.watch_error",
                error = %error,
                "配置文件变化未通过校验，保留当前配置与运行中任务"
            );
        }
        self.refresh();
        (self.snapshot.clone(), changed)
    }

    /// 当前配置文件路径：只来自启动时的路径解析，前端无法传入任意路径。
    pub(crate) fn config_file_path(&self) -> PathBuf {
        self.loaded
            .as_ref()
            .map(|loaded| loaded.config_path.clone())
            .unwrap_or_else(|| PathBuf::from(&self.snapshot.config_path))
    }

    /// 当前监听目标（路径/数据目录覆盖/轮询周期）。
    pub(crate) fn watch_spec(&self) -> Option<WatchSpec> {
        let config_path = self.config_file_path();
        if config_path.as_os_str().is_empty() {
            return None;
        }
        Some(WatchSpec {
            config_path,
            data_dir_override: data_dir::data_dir_override(&self.args, &self.cwd),
            poll_interval: self.watch_state.poll_interval(),
        })
    }

    /// 启动时清理一次；运行中由调度线程每 24 小时触发一次。
    fn cleanup_expired_logs(&mut self, now: Instant) {
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let retention_days = loaded.config.logs.retention_days;
        self.logging.cleanup_core_logs(retention_days);

        // 每个任务的保留期来自 [tasks.logs].max_days（默认 7 天）；
        // 配置里已删除但目录仍存在的任务用全局 retention_days 兜底。
        let mut dirs: Vec<(PathBuf, u32)> = loaded
            .config
            .tasks
            .iter()
            .filter(|task| loaded.is_task_valid(&task.id))
            .map(|task| (task.logs.dir.clone(), task.logs.max_days))
            .collect();
        let mut seen: HashSet<PathBuf> = dirs.iter().map(|(dir, _)| dir.clone()).collect();
        let root = loaded.data_dir.join("logs");
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                let path = entry.path();
                if !path.is_dir() || !seen.insert(path.clone()) {
                    continue;
                }
                dirs.push((path, retention_days));
            }
        }
        logs::cleanup_task_logs(&dirs);
        // 7 天保留之后执行 `[logs].max_total_mb` 全局总量上限（最旧归档优先）；
        // 失败只 warn，不阻塞启动/监督。
        let data_dir = loaded.data_dir.clone();
        if let Err(error) = logs::cleanup_total_size(&data_dir, loaded.config.logs.max_total_mb) {
            tracing::warn!(%error, "清理日志总量失败（不影响任务运行）");
        }
        // 诊断包只保留最近 5 份；crash-*.log 已在上面的核心日志清理中按保留期处理。
        if let Err(error) =
            crate::diagnostics::cleanup_old_exports(&data_dir, crate::diagnostics::KEEP_EXPORTS)
        {
            tracing::warn!(%error, "清理旧诊断包失败");
        }
        self.last_log_cleanup = Some(now);
    }

    /// 距上次清理满 24 小时才执行；配置尚未加载成功时跳过，下一轮重试。
    fn maybe_cleanup_expired_logs(&mut self, now: Instant) {
        if self.loaded.is_none() {
            return;
        }
        let due = self
            .last_log_cleanup
            .is_none_or(|last| now.duration_since(last) >= LOG_CLEANUP_INTERVAL);
        if due {
            self.cleanup_expired_logs(now);
        }
    }

    /// 当前配置里全部任务（按 id），作为下一次重载的比较基线。
    fn current_task_configs(&self) -> HashMap<String, TaskConfig> {
        self.loaded
            .as_ref()
            .map(|loaded| {
                loaded
                    .config
                    .tasks
                    .iter()
                    .map(|task| (task.id.clone(), task.clone()))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 让每任务监督状态与当前配置/state.json 对齐；返回本次新建的任务 id。
    ///
    /// - 已有且进程运行中：保留旧状态机/健康监视器（新策略下一实例生效）；
    /// - 已有且已停止：按新策略重建状态机，但保留暂停/退避/失败计数记忆；
    /// - 新任务：从 state.json 恢复种子（paused/needs_human/backoff）。
    fn sync_supervisors(&mut self, now: Instant, wall_now: Timestamp) -> Vec<String> {
        let mut added = Vec::new();
        let Some(loaded) = self.loaded.as_ref() else {
            return added;
        };

        let mut valid_ids = HashSet::new();
        for task in &loaded.config.tasks {
            if !loaded.is_task_valid(&task.id) {
                continue;
            }
            valid_ids.insert(task.id.clone());
            // 命令执行期间保留旧监督状态，命令结束后 finish_command 会补 sync。
            if self.gate.is_in_flight(&task.id) {
                continue;
            }
            let stopped = self
                .runtimes
                .get(&task.id)
                .is_none_or(|runtime| runtime.status() == TaskStatus::Stopped);
            if let Some(supervisor) = self.supervisors.get_mut(&task.id) {
                if stopped {
                    let seed = supervisor.machine.seed();
                    supervisor.machine = RestartMachine::restore(
                        RestartPolicy::from_config(&task.restart),
                        SystemRandom::new(),
                        seed,
                    );
                    supervisor.health_interval = health_interval(task);
                }
                continue;
            }

            let state = self.state.task(&task.id);
            let seed = restore_seed(task, state, now, wall_now);
            let machine = RestartMachine::restore(
                RestartPolicy::from_config(&task.restart),
                SystemRandom::new(),
                seed,
            );
            self.supervisors.insert(
                task.id.clone(),
                TaskSupervisor::new(machine, health_interval(task)),
            );
            added.push(task.id.clone());
        }

        // 删除/失效任务：运行中实例保留 supervisor 直到 reconcile 停止完成；
        // in-flight 命令结束后由 finish_command 补 sync（B3）。
        let gate = &self.gate;
        let runtimes = &self.runtimes;
        self.supervisors.retain(|id, _| {
            valid_ids.contains(id)
                || gate.is_in_flight(id)
                || runtimes
                    .get(id)
                    .is_some_and(|runtime| runtime.status() != TaskStatus::Stopped)
        });
        // 运行时段状态只保留当前配置里仍存在的任务。
        self.schedule_states.retain(|id, _| valid_ids.contains(id));
        self.manual_started_schedule
            .retain(|id| valid_ids.contains(id));
        added
    }

    /// 计算自动启动候选与错峰时间（Q5/Q12：paused/backoff 不立即启动）。
    ///
    /// only 为 None 表示启动时全量计算；reload 只对新任务排期。
    fn schedule_auto_start(&mut self, now: Instant, wall_now: Timestamp, only: Option<&[String]>) {
        if self.shutting_down {
            return;
        }
        let Some(loaded) = self.loaded.as_ref() else {
            return;
        };
        let stagger = Duration::from_millis(loaded.config.supervisor.start_stagger_ms);
        let mut inputs = Vec::new();
        let mut backoff_ids = Vec::new();
        for task in &loaded.config.tasks {
            if !loaded.is_task_valid(&task.id) {
                continue;
            }
            if let Some(only) = only
                && !only.iter().any(|id| id == &task.id)
            {
                continue;
            }
            // 插件导入动作本身不启动任务：首次 reload 抑制一次；auto_start
            // 只影响下一次应用启动（docs/15 §6.5）。
            if self.suppress_auto_start_once.remove(&task.id) {
                tracing::info!(
                    task = %task.id,
                    "插件导入：本次重载不自动启动，auto_start 下次应用启动生效"
                );
                continue;
            }
            // 2026-11-04：启用运行时段的调度任务不参与开机/重载的 auto_start
            // 排期；由 schedule_work 在窗口内启动，避免“开机即启动”绕过时段。
            if task.schedule.enabled {
                continue;
            }
            let Some(supervisor) = self.supervisors.get(&task.id) else {
                continue;
            };
            if self
                .runtimes
                .get(&task.id)
                .is_some_and(|runtime| runtime.status() != TaskStatus::Stopped)
            {
                continue;
            }
            match supervisor.machine.status() {
                RestartStatus::Stopped => inputs.push(AutoStartInput {
                    task,
                    state: self.state.task(&task.id),
                }),
                // backoff 由状态机 Tick + next_retry_at 负责（Q12），不参与错峰。
                RestartStatus::Backoff => backoff_ids.push(task.id.clone()),
                RestartStatus::Starting | RestartStatus::Running | RestartStatus::Paused => {}
            }
        }

        let plan = plan_auto_start(&inputs, now, stagger, wall_now);
        let candidates = plan.candidates.len();
        for candidate in &plan.candidates {
            if let Some(supervisor) = self.supervisors.get_mut(&candidate.task_id) {
                supervisor.auto_start_at = Some(candidate.due);
            }
        }
        for (task_id, reason) in &plan.skipped {
            tracing::debug!(task = %task_id, reason = %reason, "自动启动跳过");
        }
        for task_id in &backoff_ids {
            tracing::debug!(task = %task_id, "恢复的退避任务等待 next_retry_at 到期");
        }
        tracing::debug!(
            candidates,
            skipped = plan.skipped.len(),
            backoff = backoff_ids.len(),
            "自动启动计划已生成"
        );
    }

    /// 数据目录变化时切换到新的 state.json；否则保留内存中的最新状态。
    fn sync_state_store(&mut self) {
        let Some(loaded) = self.loaded.as_ref() else {
            // 结构错误：继续使用上一次的状态文件与原内存状态，等待修复。
            return;
        };
        let candidate = StateStore::in_dir(&loaded.data_dir);
        if self
            .state_store
            .as_ref()
            .is_some_and(|store| store.path() == candidate.path())
        {
            return;
        }
        let loaded_state = candidate.load();
        log_load_status(&candidate, &loaded_state.status);
        self.state_store = Some(candidate);
        self.state = loaded_state.state;
    }

    /// 配置里已删除的任务：清理 state.json 中对应的持久状态。
    ///
    /// 返回是否删除了条目（调用方据此决定是否立即落盘）。
    fn prune_state_tasks(&mut self) -> bool {
        let Some(loaded) = self.loaded.as_ref() else {
            return false;
        };
        let known: HashSet<String> = loaded
            .config
            .tasks
            .iter()
            .map(|task| task.id.clone())
            .collect();
        // B3：运行中实例的 state 暂不删除，等 reconcile 停止完成后由
        // finish_reconcile 清理；否则会丢失停止后的退出信息。
        let removable: Vec<String> = self
            .state
            .tasks
            .keys()
            .filter(|id| {
                !known.contains(id.as_str())
                    && self
                        .runtimes
                        .get(id.as_str())
                        .is_none_or(|runtime| runtime.status() == TaskStatus::Stopped)
            })
            .cloned()
            .collect();
        if removable.is_empty() {
            return false;
        }
        for id in &removable {
            self.state.remove_task(id);
        }
        tracing::info!(
            tasks = %removable.join(","),
            "任务已从配置移除，清理持久状态"
        );
        true
    }

    /// 启动时的恢复摘要（自动启动排期由 schedule_auto_start 单独记录）。
    fn log_restored_state(&self) {
        if self.state.tasks.is_empty() {
            return;
        }
        let paused = self
            .state
            .tasks
            .values()
            .filter(|state| state.paused_reason.is_some())
            .count();
        let needs_human = self
            .state
            .tasks
            .values()
            .filter(|state| state.needs_human.active)
            .count();
        let overrides = self
            .state
            .tasks
            .values()
            .filter(|state| state.desired_override.is_some())
            .count();
        tracing::info!(
            tasks = self.state.tasks.len(),
            paused,
            needs_human,
            overrides,
            "已恢复 state.json 监督状态"
        );
    }

    /// 启动时记录会话开始，并识别上次是否未干净退出（B1）。
    ///
    /// 只有“state.json 成功加载且上次写过 `started_at`”时才报告异常退出：
    /// 首次运行（文件缺失）与旧版 state（没有 app_session）都不算异常。
    fn begin_app_session(&mut self) {
        let previous = self.state.app_session.clone();
        let unclean = matches!(self.state_load_status, Some(LoadStatus::Loaded))
            && !previous.clean_exit
            && previous.started_at.is_some();
        if unclean {
            tracing::warn!(
                event = "app.unclean_exit",
                last_started_at = previous
                    .started_at
                    .map(|time| time.as_rfc3339())
                    .unwrap_or_else(|| "-".to_owned()),
                last_version = previous.last_version.as_deref().unwrap_or("-"),
                "上次应用未干净退出"
            );
        }
        self.state.app_session.started_at = Some(Timestamp::now());
        self.state.app_session.clean_exit = false;
        self.state.app_session.last_version = Some(env!("CARGO_PKG_VERSION").to_owned());
        self.persist();
    }

    /// 按需保存 state.json。失败只记日志：状态缓存不可用不应影响任务运行。
    pub(crate) fn persist(&self) {
        let Some(store) = self.state_store.as_ref() else {
            return;
        };
        if let Err(error) = store.save(&self.state) {
            tracing::warn!(
                path = %store.path().display(),
                error = %error,
                "保存 state.json 失败（不影响任务运行）"
            );
        }
    }

    /// 退出流程的幂等门（与托盘/会话结束共用）。
    pub(crate) fn shutdown_coordinator(&self) -> Arc<ShutdownCoordinator> {
        Arc::clone(&self.shutdown)
    }

    /// 是否已经进入退出流程。
    pub(crate) fn shutdown_started(&self) -> bool {
        self.shutting_down || self.shutdown.is_started()
    }

    /// 退出预算：`[supervisor].shutdown_timeout_ms`；配置不可用时用 8s。
    pub(crate) fn shutdown_budget(&self) -> Duration {
        self.loaded
            .as_ref()
            .map(|loaded| Duration::from_millis(loaded.config.supervisor.shutdown_timeout_ms))
            .unwrap_or(Duration::from_millis(8_000))
    }

    /// 托盘退出确认所需信息：存活实例数与 `[app].confirm_exit`。
    pub(crate) fn exit_confirmation(&self) -> (usize, bool) {
        let running = self
            .runtimes
            .values()
            .filter(|runtime| runtime.status() != TaskStatus::Stopped)
            .count();
        let confirm = self
            .loaded
            .as_ref()
            .map(|loaded| loaded.config.app.confirm_exit)
            .unwrap_or(true);
        (running, confirm)
    }

    /// 退出流程第一阶段（锁内）：置位 `shutting_down`、标记停止来源、保存 state，
    /// 返回需要在锁外阻塞停止的任务列表。
    ///
    /// 不在锁内调用 `runtime.stop`：停止最多等 `shutdown_timeout_ms`，不能阻塞
    /// 事件转发/快照读取。
    pub(crate) fn begin_shutdown(
        &mut self,
        reason: ExitReason,
        budget: Duration,
    ) -> Option<ShutdownPlan> {
        if self.shutting_down {
            return None;
        }
        self.shutting_down = true;
        let deadline = Instant::now() + budget;
        // 退出前保存：即便随后某个任务停止失败，state 也已是可恢复的最新意图。
        self.persist();

        let mut tasks = Vec::new();
        let ids: Vec<String> = self.runtimes.keys().cloned().collect();
        for id in ids {
            let Some(runtime) = self.runtimes.get(&id).cloned() else {
                continue;
            };
            if runtime.status() == TaskStatus::Stopped {
                continue;
            }
            let timeout = runtime.spec().watch.stop_timeout;
            if let Some(supervisor) = self.supervisors.get_mut(&id) {
                // 标记为退出停止：Stopped 事件到达时不进入重启/失败计数。
                supervisor.stop_cause = Some(StopCause::Shutdown);
                supervisor.pending_stop = None;
                supervisor.pending_start = false;
                supervisor.auto_start_at = None;
                supervisor.monitor = None;
                supervisor.next_health_at = None;
                supervisor.stop_retry_at = None;
                supervisor.stop_retry_count = 0;
            }
            tracing::info!(
                event = "task.stop",
                task = %id,
                intent = "app_exit",
                reason = reason.as_str(),
                method = runtime.spec().watch.method.as_str(),
                grace_ms = runtime.spec().watch.grace.as_millis() as u64,
                timeout_ms = timeout.as_millis() as u64,
                "应用退出：请求停止任务实例"
            );
            tasks.push(ShutdownTask {
                id,
                runtime,
                timeout,
            });
        }
        Some(ShutdownPlan { deadline, tasks })
    }

    /// 退出流程最后阶段（锁内）：写 `clean_exit` 标记并保存 state。
    pub(crate) fn finish_shutdown(&mut self, reason: ExitReason, _outcome: &ShutdownOutcome) {
        self.state.app_session.clean_exit = true;
        self.state.app_session.last_exit_reason = Some(reason.as_str().to_owned());
        self.state.app_session.last_exit_at = Some(Timestamp::now());
        self.persist();
    }

    /// 退出前刷盘日志（只在退出流程最后调用）。
    pub(crate) fn flush_logs(&self) {
        self.logging.flush();
    }

    /// 窗口关闭：若从未展示过“仍在托盘运行”提示，则标记为待展示（一次性）。
    pub(crate) fn note_window_closed(&mut self) {
        if self.state.ui.tray_hint_shown || self.state.ui.tray_hint_pending {
            return;
        }
        self.state.ui.tray_hint_pending = true;
        self.snapshot.tray_hint_pending = true;
        self.persist();
    }

    /// 前端展示并关闭一次性托盘提示。
    pub(crate) fn acknowledge_tray_hint(&mut self) {
        if !self.state.ui.tray_hint_pending && self.state.ui.tray_hint_shown {
            return;
        }
        self.state.ui.tray_hint_pending = false;
        self.state.ui.tray_hint_shown = true;
        self.snapshot.tray_hint_pending = false;
        self.persist();
    }

    /// 仅在任务仍存在于当前配置时返回它的持久状态（避免为已删除任务重建条目）。
    fn state_task_mut(&mut self, id: &str) -> Option<&mut TaskState> {
        let known = self
            .loaded
            .as_ref()
            .is_some_and(|loaded| loaded.config.task(id).is_some());
        if !known {
            return None;
        }
        Some(self.state.task_mut(id))
    }

    /// 让运行时表与当前配置对齐（见模块文档的替换规则）。
    fn sync_runtimes(&mut self) {
        let Some(loaded) = self.loaded.as_ref() else {
            // 结构错误：不新建/替换实例；运行中的实例保留，等待用户显式停止。
            tracing::warn!("配置结构错误，任务运行时保持不变");
            return;
        };

        let mut desired = HashSet::new();
        for task in &loaded.config.tasks {
            if !loaded.is_task_valid(&task.id) {
                continue;
            }
            desired.insert(task.id.clone());

            // reload/start 竞态门：命令执行期间不替换/不丢弃该任务的 runtime。
            // 命令结束后 Engine 会检测代际变化并补一次 sync。
            if self.gate.is_in_flight(&task.id) {
                tracing::debug!(
                    task = %task.id,
                    "任务命令执行中，本次重载保留旧运行时，命令结束后再同步"
                );
                continue;
            }

            let keep_old = self
                .runtimes
                .get(&task.id)
                .is_some_and(|runtime| runtime.status() != TaskStatus::Stopped);
            if keep_old {
                tracing::debug!(
                    task = %task.id,
                    "任务运行中，保留当前实例（配置变更由 reconcile 按 apply_changes 处理）"
                );
                continue;
            }

            let spec =
                TaskSpec::from_config_with_paths(task, &loaded.data_dir, &loaded.config_path);
            let runtime = Arc::new(TaskRuntime::with_events(spec, self.events.clone()));
            // 被替换的实例处于 stopped，Drop 立即返回，不会阻塞配置加载。
            if self.runtimes.insert(task.id.clone(), runtime).is_some() {
                tracing::debug!(task = %task.id, "已按新配置重建任务运行时");
            }
        }

        // 消失/失效的任务：stopped 的直接丢弃；运行中的保留，便于用户停止；
        // 命令执行中的也保留，等命令结束后再按新表收敛。
        let gate = &self.gate;
        self.runtimes.retain(|id, runtime| {
            if desired.contains(id) {
                return true;
            }
            if gate.is_in_flight(id) {
                tracing::debug!(task = %id, "任务已从配置移除但命令执行中，保留运行时直到命令结束");
                return true;
            }
            if runtime.status() == TaskStatus::Stopped {
                tracing::debug!(task = %id, "任务已从配置移除，丢弃运行时");
                return false;
            }
            tracing::warn!(task = %id, "任务已从配置移除但仍在运行，保留实例以便停止");
            true
        });
    }

    /// C8：比对每个插件任务"清单里的版本"与"导入时保存的版本"，算出可更新集合。
    ///
    /// 只在配置加载/重载时执行一次（避免每个 tick 都读清单文件）；清单缺失或损坏
    /// 时只记 debug，不影响快照生成，也不会把任务标成"可更新"。
    fn refresh_plugin_updates(&mut self) {
        let mut updates = HashSet::new();
        if let Some(loaded) = self.loaded.as_ref() {
            for task in &loaded.config.tasks {
                let Some(plugin) = task.plugin.as_ref() else {
                    continue;
                };
                match config::plugin::manifest_version(&plugin.manifest) {
                    Ok(version) => {
                        if config::plugin::version_is_newer(&version, plugin.version.as_deref()) {
                            tracing::debug!(
                                task = %task.id,
                                manifest = %plugin.manifest.display(),
                                manifest_version = %version,
                                stored_version = plugin.version.as_deref().unwrap_or("-"),
                                "插件清单版本高于已存版本，界面提示可更新"
                            );
                            updates.insert(task.id.clone());
                        }
                    }
                    Err(error) => tracing::debug!(
                        task = %task.id,
                        manifest = %plugin.manifest.display(),
                        error = %error,
                        "读取插件清单版本失败，跳过可更新检测"
                    ),
                }
            }
        }
        self.plugin_updates = updates;
    }

    /// 把运行时视图与恢复的监督状态填进快照（只更新快照里存在的任务）。
    pub(crate) fn refresh(&mut self) {
        // 先复制时段配置，避免在可变借用 snapshot.tasks 时再借用 loaded。
        let schedules: HashMap<String, crate::config::ScheduleConfig> = self
            .loaded
            .as_ref()
            .map(|loaded| {
                loaded
                    .config
                    .tasks
                    .iter()
                    .map(|task| (task.id.clone(), task.schedule.clone()))
                    .collect()
            })
            .unwrap_or_default();
        for task in &mut self.snapshot.tasks {
            task.runtime = self
                .runtimes
                .get(&task.id)
                .map(|runtime| RuntimeView::capture(&runtime.snapshot()));
            task.supervisor = self
                .state
                .tasks
                .get(&task.id)
                .map(SupervisorView::capture)
                .unwrap_or_default();
            // M3-6a：apply_changes=next_start 下配置已改但进程仍在用旧配置。
            task.supervisor.restart_pending = self.restart_pending.contains(&task.id);
            // A3：展示状态与可用动作由后端用权威规则算好，托盘/界面只读字段。
            let runtime_status = task.runtime.as_ref().map(|runtime| runtime.status.as_str());
            task.display_status = crate::snapshot::compute_display_status(
                task.valid,
                task.enabled,
                runtime_status,
                task.supervisor.last_status.as_deref(),
                task.supervisor.needs_human.active,
                task.supervisor.paused_reason.is_some(),
            )
            .to_owned();
            task.allowed_actions =
                crate::snapshot::compute_allowed_actions(task.valid, task.enabled, runtime_status);
            // C8：插件任务的"可更新"标记（配置加载时已比对好清单版本）。
            if let Some(plugin) = task.plugin.as_mut() {
                plugin.update_available = self.plugin_updates.contains(&task.id);
            }
            // 2026-11-04：运行时段动态字段（窗口静态字段也随配置刷新）。
            // 2026-11-05：不再区分启动来源（schedule_started 恒 false，字段保留
            // 只为兼容旧前端协议）。
            if let Some(schedule) = schedules.get(&task.id) {
                task.schedule = crate::snapshot::ScheduleView::from_config(schedule);
            }
        }
        // B1/C3：退出确认配置与一次性托盘提示（都来自当前内存状态）。
        self.snapshot.confirm_exit = self
            .loaded
            .as_ref()
            .map(|loaded| loaded.config.app.confirm_exit)
            .unwrap_or(true);
        self.snapshot.tray_hint_pending = self.state.ui.tray_hint_pending;
        // 配置监听状态：watcher 线程只更新共享状态，这里复制进快照。
        self.snapshot.config_watch = self.watch_state.view();
    }

    /// 命令前置检查 + 登记 in-flight。
    ///
    /// 返回后调用方释放 Engine 锁执行阻塞的启停；期间 reload 会跳过该任务。
    pub(crate) fn begin_command(
        &mut self,
        id: &str,
        action: Action,
    ) -> Result<CommandPlan, IpcError> {
        let Some(runtime) = self.runtimes.get(id).cloned() else {
            return Err(self.unknown_task_error(id));
        };

        // Stop 只依赖运行时：任务被禁用、或已从配置删除但实例仍在运行时，
        // 仍必须允许用户收敛进程（M2-4b 语义）。
        if matches!(action, Action::Stop) {
            let Some(generation) = self.gate.try_begin_command(id) else {
                return Err(IpcError::with_code(
                    "invalid_state",
                    None,
                    format!("任务 {id} 有操作正在进行，请稍后重试"),
                    Some(id.to_owned()),
                ));
            };
            self.prepare_manual_supervision(id, Action::Stop);
            let timeout = runtime.spec().watch.stop_timeout;
            return Ok(CommandPlan {
                runtime,
                previous: None,
                timeout,
                generation,
                context: None,
            });
        }

        let (valid, enabled, spec) = {
            let Some(loaded) = self.loaded.as_ref() else {
                return Err(self.unknown_task_error(id));
            };
            let Some(task) = loaded.config.task(id) else {
                return Err(self.unknown_task_error(id));
            };
            // 用“当前配置”的路径/任务名/日志参数构造启动上下文，而不是沿用
            // 运行中实例可能已过期的 spec（配置变更在下一个实例生效）。
            // 2026-11-05：手动 Start/Restart/恢复统一注入 START_REASON=user_start。
            let mut spec =
                TaskSpec::from_config_with_paths(task, &loaded.data_dir, &loaded.config_path);
            spec.instance.start_reason = StartReason::UserStart;
            spec.instance.window_end = schedule_window_end(task);
            (loaded.is_task_valid(id), task.enabled, spec)
        };
        let context = spec.instance.clone();
        let status = runtime.status();
        let allowed = match action {
            Action::Start => manual_start_allowed(valid, enabled, status),
            Action::Restart => manual_restart_allowed(valid, enabled, status),
            // Stop 已在上面提前返回。
            Action::Stop => unreachable!(),
        };
        if !allowed {
            let disabled = !enabled && matches!(action, Action::Start | Action::Restart);
            let message = if disabled {
                format!("任务 {id} 已禁用（enabled=false），不能手动启动")
            } else {
                format!(
                    "任务 {id} 当前状态为 {status}，不能执行 {}",
                    action.as_str()
                )
            };
            let stage = if disabled {
                "disabled"
            } else {
                "invalid_state"
            };
            return Err(IpcError::with_code(
                stage,
                None,
                message,
                Some(id.to_owned()),
            ));
        }
        let Some(generation) = self.gate.try_begin_command(id) else {
            return Err(IpcError::with_code(
                "invalid_state",
                None,
                format!("任务 {id} 有操作正在进行，请稍后重试"),
                Some(id.to_owned()),
            ));
        };

        // 已停止的运行时按当前配置重建：这样 Start/Restart 的下一个实例使用
        // 最新命令/日志目录/心跳路径，而不是等待下一次 reload。
        // M3-6a：运行中的 Restart 也按当前配置新建运行时（`apply_changes=next_start`
        // 的 restartPending 就是靠这一步落地），旧实例先停止再换表。
        let (runtime, previous) = if status == TaskStatus::Stopped {
            let fresh = Arc::new(TaskRuntime::with_events(spec, self.events.clone()));
            self.runtimes.insert(id.to_owned(), Arc::clone(&fresh));
            tracing::debug!(task = %id, "任务已停止，按当前配置重建运行时");
            (fresh, None)
        } else if matches!(action, Action::Restart) {
            let fresh = Arc::new(TaskRuntime::with_events(spec, self.events.clone()));
            tracing::debug!(task = %id, "任务运行中，重启时按当前配置新建运行时");
            (fresh, Some(runtime))
        } else {
            (runtime, None)
        };
        self.prepare_manual_supervision(id, action);
        let timeout = runtime.spec().watch.stop_timeout;
        Ok(CommandPlan {
            runtime,
            previous,
            timeout,
            generation,
            context: Some(context),
        })
    }

    /// Restart 时旧实例已停止：把运行时表切换到新配置的实例。
    ///
    /// 在停止成功之后、启动之前调用（持 Engine 锁），避免停止失败时把仍在
    /// 运行的旧实例从表里换掉。
    pub(crate) fn install_restart_runtime(&mut self, id: &str, runtime: Arc<TaskRuntime>) {
        self.runtimes.insert(id.to_owned(), runtime);
    }

    /// 运行中 Restart 在停止旧实例阶段失败：旧进程可能仍在运行，不能按
    /// 启动失败清理（清 monitor/instance 会让旧进程变成无监督的幽灵）。
    ///
    /// `begin_command` 已把 supervisor 切到 Restart 的临时状态；这里恢复旧实例：
    /// - Running/Starting：重放运行事实、重建监视器；命令期间 reload 产生的
    ///   pending_stop（配置重启/删除收敛）保留给调度线程继续处理；
    /// - Stopping：保留用户停止来源，退出事件只收敛为 stopped，不自动重启；
    /// - Stopped：按已停止收敛，不拉起。
    pub(crate) fn recover_restart_stop_failure(&mut self, task_id: &str, message: &str) -> bool {
        let mono_now = Instant::now();
        let wall_now = Timestamp::now();
        let Some(runtime) = self.runtimes.get(task_id).cloned() else {
            return false;
        };
        let status = runtime.status();
        let snapshot = runtime.snapshot();
        let started = runtime.instance_started_instant();
        let instance_id = snapshot.instance_id.clone();
        // 就绪计划要用当前配置；先算好，避免与 supervisor 的可变借用冲突。
        // alive 以旧状态机为准：Starting 的实例即使 runtime 已 Running，也不能
        // 在恢复时跳过 readiness/启动宽限（QA C / B 非阻断）。
        let old_machine_status = self
            .supervisors
            .get(task_id)
            .map(|supervisor| supervisor.machine.status())
            .unwrap_or(RestartStatus::Stopped);
        let alive = alive_status_for_rebuild(old_machine_status, Some(status));
        let readiness = if alive == Some(RestartStatus::Starting) {
            started
                .zip(
                    self.loaded
                        .as_ref()
                        .and_then(|loaded| loaded.config.task(task_id)),
                )
                .map(|(started, task)| {
                    readiness_plan(
                        task.health.kind,
                        started,
                        Duration::from_millis(task.health.startup_grace_ms),
                    )
                })
        } else {
            None
        };

        let Some(supervisor) = self.supervisors.get_mut(task_id) else {
            return false;
        };
        let pending_stop = supervisor.pending_stop.take();
        supervisor.expect_start = false;
        supervisor.pending_start = false;
        supervisor.auto_start_at = None;
        supervisor.health_startup_failure = false;
        supervisor.stop_retry_at = None;
        supervisor.stop_retry_count = 0;
        supervisor.restart_count_pending = false;
        supervisor.stop_cause = None;
        match status {
            TaskStatus::Starting | TaskStatus::Running => {
                let alive = alive.unwrap_or(if status == TaskStatus::Running {
                    RestartStatus::Running
                } else {
                    RestartStatus::Starting
                });
                // 用户显式 Restart 已清除 needs_human，恢复时不再重新置位。
                replay_alive_machine(&mut supervisor.machine, alive, None, mono_now);
                if instance_id.is_some() {
                    supervisor.instance_id = instance_id.clone();
                }
                supervisor.started_instant = started;
                supervisor.readiness = readiness;
                // 保留命令期间 reload 请求的停止，让调度线程继续收敛。
                supervisor.pending_stop = pending_stop;
            }
            TaskStatus::Stopping => {
                let _ = supervisor
                    .machine
                    .handle(RestartEvent::StopRequested, mono_now);
                supervisor.monitor = None;
                supervisor.next_health_at = None;
                supervisor.started_instant = None;
                supervisor.readiness = None;
                supervisor.pending_stop = None;
                // 旧进程仍在停止中：退出事件按用户停止处理，不自动重启。
                supervisor.stop_cause = Some(StopCause::User);
            }
            TaskStatus::Stopped => {
                let _ = supervisor
                    .machine
                    .handle(RestartEvent::StopRequested, mono_now);
                supervisor.monitor = None;
                supervisor.next_health_at = None;
                supervisor.clear_instance();
                supervisor.started_instant = None;
                supervisor.readiness = None;
                supervisor.pending_stop = None;
            }
        }

        if matches!(status, TaskStatus::Starting | TaskStatus::Running)
            && let (Some(instance_id), Some(started)) = (instance_id.as_deref(), started)
        {
            self.start_health_monitor(task_id, instance_id, &snapshot, started, mono_now);
        }
        let mut changed = self.sync_supervisor_state(task_id, mono_now, wall_now);
        if status == TaskStatus::Stopping {
            // 状态机已收敛为 stopped，但运行时仍在停止中；保留运行时事实。
            changed |= self.apply_runtime_snapshot_state(task_id, &snapshot, wall_now);
        }
        tracing::warn!(
            task = %task_id,
            status = status.as_str(),
            message,
            "重启时停止旧实例失败，已保留旧实例监督状态"
        );
        changed
    }

    /// 命令结束：清 in-flight、合并用户意图；若期间 reload 过则补一次同步。
    pub(crate) fn finish_command(&mut self, id: &str, action: Action, started_generation: u64) {
        let reloaded = self.gate.finish_command(id, started_generation);
        if let Some(state) = self.state_task_mut(id) {
            apply_user_intent(state, UserAction::from(action));
        }
        if reloaded {
            tracing::debug!(task = %id, "命令期间配置已重载，补一次运行时同步");
            self.resync_after_reload();
        }
        self.wake();
    }

    /// 手动 Start/Restart/Stop 的监督状态迁移（状态机 + state.json 同步）。
    fn prepare_manual_supervision(&mut self, id: &str, action: Action) {
        let mono_now = Instant::now();
        let wall_now = Timestamp::now();
        let Some(supervisor) = self.supervisors.get_mut(id) else {
            return;
        };
        // Restart 的旧实例退出、Stop 的退出都不计入重启失败；普通 Start 清掉
        // 可能残留的上一次停止来源。
        supervisor.stop_cause = match action {
            Action::Start => None,
            Action::Stop => Some(StopCause::User),
            // M3-6a：Restart 之后还有新实例要启动，退出事件不能清掉 expect_start。
            Action::Restart => Some(StopCause::UserRestart),
        };
        supervisor.monitor = None;
        supervisor.next_health_at = None;
        supervisor.pending_start = false;
        supervisor.auto_start_at = None;
        supervisor.health_startup_failure = false;
        // 用户 Stop 可接管正在重试的健康停止；保留 cleanup_on_stop（B3/B13）。
        supervisor.pending_stop = None;
        supervisor.stop_retry_at = None;
        supervisor.stop_retry_count = 0;
        supervisor.readiness = None;
        supervisor.restart_count_pending = false;
        supervisor.expect_start = matches!(action, Action::Start | Action::Restart);
        if matches!(action, Action::Start | Action::Restart) {
            supervisor.cleanup_on_stop = None;
            supervisor.cleanup_heartbeat = None;
        }
        match action {
            Action::Start => {
                supervisor.clear_instance();
                supervisor.started_instant = None;
                let _ = supervisor
                    .machine
                    .handle(RestartEvent::StartRequested, mono_now);
            }
            Action::Restart => {
                let _ = supervisor
                    .machine
                    .handle(RestartEvent::StopRequested, mono_now);
                supervisor.started_instant = None;
                let _ = supervisor
                    .machine
                    .handle(RestartEvent::StartRequested, mono_now);
            }
            Action::Stop => {
                let _ = supervisor
                    .machine
                    .handle(RestartEvent::StopRequested, mono_now);
            }
        }
        let snapshot = supervisor.machine.snapshot(mono_now);
        let state = self.state.task_mut(id);
        sync_state(state, &snapshot, mono_now, wall_now);
        // Q4/Q15：用户意图（override/失败计数）在命令发出时立即持久化，
        // 不等待阻塞的 stop/start 完成；finish_command 会再次幂等合并。
        apply_user_intent(state, UserAction::from(action));
        if matches!(action, Action::Start | Action::Restart) {
            // M3-5b：用户显式重试即"已处理"，再次失败可以重新通知。
            self.notifications.clear_task(id);
        }
        // 2026-11-04：用户手动停止时记下本窗口抑制；手动启动清除。必须在
        // persist 前写入，应用重启后仍保持“本窗口内不再自动拉起”。
        self.observe_manual_schedule_action(id, action);
        self.persist();
        self.wake();
    }

    fn unknown_task_error(&self, id: &str) -> IpcError {
        let message = match self.loaded.as_ref() {
            None => format!("配置未成功加载，无法操作任务 {id}"),
            Some(loaded) if loaded.config.task(id).is_none() => {
                format!("配置中没有任务 {id}")
            }
            Some(_) => format!("任务 {id} 未通过配置校验，无法启动"),
        };
        IpcError::with_code("invalid_task", None, message, Some(id.to_owned()))
    }
}

/// M3-4b：运行时事件 → 监督状态迁移。
impl Engine {
    /// 唤醒后台调度线程（tick 之外的即时唤醒）。
    pub(crate) fn wake(&self) {
        let _ = self.wake.send(());
    }

    /// 本次任务现在是否仍允许自动启动/自动重启。
    ///
    /// 排期后配置可能变化（enabled/auto_start/override/paused/needs_human），
    /// 调度执行前必须再检查一次，绝不在用户停止/暂停后偷偷拉起（Q5）。
    fn auto_start_permitted(&self, task_id: &str, require_auto_start: bool) -> bool {
        let Some(loaded) = self.loaded.as_ref() else {
            return false;
        };
        let Some(task) = loaded.config.task(task_id) else {
            return false;
        };
        if !task.enabled || (require_auto_start && !task.auto_start) {
            return false;
        }
        !self.state.task(task_id).is_some_and(|state| {
            state.desired_override.is_some()
                || state.paused_reason.is_some()
                || state.needs_human.active
        })
    }

    /// 运行时段进入窗口时的自动启动许可（2026-11-05）。
    ///
    /// 与 [`Engine::auto_start_permitted`] 的差别只有一点：进入新窗口会清除上一次
    /// 窗口残留的 Stop override，因此这里忽略 `desired_override=stopped`；
    /// `desired_override=paused` / `paused_reason` / `needs_human` 仍然拦截，
    /// 长期意图不会被时段调度偷偷覆盖。本窗口内的用户 Stop 由
    /// `schedule_suppressed` 单独判断（见 [`Engine::schedule_work`]）。
    fn schedule_start_permitted(&self, task_id: &str) -> bool {
        let Some(loaded) = self.loaded.as_ref() else {
            return false;
        };
        let Some(task) = loaded.config.task(task_id) else {
            return false;
        };
        if !task.enabled {
            return false;
        }
        !self.state.task(task_id).is_some_and(|state| {
            state.desired_override == Some(DesiredOverride::Paused)
                || state.paused_reason.is_some()
                || state.needs_human.active
        })
    }

    /// B3/B13/M3-6a：配置删除/失效/enabled=false 的停止收敛，以及进程字段
    /// 变化后的重启/待重启标记。
    ///
    /// 必须先于 sync_runtimes：已停止的删除任务需要在这里清理心跳文件，
    /// 否则 runtime 被丢弃后路径信息就丢失了。
    ///
    /// `previous` 是上一次成功加载的任务配置（reload 在替换 `self.loaded`
    /// 之前保存）；没有上一版（首次加载/结构错误恢复）时只做 B3/B13。
    fn reconcile_config_changes(
        &mut self,
        now: Instant,
        wall_now: Timestamp,
        previous: &HashMap<String, TaskConfig>,
    ) -> bool {
        let mut changed = false;
        let Some(_) = self.loaded.as_ref() else {
            return false;
        };
        let ids: Vec<String> = self.runtimes.keys().cloned().collect();
        for task_id in ids {
            if self.gate.is_in_flight(&task_id) {
                // in-flight 命令结束后 finish_command → resync_after_reload 再收敛。
                continue;
            }
            let Some(runtime) = self.runtimes.get(&task_id).cloned() else {
                continue;
            };
            let status = runtime.status();
            let (valid, enabled) = {
                let loaded = self.loaded.as_ref().expect("已检查存在");
                match loaded.config.task(&task_id) {
                    Some(task) => (loaded.is_task_valid(&task_id), task.enabled),
                    None => (false, false),
                }
            };
            let action = reconcile_action(valid, enabled, Some(status));
            if action == ConfigReconcile::Keep {
                // 有效且启用的任务：按 docs/02 §7.3 处理字段级变化。
                // 复制当前任务配置，避免与 apply_config_change 的 &mut self 冲突。
                let current_task = self
                    .loaded
                    .as_ref()
                    .and_then(|loaded| loaded.config.task(&task_id))
                    .cloned();
                if let Some(previous_task) = previous.get(&task_id)
                    && let Some(current_task) = current_task
                {
                    changed |= self.apply_config_change(
                        &task_id,
                        previous_task,
                        &current_task,
                        status,
                        now,
                        wall_now,
                    );
                }
                continue;
            }
            let heartbeat = runtime.instance_context().heartbeat;
            if status == TaskStatus::Stopped {
                // 已经停止的删除/失效任务：直接清理（B3）。
                changed |= self.finish_reconcile(&task_id, action, heartbeat);
                continue;
            }
            if self
                .supervisors
                .get(&task_id)
                .is_some_and(|supervisor| supervisor.cleanup_on_stop.is_some())
            {
                // 已请求过收敛停止，等待退出事件。
                continue;
            }
            if !self.supervisors.contains_key(&task_id) {
                // 防御：运行中实例必须有 supervisor（B3 不应出现无监督进程）。
                let machine = RestartMachine::new(RestartPolicy::default());
                self.supervisors.insert(
                    task_id.clone(),
                    TaskSupervisor::new(machine, Duration::from_secs(10)),
                );
            }
            let cause = match action {
                ConfigReconcile::StopAndKeep => StopCause::ConfigDisabled,
                ConfigReconcile::StopAndRemove => StopCause::ConfigRemoved,
                ConfigReconcile::Keep => continue,
            };
            let decision = {
                let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                supervisor.pending_stop = Some(cause);
                supervisor.stop_retry_at = None;
                supervisor.stop_retry_count = 0;
                supervisor.cleanup_on_stop = Some(action);
                supervisor.cleanup_heartbeat = heartbeat;
                supervisor.monitor = None;
                supervisor.next_health_at = None;
                supervisor.pending_start = false;
                supervisor.auto_start_at = None;
                supervisor.expect_start = false;
                supervisor.readiness = None;
                supervisor.machine.handle(RestartEvent::StopRequested, now)
            };
            changed |= self.apply_decision(&task_id, &decision, now, wall_now);
            // 停止原因已由 apply_decision 的 task.decision 事件记录；
            // 真正执行停止时 prepare_stop_job 会再写一条 task.stop。
        }
        changed
    }

    /// docs/02 §7.3：按字段分类处理一个有效任务的配置变化。
    ///
    /// - 立即生效字段（name/notify/logs.encoding/logs.rotation/panel_url）无需动作，
    ///   快照已随 reload 更新；
    /// - `health.*` / `restart.*` / `power.resume_grace_ms` 立即重建监视器与状态机；
    /// - 进程字段按 `apply_changes` 重启（`restart`）或标记 `restart_pending`
    ///   （`next_start`）。用户意图/暂停/人工处理优先，不会自动拉起。
    fn apply_config_change(
        &mut self,
        task_id: &str,
        previous: &TaskConfig,
        current: &TaskConfig,
        status: TaskStatus,
        now: Instant,
        wall_now: Timestamp,
    ) -> bool {
        let change = classify_config_change(previous, current);
        if change.is_empty() {
            return false;
        }
        let apply = self
            .loaded
            .as_ref()
            .map_or(ApplyChanges::Restart, |loaded| {
                loaded.config.supervisor.apply_changes
            });
        let decision = decide_reconcile(&change, status != TaskStatus::Stopped, apply);
        let mut changed = false;

        // immediate / auto_start 不需要动作，也不再各写一条 info：配置重载本身
        // 已有 config.reload 事件，逐任务重复只会淹没真正的状态迁移。
        if change.supervision && matches!(status, TaskStatus::Starting | TaskStatus::Running) {
            // 已停止/停止中的任务不在这里重建：前者由 sync_supervisors 按新策略
            // 恢复（保留退避/暂停记忆），后者要等停止流程结束。
            changed |= self.rebuild_supervision(task_id, current, now, wall_now);
        }

        match decision {
            ReconcileDecision::RestartNow => {
                if self.config_restart_allowed(task_id) {
                    changed |= self.request_config_restart(task_id, now, wall_now);
                } else if self.restart_pending.insert(task_id.to_owned()) {
                    // 用户停止/暂停/需要人工处理：不停止进程，只提示重启后生效。
                    tracing::info!(
                        event = "config.change",
                        task = %task_id,
                        change = "restart_pending",
                        "配置变更需要重启，但当前意图不允许自动拉起，标记为待重启"
                    );
                    changed = true;
                }
            }
            ReconcileDecision::RestartNextStart => {
                if self.restart_pending.insert(task_id.to_owned()) {
                    tracing::info!(
                        event = "config.change",
                        task = %task_id,
                        change = "restart_pending",
                        "配置变更需要重启，已标记 restart_pending（apply_changes=next_start）"
                    );
                    changed = true;
                }
            }
            ReconcileDecision::Keep | ReconcileDecision::RefreshSupervision => {}
        }
        changed
    }

    /// 运行中实例能否被配置变更自动重启（用户意图/暂停/人工处理优先）。
    fn config_restart_allowed(&self, task_id: &str) -> bool {
        let Some(loaded) = self.loaded.as_ref() else {
            return false;
        };
        let Some(task) = loaded.config.task(task_id) else {
            return false;
        };
        if !task.enabled || !loaded.is_task_valid(task_id) {
            return false;
        }
        !self.state.task(task_id).is_some_and(|state| {
            state.desired_override.is_some()
                || state.paused_reason.is_some()
                || state.needs_human.active
        })
    }

    /// 进程字段变化（apply_changes=restart）：停止旧实例，退出后按新配置启动。
    ///
    /// 停止沿用既有 pending_stop → StopJob 流程（in-flight/重试/超时都在
    /// 那里处理），退出事件按 [`StopCause::ConfigRestart`] 不计失败、不进
    /// 退避，并由事件处理重新走 StartRequested（consecutive_failures 清零）。
    fn request_config_restart(&mut self, task_id: &str, now: Instant, wall_now: Timestamp) -> bool {
        let Some(supervisor) = self.supervisors.get_mut(task_id) else {
            return false;
        };
        if supervisor.pending_stop.is_some()
            || supervisor.cleanup_on_stop.is_some()
            || supervisor.stop_cause.is_some()
        {
            // 已有停止收敛在进行，等它结束后按新配置启动。
            return false;
        }
        supervisor.monitor = None;
        supervisor.next_health_at = None;
        supervisor.pending_start = false;
        supervisor.auto_start_at = None;
        supervisor.expect_start = false;
        supervisor.readiness = None;
        supervisor.stop_retry_at = None;
        supervisor.stop_retry_count = 0;
        supervisor.health_startup_failure = false;
        supervisor.pending_stop = Some(StopCause::ConfigRestart);
        let decision = supervisor.machine.handle(RestartEvent::StopRequested, now);
        let mut changed = self.apply_decision(task_id, &decision, now, wall_now);
        if let Some(state) = self.state_task_mut(task_id)
            && state.last_reason.as_deref() != Some("config_changed")
        {
            state.last_reason = Some("config_changed".to_owned());
            changed = true;
        }
        // 具体停止动作由 prepare_stop_job 写 task.stop（reason=config_restart），
        // 这里不再重复一条 info。
        changed
    }

    /// `health.*` / `restart.*` 变化：重建运行中实例的监视器与状态机。
    ///
    /// 失败计数与探测调度重置（新的 HealthMonitor/RestartMachine），但
    /// `needs_human` / 暂停意图保留——它们需要用户处理或心跳恢复才清除。
    fn rebuild_supervision(
        &mut self,
        task_id: &str,
        task: &TaskConfig,
        now: Instant,
        wall_now: Timestamp,
    ) -> bool {
        let Some(supervisor) = self.supervisors.get(task_id) else {
            return false;
        };
        if supervisor.pending_stop.is_some() || supervisor.stop_cause.is_some() {
            // 停止收敛中：重建会丢失停止来源，等停止完成后再按新策略恢复。
            tracing::debug!(task = %task_id, "停止收敛中，跳过监督状态重建");
            return false;
        }
        let old_status = supervisor.machine.status();
        let runtime_status = self.runtimes.get(task_id).map(|runtime| runtime.status());
        // 状态机仍在 starting 时（runtime 可能已 spawn 成功）保留 starting 语义，
        // 不能因为 runtime 已 Running 就跳过 readiness/启动宽限；否则才按运行时
        // 实际存活状态重放（needs_human 暂停的活进程也在此列，QA C）。
        let alive = alive_status_for_rebuild(old_status, runtime_status);

        // 监视器需要运行时的 pid/实例 id/启动时刻/注入上下文。
        let runtime_facts = self.runtimes.get(task_id).and_then(|runtime| {
            let snapshot = runtime.snapshot();
            let pid = snapshot.pid?;
            let instance_id = snapshot.instance_id.clone()?;
            let started = runtime.instance_started_instant()?;
            Some((pid, instance_id, started, runtime.instance_context()))
        });

        let seed = supervisor.machine.seed();
        // 只有“运行时存活且状态机没被熔断/暂停”才重放运行事实；已因
        // crash_loop 暂停的存活实例保持暂停，不因一次配置重载被放出来。
        // needs_human 暂停是例外：标志正交，进程仍应保持 running。
        let needs_human_paused =
            old_status == RestartStatus::Paused && seed.paused_reason == Some(Reason::NeedsHuman);
        let replay_alive = alive.is_some()
            && (matches!(old_status, RestartStatus::Starting | RestartStatus::Running)
                || needs_human_paused);

        let reset = RestartSeed {
            // 新策略下重新计失败预算；暂停/人工标志与用户意图保持不变。
            consecutive_failures: 0,
            // 存活实例的 needs_human 不能交给 restore：restore 会强制 Paused，
            // 随后 sync 把存活进程写成 paused。先按运行意图恢复，重放
            // Started 之后再补 NeedsHumanObserved（进程保持 running）。
            paused: if replay_alive { false } else { seed.paused },
            paused_reason: if replay_alive {
                None
            } else {
                seed.paused_reason
            },
            needs_human: if replay_alive {
                None
            } else {
                seed.needs_human.clone()
            },
            next_retry_at: None,
        };
        let mut machine = RestartMachine::restore(
            RestartPolicy::from_config(&task.restart),
            SystemRandom::new(),
            reset,
        );
        if let Some(alive) = alive
            && replay_alive
        {
            replay_alive_machine(&mut machine, alive, seed.needs_human.clone(), now);
        }
        let interval = health_interval(task);
        let started = runtime_facts.as_ref().map(|(_, _, started, _)| *started);
        let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
        supervisor.machine = machine;
        supervisor.health_interval = interval;
        // Starting 实例按新 health 配置重算就绪计划（启动宽限/首健康策略）；
        // 状态机已 Running 的实例不需要 readiness。
        if replay_alive {
            supervisor.readiness = if alive == Some(RestartStatus::Starting) {
                started.map(|started| {
                    readiness_plan(
                        task.health.kind,
                        started,
                        Duration::from_millis(task.health.startup_grace_ms),
                    )
                })
            } else {
                None
            };
        }

        // 用新配置重建监视器：失败计数与下次探测时刻一并重置。
        match runtime_facts {
            Some((pid, instance_id, started, context)) => {
                match build_health_monitor(task, &context, pid, &instance_id, started) {
                    Some(monitor) => {
                        supervisor.monitor = Some(monitor);
                        supervisor.next_health_at = Some(now + interval);
                    }
                    None => {
                        tracing::warn!(
                            task = %task_id,
                            "新健康配置缺少必要字段（heartbeat path / http url），本实例暂停健康探测"
                        );
                        supervisor.monitor = None;
                        supervisor.next_health_at = None;
                    }
                }
            }
            None => {
                supervisor.monitor = None;
                supervisor.next_health_at = None;
            }
        }

        let changed = self.sync_supervisor_state(task_id, now, wall_now);
        tracing::info!(
            event = "config.change",
            task = %task_id,
            change = "supervision",
            health = task.health.kind.as_str(),
            interval_ms = task.health.interval_ms,
            "配置变更：已按新健康/重启策略重建监督状态"
        );
        changed
    }

    /// 命令/调度动作期间发生过 reload：补一次 runtime/supervisor 对齐。
    pub(crate) fn resync_after_reload(&mut self) {
        let now = Instant::now();
        let wall_now = Timestamp::now();
        // 命令期间 reload 已更新 previous_tasks；这里复制一份避免借用冲突
        //（只在命令期间确实重载过时才走到，频率很低）。
        let previous = self.previous_tasks.clone();
        self.reconcile_config_changes(now, wall_now, &previous);
        self.sync_runtimes();
        let added = self.sync_supervisors(now, wall_now);
        self.schedule_auto_start(now, wall_now, Some(&added));
        // 本任务的命令已结束，reconcile 也已消费掉这次变化：把基线写回当前
        // 配置，避免下一次无关 reload 再判一次相同变化（重复重启/待重启）。
        // 其他仍在执行命令的任务由 update_baseline 继续保留旧基线。
        if self.loaded.is_some() {
            self.previous_tasks = update_baseline(
                &previous,
                self.current_task_configs(),
                self.gate.in_flight(),
            );
        }
    }

    /// 事件携带的 instance_id 是否属于当前实例（docs/03 §11 不变量 6）。
    fn event_instance_current(&self, task_id: &str, event_instance: Option<&str>) -> bool {
        self.supervisors.get(task_id).is_some_and(|supervisor| {
            instance_matches(supervisor.instance_id.as_deref(), event_instance)
        })
    }

    /// 把运行时快照合并进 state.json 内存模型中的运行事实。
    fn apply_runtime_snapshot_state(
        &mut self,
        task_id: &str,
        snapshot: &RuntimeSnapshot,
        wall_now: Timestamp,
    ) -> bool {
        let Some(state) = self.state_task_mut(task_id) else {
            return false;
        };
        apply_runtime_snapshot(state, snapshot, wall_now)
    }

    /// 为当前实例创建健康监视器（启动宽限从进程创建成功时刻开始）。
    fn start_health_monitor(
        &mut self,
        task_id: &str,
        instance_id: &str,
        snapshot: &RuntimeSnapshot,
        started_instant: Instant,
        mono_now: Instant,
    ) -> bool {
        let Some(pid) = snapshot.pid else {
            tracing::warn!(task = %task_id, "运行中实例没有 PID，无法建立健康探测");
            return false;
        };
        let prepared = {
            let Some(loaded) = self.loaded.as_ref() else {
                return false;
            };
            let Some(task) = loaded.config.task(task_id) else {
                return false;
            };
            let context = self
                .runtimes
                .get(task_id)
                .map(|runtime| runtime.instance_context())
                .unwrap_or_default();
            build_health_monitor(task, &context, pid, instance_id, started_instant)
        };
        let Some(monitor) = prepared else {
            tracing::warn!(
                task = %task_id,
                "健康配置缺少必要字段（heartbeat path / http url），本实例跳过健康探测"
            );
            return false;
        };
        let interval = self
            .supervisors
            .get(task_id)
            .map_or(Duration::from_secs(10), |supervisor| {
                supervisor.health_interval
            });
        if let Some(supervisor) = self.supervisors.get_mut(task_id) {
            supervisor.monitor = Some(monitor);
            let first = started_instant + interval;
            supervisor.next_health_at = Some(if first > mono_now { first } else { mono_now });
        }
        true
    }

    /// 以状态机快照为准同步 last_status/last_reason 等（B1：就绪前保持 starting）。
    fn sync_supervisor_state(
        &mut self,
        task_id: &str,
        mono_now: Instant,
        wall_now: Timestamp,
    ) -> bool {
        let Some(snapshot) = self
            .supervisors
            .get(task_id)
            .map(|supervisor| supervisor.machine.snapshot(mono_now))
        else {
            return false;
        };
        let Some(state) = self.state_task_mut(task_id) else {
            return false;
        };
        sync_state(state, &snapshot, mono_now, wall_now)
    }

    /// 就绪迁移：首次健康通过（heartbeat/http）或启动宽限到期（process）后
    /// 才 machine.Started；stable_after 从这一刻开始计时（B1）。
    fn mark_instance_started(
        &mut self,
        task_id: &str,
        mono_now: Instant,
        wall_now: Timestamp,
    ) -> bool {
        let Some(status) = self
            .supervisors
            .get(task_id)
            .map(|supervisor| supervisor.machine.status())
        else {
            return false;
        };
        if status != RestartStatus::Starting {
            return false;
        }
        let count_restart = self
            .supervisors
            .get(task_id)
            .is_some_and(|supervisor| supervisor.restart_count_pending);
        let decision = {
            let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
            supervisor.machine.handle(RestartEvent::Started, mono_now)
        };
        let mut changed = self.apply_decision(task_id, &decision, mono_now, wall_now);
        if let Some(supervisor) = self.supervisors.get_mut(task_id) {
            supervisor.readiness = None;
            supervisor.restart_count_pending = false;
        }
        if let Some(state) = self.state_task_mut(task_id) {
            // N6：只有成功就绪的自动重启才计入 total_restarts。
            changed |= count_restart_success(state, count_restart);
        }
        tracing::info!(event = "task.ready", task = %task_id, "实例已就绪，监督状态进入 running");
        // M3-5b：恢复到 running 即视为"已恢复"，再次进入异常可以重新通知
        // （仍受 5 分钟同类节流约束）。
        self.notifications.clear_task(task_id);
        changed
    }

    /// 请求停止实例（健康失败/配置收敛）；返回可立即执行的 StopJob。
    fn request_stop(
        &mut self,
        task_id: &str,
        cause: StopCause,
        startup_failure: bool,
        mono_now: Instant,
    ) -> bool {
        let snapshot = {
            let Some(supervisor) = self.supervisors.get_mut(task_id) else {
                return false;
            };
            supervisor.monitor = None;
            supervisor.next_health_at = None;
            supervisor.pending_start = false;
            supervisor.auto_start_at = None;
            supervisor.pending_stop = Some(cause);
            supervisor.stop_retry_at = None;
            supervisor.stop_retry_count = 0;
            supervisor.health_startup_failure = startup_failure;
            supervisor.machine.snapshot(mono_now)
        };
        // 实际停止由调度线程在锁外执行；这里只记录变更。
        if let Some(state) = self.state_task_mut(task_id) {
            sync_state(state, &snapshot, mono_now, Timestamp::now());
        }
        true
    }

    /// 为 pending_stop 准备停止任务；失败/重试在 run_stop_job 中处理。
    fn prepare_stop_job(&mut self, task_id: &str, _now: Instant) -> Option<StopJob> {
        let cause = self.supervisors.get(task_id)?.pending_stop?;
        let runtime = self.runtimes.get(task_id)?.clone();
        if !matches!(
            runtime.status(),
            TaskStatus::Starting | TaskStatus::Running | TaskStatus::Stopping
        ) {
            return None;
        }
        let generation = self.gate.try_begin_command(task_id)?;
        let Some(supervisor) = self.supervisors.get_mut(task_id) else {
            let _ = self.gate.finish_command(task_id, generation);
            return None;
        };
        supervisor.pending_stop = None;
        supervisor.stop_retry_at = None;
        supervisor.stop_cause = Some(cause);
        if supervisor.stop_retry_count == 0 {
            tracing::info!(
                event = "task.stop",
                task = %task_id,
                reason = cause.as_str(),
                method = runtime.spec().watch.method.as_str(),
                grace_ms = runtime.spec().watch.grace.as_millis() as u64,
                "请求停止任务实例"
            );
        } else {
            tracing::debug!(
                task = %task_id,
                reason = cause.as_str(),
                method = runtime.spec().watch.method.as_str(),
                grace_ms = runtime.spec().watch.grace.as_millis() as u64,
                attempt = supervisor.stop_retry_count,
                "重试停止任务实例"
            );
        }
        let timeout = runtime.spec().watch.stop_timeout;
        Some(StopJob {
            task_id: task_id.to_owned(),
            runtime,
            timeout,
            generation,
            reason: "health",
        })
    }

    /// C6：本轮资源守卫检查，返回需要执行的停止任务。
    ///
    /// 只采样同时满足"实例 running / 没有待处理的停止 / 配置了 limits 任一非 0"
    /// 的任务，且全局最多每 [`LIMIT_SAMPLE_INTERVAL`] 一次。连续
    /// [`LimitTracker::THRESHOLD`] 次采样超限才停止：停止沿用健康失败路径
    /// （`StopCause::Health` → 退出后按 `restart` 策略处理），不新增 PausedReason。
    ///
    /// 采样失败（进程刚退出、权限不足）视为"本轮无结论"：清空基线与计数，
    /// 绝不因为读不到数据而停止任务。
    fn collect_limit_stops(&mut self, now: Instant) -> Vec<StopJob> {
        if let Some(last) = self.last_limit_check
            && now.saturating_duration_since(last) < LIMIT_SAMPLE_INTERVAL
        {
            return Vec::new();
        }
        self.last_limit_check = Some(now);

        let mut stops = Vec::new();
        let ids: Vec<String> = self.supervisors.keys().cloned().collect();
        for task_id in ids {
            let (max_rss_mb, max_cpu_percent) = self
                .loaded
                .as_ref()
                .and_then(|loaded| loaded.config.task(&task_id))
                .map(|task| (task.limits.max_rss_mb, task.limits.max_cpu_percent))
                .unwrap_or((0, 0));
            let enforcing = max_rss_mb > 0 || max_cpu_percent > 0;
            let Some(runtime) = self.runtimes.get(&task_id).cloned() else {
                self.reset_limit_tracking(&task_id);
                continue;
            };
            let supervised = self.supervisors.get(&task_id).is_some_and(|supervisor| {
                supervisor.pending_stop.is_none() && supervisor.stop_cause.is_none()
            });
            if !enforcing || runtime.status() != TaskStatus::Running || !supervised {
                // 不采样的任务必须清掉基线：等它重新进入 running 时从第 1 次算起。
                self.reset_limit_tracking(&task_id);
                continue;
            }
            let previous = self
                .supervisors
                .get(&task_id)
                .and_then(|supervisor| supervisor.last_limit_sample);
            let Some(sample) = runtime.snapshot().pid.and_then(limits::sample) else {
                self.reset_limit_tracking(&task_id);
                continue;
            };

            // RSS 取 MiB 整数；CPU 是"占满一个核"的百分比（多核可 > 100）。
            let rss_mb = sample.rss_bytes / 1_048_576;
            let cpu_percent = cpu_percent_between(previous, sample);
            let hit = if max_rss_mb > 0 && rss_mb > u64::from(max_rss_mb) {
                Some(("rss", rss_mb as f64, max_rss_mb))
            } else if max_cpu_percent > 0 && cpu_percent > f64::from(max_cpu_percent) {
                Some(("cpu", cpu_percent, max_cpu_percent))
            } else {
                None
            };

            let triggered = {
                let Some(supervisor) = self.supervisors.get_mut(&task_id) else {
                    continue;
                };
                supervisor.last_limit_sample = Some(sample);
                supervisor.limit_tracker.observe(hit.is_some())
            };
            let Some((kind, value, limit)) = hit.filter(|_| triggered) else {
                continue;
            };

            tracing::warn!(
                event = "task.limit_exceeded",
                task = %task_id,
                kind = kind,
                value = value,
                limit = limit,
                "资源超限，停止实例"
            );
            // 复用健康失败路径：设置 stop_cause/monitor 收敛，再把 StopJob 来源
            // 标成 resource_limit（prepare_stop_job 默认写 "health"）。
            if self.request_stop(&task_id, StopCause::Health, false, now)
                && let Some(mut job) = self.prepare_stop_job(&task_id, now)
            {
                job.reason = "resource_limit";
                stops.push(job);
            }
            self.reset_limit_tracking(&task_id);
        }
        stops
    }

    /// C6：清空资源守卫的基线与连续计数。
    fn reset_limit_tracking(&mut self, task_id: &str) {
        if let Some(supervisor) = self.supervisors.get_mut(task_id) {
            supervisor.limit_tracker.reset();
            supervisor.last_limit_sample = None;
        }
    }

    /// 停止失败/超时：保留 stop_cause 已由 prepare 设置，重排重试并升级观测（B2）。
    pub(crate) fn record_stop_failure(&mut self, task_id: &str, message: &str) {
        let mono_now = Instant::now();
        let runtime_stopped = self
            .runtimes
            .get(task_id)
            .is_none_or(|runtime| runtime.status() == TaskStatus::Stopped);
        let attempt = {
            let Some(supervisor) = self.supervisors.get_mut(task_id) else {
                return;
            };
            if runtime_stopped || supervisor.stop_cause.is_none() {
                return;
            }
            let Some(cause) = supervisor.stop_cause.take() else {
                return;
            };
            supervisor.pending_stop = Some(cause);
            supervisor.stop_retry_count = supervisor.stop_retry_count.saturating_add(1);
            let attempt = supervisor.stop_retry_count;
            supervisor.stop_retry_at = Some(mono_now + stop_retry_delay(attempt));
            if attempt == 3 {
                let _ = supervisor.machine.handle(
                    RestartEvent::NeedsHumanObserved {
                        source: "stop_timeout".to_owned(),
                        message: format!("停止实例连续失败 {attempt} 次，需人工处理：{message}"),
                    },
                    mono_now,
                );
            }
            attempt
        };
        if attempt == 3 {
            // 多次停止失败：升级为 needs_human(stop_timeout) 并继续按退避重试。
            let _ = self.sync_supervisor_state(task_id, mono_now, Timestamp::now());
            tracing::error!(task = %task_id, attempts = attempt, "停止重试升级为 needs_human(stop_timeout)");
            // M3-5b：进程可能仍在运行，必须让人知道（docs/05 §8 高优先级）。
            let text = format!(
                "任务 {} 停止超时（连续 {attempt} 次未退出），进程可能仍在运行：{message}。请检查进程后手动处理。",
                self.task_name(task_id)
            );
            self.raise_notification(task_id, NotificationKind::StopTimeout, text);
        } else if should_log_stop_retry(attempt) {
            tracing::warn!(
                task = %task_id,
                attempt,
                retry_ms = stop_retry_delay(attempt).as_millis(),
                error = %message,
                "健康停止失败，安排重试"
            );
        } else {
            tracing::debug!(task = %task_id, attempt, error = %message, "健康停止重试失败");
        }
    }

    /// 停止完成后执行配置收敛清理（B3/B13）。
    fn finish_reconcile(
        &mut self,
        task_id: &str,
        action: ConfigReconcile,
        heartbeat: Option<PathBuf>,
    ) -> bool {
        match action {
            ConfigReconcile::Keep => false,
            ConfigReconcile::StopAndKeep => {
                // B13：保留配置与 state，只把状态标为 stopped/reason=config_disabled。
                self.restart_pending.remove(task_id);
                if let Some(state) = self.state_task_mut(task_id) {
                    state.last_status = Some(crate::state_store::LastStatus::Stopped);
                    state.last_reason = Some("config_disabled".to_owned());
                }
                tracing::debug!(task = %task_id, "任务已禁用，实例已停止并保留配置/状态");
                true
            }
            ConfigReconcile::StopAndRemove => {
                self.restart_pending.remove(task_id);
                if let Some(heartbeat) = heartbeat
                    && let Err(error) = std::fs::remove_file(&heartbeat)
                    && error.kind() != std::io::ErrorKind::NotFound
                {
                    tracing::warn!(
                        task = %task_id,
                        path = %heartbeat.display(),
                        error = %error,
                        "删除任务后清理心跳文件失败"
                    );
                }
                self.runtimes.remove(task_id);
                self.supervisors.remove(task_id);
                self.state.remove_task(task_id);
                tracing::debug!(task = %task_id, "任务已删除/失效，实例与监督状态已清理（日志保留）");
                true
            }
        }
    }

    /// 把状态机快照同步进 state.json；StartNow 只登记 pending，由调度线程执行。
    fn apply_decision(
        &mut self,
        task_id: &str,
        decision: &Decision,
        mono_now: Instant,
        wall_now: Timestamp,
    ) -> bool {
        let Some(snapshot) = self
            .supervisors
            .get(task_id)
            .map(|supervisor| supervisor.machine.snapshot(mono_now))
        else {
            return false;
        };
        let start_now = matches!(decision, Decision::StartNow { .. });
        let terminating = matches!(decision, Decision::Paused { .. } | Decision::Stopped { .. });
        let stay = matches!(decision, Decision::Stay { .. });
        let previous_reason = self
            .state
            .task(task_id)
            .and_then(|state| state.last_reason.clone());
        let mut changed = false;
        if let Some(state) = self.state_task_mut(task_id) {
            changed |= sync_state(state, &snapshot, mono_now, wall_now);
            // Stay 多为重复/迟到事件：不要把更有诊断价值的 last_reason
            //（如 start_failed:port_check）覆盖成 startup_failure。
            if stay && state.last_reason != previous_reason {
                state.last_reason = previous_reason;
                changed = true;
            }
        }
        if let Some(supervisor) = self.supervisors.get_mut(task_id) {
            if start_now {
                supervisor.pending_start = true;
                supervisor.auto_start_at = None;
            }
            if terminating {
                supervisor.pending_start = false;
                supervisor.auto_start_at = None;
            }
        }
        let reason = decision.reason();
        // Stay 表示没有状态迁移（tick/重复/迟到事件），不写 info；
        // 只记录真正的状态变化，避免同一状态每次 tick 重复刷屏。
        if !stay && reason.as_str() != "tick" {
            tracing::info!(
                event = "task.decision",
                task = %task_id,
                decision = %reason,
                failures = snapshot.consecutive_failures,
                "监督决策"
            );
        }
        // M3-5b：只有"进入需要人介入"的暂停才通知。退避重启、健康失败重启、
        // 用户主动停止/重启都不发；同一状态由通知中心去重，不会重复提醒。
        if let Decision::Paused { reason } = decision {
            let kind = match reason {
                Reason::CrashLoop => Some(NotificationKind::CrashLoop),
                // 窗口内启动过于频繁：不是崩溃计数问题，单独提示"稍后/手动"。
                Reason::RestartBudget => Some(NotificationKind::RestartBudget),
                // mode=never 等"不再自动重启"的暂停：需要人看一眼配置。
                Reason::CrashNoRestart => Some(NotificationKind::StartFailed),
                Reason::NeedsHuman => Some(
                    if snapshot
                        .needs_human
                        .as_ref()
                        .is_some_and(|needs| needs.source == "stop_timeout")
                    {
                        NotificationKind::StopTimeout
                    } else {
                        NotificationKind::NeedsHuman
                    },
                ),
                _ => None,
            };
            if let Some(kind) = kind {
                let message = self.paused_notification_message(task_id, kind, &snapshot);
                self.raise_notification(task_id, kind, message);
            }
        }
        changed
    }

    /// 处理一个运行时事件；返回是否修改了持久化状态。
    pub(crate) fn handle_runtime_event(
        &mut self,
        event: &RuntimeEvent,
        mono_now: Instant,
        wall_now: Timestamp,
    ) -> bool {
        let snapshot = event.snapshot();
        let task_id = snapshot.id.clone();
        if !self.supervisors.contains_key(&task_id) {
            return false;
        }
        match event {
            RuntimeEvent::Starting(snapshot) => {
                // N1：先取运行时快照里的最新实例 id（锁外先读，避免与
                // supervisor 的可变借用冲突），再与 supervisor 状态一起判定。
                let latest = self
                    .runtimes
                    .get(&task_id)
                    .and_then(|runtime| runtime.snapshot().instance_id);
                let accept = self.supervisors.get(&task_id).is_some_and(|supervisor| {
                    starting_instance_accepted(
                        supervisor.instance_id.as_deref(),
                        supervisor.previous_instance_id.as_deref(),
                        latest.as_deref(),
                        snapshot.instance_id.as_deref(),
                        supervisor.expect_start,
                    )
                });
                if !accept {
                    tracing::debug!(
                        task = %task_id,
                        instance = ?snapshot.instance_id,
                        latest = ?latest,
                        "丢弃迟到/非期望的 Starting 事件"
                    );
                    return false;
                }
                if let Some(supervisor) = self.supervisors.get_mut(&task_id) {
                    supervisor.instance_id = snapshot.instance_id.clone();
                    supervisor.expect_start = false;
                    supervisor.monitor = None;
                    supervisor.next_health_at = None;
                    supervisor.stop_cause = None;
                    supervisor.pending_start = false;
                    supervisor.health_startup_failure = false;
                    supervisor.pending_stop = None;
                    supervisor.stop_retry_at = None;
                    supervisor.stop_retry_count = 0;
                    supervisor.readiness = None;
                }
                // runtime snapshot 可能把 last_status 写成 starting；随后以
                // 状态机快照为准，保证 UI 在就绪前看不到 running（B1）。
                let mut changed = self.apply_runtime_snapshot_state(&task_id, snapshot, wall_now);
                changed |= self.sync_supervisor_state(&task_id, mono_now, wall_now);
                changed
            }
            RuntimeEvent::Running {
                instance_id,
                started_instant,
                snapshot,
                ..
            } => {
                let accept = self.supervisors.get(&task_id).is_some_and(|supervisor| {
                    running_instance_accepted(
                        supervisor.instance_id.as_deref(),
                        Some(instance_id.as_str()),
                    ) && supervisor.machine.status() == RestartStatus::Starting
                });
                if !accept {
                    tracing::debug!(
                        task = %task_id,
                        instance = %instance_id,
                        "丢弃非当前实例/重复的 Running 事件"
                    );
                    return false;
                }
                let (readiness, kind) = {
                    let task_config = self
                        .loaded
                        .as_ref()
                        .and_then(|loaded| loaded.config.task(&task_id));
                    let kind = task_config.map_or(HealthKind::Process, |task| task.health.kind);
                    let grace = task_config
                        .map(|task| Duration::from_millis(task.health.startup_grace_ms))
                        .unwrap_or_default();
                    let readiness = readiness_plan(kind, *started_instant, grace);
                    let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                    supervisor.instance_id = Some(instance_id.clone());
                    supervisor.started_instant = Some(*started_instant);
                    supervisor.readiness = Some(readiness);
                    (readiness, kind)
                };
                let monitor_ready = self.start_health_monitor(
                    &task_id,
                    instance_id,
                    snapshot,
                    *started_instant,
                    mono_now,
                );
                if let Some(supervisor) = self.supervisors.get_mut(&task_id) {
                    supervisor.readiness = Some(readiness);
                }
                let mut changed = self.apply_runtime_snapshot_state(&task_id, snapshot, wall_now);
                changed |= self.sync_supervisor_state(&task_id, mono_now, wall_now);

                if !monitor_ready && kind != HealthKind::Process {
                    // 健康配置不完整，无法判断首次健康（配置校验本应拦截）：
                    // 按启动失败收敛，避免永远停在 starting。
                    tracing::warn!(task = %task_id, "健康监视器无法构造，按启动失败处理");
                    changed |= self.request_stop(&task_id, StopCause::Health, true, mono_now);
                }
                changed
            }
            RuntimeEvent::Stopping(snapshot) => {
                if !self.event_instance_current(&task_id, snapshot.instance_id.as_deref()) {
                    return false;
                }
                self.apply_runtime_snapshot_state(&task_id, snapshot, wall_now)
            }
            RuntimeEvent::Stopped(snapshot) => {
                if !self.event_instance_current(&task_id, snapshot.instance_id.as_deref()) {
                    tracing::debug!(
                        task = %task_id,
                        instance = ?snapshot.instance_id,
                        "丢弃迟到/旧实例的 Stopped 事件"
                    );
                    return false;
                }
                let mut changed = self.apply_runtime_snapshot_state(&task_id, snapshot, wall_now);
                let (cause, startup_failure, cleanup, cleanup_heartbeat) = {
                    let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                    let cause = supervisor
                        .stop_cause
                        .take()
                        .or_else(|| supervisor.pending_stop.take());
                    (
                        cause,
                        supervisor.health_startup_failure,
                        supervisor.cleanup_on_stop,
                        supervisor.cleanup_heartbeat.clone(),
                    )
                };
                if let Some(supervisor) = self.supervisors.get_mut(&task_id) {
                    // 用户 Restart 的旧实例退出后还有新实例要启动：保留
                    // expect_start，否则新实例的 Starting 会被当成迟到事件丢弃。
                    let expect_restart = matches!(cause, Some(StopCause::UserRestart));
                    supervisor.monitor = None;
                    supervisor.next_health_at = None;
                    supervisor.clear_instance();
                    supervisor.started_instant = None;
                    supervisor.auto_start_at = None;
                    supervisor.pending_start = false;
                    supervisor.health_startup_failure = false;
                    supervisor.pending_stop = None;
                    supervisor.stop_retry_at = None;
                    supervisor.stop_retry_count = 0;
                    supervisor.expect_start = expect_restart;
                    supervisor.readiness = None;
                    supervisor.restart_count_pending = false;
                    supervisor.cleanup_on_stop = None;
                    supervisor.cleanup_heartbeat = None;
                }
                if cause.is_some() {
                    // 停止流程正常结束：下次停止超时需要重新提醒。
                    self.notifications
                        .clear(&task_id, NotificationKind::StopTimeout);
                }
                // 排障关键状态迁移：退出码 + 停止来源（自然退出时为 exited）。
                // 优雅停止的诊断随运行时报告带出：method / forced / waited_ms。
                let stop_report = self
                    .runtimes
                    .get(&task_id)
                    .and_then(|runtime| runtime.last_stop_report());
                tracing::info!(
                    event = "task.exit",
                    task = %task_id,
                    exit_code = snapshot.exit_code.map(i64::from).unwrap_or(-1),
                    reason = cause.map(StopCause::as_str).unwrap_or("exited"),
                    instance = snapshot.instance_id.as_deref().unwrap_or("-"),
                    method = stop_report
                        .as_ref()
                        .map(|report| report.method.as_str())
                        .unwrap_or("-"),
                    forced = snapshot.forced_kill,
                    waited_ms = stop_report
                        .as_ref()
                        .map(|report| report.waited.as_millis() as i64)
                        .unwrap_or(-1),
                    signal_error = stop_report
                        .as_ref()
                        .and_then(|report| report.signal_error.as_deref())
                        .unwrap_or("-"),
                    "任务实例已退出"
                );
                // 2026-11-04：运行时段停止不写用户 override，但 state.last_reason
                // 需要明确记为 schedule_stop（运行时退出原因只区分 user_stop）。
                if matches!(cause, Some(StopCause::Schedule))
                    && let Some(state) = self.state_task_mut(&task_id)
                    && state.last_reason.as_deref() != Some("schedule_stop")
                {
                    state.last_reason = Some("schedule_stop".to_owned());
                    changed = true;
                }
                // Q3：用户 Stop/Restart 的旧实例退出不参与重启计数；健康停止
                // 区分启动宽限到期（StartFailed）与运行期健康失败（HealthFailed）。
                // M3-6a：配置变更重启的退出同样不计失败、不进退避。
                let machine_event = match cause {
                    Some(StopCause::User | StopCause::UserRestart) => None,
                    Some(
                        StopCause::ConfigDisabled
                        | StopCause::ConfigRemoved
                        | StopCause::ConfigRestart
                        | StopCause::Schedule
                        | StopCause::Shutdown,
                    ) => None,
                    Some(StopCause::Health) if startup_failure => Some(RestartEvent::StartFailed),
                    Some(StopCause::Health) => Some(RestartEvent::HealthFailed),
                    None => Some(RestartEvent::Exited {
                        code: snapshot.exit_code.unwrap_or(0),
                        clean: snapshot.exit_code == Some(0),
                    }),
                };
                if let Some(event) = machine_event {
                    let decision = {
                        let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                        supervisor.machine.handle(event, mono_now)
                    };
                    changed |= self.apply_decision(&task_id, &decision, mono_now, wall_now);
                }
                if let Some(cleanup) = cleanup {
                    changed |= self.finish_reconcile(&task_id, cleanup, cleanup_heartbeat);
                }
                if matches!(cause, Some(StopCause::ConfigRestart)) {
                    // 旧实例已按配置变更退出：用新配置重新启动。StartRequested
                    // 会清零失败计数；禁用/override/paused/needs_human 由调度
                    // 线程在真正启动前再复核一次，不会偷偷拉起。
                    let decision = {
                        let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                        supervisor
                            .machine
                            .handle(RestartEvent::StartRequested, mono_now)
                    };
                    changed |= self.apply_decision(&task_id, &decision, mono_now, wall_now);
                    if let Some(state) = self.state_task_mut(&task_id)
                        && state.last_reason.as_deref() != Some("config_changed")
                    {
                        state.last_reason = Some("config_changed".to_owned());
                        changed = true;
                    }
                }
                // 旧进程已经退出：无论是否重启，restart_pending 都不再成立
                //（下一次启动本来就会用当前配置）。
                if self.restart_pending.remove(&task_id) {
                    changed = true;
                }
                changed
            }
            RuntimeEvent::StartFailed(snapshot) => {
                if !self.event_instance_current(&task_id, snapshot.instance_id.as_deref()) {
                    return false;
                }
                let mut changed = self.apply_runtime_snapshot_state(&task_id, snapshot, wall_now);
                let decision = {
                    let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                    supervisor.monitor = None;
                    supervisor.next_health_at = None;
                    supervisor.clear_instance();
                    supervisor.started_instant = None;
                    supervisor.auto_start_at = None;
                    supervisor.pending_start = false;
                    supervisor.health_startup_failure = false;
                    supervisor.pending_stop = None;
                    supervisor.stop_retry_at = None;
                    supervisor.stop_retry_count = 0;
                    supervisor.expect_start = false;
                    supervisor.readiness = None;
                    supervisor.restart_count_pending = false;
                    supervisor
                        .machine
                        .handle(RestartEvent::StartFailed, mono_now)
                };
                changed |= self.apply_decision(&task_id, &decision, mono_now, wall_now);
                // 启动失败已回滚到 stopped：没有旧进程在跑，待重启标记失去意义。
                if self.restart_pending.remove(&task_id) {
                    changed = true;
                }
                changed
            }
        }
    }

    /// 启动失败（spawn/端口/日志/心跳目录等）：failure++ 并进入 backoff/熔断。
    pub(crate) fn apply_start_failure(&mut self, task_id: &str, failure: StartFailure) -> bool {
        let mono_now = Instant::now();
        let wall_now = Timestamp::now();
        let Some(_) = self.supervisors.get(task_id) else {
            return false;
        };
        let decision = {
            let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
            supervisor.monitor = None;
            supervisor.next_health_at = None;
            supervisor.clear_instance();
            supervisor.started_instant = None;
            supervisor.auto_start_at = None;
            supervisor.pending_start = false;
            supervisor.stop_cause = None;
            supervisor.health_startup_failure = false;
            supervisor.pending_stop = None;
            supervisor.stop_retry_at = None;
            supervisor.stop_retry_count = 0;
            supervisor.expect_start = false;
            supervisor.readiness = None;
            supervisor.restart_count_pending = false;
            supervisor.cleanup_on_stop = None;
            supervisor.cleanup_heartbeat = None;
            supervisor
                .machine
                .handle(RestartEvent::StartFailed, mono_now)
        };
        let mut changed = self.apply_decision(task_id, &decision, mono_now, wall_now);
        if let Some(state) = self.state_task_mut(task_id) {
            let reason = format!("start_failed:{}", failure.stage);
            if state.last_reason.as_deref() != Some(reason.as_str()) {
                state.last_reason = Some(reason);
                changed = true;
            }
        }
        tracing::warn!(
            event = "task.start_failed",
            task = %task_id,
            stage = failure.stage,
            message = %failure.message,
            "任务启动失败，进入退避/熔断"
        );
        changed
    }

    /// 为任务生成一条通知（`notify=false` 或任务已从配置删除时静默）。
    ///
    /// 只入内存队列，由释放 Engine 锁之后的 [`flush_notifications`] 投递；
    /// 绝不在锁内 emit，也不打开/聚焦窗口。
    fn raise_notification(
        &mut self,
        task_id: &str,
        kind: NotificationKind,
        message: String,
    ) -> bool {
        let Some((id, name, notify)) = self
            .loaded
            .as_ref()
            .and_then(|loaded| loaded.config.task(task_id))
            .map(|task| (task.id.clone(), task.name.clone(), task.notify))
        else {
            return false;
        };
        let notice = TaskNotice {
            id: &id,
            name: &name,
            notify,
        };
        self.notifications
            .raise(Instant::now(), now_millis(), notice, kind, message)
    }

    /// 任务显示名（配置里没有该任务时退回 id）。
    fn task_name(&self, task_id: &str) -> String {
        self.loaded
            .as_ref()
            .and_then(|loaded| loaded.config.task(task_id))
            .map_or_else(|| task_id.to_owned(), |task| task.name.clone())
    }

    /// 暂停类通知的文案（docs/05 §7.3：发生了什么 + 影响 + 下一步）。
    fn paused_notification_message(
        &self,
        task_id: &str,
        kind: NotificationKind,
        snapshot: &RestartSnapshot,
    ) -> String {
        let name = self.task_name(task_id);
        let detail = snapshot
            .needs_human
            .as_ref()
            .map(|needs| needs.message.clone())
            .or_else(|| {
                self.state
                    .task(task_id)
                    .and_then(|state| state.last_reason.clone())
            })
            .unwrap_or_else(|| "原因未记录".to_owned());
        match kind {
            NotificationKind::CrashLoop => format!(
                "任务 {name} 连续失败 {} 次，已暂停自动重启。请检查日志后手动启动。",
                snapshot.consecutive_failures
            ),
            NotificationKind::RestartBudget => format!(
                "任务 {name} 在重启时间窗内启动次数超过上限，已暂停自动重启。请检查崩溃原因后手动启动（手动启动会重新计数）。"
            ),
            NotificationKind::StartFailed => format!(
                "任务 {name} 启动失败且不会自动重启（{detail}）。请检查命令/端口后手动启动。"
            ),
            NotificationKind::NeedsHuman => format!(
                "任务 {name} 需要人工处理：{detail}。自动重启已暂停；脚本恢复后会自动继续监督，也可在任务上点「重启」立即重试。"
            ),
            NotificationKind::StopTimeout => {
                format!("任务 {name} 停止超时，进程可能仍在运行：{detail}。请检查进程后手动处理。")
            }
            NotificationKind::ConfigError => String::new(),
        }
    }

    /// 配置结构错误 → 通知一次（指纹去重；恢复后清空以便再次损坏时重发）。
    fn note_config_error(&mut self) {
        let Some(error) = self.snapshot.config_error.as_ref() else {
            self.notifications.clear_config_error();
            return;
        };
        let fingerprint = format!(
            "{}:{}:{}:{}",
            error.file, error.line, error.col, error.message
        );
        let message = format!(
            "配置文件加载失败：{}:{}:{} {}。正在运行的任务未受影响，请修正后重新加载。",
            error.file, error.line, error.col, error.message
        );
        self.notifications
            .raise_config_error(Instant::now(), now_millis(), &fingerprint, message);
    }

    /// 取出待投递的通知（调用方必须在释放 Engine 锁之后调用）。
    pub(crate) fn drain_notifications(&mut self) -> Vec<RaisedNotification> {
        self.notifications.drain()
    }

    /// 任务级 `power.resume_grace_ms`（配置缺失/任务已删除时用配置默认值）。
    fn resume_grace(&self, task_id: &str) -> Duration {
        self.loaded
            .as_ref()
            .and_then(|loaded| loaded.config.task(task_id))
            .map(|task| Duration::from_millis(task.power.resume_grace_ms))
            .unwrap_or_else(|| Duration::from_millis(PowerConfig::default().resume_grace_ms))
    }

    /// 收到 suspend：冻结所有实例的健康评估（探测不再产生失败计数）。
    ///
    /// 真正的"冻结"由调度线程跳过本轮工作完成；这里只把每个监视器标记为
    /// suspended，并递增基线代际，丢弃冻结前在途的探测结果。
    fn handle_power_suspend(&mut self, now: Instant, wall: SystemTime) -> bool {
        if !self.power.on_suspend(now, wall) {
            return false;
        }
        self.power_epoch = self.power_epoch.wrapping_add(1);
        tracing::warn!("系统进入休眠：冻结健康探测、退避与自动启动计时（睡眠不计失败）");
        for supervisor in self.supervisors.values_mut() {
            if let Some(monitor) = supervisor.monitor.as_mut() {
                monitor.suspend(now);
            }
        }
        false
    }

    /// 收到 resume：平移全部计时器、重新给健康宽限，并恢复监视器。
    ///
    /// 不启动任何进程：`auto_start_at` 只是平移，到期后仍要经过
    /// [`auto_start_permitted`](Self::auto_start_permitted) 过滤，
    /// paused / needs_human / override 任务不会被唤醒顺带拉起。
    fn handle_power_resume(&mut self, now: Instant, wall: SystemTime) -> bool {
        let wake = self.power.on_resume(now, wall);
        self.power_epoch = self.power_epoch.wrapping_add(1);
        tracing::warn!(
            slept_secs = wake.slept.as_secs_f64(),
            frozen_secs = wake.mono_elapsed.as_secs_f64(),
            "系统从休眠恢复：平移被冻结的计时器并按 power.resume_grace_ms 重新给健康宽限"
        );

        let wall_now = Timestamp::now();
        let ids: Vec<String> = self.supervisors.keys().cloned().collect();
        let mut changed = false;
        for task_id in ids {
            let grace = self.resume_grace(&task_id);
            let Some(supervisor) = self.supervisors.get_mut(&task_id) else {
                continue;
            };
            // 睡眠期间单调计时器可能停走（QPC 不跨 S3）或照常走，两种情况下
            // 都按"实际走过的单调时长"平移，剩余时间保持不变。
            supervisor.started_instant = shift_timer(supervisor.started_instant, wake.mono_elapsed);
            supervisor.next_health_at =
                resume_deadline(supervisor.next_health_at, wake.mono_elapsed, now, grace);
            supervisor.auto_start_at = shift_timer(supervisor.auto_start_at, wake.mono_elapsed);
            supervisor.stop_retry_at = shift_timer(supervisor.stop_retry_at, wake.mono_elapsed);
            if let Some(plan) = supervisor.readiness.as_mut() {
                // 启动宽限同样冻结，并额外给一轮 resume 宽限（睡眠不算启动失败）。
                // 截止时间再加一个探测间隔：保证宽限后至少有一次探测结果可用，
                // 不会在同一 tick 里"宽限到期 + 探测还没跑"就判启动失败。
                plan.deadline = resume_deadline(
                    plan.deadline,
                    wake.mono_elapsed,
                    now,
                    grace + supervisor.health_interval,
                );
            }
            supervisor.machine.shift_timers(wake.mono_elapsed);
            if let Some(monitor) = supervisor.monitor.as_mut() {
                monitor.resume(now);
            }
            // 在途探测在睡眠期间被丢弃时不会留下排期：这里按唤醒时刻补一次。
            // 该排期不参与上面的平移（它本来就是唤醒之后才产生的）。
            if supervisor.next_health_at.is_none() && supervisor.monitor.is_some() {
                supervisor.next_health_at = Some(now + supervisor.health_interval);
            }
            changed |= self.sync_supervisor_state(&task_id, now, wall_now);
        }
        tracing::info!(
            tasks = self.supervisors.len(),
            "休眠恢复后的计时器已平移，健康宽限重新起算"
        );
        changed
    }

    /// 检测到时钟跳变：只重置心跳新鲜度基线并给宽限，不触发重启。
    fn handle_clock_jump(&mut self, now: Instant, jump: ClockJump) -> bool {
        self.power_epoch = self.power_epoch.wrapping_add(1);
        tracing::warn!(
            direction = jump.direction.as_str(),
            magnitude_secs = jump.magnitude.as_secs_f64(),
            signed_secs = jump.signed_seconds(),
            "检测到系统时钟跳变：重置心跳新鲜度基线并给健康宽限（不触发重启）"
        );

        let ids: Vec<String> = self.supervisors.keys().cloned().collect();
        for task_id in ids {
            let grace = self.resume_grace(&task_id);
            let Some(supervisor) = self.supervisors.get_mut(&task_id) else {
                continue;
            };
            // 跳变不改变单调时钟，计时器不平移；只保证宽限期内不判失败。
            supervisor.next_health_at =
                resume_deadline(supervisor.next_health_at, Duration::ZERO, now, grace);
            if let Some(plan) = supervisor.readiness.as_mut() {
                plan.deadline = resume_deadline(
                    plan.deadline,
                    Duration::ZERO,
                    now,
                    grace + supervisor.health_interval,
                );
            }
            if let Some(monitor) = supervisor.monitor.as_mut() {
                // 心跳 last_change 以当前时刻重新起算，避免旧心跳被误判过期/新鲜。
                monitor.reset_observation(now);
            }
        }
        // 计时器未变（state.json 无字段变化），不需要落盘/广播。
        false
    }

    /// 当前电源/时钟基线快照（引擎 epoch + 回调侧 generation）。
    ///
    /// 在途探测在取出时记录一份，回锁后与最新值比较；回调 generation 由
    /// 系统线程递增，无需调度线程先处理事件即可发现"探测期间休眠/唤醒"。
    fn power_stamp(&self) -> PowerStamp {
        PowerStamp {
            epoch: self.power_epoch,
            generation: self.power_wiring.generation(),
        }
    }

    /// 任务当前运行时段 key（schedule 未启用/不在窗口时 None）。
    fn current_schedule_key(&self, task_id: &str) -> Option<String> {
        let loaded = self.loaded.as_ref()?;
        let task = loaded.config.task(task_id)?;
        if !task.schedule.enabled {
            return None;
        }
        let plan = Schedule {
            windows: task.schedule.windows.clone(),
        };
        plan.window_key(now_local())
    }

    /// 用户手动启动/停止后同步运行时段状态：
    /// - 手动停止：记下当前窗口 key，本窗口内不再自动拉起；
    /// - 手动启动/重启：清掉抑制（本次运行不再受窗口结束豁免——2026-11-05
    ///   起离开窗口无条件停止任何存活实例，不再区分谁启动）。
    fn observe_manual_schedule_action(&mut self, task_id: &str, action: Action) {
        let key = self.current_schedule_key(task_id);
        // 2026-11-05：用户显式手动启动过的任务，窗口外仍允许监督器自动重启
        //（“窗口外手动启动不用管”）；手动 Stop 撤销该例外。是否受窗口约束由
        // [`Engine::supervisor_autostart_allowed`] 按当前 schedule.enabled 判断。
        match action {
            Action::Start | Action::Restart => {
                self.manual_started_schedule.insert(task_id.to_owned());
            }
            Action::Stop => {
                self.manual_started_schedule.remove(task_id);
            }
        }
        if let Some(state) = self.state_task_mut(task_id) {
            match action {
                Action::Stop => {
                    state.schedule_suppressed = key;
                }
                Action::Start | Action::Restart => {
                    state.schedule_suppressed = None;
                }
            }
        }
    }

    /// 监督器自动启动的“唯一时钟”门（2026-11-05）。
    ///
    /// schedule.enabled 的任务只允许在窗口内、或用户显式手动启动过时由监督器
    /// 自动拉起（pending_start / 退避重试 / auto_start_at / needs_human 恢复）；
    /// 窗口外且无用户启动意图一律取消，避免应用在窗口外自动开工。判断本身是
    /// [`may_supervisor_autostart`] 纯函数，便于单测。
    fn supervisor_autostart_allowed(&self, task_id: &str) -> bool {
        let Some(loaded) = self.loaded.as_ref() else {
            return true;
        };
        let Some(task) = loaded.config.task(task_id) else {
            return true;
        };
        may_supervisor_autostart(
            task.schedule.enabled,
            self.current_schedule_key(task_id).is_some(),
            self.manual_started_schedule.contains(task_id),
        )
    }

    /// 每周运行时段调度（2026-11-05 架构修正：一个时钟、一个开关）。
    ///
    /// - 进入窗口（out→in / window_key 变化 / 启动后首次观测在窗口内）：未被
    ///   本窗口用户 Stop 抑制且自动启动许可通过时，按 `schedule_start` 启动一次；
    /// - 离开窗口（in→out，含休眠唤醒后的首次观测、并集结束）：停止该任务任何
    ///   存活实例（Starting/Running/Stopping 都算），不区分谁启动、不因
    ///   needs_human 跳过；`last_reason=schedule_stop`、不写用户 override；
    /// - 窗口外稳态：不停止任何实例；本进程首次观测已在窗口外时也不停。
    ///
    /// 配置里的 `start_in_window`/`stop_outside` 只保留兼容，这里完全忽略。
    /// 决策本身在 [`crate::schedule::window::plan_window_tick`]（纯逻辑、可单测）。
    fn schedule_work(&mut self, now: Instant, wall_now: Timestamp, work: &mut SchedulerWork) {
        let tasks = {
            let Some(loaded) = self.loaded.as_ref() else {
                return;
            };
            loaded
                .config
                .tasks
                .iter()
                .filter(|task| loaded.is_task_valid(&task.id) && task.schedule.enabled)
                .map(|task| (task.id.clone(), task.schedule.clone()))
                .collect::<Vec<_>>()
        };
        // schedule.enabled 被关闭/任务失效后丢弃内存观测，避免“陈旧 in_window”
        // 在重新启用时被误判为离开窗口而停掉窗口外手动启动的实例。
        let enabled_ids: HashSet<String> = tasks.iter().map(|(id, _)| id.clone()).collect();
        self.schedule_states
            .retain(|id, _| enabled_ids.contains(id));

        let now_local = now_local();
        for (task_id, schedule) in tasks {
            if self.gate.is_in_flight(&task_id) {
                continue;
            }
            let previous = self.schedule_states.get(&task_id).cloned();
            let (persisted_window, suppressed_key, user_stopped) = self
                .state
                .task(&task_id)
                .map(|state| {
                    (
                        state.schedule_window.clone(),
                        state.schedule_suppressed.clone(),
                        state.desired_override == Some(DesiredOverride::Stopped),
                    )
                })
                .unwrap_or((None, None, false));
            let machine_stopped = self
                .supervisors
                .get(&task_id)
                .is_some_and(|supervisor| supervisor.machine.status() == RestartStatus::Stopped);
            let instance_running = self.runtimes.get(&task_id).is_some_and(|runtime| {
                matches!(
                    runtime.status(),
                    TaskStatus::Starting | TaskStatus::Running | TaskStatus::Stopping
                )
            });
            // 进入新窗口会清除上一次窗口残留的 Stop override；其余自动启动条件
            // （enabled / paused / needs_human）仍由 schedule_start_permitted 把关。
            let auto_start_permitted = self.schedule_start_permitted(&task_id);
            let tick = plan_window_tick(WindowTickInput {
                windows: &schedule.windows,
                now: now_local,
                previous: previous.as_ref(),
                persisted_window: persisted_window.as_deref(),
                suppressed_key: suppressed_key.as_deref(),
                user_stopped,
                auto_start_permitted,
                machine_stopped,
                instance_running,
            });

            // 回写内存观测状态（纯逻辑输出，Engine 只负责保存）。
            self.schedule_states
                .insert(task_id.clone(), tick.runtime.clone());

            // 持久化 state.schedule_window：进入窗口写 key，离开窗口/窗口外清空。
            match &tick.persisted {
                PersistedWindowChange::Keep => {}
                PersistedWindowChange::Set(key) => {
                    if let Some(state) = self.state_task_mut(&task_id)
                        && state.schedule_window.as_deref() != Some(key.as_str())
                    {
                        state.schedule_window = Some(key.clone());
                        work.changed = true;
                    }
                }
                PersistedWindowChange::Clear => {
                    if let Some(state) = self.state_task_mut(&task_id)
                        && state.schedule_window.take().is_some()
                    {
                        work.changed = true;
                    }
                }
            }
            // 进入新窗口：清掉上一窗口残留的用户 Stop 抑制/意图。
            if tick.clear_stale_suppression
                && let Some(state) = self.state_task_mut(&task_id)
                && state.schedule_suppressed.take().is_some()
            {
                work.changed = true;
            }
            if tick.clear_user_stop
                && let Some(state) = self.state_task_mut(&task_id)
                && state.desired_override == Some(DesiredOverride::Stopped)
            {
                state.desired_override = None;
                work.changed = true;
                tracing::info!(
                    task = %task_id,
                    window = tick.window_key.as_deref().unwrap_or("-"),
                    "进入新的运行时段，清除上一次的用户停止意图"
                );
            }

            match tick.action {
                WindowAction::Start => {
                    let key = tick.window_key.clone().unwrap_or_default();
                    let Some(job) =
                        self.prepare_start_job(&task_id, now, false, StartReason::ScheduleStart)
                    else {
                        tracing::info!(
                            task = %task_id,
                            window = %key,
                            "运行时段已到，但监督状态/配置不允许自动启动"
                        );
                        continue;
                    };
                    let decision = {
                        let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                        supervisor.machine.handle(RestartEvent::StartRequested, now)
                    };
                    work.changed |= self.apply_decision(&task_id, &decision, now, wall_now);
                    if let Some(supervisor) = self.supervisors.get_mut(&task_id) {
                        supervisor.pending_start = false;
                        supervisor.auto_start_at = None;
                    }
                    work.starts.push(job);
                    work.changed = true;
                    tracing::info!(
                        event = "schedule.start",
                        task = %task_id,
                        window = %key,
                        "运行时段开始，自动启动任务"
                    );
                }
                WindowAction::Stop => {
                    // 无条件停止：不区分谁启动、不因 needs_human 跳过。
                    let decision = {
                        let supervisor = self.supervisors.get_mut(&task_id).expect("已检查存在");
                        supervisor
                            .machine
                            .handle(RestartEvent::ScheduleStopRequested, now)
                    };
                    work.changed |= self.apply_decision(&task_id, &decision, now, wall_now);
                    if self.request_stop(&task_id, StopCause::Schedule, false, now)
                        && let Some(job) = self.prepare_stop_job(&task_id, now)
                    {
                        work.stops.push(job);
                        work.changed = true;
                        tracing::info!(
                            event = "schedule.stop",
                            task = %task_id,
                            "运行时段结束，停止运行中的实例"
                        );
                    }
                    // 窗口结束即本次“工作时段”结束：清掉“用户手动启动过”的例外。
                    // 否则同一应用会话里，之后的事件（配置热重载/needs_human 恢复等）
                    // 可能凭陈旧标记在窗口外把任务重新拉起，违背唯一时钟。
                    self.manual_started_schedule.remove(&task_id);
                }
                WindowAction::None => {}
            }
        }
    }

    /// 调度线程一轮：先处理电源/时钟事件，再处理 Tick（退避/stable）、自动启动
    /// 到期、pending 启动与健康探测。
    pub(crate) fn scheduler_work(&mut self, now: Instant) -> SchedulerWork {
        // B1：退出流程开始后不再自动拉起/探测/重排任何任务；停止由退出流程在锁外执行。
        if self.shutting_down {
            return SchedulerWork::default();
        }
        let wall_now = Timestamp::now();
        // 至少每天清理一次过期日志（运行中生效；失败只记录，不影响调度）。
        self.maybe_cleanup_expired_logs(now);
        let mut work = SchedulerWork::default();

        // 0) M3-6b：先处理电源/时钟事件，再决定本轮是否推进计时器。
        //    顺序必须是 suspend → resume → tick：resume 会重设 tick 基线，
        //    否则第一轮 tick 会把整段睡眠误判成时钟跳变。
        //
        //    每个事件用回调侧捕获的时刻，而不是同一个 `now`：系统睡眠时调度
        //    线程通常一起挂起，唤醒后一轮里同时取到 suspend+resume，若共用
        //    `now`，`on_resume` 的单调增量恒为 0，已过期的退避/自动启动不会
        //    平移，会立刻误重启（QA-M3.6b 阻断 2）。
        let wall_system = SystemTime::now();
        let signals = self.power_wiring.take_signals();
        if let Some(suspend_at) = signals.suspend {
            work.changed |= self.handle_power_suspend(suspend_at.mono, suspend_at.wall);
        }
        if let Some(resume_at) = signals.resume {
            work.changed |= self.handle_power_resume(resume_at.mono, resume_at.wall);
        }
        if let Some(jump) = self.power.on_tick(now, wall_system) {
            work.changed |= self.handle_clock_jump(now, jump);
        }
        if self.power.is_suspended() {
            // 冻结：不探测、不退避到期、不自动启动、不算启动宽限超时。
            tracing::debug!("系统休眠中，本轮调度冻结");
            return work;
        }

        // 1) 状态机 Tick：只在退避到期或 stable_after 到期时调用。
        let ids: Vec<String> = self.supervisors.keys().cloned().collect();
        for task_id in &ids {
            let due = self
                .supervisors
                .get(task_id)
                .is_some_and(|supervisor| machine_tick_due(&supervisor.machine, now));
            if !due {
                continue;
            }
            let decision = {
                let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
                supervisor.machine.handle(RestartEvent::Tick, now)
            };
            work.changed |= self.apply_decision(task_id, &decision, now, wall_now);
        }

        // 1b) B1：启动宽限到期。
        // - process：进程仍存活 → 就绪（Q10，无 300ms 特例）；
        // - heartbeat/http：宽限内仍未首次健康 → 启动失败（未就绪）。
        for task_id in &ids {
            let exhausted = self.supervisors.get(task_id).is_some_and(|supervisor| {
                supervisor.machine.status() == RestartStatus::Starting
                    && supervisor.stop_cause.is_none()
                    && supervisor.pending_stop.is_none()
                    && supervisor
                        .readiness
                        .is_some_and(|plan| readiness_deadline_reached(plan, now))
            });
            if !exhausted {
                continue;
            }
            let runtime_running = self
                .runtimes
                .get(task_id)
                .is_some_and(|runtime| runtime.status() == TaskStatus::Running);
            if !runtime_running {
                continue;
            }
            let await_healthy = self
                .supervisors
                .get(task_id)
                .and_then(|supervisor| supervisor.readiness)
                .is_some_and(|plan| plan.await_first_healthy);
            if await_healthy {
                tracing::warn!(task = %task_id, "启动宽限到期仍未首次健康，按启动失败处理");
                work.changed |= self.request_stop(task_id, StopCause::Health, true, now);
                if let Some(job) = self.prepare_stop_job(task_id, now) {
                    work.stops.push(job);
                    work.changed = true;
                }
            } else {
                work.changed |= self.mark_instance_started(task_id, now, wall_now);
            }
        }

        // 1c) 每周运行时段（2026-11-04）：进入窗口自动启动、离开窗口自动停止。
        self.schedule_work(now, wall_now, &mut work);

        // 2) 自动启动排期到期（错峰/启动时计划）。
        for task_id in &ids {
            let due = self
                .supervisors
                .get(task_id)
                .and_then(|supervisor| supervisor.auto_start_at)
                .is_some_and(|at| at <= now);
            if !due {
                continue;
            }
            if !self.supervisor_autostart_allowed(task_id) {
                // 2026-11-05：窗口外且用户没有显式手动启动过 → 不得自动开工。
                tracing::info!(
                    task = %task_id,
                    "运行时段外且无用户启动意图，取消自动启动排期"
                );
                if let Some(supervisor) = self.supervisors.get_mut(task_id) {
                    supervisor.auto_start_at = None;
                }
                continue;
            }
            if !self.auto_start_permitted(task_id, true) {
                tracing::info!(task = %task_id, "自动启动排期到期，但当前配置/意图已不允许，取消");
                if let Some(supervisor) = self.supervisors.get_mut(task_id) {
                    supervisor.auto_start_at = None;
                }
                continue;
            }
            let stopped = self
                .supervisors
                .get(task_id)
                .is_some_and(|supervisor| supervisor.machine.status() == RestartStatus::Stopped);
            if !stopped {
                if let Some(supervisor) = self.supervisors.get_mut(task_id) {
                    supervisor.auto_start_at = None;
                }
                continue;
            }
            let Some(job) = self.prepare_start_job(task_id, now, false, StartReason::Autostart)
            else {
                if let Some(supervisor) = self.supervisors.get_mut(task_id) {
                    supervisor.auto_start_at = Some(now + SCHEDULER_TICK);
                }
                continue;
            };
            let decision = {
                let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
                supervisor.machine.handle(RestartEvent::StartRequested, now)
            };
            work.changed |= self.apply_decision(task_id, &decision, now, wall_now);
            if let Some(supervisor) = self.supervisors.get_mut(task_id) {
                supervisor.pending_start = false;
                supervisor.auto_start_at = None;
            }
            work.starts.push(job);
            work.changed = true;
        }

        // 3) 状态机已要求立即启动（退避到期/健康重启后的 StartNow）。
        for task_id in &ids {
            let pending = self
                .supervisors
                .get(task_id)
                .is_some_and(|supervisor| supervisor.pending_start);
            if !pending {
                continue;
            }
            if !self.supervisor_autostart_allowed(task_id) {
                // 2026-11-05：窗口外且用户没有显式手动启动过 → 取消监督器重启，
                // 并把状态机收敛回 stopped（退避重试/needs_human 恢复同此路径）。
                tracing::info!(
                    task = %task_id,
                    "运行时段外且无用户启动意图，取消监督器自动重启"
                );
                let decision = {
                    let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
                    supervisor.pending_start = false;
                    supervisor.machine.handle(RestartEvent::StopRequested, now)
                };
                work.changed |= self.apply_decision(task_id, &decision, now, wall_now);
                continue;
            }
            if !self.auto_start_permitted(task_id, false) {
                tracing::info!(task = %task_id, "自动重启待执行，但用户意图/配置已不允许，取消");
                let decision = {
                    let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
                    supervisor.pending_start = false;
                    supervisor.machine.handle(RestartEvent::StopRequested, now)
                };
                work.changed |= self.apply_decision(task_id, &decision, now, wall_now);
                continue;
            }
            let Some(job) = self.prepare_start_job(task_id, now, true, StartReason::Restart) else {
                continue;
            };
            if let Some(supervisor) = self.supervisors.get_mut(task_id) {
                supervisor.pending_start = false;
            }
            work.starts.push(job);
            work.changed = true;
        }

        // 4) pending_stop（健康失败/配置禁用/删除）：退避到期后重新 stop。
        for task_id in &ids {
            let pending = self.supervisors.get(task_id).is_some_and(|supervisor| {
                supervisor.pending_stop.is_some()
                    && supervisor.stop_retry_at.is_none_or(|at| now >= at)
            });
            if !pending || self.gate.is_in_flight(task_id) {
                continue;
            }
            if let Some(job) = self.prepare_stop_job(task_id, now) {
                work.stops.push(job);
                work.changed = true;
            }
        }

        // 5) 健康探测到期：取出监视器，锁外执行（Engine 锁不被 HTTP/文件 IO 阻塞）。
        for task_id in &ids {
            let due = self.supervisors.get(task_id).is_some_and(|supervisor| {
                probe_due(supervisor.next_health_at, now) && supervisor.monitor.is_some()
            });
            if !due {
                continue;
            }
            let taken = {
                let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
                supervisor.next_health_at = None;
                supervisor
                    .monitor
                    .take()
                    .map(|monitor| (supervisor.instance_id.clone(), monitor))
            };
            let Some((Some(instance_id), monitor)) = taken else {
                continue;
            };
            work.probes.push(ProbeJob {
                task_id: task_id.clone(),
                instance_id,
                monitor,
                capture: ProbeCapture {
                    stamp: self.power_stamp(),
                    started_mono: now,
                    started_wall: SystemTime::now(),
                },
            });
        }

        // 6) C6：资源守卫（放在正常探测/启停排期之后，不影响上面的决策）。
        let limit_stops = self.collect_limit_stops(now);
        if !limit_stops.is_empty() {
            work.stops.extend(limit_stops);
            work.changed = true;
        }

        work
    }

    /// 准备一次启动（自动启动/退避重启）：登记 in-flight 并按当前配置重建 runtime。
    ///
    /// `reason` 决定注入的 `CAMPUS_SENTRY_START_REASON`（schedule_start / autostart /
    /// restart）；手动路径在 [`Engine::begin_command`] 里直接用 user_start。
    fn prepare_start_job(
        &mut self,
        task_id: &str,
        _now: Instant,
        auto_restart: bool,
        reason: StartReason,
    ) -> Option<StartJob> {
        if !self.supervisors.contains_key(task_id) {
            return None;
        }
        let runtime = self.runtimes.get(task_id)?.clone();
        if runtime.status() != TaskStatus::Stopped {
            return None;
        }
        let spec = {
            let loaded = self.loaded.as_ref()?;
            if !loaded.is_task_valid(task_id) {
                return None;
            }
            let task = loaded.config.task(task_id)?;
            let mut spec =
                TaskSpec::from_config_with_paths(task, &loaded.data_dir, &loaded.config_path);
            spec.instance.start_reason = reason;
            spec.instance.window_end = schedule_window_end(task);
            spec
        };
        let context = spec.instance.clone();
        let generation = self.gate.try_begin_command(task_id)?;
        let fresh = Arc::new(TaskRuntime::with_events(spec, self.events.clone()));
        self.runtimes.insert(task_id.to_owned(), Arc::clone(&fresh));
        let supervisor = self.supervisors.get_mut(task_id)?;
        supervisor.monitor = None;
        supervisor.next_health_at = None;
        supervisor.clear_instance();
        supervisor.started_instant = None;
        supervisor.stop_cause = None;
        supervisor.health_startup_failure = false;
        supervisor.pending_stop = None;
        supervisor.stop_retry_at = None;
        supervisor.stop_retry_count = 0;
        supervisor.readiness = None;
        supervisor.expect_start = true;
        // N6：自动重启只有就绪（Started）后才计入 total_restarts。
        supervisor.restart_count_pending = auto_restart;
        // C6：新实例 = 新的采样基线，连续超限计数从 0 开始。
        supervisor.limit_tracker.reset();
        supervisor.last_limit_sample = None;
        Some(StartJob {
            task_id: task_id.to_owned(),
            runtime: fresh,
            context,
            generation,
        })
    }

    /// 应用一次健康探测结果；可能返回需要执行的停止任务（健康失败）。
    pub(crate) fn apply_probe(
        &mut self,
        task_id: &str,
        instance_id: &str,
        monitor: HealthMonitor,
        verdict: HealthVerdict,
        capture: ProbeCapture,
        mono_now: Instant,
    ) -> (bool, Option<StopJob>) {
        let wall_now = Timestamp::now();
        let Some(interval) = self
            .supervisors
            .get(task_id)
            .map(|supervisor| supervisor.health_interval)
        else {
            return (false, None);
        };
        // M3-6b：探测在锁外执行期间系统可能休眠/唤醒/跳时钟。回调侧 generation
        // 与引擎 epoch 任一变化、或当前处于冻结，都说明这轮结果属于旧基线，
        // 直接丢弃——否则睡眠期间的"心跳过期"会被记成健康失败并触发停止。
        let suspended = self.power.is_suspended() || self.power_wiring.is_suspended();
        // 时钟跳变没有回调：用探测开始/结束两端的墙钟与单调样本再判一次。
        let probe_span_jump = clock_skew_between(
            capture.started_mono,
            capture.started_wall,
            mono_now,
            SystemTime::now(),
            CLOCK_JUMP_THRESHOLD,
        );
        if capture.stamp.is_stale_from(self.power_stamp(), suspended) || probe_span_jump.is_some() {
            tracing::debug!(task = %task_id, "丢弃跨休眠/时钟跳变的健康探测结果");
            // 电源事件还没被调度线程处理时不要在这里排期：唤醒处理会按
            // `resume_at` 重新给宽限，这里先排一次反而会被随后的计时器平移
            // 再推后一个睡眠时长，等于醒来后很久不再探测。
            let defer_to_power = suspended || self.power_wiring.pending();
            // 监视器只在实例仍是当前、且没有正在停止时才放回；否则随旧实例丢弃，
            // 避免把过期监视器挂到新实例上。
            if let Some(supervisor) = self.supervisors.get_mut(task_id)
                && instance_matches(supervisor.instance_id.as_deref(), Some(instance_id))
                && supervisor.stop_cause.is_none()
                && supervisor.pending_stop.is_none()
                && supervisor.monitor.is_none()
            {
                supervisor.monitor = Some(monitor);
                supervisor.next_health_at = if defer_to_power {
                    None
                } else {
                    Some(mono_now + interval)
                };
            }
            return (false, None);
        }
        let Some(supervisor) = self.supervisors.get(task_id) else {
            return (false, None);
        };
        if supervisor.pending_stop.is_some() || supervisor.stop_cause.is_some() {
            // 已在停止/重试流程中，忽略本轮探测结果。
            return (false, None);
        }
        if !instance_matches(supervisor.instance_id.as_deref(), Some(instance_id))
            || supervisor.monitor.is_some()
        {
            tracing::debug!(task = %task_id, "丢弃非当前实例的健康探测结果");
            return (false, None);
        }
        let running = self.runtimes.get(task_id).is_some_and(|runtime| {
            matches!(runtime.status(), TaskStatus::Starting | TaskStatus::Running)
        });
        if !running {
            return (false, None);
        }
        let mut changed = false;

        if verdict.state_changed {
            tracing::info!(
                event = "health.change",
                task = %task_id,
                health = verdict.state.as_str(),
                failures = verdict.consecutive_failures,
                "健康状态变化"
            );
        }

        // needs_human：置位/恢复都只迁移一次；存活进程不停止（Q7/Q8）。
        if verdict.needs_human_changed {
            let event = match &verdict.needs_human {
                Some(status) => RestartEvent::NeedsHumanObserved {
                    source: "heartbeat".to_owned(),
                    message: format!("心跳状态 {status} 需要人工处理"),
                },
                None => RestartEvent::NeedsHumanCleared,
            };
            let decision = {
                let supervisor = self.supervisors.get_mut(task_id).expect("已检查存在");
                supervisor.machine.handle(event, mono_now)
            };
            changed |= self.apply_decision(task_id, &decision, mono_now, wall_now);
            match &verdict.needs_human {
                Some(status) => {
                    tracing::warn!(task = %task_id, status = %status, "需要人工处理：暂停自动重启");
                    // M3-5b：进入 needs_human 只发一次；进程仍存活也不停止（Q7/Q8）。
                    let message = format!(
                        "任务 {} 需要人工处理：心跳状态 {status}（扫码/登录/凭据）。自动重启已暂停；状态恢复后会自动继续监督，也可点「重启」立即重试。",
                        self.task_name(task_id)
                    );
                    self.raise_notification(task_id, NotificationKind::NeedsHuman, message);
                }
                None => {
                    // 恢复：允许下次再进入时重新通知（仍受 5 分钟节流）。
                    self.notifications
                        .clear(task_id, NotificationKind::NeedsHuman);
                    self.notifications
                        .clear(task_id, NotificationKind::StopTimeout);
                    tracing::info!(task = %task_id, "人工处理已完成：恢复普通监督");
                }
            }
        }

        if verdict.state == HealthState::Unhealthy {
            tracing::warn!(
                task = %task_id,
                startup = verdict.startup_failure,
                failure = verdict.failure.as_ref().map(|failure| failure.kind.as_str()),
                "健康失败达到阈值，停止实例"
            );
            changed |= self.request_stop(
                task_id,
                StopCause::Health,
                verdict.startup_failure,
                mono_now,
            );
            let stop_job = self.prepare_stop_job(task_id, mono_now);
            return (changed, stop_job);
        }

        // B1：heartbeat/http 首次健康通过才进入 running；process 模式由调度
        // 线程按 startup_grace 到期迁移。
        let should_start = self.supervisors.get(task_id).is_some_and(|supervisor| {
            supervisor.machine.status() == RestartStatus::Starting
                && supervisor
                    .readiness
                    .is_some_and(|plan| readiness_first_healthy(plan, verdict.state))
        });
        if should_start {
            changed |= self.mark_instance_started(task_id, mono_now, wall_now);
        }

        if let Some(supervisor) = self.supervisors.get_mut(task_id) {
            supervisor.monitor = Some(monitor);
            supervisor.next_health_at = Some(mono_now + interval);
        }
        (changed, None)
    }
}

/// 手动命令的执行结果（启动前检查失败与运行时结果分开处理）。
pub(crate) enum CommandOutcome {
    Done(Result<RuntimeSnapshot, RuntimeError>),
    Precheck(String),
    /// 运行中 Restart 停止旧实例失败：旧进程可能仍在运行，不能按启动失败清理。
    RestartStopFailed(RuntimeError),
}

/// 启动时注入的 `CAMPUS_SENTRY_WINDOW_END`（2026-11-05 用户决策）。
///
/// 只有 `schedule.enabled=true` 且配了时段的任务才注入：在窗口内取当前并集
/// 结束，在窗口外取下一次并集结束；无 windows/未启用返回 `None`。
fn schedule_window_end(task: &TaskConfig) -> Option<String> {
    window_end_rfc3339(task.schedule.enabled, &task.schedule.windows, now_local())
}

/// C6：两次采样之间的 CPU 占用百分比（100 = 占满一个核）。
///
/// `cpu_time` 是累计值，除以 10_000 得到毫秒；`elapsed` 为两次采样的间隔。
/// 首次采样（无基线）或累计值倒退（实例已换/进程重启）时返回 0：只更新基线，
/// 不产生任何超限结论。
fn cpu_percent_between(previous: Option<ProcessSample>, current: ProcessSample) -> f64 {
    let Some(previous) = previous else {
        return 0.0;
    };
    let Some(delta_100ns) = current.cpu_time_100ns.checked_sub(previous.cpu_time_100ns) else {
        return 0.0;
    };
    let elapsed_ms = current
        .at
        .saturating_duration_since(previous.at)
        .as_secs_f64()
        * 1000.0;
    if elapsed_ms <= 0.0 {
        return 0.0;
    }
    (delta_100ns as f64 / 10_000.0) / elapsed_ms * 100.0
}

/// 成功加载后的统一日志/级别/清理副作用（手动重载与监听重载共用）。
fn log_loaded_config(loaded: &LoadedConfig, logging: &Logging) {
    logging.apply_core_level(loaded.config.logs.core_level);
    logging.cleanup_core_logs(loaded.config.logs.retention_days);
    tracing::info!(
        event = "config.reload",
        version = env!("CARGO_PKG_VERSION"),
        config = %loaded.config_path.display(),
        data_dir = %loaded.data_dir.display(),
        issues = loaded.issues.len(),
        "配置已加载"
    );
}

/// 配置监听回退轮询周期：下限 500ms，上限 24h（避免异常配置造成失控）。
fn watch_poll_interval(loaded: &LoadedConfig) -> Duration {
    Duration::from_millis(
        loaded
            .config
            .supervisor
            .config_poll_interval_ms
            .clamp(500, 24 * 60 * 60 * 1000),
    )
}

fn load_config(args: &CliArgs, cwd: &Path, logging: &Logging) -> (Option<LoadedConfig>, Snapshot) {
    // dev profile 下把 `.dev-data` 当作 `--data-dir` 传下去，隔离任务日志与状态。
    let data_dir = data_dir::data_dir_override(args, cwd);
    // 没有显式 --config 时，dev profile 的配置也放进 dev 数据目录；
    // 否则开发实例会读写生产配置（%APPDATA%/Rook/config.toml）。
    let dev_config = if args.config.is_none() && args.is_dev() {
        data_dir::dev_config_path(args, cwd)
    } else {
        None
    };
    if let Some(path) = &dev_config
        && !path.is_file()
        && let Err(error) = config::write_default_config(path)
    {
        tracing::warn!(path = %path.display(), error = %error, "写 dev 默认配置失败");
    }
    let config_path = args.config.clone().or(dev_config);
    let overrides = Overrides {
        config: config_path.as_deref(),
        data_dir: data_dir.as_deref(),
    };
    match config::load(&overrides) {
        Ok(loaded) => {
            log_loaded_config(&loaded, logging);
            let snapshot = Snapshot::from_loaded(&loaded);
            (Some(loaded), snapshot)
        }
        Err(error) => {
            // 结构错误只记录状态，绝不 panic/退出：托盘与错误界面继续可用。
            tracing::error!(event = "config.error", error = %error, "配置加载失败");
            let snapshot = Snapshot::from_load_error(&error, data_dir.as_deref());
            (None, snapshot)
        }
    }
}

/// 启动时加载 state.json；配置不可用时不读写状态文件。
fn load_persisted_state(
    loaded: Option<&LoadedConfig>,
) -> (Option<StateStore>, PersistedState, Option<LoadStatus>) {
    let Some(loaded) = loaded else {
        return (None, PersistedState::default(), None);
    };
    let store = StateStore::in_dir(&loaded.data_dir);
    let loaded_state = store.load();
    log_load_status(&store, &loaded_state.status);
    (Some(store), loaded_state.state, Some(loaded_state.status))
}

fn log_load_status(store: &StateStore, status: &LoadStatus) {
    match status {
        LoadStatus::Loaded => tracing::debug!(
            path = %store.path().display(),
            "已恢复监督状态"
        ),
        LoadStatus::Missing => tracing::debug!(
            path = %store.path().display(),
            "无 state.json，使用默认监督状态"
        ),
        other => tracing::warn!(
            path = %store.path().display(),
            status = %other,
            "state.json 不可用，使用默认监督状态"
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};

    use chrono::Timelike;

    use super::*;
    use crate::config::{EnvVars, LoadContext, SystemEnv, TaskConfig};

    /// 固定“没有业务环境变量”的展开上下文：测试结果不随本机 `%VAR%` 变化。
    ///
    /// 只有 `PATH` / `PATHEXT` 转发给真实环境：配置校验用它们解析
    /// `command = "cmd.exe"` 这类不带路径的命令，否则所有任务都会是校验失败。
    struct NoEnv;

    impl EnvVars for NoEnv {
        fn var(&self, name: &str) -> Option<String> {
            if name.eq_ignore_ascii_case("PATH") || name.eq_ignore_ascii_case("PATHEXT") {
                return SystemEnv.var(name);
            }
            None
        }
    }

    /// 每个测试一个独立目录（`load_bytes` 解析数据目录时会创建，Drop 时删除）。
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static SEQ: AtomicUsize = AtomicUsize::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            Self(
                std::env::temp_dir().join(format!("rook-engine-test-{}-{seq}", std::process::id())),
            )
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// 走真实的配置解析/校验路径，但只读内存字节，不写配置/状态文件。
    fn load(dir: &TestDir, text: &str) -> LoadedConfig {
        let config_path = dir.0.join("config.toml");
        let data_dir = dir.0.join("data");
        let ctx = LoadContext {
            config_path: &config_path,
            data_dir_override: Some(&data_dir),
            local_appdata: None,
            env: &NoEnv,
        };
        config::load_bytes(&ctx, text.as_bytes()).expect("测试配置应通过校验")
    }

    fn task_of(loaded: &LoadedConfig, id: &str) -> TaskConfig {
        loaded.config.task(id).expect("任务应存在").clone()
    }

    /// 两个可用任务的最小配置。
    const TWO_TASKS: &str = r#"
version = 1

[[tasks]]
id = "alpha"
name = "甲任务"
command = "cmd.exe"
working_dir = "."

[[tasks]]
id = "beta"
name = "乙任务"
command = "cmd.exe"
working_dir = "."
"#;

    /// 单任务、禁用。
    const DISABLED_TASK: &str = r#"
version = 1

[[tasks]]
id = "alpha"
name = "甲任务"
command = "cmd.exe"
working_dir = "."
enabled = false
"#;

    /// 开启时段但没有配置任何窗口。
    const ENABLED_NO_WINDOWS: &str = r#"
version = 1

[[tasks]]
id = "alpha"
name = "甲任务"
command = "cmd.exe"
working_dir = "."
schedule = { enabled = true }
"#;

    /// 七天全覆盖的窗口（00:00–23:59）。
    const ALL_DAY_WINDOW: &str = r#"
version = 1

[[tasks]]
id = "alpha"
name = "甲任务"
command = "cmd.exe"
working_dir = "."
schedule = { enabled = true, windows = [{ days = ["mon","tue","wed","thu","fri","sat","sun"], start = "00:00", end = "23:59" }] }
"#;

    // ---------------------------------------------------------------- update_baseline

    #[test]
    fn update_baseline_keeps_new_task() {
        let dir = TestDir::new();
        let loaded = load(&dir, TWO_TASKS);
        let current = HashMap::from([("alpha".to_owned(), task_of(&loaded, "alpha"))]);

        let baseline = update_baseline(&HashMap::new(), current, &HashSet::new());

        assert_eq!(baseline.len(), 1, "新任务应进入基线");
        assert_eq!(baseline["alpha"].name, "甲任务");
    }

    #[test]
    fn update_baseline_drops_removed_task() {
        let dir = TestDir::new();
        let loaded = load(&dir, TWO_TASKS);
        let previous = HashMap::from([
            ("alpha".to_owned(), task_of(&loaded, "alpha")),
            ("beta".to_owned(), task_of(&loaded, "beta")),
        ]);
        // 新版配置里 alpha 已被删除。
        let current = HashMap::from([("beta".to_owned(), task_of(&loaded, "beta"))]);

        let baseline = update_baseline(&previous, current, &HashSet::new());

        assert!(!baseline.contains_key("alpha"), "删除的任务不应留在基线");
        assert!(baseline.contains_key("beta"));
    }

    #[test]
    fn update_baseline_keeps_old_config_for_in_flight_task() {
        let dir = TestDir::new();
        let loaded = load(&dir, TWO_TASKS);
        let mut old_alpha = task_of(&loaded, "alpha");
        old_alpha.name = "甲任务（旧）".to_owned();
        let mut new_alpha = task_of(&loaded, "alpha");
        new_alpha.name = "甲任务（新）".to_owned();
        let mut beta = task_of(&loaded, "beta");
        beta.name = "乙任务（新）".to_owned();

        let previous = HashMap::from([("alpha".to_owned(), old_alpha)]);
        let current = HashMap::from([("alpha".to_owned(), new_alpha), ("beta".to_owned(), beta)]);
        let in_flight = HashSet::from(["alpha".to_owned()]);

        let baseline = update_baseline(&previous, current, &in_flight);

        assert_eq!(
            baseline["alpha"].name, "甲任务（旧）",
            "in-flight 任务必须保留旧基线，避免把进行中的动作误判为配置变化"
        );
        assert_eq!(baseline["beta"].name, "乙任务（新）");
    }

    // ---------------------------------------------------------------- schedule_window_end

    #[test]
    fn schedule_window_end_is_none_when_schedule_disabled() {
        let dir = TestDir::new();
        let loaded = load(&dir, TWO_TASKS);
        assert!(schedule_window_end(&task_of(&loaded, "alpha")).is_none());
    }

    #[test]
    fn schedule_window_end_is_none_for_enabled_schedule_without_windows() {
        let dir = TestDir::new();
        let loaded = load(&dir, ENABLED_NO_WINDOWS);
        assert!(schedule_window_end(&task_of(&loaded, "alpha")).is_none());
    }

    #[test]
    fn schedule_window_end_reports_rfc3339_inside_window() {
        let dir = TestDir::new();
        let loaded = load(&dir, ALL_DAY_WINDOW);
        let now = now_local();
        let minutes = now.hour() * 60 + now.minute();
        // 全天窗口唯一落在窗口外的时刻是 23:59 那一分钟；其余时刻必须给出结束值。
        if minutes >= 23 * 60 + 59 {
            return;
        }

        let end = schedule_window_end(&task_of(&loaded, "alpha")).expect("窗口内应注入结束时间");

        assert!(
            end.contains("T23:59:00"),
            "全天窗口的结束应是本地的 23:59：{end}"
        );
    }

    // ---------------------------------------------------------------- watch_poll_interval

    #[test]
    fn watch_poll_interval_uses_configured_ms() {
        let dir = TestDir::new();
        let loaded = load(
            &dir,
            r#"
version = 1

[supervisor]
config_poll_interval_ms = 2000

[[tasks]]
id = "alpha"
name = "甲任务"
command = "cmd.exe"
working_dir = "."
"#,
        );
        assert_eq!(watch_poll_interval(&loaded), Duration::from_millis(2000));
    }

    #[test]
    fn watch_poll_interval_clamps_out_of_range_values() {
        let dir = TestDir::new();
        let too_small = load(
            &dir,
            r#"
version = 1

[supervisor]
config_poll_interval_ms = 10

[[tasks]]
id = "alpha"
name = "甲任务"
command = "cmd.exe"
working_dir = "."
"#,
        );
        assert_eq!(watch_poll_interval(&too_small), Duration::from_millis(500));
    }

    // ---------------------------------------------------------------- begin_command

    #[test]
    fn begin_command_rejects_unknown_task() {
        let dir = TestDir::new();
        let mut engine = Engine::for_test(load(&dir, TWO_TASKS));

        let Err(error) = engine.begin_command("ghost", Action::Start) else {
            panic!("不存在的任务必须被拒绝");
        };

        assert_eq!(error.stage, "invalid_task");
        assert!(
            error.message.contains("配置中没有任务 ghost"),
            "{}",
            error.message
        );
        assert_eq!(error.task_id.as_deref(), Some("ghost"));
    }

    #[test]
    fn begin_command_rejects_start_for_disabled_task() {
        let dir = TestDir::new();
        let mut engine = Engine::for_test(load(&dir, DISABLED_TASK));

        let Err(error) = engine.begin_command("alpha", Action::Start) else {
            panic!("enabled=false 的任务不能手动启动");
        };

        assert_eq!(error.stage, "disabled");
        assert!(error.message.contains("已禁用"), "{}", error.message);
    }

    #[test]
    fn begin_command_still_allows_stop_for_disabled_task() {
        let dir = TestDir::new();
        let mut engine = Engine::for_test(load(&dir, DISABLED_TASK));

        let plan = engine
            .begin_command("alpha", Action::Stop)
            .expect("禁用任务仍要允许收敛进程");

        assert!(plan.context.is_none(), "Stop 不需要启动上下文");
        let loaded = engine.loaded.clone().expect("配置应已加载");
        let expected = Duration::from_millis(task_of(&loaded, "alpha").stop.timeout_ms);
        assert_eq!(plan.timeout, expected);
    }

    #[test]
    fn begin_command_rejects_second_concurrent_action() {
        let dir = TestDir::new();
        let mut engine = Engine::for_test(load(&dir, TWO_TASKS));
        let first = engine
            .begin_command("alpha", Action::Stop)
            .expect("首次停止应被接受");

        let Err(error) = engine.begin_command("alpha", Action::Stop) else {
            panic!("同一任务的并发命令必须被拒绝");
        };

        assert_eq!(error.stage, "invalid_state");
        assert!(error.message.contains("操作正在进行"), "{}", error.message);
        drop(first);
    }

    #[test]
    fn begin_command_allows_start_for_paused_task() {
        let dir = TestDir::new();
        let mut engine = Engine::for_test(load(&dir, TWO_TASKS));
        engine.state.task_mut("alpha").desired_override = Some(DesiredOverride::Paused);
        engine.refresh();

        let plan = engine
            .begin_command("alpha", Action::Start)
            .expect("暂停任务应允许手动启动（用户意图优先）");

        let context = plan.context.expect("Start 必须带启动上下文");
        assert_eq!(context.start_reason, StartReason::UserStart);
        assert!(
            engine
                .state
                .task("alpha")
                .and_then(|state| state.desired_override)
                .is_none(),
            "手动启动应清除 pause override"
        );
    }
}
