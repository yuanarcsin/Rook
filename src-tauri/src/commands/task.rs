//! 任务命令（启停、结构化草稿、脚本面板）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use super::prelude::*;

/// 启动任务；阻塞部分在线程池执行，避免卡住 UI 线程。
#[tauri::command]
pub(crate) async fn start_task(app: AppHandle, id: String) -> Result<TaskCommandResult, IpcError> {
    run_command(app, id, Action::Start).await
}

/// 停止任务（最多等 `[tasks.stop].timeout_ms`）。
#[tauri::command]
pub(crate) async fn stop_task(app: AppHandle, id: String) -> Result<TaskCommandResult, IpcError> {
    run_command(app, id, Action::Stop).await
}

/// 停止后立即启动一个新实例。
#[tauri::command]
pub(crate) async fn restart_task(
    app: AppHandle,
    id: String,
) -> Result<TaskCommandResult, IpcError> {
    run_command(app, id, Action::Restart).await
}

// ---------------------------------------------------------------- 结构化任务草稿（M4-4a）

/// 保存/删除成功的结果：落盘后的 revision + 重新加载后的完整快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveTaskDraftResult {
    /// 落盘后文件内容的 revision（与文本编辑器同一套算法）。
    pub new_revision: String,
    /// `false` 表示草稿与磁盘内容一致，未写盘、也未触发重载。
    pub changed: bool,
    pub snapshot: Snapshot,
}

/// 新建任务表单的默认值（A6）。
///
/// 直接返回 [`crate::config::TaskDraft::default()`]，前端不再自己维护一份默认值；
/// 默认值与配置文件模板/校验规则一起演进，避免两处漂移。
#[tauri::command]
pub(crate) fn get_task_defaults() -> crate::config::TaskDraft {
    crate::config::TaskDraft::default()
}

/// 读取某个任务的结构化草稿（只读，无副作用）。
///
/// 值来自配置文件原文；缺省字段填文档默认值，把返回的草稿原样交给
/// [`save_task_draft`] 不会产生任何字节变化。
#[tauri::command]
pub(crate) async fn get_task_draft(
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
) -> Result<config::TaskDraft, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, _) = lock(&engine).config_write_target("编辑任务草稿", None)?;

    let joined =
        tauri::async_runtime::spawn_blocking(move || config::get_task_draft(&path, &task_id)).await;

    joined
        .map_err(|error| IpcError::new("join", format!("读取任务草稿线程异常结束：{error}")))?
        .map_err(IpcError::from)
}

/// 保存结构化任务草稿（`toml_edit` 只改差异字段，保留注释与其他任务）。
///
/// 冲突/结构错误/写入失败都不会改动原文件；成功后以落盘内容为准重新加载
/// Engine 并广播 `snapshot:changed`，返回新 revision 与快照。
#[tauri::command]
pub(crate) async fn save_task_draft(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    draft: config::TaskDraft,
    expected_revision: String,
) -> Result<SaveTaskDraftResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("保存任务草稿", None)?;

    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome =
            config::save_task_draft(&path, &draft, &expected_revision, data_dir.as_deref())
                .map_err(IpcError::from)?;
        finish_draft_write(&worker_app, &engine, outcome, "draft")
    })
    .await;

    joined.map_err(|error| IpcError::new("join", format!("保存任务草稿线程异常结束：{error}")))?
}

/// 删除某个任务（移除对应 `[[tasks]]` 表，保留其他任务与注释）。
#[tauri::command]
pub(crate) async fn delete_task(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
    expected_revision: String,
) -> Result<SaveTaskDraftResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("删除任务", None)?;

    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome = config::delete_task(&path, &task_id, &expected_revision, data_dir.as_deref())
            .map_err(IpcError::from)?;
        finish_draft_write(&worker_app, &engine, outcome, "delete")
    })
    .await;

    joined.map_err(|error| IpcError::new("join", format!("删除任务线程异常结束：{error}")))?
}

// ---------------------------------------------------------------- 任务顺序（C7）

/// 按 `ids` 的顺序重写所有任务的 `order`（只改这一个字段，保留注释）。
///
/// 要求 `ids` 与配置文件里现有任务集合完全一致；冲突/不一致都不写盘。
/// 成功后走统一的 [`publish_config_change`]，`config.applied` 的 `source` 为 `reorder`。
#[tauri::command]
pub(crate) async fn reorder_tasks(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    ids: Vec<String>,
    expected_revision: String,
) -> Result<SaveTaskDraftResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("调整任务顺序", None)?;

    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome = config::reorder_tasks(&path, &ids, &expected_revision, data_dir.as_deref())
            .map_err(reorder_error)?;
        finish_draft_write(&worker_app, &engine, outcome, "reorder")
    })
    .await;

    joined.map_err(|error| IpcError::new("join", format!("调整任务顺序线程异常结束：{error}")))?
}

/// [`config::ReorderError`] → 命令错误；`stage` 用 `conflict`/`invalid`/`parse`/`write`。
fn reorder_error(error: config::ReorderError) -> IpcError {
    use config::ReorderError;
    match error {
        ReorderError::Conflict { current_revision } => IpcError::conflict(
            "conflict",
            "文件已被外部修改，请重新加载后再调整顺序",
            current_revision,
        ),
        ReorderError::Invalid { message } => IpcError::new("invalid", message),
        ReorderError::Parse {
            message, line, col, ..
        } => IpcError::parse("parse", message, line, col),
        other => IpcError::new(other.stage(), other.to_string()),
    }
}

// ---------------------------------------------------------------- 脚本面板（2026-11-04）

/// 用系统默认浏览器打开任务的 `panel_url`。
///
/// 只接受配置里已经校验过的 http/https 地址；不经过 shell 拼接，避免命令注入。
#[tauri::command]
pub(crate) fn open_task_panel(
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
) -> Result<(), IpcError> {
    let url = {
        let guard = lock(state.inner());
        let Some(loaded) = guard.loaded.as_ref() else {
            return Err(IpcError::with_code(
                "config",
                None,
                "配置文件尚未成功加载，无法打开脚本面板",
                Some(task_id.clone()),
            ));
        };
        let Some(task) = loaded.config.task(&task_id) else {
            return Err(IpcError::with_code(
                "invalid_task",
                None,
                format!("任务 {task_id} 不存在"),
                Some(task_id.clone()),
            ));
        };
        task.panel_url.clone()
    };
    let Some(url) = url else {
        return Err(IpcError::with_code(
            "no_panel",
            None,
            format!("任务 {task_id} 没有配置 panel_url"),
            Some(task_id),
        ));
    };
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err(IpcError::with_code(
            "invalid_url",
            None,
            format!("panel_url 不是 http/https 地址：{url}"),
            Some(task_id),
        ));
    }
    std::process::Command::new("explorer")
        .arg(&url)
        .spawn()
        .map_err(|error| {
            IpcError::with_code(
                "spawn",
                None,
                format!("调用系统浏览器打开 {url} 失败：{error}"),
                Some(task_id.clone()),
            )
        })?;
    tracing::info!(
        event = "panel.open",
        task = %task_id,
        url = %url,
        "已请求系统浏览器打开脚本面板"
    );
    Ok(())
}

/// 草稿写入成功后的收尾：未变更只回快照，变更则走统一的 [`publish_config_change`]。
///
/// `source` 区分保存草稿（`draft`）与删除任务（`delete`），写入 `config.applied`。
fn finish_draft_write(
    app: &AppHandle,
    engine: &Arc<Mutex<Engine>>,
    outcome: config::SaveOutcome,
    source: &str,
) -> Result<SaveTaskDraftResult, IpcError> {
    if !outcome.changed {
        let snapshot = lock(engine).snapshot.clone();
        return Ok(SaveTaskDraftResult {
            new_revision: outcome.revision,
            changed: false,
            snapshot,
        });
    }

    let snapshot =
        publish_config_change(app, engine, &outcome.revision, outcome.issues.len(), source);
    Ok(SaveTaskDraftResult {
        new_revision: outcome.revision,
        changed: true,
        snapshot,
    })
}
