use tauri::{AppHandle, Manager, WebviewWindowBuilder};

/// 在独立线程中显示主窗口，避免在托盘事件或同步命令里直接构建 WebView2 造成死锁。
pub fn spawn_show(app: &AppHandle) {
    tracing::info!("请求显示主窗口");
    let app = app.clone();
    std::thread::spawn(move || show(&app));
}

fn show(app: &AppHandle) {
    if let Some(window) = app.get_webview_window("main") {
        tracing::info!("主窗口已存在：显示、取消最小化并聚焦");
        let _ = window.show();
        let _ = window.unminimize();
        let _ = window.set_focus();
        return;
    }

    let Some(config) = app
        .config()
        .app
        .windows
        .iter()
        .find(|window| window.label == "main")
        .cloned()
    else {
        tracing::error!("no window config with label \"main\"");
        return;
    };

    tracing::info!("创建主窗口（WebView2 按需创建）");
    match WebviewWindowBuilder::from_config(app, &config).and_then(|builder| builder.build()) {
        Ok(window) => {
            // 窗口无系统边框（标题栏由前端自绘）：Win11 下补回圆角与细边框，
            // 旧系统调用失败只降级为方角，不影响窗口可用性。
            #[cfg(windows)]
            apply_frameless_frame(&window);

            // 关闭窗口 = 销毁 WebView2，应用继续驻留托盘；托盘左键/第二实例会重建。
            // 记录一次性提示标记，下次打开窗口时展示“仍在托盘运行”。
            let app_for_event = app.clone();
            window.on_window_event(move |event| {
                if let tauri::WindowEvent::CloseRequested { .. } = event {
                    let app = app_for_event.clone();
                    std::thread::spawn(move || crate::engine_host::note_window_closed(&app));
                }
            });
            let _ = window.show();
            let _ = window.unminimize();
            let _ = window.set_focus();
        }
        Err(err) => tracing::error!("failed to build main window: {err}"),
    }
}

/// 无边框窗口的 DWM 细节：圆角 + 与界面同色系的 1px 边框。
///
/// `DwmSetWindowAttribute` 在 Windows 10 上对这两个属性返回错误，
/// 这里刻意忽略：方角仍可正常使用。
#[cfg(windows)]
fn apply_frameless_frame(window: &tauri::WebviewWindow) {
    use windows::Win32::Graphics::Dwm::{
        DWM_WINDOW_CORNER_PREFERENCE, DWMWA_BORDER_COLOR, DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_ROUND, DwmSetWindowAttribute,
    };

    let Ok(hwnd) = window.hwnd() else {
        return;
    };
    // COLORREF = 0x00BBGGRR；浅色 #e4e4e8 / 深色 #2e2e33。
    let border: u32 = match window.theme() {
        Ok(tauri::Theme::Dark) => 0x0033_2E2E,
        _ => 0x00E8_E4E4,
    };
    let corner = DWMWCP_ROUND;
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const DWM_WINDOW_CORNER_PREFERENCE as *const core::ffi::c_void,
            std::mem::size_of::<DWM_WINDOW_CORNER_PREFERENCE>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const u32 as *const core::ffi::c_void,
            std::mem::size_of::<u32>() as u32,
        );
    }
}
