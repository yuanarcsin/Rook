//! M2-2：每实例退出监控（根进程退出 → 等 Job 树空 → 必要时强制清树）。
//!
//! 每个被监控实例有且只有一个等待线程：
//!
//! 1. `WaitForSingleObject` 等**根进程**退出（100ms 切片，避免依赖无限等待）；
//! 2. `GetExitCodeProcess` 取退出码；
//! 3. 轮询 Job 的 `ActiveProcesses`，直到为 0 或超过 [`WatchOptions::tree_wait`]；
//! 4. 仍非 0 时 `TerminateJobObject` 强制清树，再确认一次是否已空；
//! 5. 组装 [`ExitEvent`] 发出——**同一实例只发一次**，然后标记完成。
//!
//! # 线程/句柄安全
//!
//! - 根进程句柄与 Job 句柄由 [`std::sync::Arc`] 共享，等待线程、[`ExitWatcher::request_stop`]
//!   和 Drop 看到的是同一个有效句柄；句柄只在最后一个引用释放时关闭；
//! - `Job` 的 `KILL_ON_JOB_CLOSE` 语义因此仍然成立：所有引用都释放后 Job 关闭，
//!   残余进程树被系统回收；
//! - 事件里的 `instance_id` 由调用方在 [`ExitWatcher::start`] 时提供；本模块不做全局去重，
//!   旧实例的事件由调用方按 id 丢弃。
//!
//! # 停止语义
//!
//! [`ExitWatcher::request_stop`] 幂等：第一次调用按 [`StopMode`] 执行停止，
//! 之后的重复调用不再重复发信号/强杀，只按调用方给的 timeout 等既有流程收敛。
//! 停止只影响事件里的 `stop_requested` 字段，不会额外发送第二个事件。
//!
//! - [`StopMode::Terminate`]：立即 `TerminateJobObject`（v1 旧行为）；
//! - [`StopMode::Graceful`]：先向根进程组发 `CTRL_BREAK_EVENT`，等根退出且
//!   Job 变空（最多 [`WatchOptions::grace`]），仍未退出则 `TerminateJobObject`
//!   兜底，并在 timeout 剩余预算内确认根退出/树空。
//!
//! 优雅成功的判据是**根进程退出 + Job 空**，不看退出码：控制台事件的默认
//! 处理程序可能让脚本以 `0xC000013A` 结束，但清理已经完成（spike §4.2）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime};

use super::error::{SpawnError, Stage};
use super::win32::{ConsoleSignal, Job, OwnedHandle, exit_code, send_ctrl_break, wait_process};
use super::{Child, ExitStatus};

/// 强制清树时使用的退出码（与 M2-1 `Child::terminate` 保持一致）。
const STOP_EXIT_CODE: u32 = 1;

/// 等待根进程的切片长度：让等待线程不必依赖“无限等待”，也为后续
/// 增加停止标志轮询留出位置。
const WAIT_SLICE: Duration = Duration::from_millis(100);

/// 停止等待时轮询 Job/根进程的切片：比 `WAIT_SLICE` 更细，避免宽限期
/// 结束到兜底之间出现明显延迟。
const STOP_POLL: Duration = Duration::from_millis(25);

/// 强制清树后的确认窗口：`TerminateJobObject` 返回后进程可能还要几毫秒才从
/// Job 记账里消失。
const FORCE_CONFIRM: Duration = Duration::from_secs(1);

/// 无法取得退出码时的哨兵值（等待/查询句柄失败，正常路径不会出现）。
pub const UNKNOWN_EXIT_CODE: u32 = u32::MAX;

/// 停止方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StopMode {
    /// 立即 `TerminateJobObject`：脚本没有清理机会。
    Terminate,
    /// 先发 `CTRL_BREAK_EVENT`，宽限期内未退出再用 `TerminateJobObject` 兜底。
    #[default]
    Graceful,
}

impl StopMode {
    /// 稳定的英文标识（日志/事件共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Terminate => "terminate",
            Self::Graceful => "graceful",
        }
    }
}

impl std::fmt::Display for StopMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次停止请求的诊断结果。
///
/// 随 [`ExitEvent`] 一起发出，供上层日志记录 `method` / `forced` / `waited_ms`；
/// `forced=true` 表示走了 `TerminateJobObject` 兜底，**不算优雅成功**。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StopReport {
    /// 本次停止使用的方式。
    pub method: StopMode,
    /// 优雅停止的宽限期（terminate 方式下无意义）。
    pub grace: Duration,
    /// 是否因宽限内未退出/信号未送达而强制 `TerminateJobObject`。
    pub forced: bool,
    /// 从请求停止到根退出 + 树空确认的耗时。
    pub waited: Duration,
    /// 控制台信号未能送达时的可读原因（此时已降级为 Job 兜底）。
    pub signal_error: Option<String>,
}

/// 退出监控参数。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WatchOptions {
    /// 根进程退出后，最多等多久让 Job 内剩余进程自然结束；超时则强制清树。
    pub tree_wait: Duration,
    /// 轮询 Job `ActiveProcesses` 的间隔。
    pub poll_interval: Duration,
    /// Drop 时等待等待线程结束的上限（超过后放弃 join，避免阻塞调用方）。
    pub stop_timeout: Duration,
    /// 停止方式（默认优雅）。
    pub method: StopMode,
    /// 优雅停止的宽限期：发 `CTRL_BREAK` 后等根退出/树空的上限。
    pub grace: Duration,
}

impl Default for WatchOptions {
    fn default() -> Self {
        Self {
            tree_wait: Duration::from_secs(2),
            poll_interval: Duration::from_millis(25),
            stop_timeout: Duration::from_secs(5),
            method: StopMode::Graceful,
            grace: Duration::from_millis(2500),
        }
    }
}

/// 一次退出监控的最终结果；同一实例只会产生一个。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExitEvent {
    /// 调用方提供的实例标识（旧实例事件由调用方按此丢弃）。
    pub instance_id: String,
    /// 事件完成时间（根退出 + 树空确认后），供上层记录/排序。
    pub finished_at: SystemTime,
    /// 根进程退出码；无法取得时为 [`UNKNOWN_EXIT_CODE`]。
    pub exit_code: u32,
    /// 事件发出时 Job 内是否已经没有活动进程。
    pub tree_empty: bool,
    /// 从根进程退出到确认树空（或放弃确认）所花的毫秒数。
    pub tree_wait_ms: u64,
    /// 是否因等待超时而调用了 `TerminateJobObject`。
    pub forced_tree_kill: bool,
    /// 事件发生前是否收到过 [`ExitWatcher::request_stop`]。
    pub stop_requested: bool,
    /// 停止请求的诊断信息；自然退出时为 `None`。
    pub stop: Option<StopReport>,
}

/// 停止失败：Win32 调用失败，或根进程在给定时间内没有退出。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StopError {
    /// `TerminateJobObject` / 等待根进程失败。
    Win32(SpawnError),
    /// 根进程在 timeout 内没有退出（进程可能仍未终止）。
    Timeout {
        instance_id: String,
        waited: Duration,
    },
}

impl std::fmt::Display for StopError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Win32(error) => write!(f, "停止进程失败：{error}"),
            Self::Timeout {
                instance_id,
                waited,
            } => write!(
                f,
                "停止实例 {instance_id} 超时：等待根进程退出 {}ms 未完成",
                waited.as_millis()
            ),
        }
    }
}

impl std::error::Error for StopError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Win32(error) => Some(error),
            Self::Timeout { .. } => None,
        }
    }
}

/// 等待线程与停止调用共享的完成状态。
#[derive(Default)]
struct WatchState {
    stop_requested: AtomicBool,
    /// 第一次停止请求的完整记录；重复调用不覆盖。
    stop: Mutex<Option<StopRecord>>,
    done: Mutex<bool>,
    done_signal: Condvar,
}

/// 一次停止请求的进行中记录（事件发出时转成 [`StopReport`]）。
struct StopRecord {
    method: StopMode,
    grace: Duration,
    started: Instant,
    forced: bool,
    signal_error: Option<String>,
}

impl WatchState {
    /// 记录第一次停止请求；返回 `true` 表示这是第一次（调用方应执行停止动作）。
    fn begin_stop(&self, method: StopMode, grace: Duration) -> bool {
        let mut stop = self
            .stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if stop.is_some() {
            return false;
        }
        *stop = Some(StopRecord {
            method,
            grace,
            started: Instant::now(),
            forced: false,
            signal_error: None,
        });
        self.stop_requested.store(true, Ordering::SeqCst);
        true
    }

    fn stop_requested(&self) -> bool {
        self.stop_requested.load(Ordering::SeqCst)
    }

    /// 标记走了 `TerminateJobObject` 兜底。
    fn record_forced(&self) {
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            stop.forced = true;
        }
    }

    /// 记录控制台信号未能送达的原因（可读文本，已降级）。
    fn record_signal_error(&self, message: String) {
        if let Some(stop) = self
            .stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_mut()
        {
            stop.signal_error = Some(message);
        }
    }

    /// 组装诊断报告；未请求停止时为 `None`。
    fn report(&self) -> Option<StopReport> {
        self.stop
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .as_ref()
            .map(|stop| StopReport {
                method: stop.method,
                grace: stop.grace,
                forced: stop.forced,
                waited: stop.started.elapsed(),
                signal_error: stop.signal_error.clone(),
            })
    }

    /// 等待线程发完事件后调用。
    fn finish(&self) {
        let mut done = self
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *done = true;
        self.done_signal.notify_all();
    }

    /// 有界等待线程完成；返回 `true` 表示已完成。
    fn wait_done(&self, timeout: Duration) -> bool {
        let done = self
            .done
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (done, _) = self
            .done_signal
            .wait_timeout_while(done, timeout, |done| !*done)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *done
    }
}

/// 每实例退出监控。
///
/// 由 [`ExitWatcher::start`] 创建；Drop 时停止进程树、等待线程结束并关闭句柄。
/// 事件通过调用方提供的 `Sender<ExitEvent>` 送出，不依赖 Tauri。
pub struct ExitWatcher {
    instance_id: String,
    process: Arc<OwnedHandle>,
    job: Job,
    /// 根进程 PID（同时是 `CREATE_NEW_PROCESS_GROUP` 的进程组 id）。
    pid: u32,
    state: Arc<WatchState>,
    thread: Option<JoinHandle<()>>,
    options: WatchOptions,
}

impl ExitWatcher {
    /// 接管 `child` 的根进程与 Job，启动等待线程。
    ///
    /// **管道**：`start` 会消费 `child`，stdout/stderr 读端随之关闭。需要日志采集时，
    /// 先调用 [`Child::take_stdout`] / [`Child::take_stderr`] 把读端取走。
    ///
    /// 线程创建失败会返回 [`Stage::SpawnWatcher`] 错误；此时 `child` 已被消费，
    /// Job 的 `KILL_ON_JOB_CLOSE` 会回收进程树。
    pub fn start(
        instance_id: impl Into<String>,
        child: Child,
        events: Sender<ExitEvent>,
        options: WatchOptions,
    ) -> Result<Self, SpawnError> {
        let instance_id = instance_id.into();
        let pid = child.pid();
        let process = child.process;
        let job = child.job;
        // 其余字段（管道读端、缓存退出码等）随 `child` 在本函数结束时释放。

        let state = Arc::new(WatchState::default());
        let thread_process = Arc::clone(&process);
        let thread_job = job.clone();
        let thread_state = Arc::clone(&state);
        let thread_id = instance_id.clone();
        let thread_options = options.clone();

        let thread = thread::Builder::new()
            .name("cs-exit-watch".to_owned())
            .spawn(move || {
                let event = collect(
                    thread_id,
                    &thread_process,
                    &thread_job,
                    &thread_state,
                    thread_options,
                );
                // 接收端可能已经关闭：事件无人接收不是错误。
                let _ = events.send(event);
                thread_state.finish();
            })
            .map_err(|error| {
                SpawnError::with_code(
                    Stage::SpawnWatcher,
                    error.raw_os_error().unwrap_or(0) as u32,
                    format!("创建退出监控线程失败：{error}"),
                )
            })?;

        Ok(Self {
            instance_id,
            process,
            job,
            pid,
            state,
            thread: Some(thread),
            options,
        })
    }

    /// 调用方提供的实例标识。
    pub fn instance_id(&self) -> &str {
        &self.instance_id
    }

    /// 是否收到过停止请求。
    pub fn stop_requested(&self) -> bool {
        self.state.stop_requested()
    }

    /// 幂等停止：按 [`WatchOptions::method`] 停止整棵树，最多等 `timeout` 收敛。
    ///
    /// - `terminate`：立即 `TerminateJobObject`；
    /// - `graceful`：先向根进程组发 `CTRL_BREAK_EVENT`，等根退出 + Job 空，
    ///   最多 `grace`；仍未收敛则 `TerminateJobObject(1)` 兜底。
    ///
    /// 成功判据是根进程退出 + Job 空，不看退出码。重复调用安全：已完全停止
    /// 时立即返回；停止进行中时只等待，不再重复发信号/强杀。停止不会额外产生
    /// 第二个 [`ExitEvent`]，只在事件里把 `stop_requested` 置为 `true`。
    pub fn request_stop(&self, timeout: Duration) -> Result<ExitStatus, StopError> {
        let started = Instant::now();
        let first = self
            .state
            .begin_stop(self.options.method, self.options.grace);

        let root_exited = self.process_exited()?;
        // 已完全停止（根退出 + 树空）：直接返回，不再触碰进程树。
        if root_exited && self.job_empty() {
            return self.exit_status();
        }

        if first {
            self.perform_first_stop(root_exited, timeout, started)?;
        } else if root_exited && !self.job_empty() {
            // 重复调用且根已退出但树仍在：没有可附加的控制台，直接清树。
            self.state.record_forced();
            self.terminate_job()?;
        }

        self.wait_until_stopped(started, timeout)
    }

    /// 第一次停止：按方式执行信号/强杀；graceful 在宽限内收敛则直接返回。
    fn perform_first_stop(
        &self,
        root_exited: bool,
        timeout: Duration,
        started: Instant,
    ) -> Result<(), StopError> {
        match self.options.method {
            StopMode::Terminate => self.terminate_job(),
            StopMode::Graceful if root_exited => {
                // 根已退出但树非空：控制台随属主消失，信号无意义，直接兜底。
                self.state.record_forced();
                self.terminate_job()
            }
            StopMode::Graceful => {
                let delivered = match send_ctrl_break(self.pid) {
                    Ok(ConsoleSignal::Delivered) => true,
                    Ok(ConsoleSignal::Skipped(reason)) => {
                        self.state.record_signal_error(reason);
                        false
                    }
                    Err(error) => {
                        // 不可降级错误也不阻断停止：记录可读原因后走 Job 兜底。
                        self.state.record_signal_error(error.to_string());
                        false
                    }
                };

                if !delivered {
                    // 信号没送到，宽限没有意义，直接强杀。
                    self.state.record_forced();
                    return self.terminate_job();
                }

                // 宽限内等“根退出 + 树空”；两者都满足才算优雅成功。
                let grace = self.options.grace.min(timeout);
                let deadline = started + grace;
                loop {
                    if self.process_exited()? && self.job_empty() {
                        return Ok(());
                    }
                    let left = deadline.saturating_duration_since(Instant::now());
                    if left.is_zero() {
                        break;
                    }
                    // 根已退出时 Job 可能还在收尾，短暂休眠即可；否则等句柄。
                    if self.process_exited()? {
                        thread::sleep(left.min(STOP_POLL));
                    } else {
                        wait_process(self.process.raw(), to_millis(left.min(STOP_POLL)))
                            .map_err(StopError::Win32)?;
                    }
                }

                // 宽限到期仍有进程存活：无条件 TerminateJobObject 兜底。
                self.state.record_forced();
                self.terminate_job()
            }
        }
    }

    /// 在 timeout 剩余预算内等到根退出 + Job 空。
    fn wait_until_stopped(
        &self,
        started: Instant,
        timeout: Duration,
    ) -> Result<ExitStatus, StopError> {
        let deadline = started + timeout;
        loop {
            let root_exited = self.process_exited()?;
            if root_exited && self.job_empty() {
                return self.exit_status();
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(StopError::Timeout {
                    instance_id: self.instance_id.clone(),
                    waited: timeout,
                });
            }
            let slice = left.min(STOP_POLL);
            if root_exited {
                thread::sleep(slice);
            } else {
                wait_process(self.process.raw(), to_millis(slice)).map_err(StopError::Win32)?;
            }
        }
    }

    fn process_exited(&self) -> Result<bool, StopError> {
        wait_process(self.process.raw(), 0).map_err(StopError::Win32)
    }

    fn job_empty(&self) -> bool {
        // 查询失败按“仍非空”处理：宁可多等一轮，也不误报完成。
        matches!(self.job.active_processes(), Ok(0))
    }

    fn terminate_job(&self) -> Result<(), StopError> {
        self.job.terminate(STOP_EXIT_CODE).map_err(StopError::Win32)
    }

    fn exit_status(&self) -> Result<ExitStatus, StopError> {
        let code = exit_code(self.process.raw()).map_err(StopError::Win32)?;
        Ok(ExitStatus { code })
    }
}

impl Drop for ExitWatcher {
    fn drop(&mut self) {
        // Drop 等价于“立即停止”：关闭句柄前先终止整棵树，避免等待线程被
        // 一个永不退出的进程拖住。
        self.state
            .begin_stop(StopMode::Terminate, Duration::from_millis(0));
        let _ = self.job.terminate(STOP_EXIT_CODE);

        let finished = self.state.wait_done(self.options.stop_timeout);
        if let Some(thread) = self.thread.take()
            && finished
        {
            // 线程已发完事件，join 立即返回。
            let _ = thread.join();
        }
        // 未在 stop_timeout 内完成时放弃 join：线程持有 `Arc` 句柄，
        // 会在根进程退出后自行结束，Drop 不能无限阻塞。
    }
}

/// 等待线程主体：根进程 → 树空确认 → 组装事件。
fn collect(
    instance_id: String,
    process: &OwnedHandle,
    job: &Job,
    state: &WatchState,
    options: WatchOptions,
) -> ExitEvent {
    let mut wait_failed = false;
    loop {
        match wait_process(process.raw(), WAIT_SLICE.as_millis() as u32) {
            Ok(true) => break,
            Ok(false) => continue,
            Err(_) => {
                wait_failed = true;
                break;
            }
        }
    }

    let exit_code = if wait_failed {
        UNKNOWN_EXIT_CODE
    } else {
        exit_code(process.raw()).unwrap_or(UNKNOWN_EXIT_CODE)
    };
    let (tree_empty, tree_wait_ms, forced_tree_kill) = wait_tree(job, &options);

    ExitEvent {
        instance_id,
        finished_at: SystemTime::now(),
        exit_code,
        tree_empty,
        tree_wait_ms,
        forced_tree_kill,
        stop_requested: state.stop_requested(),
        stop: state.report(),
    }
}

/// 等 Job 树空；超时则强制清树并确认。返回 `(tree_empty, 耗时毫秒, 是否强制)`。
fn wait_tree(job: &Job, options: &WatchOptions) -> (bool, u64, bool) {
    let started = Instant::now();

    loop {
        // 查询失败按“仍非空”处理：宁可多等一轮，也不误报树空。
        if matches!(job.active_processes(), Ok(0)) {
            return (true, elapsed_ms(started), false);
        }
        let elapsed = started.elapsed();
        if elapsed >= options.tree_wait {
            break;
        }
        sleep_for(remaining(
            options.tree_wait - elapsed,
            options.poll_interval,
        ));
    }

    // 根进程已退出但树没空：强制清树。
    let _ = job.terminate(STOP_EXIT_CODE);
    let confirm_started = Instant::now();
    loop {
        if matches!(job.active_processes(), Ok(0)) {
            return (true, elapsed_ms(started), true);
        }
        if confirm_started.elapsed() >= FORCE_CONFIRM {
            break;
        }
        sleep_for(options.poll_interval);
    }
    (false, elapsed_ms(started), true)
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

fn remaining(left: Duration, poll: Duration) -> Duration {
    if poll.is_zero() { left } else { left.min(poll) }
}

fn sleep_for(duration: Duration) {
    if !duration.is_zero() {
        thread::sleep(duration);
    }
}

/// 转成 `WaitForSingleObject` 的毫秒数；避开 `INFINITE`（u32::MAX）。
fn to_millis(timeout: Duration) -> u32 {
    timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32
}
