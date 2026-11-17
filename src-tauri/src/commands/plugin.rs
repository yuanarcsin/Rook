//! 插件命令（预览、导入、同步）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use super::prelude::*;

// ---------------------------------------------------------------- 插件清单（2026-11-04）

/// 插件预览 + 当前配置 revision。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginInspectView {
    #[serde(flatten)]
    pub preview: config::plugin::PluginPreview,
    pub config_revision: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginImportResult {
    pub task_id: String,
    pub snapshot: Snapshot,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct PluginSyncResult {
    pub task_id: String,
    pub changed: bool,
    pub snapshot: Snapshot,
}

/// 读取插件清单并预览；不写配置。
#[tauri::command]
pub(crate) async fn inspect_plugin(
    state: State<'_, Arc<Mutex<Engine>>>,
    manifest_path: String,
) -> Result<PluginInspectView, IpcError> {
    let engine = Arc::clone(state.inner());
    let (config_path, task_ids) = {
        let guard = lock(&engine);
        let Some(loaded) = guard.loaded.as_ref() else {
            return Err(IpcError::with_task(
                "config",
                "配置文件尚未成功加载，无法导入脚本",
                None,
            ));
        };
        (
            guard.config_file_path(),
            loaded
                .config
                .tasks
                .iter()
                .map(|task| task.id.clone())
                .collect::<Vec<_>>(),
        )
    };
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let preview = config::plugin::load_preview(Path::new(&manifest_path), &task_ids)?;
        let revision = config::config_revision(&config_path).map_err(|error| {
            config::plugin::PluginError::Read {
                message: error.to_string(),
            }
        })?;
        Ok::<_, config::plugin::PluginError>((preview, revision))
    })
    .await;
    let (preview, config_revision) = joined
        .map_err(|error| {
            IpcError::with_task("join", format!("读取插件清单线程异常结束：{error}"), None)
        })?
        .map_err(|error| IpcError::from_plugin(error, None))?;
    Ok(PluginInspectView {
        preview,
        config_revision,
    })
}

/// 把插件清单导入为普通 `[[tasks]]`。
#[tauri::command]
pub(crate) async fn import_plugin(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    manifest_path: String,
    expected_revision: String,
) -> Result<PluginImportResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("导入插件", None)?;
    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome = config::plugin::import_plugin(
            &path,
            Path::new(&manifest_path),
            &expected_revision,
            data_dir.as_deref(),
        )
        .map_err(|error| IpcError::from_plugin(error, None))?;
        // 导入动作本身不启动任务：reload 前登记一次性抑制；auto_start 只影响
        // 下一次应用启动（docs/15 §6.5）。
        {
            let mut guard = lock(&engine);
            guard
                .suppress_auto_start_once
                .insert(outcome.task_id.clone());
        }
        // `PluginWriteOutcome` 不返回本次写入的 issues；导入后的任务级问题
        // 由 reload 结果进入快照 `issues`，日志里按 0 记录（不伪造计数）。
        let snapshot =
            publish_config_change(&worker_app, &engine, &outcome.revision, 0, "plugin_import");
        Ok(PluginImportResult {
            task_id: outcome.task_id,
            snapshot,
        })
    })
    .await;
    joined.map_err(|error| {
        IpcError::with_task("join", format!("导入插件线程异常结束：{error}"), None)
    })?
}

/// 用清单同步已有插件任务。
#[tauri::command]
pub(crate) async fn sync_plugin(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
    expected_revision: String,
) -> Result<PluginSyncResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("同步插件", Some(task_id.clone()))?;
    let worker_app = app.clone();
    let worker_task = task_id.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome = config::plugin::sync_plugin(
            &path,
            &worker_task,
            &expected_revision,
            data_dir.as_deref(),
        )
        .map_err(|error| IpcError::from_plugin(error, Some(worker_task.clone())))?;
        if !outcome.changed {
            let snapshot = lock(&engine).snapshot.clone();
            return Ok(PluginSyncResult {
                task_id: outcome.task_id,
                changed: false,
                snapshot,
            });
        }
        // 同导入：`PluginWriteOutcome` 不含 issues，按 0 记录。
        let snapshot =
            publish_config_change(&worker_app, &engine, &outcome.revision, 0, "plugin_sync");
        Ok(PluginSyncResult {
            task_id: outcome.task_id,
            changed: true,
            snapshot,
        })
    })
    .await;
    joined.map_err(|error| {
        IpcError::with_task(
            "join",
            format!("同步插件线程异常结束：{error}"),
            Some(task_id),
        )
    })?
}
