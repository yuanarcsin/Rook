//! 纯重启决策状态机。
//!
//! 输入是 Engine 转发的监督事件，输出是"现在该做什么"（[`Decision`]）。
//! 本模块不持有进程、定时器、线程或 Tauri 对象；时间由调用方以
//! [`Instant`] 传入，随机数由 [`RandomSource`] 注入，因此所有迁移都可以
//! 在单元测试里精确复现。
//!
//! # 状态图（只包含重启决策相关部分）
//!
//! ```text
//!                    ┌──────────── paused ◄──────────────┐
//!                    │  crash_loop / restart_budget /    │
//!                    │  crash_no_restart / needs_human    │
//!                    │  / user stop                       │
//!                    ▼                                   │
//!  stopped ──Start──► starting ──Started──► running ──异常退出/健康失败──┐
//!     ▲                  │                    │                        │
//!     │                  │ 启动失败            │ 用户 Stop               ▼
//!     │                  ▼                    └──────────────────► stopped
//!     │               backoff ◄──────────────────────────────────────┘
//!     │                  │  Tick 且 now >= next_retry_at
//!     │                  └────────────► starting
//!     └── Stop（任意状态）
//! ```
//!
//! - `Intent` 表达"用户想要它运行吗"：用户 Stop 后恒为 `Stopped`，任何
//!   自动路径都不得启动；熔断/限速/`never` 把意图改为 `Paused`；
//! - `needs_human` 是正交注意标志：置位时抑制一切自动重启，但不停止存活进程；
//! - 进入 backoff 时立即算好 `next_retry_at`，快照/决策都暴露剩余延迟；
//! - **启动窗口限速**（C1）：记录最近 `window` 内的启动时刻，窗口内启动次数达到
//!   `max_starts_in_window` 后不再产生自动 `StartNow`，直接 paused(restart_budget)；
//!   窗口历史只在内存里（`VecDeque<Instant>`），**不持久化**——应用重启后清空，
//!   但 `consecutive_failures` 仍按 state.json 恢复。用户显式 Start/Stop 清空历史。
//! - 所有事件都携带当前实例语义，重复/迟到的 `Exited`/`HealthFailed` 在
//!   非 running/starting 状态下被丢弃，不会重复计数。
//!
//! 实例 id 过滤、配置重载、应用退出/休眠等由 Engine 负责，不在本模块内。

use std::collections::VecDeque;
use std::fmt;
use std::time::{Duration, Instant};

use super::config::{RandomSource, RestartPolicy, SystemRandom, backoff_delay};
use crate::config::RestartMode;

/// 监督器实际观察到的状态（与 docs/03 §2 对齐）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartStatus {
    /// 没有进程，也不等待重启。
    Stopped,
    /// 已发出启动，等待进程创建/首次就绪。
    Starting,
    /// 进程存活并处于监督中。
    Running,
    /// 等待下一次重启，`next_retry_at` 有效。
    Backoff,
    /// 自动重启被熔断/人工处理阻塞。
    Paused,
}

impl RestartStatus {
    /// 稳定的英文标识（日志/快照/前端共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Backoff => "backoff",
            Self::Paused => "paused",
        }
    }
}

impl fmt::Display for RestartStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 用户意图：自动重启只看它，不看"上次是不是死了"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Intent {
    /// 期望运行（`desired_override = null` 或用户刚 Start）。
    Running,
    /// 用户显式停止。
    Stopped,
    /// 用户暂停/熔断。
    Paused,
}

impl Intent {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Paused => "paused",
        }
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 决策原因（稳定英文标识，写入日志/快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    /// 用户 Start/Restart/Resume。
    StartRequested,
    /// 进程创建成功。
    Spawned,
    /// 正常退出（退出码 0）且 `on-failure`。
    CleanExit,
    /// 异常退出。
    ProcessExit,
    /// 健康失败达到阈值。
    HealthFailure,
    /// 启动失败（spawn 失败/启动宽限到期）。
    StartupFailure,
    /// 用户停止。
    UserStop,
    /// 每周运行时段结束触发的停止（不是用户意图）。
    ScheduleStop,
    /// 连续失败达到上限。
    CrashLoop,
    /// 窗口内启动次数达到上限（`restart_budget`）。
    RestartBudget,
    /// `mode = never` 时进程退出。
    CrashNoRestart,
    /// 需要人工处理（心跳 status / 退出码 / 已持久化）。
    NeedsHuman,
    /// 人工处理完成/标志被清除。
    NeedsHumanCleared,
    /// 稳定运行达到 `stable_after_ms`。
    Stable,
    /// 退避到期，开始重试。
    BackoffElapsed,
    /// 无变化的 tick。
    Tick,
}

impl Reason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::StartRequested => "start_requested",
            Self::Spawned => "spawned",
            Self::CleanExit => "clean_exit",
            Self::ProcessExit => "process_exit",
            Self::HealthFailure => "health_failure",
            Self::StartupFailure => "startup_failure",
            Self::UserStop => "user_stop",
            Self::ScheduleStop => "schedule_stop",
            Self::CrashLoop => "crash_loop",
            Self::RestartBudget => "restart_budget",
            Self::CrashNoRestart => "crash_no_restart",
            Self::NeedsHuman => "needs_human",
            Self::NeedsHumanCleared => "needs_human_cleared",
            Self::Stable => "stable",
            Self::BackoffElapsed => "backoff_elapsed",
            Self::Tick => "tick",
        }
    }
}

impl fmt::Display for Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 当前 `needs_human` 标志。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NeedsHuman {
    /// 来源："heartbeat" / "exit_code" / "restored"。
    pub source: String,
    /// 面向用户/日志的说明。
    pub message: String,
    /// 置位时刻。
    pub since: Instant,
}

/// 监督事件。
///
/// 事件本身不带时间；[`RestartMachine::handle`] 的 `now` 才是时钟。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestartEvent {
    /// 用户 Start/Restart/Resume：清除 paused/needs_human，清零失败计数。
    StartRequested,
    /// 进程创建成功（进入 running，开始 stable 计时）。
    Started,
    /// 启动失败（spawn 失败、启动宽限到期仍未就绪）。
    StartFailed,
    /// 进程自行退出；`clean` 由调用方按退出码判定（通常 `code == 0`）。
    Exited { code: u32, clean: bool },
    /// 运行期健康连续失败达到阈值（Engine 已/将停止实例）。
    HealthFailed,
    /// 用户停止（UI/托盘/配置禁用）。
    StopRequested,
    /// 每周运行时段结束：停止并保持 stopped，但原因记作 schedule_stop，
    /// 不写用户 override（由 Engine 传入 StopCause::Schedule）。
    ScheduleStopRequested,
    /// 稳定运行达到 `stable_after_ms`。
    StableReached,
    /// 观察到需要人工处理（心跳 status、退出码、持久化恢复）。
    NeedsHumanObserved { source: String, message: String },
    /// 心跳 status 恢复，自动清除 needs_human。
    NeedsHumanCleared,
    /// 用户显式确认「我已处理」：清除标志；进程已退出时立即启动。
    ///
    /// 预留事件：当前没有命令/UI 触发（恢复路径是心跳 status 恢复 →
    /// [`Self::NeedsHumanCleared`]，或用户点「重启」）。
    AcknowledgeNeedsHuman,
    /// 时间推进：检查退避是否到期、稳定计时是否达到。
    Tick,
}

/// 状态机的输出决策。
///
/// 每个决策都带 [`Reason`]；状态/计数器/剩余延迟通过 getter 或
/// [`RestartMachine::snapshot`] 读取。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// 保持现状，不启动、不排定定时器。
    Stay { reason: Reason },
    /// 已停止：没有进程，也不等待重启。
    Stopped { reason: Reason },
    /// 暂停自动重启；`reason` 为 `crash_loop` / `crash_no_restart` /
    /// `restart_budget` / `needs_human` 之一。
    Paused { reason: Reason },
    /// 立即启动。
    StartNow { reason: Reason },
    /// 等待到 `at` 再启动；`delay` 为本次等待时长。
    Backoff {
        at: Instant,
        delay: Duration,
        reason: Reason,
    },
    /// 稳定运行达到阈值，`consecutive_failures` 已清零。
    Stable { reason: Reason },
}

impl Decision {
    /// 决策原因。
    pub fn reason(&self) -> Reason {
        match self {
            Self::Stay { reason }
            | Self::Stopped { reason }
            | Self::Paused { reason }
            | Self::StartNow { reason }
            | Self::Backoff { reason, .. }
            | Self::Stable { reason } => *reason,
        }
    }

    /// 是否要求（立即或稍后）启动新实例。
    pub fn is_restart(&self) -> bool {
        matches!(self, Self::StartNow { .. } | Self::Backoff { .. })
    }
}

/// 一次只读快照。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RestartSnapshot {
    pub status: RestartStatus,
    pub intent: Intent,
    pub consecutive_failures: u32,
    pub next_retry_at: Option<Instant>,
    /// 相对快照时刻的剩余等待；没有定时器时为 `None`。
    pub remaining: Option<Duration>,
    pub paused_reason: Option<Reason>,
    pub needs_human: Option<NeedsHuman>,
    pub running_since: Option<Instant>,
    pub last_reason: Option<Reason>,
}

/// 从 state.json 恢复的重启监督种子（Engine 负责把持久时间转成单调时刻）。
///
/// `paused` 与 `paused_reason` 分开：配置层的 user_pause / elevation_unavailable
/// 等原因没有对应的状态机 `Reason`，但仍必须保持 paused 意图。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RestartSeed {
    /// 恢复的连续失败计数（熔断预算）。
    pub consecutive_failures: u32,
    /// 是否处于 paused（含 crash_loop / crash_no_restart / 用户暂停）。
    pub paused: bool,
    /// 能映射到状态机原因时的 paused 原因。
    pub paused_reason: Option<Reason>,
    /// 恢复的 needs_human 标志。
    pub needs_human: Option<NeedsHuman>,
    /// 已换算到单调时钟的退避到期时刻。
    pub next_retry_at: Option<Instant>,
}

/// 单任务重启决策状态机。
///
/// 每个任务一个实例；配置变更时重建（失败计数是否保留由 Engine 决定）。
#[derive(Debug, Clone)]
pub struct RestartMachine<R: RandomSource = SystemRandom> {
    policy: RestartPolicy,
    random: R,
    status: RestartStatus,
    intent: Intent,
    consecutive_failures: u32,
    next_retry_at: Option<Instant>,
    running_since: Option<Instant>,
    paused_reason: Option<Reason>,
    needs_human: Option<NeedsHuman>,
    last_reason: Option<Reason>,
    /// 启动窗口内的启动时刻（升序，仅内存，不持久化）。
    start_history: VecDeque<Instant>,
}

impl RestartMachine<SystemRandom> {
    /// 使用系统随机源创建；初始为 `stopped`、意图 `stopped`。
    pub fn new(policy: RestartPolicy) -> Self {
        Self::with_random(policy, SystemRandom::new())
    }
}

impl<R: RandomSource> RestartMachine<R> {
    /// 从持久化种子恢复（应用重启/配置重载时由 Engine 调用）。
    ///
    /// 优先级：needs_human > paused > backoff > stopped；`next_retry_at` 已由
    /// 调用方换算成单调时刻。
    pub fn restore(policy: RestartPolicy, random: R, seed: RestartSeed) -> Self {
        let mut machine = Self::with_random(policy, random);
        machine.consecutive_failures = seed.consecutive_failures;
        machine.needs_human = seed.needs_human;
        if machine.needs_human.is_some() {
            // needs_human 是正交标志：保持 running 意图，但抑制自动重启。
            machine.status = RestartStatus::Paused;
            machine.paused_reason = Some(Reason::NeedsHuman);
            machine.intent = Intent::Running;
        } else if seed.paused {
            machine.status = RestartStatus::Paused;
            machine.paused_reason = seed.paused_reason;
            machine.intent = Intent::Paused;
            machine.next_retry_at = None;
        } else if let Some(at) = seed.next_retry_at {
            machine.status = RestartStatus::Backoff;
            machine.next_retry_at = Some(at);
            machine.intent = Intent::Running;
        }
        machine
    }

    /// 当前状态机快照的持久化种子（配置重载重建时保留暂停/退避记忆）。
    pub fn seed(&self) -> RestartSeed {
        RestartSeed {
            consecutive_failures: self.consecutive_failures,
            paused: self.status == RestartStatus::Paused,
            paused_reason: self.paused_reason,
            needs_human: self.needs_human.clone(),
            next_retry_at: self.next_retry_at,
        }
    }

    /// 注入随机源（测试用固定/序列值）。
    pub fn with_random(policy: RestartPolicy, random: R) -> Self {
        Self {
            policy,
            random,
            status: RestartStatus::Stopped,
            intent: Intent::Stopped,
            consecutive_failures: 0,
            next_retry_at: None,
            running_since: None,
            paused_reason: None,
            needs_human: None,
            last_reason: None,
            start_history: VecDeque::new(),
        }
    }

    pub fn policy(&self) -> &RestartPolicy {
        &self.policy
    }

    pub fn status(&self) -> RestartStatus {
        self.status
    }

    pub fn intent(&self) -> Intent {
        self.intent
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn next_retry_at(&self) -> Option<Instant> {
        self.next_retry_at
    }

    /// 距离下一次重试的剩余时间；定时器已到期返回 `0`。
    pub fn remaining(&self, now: Instant) -> Option<Duration> {
        self.next_retry_at
            .map(|at| at.saturating_duration_since(now))
    }

    pub fn paused_reason(&self) -> Option<Reason> {
        self.paused_reason
    }

    pub fn needs_human(&self) -> Option<&NeedsHuman> {
        self.needs_human.as_ref()
    }

    pub fn running_since(&self) -> Option<Instant> {
        self.running_since
    }

    pub fn last_reason(&self) -> Option<Reason> {
        self.last_reason
    }

    /// 当前记录的启动时刻数（未按 `now` 清理；完整窗口口径用 [`Self::starts_in_window`]）。
    pub fn recent_start_count(&self) -> usize {
        self.start_history.len()
    }

    /// 最近 `window` 内的启动次数（按 `now` 清理过期项后计数；窗口不限时为 0）。
    pub fn starts_in_window(&mut self, now: Instant) -> usize {
        self.prune_start_history(now);
        self.start_history.len()
    }

    /// 休眠/时钟跳变：把单调计时器整体平移，保持剩余时间不变。
    ///
    /// 睡眠时长既不能让退避提前到期，也不能让 stable_after 窗口提前满足；
    /// 平移后 `remaining(now + delta)` 与平移前一致。失败计数不在这里改。
    /// 启动窗口历史一并平移：睡眠不算“窗口已滑过”，避免唤醒后瞬间放开限速。
    pub fn shift_timers(&mut self, delta: Duration) {
        if delta.is_zero() {
            return;
        }
        self.next_retry_at = self
            .next_retry_at
            .map(|at| at.checked_add(delta).unwrap_or(at));
        self.running_since = self
            .running_since
            .map(|at| at.checked_add(delta).unwrap_or(at));
        for at in &mut self.start_history {
            *at = at.checked_add(delta).unwrap_or(*at);
        }
    }

    /// 当前只读快照。
    pub fn snapshot(&self, now: Instant) -> RestartSnapshot {
        RestartSnapshot {
            status: self.status,
            intent: self.intent,
            consecutive_failures: self.consecutive_failures,
            next_retry_at: self.next_retry_at,
            remaining: self.remaining(now),
            paused_reason: self.paused_reason,
            needs_human: self.needs_human.clone(),
            running_since: self.running_since,
            last_reason: self.last_reason,
        }
    }

    /// 处理一个事件并返回决策。
    ///
    /// 纯内存迁移，不 sleep、不做 I/O；`now` 必须是单调时钟。
    pub fn handle(&mut self, event: RestartEvent, now: Instant) -> Decision {
        let decision = match event {
            RestartEvent::StartRequested => self.start(),
            RestartEvent::Started => self.started(now),
            RestartEvent::StartFailed => self.start_failed(now),
            RestartEvent::Exited { code, clean } => self.exited(code, clean, now),
            RestartEvent::HealthFailed => self.health_failed(now),
            RestartEvent::StopRequested => self.stop_with(Reason::UserStop),
            RestartEvent::ScheduleStopRequested => self.stop_with(Reason::ScheduleStop),
            RestartEvent::StableReached => self.stable_reached(now),
            RestartEvent::NeedsHumanObserved { source, message } => {
                self.needs_human_observed(source, message, now)
            }
            RestartEvent::NeedsHumanCleared => self.needs_human_cleared(),
            RestartEvent::AcknowledgeNeedsHuman => self.acknowledge(now),
            RestartEvent::Tick => self.tick(now),
        };
        self.last_reason = Some(decision.reason());
        decision
    }

    fn start(&mut self) -> Decision {
        // 已在 starting/running 时重复 Start 幂等，避免双实例；
        // 但用户显式操作仍要清除 needs_human 并清零失败计数。
        if self.intent == Intent::Running
            && matches!(
                self.status,
                RestartStatus::Starting | RestartStatus::Running
            )
        {
            self.needs_human = None;
            self.consecutive_failures = 0;
            return Decision::Stay {
                reason: Reason::StartRequested,
            };
        }

        self.intent = Intent::Running;
        self.status = RestartStatus::Starting;
        self.consecutive_failures = 0;
        self.next_retry_at = None;
        self.running_since = None;
        self.paused_reason = None;
        self.needs_human = None;
        // 显式启动（Start/Restart/Resume）清空启动窗口：用户意图优先。
        // 注意 Engine 的 auto_start / 时段启动也走 StartRequested，同样清零。
        self.start_history.clear();
        Decision::StartNow {
            reason: Reason::StartRequested,
        }
    }

    fn started(&mut self, now: Instant) -> Decision {
        if self.status != RestartStatus::Starting || self.intent != Intent::Running {
            return Decision::Stay {
                reason: Reason::Spawned,
            };
        }
        self.status = RestartStatus::Running;
        self.running_since = Some(now);
        self.next_retry_at = None;
        self.paused_reason = None;
        // 只统计真正创建成功的实例（spawn 失败由 StartFailed 处理，不算启动）。
        self.record_start(now);
        Decision::Stay {
            reason: Reason::Spawned,
        }
    }

    fn start_failed(&mut self, now: Instant) -> Decision {
        // Q3：启动宽限到期时进程可能还处于 Running（Engine 先 stop 再上报
        // StartFailed），因此 Starting/Running 都要按启动失败计数。
        if !matches!(
            self.status,
            RestartStatus::Starting | RestartStatus::Running
        ) || self.intent != Intent::Running
        {
            return Decision::Stay {
                reason: Reason::StartupFailure,
            };
        }
        if self.needs_human.is_some() {
            return self.pause(now, Reason::NeedsHuman);
        }
        self.fail_and_backoff(now, Reason::StartupFailure)
    }

    fn exited(&mut self, code: u32, clean: bool, now: Instant) -> Decision {
        // 用户已停止/暂停，或实例已经结束：迟到/重复事件一律丢弃。
        if self.intent != Intent::Running
            || !matches!(
                self.status,
                RestartStatus::Starting | RestartStatus::Running
            )
        {
            return Decision::Stay {
                reason: Reason::ProcessExit,
            };
        }

        // needs_human 期间退出：不重启、不计失败。
        if self.needs_human.is_some() {
            return self.pause(now, Reason::NeedsHuman);
        }

        // 命中 pause_on_exit_codes：paused(needs_human)，不计失败。
        if self.policy.pause_on_exit_codes.contains(&code) {
            self.needs_human = Some(NeedsHuman {
                source: "exit_code".to_owned(),
                message: format!("进程退出码 {code} 需要人工处理"),
                since: now,
            });
            return self.pause(now, Reason::NeedsHuman);
        }

        match self.policy.mode {
            RestartMode::Never => self.pause(now, Reason::CrashNoRestart),
            // on-failure + clean：本轮正常完成，意图转为 stopped。
            RestartMode::OnFailure if clean => {
                self.intent = Intent::Stopped;
                self.status = RestartStatus::Stopped;
                self.next_retry_at = None;
                self.running_since = None;
                self.paused_reason = None;
                Decision::Stopped {
                    reason: Reason::CleanExit,
                }
            }
            // always + clean：重启但不计失败、不熔断。
            RestartMode::Always if clean => self.schedule(now, Reason::CleanExit),
            _ => self.fail_and_backoff(now, Reason::ProcessExit),
        }
    }

    fn health_failed(&mut self, now: Instant) -> Decision {
        if self.intent != Intent::Running
            || !matches!(
                self.status,
                RestartStatus::Starting | RestartStatus::Running
            )
        {
            return Decision::Stay {
                reason: Reason::HealthFailure,
            };
        }
        if self.needs_human.is_some() {
            return self.pause(now, Reason::NeedsHuman);
        }
        self.fail_and_backoff(now, Reason::HealthFailure)
    }

    fn stop_with(&mut self, reason: Reason) -> Decision {
        // 显式/时段停止都清空启动窗口：下次启动重新计数。
        self.start_history.clear();
        if self.status == RestartStatus::Stopped
            && self.intent == Intent::Stopped
            && self.needs_human.is_none()
        {
            return Decision::Stay { reason };
        }

        self.intent = Intent::Stopped;
        self.status = RestartStatus::Stopped;
        self.next_retry_at = None;
        self.running_since = None;
        self.paused_reason = None;
        // 显式停止清除 needs_human（docs/03 §7.1）。
        self.needs_human = None;
        Decision::Stopped { reason }
    }

    fn stable_reached(&mut self, now: Instant) -> Decision {
        if self.status != RestartStatus::Running
            || self.intent != Intent::Running
            || !self.stable_elapsed(now)
            || self.consecutive_failures == 0
        {
            return Decision::Stay {
                reason: Reason::Stable,
            };
        }
        self.consecutive_failures = 0;
        Decision::Stable {
            reason: Reason::Stable,
        }
    }

    fn needs_human_observed(&mut self, source: String, message: String, now: Instant) -> Decision {
        self.needs_human = Some(NeedsHuman {
            source,
            message,
            since: now,
        });

        // 用户已停止/暂停：保留用户意图，不因外部标志改变状态。
        if self.intent != Intent::Running {
            return Decision::Stay {
                reason: Reason::NeedsHuman,
            };
        }

        match self.status {
            // 有进程：保持 running，仅置标志、不排定时器。
            RestartStatus::Starting | RestartStatus::Running => Decision::Stay {
                reason: Reason::NeedsHuman,
            },
            // 等待重启中：取消定时器，转入 paused(needs_human)。
            RestartStatus::Backoff | RestartStatus::Stopped => self.pause(now, Reason::NeedsHuman),
            // 已因 crash_loop 暂停：保留暂停原因，只置标志。
            RestartStatus::Paused => Decision::Stay {
                reason: Reason::NeedsHuman,
            },
        }
    }

    fn needs_human_cleared(&mut self) -> Decision {
        self.needs_human = None;
        if self.status == RestartStatus::Paused && self.paused_reason == Some(Reason::NeedsHuman) {
            // 进程已退出：保持 stopped，等用户显式启动。
            self.status = RestartStatus::Stopped;
            self.intent = Intent::Stopped;
            self.paused_reason = None;
            self.next_retry_at = None;
            self.running_since = None;
            return Decision::Stopped {
                reason: Reason::NeedsHumanCleared,
            };
        }
        Decision::Stay {
            reason: Reason::NeedsHumanCleared,
        }
    }

    fn acknowledge(&mut self, _now: Instant) -> Decision {
        self.needs_human = None;
        // 用户显式「我已处理」属于手动操作：清空窗口历史（与 Start 一致）。
        self.start_history.clear();

        // 进程还活着：只清标志，不重启。
        if matches!(
            self.status,
            RestartStatus::Starting | RestartStatus::Running
        ) {
            return Decision::Stay {
                reason: Reason::NeedsHumanCleared,
            };
        }

        // 进程已退出且暂停原因是 needs_human：清除并启动。
        if self.status == RestartStatus::Paused && self.paused_reason == Some(Reason::NeedsHuman) {
            self.intent = Intent::Running;
            self.status = RestartStatus::Starting;
            self.consecutive_failures = 0;
            self.next_retry_at = None;
            self.running_since = None;
            self.paused_reason = None;
            return Decision::StartNow {
                reason: Reason::NeedsHumanCleared,
            };
        }

        Decision::Stay {
            reason: Reason::NeedsHumanCleared,
        }
    }

    fn tick(&mut self, now: Instant) -> Decision {
        match self.status {
            RestartStatus::Backoff => {
                let Some(at) = self.next_retry_at else {
                    return Decision::Stay {
                        reason: Reason::Tick,
                    };
                };
                if now < at {
                    return Decision::Stay {
                        reason: Reason::Tick,
                    };
                }
                // 定时器本应在进入 paused 时取消；这里再兜底一次。
                if self.intent != Intent::Running || self.needs_human.is_some() {
                    return Decision::Stay {
                        reason: Reason::BackoffElapsed,
                    };
                }
                if self.policy.max_consecutive_failures > 0
                    && self.consecutive_failures >= self.policy.max_consecutive_failures
                {
                    return self.pause(now, Reason::CrashLoop);
                }
                // 启动窗口限速：超限就不再排定时器/不启动，保留失败计数。
                if self.budget_exhausted(now) {
                    return self.pause(now, Reason::RestartBudget);
                }
                self.status = RestartStatus::Starting;
                self.next_retry_at = None;
                self.running_since = None;
                Decision::StartNow {
                    reason: Reason::BackoffElapsed,
                }
            }
            RestartStatus::Running
                if self.intent == Intent::Running
                    && self.stable_elapsed(now)
                    && self.consecutive_failures > 0 =>
            {
                self.consecutive_failures = 0;
                Decision::Stable {
                    reason: Reason::Stable,
                }
            }
            _ => Decision::Stay {
                reason: Reason::Tick,
            },
        }
    }

    fn stable_elapsed(&self, now: Instant) -> bool {
        self.running_since
            .is_some_and(|since| now.saturating_duration_since(since) >= self.policy.stable_after)
    }

    /// 记录一次启动并清理窗口外的历史。
    ///
    /// 窗口或上限为 0 时不记录（不限速），避免无意义增长。
    fn record_start(&mut self, now: Instant) {
        if self.policy.window.is_zero() || self.policy.max_starts_in_window == 0 {
            return;
        }
        self.start_history.push_back(now);
        self.prune_start_history(now);
    }

    /// 丢弃 `now - window` 及更早的启动记录；窗口为半开区间 `(now - window, now]`。
    fn prune_start_history(&mut self, now: Instant) {
        if self.policy.window.is_zero() {
            self.start_history.clear();
            return;
        }
        let Some(cutoff) = now.checked_sub(self.policy.window) else {
            // 单调时钟起点之后还没走满一个窗口：没有可清理的项。
            return;
        };
        while let Some(at) = self.start_history.front() {
            if *at <= cutoff {
                self.start_history.pop_front();
            } else {
                break;
            }
        }
    }

    /// 窗口内启动次数是否已达上限（窗口/上限为 0 = 不限）。
    fn budget_exhausted(&mut self, now: Instant) -> bool {
        if self.policy.window.is_zero() || self.policy.max_starts_in_window == 0 {
            return false;
        }
        self.prune_start_history(now);
        self.start_history.len() >= self.policy.max_starts_in_window as usize
    }

    /// 失败计数 +1；达到上限则 paused(crash_loop)，否则进入 backoff。
    fn fail_and_backoff(&mut self, now: Instant, reason: Reason) -> Decision {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        if self.policy.max_consecutive_failures > 0
            && self.consecutive_failures >= self.policy.max_consecutive_failures
        {
            return self.pause(now, Reason::CrashLoop);
        }
        self.schedule(now, reason)
    }

    fn schedule(&mut self, now: Instant, reason: Reason) -> Decision {
        let unit = self.random.next_unit();
        let delay = backoff_delay(&self.policy, self.consecutive_failures.max(1), unit);
        let at = now + delay;

        self.status = RestartStatus::Backoff;
        self.next_retry_at = Some(at);
        self.running_since = None;
        self.paused_reason = None;
        Decision::Backoff { at, delay, reason }
    }

    fn pause(&mut self, _now: Instant, reason: Reason) -> Decision {
        self.status = RestartStatus::Paused;
        self.paused_reason = Some(reason);
        self.next_retry_at = None;
        self.running_since = None;
        // needs_human 是正交标志，不改变用户意图；熔断/never 则把意图改为 paused。
        if reason != Reason::NeedsHuman {
            self.intent = Intent::Paused;
        }
        Decision::Paused { reason }
    }
}
