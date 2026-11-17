//! 健康探测结果与评估状态机。
//!
//! 本模块只做"一次探测结果 + 当前时间 → 健康判定"，不持有进程、线程或
//! Win32 句柄。探针在 [`super::process`]、[`super::heartbeat`]、[`super::http`]，
//! Engine 只需按 `interval_ms` 调用 [`HealthMonitor::check`]。
//!
//! # 状态机
//!
//! ```text
//! Idle ──start()──> Starting ──首次成功──> Healthy
//!                      │                    │
//!                      │宽限结束仍从未成功    │连续失败 < threshold
//!                      ▼                    ▼
//!                   Unhealthy            Degraded ──达到 threshold──> Unhealthy
//! ```
//!
//! - 启动宽限内失败只记录，不增加失败计数；宽限结束仍未成功 = 启动失败
//!   （[`HealthVerdict::startup_failure`]），不等同于运行期健康故障；
//! - 运行期连续 `failure_threshold` 次失败才进入 `Unhealthy`，一次成功立即清零；
//! - `needs_human` 是正交标志：命中 `needs_human_status` 时健康状态仍可为
//!   `Healthy`，不增加失败计数；只由心跳 status、用户操作改变；
//! - `suspend` 冻结判定，`resume`/`reset_observation` 进入 `resume_grace`，
//!   宽限内失败只报 `Degraded`，不计失败（休眠/时钟跳变不算故障）。

use std::time::{Duration, Instant};

use super::HeartbeatReport;

/// 一次探测的失败分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    /// PID 已不存在（进程退出由退出监控负责，这里只是显式查询结果）。
    ProcessExited,
    /// 查询 PID 时操作系统返回错误。
    ProcessQuery,
    /// 心跳文件不存在（且此前已经见过心跳）。
    HeartbeatMissing,
    /// 心跳文件读取失败（权限、被占用等）。
    HeartbeatRead,
    /// 心跳内容既不是合法 JSON，也不是合法单行 ISO-8601。
    HeartbeatParse,
    /// 心跳 `instance` 与当前实例不匹配（旧实例残留写入）。
    HeartbeatInstanceMismatch,
    /// 心跳内容超过 `max_age` 没有变化。
    HeartbeatStale,
    /// 启动宽限结束仍从未见过心跳。
    HeartbeatNotSeen,
    /// HTTP URL 不是 `http://` 或无法解析。
    HttpInvalidUrl,
    /// TCP 连接失败（拒绝连接、DNS 失败等）。
    HttpConnect,
    /// 连接/读取/写入超时。
    HttpTimeout,
    /// 连接建立后的 IO 错误。
    HttpIo,
    /// 响应不符合 HTTP 基本格式。
    HttpProtocol,
    /// 状态码不在 `expect_status` 中（3xx 不跟随，按状态码判定）。
    HttpStatus,
    /// 响应体前 64 KiB 不包含 `expect_body_contains`。
    HttpBodyMismatch,
    /// 启动宽限结束仍未成功（由评估器合成，携带最后一次探测失败）。
    StartupTimeout,
}

impl FailureKind {
    /// 稳定的英文标识（日志/快照/前端共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ProcessExited => "process_exited",
            Self::ProcessQuery => "process_query",
            Self::HeartbeatMissing => "heartbeat_missing",
            Self::HeartbeatRead => "heartbeat_read",
            Self::HeartbeatParse => "heartbeat_parse",
            Self::HeartbeatInstanceMismatch => "heartbeat_instance_mismatch",
            Self::HeartbeatStale => "heartbeat_stale",
            Self::HeartbeatNotSeen => "heartbeat_not_seen",
            Self::HttpInvalidUrl => "http_invalid_url",
            Self::HttpConnect => "http_connect",
            Self::HttpTimeout => "http_timeout",
            Self::HttpIo => "http_io",
            Self::HttpProtocol => "http_protocol",
            Self::HttpStatus => "http_status",
            Self::HttpBodyMismatch => "http_body_mismatch",
            Self::StartupTimeout => "startup_timeout",
        }
    }
}

/// 一次失败的结构化说明。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthFailure {
    pub kind: FailureKind,
    pub message: String,
}

impl HealthFailure {
    pub fn new(kind: FailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    fn not_seen() -> Self {
        Self::new(FailureKind::HeartbeatNotSeen, "启动宽限内尚未观察到心跳")
    }

    fn startup_timeout(last: &HealthFailure) -> Self {
        Self::new(
            FailureKind::StartupTimeout,
            format!("启动宽限结束仍未健康：{}", last.message),
        )
    }
}

impl std::fmt::Display for HealthFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}（{}）", self.message, self.kind.as_str())
    }
}

/// 心跳文件里无法参与存活判定的异常，只做告警。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HealthWarning {
    /// 心跳 `ts` 与核心墙钟偏差超过 10 分钟（NTP/手动改时间）。
    ClockSkew { seconds: i64 },
    /// `seq` 没有递增，但内容发生了变化（并发写/时钟冻结）。
    SequenceNotIncreasing { previous: i64, current: i64 },
    /// JSON 心跳缺少 `ts`。
    TimestampMissing,
    /// JSON 心跳的 `ts` 不是合法 ISO-8601。
    TimestampInvalid,
    /// 心跳文件超过读取上限（按内容损坏处理）。
    HeartbeatTooLarge { limit: usize },
}

impl HealthWarning {
    /// 稳定的英文标识（日志/快照共用）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::ClockSkew { .. } => "clock_skew",
            Self::SequenceNotIncreasing { .. } => "sequence_not_increasing",
            Self::TimestampMissing => "timestamp_missing",
            Self::TimestampInvalid => "timestamp_invalid",
            Self::HeartbeatTooLarge { .. } => "heartbeat_too_large",
        }
    }
}

/// 心跳 status 对应的业务信号。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum BusinessSignal {
    /// 本轮没有给出 status（process/http 探针，或心跳读取/解析失败）。
    #[default]
    NotReported,
    /// 本轮 status 存在且未命中 `needs_human_status`，应清除 needs_human。
    Clear,
    /// 本轮 status 命中 `needs_human_status`。
    NeedsHuman(String),
}

/// 一次探测的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// 探测成功。
    Healthy,
    /// 探测失败。
    Unhealthy(HealthFailure),
    /// 尚未观察到任何心跳（只可能出现在启动宽限内）。
    Pending,
}

/// 探针返回的完整报告。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProbeReport {
    pub outcome: ProbeOutcome,
    /// 本次探测耗时（HTTP 为请求往返，其余为本地查询耗时）。
    pub latency: Duration,
    /// HTTP 状态码（仅 http 探针）。
    pub http_status: Option<u16>,
    /// 心跳细节（仅 heartbeat 探针）。
    pub heartbeat: Option<HeartbeatReport>,
    /// 业务状态信号。
    pub business: BusinessSignal,
    /// 不参与存活判定、只用于日志的告警。
    pub warnings: Vec<HealthWarning>,
}

impl ProbeReport {
    /// 成功报告。
    pub fn healthy(latency: Duration) -> Self {
        Self {
            outcome: ProbeOutcome::Healthy,
            latency,
            http_status: None,
            heartbeat: None,
            business: BusinessSignal::NotReported,
            warnings: Vec::new(),
        }
    }

    /// 失败报告。
    pub fn failed(kind: FailureKind, message: impl Into<String>, latency: Duration) -> Self {
        Self {
            outcome: ProbeOutcome::Unhealthy(HealthFailure::new(kind, message)),
            latency,
            http_status: None,
            heartbeat: None,
            business: BusinessSignal::NotReported,
            warnings: Vec::new(),
        }
    }

    /// 尚未见过心跳（启动宽限内由评估器忽略）。
    pub fn pending(latency: Duration) -> Self {
        Self {
            outcome: ProbeOutcome::Pending,
            latency,
            http_status: None,
            heartbeat: None,
            business: BusinessSignal::NotReported,
            warnings: Vec::new(),
        }
    }

    pub fn with_http_status(mut self, status: Option<u16>) -> Self {
        self.http_status = status;
        self
    }

    pub fn with_heartbeat(mut self, heartbeat: HeartbeatReport) -> Self {
        self.heartbeat = Some(heartbeat);
        self
    }

    pub fn with_business(mut self, business: BusinessSignal) -> Self {
        self.business = business;
        self
    }

    pub fn with_warning(mut self, warning: HealthWarning) -> Self {
        self.warnings.push(warning);
        self
    }
}

/// 探针接口：一次调用返回一份报告。
///
/// `now` 由调用方注入，心跳新鲜度/启动宽限都基于这个单调时钟，
/// 测试可以任意推进时间而不 sleep。
pub trait HealthProbe: Send {
    fn probe(&mut self, now: Instant) -> ProbeReport;

    /// 休眠唤醒/时钟跳变后重置观察基线（默认无操作）。
    fn reset_observation(&mut self, _now: Instant) {}
}

/// 健康评估的静态策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HealthPolicy {
    /// 运行期连续失败多少次进入 `Unhealthy`；0 按 1 处理。
    pub failure_threshold: u32,
    /// 进程创建成功后的启动宽限。
    pub startup_grace: Duration,
    /// 休眠唤醒/时钟跳变后的观察宽限。
    pub resume_grace: Duration,
}

impl Default for HealthPolicy {
    fn default() -> Self {
        Self {
            failure_threshold: 3,
            startup_grace: Duration::from_secs(20),
            resume_grace: Duration::from_secs(20),
        }
    }
}

impl HealthPolicy {
    pub fn new(failure_threshold: u32, startup_grace: Duration, resume_grace: Duration) -> Self {
        Self {
            failure_threshold: failure_threshold.max(1),
            startup_grace,
            resume_grace,
        }
    }
}

/// 健康状态（与任务主状态正交：`Unhealthy` 仍可能处于 running）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HealthState {
    /// 尚未开始监督。
    Idle,
    /// 启动宽限内，尚未首次成功。
    Starting,
    /// 探测成功。
    Healthy,
    /// 有失败但未达阈值，或处于 resume 宽限。
    Degraded,
    /// 达到失败阈值，或启动宽限结束仍未成功。
    Unhealthy,
}

impl HealthState {
    /// 稳定的英文标识（快照/日志/前端共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Healthy => "healthy",
            Self::Degraded => "degraded",
            Self::Unhealthy => "unhealthy",
        }
    }
}

impl std::fmt::Display for HealthState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 一次评估的完整结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HealthVerdict {
    pub state: HealthState,
    /// 当前连续健康失败次数（启动宽限/resume 宽限内不计）。
    pub consecutive_failures: u32,
    /// 命中的 `needs_human_status`；`None` = 未置位。
    pub needs_human: Option<String>,
    /// 与上一次评估相比健康状态是否变化（Engine 用于日志/通知去重）。
    pub state_changed: bool,
    /// `needs_human` 是否变化。
    pub needs_human_changed: bool,
    /// 最近一次失败；成功后清空。
    pub failure: Option<HealthFailure>,
    /// 本次 `Unhealthy` 是否属于"启动宽限结束仍未成功"。
    pub startup_failure: bool,
    /// 当前是否处于 suspend 冻结。
    pub suspended: bool,
}

/// 单实例健康评估状态机。
///
/// 每个进程实例应创建一个新的评估器并调用 [`start`](Self::start)；
/// 不要把旧实例的计数/时间带入新实例。
#[derive(Debug, Clone)]
pub struct HealthEvaluator {
    policy: HealthPolicy,
    state: HealthState,
    consecutive_failures: u32,
    ever_healthy: bool,
    startup_deadline: Option<Instant>,
    resume_deadline: Option<Instant>,
    suspended: bool,
    suspended_at: Option<Instant>,
    needs_human: Option<String>,
    last_failure: Option<HealthFailure>,
    startup_failure: bool,
}

impl HealthEvaluator {
    pub fn new(policy: HealthPolicy) -> Self {
        Self {
            policy: HealthPolicy::new(
                policy.failure_threshold,
                policy.startup_grace,
                policy.resume_grace,
            ),
            state: HealthState::Idle,
            consecutive_failures: 0,
            ever_healthy: false,
            startup_deadline: None,
            resume_deadline: None,
            suspended: false,
            suspended_at: None,
            needs_human: None,
            last_failure: None,
            startup_failure: false,
        }
    }

    pub fn policy(&self) -> HealthPolicy {
        self.policy
    }

    pub fn state(&self) -> HealthState {
        self.state
    }

    pub fn consecutive_failures(&self) -> u32 {
        self.consecutive_failures
    }

    pub fn needs_human(&self) -> Option<&str> {
        self.needs_human.as_deref()
    }

    pub fn last_failure(&self) -> Option<&HealthFailure> {
        self.last_failure.as_ref()
    }

    pub fn is_suspended(&self) -> bool {
        self.suspended
    }

    /// 新实例开始监督：清零计数、清除 needs_human、进入启动宽限。
    ///
    /// 若 needs_human 需要跨应用重启保留，由 Engine 在调用后使用
    /// [`set_needs_human`](Self::set_needs_human) 恢复。
    pub fn start(&mut self, now: Instant) -> HealthVerdict {
        let previous = self.state;
        self.state = HealthState::Starting;
        self.consecutive_failures = 0;
        self.ever_healthy = false;
        self.startup_deadline = Some(now + self.policy.startup_grace);
        self.resume_deadline = None;
        self.suspended = false;
        self.suspended_at = None;
        self.needs_human = None;
        self.last_failure = None;
        self.startup_failure = false;
        self.snapshot(self.state != previous, false)
    }

    /// 观察一次探测结果。
    ///
    /// `now` 必须是单调时钟；评估器不读取墙钟。
    pub fn observe(&mut self, report: &ProbeReport, now: Instant) -> HealthVerdict {
        let previous_state = self.state;
        let previous_needs = self.needs_human.clone();

        if self.state == HealthState::Idle || self.suspended {
            return self.snapshot(false, false);
        }

        match &report.business {
            BusinessSignal::NotReported => {}
            BusinessSignal::Clear => self.needs_human = None,
            BusinessSignal::NeedsHuman(status) => self.needs_human = Some(status.clone()),
        }

        match &report.outcome {
            ProbeOutcome::Healthy => {
                self.consecutive_failures = 0;
                self.ever_healthy = true;
                self.startup_deadline = None;
                self.resume_deadline = None;
                self.last_failure = None;
                self.startup_failure = false;
                self.state = HealthState::Healthy;
            }
            ProbeOutcome::Unhealthy(failure) => {
                self.last_failure = Some(failure.clone());
                self.apply_failure(failure, now);
            }
            ProbeOutcome::Pending => {
                let failure = HealthFailure::not_seen();
                self.last_failure = Some(failure.clone());
                self.apply_failure(&failure, now);
            }
        }

        let state_changed = self.state != previous_state;
        let needs_human_changed = self.needs_human != previous_needs;
        self.snapshot(state_changed, needs_human_changed)
    }

    fn apply_failure(&mut self, failure: &HealthFailure, now: Instant) {
        if !self.ever_healthy {
            let in_startup = self.startup_deadline.is_some_and(|deadline| now < deadline);
            if in_startup {
                // 启动宽限内：只记录，不增加失败计数。
                self.state = HealthState::Starting;
                return;
            }
            // 宽限结束仍从未成功：启动失败，不计入 health_failures。
            self.state = HealthState::Unhealthy;
            self.startup_failure = true;
            self.last_failure = Some(HealthFailure::startup_timeout(failure));
            return;
        }

        let in_resume = self.resume_deadline.is_some_and(|deadline| now < deadline);
        if in_resume {
            // 休眠/时钟跳变宽限内失败只记 degraded，不增加失败计数。
            if self.state != HealthState::Unhealthy {
                self.state = HealthState::Degraded;
            }
            return;
        }

        self.startup_failure = false;
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        self.state = if self.consecutive_failures >= self.policy.failure_threshold {
            HealthState::Unhealthy
        } else {
            HealthState::Degraded
        };
    }

    /// 冻结健康计时与失败计数（系统休眠）。
    pub fn suspend(&mut self, now: Instant) {
        if !self.suspended {
            self.suspended = true;
            self.suspended_at = Some(now);
        }
    }

    /// 唤醒：补齐被冻结的宽限，并进入 `resume_grace`。
    ///
    /// 调用方还应调用探针的 [`HealthProbe::reset_observation`]；使用
    /// [`HealthMonitor::resume`] 可以自动完成两者。
    pub fn resume(&mut self, now: Instant) -> HealthVerdict {
        let previous = self.state;
        if let Some(at) = self.suspended_at.take() {
            let paused = now.saturating_duration_since(at);
            // 启动失败已是终态：休眠时长不能再把启动宽限"续"到未来，否则会复活 Starting。
            if !self.startup_failure
                && let Some(deadline) = self.startup_deadline
            {
                self.startup_deadline = Some(deadline + paused);
            }
            if let Some(deadline) = self.resume_deadline {
                self.resume_deadline = Some(deadline + paused);
            }
        }
        self.suspended = false;
        self.reset_observation(now);
        self.snapshot(self.state != previous, false)
    }

    /// 时钟跳变/手动重置观察基线：进入 `resume_grace`，宽限内失败不计。
    ///
    /// 调用方还应重置探针的观察基线；[`HealthMonitor::reset_observation`]
    /// 会同时处理。
    pub fn reset_observation(&mut self, now: Instant) {
        let until = now + self.policy.resume_grace;
        if self.startup_failure {
            // 启动失败（宽限结束仍未成功）是终态：reset/唤醒不得重新打开启动宽限，
            // 否则下一次失败会把状态从 Unhealthy 复活成 Starting，并残留 startup_failure。
            self.startup_deadline = None;
        } else if !self.ever_healthy {
            match self.startup_deadline {
                Some(deadline) if deadline >= until => {}
                _ => self.startup_deadline = Some(until),
            }
        }
        self.resume_deadline = Some(until);
    }

    /// 清除内存中的 needs_human 标志（测试/未来「我已处理」命令使用）。
    ///
    /// 当前产品路径没有显式确认命令：心跳 status 恢复时由 Engine 发
    /// [`crate::restart::RestartEvent::NeedsHumanCleared`] 自动清除。
    pub fn clear_needs_human(&mut self) {
        self.needs_human = None;
    }

    /// 恢复持久化的 needs_human（Engine 在应用重启后调用）。
    pub fn set_needs_human(&mut self, status: impl Into<String>) {
        self.needs_human = Some(status.into());
    }

    /// 当前判定快照（不产生 `state_changed` 事件）。
    pub fn verdict(&self) -> HealthVerdict {
        self.snapshot(false, false)
    }

    fn snapshot(&self, state_changed: bool, needs_human_changed: bool) -> HealthVerdict {
        HealthVerdict {
            state: self.state,
            consecutive_failures: self.consecutive_failures,
            needs_human: self.needs_human.clone(),
            state_changed,
            needs_human_changed,
            failure: self.last_failure.clone(),
            startup_failure: self.startup_failure,
            suspended: self.suspended,
        }
    }
}

/// 探针 + 评估器的便捷组合。
///
/// 每个实例创建一个；`check` 在 suspend 期间不会触发探测。
pub struct HealthMonitor {
    probe: Box<dyn HealthProbe>,
    evaluator: HealthEvaluator,
    last_report: Option<ProbeReport>,
}

impl HealthMonitor {
    /// 创建并立即进入启动宽限（等价于 `evaluator.start(now)`）。
    ///
    /// 同时把探针的观察基线重置到 `now`，因此探针可以在更早创建。
    pub fn new(mut probe: Box<dyn HealthProbe>, policy: HealthPolicy, now: Instant) -> Self {
        let mut evaluator = HealthEvaluator::new(policy);
        evaluator.start(now);
        probe.reset_observation(now);
        Self {
            probe,
            evaluator,
            last_report: None,
        }
    }

    /// 执行一次探测并评估。
    pub fn check(&mut self, now: Instant) -> HealthVerdict {
        if self.evaluator.is_suspended() {
            return self.evaluator.verdict();
        }
        let report = self.probe.probe(now);
        let verdict = self.evaluator.observe(&report, now);
        self.last_report = Some(report);
        verdict
    }

    /// 最近一次探测报告（延迟、HTTP 状态码、心跳细节等）。
    pub fn last_report(&self) -> Option<&ProbeReport> {
        self.last_report.as_ref()
    }

    /// 冻结评估（系统休眠）。
    pub fn suspend(&mut self, now: Instant) {
        self.evaluator.suspend(now);
    }

    /// 唤醒：进入 resume 宽限，并重置探针的观察基线。
    pub fn resume(&mut self, now: Instant) -> HealthVerdict {
        let verdict = self.evaluator.resume(now);
        self.probe.reset_observation(now);
        verdict
    }

    /// 时钟跳变：只重置观察基线，不改变 suspend 状态。
    pub fn reset_observation(&mut self, now: Instant) -> HealthVerdict {
        self.evaluator.reset_observation(now);
        self.probe.reset_observation(now);
        self.evaluator.verdict()
    }

    pub fn evaluator(&self) -> &HealthEvaluator {
        &self.evaluator
    }

    pub fn evaluator_mut(&mut self) -> &mut HealthEvaluator {
        &mut self.evaluator
    }

    pub fn probe(&mut self) -> &mut dyn HealthProbe {
        self.probe.as_mut()
    }
}
