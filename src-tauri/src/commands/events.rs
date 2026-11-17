//! 事件日志与诊断包命令（JSONL 尾部读取、导出诊断 zip）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use super::prelude::*;

// ---------------------------------------------------------------- 事件日志/诊断（2026-11-04）

/// 事件日志尾部视图。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EventLogView {
    pub path: String,
    pub lines: Vec<String>,
    pub truncated: bool,
    pub total_bytes: u64,
}

/// 读取 JSONL 事件日志尾部。
#[tauri::command]
pub(crate) async fn read_event_log(
    state: State<'_, Arc<Mutex<Engine>>>,
    tail_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<EventLogView, IpcError> {
    let dir = {
        let guard = lock(state.inner());
        let Some(loaded) = guard.loaded.as_ref() else {
            return Err(IpcError::new(
                "config",
                "配置文件尚未成功加载，无法定位事件日志",
            ));
        };
        loaded.data_dir.join(logging::CORE_DIR)
    };
    let tail_lines = tail_lines.unwrap_or(200).clamp(1, 5000);
    let max_bytes = max_bytes.unwrap_or(256 * 1024).clamp(1024, 1024 * 1024);
    let joined = tauri::async_runtime::spawn_blocking(move || {
        logging::read_event_log_tail(&dir, tail_lines, max_bytes)
    })
    .await;
    let tail = joined
        .map_err(|error| IpcError::new("join", format!("读取事件日志线程异常结束：{error}")))?
        .map_err(|error| IpcError::new("read", format!("读取事件日志失败：{error}")))?;
    Ok(EventLogView {
        path: tail.path.to_string_lossy().into_owned(),
        lines: tail.lines,
        truncated: tail.truncated,
        total_bytes: tail.total_bytes,
    })
}

/// 导出诊断包到 `<data>/diagnostics/diag-<时间戳>/`。
#[tauri::command]
pub(crate) async fn export_diagnostics(
    state: State<'_, Arc<Mutex<Engine>>>,
) -> Result<crate::diagnostics::DiagnosticsResult, IpcError> {
    let (data_dir, config_path) = {
        let guard = lock(state.inner());
        let Some(loaded) = guard.loaded.as_ref() else {
            return Err(IpcError::new(
                "config",
                "配置文件尚未成功加载，无法导出诊断包",
            ));
        };
        (loaded.data_dir.clone(), loaded.config_path.clone())
    };
    let joined = tauri::async_runtime::spawn_blocking(move || {
        crate::diagnostics::export(&data_dir, &config_path)
    })
    .await;
    joined
        .map_err(|error| IpcError::new("join", format!("导出诊断包线程异常结束：{error}")))?
        .map_err(|message| IpcError::new("export", message))
}
