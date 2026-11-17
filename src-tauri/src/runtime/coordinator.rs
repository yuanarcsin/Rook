//! M3-4a：Engine 协调的纯逻辑（不依赖 Tauri/Engine/UI）。
//!
//! Engine 的「配置重载」与「启停命令」在不同线程并发；本模块把其中可测的
//! 决策与状态合并抽出来：
//!
//! - `ReloadGate`：配置代际 + 每任务 in-flight 标记。命令执行期间 reload
//!   不得替换/丢弃该任务的 runtime，否则旧 runtime 可能刚被命令启动就从表里
//!   消失（进程在跑但 UI 停止不了）——这就是 reload/start 竞态。命令完成后
//!   若代际变化，Engine 再补一次 sync。
//! - `manual_start_allowed` / `manual_restart_allowed`：`enabled=false`
//!   一律不可手动启动（docs/02 §4）；Engine 命令与托盘/UI 共用同一语义，
//!   Stop 仍然允许，避免禁用后无法收敛正在运行的实例。
//! - `auto_start_allowed`：docs/03 §4 的自动启动判定。本里程碑只恢复状态，
//!   不实际调度自动启动（下一任务接线）。
//! - `apply_user_intent` / `apply_runtime_snapshot`：把用户命令与运行时
//!   事件合并进 state.json 的内存模型（纯函数；落盘由 Engine 负责）。
//!
//! 不启动进程、不读写文件、不调度定时器。

use std::collections::HashSet;

use crate::state_store::{DesiredOverride, LastStatus, PersistedState, TaskState, Timestamp};

use super::state::{RuntimeSnapshot, TaskStatus};

/// 用户可见的启停动作（与 Engine 的 start/stop/restart 命令一一对应）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UserAction {
    /// 启动；清除 stopped/paused override 与失败计数（docs/03 §4）。
    Start,
    /// 停止；写入 `desired_override=stopped`，保留 `consecutive_failures`。
    Stop,
    /// 停止后立即启动；Q1 裁定：清除 override（用户显式要它运行），
    /// 同时清零失败计数与 needs_human。
    Restart,
}

impl UserAction {
    /// 稳定英文标识（日志/命令结果共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
            Self::Restart => "restart",
        }
    }
}

/// 配置代际 + 任务 in-flight 标记。
///
/// Engine 在取到 `Arc<TaskRuntime>` 之前调用 `ReloadGate::try_begin_command`，
/// 命令结束后调用 `ReloadGate::finish_command`；`reload` 每次递增代际。
/// `sync_runtimes` 看到任务 in-flight 时保留旧 runtime，不与本次重载的表交换。
#[derive(Debug, Default)]
pub struct ReloadGate {
    generation: u64,
    in_flight: HashSet<String>,
}

impl ReloadGate {
    /// 初始代际为 0。
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前配置代际；每次 reload 递增。
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// 标记任务命令开始；同一任务已有命令在执行时返回 `None`（Engine 拒绝并发命令）。
    ///
    /// 返回本次命令看到的配置代际，供命令结束时判断期间是否发生 reload。
    pub fn try_begin_command(&mut self, id: &str) -> Option<u64> {
        if !self.in_flight.insert(id.to_owned()) {
            return None;
        }
        Some(self.generation)
    }

    /// 结束任务命令；返回期间是否发生过配置重载。
    ///
    /// Engine 据此决定是否在命令完成后补一次 `sync_runtimes`（把重载时因为
    /// in-flight 而跳过的替换补上）。
    pub fn finish_command(&mut self, id: &str, started_generation: u64) -> bool {
        self.in_flight.remove(id);
        self.generation != started_generation
    }

    /// 该任务是否有命令正在执行。
    pub fn is_in_flight(&self, id: &str) -> bool {
        self.in_flight.contains(id)
    }

    /// 当前有命令在执行的任务 id（只读，供配置基线保留旧值）。
    pub fn in_flight(&self) -> &HashSet<String> {
        &self.in_flight
    }

    /// 记录一次配置重载，返回新代际（成功/失败都递增）。
    pub fn reload(&mut self) -> u64 {
        self.generation = self.generation.wrapping_add(1);
        self.generation
    }
}

/// 手动 Start 是否允许（docs/02 §4：enabled=false 不可手动启动）。
pub fn manual_start_allowed(valid: bool, enabled: bool, status: TaskStatus) -> bool {
    valid && enabled && status == TaskStatus::Stopped
}

/// 手动 Restart 是否允许（停止后立即启动，因此同样受 enabled 约束）。
pub fn manual_restart_allowed(valid: bool, enabled: bool, status: TaskStatus) -> bool {
    valid && enabled && matches!(status, TaskStatus::Stopped | TaskStatus::Running)
}

/// 监督器启动时是否允许自动拉起某个任务（docs/03 §4 / Q5）。
///
/// 条件：`enabled && auto_start && desired_override=null`，且恢复出的
/// `paused` / `needs_human` 不抑制启动。`last_status=paused/backoff` 不直接
/// 自动启动：Engine 先由 `plan_auto_start` 按退避剩余 + initial_delay 排期，
/// 再在真正启动前用 `auto_start_permitted` 复核。`last_status=stopped` 不抑制，
/// 因为 `auto_start` 任务在正常关机后仍应被拉起。
pub fn auto_start_allowed(
    enabled: bool,
    auto_start: bool,
    has_desired_override: bool,
    paused: bool,
    needs_human: bool,
    last_status: Option<&str>,
) -> bool {
    enabled
        && auto_start
        && !has_desired_override
        && !paused
        && !needs_human
        && !matches!(last_status, Some("paused" | "backoff"))
}

/// 用户命令成功/意图变更后更新持久状态（docs/03 §4、§6.3）。
///
/// - Start：清除 override、paused、needs_human，失败计数清零、取消退避；
/// - Restart：清除 override（Q1 裁定）、paused/needs_human，失败计数清零、取消退避；
/// - Stop：override=stopped，保留失败计数（下次手动 Start 清零），清除 paused/
///   needs_human/退避计时。
pub fn apply_user_intent(state: &mut TaskState, action: UserAction) {
    match action {
        UserAction::Start => {
            state.desired_override = None;
            state.paused_reason = None;
            state.consecutive_failures = 0;
            state.next_retry_at = None;
            state.needs_human = Default::default();
        }
        UserAction::Restart => {
            // Q1 裁定：显式 Restart = “要它运行”，与 §5/invariant 5 一致。
            state.desired_override = None;
            state.paused_reason = None;
            state.consecutive_failures = 0;
            state.next_retry_at = None;
            state.needs_human = Default::default();
        }
        UserAction::Stop => {
            state.desired_override = Some(DesiredOverride::Stopped);
            state.paused_reason = None;
            state.next_retry_at = None;
            state.needs_human = Default::default();
        }
    }
}

/// 把运行时快照合并进持久状态（只更新运行事实，不猜测重启/熔断策略）。
///
/// - `last_status` 跟随运行时；
/// - 进入 `running` 且上次不是 running：`total_starts += 1`、记录启动时间、
///   取消退避计时；
/// - 进入 `stopped`：记录退出码/退出时间；启动失败记录 `start_failed:<stage>`，
///   自然退出/用户停止记录 exit reason（后续里程碑会细化为 clean_exit 等）。
///
/// 返回是否有字段变化，便于 Engine 决定是否落盘。
pub fn apply_runtime_snapshot(
    state: &mut TaskState,
    snapshot: &RuntimeSnapshot,
    now: Timestamp,
) -> bool {
    let mut changed = false;
    let previous = state
        .last_status
        .as_ref()
        .map(LastStatus::as_str)
        .map(str::to_owned);
    let status = last_status(snapshot.status);
    let entering = previous.as_deref() != Some(status.as_str());
    if entering {
        state.last_status = Some(status);
        changed = true;
    }

    match snapshot.status {
        TaskStatus::Running => {
            if entering {
                state.total_starts = state.total_starts.saturating_add(1);
                state.last_start_at = Some(now);
                state.next_retry_at = None;
                changed = true;
            }
        }
        TaskStatus::Stopped => {
            if let Some(code) = snapshot.exit_code
                && state.last_exit_code != Some(code)
            {
                state.last_exit_code = Some(code);
                changed = true;
            }
            if snapshot.exit_code.is_some() && state.last_exit_at != Some(now) {
                state.last_exit_at = Some(now);
                changed = true;
            }
            let reason = match (snapshot.last_error.as_ref(), snapshot.exit_reason) {
                (Some(error), _) => Some(format!("start_failed:{}", error.stage().key())),
                (None, Some(reason)) => Some(reason.as_str().to_owned()),
                (None, None) => None,
            };
            if let Some(reason) = reason
                && state.last_reason.as_deref() != Some(reason.as_str())
            {
                state.last_reason = Some(reason);
                changed = true;
            }
        }
        TaskStatus::Starting | TaskStatus::Stopping => {}
    }

    changed
}

/// 配置删除任务后清理对应持久状态；返回被删除的 id（供日志）。
///
/// 未知 task id 是否保留由 Engine 决定：这里只在“任务已从配置删除”时调用。
pub fn prune_removed_tasks(state: &mut PersistedState, known: &HashSet<String>) -> Vec<String> {
    let removed: Vec<String> = state
        .tasks
        .keys()
        .filter(|id| !known.contains(id.as_str()))
        .cloned()
        .collect();
    for id in &removed {
        state.remove_task(id);
    }
    removed
}

fn last_status(status: TaskStatus) -> LastStatus {
    match status {
        TaskStatus::Stopped => LastStatus::Stopped,
        TaskStatus::Starting => LastStatus::Starting,
        TaskStatus::Running => LastStatus::Running,
        TaskStatus::Stopping => LastStatus::Stopping,
    }
}
