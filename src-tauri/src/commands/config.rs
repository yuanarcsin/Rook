//! 配置命令（文本读写、备份恢复、重载）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use super::prelude::*;

// ---------------------------------------------------------------- 配置文本编辑器（M4-2）

/// 配置文本读取结果；字段与 `src/lib/api.ts` 的 `ConfigTextView` 一一对应。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ConfigTextView {
    /// 实际读取的配置文件路径（来自启动时的路径解析，不接受前端指定）。
    pub path: String,
    pub text: String,
    /// 内容 revision（SHA-256 前 16 字节 hex），保存时必须原样带回。
    pub revision: String,
}

/// 保存成功的结果：落盘后的 revision + 重新加载后的完整快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct SaveConfigTextResult {
    pub revision: String,
    /// `false` 表示内容与磁盘相同，未写盘、也未触发重载。
    pub changed: bool,
    pub snapshot: Snapshot,
}

/// 读取当前配置文件的文本与 revision。
///
/// 只读、无副作用；文件读取在阻塞线程池执行，Engine 锁只用于取路径。
#[tauri::command]
pub(crate) async fn read_config_text(
    state: State<'_, Arc<Mutex<Engine>>>,
) -> Result<ConfigTextView, IpcError> {
    let engine = Arc::clone(state.inner());
    let path = lock(&engine).config_file_path();
    if path.as_os_str().is_empty() {
        return Err(IpcError::new(
            "config",
            "配置文件路径未知，无法读取配置文本",
        ));
    }

    let read_path = path.clone();
    let joined =
        tauri::async_runtime::spawn_blocking(move || config::read_config_text(&read_path)).await;
    let view = joined
        .map_err(|error| IpcError::new("join", format!("读取配置线程异常结束：{error}")))?
        .map_err(IpcError::from)?;

    Ok(ConfigTextView {
        path: view.path.to_string_lossy().into_owned(),
        text: view.text,
        revision: view.revision,
    })
}

/// 保存配置文本（revision 冲突检测 + 原子替换 + 重新加载）。
///
/// 冲突/结构错误/写入失败都不会改动原文件；成功后以落盘内容为准重新加载
/// Engine 并广播 `snapshot:changed`，返回新 revision 与快照。
#[tauri::command]
pub(crate) async fn save_config_text(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
    text: String,
    expected_revision: String,
) -> Result<SaveConfigTextResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("保存配置文本", None)?;

    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome =
            config::save_config_text(&path, &text, &expected_revision, data_dir.as_deref())
                .map_err(IpcError::from)?;

        if !outcome.changed {
            // 内容未变：不重载、不广播，只把当前快照回给前端。
            let snapshot = lock(&engine).snapshot.clone();
            return Ok(SaveConfigTextResult {
                revision: outcome.revision,
                changed: false,
                snapshot,
            });
        }

        // 不信任内存草稿：以落盘内容为准重新加载，成功后才算"已保存"。
        let snapshot = publish_config_change(
            &worker_app,
            &engine,
            &outcome.revision,
            outcome.issues.len(),
            "text",
        );
        Ok(SaveConfigTextResult {
            revision: outcome.revision,
            changed: true,
            snapshot,
        })
    })
    .await;

    joined.map_err(|error| IpcError::new("join", format!("保存配置线程异常结束：{error}")))?
}

/// 用 `<配置文件>.bak` 覆盖当前配置（C2：一键回滚到上一版）。
///
/// 备份内容走与保存同一套校验，不合法则主文件不变；成功后把当前（可能已损坏的）
/// 文件另存为 `.bak.bad`，再以落盘内容为准重新加载并广播 `snapshot:changed`。
/// 这里不经过 `backup_previous`，避免坏文件覆盖唯一一份好备份。
#[tauri::command]
pub(crate) async fn restore_config_backup(
    app: AppHandle,
    state: State<'_, Arc<Mutex<Engine>>>,
) -> Result<SaveConfigTextResult, IpcError> {
    let engine = Arc::clone(state.inner());
    let (path, data_dir) = lock(&engine).config_write_target("恢复配置备份", None)?;

    let worker_app = app.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        let outcome =
            config::editor::restore_backup(&path, data_dir.as_deref()).map_err(IpcError::from)?;
        let snapshot = publish_config_change(
            &worker_app,
            &engine,
            &outcome.revision,
            outcome.issues.len(),
            "backup",
        );
        Ok(SaveConfigTextResult {
            revision: outcome.revision,
            changed: true,
            snapshot,
        })
    })
    .await;

    joined.map_err(|error| IpcError::new("join", format!("恢复配置备份线程异常结束：{error}")))?
}
