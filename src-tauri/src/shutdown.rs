//! 应用退出协调（B1/B3）：统一托盘退出、窗口内退出与会话结束的停止流程。
//!
//! 本模块分两层：
//!
//! - **纯逻辑**（始终编译、可单测）：退出原因、预算分配、幂等协调器与结果类型；
//! - **Tauri 编排**（`cfg(not(test))`）：从 Engine 取停止计划、在锁外逐个停止、
//!   保存 state/刷新日志，最后交给调用方 `app.exit(0)`。
//!
//! 退出流程必须满足：
//!
//! 1. **幂等**：同一进程内只允许一个调用者执行停止；重复调用只等待/直接返回，
//!    不会重复停止任务或重复写 `clean_exit`；
//! 2. **可超时**：每个任务用 `min([tasks.stop].timeout_ms, 剩余预算)`，预算耗尽
//!    仍继续收尾（以 0 预算请求停止，由 Job 兜底），最终一定返回；
//! 3. **失败不阻止退出**：单个任务停止失败只计数，不影响其他任务与应用退出；
//! 4. **会话结束不弹 UI**：`ExitReason::SessionEnd` 从不显示确认框。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// 应用退出原因（写入 `app.exit.reason` 与 state.json 的 `last_exit_reason`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// 用户从托盘或窗口内主动退出。
    User,
    /// 用户注销 / 系统关机（`WM_ENDSESSION`）。
    SessionEnd,
}

impl ExitReason {
    /// 稳定英文标识（事件/state.json 共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::SessionEnd => "session_end",
        }
    }
}

impl std::fmt::Display for ExitReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 会话结束（注销/关机）的预算上限：系统只给很短的处理窗口。
pub const SESSION_END_BUDGET_CAP: Duration = Duration::from_secs(5);

/// 单个任务的停止预算：`min(剩余退出预算, 任务自己的 stop.timeout_ms)`。
///
/// 预算耗尽时返回 `Duration::ZERO`：仍然发起停止请求（由运行时/Job 兜底），
/// 只是不再等待，保证退出流程最终返回。
pub fn task_budget(remaining: Duration, task_timeout: Duration) -> Duration {
    remaining.min(task_timeout)
}

/// 会话结束预算：`min(shutdown_timeout_ms, 5000ms)`。
pub fn session_budget(configured: Duration) -> Duration {
    configured.min(SESSION_END_BUDGET_CAP)
}

/// 一次退出流程的结果（`app.exit` 事件字段）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShutdownOutcome {
    /// 退出原因。
    pub reason: ExitReason,
    /// 成功停止的任务数。
    pub stopped: usize,
    /// 停止失败/超时的任务数（不阻止退出）。
    pub failed: usize,
    /// 其中走强制终止兜底的任务数。
    pub forced: usize,
    /// 整个退出流程耗时（毫秒）。
    pub duration_ms: u64,
}

impl ShutdownOutcome {
    /// 没有可停止任务或作为幂等等待方的占位结果。
    pub fn empty(reason: ExitReason) -> Self {
        Self {
            reason,
            stopped: 0,
            failed: 0,
            forced: 0,
            duration_ms: 0,
        }
    }
}

/// 退出流程的幂等门。
///
/// 第一个调用者 `begin()` 返回 `true` 并负责执行停止；之后的调用者返回
/// `false`，可以 `wait()` 等待首个调用者完成（有界等待），也可以直接返回。
#[derive(Debug, Default)]
pub struct ShutdownCoordinator {
    started: AtomicBool,
    outcome: Mutex<Option<ShutdownOutcome>>,
    finished: Condvar,
}

impl ShutdownCoordinator {
    /// 新建协调器（尚未开始退出）。
    pub fn new() -> Self {
        Self::default()
    }

    /// 尝试成为退出流程的执行者。
    ///
    /// 只有第一次调用返回 `true`；后续调用（含并发）都返回 `false`。
    pub fn begin(&self) -> bool {
        !self.started.swap(true, Ordering::SeqCst)
    }

    /// 是否已经开始退出（用于调度线程/命令快速拒绝新工作）。
    pub fn is_started(&self) -> bool {
        self.started.load(Ordering::SeqCst)
    }

    /// 记录结果并唤醒所有等待者；重复调用只保留第一次结果。
    pub fn complete(&self, outcome: ShutdownOutcome) {
        if let Ok(mut slot) = self.outcome.lock()
            && slot.is_none()
        {
            *slot = Some(outcome);
        }
        self.finished.notify_all();
    }

    /// 等待首个调用者完成，最多等 `timeout`。
    ///
    /// 返回已完成的结果；超时或尚未完成时返回 `None`（调用方仍应继续退出，
    /// 不能因为等待而卡死）。
    pub fn wait(&self, timeout: Duration) -> Option<ShutdownOutcome> {
        let deadline = std::time::Instant::now() + timeout;
        let mut slot = self.outcome.lock().ok()?;
        loop {
            if let Some(outcome) = slot.as_ref() {
                return Some(outcome.clone());
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return None;
            }
            let (next, timed_out) = self.finished.wait_timeout(slot, deadline - now).ok()?;
            slot = next;
            if timed_out.timed_out() && slot.is_none() {
                return None;
            }
        }
    }
}

#[cfg(not(test))]
mod orchestration {
    use std::time::Instant;

    use tauri::AppHandle;

    use super::{ExitReason, ShutdownOutcome, session_budget, task_budget};
    use crate::engine_host;

    /// 托盘/窗口/会话结束的统一入口。
    ///
    /// - `interactive=true`：用户主动退出。若仍有任务运行且 `[app].confirm_exit=true`，
    ///   用 `MessageBoxW` 弹一次 Yes/No（无窗口也能弹）；选 No 直接返回。
    /// - `interactive=false`：窗口内已确认（前端 ConfirmDialog）或会话结束，绝不弹窗。
    ///
    /// 阻塞的停止流程在独立线程执行，调用方（托盘菜单/窗口事件/命令）立即返回。
    pub(crate) fn request_quit(app: &AppHandle, reason: ExitReason, interactive: bool) {
        let engine = engine_host::engine_handle(app);
        if engine_host::lock(&engine).shutdown_started() {
            tracing::debug!(reason = reason.as_str(), "退出流程已开始，忽略重复请求");
            return;
        }
        if interactive {
            let (running, confirm) = engine_host::lock(&engine).exit_confirmation();
            if running > 0 && confirm && !confirm_exit_message(running) {
                tracing::info!(
                    event = "app.exit_cancelled",
                    running,
                    "用户取消退出（托盘确认框选择 No）"
                );
                return;
            }
        }
        let budget = {
            let guard = engine_host::lock(&engine);
            let configured = guard.shutdown_budget();
            if reason == ExitReason::SessionEnd {
                session_budget(configured)
            } else {
                configured
            }
        };
        let app = app.clone();
        std::thread::Builder::new()
            .name("cs-shutdown".to_owned())
            .spawn(move || {
                let outcome = run(&app, reason, budget);
                tracing::info!(
                    event = "app.exit",
                    reason = outcome.reason.as_str(),
                    stopped = outcome.stopped,
                    failed = outcome.failed,
                    forced = outcome.forced,
                    duration_ms = outcome.duration_ms,
                    "应用退出流程完成"
                );
                // app.exit 事件写完后刷新日志；之后再写日志不会被落盘。
                engine_host::lock(&engine_host::engine_handle(&app)).flush_logs();
                app.exit(0);
            })
            .ok();
    }

    /// 执行一次退出流程（只在 `request_quit` 的工作线程里调用）。
    fn run(app: &AppHandle, reason: ExitReason, budget: std::time::Duration) -> ShutdownOutcome {
        let engine = engine_host::engine_handle(app);
        let coordinator = engine_host::lock(&engine).shutdown_coordinator();
        if !coordinator.begin() {
            // 幂等：另一个调用者正在退出。有界等待它的结果；超时也继续退出。
            return coordinator
                .wait(budget + std::time::Duration::from_secs(1))
                .unwrap_or_else(|| ShutdownOutcome::empty(reason));
        }

        let started = Instant::now();
        let plan = engine_host::lock(&engine).begin_shutdown(reason, budget);
        let mut stopped = 0usize;
        let mut failed = 0usize;
        let mut forced = 0usize;
        if let Some(plan) = plan {
            for task in plan.tasks {
                let now = Instant::now();
                let remaining = plan.deadline.saturating_duration_since(now);
                let timeout = task_budget(remaining, task.timeout);
                // 启动中的实例不允许直接 stop（InvalidState）；在预算内短暂等待
                // 它进入 running/stopped，再走正常停止。等不到时仍继续退出，
                // 由 Job KILL_ON_JOB_CLOSE 兜底。
                if task.runtime.status() == crate::runtime::TaskStatus::Starting {
                    let wait_until = Instant::now() + timeout;
                    while task.runtime.status() == crate::runtime::TaskStatus::Starting
                        && Instant::now() < wait_until
                    {
                        std::thread::sleep(std::time::Duration::from_millis(20));
                    }
                }
                let now = Instant::now();
                let remaining = plan.deadline.saturating_duration_since(now);
                let timeout = task_budget(remaining, task.timeout);
                match task.runtime.stop(timeout) {
                    Ok(snapshot) => {
                        stopped += 1;
                        if snapshot.forced_kill {
                            forced += 1;
                        }
                    }
                    Err(error) => {
                        failed += 1;
                        tracing::warn!(
                            task = %task.id,
                            error = %error,
                            timeout_ms = timeout.as_millis() as u64,
                            "退出时停止任务失败/超时（不阻止退出）"
                        );
                    }
                }
            }
        }
        let outcome = ShutdownOutcome {
            reason,
            stopped,
            failed,
            forced,
            duration_ms: started.elapsed().as_millis() as u64,
        };
        engine_host::lock(&engine).finish_shutdown(reason, &outcome);
        // 停止配置监听线程：退出后不再重载/写日志；join 在 Engine 锁外。
        engine_host::stop_config_watch(&engine);
        coordinator.complete(outcome.clone());
        outcome
    }

    /// 托盘退出确认框（Yes/No）。返回是否继续退出。
    fn confirm_exit_message(running: usize) -> bool {
        use windows::Win32::UI::WindowsAndMessaging::{
            IDYES, MB_DEFBUTTON2, MB_ICONWARNING, MB_SETFOREGROUND, MB_YESNO, MessageBoxW,
        };
        use windows::core::HSTRING;

        use crate::brand::DISPLAY_NAME;

        let text = HSTRING::from(format!(
            "还有 {running} 个任务正在运行。退出 {DISPLAY_NAME} 会先请求它们停止，最多等待退出预算。\n\n确定停止全部任务并退出吗？"
        ));
        let caption = HSTRING::from(DISPLAY_NAME);
        let result = unsafe {
            MessageBoxW(
                None,
                &text,
                &caption,
                MB_YESNO | MB_ICONWARNING | MB_DEFBUTTON2 | MB_SETFOREGROUND,
            )
        };
        result == IDYES
    }
}

#[cfg(not(test))]
pub(crate) use orchestration::request_quit;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_budget_is_min_of_remaining_and_task_timeout() {
        assert_eq!(
            task_budget(Duration::from_secs(8), Duration::from_secs(2)),
            Duration::from_secs(2)
        );
        assert_eq!(
            task_budget(Duration::from_secs(1), Duration::from_secs(2)),
            Duration::from_secs(1)
        );
        // 预算耗尽仍然发起停止，只是不再等待。
        assert_eq!(
            task_budget(Duration::ZERO, Duration::from_secs(2)),
            Duration::ZERO
        );
    }

    #[test]
    fn session_budget_caps_at_five_seconds() {
        assert_eq!(
            session_budget(Duration::from_secs(8)),
            Duration::from_secs(5)
        );
        assert_eq!(
            session_budget(Duration::from_secs(3)),
            Duration::from_secs(3)
        );
    }

    #[test]
    fn exit_reason_maps_to_stable_strings() {
        assert_eq!(ExitReason::User.as_str(), "user");
        assert_eq!(ExitReason::SessionEnd.as_str(), "session_end");
    }

    #[test]
    fn coordinator_is_idempotent_and_wakes_waiters() {
        let coordinator = ShutdownCoordinator::new();
        assert!(coordinator.begin());
        assert!(!coordinator.begin());
        assert!(coordinator.is_started());
        assert!(coordinator.wait(Duration::from_millis(10)).is_none());

        let outcome = ShutdownOutcome {
            reason: ExitReason::User,
            stopped: 2,
            failed: 1,
            forced: 1,
            duration_ms: 42,
        };
        coordinator.complete(outcome.clone());
        assert_eq!(
            coordinator.wait(Duration::from_millis(10)),
            Some(outcome.clone())
        );
        // 重复完成不改变结果。
        coordinator.complete(ShutdownOutcome::empty(ExitReason::SessionEnd));
        assert_eq!(coordinator.wait(Duration::ZERO), Some(outcome));
        assert!(!coordinator.begin());
    }
}
