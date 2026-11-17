//! 系统命令（快照、退出、托盘提示、自启/通知、路径打开）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use std::time::Duration;

use super::prelude::*;

/// 返回当前快照；不重新读取配置文件。
#[tauri::command]
pub(crate) fn get_snapshot(state: State<'_, Arc<Mutex<Engine>>>) -> Snapshot {
    lock(state.inner()).snapshot.clone()
}

/// 重新加载配置，更新状态并广播 `snapshot:changed`。
#[tauri::command]
pub(crate) fn reload_config(app: AppHandle, state: State<'_, Arc<Mutex<Engine>>>) -> Snapshot {
    let engine = Arc::clone(state.inner());
    let snapshot = lock(&engine).reload();
    broadcast(&app, &snapshot);
    flush_notifications(&app, &engine);
    snapshot
}

/// 窗口内退出（B1）：前端已用 ConfirmDialog 确认，这里不再弹 MessageBox。
///
/// 后端在独立线程停止全部任务、保存 state、刷新日志，然后 `app.exit(0)`；
/// 命令立即返回，前端可以显示“正在退出…”。
#[tauri::command]
pub(crate) async fn quit_app(app: AppHandle) {
    crate::shutdown::request_quit(&app, ExitReason::User, false);
}

/// 前端已展示“仍在托盘运行”提示：清除一次性标记。
#[tauri::command]
pub(crate) fn acknowledge_tray_hint(state: State<'_, Arc<Mutex<Engine>>>) -> Snapshot {
    let mut guard = lock(state.inner());
    guard.acknowledge_tray_hint();
    guard.snapshot.clone()
}

/// 只读查询开机自启状态（M4-3）：比较配置期望与 HKCU Run 实际值。
///
/// 绝不写注册表；取 exe 或读取注册表失败时只填 `error`，命令本身不 panic、
/// 也不拒绝，前端始终能拿到一个可渲染的状态。
#[tauri::command]
pub(crate) fn get_autostart_status(
    state: State<'_, Arc<Mutex<Engine>>>,
) -> crate::autostart::AutostartStatus {
    let config = lock(state.inner())
        .loaded
        .as_ref()
        .map(|loaded| loaded.config.autostart.clone());
    let Some(config) = config else {
        return crate::autostart::AutostartStatus::unavailable(
            "配置文件尚未成功加载，无法读取开机自启设置",
        );
    };

    let exe = std::env::current_exe().map_err(|error| format!("取当前 exe 路径失败：{error}"));

    #[cfg(windows)]
    let actual: Result<Option<String>, String> = {
        use crate::autostart::RunKeyStore;
        crate::autostart::WindowsRunKey::new()
            .read()
            .map_err(|error| format!("读取 HKCU Run 键失败：{error}"))
    };
    #[cfg(not(windows))]
    let actual: Result<Option<String>, String> = Ok(None);

    let exe_ref = exe
        .as_ref()
        .map(PathBuf::as_path)
        .map_err(|error| error.clone());
    crate::autostart::status(&config, exe_ref, actual)
}

/// 系统通知可用性（只读；不创建/修改开始菜单快捷方式、不写注册表）。
#[tauri::command]
pub(crate) fn get_toast_status() -> crate::toast::ToastStatus {
    crate::toast::status()
}

/// 发送一条测试系统通知；返回发送后的状态（不可用时 `reason` 说明原因）。
#[tauri::command]
pub(crate) async fn test_toast() -> crate::toast::ToastStatus {
    tauri::async_runtime::spawn_blocking(|| crate::toast::test_toast(Duration::from_secs(5)))
        .await
        .unwrap_or_else(|_| crate::toast::status())
}

/// 用资源管理器打开应用自身的目录（C4）：只接受枚举，不接受任意路径。
///
/// 目录不存在直接返回 `not_found`（不创建、不打开文件），路径全部来自启动时的
/// 路径解析，前端无法指定任意位置。
#[tauri::command]
pub(crate) fn open_app_path(
    state: State<'_, Arc<Mutex<Engine>>>,
    kind: String,
) -> Result<(), IpcError> {
    let dir = {
        let guard = lock(state.inner());
        let Some(loaded) = guard.loaded.as_ref() else {
            return Err(IpcError::with_code(
                "config",
                None,
                "配置文件尚未成功加载，无法打开目录",
                None,
            ));
        };
        match kind.as_str() {
            "logs" => loaded.data_dir.join("logs"),
            "data" => loaded.data_dir.clone(),
            "config_dir" => match loaded.config_path.parent() {
                Some(parent) if !parent.as_os_str().is_empty() => parent.to_path_buf(),
                _ => {
                    return Err(IpcError::with_code(
                        "invalid_argument",
                        None,
                        format!("配置文件路径没有所在目录：{}", loaded.config_path.display()),
                        None,
                    ));
                }
            },
            other => {
                return Err(IpcError::with_code(
                    "invalid_argument",
                    None,
                    format!("未知目录类型：{other}"),
                    None,
                ));
            }
        }
    };

    if !dir.is_dir() {
        return Err(IpcError::with_code(
            "not_found",
            None,
            format!("目录不存在：{}", dir.display()),
            None,
        ));
    }

    std::process::Command::new("explorer")
        .arg(&dir)
        .spawn()
        .map_err(|error| {
            IpcError::with_code(
                "spawn",
                None,
                format!("调用资源管理器打开 {} 失败：{error}", dir.display()),
                None,
            )
        })?;
    tracing::info!(
        event = "app_path.open",
        kind = %kind,
        path = %dir.display(),
        "已请求资源管理器打开目录"
    );
    Ok(())
}
