//! M3-2：重启/退避/熔断决策状态机（不依赖 Tauri/Engine/UI）。
//!
//! 本层回答一个问题：**进程退出、健康失败、用户停止、稳定运行之后，
//! 该不该重启、何时重启、要不要熔断？** 它不创建进程、不调度定时器、
//! 不读写 state.json；Engine（M3-4b）把监督事件交给
//! [`RestartMachine::handle`]，再按 [`Decision`] 执行/排期，并用
//! [`RestartMachine::restore`] 从 state.json 恢复暂停/退避记忆。
//!
//! - [`RestartPolicy`]：`[tasks.restart]` 的归一化视图；
//! - [`RestartMachine`]：纯状态机，时间由 `handle(event, now)` 注入，
//!   随机数由 [`RandomSource`] 注入；
//! - [`Decision`]：`Stay` / `Stopped` / `Paused` / `StartNow` / `Backoff` /
//!   `Stable`，覆盖"不重启 / 等待后退避重启 / 立即重启 / 清零计数"。
//!
//! 语义要点（docs/03 §5–§7）：
//!
//! - `restart.mode`：`on-failure` 下退出码 0 = clean exit → stopped；
//!   `always` 下 clean exit 也重启但不计失败；`never` 下任何退出都 paused；
//! - `pause_on_exit_codes` 命中 → paused(needs_human)，不重启、不计失败；
//! - `max_consecutive_failures`（0 = 不熔断）达到 → paused(crash_loop)；
//! - 稳定运行 `stable_after_ms` → 清零 `consecutive_failures`；
//! - 用户 Stop 后意图为 stopped，迟到退出/健康失败/tick 都不会自动启动；
//! - `needs_human` 置位期间抑制一切自动重启，但不停止存活进程。
//!
//! 实例 id 过滤、配置重载、应用退出、休眠/时钟跳变由 Engine 负责；
//! 本模块只处理"重启决策"本身。

mod config;
mod state;

#[cfg(test)]
mod tests;

pub use config::{
    MAX_JITTER, MIN_DELAY_MS, RandomSource, RestartPolicy, SystemRandom, backoff_delay,
};
pub use state::{
    Decision, Intent, NeedsHuman, Reason, RestartEvent, RestartMachine, RestartSeed,
    RestartSnapshot, RestartStatus,
};
