//! Tauri 命令层（A1）：按业务域拆分，供 `lib.rs` 的 `generate_handler!` 注册。
//!
//! 命令只负责参数校验、把 `State`/`AppHandle` 交给 core/宿主层、把结果序列化；
//! 编排逻辑在 [`crate::engine`]（可测）与 [`crate::engine_host`]（线程/广播）。

pub(crate) mod config;
pub(crate) mod events;
pub(crate) mod log;
pub(crate) mod plugin;
pub(crate) mod schedule;
pub(crate) mod system;
pub(crate) mod task;

// ---------------------------------------------------------------- 命令共用

impl crate::engine::Engine {
    /// 配置写命令共用：当前配置文件路径 + 启动时的 `--data-dir` 覆盖项。
    ///
    /// 路径来自启动时的解析结果，前端无法指定任意文件；`action` 只用于错误文案，
    /// `task_id` 在插件/任务命令里透传给统一错误 DTO。
    pub(crate) fn config_write_target(
        &self,
        action: &str,
        task_id: Option<String>,
    ) -> Result<(std::path::PathBuf, Option<std::path::PathBuf>), crate::ipc_error::IpcError> {
        let path = self.config_file_path();
        if path.as_os_str().is_empty() {
            return Err(crate::ipc_error::IpcError::with_task(
                "config",
                format!("配置文件路径未知，无法{action}"),
                task_id,
            ));
        }
        Ok((
            path,
            crate::data_dir::data_dir_override(&self.args, &self.cwd),
        ))
    }
}

/// 各命令模块共用的一组导入：子模块统一写 `use super::prelude::*;`。
pub(crate) mod prelude {
    pub(crate) use std::path::{Path, PathBuf};
    pub(crate) use std::sync::{Arc, Mutex};

    pub(crate) use serde::Serialize;
    pub(crate) use tauri::{AppHandle, State};

    pub(crate) use crate::config;
    pub(crate) use crate::engine::{Action, Engine};
    pub(crate) use crate::engine_host::{
        TaskCommandResult, broadcast, flush_notifications, lock, publish_config_change, run_command,
    };
    pub(crate) use crate::ipc_error::IpcError;
    pub(crate) use crate::logging;
    pub(crate) use crate::logs;
    pub(crate) use crate::shutdown::ExitReason;
    pub(crate) use crate::snapshot::Snapshot;
}
