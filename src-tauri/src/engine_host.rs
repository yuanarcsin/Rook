//! Tauri 宿主层：命令注册、配置监听、调度线程与事件广播。
//!
//! 本文件只做“翻译”：把 Tauri 的 `State`/`AppHandle` 转成对 [`crate::engine::Engine`]
//! 的调用，并把结果序列化回前端；所有可测逻辑都在 `engine`（core）里。

use std::path::PathBuf;
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex, MutexGuard, Weak};
use std::thread;
use std::time::{Duration, Instant};

use serde::Serialize;
use tauri::{AppHandle, Emitter, Manager};

use crate::cli::CliArgs;
use crate::config::{self, ConfigWatchState, EnvVars, LoadedConfig, SystemEnv, WatchSpec};
use crate::engine::{
    Action, CommandOutcome, CommandPlan, Engine, SCHEDULER_TICK, StartFailure, StartJob, StopJob,
};
use crate::ipc_error::IpcError;
use crate::logging::Logging;
use crate::runtime::{PowerWiring, RuntimeEvent, RuntimeSnapshot, ensure_heartbeat_dir};
use crate::snapshot::{RuntimeView, Snapshot};
use crate::state_store::Timestamp;

/// 在 `setup` 中调用：加载配置、注册全局状态并启动运行时事件转发。
pub fn initialize(app: &AppHandle, args: CliArgs, logging: Logging) {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let (events, receiver) = mpsc::channel();
    let (wake, wake_receiver) = mpsc::channel();
    // M3-6b：注册休眠/唤醒通知（无窗口回调）；失败只降级，不影响启动。
    let power_wiring = PowerWiring::install();
    let watch_state = Arc::new(ConfigWatchState::default());
    let engine = Arc::new(Mutex::new(Engine::new(
        args,
        cwd,
        logging,
        events,
        wake.clone(),
        power_wiring,
        Arc::clone(&watch_state),
    )));
    spawn_event_forwarder(app.clone(), Arc::downgrade(&engine), receiver);
    spawn_scheduler(app.clone(), Arc::downgrade(&engine), wake_receiver);
    // 启动时配置不可用也默认开启监听：用户修好文件后无需手动点“重新加载”。
    {
        let mut guard = lock(&engine);
        if guard.loaded.is_none() && !guard.watch_state.enabled() {
            guard
                .watch_state
                .configure(true, Duration::from_millis(1000));
            guard.refresh();
        }
    }
    start_config_watch(app, &engine, &watch_state);
    // 自动启动/退避恢复排期可能立即到期，唤醒调度线程执行首个 tick。
    let _ = wake.send(());
    let (tasks, issues) = {
        let guard = lock(&engine);
        (guard.snapshot.tasks.len(), guard.snapshot.issues.len())
    };
    app.manage(engine);
    tracing::info!(event = "app.ready", tasks, issues, "监督内核已就绪");
}

/// 停止配置监听并等待线程退出（应用退出时调用；锁外 join）。
pub(crate) fn stop_config_watch(engine: &Arc<Mutex<Engine>>) {
    let handle = lock(engine).watch_handle.take();
    drop(handle);
    lock(engine).watch_state.disable();
}

/// 启动配置监听（`[supervisor].watch_config=true` 时）。
///
/// 监听线程只持 `Weak<Mutex<Engine>>`；回调里的文件读取/校验在 Engine 锁外，
/// 只有“应用已校验的配置/记录错误”才短暂回锁。
fn start_config_watch(
    app: &AppHandle,
    engine: &Arc<Mutex<Engine>>,
    watch_state: &Arc<ConfigWatchState>,
) {
    let spec = {
        let guard = lock(engine);
        if !guard.watch_state.enabled() {
            return;
        }
        guard.watch_spec()
    };
    let Some(spec) = spec else {
        watch_state.disable();
        lock(engine).refresh();
        tracing::warn!(
            event = "config.watch_error",
            "配置文件路径未知，无法启动自动重载监听"
        );
        return;
    };

    let weak = Arc::downgrade(engine);
    let app = app.clone();
    let tick_spec = spec.clone();
    let handle = config::watch::spawn(spec, Arc::clone(watch_state), move || {
        config_watch_tick(&app, &weak, &tick_spec);
    });
    let mut guard = lock(engine);
    guard.watch_handle = Some(handle);
    // 线程可能在启动瞬间失败/回退轮询，立即把状态投影进快照。
    guard.refresh();
}

/// 一次监听回调：读文件 → 自身保存抑制 → 锁外校验 → 合法才应用。
fn config_watch_tick(app: &AppHandle, engine: &Weak<Mutex<Engine>>, spec: &WatchSpec) {
    let Some(engine) = engine.upgrade() else {
        return;
    };

    // 1) 文件读取在 Engine 锁外。
    let bytes = match std::fs::read(&spec.config_path) {
        Ok(bytes) => bytes,
        Err(error) => {
            let error = config::LoadError::io(&spec.config_path, error);
            let (snapshot, changed) = lock(&engine).record_watch_error(&error);
            if changed {
                broadcast(app, &snapshot);
            }
            return;
        }
    };
    let revision = config::revision_of(&bytes);

    // 2) 自身保存抑制：UI 保存后已 reload，同内容事件直接跳过。
    {
        let guard = lock(&engine);
        if !guard.watch_state.enabled() {
            return;
        }
        if !config::watch::should_reload(guard.watch_state.applied_revision().as_deref(), &revision)
        {
            tracing::debug!(
                event = "config.watch_skipped",
                revision = %revision,
                "配置文件变化与已应用内容相同（自身保存），跳过重载"
            );
            return;
        }
    }

    // 3) 校验在 Engine 锁外；用同一份字节计算 revision，避免二次读取。
    let loaded = load_watch_bytes(spec, &bytes);

    // 4) 回锁应用：合法走完整 reconcile，非法只记录监听错误。
    let mut guard = lock(&engine);
    if !guard.watch_state.enabled() {
        return;
    }
    if !config::watch::should_reload(guard.watch_state.applied_revision().as_deref(), &revision) {
        return;
    }
    match loaded {
        Ok(loaded) => {
            let snapshot = guard.apply_watched_loaded(loaded);
            drop(guard);
            broadcast(app, &snapshot);
            flush_notifications(app, &engine);
        }
        Err(error) => {
            let (snapshot, changed) = guard.record_watch_error(&error);
            drop(guard);
            if changed {
                broadcast(app, &snapshot);
            }
        }
    }
}

/// 用监听读到的同一份字节重新走完整配置校验。
fn load_watch_bytes(spec: &WatchSpec, bytes: &[u8]) -> Result<LoadedConfig, config::LoadError> {
    let env = SystemEnv;
    let local_appdata = env.var("LOCALAPPDATA").map(PathBuf::from);
    config::load_bytes(
        &config::LoadContext {
            config_path: &spec.config_path,
            data_dir_override: spec.data_dir_override.as_deref(),
            local_appdata: local_appdata.as_deref(),
            env: &env,
        },
        bytes,
    )
}

/// M3-4b 调度线程：250ms tick + 事件/命令唤醒；实际探测/启停在锁外执行。
fn spawn_scheduler(app: AppHandle, engine: Weak<Mutex<Engine>>, wake_receiver: Receiver<()>) {
    let spawned = thread::Builder::new()
        .name("cs-supervisor".to_owned())
        .spawn(move || {
            loop {
                // 超时即 tick；收到唤醒则立即执行一轮。
                let _ = wake_receiver.recv_timeout(SCHEDULER_TICK);
                let Some(engine) = engine.upgrade() else {
                    // Engine 已释放（应用退出）：线程自行结束，不阻止进程退出。
                    break;
                };
                scheduler_pass(&app, &engine);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!("创建监督调度线程失败：{error}");
    }
}

/// 执行一轮监督调度：健康探测/启动/停止都在 Engine 锁外（阻塞操作不持锁）。
fn scheduler_pass(app: &AppHandle, engine: &Arc<Mutex<Engine>>) {
    let work = {
        let mut guard = lock(engine);
        guard.scheduler_work(Instant::now())
    };
    let mut changed = work.changed;

    for probe in work.probes {
        let mono_now = Instant::now();
        let mut monitor = probe.monitor;
        let verdict = monitor.check(mono_now);
        let (probe_changed, stop_job) = {
            let mut guard = lock(engine);
            guard.apply_probe(
                &probe.task_id,
                &probe.instance_id,
                monitor,
                verdict,
                probe.capture,
                mono_now,
            )
        };
        changed |= probe_changed;
        if let Some(stop_job) = stop_job {
            changed |= run_stop_job(engine, stop_job);
        }
    }

    for start_job in work.starts {
        changed |= run_start_job(engine, start_job);
    }
    for stop_job in work.stops {
        changed |= run_stop_job(engine, stop_job);
    }

    let snapshot = {
        let mut guard = lock(engine);
        if changed {
            guard.refresh();
            guard.persist();
        }
        guard.snapshot.clone()
    };
    if changed {
        broadcast(app, &snapshot);
    }
    flush_notifications(app, engine);
}

/// 锁外启动一个自动启动/退避重启实例；回锁后按结果迁移状态机。
fn run_start_job(engine: &Arc<Mutex<Engine>>, job: StartJob) -> bool {
    let outcome: Result<RuntimeSnapshot, StartFailure> = match ensure_heartbeat_dir(&job.context) {
        Err(message) => Err(StartFailure {
            stage: "heartbeat_dir",
            message,
        }),
        Ok(()) => job
            .runtime
            .start_with_context(&job.context)
            .map_err(|error| StartFailure {
                stage: error.stage().key(),
                message: error.to_string(),
            }),
    };

    let mut guard = lock(engine);
    let reloaded = guard.gate.finish_command(&job.task_id, job.generation);
    if let Some(supervisor) = guard.supervisors.get_mut(&job.task_id) {
        supervisor.pending_start = false;
    }
    match outcome {
        Err(failure) => {
            guard.apply_start_failure(&job.task_id, failure);
        }
        Ok(snapshot) => {
            tracing::info!(
                event = "task.start",
                task = %job.task_id,
                reason = job.context.start_reason.as_str(),
                pid = snapshot.pid.map(i64::from).unwrap_or(-1),
                instance = snapshot.instance_id.as_deref().unwrap_or("-"),
                "任务进程已启动"
            );
            guard.wake();
        }
    }
    if reloaded {
        guard.resync_after_reload();
    }
    true
}

/// 锁外停止一个实例（健康失败/资源超限）；退出事件（cause=Health）负责后续迁移。
fn run_stop_job(engine: &Arc<Mutex<Engine>>, job: StopJob) -> bool {
    let result = job.runtime.stop(job.timeout);
    if let Err(error) = &result {
        tracing::warn!(
            task = %job.task_id,
            error = %error,
            reason = job.reason,
            "停止实例未在超时内完成（{}）",
            job.reason
        );
    }
    let mut guard = lock(engine);
    let reloaded = guard.gate.finish_command(&job.task_id, job.generation);
    if let Err(error) = &result {
        // B2：停止失败/超时不能静默；保留 stop_cause 并安排 1s/2s/4s 重试，
        // 多次失败升级 needs_human(stop_timeout) 并继续重试。
        guard.record_stop_failure(&job.task_id, &error.to_string());
    }
    if reloaded {
        guard.resync_after_reload();
    }
    true
}

/// 把各运行时的事件统一转成 `snapshot:changed`。
///
/// 只持有 `Weak`：应用退出、Engine 被释放后 `upgrade` 失败，线程立即结束，
/// 不会在 AppHandle 失效后继续发送事件。
fn spawn_event_forwarder(
    app: AppHandle,
    engine: Weak<Mutex<Engine>>,
    events: Receiver<RuntimeEvent>,
) {
    let spawned = std::thread::Builder::new()
        .name("cs-runtime-events".to_owned())
        .spawn(move || {
            while let Ok(event) = events.recv() {
                let Some(engine) = engine.upgrade() else {
                    break;
                };
                let snapshot = {
                    let mut guard = lock(&engine);
                    let mono_now = Instant::now();
                    let wall_now = Timestamp::now();
                    let changed = guard.handle_runtime_event(&event, mono_now, wall_now);
                    guard.refresh();
                    if changed {
                        guard.persist();
                    }
                    // 新实例/退出都可能触发探测或自动重启，立即唤醒调度线程。
                    guard.wake();
                    guard.snapshot.clone()
                };
                tracing::debug!(
                    task = %event.snapshot().id,
                    status = event.status().as_str(),
                    "运行时状态变化"
                );
                broadcast(&app, &snapshot);
                // 通知在锁外投递（`notification:raised` 只到已打开窗口）。
                flush_notifications(&app, &engine);
                drop(engine);
            }
        });
    if let Err(error) = spawned {
        tracing::warn!("创建运行时事件转发线程失败：{error}");
    }
}

pub(crate) fn lock(engine: &Mutex<Engine>) -> MutexGuard<'_, Engine> {
    engine
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub(crate) fn engine_handle(app: &AppHandle) -> Arc<Mutex<Engine>> {
    Arc::clone(app.state::<Arc<Mutex<Engine>>>().inner())
}

/// 主窗口关闭：记录一次性“仍在托盘运行”提示（在独立线程里落盘，不阻塞 UI）。
pub(crate) fn note_window_closed(app: &AppHandle) {
    let engine = engine_handle(app);
    lock(&engine).note_window_closed();
}

/// 当前快照（短锁，不重新加载配置）；供托盘首次构建菜单使用。
pub(crate) fn current_snapshot(app: &AppHandle) -> Snapshot {
    let engine = engine_handle(app);
    lock(&engine).snapshot.clone()
}

/// 广播快照：先发给前端，再重建托盘菜单。
///
/// 调用点都在释放 Engine 锁之后，`tray::refresh` 自身也不再取 Engine 锁，
/// 因此不会出现「引擎锁 → 主线程 → 引擎锁」的交叉等待。
pub(crate) fn broadcast(app: &AppHandle, snapshot: &Snapshot) {
    if let Err(error) = app.emit("snapshot:changed", snapshot) {
        tracing::warn!("发送 snapshot:changed 失败：{error}");
    }
    crate::tray::refresh(app, snapshot);
}

/// 投递通知（M3-5b 降级路径 + 2026-11-05 系统 Toast）。
///
/// - `notification:raised` 始终发给已打开的窗口（应用内 banner）；
/// - 主窗口**未聚焦**且系统 Toast 可用时，额外发一条系统通知；聚焦时只显示 banner，
///   避免同一事件重复提醒；
/// - 窗口关闭/隐藏/失焦时 Toast 照常出现；这里绝不创建、显示或聚焦窗口；
/// - 总开关 `[notifications].enabled=false`、任务 `notify=false`、节流/去重仍由
///   现有 [`NotificationCenter`] 决定，本函数不绕过。
pub(crate) fn flush_notifications(app: &AppHandle, engine: &Arc<Mutex<Engine>>) {
    let (raised, enabled) = {
        let mut guard = lock(engine);
        let enabled = guard
            .loaded
            .as_ref()
            .is_none_or(|loaded| loaded.config.notifications.enabled);
        (guard.drain_notifications(), enabled)
    };
    if raised.is_empty() {
        return;
    }
    if !enabled {
        tracing::debug!(
            count = raised.len(),
            "通知总开关已关闭，跳过 banner 与系统通知"
        );
        return;
    }

    let toast_available = crate::toast::is_available();
    let window_focused = main_window_focused(app);

    for notification in raised {
        let send_toast = toast_available && !window_focused;
        tracing::info!(
            event = "notification.raise",
            kind = notification.kind,
            task = notification.task_id.as_deref().unwrap_or("-"),
            count = notification.count,
            toast = send_toast,
            "通知（应用内 banner{}）",
            if send_toast {
                "；窗口未聚焦，同时发送系统 Toast"
            } else {
                ""
            }
        );
        if let Err(error) = app.emit("notification:raised", &notification) {
            tracing::warn!("发送 notification:raised 失败：{error}");
        }
        if !send_toast {
            continue;
        }
        if crate::toast::notify(
            &notification.title,
            &notification.message,
            notification.kind,
        ) {
            tracing::info!(
                event = "toast",
                kind = notification.kind,
                task = notification.task_id.as_deref().unwrap_or("-"),
                "系统通知已投递"
            );
        } else {
            tracing::warn!(
                event = "toast_error",
                kind = notification.kind,
                task = notification.task_id.as_deref().unwrap_or("-"),
                "系统通知入队失败（通知线程可能已退出）"
            );
        }
    }
}

/// 主窗口是否存在、可见且处于前台（关闭/隐藏/最小化失焦都返回 `false`）。
pub(crate) fn main_window_focused(app: &AppHandle) -> bool {
    app.get_webview_window("main").is_some_and(|window| {
        window.is_visible().unwrap_or(false) && window.is_focused().unwrap_or(false)
    })
}

/// 配置写盘成功后的统一收尾（A5）：reload → 广播 → 投递通知 → 一条摘要日志。
///
/// 五条写入路径（配置文本、任务草稿、删除任务、运行时段、插件导入/同步）都只
/// 负责写盘并自行判断 `changed`，其余动作全部收敛到这里，
/// 避免出现“某条路径忘了广播/flush/日志”的不一致。
///
/// `source` 是稳定标识（`text`/`draft`/`delete`/`schedule`/`plugin_import`/
/// `plugin_sync`/`backup`），写入 `config.applied` 事件；`revision`/`issues` 来自落盘结果。
pub(crate) fn publish_config_change(
    app: &AppHandle,
    engine: &Arc<Mutex<Engine>>,
    revision: &str,
    issues: usize,
    source: &str,
) -> Snapshot {
    // 不信任内存草稿：以落盘内容为准重新加载，成功后才算"已保存"。
    let snapshot = lock(engine).reload();
    broadcast(app, &snapshot);
    // 通知在锁外投递（`notification:raised` 只到已打开窗口）。
    flush_notifications(app, engine);
    tracing::info!(
        event = "config.applied",
        source = %source,
        revision = %revision,
        issues = issues,
        "配置已保存并重新加载"
    );
    snapshot
}

/// 启停命令的返回值：本次动作 + 运行时视图 + 最新完整快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskCommandResult {
    pub task_id: String,
    /// `"start"` / `"stop"` / `"restart"`。
    pub action: &'static str,
    /// 动作完成后的运行时视图（失败时命令返回 [`IpcError`]，不会走到这里）。
    pub runtime: Option<RuntimeView>,
    /// 动作完成后的完整快照（与 `snapshot:changed` 内容一致）。
    pub snapshot: Snapshot,
}

/// 托盘菜单触发的启停动作（M2-5）。
///
/// 在 Tauri 的异步运行时里执行，菜单事件回调本身立即返回；动作结果只通过
/// `snapshot:changed` 与托盘菜单刷新体现，绝不打开/聚焦窗口。
pub(crate) fn spawn_task_action(app: &AppHandle, id: &str, action: &str) {
    let Some(action) = Action::parse(action) else {
        tracing::warn!(action, "未知的托盘任务动作");
        return;
    };
    let app = app.clone();
    let id = id.to_owned();
    tauri::async_runtime::spawn(async move {
        match run_command(app, id.clone(), action).await {
            Ok(result) => tracing::debug!(
                task = %result.task_id,
                action = result.action,
                "托盘任务操作完成"
            ),
            Err(error) => tracing::warn!(
                task = %id,
                action = action.as_str(),
                stage = %error.stage,
                message = %error.message,
                "托盘任务操作失败"
            ),
        }
    });
}

pub(crate) async fn run_command(
    app: AppHandle,
    id: String,
    action: Action,
) -> Result<TaskCommandResult, IpcError> {
    let engine = engine_handle(&app);
    let worker_app = app.clone();
    let worker_id = id.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        execute(&engine, &worker_app, &worker_id, action)
    })
    .await;
    joined.map_err(|error| {
        IpcError::with_code(
            "join",
            None,
            format!("任务操作线程异常结束：{error}"),
            Some(id),
        )
    })?
}

/// 在工作线程里执行一次启停：取实例 → 调用运行时 → 刷新快照 → 广播。
fn execute(
    engine: &Arc<Mutex<Engine>>,
    app: &AppHandle,
    id: &str,
    action: Action,
) -> Result<TaskCommandResult, IpcError> {
    // 只在 Engine 锁内做检查/登记 in-flight，随后释放锁：阻塞的启停不会挡住
    // 快照读取/事件转发；命令执行期间 reload 会保留该任务的旧 runtime。
    let CommandPlan {
        runtime,
        previous,
        timeout,
        generation: command_generation,
        context,
    } = {
        let mut guard = lock(engine);
        guard.begin_command(id, action)?
    };

    // 每次启动/重启都显式带上按当前配置算好的任务上下文
    // （任务名/HEARTBEAT/DATA_DIR/CONFIG），运行时不会猜测默认路径。
    let context = context.unwrap_or_else(|| runtime.instance_context());
    // Q11：启动前创建心跳目录并验证可写；失败按启动前检查失败处理（不 spawn）。
    let precheck = if matches!(action, Action::Start | Action::Restart) {
        ensure_heartbeat_dir(&context).err()
    } else {
        None
    };
    // 运行中 Restart 的启动前检查发生在停止旧实例之前：旧进程仍在运行，
    // 失败时同样不能按启动失败清理监督状态。
    let had_previous = previous.is_some();
    let outcome = match &precheck {
        Some(message) => CommandOutcome::Precheck(message.clone()),
        None => match action {
            Action::Start => CommandOutcome::Done(runtime.start_with_context(&context)),
            Action::Stop => CommandOutcome::Done(runtime.stop(timeout)),
            Action::Restart => match previous {
                // M3-6a：运行中重启时新运行时已按当前配置建好，旧实例先停，
                // 停止成功后才换表并用新配置/上下文启动（失败保持旧实例不动）。
                Some(old) => match old.stop(timeout) {
                    Ok(_) => {
                        {
                            let mut guard = lock(engine);
                            guard.install_restart_runtime(id, Arc::clone(&runtime));
                        }
                        CommandOutcome::Done(runtime.start_with_context(&context))
                    }
                    // 停止阶段失败：旧进程可能仍在，交给收尾恢复旧监督状态。
                    Err(error) => CommandOutcome::RestartStopFailed(error),
                },
                None => CommandOutcome::Done(runtime.restart_with_context(timeout, &context)),
            },
        },
    };

    // 无论成功失败都刷新快照并保存：失败会写入 last_error/监督状态，界面需要看到。
    let snapshot = {
        let mut guard = lock(engine);
        guard.finish_command(id, action, command_generation);
        match &outcome {
            CommandOutcome::Precheck(message) => {
                if had_previous {
                    guard.recover_restart_stop_failure(id, message);
                } else {
                    guard.apply_start_failure(
                        id,
                        StartFailure {
                            stage: "heartbeat_dir",
                            message: message.clone(),
                        },
                    );
                }
            }
            CommandOutcome::RestartStopFailed(error) => {
                guard.recover_restart_stop_failure(id, error.message());
            }
            CommandOutcome::Done(Err(error))
                if matches!(action, Action::Start | Action::Restart) =>
            {
                guard.apply_start_failure(
                    id,
                    StartFailure {
                        stage: error.stage().key(),
                        message: error.to_string(),
                    },
                );
            }
            CommandOutcome::Done(_) => {}
        }
        guard.refresh();
        guard.persist();
        guard.snapshot.clone()
    };
    broadcast(app, &snapshot);
    flush_notifications(app, engine);

    match outcome {
        CommandOutcome::Precheck(message) => Err(IpcError::with_code(
            "heartbeat_dir",
            None,
            message,
            Some(id.to_owned()),
        )),
        CommandOutcome::RestartStopFailed(error) => {
            tracing::warn!(task = %id, action = action.as_str(), error = %error, "重启时停止旧实例失败");
            Err(IpcError::from_runtime(id, &error))
        }
        CommandOutcome::Done(outcome) => {
            let runtime_snapshot: RuntimeSnapshot = outcome.map_err(|error| {
                tracing::warn!(task = %id, action = action.as_str(), error = %error, "任务操作失败");
                IpcError::from_runtime(id, &error)
            })?;
            match action {
                Action::Start => tracing::info!(
                    event = "task.start",
                    task = %id,
                    reason = "user_start",
                    pid = runtime_snapshot.pid.map(i64::from).unwrap_or(-1),
                    instance = runtime_snapshot.instance_id.as_deref().unwrap_or("-"),
                    "任务进程已启动"
                ),
                Action::Restart => tracing::info!(
                    event = "task.start",
                    task = %id,
                    reason = "restart",
                    pid = runtime_snapshot.pid.map(i64::from).unwrap_or(-1),
                    instance = runtime_snapshot.instance_id.as_deref().unwrap_or("-"),
                    "任务进程已启动"
                ),
                Action::Stop => {
                    // 手动停止在运行时收敛后记录完整诊断：方式/宽限/兜底/耗时。
                    let report = runtime.last_stop_report();
                    tracing::info!(
                        event = "task.stop",
                        task = %id,
                        reason = "user_stop",
                        method = report
                            .as_ref()
                            .map(|report| report.method.as_str())
                            .unwrap_or("-"),
                        grace_ms = runtime.spec().watch.grace.as_millis() as u64,
                        forced = runtime_snapshot.forced_kill,
                        waited_ms = report
                            .as_ref()
                            .map(|report| report.waited.as_millis() as i64)
                            .unwrap_or(-1),
                        "任务实例已停止"
                    );
                }
            }
            Ok(TaskCommandResult {
                task_id: id.to_owned(),
                action: action.as_str(),
                runtime: Some(RuntimeView::capture(&runtime_snapshot)),
                snapshot,
            })
        }
    }
}
