//! 运行时段命令（每周 start/stop 时段的读写）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use super::prelude::*;

// ---------------------------------------------------------------- 运行时段（2026-11-04）

/// 读取运行时段的结果；字段与 `src/lib/api.ts` 的 ScheduleEditorView 一致。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ScheduleEditorView {
    pub task_id: String,
    pub schedule: config::ScheduleDraft,
    pub revision: String,
}

/// 保存运行时段的结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveScheduleResult {
    pub revision: String,
    pub changed: bool,
    pub snapshot: Snapshot,
}

/// 读取某个任务的每周运行时段（只读）。
#[tauri::command]
pub(crate) async fn get_schedule(
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
) -> Result<ScheduleEditorView, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, schedule) = {
        let guard = lock(&engine);
        let Some(loaded) = guard.loaded.as_ref() else {
            return Err(IpcError::new(
                "config",
                "配置文件尚未成功加载，无法读取运行时段",
            ));
        };
        let Some(task) = loaded.config.task(&task_id) else {
            return Err(IpcError::new(
                "invalid_task",
                format!("任务 {task_id} 不存在"),
            ));
        };
        (
            guard.config_file_path(),
            config::ScheduleDraft::from_config(&task.schedule),
        )
    };
    let joined = tauri::async_runtime::spawn_blocking(move || config::config_revision(&path)).await;
    let revision = joined
        .map_err(|error| IpcError::new("join", format!("读取配置 revision 线程异常结束：{error}")))?
        .map_err(IpcError::from)?;
    Ok(ScheduleEditorView {
        task_id,
        schedule,
        revision,
    })
}

/// 保存某个任务的每周运行时段（只改 `[tasks.schedule]`，保留注释与其他任务）。
#[tauri::command]
pub(crate) async fn save_schedule(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
    draft: config::ScheduleDraft,
    expected_revision: String,
) -> Result<SaveScheduleResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("保存运行时段", None)?;
    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome = config::save_schedule(
            &path,
            &task_id,
            &draft,
            &expected_revision,
            data_dir.as_deref(),
        )
        .map_err(IpcError::from)?;
        if !outcome.changed {
            let snapshot = lock(&engine).snapshot.clone();
            return Ok(SaveScheduleResult {
                revision: outcome.revision,
                changed: false,
                snapshot,
            });
        }
        let snapshot = publish_config_change(
            &worker_app,
            &engine,
            &outcome.revision,
            outcome.issues.len(),
            "schedule",
        );
        Ok(SaveScheduleResult {
            revision: outcome.revision,
            changed: true,
            snapshot,
        })
    })
    .await;
    joined.map_err(|error| IpcError::new("join", format!("保存运行时段线程异常结束：{error}")))?
}
