//! M2-4a：任务运行时核心（不依赖 Tauri/Engine/前端）。
//!
//! 组合 [`crate::process`]（启动/退出监控/端口预检）与 [`crate::logs`]（日志采集），
//! 为单个任务提供 `start / stop / restart / snapshot` 生命周期：
//!
//! - [`TaskSpec`]：启动一个任务所需的静态信息（命令、参数、环境、端口、日志参数）；
//! - [`InstanceContext`]：实例级注入上下文（任务名、心跳、数据目录、配置文件路径）；
//! - [`TaskRuntime`]：线程安全的状态机，持有当前实例的进程树、日志采集器与退出监控；
//! - [`RuntimeSnapshot`]：任意时刻可读取的完整状态（状态/PID/实例 id/退出码/日志统计）；
//! - [`RuntimeEvent`]：可选的事件通道，供后续 Engine 转发；`Running` 事件携带实例 id
//!   与进程创建时刻（单调时钟），供 Engine 计算 `startup_grace`。
//!
//! 每次 start 生成新的实例 id，并按 docs/02 §5.1 注入 `CAMPUS_SENTRY_*` 保留变量
//! （TASK_ID / TASK_NAME / INSTANCE_ID / HEARTBEAT / LOG_DIR / DATA_DIR / CONFIG /
//! SUPERVISOR_PID / ELEVATED / START_REASON / WINDOW_END / WORK_MODE）；任务自身
//! env 不能覆盖保留变量。START_REASON 每实例必有（[StartReason]），WORK_MODE
//! 固定 `active`，WINDOW_END 只在 `schedule.enabled=true` 且能算出结束时刻时
//! 注入。配置了心跳路径时启动前删除旧心跳文件，避免上一实例残留被误判为新鲜。
//! 路径全部由调用方提供，本层不读取真实用户目录。
//!
//! 本层不调度健康探测/退避定时器、不写 state.json、不做托盘/UI/配置热更新；
//! 进程退出后保持 `stopped`。M3-4b 的 Engine 调度线程组合本层、[supervision]
//! 与 [crate::health]/[crate::restart] 完成健康检查、自动重启与自动启动。
//!
//! [coordinator] 是 M3-4a 增加的纯逻辑子模块：配置代际/in-flight 竞态门、
//! enabled/自动启动策略、用户意图与运行时快照到 state.json 内存模型的合并。
//! [supervision] 是 M3-4b 的纯逻辑：心跳目录检查、健康监视器构造、
//! 状态恢复/回写、自动启动筛选与错峰。两者都不启动进程/线程，便于 Engine
//! 在 Tauri 之外单测。[reconcile] 是 M3-6a 的纯逻辑：配置字段分类与
//! `apply_changes` 决策（立即生效 / 重建监督 / 重启 / restart_pending）。
//! [power] 是 M3-6b 的纯逻辑：休眠/唤醒/时钟跳变跟踪、计时器平移，以及
//! 无窗口的 `PowerRegisterSuspendResumeNotification` 接线（回调只写原子标志）。

mod coordinator;
mod power;
mod reconcile;
mod state;
mod supervision;
mod task;

#[cfg(test)]
mod tests;

pub use coordinator::{
    ReloadGate, UserAction, apply_runtime_snapshot, apply_user_intent, auto_start_allowed,
    manual_restart_allowed, manual_start_allowed, prune_removed_tasks,
};
pub use power::{
    CLOCK_JUMP_THRESHOLD, ClockJump, ClockJumpDirection, PowerMoment, PowerSignalSink,
    PowerSignals, PowerStamp, PowerState, PowerTracker, PowerWake, PowerWiring, clock_skew_between,
    resume_deadline, shift_timer,
};
pub use reconcile::{
    ConfigChange, ReconcileDecision, classify as classify_config_change,
    decide as decide_reconcile, update_baseline,
};
pub use state::{
    ExitReason, RuntimeError, RuntimeEvent, RuntimeSnapshot, RuntimeStage, TaskStatus,
};
pub use supervision::{
    AutoStartCandidate, AutoStartInput, AutoStartPlan, ConfigReconcile, ReadinessPlan,
    alive_status_for_rebuild, build_health_monitor, count_restart_success, ensure_heartbeat_dir,
    health_interval, instance_matches, machine_tick_due, plan_auto_start, probe_due,
    readiness_deadline_reached, readiness_first_healthy, readiness_plan, reconcile_action,
    replay_alive_machine, restore_seed, running_instance_accepted, should_log_stop_retry,
    starting_instance_accepted, stop_retry_delay, sync_state,
};
pub use task::{InstanceContext, StartReason, TaskRuntime, TaskSpec};
