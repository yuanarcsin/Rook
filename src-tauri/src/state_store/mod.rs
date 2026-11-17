//! M3-3：state.json 持久化（纯模块，不依赖 Tauri/Engine/UI）。
//!
//! 本层只回答两个问题：**监督状态怎么落盘、下次启动怎么读回来**。它不创建
//! 进程、不调度定时器、不判断该不该重启，也不决定“恢复时是否允许自动启动”；
//! 这些属于 Engine 的职责。
//!
//! - [`PersistedState`]：与 docs/03 §10 对应的磁盘模型（schema、tasks、window、ui）；
//! - [`load`]/[`save`]：路径由调用方传入；缺失/损坏/未知版本返回默认状态 + 报告；
//!   保存走“临时文件 → sync_all → rename”，内容不变不写；
//! - [`StateStore`]：绑定路径的便捷封装。
//!
//! 磁盘字段使用 snake_case（docs/03 §10 契约），不要改成前端快照的 camelCase。
//!
//! 恢复语义要点：
//!
//! - `desired_override`、`consecutive_failures`、`paused_reason`、`next_retry_at`
//!   必须持久化，避免“重启托盘绕过退避/熔断”；
//! - `needs_human` 的 active/source/message/since 必须持久化，重启后不得自动启动；
//! - 不保存运行中进程/PID/句柄：监督器崩溃时 Job 已由内核回收，重启后按配置
//!   重新拉起，避免“接管孤儿进程”（docs/03 §10）；
//! - 未知字段忽略、未知 schema 整体回退默认状态并报告，由 Engine 决定保守策略。
//!
//! 并发/原子写/损坏处理的细节见 `store` 模块文档。

mod model;
mod store;

#[cfg(test)]
mod tests;

pub use model::{
    AppSession, DesiredOverride, LastStatus, NeedsHumanState, PausedReason, PersistedState,
    SCHEMA_VERSION, TaskState, Timestamp, UiState, WindowState,
};
pub use store::{
    LoadStatus, LoadedState, STATE_FILE_NAME, SaveOutcome, StateStore, StateStoreError, load,
    path_in, save,
};
