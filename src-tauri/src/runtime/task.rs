//! M2-4a：单任务运行时（端口预检 → 启动 → 日志采集 → 退出监控 → 状态收敛）。
//!
//! 本模块把已完成的 [`crate::process`] 与 [`crate::logs`] 组合成一个可独立测试的
//! 任务生命周期，**不依赖 Tauri/Engine/前端**。后续 Engine 只需持有
//! [`TaskRuntime`]（或 `Arc<TaskRuntime>`），调用 [`TaskRuntime::start`] /
//! [`TaskRuntime::stop`] / [`TaskRuntime::restart`]，并读取 [`TaskRuntime::snapshot`]。
//!
//! # 生命周期
//!
//! ```text
//! stopped ──start──► starting ──就绪──► running ──stop/退出──► stopping ──► stopped
//!                        │                                        ▲
//!                        └────────── 任一步失败（回滚）────────────┘
//! ```
//!
//! `start` 的顺序是固定的：
//!
//! 1. 端口预检（[`check_tcp_ports`]，只读；有占用则不创建进程）；
//! 2. 生成新的 `instance_id`，清理上一实例残留的心跳文件；
//! 3. 合并任务 env 并注入 `CAMPUS_SENTRY_*` 保留变量后 [`spawn`] 创建根进程与 Job；
//! 4. `take_stdout` / `take_stderr` 后交给 [`LogCollector`]；
//! 5. [`ExitWatcher::start`] 接管进程/Job 并启动等待线程；
//! 6. 启动本实例的完成线程，等待退出事件。
//!
//! # 实例环境（docs/02 §5.1）
//!
//! 每次 start 生成并暴露新的实例 id，并在任务自己的 `env` 之后追加保留变量，
//! 因此任务 env 的同名项不能覆盖它们。`HEARTBEAT` / `DATA_DIR` / `CONFIG` /
//! `START_REASON` / `WINDOW_END` 由调用方通过 [`InstanceContext`] 传入，
//! `LOG_DIR` 取 [`LogParams::dir`]；`WORK_MODE` 固定为 `active`（一个时钟、
//! 一个开关：进程活着=应该工作）。运行时不会读取真实用户目录、也不会猜测默认
//! 路径。配置了心跳路径时，spawn 前删除旧心跳文件（`NotFound` 忽略；其他失败
//! 记录但不阻止启动）。
//!
//! 第 2 步之后任一步失败都会回滚：终止进程树、丢弃日志采集器、状态回到
//! `stopped`，并把阶段/错误码写进 `last_error`。
//!
//! # 线程模型
//!
//! ```text
//! start/stop/restart ──op 锁──► 串行化生命周期操作
//! snapshot ───────────────────► Mutex<State>（只读，不阻塞生命周期）
//! 完成线程 ──recv ExitEvent──► join 日志 ──► 更新 State ──► notify_all
//! ```
//!
//! 每个实例有且只有一个完成线程；退出事件只发一次。日志 join 使用有界等待
//! （`LogParams::stop_timeout`），极端情况下（进程树未完全清空）不会无限阻塞。
//!
//! # 明确不实现
//!
//! 健康探测、重启退避/熔断、`needs_human`、自动重启、托盘/UI、配置热更新。
//! 进程退出后保持 `stopped`，是否重启由 M3 决定。

use std::ffi::{OsStr, OsString};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::config::{HealthKind, StopMethod, TaskConfig};
use crate::logs::{LogCollector, LogParams, LogStats};
use crate::process::{
    ExitEvent, ExitWatcher, SpawnRequest, StopError, StopMode, StopReport, WatchOptions,
    check_tcp_ports, spawn,
};

use super::state::{
    ExitReason, RuntimeError, RuntimeEvent, RuntimeSnapshot, RuntimeStage, TaskStatus,
};

/// 进程内实例序号；与时间戳一起构成唯一 `instance_id`。
static INSTANCE_SEQ: AtomicU64 = AtomicU64::new(1);

/// 运行时被丢弃时等待停止的上限；超时后仍会强制释放监控句柄。
const DROP_TIMEOUT: Duration = Duration::from_secs(5);

/// 实例启动来源（`CAMPUS_SENTRY_START_REASON`，2026-11-05 用户决策）。
///
/// 每个实例都必须有明确来源，脚本用它区分“为什么这次会跑”：
///
/// | 取值 | 触发路径 |
/// | --- | --- |
/// | `schedule_start` | 运行时段进入窗口，应用按唯一时钟自动启动 |
/// | `user_start` | UI/托盘 Start / Restart / 恢复（`execute` / `begin_command` 路径） |
/// | `restart` | 监督器自动重启：退避重试、健康失败重启、配置热重载重启、电源恢复、`needs_human` 清除后自动恢复（`pending_start` 路径） |
/// | `autostart` | 应用启动时 `auto_start` 排期到期（`scheduler_work` 第 2 步路径） |
///
/// 默认值是 `user_start`：直接调用 [`TaskRuntime::start`] 没有显式来源时按
/// 手动启动处理。Engine 的所有自动路径都会显式设置来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StartReason {
    /// 运行时段自动启动（唯一时钟）。
    ScheduleStart,
    /// 用户手动 Start/Restart/恢复。
    #[default]
    UserStart,
    /// 监督器自动重启。
    Restart,
    /// 应用启动后的 `auto_start` 排期。
    Autostart,
}

impl StartReason {
    /// 稳定的环境变量取值（脚本契约）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ScheduleStart => "schedule_start",
            Self::UserStart => "user_start",
            Self::Restart => "restart",
            Self::Autostart => "autostart",
        }
    }

    /// 全部取值（测试/文档用）。
    pub const ALL: [StartReason; 4] = [
        StartReason::ScheduleStart,
        StartReason::UserStart,
        StartReason::Restart,
        StartReason::Autostart,
    ];
}

/// 实例级注入上下文（docs/02 §5.1）。
///
/// 这些路径/来源由调用方（Engine）按配置与数据目录算好传入；运行时**不会**
/// 读取真实用户目录，也不会猜测默认路径。未提供的路径对应的变量不注入、对应
/// 文件也不清理——生产调用方需要补齐 `HEARTBEAT` / `DATA_DIR` / `CONFIG`，
/// 否则脚本契约不完整；`START_REASON` 每个实例必有（默认 `user_start`）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceContext {
    /// 任务显示名（`CAMPUS_SENTRY_TASK_NAME`）；`None`/空串时回退到任务 id。
    pub task_name: Option<String>,
    /// 心跳文件绝对路径（`CAMPUS_SENTRY_HEARTBEAT`）；每次 spawn 前删除该文件。
    pub heartbeat: Option<PathBuf>,
    /// 数据目录绝对路径（`CAMPUS_SENTRY_DATA_DIR`）。
    pub data_dir: Option<PathBuf>,
    /// 配置文件绝对路径（`CAMPUS_SENTRY_CONFIG`）。
    pub config: Option<PathBuf>,
    /// 本次实例的启动来源（`CAMPUS_SENTRY_START_REASON`）。
    pub start_reason: StartReason,
    /// 本次实例应停止的窗口结束时刻（`CAMPUS_SENTRY_WINDOW_END`，RFC3339
    /// 本地时间带偏移）；只有 `schedule.enabled=true` 的任务在能算出结束时刻
    /// 时才注入，否则为 `None`（不注入）。
    pub window_end: Option<String>,
}

impl InstanceContext {
    /// 只填配置里已知的部分（任务名、heartbeat 模式下显式配置的心跳路径）。
    ///
    /// 数据目录/配置文件路径需要调用方提供，见 [`InstanceContext::for_task`]。
    fn from_config_task(task: &TaskConfig) -> Self {
        Self {
            task_name: Some(task.name.clone()),
            // 只有 heartbeat 模式才把 health.path 当作心跳文件。
            heartbeat: if task.health.kind == HealthKind::Heartbeat {
                task.health.path.as_deref().map(absolute_path)
            } else {
                None
            },
            data_dir: None,
            config: None,
            // 来源与窗口结束时刻依赖启动时机，由 Engine 在 start 前显式补齐。
            start_reason: StartReason::default(),
            window_end: None,
        }
    }

    /// 由配置 + 调用方提供的绝对路径派生完整实例上下文（docs/02 §5.1）。
    ///
    /// - `task_name` 取配置里的 `name`；
    /// - `DATA_DIR` / `CONFIG` 一律归一化为绝对路径；
    /// - heartbeat 任务：优先用配置的 `health.path`；为空时用
    ///   `<data_dir>/heartbeats/<task_id>.hb`（绝对路径）；
    /// - 非 heartbeat 任务不注入 `CAMPUS_SENTRY_HEARTBEAT`。
    pub fn for_task(task: &TaskConfig, data_dir: &Path, config_path: &Path) -> Self {
        let data_dir = absolute_path(data_dir);
        let config = absolute_path(config_path);
        let heartbeat = if task.health.kind == HealthKind::Heartbeat {
            Some(match task.health.path.as_deref() {
                Some(path) => absolute_path(path),
                None => data_dir.join("heartbeats").join(format!("{}.hb", task.id)),
            })
        } else {
            None
        };
        Self {
            task_name: Some(task.name.clone()),
            heartbeat,
            data_dir: Some(data_dir),
            config: Some(config),
            start_reason: StartReason::default(),
            window_end: None,
        }
    }
}

/// 转成绝对路径（不要求存在、不做 canonicalize）；失败时退回原路径。
fn absolute_path(path: &Path) -> PathBuf {
    std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf())
}

/// 启动一个任务所需的全部信息。
///
/// 字段与 [`crate::config::TaskConfig`] 一一对应，但刻意不依赖配置模块的加载流程：
/// [`TaskSpec::from_config`] 负责把已归一化的任务转成本类型（路径、环境变量、
/// 日志参数、停止超时），Engine 只负责决定何时创建/替换实例并补齐
/// [`InstanceContext`] 中的绝对路径。
#[derive(Debug, Clone)]
pub struct TaskSpec {
    /// 任务 id（同时用于日志目录与实例 id 前缀）。
    pub id: String,
    /// 可执行文件：绝对/相对路径，或 PATH 中的裸名字。
    pub program: OsString,
    /// 参数（不含 program 本身）。
    pub args: Vec<String>,
    /// 工作目录；`None` 表示继承父进程当前目录。
    pub working_dir: Option<PathBuf>,
    /// 环境变量覆盖（覆盖继承值）。
    pub env: Vec<(OsString, OsString)>,
    /// 启动前必须空闲的 TCP 端口。
    pub ports: Vec<u16>,
    /// 日志采集参数（目录、轮转、超时）。
    pub logs: LogParams,
    /// 退出监控参数（树等待、轮询间隔、停止超时）。
    pub watch: WatchOptions,
    /// 实例级注入上下文（任务名、心跳、数据目录、配置文件）。
    ///
    /// 日志目录固定取 [`LogParams::dir`]，启动时注入为
    /// `CAMPUS_SENTRY_LOG_DIR`；其余路径由调用方在此提供。
    pub instance: InstanceContext,
}

impl TaskSpec {
    /// 用 id、命令与日志参数创建任务；其余字段为空/默认值。
    pub fn new(id: impl Into<String>, program: impl Into<OsString>, logs: LogParams) -> Self {
        Self {
            id: id.into(),
            program: program.into(),
            args: Vec::new(),
            working_dir: None,
            env: Vec::new(),
            ports: Vec::new(),
            logs,
            watch: WatchOptions::default(),
            instance: InstanceContext::default(),
        }
    }

    /// 替换实例级注入上下文（链式；路径由调用方按配置/数据目录算好）。
    pub fn with_instance_context(mut self, context: InstanceContext) -> Self {
        self.instance = context;
        self
    }

    /// 从已归一化的配置任务派生启动参数。
    ///
    /// - `working_dir` 在配置层已展开为绝对路径，这里总是显式传入；
    /// - `env` 按配置顺序转成大小写不敏感的覆盖项（由进程层处理同名覆盖）；
    /// - 日志参数用 [`LogParams::from_config`]；
    /// - 停止参数来自 `[tasks.stop]`：`method` / `grace_ms` / `timeout_ms`。
    pub fn from_config(task: &TaskConfig) -> Self {
        Self {
            id: task.id.clone(),
            program: OsString::from(&task.command),
            args: task.args.clone(),
            working_dir: Some(task.working_dir.clone()),
            env: task
                .env
                .iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value)))
                .collect(),
            ports: task.ports.clone(),
            logs: LogParams::from_config(&task.logs),
            watch: WatchOptions {
                stop_timeout: Duration::from_millis(task.stop.timeout_ms),
                method: match task.stop.method {
                    StopMethod::Terminate => StopMode::Terminate,
                    StopMethod::Graceful => StopMode::Graceful,
                },
                grace: Duration::from_millis(task.stop.grace_ms),
                ..WatchOptions::default()
            },
            instance: InstanceContext::from_config_task(task),
        }
    }

    /// 从配置派生启动参数，并由调用方补齐 [`InstanceContext`] 的绝对路径。
    ///
    /// 这是 Engine 接线的推荐入口：`data_dir` / `config_path` 由配置加载结果
    /// 给出，heartbeat 任务缺省路径由此推导为
    /// `<data_dir>/heartbeats/<task_id>.hb`；运行时本身仍不读取用户目录。
    pub fn from_config_with_paths(task: &TaskConfig, data_dir: &Path, config_path: &Path) -> Self {
        let mut spec = Self::from_config(task);
        spec.instance = InstanceContext::for_task(task, data_dir, config_path);
        spec
    }

    /// 组装进程启动请求；`env` 由运行时按实例生成（任务 env + 保留变量）。
    fn spawn_request(&self, env: Vec<(OsString, OsString)>) -> SpawnRequest {
        SpawnRequest {
            program: self.program.clone(),
            args: self.args.clone(),
            working_dir: self.working_dir.clone(),
            env,
        }
    }
}

/// 单个任务的运行时。
///
/// 所有方法都接受 `&self`，因此可以放进 `Arc` 后由 Engine、停止线程和快照读取方
/// 共享。生命周期操作（start/stop/restart）由内部互斥锁串行化；`snapshot` 只读
/// 状态锁，不会被长时间阻塞。
///
/// 运行中被 Drop 时会尽力停止进程树并回收日志线程（有界等待），避免遗留进程；
/// 正常关闭流程仍应显式调用 [`TaskRuntime::stop`]。
pub struct TaskRuntime {
    spec: TaskSpec,
    log_path: PathBuf,
    shared: Arc<Shared>,
    /// 可选事件发送端；用 `Mutex` 包裹以保证 `TaskRuntime: Sync`。
    events: Mutex<Option<Sender<RuntimeEvent>>>,
    /// 最近一次启动使用的实例上下文：`start_with_context` 更新，`restart` 复用。
    /// 避免“首次用自定义 context 启动后，重启丢失 HEARTBEAT/DATA_DIR/CONFIG 注入”。
    context: Mutex<InstanceContext>,
    /// 串行化 start/stop/restart，避免并发启动或启动中被停止。
    op: Mutex<()>,
}

impl TaskRuntime {
    /// 创建运行时；不启动进程。
    pub fn new(spec: TaskSpec) -> Self {
        Self::build(spec, None)
    }

    /// 创建运行时并注册事件通道；发送失败不会影响任务。
    pub fn with_events(spec: TaskSpec, events: Sender<RuntimeEvent>) -> Self {
        Self::build(spec, Some(events))
    }

    fn build(spec: TaskSpec, events: Option<Sender<RuntimeEvent>>) -> Self {
        let log_path = spec.logs.current_path();
        let context = Mutex::new(spec.instance.clone());
        Self {
            spec,
            log_path,
            shared: Arc::new(Shared {
                state: Mutex::new(State::new()),
                changed: Condvar::new(),
            }),
            events: Mutex::new(events),
            context,
            op: Mutex::new(()),
        }
    }

    /// 最近一次启动使用的实例上下文。
    ///
    /// 初始为 [`TaskSpec::instance`]；调用 [`TaskRuntime::start_with_context`]
    /// 后更新为该上下文，[`TaskRuntime::restart`] 会复用它。返回克隆值，调用方
    /// 修改返回值不会影响运行时。
    pub fn instance_context(&self) -> InstanceContext {
        self.context
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    /// 任务 id。
    pub fn id(&self) -> &str {
        &self.spec.id
    }

    /// 任务定义（只读）。
    pub fn spec(&self) -> &TaskSpec {
        &self.spec
    }

    /// 当前状态（短锁）。
    pub fn status(&self) -> TaskStatus {
        self.shared.lock().status
    }

    /// 当前完整快照（短锁；日志统计为实时值）。
    pub fn snapshot(&self) -> RuntimeSnapshot {
        self.shared.lock().snapshot(&self.spec.id, &self.log_path)
    }

    /// 最近一次实例的启动单调时刻（进程创建成功时记录）。
    ///
    /// 与 [`RuntimeEvent::Running`] 里的 `started_instant` 相同，供 Engine 在没有
    /// 事件接收方时计算 `startup_grace`；进程停止后仍保留最近一次实例的值。
    pub fn instance_started_instant(&self) -> Option<Instant> {
        self.shared.lock().started_instant
    }

    /// 最近一次启动前清理旧心跳文件失败的原因；成功或未配置心跳时为 `None`。
    pub fn heartbeat_cleanup_error(&self) -> Option<String> {
        self.shared.lock().heartbeat_cleanup_error.clone()
    }

    /// 最近一次实例的停止诊断（method / grace / forced / waited / 信号错误）。
    ///
    /// 自然退出的实例为 `None`；停止请求发出后即使尚未完成也可能已可读取。
    pub fn last_stop_report(&self) -> Option<StopReport> {
        self.shared.lock().last_stop.clone()
    }

    /// 启动任务；失败时已回滚到 `stopped`，错误同时写入 `last_error`。
    ///
    /// 端口被占用时**不会创建进程**。运行中/停止中调用返回
    /// [`RuntimeStage::InvalidState`]。
    pub fn start(&self) -> Result<RuntimeSnapshot, RuntimeError> {
        let _op = self.lock_op();
        let context = self.instance_context();
        self.start_locked(&context)
    }

    /// 用调用方提供的实例上下文启动，并把它记录为“最近上下文”。
    ///
    /// 便于 Engine 在数据目录/配置文件路径确定后按次注入（其余行为与
    /// [`TaskRuntime::start`] 完全一致，同样是新建实例 id）。运行时不会
    /// 猜测默认路径：未提供的路径不注入、对应文件不清理。
    ///
    /// 记录发生在启动尝试之前：即使本次启动失败，后续 [`TaskRuntime::restart`]
    /// 仍沿用调用方明确给出的 HEARTBEAT/DATA_DIR/CONFIG，避免路径注入丢失。
    pub fn start_with_context(
        &self,
        context: &InstanceContext,
    ) -> Result<RuntimeSnapshot, RuntimeError> {
        let _op = self.lock_op();
        *self
            .context
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = context.clone();
        self.start_locked(context)
    }

    /// 停止任务并等待收尾（进程退出 + 日志 join），最多等 `timeout`。
    ///
    /// - 幂等：已停止时直接返回当前快照；
    /// - 用户停止的实例在 `exit_reason` 中标记为 [`ExitReason::UserStop`]，
    ///   不会与自然退出混淆；
    /// - 超时返回 [`RuntimeStage::Stop`] 错误，状态保持 `stopping`，
    ///   完成线程仍会在后台收尾，后续可再次调用 `stop`。
    pub fn stop(&self, timeout: Duration) -> Result<RuntimeSnapshot, RuntimeError> {
        let _op = self.lock_op();
        self.stop_locked(timeout)
    }

    /// 先停止（最多等 `timeout`），再启动一个新实例。
    ///
    /// 每次启动都会分配新的 `instance_id`；旧实例的退出事件不会影响新实例。
    /// 复用最近一次启动的上下文（可能是 [`TaskRuntime::start_with_context`]
    /// 提供的路径）。
    pub fn restart(&self, timeout: Duration) -> Result<RuntimeSnapshot, RuntimeError> {
        let _op = self.lock_op();
        let context = self.instance_context();
        self.stop_locked(timeout)?;
        self.start_locked(&context)
    }

    /// 先停止，再用调用方给出的实例上下文启动新实例，并记为最近上下文。
    ///
    /// 与 [`TaskRuntime::restart`] 相同，只是让 Engine 在重启时也能显式传入
    /// HEARTBEAT/DATA_DIR/CONFIG，避免“首次自定义 context 启动后重启丢注入”。
    /// 记录发生在停止/启动尝试之前。
    pub fn restart_with_context(
        &self,
        timeout: Duration,
        context: &InstanceContext,
    ) -> Result<RuntimeSnapshot, RuntimeError> {
        let _op = self.lock_op();
        *self
            .context
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = context.clone();
        self.stop_locked(timeout)?;
        self.start_locked(context)
    }

    fn lock_op(&self) -> MutexGuard<'_, ()> {
        self.op
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn event_sender(&self) -> Option<Sender<RuntimeEvent>> {
        self.events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    fn emit(&self, event: RuntimeEvent) {
        let events = self
            .events
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(sender) = events.as_ref() {
            // 接收端已关闭不是错误：事件通道是可选旁路。
            let _ = sender.send(event);
        }
    }

    fn start_locked(&self, context: &InstanceContext) -> Result<RuntimeSnapshot, RuntimeError> {
        let instance_id = next_instance_id(&self.spec.id);

        // 先回收上一实例可能残留的句柄；正常路径下都已是 None。
        let (previous, stale_logs, stale_watcher) = {
            let mut state = self.shared.lock();
            if state.status != TaskStatus::Stopped {
                return Err(RuntimeError::new(
                    RuntimeStage::InvalidState,
                    None,
                    format!(
                        "任务 {} 当前状态为 {}，不能启动",
                        self.spec.id, state.status
                    ),
                ));
            }
            let previous = state.completion.take();
            let stale_logs = state.logs.take();
            let stale_watcher = state.watcher.take();
            state.reset_for_start(instance_id.clone());
            (previous, stale_logs, stale_watcher)
        };
        drop(stale_watcher);
        if let Some(collector) = stale_logs {
            drop(collector);
        }
        if let Some(handle) = previous {
            let _ = handle.join();
        }

        self.emit(RuntimeEvent::Starting(self.snapshot()));

        // 1. 端口预检：只读；有占用就绝不创建进程。
        if let Err(error) = self.precheck_ports() {
            self.fail_start(error.clone(), LogStats::default());
            return Err(error);
        }

        // 2. 清理旧心跳：必须在 spawn 前完成，避免上一实例残留被健康探测误判
        //    为“新鲜”。删除失败（权限/被占用等）不阻止启动，只记录供 Engine 告警。
        let heartbeat_cleanup_error = self.clear_old_heartbeat(context);
        if let Some(error) = &heartbeat_cleanup_error {
            self.shared.lock().heartbeat_cleanup_error = Some(error.clone());
        }

        // 3. 创建进程：任务 env 在前、保留变量在后，进程层按同名后者胜出。
        let env = self.instance_environment(context, &instance_id);
        let mut child = match spawn(self.spec.spawn_request(env)) {
            Ok(child) => child,
            Err(error) => {
                let error = RuntimeError::new(
                    RuntimeStage::Spawn,
                    Some(error.code()),
                    format!("{}：{}", error.stage(), error.message),
                );
                self.fail_start(error.clone(), LogStats::default());
                return Err(error);
            }
        };
        let pid = child.pid();
        // 进程创建成功时刻：Engine 的 startup_grace 从这里开始计时。
        let instance_started = Instant::now();

        // 4. 日志采集接管两条管道。
        let stdout = child.take_stdout();
        let stderr = child.take_stderr();
        let collector = match LogCollector::start(stdout, stderr, self.spec.logs.clone()) {
            Ok(collector) => collector,
            Err(error) => {
                // 日志目录不可用：必须回收已经创建的进程树。
                let _ = child.terminate();
                drop(child);
                let error = RuntimeError::new(RuntimeStage::Logs, None, error.to_string());
                self.fail_start(error.clone(), LogStats::default());
                return Err(error);
            }
        };

        // 5. 退出监控接管进程/Job。
        let (sender, receiver) = mpsc::channel::<ExitEvent>();
        let watcher =
            match ExitWatcher::start(instance_id.clone(), child, sender, self.spec.watch.clone()) {
                Ok(watcher) => Arc::new(watcher),
                Err(error) => {
                    // `ExitWatcher::start` 已消费 child；Job 关闭会回收进程树。
                    let stats = finish_logs(collector, self.spec.logs.stop_timeout);
                    let error = RuntimeError::new(
                        RuntimeStage::Watch,
                        Some(error.code()),
                        format!("{}：{}", error.stage(), error.message),
                    );
                    self.fail_start(error.clone(), stats);
                    return Err(error);
                }
            };

        // 6. 所有资源就绪后再进入 running。
        {
            let mut state = self.shared.lock();
            state.status = TaskStatus::Running;
            state.pid = Some(pid);
            state.started_at = Some(SystemTime::now());
            state.started_instant = Some(instance_started);
            state.logs = Some(collector);
            state.watcher = Some(Arc::clone(&watcher));
        }
        self.emit(RuntimeEvent::Running {
            instance_id: instance_id.clone(),
            started_instant: instance_started,
            heartbeat_cleanup_error,
            snapshot: self.snapshot(),
        });

        let context = Completion {
            task_id: self.spec.id.clone(),
            log_path: self.log_path.clone(),
            log_timeout: self.spec.logs.stop_timeout,
        };
        let shared = Arc::clone(&self.shared);
        let events = self.event_sender();
        let completion_instance = instance_id;
        let spawned = thread::Builder::new()
            .name(format!("cs-task-{}", self.spec.id))
            .spawn(move || {
                complete_instance(shared, events, context, completion_instance, receiver);
            });

        match spawned {
            Ok(handle) => {
                self.shared.lock().completion = Some(handle);
                Ok(self.snapshot())
            }
            Err(error) => {
                // 线程创建失败：回收监控句柄（会终止进程树）与日志线程。
                let (stored_watcher, stored_logs) = {
                    let mut state = self.shared.lock();
                    (state.watcher.take(), state.logs.take())
                };
                drop(stored_watcher);
                drop(watcher);
                let stats = match stored_logs {
                    Some(collector) => finish_logs(collector, self.spec.logs.stop_timeout),
                    None => LogStats::default(),
                };
                let error = RuntimeError::new(
                    RuntimeStage::Watch,
                    error.raw_os_error().map(|code| code as u32),
                    format!("创建任务完成线程失败：{error}"),
                );
                self.fail_start(error.clone(), stats);
                Err(error)
            }
        }
    }

    fn stop_locked(&self, timeout: Duration) -> Result<RuntimeSnapshot, RuntimeError> {
        let deadline = Instant::now() + timeout;

        let (watcher, stopping_snapshot) = {
            let mut state = self.shared.lock();
            match state.status {
                TaskStatus::Stopped => {
                    let snapshot = state.snapshot(&self.spec.id, &self.log_path);
                    let completion = state.completion.take();
                    drop(state);
                    if let Some(handle) = completion {
                        let _ = handle.join();
                    }
                    return Ok(snapshot);
                }
                TaskStatus::Starting => {
                    return Err(RuntimeError::new(
                        RuntimeStage::InvalidState,
                        None,
                        format!("任务 {} 正在启动，不能停止", self.spec.id),
                    ));
                }
                TaskStatus::Running => {
                    state.status = TaskStatus::Stopping;
                    (
                        state.watcher.clone(),
                        Some(state.snapshot(&self.spec.id, &self.log_path)),
                    )
                }
                TaskStatus::Stopping => (state.watcher.clone(), None),
            }
        };

        if let Some(snapshot) = stopping_snapshot {
            self.emit(RuntimeEvent::Stopping(snapshot));
        }

        let Some(watcher) = watcher else {
            // 监控句柄已由完成线程取走（进程已退出，只剩收尾）：直接等状态。
            // 正常路径不会在 running 时出现 None。
            return self.wait_until_stopped(deadline);
        };

        let remaining = deadline.saturating_duration_since(Instant::now());
        match watcher.request_stop(remaining) {
            Ok(_) => {}
            Err(StopError::Timeout { waited, .. }) => {
                return Err(RuntimeError::new(
                    RuntimeStage::Stop,
                    None,
                    format!(
                        "停止任务 {} 超时：等待根进程退出 {}ms 未完成",
                        self.spec.id,
                        waited.as_millis()
                    ),
                ));
            }
            Err(StopError::Win32(error)) => {
                return Err(RuntimeError::new(
                    RuntimeStage::Stop,
                    Some(error.code()),
                    format!("{}：{}", error.stage(), error.message),
                ));
            }
        }

        self.wait_until_stopped(deadline)
    }

    /// 有界等待完成线程把状态置为 `stopped`，返回最终快照。
    fn wait_until_stopped(&self, deadline: Instant) -> Result<RuntimeSnapshot, RuntimeError> {
        let snapshot = {
            let mut state = self.shared.lock();
            loop {
                if state.status == TaskStatus::Stopped {
                    break;
                }
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(RuntimeError::new(
                        RuntimeStage::Stop,
                        None,
                        format!("停止任务 {} 超时：等待日志收尾未完成", self.spec.id),
                    ));
                }
                let (guard, _) = self
                    .shared
                    .changed
                    .wait_timeout_while(state, remaining, |state| {
                        state.status != TaskStatus::Stopped
                    })
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                state = guard;
            }
            state.snapshot(&self.spec.id, &self.log_path)
        };

        let completion = self.shared.lock().completion.take();
        if let Some(handle) = completion {
            let _ = handle.join();
        }
        Ok(snapshot)
    }

    /// 合并任务 env 后注入 docs/02 §5.1 的保留变量。
    ///
    /// 保留变量追加在任务 env 之后；进程层（[`crate::process::build_environment_block`]）
    /// 按大小写不敏感的同名覆盖处理，因此任务 env 无法覆盖它们。
    fn instance_environment(
        &self,
        context: &InstanceContext,
        instance_id: &str,
    ) -> Vec<(OsString, OsString)> {
        let mut env = self.spec.env.clone();
        push_env(&mut env, "CAMPUS_SENTRY_TASK_ID", OsStr::new(&self.spec.id));
        let task_name = context
            .task_name
            .as_deref()
            .filter(|name| !name.is_empty())
            .unwrap_or(&self.spec.id);
        push_env(&mut env, "CAMPUS_SENTRY_TASK_NAME", OsStr::new(task_name));
        push_env(
            &mut env,
            "CAMPUS_SENTRY_INSTANCE_ID",
            OsStr::new(instance_id),
        );
        if let Some(path) = context.heartbeat.as_ref() {
            push_env(&mut env, "CAMPUS_SENTRY_HEARTBEAT", path.as_os_str());
        }
        // 日志目录来自 TaskSpec（配置已解析为绝对路径）。
        push_env(
            &mut env,
            "CAMPUS_SENTRY_LOG_DIR",
            self.spec.logs.dir.as_os_str(),
        );
        if let Some(path) = context.data_dir.as_ref() {
            push_env(&mut env, "CAMPUS_SENTRY_DATA_DIR", path.as_os_str());
        }
        if let Some(path) = context.config.as_ref() {
            push_env(&mut env, "CAMPUS_SENTRY_CONFIG", path.as_os_str());
        }
        let supervisor_pid = std::process::id().to_string();
        push_env(
            &mut env,
            "CAMPUS_SENTRY_SUPERVISOR_PID",
            OsStr::new(&supervisor_pid),
        );
        // v1 运行时没有提权 runner（配置层 elevated 任务尚未接入），固定 0，
        // 避免脚本误以为当前实例拥有管理员权限。
        push_env(&mut env, "CAMPUS_SENTRY_ELEVATED", OsStr::new("0"));
        // 2026-11-05 用户决策：单一权威。
        // - START_REASON：每实例必有，脚本据此区分时段/手动/自动重启/开机排期；
        // - WORK_MODE：所有实例固定 active（进程活着=应该工作）；
        // - WINDOW_END：只有 schedule.enabled=true 且能算出结束时刻时注入，
        //   窗口内=当前并集结束，窗口外=下一次并集结束。
        push_env(
            &mut env,
            "CAMPUS_SENTRY_START_REASON",
            OsStr::new(context.start_reason.as_str()),
        );
        push_env(&mut env, "CAMPUS_SENTRY_WORK_MODE", OsStr::new("active"));
        if let Some(window_end) = context.window_end.as_deref() {
            push_env(&mut env, "CAMPUS_SENTRY_WINDOW_END", OsStr::new(window_end));
        }
        env
    }

    /// 删除上一实例可能留下的心跳文件；失败返回可诊断说明（不阻止启动）。
    fn clear_old_heartbeat(&self, context: &InstanceContext) -> Option<String> {
        let path = context.heartbeat.as_ref()?;
        match fs::remove_file(path) {
            Ok(()) => None,
            // 文件本来就不存在是正常情况（首次启动/脚本尚未写心跳）。
            Err(error) if error.kind() == io::ErrorKind::NotFound => None,
            Err(error) => Some(format!("删除旧心跳文件失败：{}：{error}", path.display())),
        }
    }

    fn precheck_ports(&self) -> Result<(), RuntimeError> {
        if self.spec.ports.is_empty() {
            return Ok(());
        }
        let report = check_tcp_ports(&self.spec.ports).map_err(|error| {
            RuntimeError::new(
                RuntimeStage::PortCheck,
                Some(error.code()),
                error.to_string(),
            )
        })?;

        let mut conflicts = Vec::new();
        for status in report.occupied() {
            let pids = status.pids();
            let pid_text = if pids.is_empty() {
                "未知".to_owned()
            } else {
                pids.iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(",")
            };
            let names: Vec<&str> = status
                .listeners
                .iter()
                .filter_map(|listener| listener.process_name.as_deref())
                .collect();
            let name_text = if names.is_empty() {
                String::new()
            } else {
                format!("，进程 {}", names.join(","))
            };
            conflicts.push(format!(
                "端口 {} 已被监听（pid {}{}）",
                status.port, pid_text, name_text
            ));
        }

        if conflicts.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::new(
                RuntimeStage::PortCheck,
                None,
                conflicts.join("；"),
            ))
        }
    }

    fn fail_start(&self, error: RuntimeError, stats: LogStats) {
        {
            let mut state = self.shared.lock();
            state.status = TaskStatus::Stopped;
            state.pid = None;
            state.started_at = None;
            state.exit_code = None;
            state.exit_reason = None;
            state.tree_empty = None;
            state.forced_kill = false;
            state.last_stop = None;
            state.last_error = Some(error);
            state.log_stats = stats;
        }
        self.emit(RuntimeEvent::StartFailed(self.snapshot()));
        self.shared.changed.notify_all();
    }
}

impl Drop for TaskRuntime {
    /// 运行中被丢弃时回收进程树，避免完成线程永远等不到退出事件。
    fn drop(&mut self) {
        let _ = self.stop(DROP_TIMEOUT);
        // 兜底：若 stop 超时（例如终止调用失败），仍释放监控句柄；
        // `ExitWatcher::Drop` 会再次尝试终止整棵树。
        let (watcher, logs) = {
            let mut state = self.shared.lock();
            (state.watcher.take(), state.logs.take())
        };
        drop(watcher);
        drop(logs);
    }
}

/// 运行时状态与条件变量（完成线程唤醒 `stop` 的等待方）。
struct Shared {
    state: Mutex<State>,
    changed: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 单个实例的可变状态；只在本模块内使用。
struct State {
    status: TaskStatus,
    pid: Option<u32>,
    instance_id: Option<String>,
    started_at: Option<SystemTime>,
    /// 本次实例进程创建成功的单调时刻（Engine 的 startup_grace 基线）。
    started_instant: Option<Instant>,
    /// 启动前清理旧心跳失败的原因；None = 成功/未配置。
    heartbeat_cleanup_error: Option<String>,
    exit_code: Option<u32>,
    exit_reason: Option<ExitReason>,
    tree_empty: Option<bool>,
    forced_kill: bool,
    /// 最近一次实例的停止诊断；自然退出/启动失败为 `None`。
    last_stop: Option<StopReport>,
    last_error: Option<RuntimeError>,
    logs: Option<LogCollector>,
    log_stats: LogStats,
    watcher: Option<Arc<ExitWatcher>>,
    completion: Option<JoinHandle<()>>,
}

impl State {
    fn new() -> Self {
        Self {
            status: TaskStatus::Stopped,
            pid: None,
            instance_id: None,
            started_at: None,
            started_instant: None,
            heartbeat_cleanup_error: None,
            exit_code: None,
            exit_reason: None,
            tree_empty: None,
            forced_kill: false,
            last_stop: None,
            last_error: None,
            logs: None,
            log_stats: LogStats::default(),
            watcher: None,
            completion: None,
        }
    }

    fn reset_for_start(&mut self, instance_id: String) {
        self.status = TaskStatus::Starting;
        self.pid = None;
        self.instance_id = Some(instance_id);
        self.started_at = None;
        self.started_instant = None;
        self.heartbeat_cleanup_error = None;
        self.exit_code = None;
        self.exit_reason = None;
        self.tree_empty = None;
        self.forced_kill = false;
        self.last_stop = None;
        self.last_error = None;
        self.logs = None;
        self.log_stats = LogStats::default();
        self.watcher = None;
    }

    fn snapshot(&self, id: &str, log_path: &Path) -> RuntimeSnapshot {
        RuntimeSnapshot {
            id: id.to_owned(),
            status: self.status,
            pid: self.pid,
            instance_id: self.instance_id.clone(),
            started_at: self.started_at,
            exit_code: self.exit_code,
            exit_reason: self.exit_reason,
            tree_empty: self.tree_empty,
            forced_kill: self.forced_kill,
            last_error: self.last_error.clone(),
            logs: self
                .logs
                .as_ref()
                .map_or(self.log_stats, LogCollector::stats),
            log_path: log_path.to_path_buf(),
        }
    }
}

/// 完成线程所需的固定上下文。
struct Completion {
    task_id: String,
    log_path: PathBuf,
    log_timeout: Duration,
}

/// 完成线程主体：等退出事件 → join 日志 → 更新状态 → 通知等待方。
fn complete_instance(
    shared: Arc<Shared>,
    events: Option<Sender<RuntimeEvent>>,
    context: Completion,
    instance_id: String,
    receiver: Receiver<ExitEvent>,
) {
    match receiver.recv() {
        Ok(event) => {
            debug_assert_eq!(event.instance_id, instance_id, "退出事件必须属于当前实例");
            finalize(shared, events, context, Some(event), None);
        }
        Err(_) => {
            // 监控线程没有发事件就结束（异常路径）：按失败收尾，避免永远 running。
            let error =
                RuntimeError::new(RuntimeStage::Watch, None, "退出监控线程未发送事件就结束");
            finalize(shared, events, context, None, Some(error));
        }
    }
}

fn finalize(
    shared: Arc<Shared>,
    events: Option<Sender<RuntimeEvent>>,
    context: Completion,
    event: Option<ExitEvent>,
    error: Option<RuntimeError>,
) {
    let Completion {
        task_id,
        log_path,
        log_timeout,
    } = context;

    // 先把采集器取出来，避免 join 时持有状态锁。
    let collector = {
        let mut state = shared.lock();
        if let Some(collector) = state.logs.as_ref() {
            state.log_stats = collector.stats();
        }
        state.logs.take()
    };
    let stats = match collector {
        Some(collector) => finish_logs(collector, log_timeout),
        None => shared.lock().log_stats,
    };

    // 先释放监控句柄：若仍有残留进程树，`ExitWatcher::Drop` 会终止它。
    // 这一步可能等待监控线程收尾，因此放在状态置为 stopped 之前，
    // 让 `stop(timeout)` 的等待仍然受同一个 deadline 约束。
    let watcher = shared.lock().watcher.take();
    drop(watcher);

    // 停止诊断先取出：`event` 随后按值消费。
    let stop = event.as_ref().and_then(|event| event.stop.clone());
    // 强制兜底（graceful 宽限到期）与“根退出后树未空”的强制清树都算 forced。
    let forced_kill = event.as_ref().is_some_and(|event| {
        event.forced_tree_kill || event.stop.as_ref().is_some_and(|stop| stop.forced)
    });

    let snapshot = {
        let mut state = shared.lock();
        state.log_stats = stats;
        if let Some(event) = event {
            state.exit_code = Some(event.exit_code);
            state.exit_reason = Some(if event.stop_requested {
                ExitReason::UserStop
            } else {
                ExitReason::Natural
            });
            state.tree_empty = Some(event.tree_empty);
            state.forced_kill = forced_kill;
            state.last_stop = stop;
        } else {
            state.exit_code = None;
            state.exit_reason = None;
            state.tree_empty = None;
            state.forced_kill = false;
            state.last_stop = None;
            state.last_error = error;
        }
        state.status = TaskStatus::Stopped;
        state.pid = None;
        state.snapshot(&task_id, &log_path)
    };

    if let Some(sender) = &events {
        let _ = sender.send(RuntimeEvent::Stopped(snapshot));
    }
    shared.changed.notify_all();
}

/// 有界地等待日志线程结束；能 join 就 join，否则放弃并返回当前统计。
fn finish_logs(collector: LogCollector, timeout: Duration) -> LogStats {
    let stats = collector.stats();
    if collector.wait(timeout) {
        collector.join()
    } else {
        // Drop 会通知读线程退出并做有界等待，不会无限阻塞完成线程。
        drop(collector);
        stats
    }
}

/// 追加一个环境变量覆盖项。
///
/// 任务 env 先加入、保留变量后加入；进程层按同名（ASCII 大小写不敏感）
/// 以最后一项为准，因此保留变量无法被任务 env 覆盖。
fn push_env(env: &mut Vec<(OsString, OsString)>, key: &str, value: &OsStr) {
    env.push((OsString::from(key), value.to_os_string()));
}

/// 生成进程内唯一实例 id（任务 id + 序号 + 纳秒时间戳）。
fn next_instance_id(task_id: &str) -> String {
    let sequence = INSTANCE_SEQ.fetch_add(1, Ordering::Relaxed);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    format!("{task_id}-{sequence}-{nanos:x}")
}
