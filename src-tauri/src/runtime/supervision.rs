//! M3-4b：Engine 监督决策的纯逻辑（不依赖 Tauri/UI）。
//!
//! Engine 的后台调度线程只负责“什么时候做事”；本模块回答“该不该做”，
//! 便于用假时钟/假状态单测：
//!
//! - [ensure_heartbeat_dir]：spawn 前创建心跳目录并验证可写；
//! - [build_health_monitor]：按配置构造 process/heartbeat/http 探针；
//! - [restore_seed]：把 state.json 的暂停/退避/失败计数恢复成 [RestartSeed]；
//! - [sync_state]：把状态机快照写回 state.json 内存模型（只报告变化）；
//! - [plan_auto_start]：启动时自动启动候选 + 错峰 + 退避到期规则；
//! - [probe_due] / [instance_matches]：调度与 instance_id 过滤。
//!
//! 不启动进程、不调度线程；真正执行由 Engine 负责。

use std::fs;
use std::time::{Duration, Instant};

use crate::config::{HealthKind, TaskConfig};
use crate::health::{
    HealthMonitor, HealthPolicy, HealthProbe, HealthState, HeartbeatProbe, HeartbeatSpec,
    HttpProbe, HttpSpec, ProcessProbe,
};
use crate::restart::{
    NeedsHuman, RandomSource, Reason, RestartEvent, RestartMachine, RestartSeed, RestartSnapshot,
    RestartStatus,
};
use crate::state_store::{
    DesiredOverride, LastStatus, NeedsHumanState, PausedReason, TaskState, Timestamp,
};

use super::InstanceContext;
use super::state::TaskStatus;

/// 心跳目录写入探测失败/心跳路径非法的统一错误说明。
pub fn ensure_heartbeat_dir(context: &InstanceContext) -> Result<(), String> {
    let Some(path) = context.heartbeat.as_ref() else {
        // 非 heartbeat 任务没有心跳路径，无需创建目录。
        return Ok(());
    };
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Err(format!("心跳路径没有父目录：{}", path.display()));
    };
    fs::create_dir_all(parent)
        .map_err(|error| format!("创建心跳目录 {} 失败：{error}", parent.display()))?;

    // 只创建目录不足以证明可写（目录 ACL/磁盘满都会在脚本写心跳时失败），
    // 用一个临时文件做一次写入探测，成功后立即删除。
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "heartbeat".to_owned());
    let probe = parent.join(format!(
        ".campus-sentry-write-test-{}-{file_name}",
        std::process::id()
    ));
    fs::write(&probe, b"")
        .map_err(|error| format!("心跳目录 {} 不可写：{error}", parent.display()))?;
    fs::remove_file(&probe)
        .map_err(|error| format!("清理心跳目录写入探测文件 {} 失败：{error}", probe.display()))?;
    Ok(())
}

/// 按配置构造当前实例的健康监视器；配置缺字段时返回 None（由 Engine 记 warn）。
pub fn build_health_monitor(
    task: &TaskConfig,
    context: &InstanceContext,
    pid: u32,
    instance_id: &str,
    started_instant: Instant,
) -> Option<HealthMonitor> {
    let policy = HealthPolicy::new(
        task.health.failure_threshold,
        Duration::from_millis(task.health.startup_grace_ms),
        Duration::from_millis(task.power.resume_grace_ms),
    );
    let probe: Box<dyn HealthProbe> = match task.health.kind {
        HealthKind::Process => Box::new(ProcessProbe::new(pid)),
        HealthKind::Heartbeat => {
            let path = context.heartbeat.clone()?;
            let max_age = task
                .health
                .max_age_ms
                .map(Duration::from_millis)
                .unwrap_or_else(|| health_interval(task) * 3);
            Box::new(HeartbeatProbe::new(
                HeartbeatSpec {
                    path,
                    instance_id: instance_id.to_owned(),
                    max_age,
                    needs_human_status: task.health.needs_human_status.clone(),
                },
                started_instant,
            ))
        }
        HealthKind::Http => {
            let url = task.health.url.clone()?;
            Box::new(HttpProbe::new(HttpSpec {
                url,
                timeout: Duration::from_millis(task.health.timeout_ms),
                expect_status: task.health.expect_status.clone(),
                expect_body_contains: task.health.expect_body_contains.clone(),
            }))
        }
    };
    Some(HealthMonitor::new(probe, policy, started_instant))
}

/// 健康探测间隔（配置保证 > 0；防御性下限 1ms）。
pub fn health_interval(task: &TaskConfig) -> Duration {
    Duration::from_millis(task.health.interval_ms.max(1))
}

/// 下一次探测是否到期。
pub fn probe_due(next_health_at: Option<Instant>, now: Instant) -> bool {
    next_health_at.is_some_and(|at| now >= at)
}

/// 状态机是否需要 Tick（退避到期 / stable_after 到期），避免每 tick 打空转。
pub fn machine_tick_due<R: RandomSource>(machine: &RestartMachine<R>, now: Instant) -> bool {
    match machine.status() {
        RestartStatus::Backoff => machine.next_retry_at().is_some_and(|at| now >= at),
        RestartStatus::Running => {
            machine.consecutive_failures() > 0
                && machine.running_since().is_some_and(|at| {
                    now.saturating_duration_since(at) >= machine.policy().stable_after
                })
        }
        _ => false,
    }
}

/// 事件/探测结果是否属于当前实例（docs/03 §11 不变量 6）。
pub fn instance_matches(current: Option<&str>, candidate: Option<&str>) -> bool {
    match (current, candidate) {
        (Some(current), Some(candidate)) => current == candidate,
        _ => false,
    }
}

/// Running 事件必须严格匹配当前实例（N1：不允许 None 回退）。
pub fn running_instance_accepted(current: Option<&str>, event: Option<&str>) -> bool {
    matches!((current, event), (Some(current), Some(event)) if current == event)
}

/// Starting 事件只在"确实发出过启动且是当前运行时的新实例"时接受（N1）。
///
/// - `current`：supervisor 记录的当前实例 id（`None` = 已停止/刚清空）；
/// - `previous`：最近结束/被替换的实例 id（清空 `current` 前保存）；
/// - `latest`：运行时快照里的实例 id（事件转发积压时可能已前移到新实例）；
/// - `event`：Starting 事件携带的实例 id。
///
/// 旧事件积压时，`current` 可能已是 `None`，单靠 `current != event` 会把旧
/// Starting 当成新实例接受（随后新 Starting/Running 被丢弃，新进程没有
/// monitor，状态机卡在 Starting）。因此还要求 `latest == event` 且
/// `previous != event`：旧实例 id 不可能再是运行时的最新实例，也已被记录为
/// previous；重复的同一实例 Starting 由 `current != event` 拒绝。
pub fn starting_instance_accepted(
    current: Option<&str>,
    previous: Option<&str>,
    latest: Option<&str>,
    event: Option<&str>,
    expect_start: bool,
) -> bool {
    let Some(event) = event else {
        return false;
    };
    if latest != Some(event) {
        return false;
    }
    expect_start && current != Some(event) && previous != Some(event)
}

/// 启动就绪策略（B1/Q10）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadinessPlan {
    /// heartbeat/http：首次健康通过才就绪。
    pub await_first_healthy: bool,
    /// process：进程创建后 startup_grace 到期且仍存活才就绪。
    pub deadline: Option<Instant>,
}

/// 按健康类型计算就绪条件；process 不使用 300ms 特例，只等 startup_grace_ms。
pub fn readiness_plan(
    kind: HealthKind,
    started: Instant,
    startup_grace: Duration,
) -> ReadinessPlan {
    // 所有模式都有启动宽限：heartbeat/http 等首次健康，process 等宽限到期；
    // 到期仍未就绪由 Engine 转 StartFailed（B1）。
    ReadinessPlan {
        await_first_healthy: !matches!(kind, HealthKind::Process),
        deadline: Some(started + startup_grace),
    }
}

/// process 模式的启动宽限是否已到期。
pub fn readiness_deadline_reached(plan: ReadinessPlan, now: Instant) -> bool {
    plan.deadline.is_some_and(|deadline| now >= deadline)
}

/// heartbeat/http 首次健康是否应把实例标记为 running。
pub fn readiness_first_healthy(plan: ReadinessPlan, health: HealthState) -> bool {
    plan.await_first_healthy && health == HealthState::Healthy
}

/// 配置变化后对运行中实例的收敛动作（B3/B13）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigReconcile {
    /// 保持现状。
    Keep,
    /// 停止实例，但保留配置条目与 state（enabled=false）。
    StopAndKeep,
    /// 停止实例，随后删除 runtime/supervisor/state 与心跳（任务删除/失效）。
    StopAndRemove,
}

/// 由配置有效性与运行状态推导 reconcile 动作。
pub fn reconcile_action(
    valid: bool,
    enabled: bool,
    runtime_status: Option<TaskStatus>,
) -> ConfigReconcile {
    // 任务删除/失效：即使已经停止也要清理 runtime/state/心跳（B3）。
    if !valid {
        return ConfigReconcile::StopAndRemove;
    }
    match runtime_status {
        None | Some(TaskStatus::Stopped) => ConfigReconcile::Keep,
        Some(_) if !enabled => ConfigReconcile::StopAndKeep,
        _ => ConfigReconcile::Keep,
    }
}

/// 健康停止失败后的重试退避：1s、2s、4s 封顶（B2）。
pub fn stop_retry_delay(attempt: u32) -> Duration {
    let ms = match attempt {
        0 | 1 => 1000,
        2 => 2000,
        _ => 4000,
    };
    Duration::from_millis(ms)
}

/// 停止重试日志限流：前两次和 2 的幂次（1,2,4,8...）记录，其余降为 debug。
pub fn should_log_stop_retry(attempt: u32) -> bool {
    attempt <= 2 || attempt.is_power_of_two()
}

/// 把“进程仍存活”的事实重放到状态机（配置重建 / Restart 停止失败恢复）。
///
/// `alive` 是运行时实际看到的状态（不是状态机状态）：Running 重放
/// `StartRequested + Started`，Starting 只重放 `StartRequested`。
///
/// `needs_human` 非空时在重放之后才发 `NeedsHumanObserved`：存活进程保持
/// running，只带人工处理标志；不能交给 [`RestartMachine::restore`]，否则
/// 会把活着的实例强制写成 paused(needs_human)。
pub fn replay_alive_machine<R: RandomSource>(
    machine: &mut RestartMachine<R>,
    alive: RestartStatus,
    needs_human: Option<NeedsHuman>,
    now: Instant,
) -> bool {
    match alive {
        RestartStatus::Running => {
            let _ = machine.handle(RestartEvent::StartRequested, now);
            let _ = machine.handle(RestartEvent::Started, now);
        }
        RestartStatus::Starting => {
            let _ = machine.handle(RestartEvent::StartRequested, now);
        }
        _ => return false,
    }
    if let Some(needs) = needs_human {
        let _ = machine.handle(
            RestartEvent::NeedsHumanObserved {
                source: needs.source,
                message: needs.message,
            },
            now,
        );
    }
    true
}

/// 配置重建/停止失败恢复时，按“状态机是否仍在启动中”决定重放哪个存活状态。
///
/// `TaskRuntime` 在 spawn 成功后就报告 running，但监督状态机要等首次健康
/// （heartbeat/http）或启动宽限（process）才离开 starting。因此不能只看
/// 运行时状态：状态机仍在 starting 时必须重放 starting，否则会跳过 readiness
/// 与启动宽限（QA C）。
pub fn alive_status_for_rebuild(
    machine_status: RestartStatus,
    runtime_status: Option<TaskStatus>,
) -> Option<RestartStatus> {
    if machine_status == RestartStatus::Starting {
        return matches!(
            runtime_status,
            Some(TaskStatus::Starting | TaskStatus::Running)
        )
        .then_some(RestartStatus::Starting);
    }
    match runtime_status {
        Some(TaskStatus::Running) => Some(RestartStatus::Running),
        Some(TaskStatus::Starting) => Some(RestartStatus::Starting),
        _ => None,
    }
}

/// 从 state.json 恢复重启监督种子。
///
/// - paused / needs_human 优先于 backoff；
/// - next_retry_at 已过期时至少等 restart.initial_delay_ms 再启动，
///   避免重启托盘绕过退避（docs/03 §10）；
/// - 配置层未知的 paused 原因保持 paused 意图但映射不到状态机 Reason。
pub fn restore_seed(
    task: &TaskConfig,
    state: Option<&TaskState>,
    mono_now: Instant,
    wall_now: Timestamp,
) -> RestartSeed {
    let Some(state) = state else {
        return RestartSeed::default();
    };
    let paused_reason = state.paused_reason.as_ref().map(map_paused_reason);
    let paused = state.paused_reason.is_some()
        || matches!(state.last_status, Some(LastStatus::Paused))
        || matches!(state.desired_override, Some(DesiredOverride::Paused));
    let needs_human = state.needs_human.active.then(|| NeedsHuman {
        source: state
            .needs_human
            .source
            .clone()
            .unwrap_or_else(|| "restored".to_owned()),
        message: state
            .needs_human
            .message
            .clone()
            .unwrap_or_else(|| "需要人工处理".to_owned()),
        since: mono_now,
    });
    let initial_delay = Duration::from_millis(task.restart.initial_delay_ms);
    let next_retry_at = state.next_retry_at.map(|retry_at| {
        let remaining = remaining_between(retry_at, wall_now);
        if remaining.is_zero() {
            mono_now + initial_delay
        } else {
            mono_now + remaining
        }
    });
    RestartSeed {
        consecutive_failures: state.consecutive_failures,
        paused,
        paused_reason,
        needs_human,
        next_retry_at,
    }
}

/// 把状态机快照写回持久化模型；返回是否有字段变化（Engine 仅变化时落盘）。
pub fn sync_state(
    state: &mut TaskState,
    machine: &RestartSnapshot,
    mono_now: Instant,
    wall_now: Timestamp,
) -> bool {
    let mut changed = false;

    if state.consecutive_failures != machine.consecutive_failures {
        state.consecutive_failures = machine.consecutive_failures;
        changed = true;
    }

    // N3：UserPause/ElevationUnavailable/Other 在状态机里只能映射成
    // Reason::UserStop；paused 时映射不到就保留 state 里的稳定枚举，
    // 避免把用户暂停/提权原因清成 None。
    let mapped_paused = machine.paused_reason.and_then(map_reason_to_paused);
    let paused_reason = if machine.status == RestartStatus::Paused {
        mapped_paused.or_else(|| state.paused_reason.clone())
    } else {
        mapped_paused
    };
    if state.paused_reason != paused_reason {
        state.paused_reason = paused_reason;
        changed = true;
    }

    // N2：active→active 且 source/message 不变时保留原 since；只有新激活
    // 或来源变化才写 wall_now，避免每次 decision 都产生无意义变更。
    let needs_human = match &machine.needs_human {
        Some(needs) => {
            let unchanged = state.needs_human.active
                && state.needs_human.source.as_deref() == Some(needs.source.as_str())
                && state.needs_human.message.as_deref() == Some(needs.message.as_str());
            NeedsHumanState {
                active: true,
                source: Some(needs.source.clone()),
                message: Some(needs.message.clone()),
                since: if unchanged {
                    state.needs_human.since
                } else {
                    Some(wall_now)
                },
            }
        }
        None => NeedsHumanState::default(),
    };
    if state.needs_human != needs_human {
        state.needs_human = needs_human;
        changed = true;
    }

    let next_retry_at = machine
        .next_retry_at
        .map(|at| add_duration(wall_now, at.saturating_duration_since(mono_now)));
    if state.next_retry_at != next_retry_at {
        state.next_retry_at = next_retry_at;
        changed = true;
    }

    let last_status = last_status(machine.status);
    if state.last_status.as_ref() != Some(&last_status) {
        state.last_status = Some(last_status);
        changed = true;
    }

    let last_reason = machine.last_reason.map(|reason| reason.as_str().to_owned());
    if state.last_reason != last_reason {
        state.last_reason = last_reason;
        changed = true;
    }

    changed
}

/// N6：自动重启只有成功就绪后才计入 total_restarts；返回是否计数。
pub fn count_restart_success(state: &mut TaskState, pending: bool) -> bool {
    if !pending {
        return false;
    }
    state.total_restarts = state.total_restarts.saturating_add(1);
    true
}

/// 自动启动候选输入：配置 + 恢复的持久状态。
pub struct AutoStartInput<'a> {
    pub task: &'a TaskConfig,
    pub state: Option<&'a TaskState>,
}

/// 一个已排期的自动启动。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutoStartCandidate {
    pub task_id: String,
    /// 最早允许启动的单调时刻（含错峰/退避剩余）。
    pub due: Instant,
    /// 是否来自上次的 backoff 状态（用于日志/统计）。
    pub restored_backoff: bool,
}

/// 自动启动计划。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AutoStartPlan {
    pub candidates: Vec<AutoStartCandidate>,
    /// 未排期的任务与原因（稳定英文标识，供日志）。
    pub skipped: Vec<(String, &'static str)>,
}

/// 计算启动时的自动启动候选与错峰时间（纯逻辑）。
///
/// - 首个候选立即（now），其余按 stagger 递增；
/// - 上次 backoff：取 max(错峰时间, 退避剩余)；退避已过期则至少再等
///   restart.initial_delay_ms；
/// - paused / needs_human / desired_override 非空 / enabled=false / auto_start=false
///   一律跳过并记录原因。
pub fn plan_auto_start(
    inputs: &[AutoStartInput<'_>],
    now: Instant,
    stagger: Duration,
    wall_now: Timestamp,
) -> AutoStartPlan {
    let mut plan = AutoStartPlan::default();
    let mut index = 0u32;

    for input in inputs {
        let task = input.task;
        let state = input.state;
        let skip = if !task.enabled {
            Some("disabled")
        } else if !task.auto_start {
            Some("auto_start_disabled")
        } else if state.is_some_and(|state| state.desired_override.is_some()) {
            Some("desired_override")
        } else if state.is_some_and(|state| {
            state.paused_reason.is_some() || matches!(state.last_status, Some(LastStatus::Paused))
        }) {
            Some("paused")
        } else if state.is_some_and(|state| state.needs_human.active) {
            Some("needs_human")
        } else {
            None
        };
        if let Some(reason) = skip {
            plan.skipped.push((task.id.clone(), reason));
            continue;
        }

        let stagger_due = now + stagger * index;
        index = index.saturating_add(1);
        let initial_delay = Duration::from_millis(task.restart.initial_delay_ms);
        let (due, restored_backoff) =
            if state.is_some_and(|state| matches!(state.last_status, Some(LastStatus::Backoff))) {
                let remaining = state
                    .and_then(|state| state.next_retry_at)
                    .map(|at| remaining_between(at, wall_now))
                    .unwrap_or_default();
                let due = if remaining.is_zero() {
                    now + initial_delay
                } else {
                    now + remaining
                };
                (due.max(stagger_due), true)
            } else {
                (stagger_due, false)
            };

        plan.candidates.push(AutoStartCandidate {
            task_id: task.id.clone(),
            due,
            restored_backoff,
        });
    }

    plan
}

fn remaining_between(at: Timestamp, now: Timestamp) -> Duration {
    let delta = at.datetime().signed_duration_since(now.datetime());
    if delta <= chrono::Duration::zero() {
        Duration::ZERO
    } else {
        delta.to_std().unwrap_or_default()
    }
}

fn add_duration(at: Timestamp, duration: Duration) -> Timestamp {
    let delta = chrono::Duration::from_std(duration).unwrap_or_default();
    Timestamp::new(at.datetime() + delta)
}

fn map_paused_reason(reason: &PausedReason) -> Reason {
    match reason {
        PausedReason::CrashLoop => Reason::CrashLoop,
        PausedReason::RestartBudget => Reason::RestartBudget,
        PausedReason::NeedsHuman => Reason::NeedsHuman,
        PausedReason::CrashNoRestart => Reason::CrashNoRestart,
        // 用户暂停/提权不可用等没有对应状态机 Reason；靠 paused 标记保持意图。
        PausedReason::UserPause | PausedReason::ElevationUnavailable | PausedReason::Other(_) => {
            Reason::UserStop
        }
    }
}

fn map_reason_to_paused(reason: Reason) -> Option<PausedReason> {
    match reason {
        Reason::CrashLoop => Some(PausedReason::CrashLoop),
        Reason::RestartBudget => Some(PausedReason::RestartBudget),
        Reason::NeedsHuman => Some(PausedReason::NeedsHuman),
        Reason::CrashNoRestart => Some(PausedReason::CrashNoRestart),
        _ => None,
    }
}

fn last_status(status: RestartStatus) -> LastStatus {
    match status {
        RestartStatus::Stopped => LastStatus::Stopped,
        RestartStatus::Starting => LastStatus::Starting,
        RestartStatus::Running => LastStatus::Running,
        RestartStatus::Backoff => LastStatus::Backoff,
        RestartStatus::Paused => LastStatus::Paused,
    }
}
