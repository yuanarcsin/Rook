//! Windows 会话结束（注销/关机）消息窗口（B3）。
//!
//! 在主线程创建一个 `HWND_MESSAGE` 消息专用窗口，只处理两个系统消息：
//!
//! - `WM_QUERYENDSESSION`：记录事件并返回 `TRUE`，**不阻止**系统关机/注销；
//! - `WM_ENDSESSION`（`wParam != 0`）：触发与应用退出相同的幂等停止流程
//!   （reason=`session_end`，预算 `min(shutdown_timeout_ms, 5000ms)`），
//!   不弹任何 UI；停止完成后 `app.exit(0)`。
//!
//! `WM_ENDSESSION` 的处理会立即返回，阻塞的停止流程在工作线程执行，避免卡住
//! 消息循环。安装失败只降级为 Job `KILL_ON_JOB_CLOSE` 兜底，不影响启动。
//!
//! # 手工/自动化验证
//!
//! 隐藏参数 `--simulate-session-end` 会在正常启动约 1.2s 后向本进程的会话窗口
//! 投递 `WM_QUERYENDSESSION` 与 `WM_ENDSESSION(wParam=1)`，用于验证停止预算、
//! `app.exit {reason:"session_end"}` 与 `clean_exit` 标记。它**不模拟真实系统关机**
//! （不会触发 Job 关闭/会话注销），也不应作为日常启动参数。

use std::ffi::c_void;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicIsize, Ordering};
use std::time::Duration;

use tauri::AppHandle;
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, HCURSOR, HICON, HWND_MESSAGE, PostMessageW, RegisterClassW,
    WINDOW_EX_STYLE, WINDOW_STYLE, WM_ENDSESSION, WM_QUERYENDSESSION, WNDCLASSW,
};
use windows::core::PCWSTR;

/// 会话窗口的类名/窗口名（仅本进程使用）。
const SESSION_CLASS: &str = "RookSessionEndWindow";

/// 保存 AppHandle 供 WndProc 触发退出流程。
static APP: OnceLock<AppHandle> = OnceLock::new();
/// 会话消息窗口句柄（0 = 尚未创建）；用于 `--simulate-session-end` 投递消息。
static SESSION_HWND: AtomicIsize = AtomicIsize::new(0);

/// 在主线程安装会话结束消息窗口。
///
/// 必须在 Tauri `setup`（主线程）中调用：窗口与消息队列绑定创建线程，后续
/// 由 Tauri 的事件循环分发消息。
pub fn install(app: &AppHandle) -> Result<(), String> {
    if SESSION_HWND.load(Ordering::SeqCst) != 0 {
        return Ok(());
    }
    // 单实例应用只会安装一次；失败时保留错误给调用方 warn。
    APP.set(app.clone())
        .map_err(|_| "会话结束窗口已安装（AppHandle 已存在）".to_owned())?;

    let class_name = wide(SESSION_CLASS);
    let window_name = wide("RookSessionEnd");
    unsafe {
        let module =
            GetModuleHandleW(None).map_err(|error| format!("GetModuleHandleW：{error}"))?;
        let instance = HINSTANCE::from(module);
        let class = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wnd_proc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: instance,
            hIcon: HICON::default(),
            hCursor: HCURSOR::default(),
            hbrBackground: HBRUSH::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: PCWSTR(class_name.as_ptr()),
        };
        let atom = RegisterClassW(&class);
        if atom == 0 {
            // 类已注册（例如重复调用/同一进程二次安装）时仍可继续创建窗口。
            let error = windows::core::Error::from_thread();
            tracing::warn!(error = %error, "RegisterClassW 返回 0，继续尝试创建会话消息窗口");
        }
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(class_name.as_ptr()),
            PCWSTR(window_name.as_ptr()),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance),
            None,
        )
        .map_err(|error| format!("CreateWindowExW：{error}"))?;
        SESSION_HWND.store(hwnd.0 as isize, Ordering::SeqCst);
    }
    tracing::info!(
        event = "app.session_window",
        "已创建会话结束消息窗口（WM_QUERYENDSESSION/WM_ENDSESSION）"
    );
    Ok(())
}

/// `--simulate-session-end`：延迟后向本进程会话窗口投递会话结束消息。
///
/// 延迟是为了让 `setup` 完成 Engine/托盘/窗口初始化；仅用于测试。
pub fn spawn_simulation(app: &AppHandle) {
    let app = app.clone();
    std::thread::Builder::new()
        .name("cs-session-sim".to_owned())
        .spawn(move || {
            std::thread::sleep(Duration::from_millis(1_200));
            match simulate() {
                Ok(()) => tracing::info!(
                    event = "app.session_end",
                    phase = "simulated",
                    "已向会话消息窗口投递 WM_QUERYENDSESSION/WM_ENDSESSION"
                ),
                Err(error) => tracing::warn!(error = %error, "模拟会话结束失败"),
            }
            drop(app);
        })
        .ok();
}

/// 向本进程会话窗口投递 `WM_QUERYENDSESSION` + `WM_ENDSESSION(wParam=1)`。
pub fn simulate() -> Result<(), String> {
    let raw = SESSION_HWND.load(Ordering::SeqCst);
    if raw == 0 {
        return Err("会话消息窗口尚未创建".to_owned());
    }
    let hwnd = HWND(raw as *mut c_void);
    unsafe {
        PostMessageW(Some(hwnd), WM_QUERYENDSESSION, WPARAM(0), LPARAM(0))
            .map_err(|error| format!("投递 WM_QUERYENDSESSION 失败：{error}"))?;
        // 给查询消息一个被分发的机会，再投递结束消息。
        std::thread::sleep(Duration::from_millis(50));
        PostMessageW(Some(hwnd), WM_ENDSESSION, WPARAM(1), LPARAM(0))
            .map_err(|error| format!("投递 WM_ENDSESSION 失败：{error}"))?;
    }
    Ok(())
}

/// 会话消息窗口过程。
///
/// `WM_QUERYENDSESSION` 返回 TRUE 表示不阻止系统注销/关机；`WM_ENDSESSION`
/// 只触发工作线程，绝不在这里执行阻塞停止。
unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_QUERYENDSESSION => {
            tracing::info!(
                event = "app.session_end",
                phase = "query",
                "收到会话结束查询，允许系统继续（不阻止注销/关机）"
            );
            LRESULT(1)
        }
        WM_ENDSESSION => {
            if wparam.0 != 0 {
                tracing::info!(
                    event = "app.session_end",
                    phase = "end",
                    "收到会话结束通知，开始停止任务"
                );
                trigger_shutdown();
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 在工作线程触发与托盘退出相同的幂等流程（reason=session_end，不弹 UI）。
fn trigger_shutdown() {
    let Some(app) = APP.get().cloned() else {
        return;
    };
    std::thread::Builder::new()
        .name("cs-session-end".to_owned())
        .spawn(move || {
            crate::shutdown::request_quit(&app, crate::shutdown::ExitReason::SessionEnd, false);
        })
        .ok();
}

/// UTF-16 + NUL；调用期间由调用方保持存活。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
