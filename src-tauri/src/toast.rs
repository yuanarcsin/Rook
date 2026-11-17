//! Windows 系统 Toast 通知（2026-11-05，M5-6 的 Toast/AUMID 部分）。
//!
//! # 身份来源（不创建、不修改任何快捷方式）
//!
//! Windows 要求 Toast 的 AUMID 与开始菜单快捷方式对应。本应用使用**用户已提供**的
//! 开始菜单快捷方式：快捷方式没有显式 `System.AppUserModel.ID` 时，系统把它的
//! **target 路径**当作隐式 AUMID。因此这里直接使用 `current_exe()` 的路径字符串：
//!
//! - 只读取 `%APPDATA%\Microsoft\Windows\Start Menu\Programs\<exe_stem>.lnk`，
//!   并以 `%ProgramData%\...` 同名快捷方式兜底；
//! - 快捷方式缺失、名称不匹配或 AUMID 超过 128 字符时状态为不可用，Engine 继续走
//!   应用内 banner + 托盘摘要；**不创建/修改/删除快捷方式，也不写 HKCU
//!   `AppUserModelId` 注册表**。
//!
//! # 线程模型
//!
//! WinRT 对象只在一个专用线程里创建和使用（`RoInitialize(RO_INIT_MULTITHREADED)`），
//! Engine/UI 只通过 mpsc 投递请求，不等待、不阻塞。线程启动失败或系统通知被禁用时
//! 只更新状态并退出，不重试；后续 `notify` 直接返回 `false`。
//!
//! # 不可检测的系统状态
//!
//! Windows 的 Focus Assist（专注助手）没有可靠的公开检测 API；它开启时 Toast 可能
//! 只进通知中心。这里只检查 `ToastNotifier::Setting()` 的
//! `DisabledForApplication/User/GroupPolicy`，其余情况按"已发送"处理并在日志里保留
//! 真实结果，不伪造成功。

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use serde::Serialize;

/// WinRT 允许的 AUMID 最大长度（字符）。
pub const MAX_AUMID_LEN: usize = 128;

/// Toast 标题最大字符数（超出截断，避免系统侧溢出）。
pub const TITLE_MAX_CHARS: usize = 64;

/// Toast 正文最大字符数。
pub const BODY_MAX_CHARS: usize = 256;

/// 通知分组：同组 + 同 tag 的 Toast 会替换而不是堆叠。
pub const TOAST_GROUP: &str = crate::brand::DISPLAY_NAME;

/// 发送失败日志的最小间隔，避免异常时刷屏。
const ERROR_LOG_INTERVAL: Duration = Duration::from_secs(60);

/// 系统通知状态（Tauri `get_toast_status` / `test_toast` 的返回体）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastStatus {
    /// 当前是否具备发送系统通知的条件。
    pub available: bool,
    /// 不可用原因（面向用户的一行说明）；可用时为 `None`。
    pub reason: Option<String>,
    /// 用作 AUMID 的当前 exe 路径。
    pub aumid: Option<String>,
    /// 实际找到的开始菜单快捷方式路径；未找到时为 `None`。
    pub shortcut: Option<String>,
    /// 最近一次发送失败的错误；没有失败时为 `None`。
    pub last_error: Option<String>,
}

impl ToastStatus {
    /// 构造一个"不可用"状态。
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self {
            available: false,
            reason: Some(reason.into()),
            aumid: None,
            shortcut: None,
            last_error: None,
        }
    }
}

/// 一条待发送的 Toast；`reply` 只有 `--test-toast`/测试按钮使用。
struct Job {
    title: String,
    body: String,
    tag: String,
    reply: Option<Sender<Result<(), String>>>,
}

/// 全局运行时：状态 + 通知线程入口。只初始化一次。
struct Runtime {
    status: Arc<Mutex<ToastStatus>>,
    tx: Option<Sender<Job>>,
}

static RUNTIME: OnceLock<Runtime> = OnceLock::new();

/// 初始化进程通知身份（必须在创建 Tauri Builder/窗口之前调用）。
///
/// 幂等：重复调用只返回当前状态。快捷方式缺失/路径过长时只标记不可用并记录原因。
pub fn init() -> ToastStatus {
    runtime().status_snapshot()
}

/// 当前系统通知状态（只读；不创建/修改快捷方式）。
pub fn status() -> ToastStatus {
    runtime().status_snapshot()
}

/// 是否具备发送系统通知的条件（不含 Focus Assist 等不可检测状态）。
pub fn is_available() -> bool {
    runtime().status_snapshot().available
}

/// 异步投递一条系统通知；不可用或线程已退出时返回 `false`。
///
/// 不阻塞调用方，也不等待 WinRT 结果；真实错误由通知线程按限频写日志。
pub fn notify(title: &str, body: &str, kind: &str) -> bool {
    let runtime = runtime();
    let Some(tx) = runtime.tx.as_ref() else {
        return false;
    };
    tx.send(Job {
        title: title.to_owned(),
        body: body.to_owned(),
        tag: sanitize_tag(kind),
        reply: None,
    })
    .is_ok()
}

/// 发送一条测试通知并等待结果（最多 `timeout`）。
///
/// 用于 `--test-toast` 与设置页按钮；不创建/聚焦任何窗口。
pub fn test_toast(timeout: Duration) -> ToastStatus {
    let runtime = runtime();
    let Some(tx) = runtime.tx.as_ref() else {
        return runtime.status_snapshot();
    };

    let (reply_tx, reply_rx) = mpsc::channel();
    let job = Job {
        title: format!("{} 测试通知", crate::brand::DISPLAY_NAME),
        body: "系统通知已启用；任务异常时会在这里提醒你。".to_owned(),
        tag: "test".to_owned(),
        reply: Some(reply_tx),
    };
    if tx.send(job).is_err() {
        runtime.update(|status| {
            status.last_error = Some("通知线程已退出，无法发送测试通知".to_owned());
        });
        return runtime.status_snapshot();
    }

    match reply_rx.recv_timeout(timeout) {
        Ok(Ok(())) => runtime.update(|status| status.last_error = None),
        Ok(Err(error)) => runtime.update(|status| status.last_error = Some(error)),
        Err(_) => runtime.update(|status| {
            status.last_error = Some("等待系统通知发送结果超时".to_owned());
        }),
    }
    runtime.status_snapshot()
}

impl Runtime {
    fn status_snapshot(&self) -> ToastStatus {
        match self.status.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn update(&self, apply: impl FnOnce(&mut ToastStatus)) {
        match self.status.lock() {
            Ok(mut guard) => apply(&mut guard),
            Err(poisoned) => apply(&mut poisoned.into_inner()),
        }
    }
}

fn runtime() -> &'static Runtime {
    RUNTIME.get_or_init(Runtime::initialize)
}

// ---------------------------------------------------------------- 纯逻辑（可单测）

/// 当前 exe 路径作为 AUMID；超过 [`MAX_AUMID_LEN`] 字符时返回 `None`。
pub fn aumid_for_exe(exe: &Path) -> Option<String> {
    let value = exe.to_string_lossy().into_owned();
    (value.chars().count() <= MAX_AUMID_LEN).then_some(value)
}

/// `Start Menu/Programs` 目录。
pub fn programs_dir(base: &Path) -> PathBuf {
    base.join("Microsoft")
        .join("Windows")
        .join("Start Menu")
        .join("Programs")
}

/// 快捷方式候选路径：先用户 `%APPDATA%`，再 `%ProgramData%`。
pub fn shortcut_candidates(
    exe: &Path,
    appdata: Option<&Path>,
    programdata: Option<&Path>,
) -> Vec<PathBuf> {
    let Some(stem) = exe.file_stem() else {
        return Vec::new();
    };
    let name = format!("{}.lnk", stem.to_string_lossy());
    [appdata, programdata]
        .into_iter()
        .flatten()
        .map(|base| programs_dir(base).join(&name))
        .collect()
}

/// 按"存在同名快捷方式"判定可用性；只读取，不创建/修改。
pub fn resolve_status(
    exe: &Path,
    appdata: Option<&Path>,
    programdata: Option<&Path>,
) -> ToastStatus {
    let Some(aumid) = aumid_for_exe(exe) else {
        let length = exe.to_string_lossy().chars().count();
        return ToastStatus {
            available: false,
            reason: Some(format!(
                "当前 exe 路径 {length} 字符，超过 {MAX_AUMID_LEN} 字符上限，无法作为系统通知标识"
            )),
            aumid: None,
            shortcut: None,
            last_error: None,
        };
    };

    let candidates = shortcut_candidates(exe, appdata, programdata);
    let found = candidates.iter().find(|path| path.is_file());
    match found {
        Some(path) => ToastStatus {
            available: true,
            reason: None,
            aumid: Some(aumid),
            shortcut: Some(path.display().to_string()),
            last_error: None,
        },
        None => {
            let expected = candidates.first().map_or_else(
                || "<无法确定开始菜单目录>".to_owned(),
                |path| path.display().to_string(),
            );
            ToastStatus {
                available: false,
                reason: Some(format!("未找到开始菜单快捷方式：{expected}")),
                aumid: Some(aumid),
                shortcut: None,
                last_error: None,
            }
        }
    }
}

/// XML 文本转义（Toast 标题/正文来自任务名与错误信息，必须转义）。
pub fn xml_escape(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 按字符截断，超出时以 `…` 结尾（不切断多字节字符）。
pub fn truncate_chars(input: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if input.chars().count() <= max {
        return input.to_owned();
    }
    let mut out: String = input.chars().take(max - 1).collect();
    out.push('…');
    out
}

/// 生成 ToastGeneric XML；标题/正文先截断再转义。
pub fn toast_xml(title: &str, body: &str) -> String {
    format!(
        "<toast><visual><binding template=\"ToastGeneric\"><text>{}</text><text>{}</text></binding></visual></toast>",
        xml_escape(&truncate_chars(title, TITLE_MAX_CHARS)),
        xml_escape(&truncate_chars(body, BODY_MAX_CHARS)),
    )
}

/// Toast tag 只保留 ASCII 字母数字、`_`、`-`，最长 32 字符；空值回退 `notice`。
pub fn sanitize_tag(raw: &str) -> String {
    let mut tag: String = raw
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric() || *ch == '_' || *ch == '-')
        .take(32)
        .collect();
    if tag.is_empty() {
        tag.push_str("notice");
    }
    tag
}

// ---------------------------------------------------------------- Windows 实现

#[cfg(windows)]
impl Runtime {
    fn initialize() -> Self {
        let status = match std::env::current_exe() {
            Ok(exe) => resolve_status(
                &exe,
                env_dir("APPDATA").as_deref(),
                env_dir("PROGRAMDATA").as_deref(),
            ),
            Err(error) => ToastStatus::unavailable(format!("取当前 exe 路径失败：{error}")),
        };
        let status = Arc::new(Mutex::new(status));

        let snapshot = match status.lock() {
            Ok(guard) => guard.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        };
        // AUMID 有效就设置进程身份（即使快捷方式缺失，也不影响后续修复快捷方式）；
        // 快捷方式缺失只决定是否启动通知线程。
        let Some(aumid) = snapshot.aumid.clone() else {
            return Self { status, tx: None };
        };
        if let Err(error) = set_process_aumid(&aumid) {
            mark_unavailable(&status, format!("设置进程通知标识失败：{error}"));
            return Self { status, tx: None };
        }
        if !snapshot.available {
            return Self { status, tx: None };
        }

        let (tx, rx) = mpsc::channel();
        let worker_status = Arc::clone(&status);
        let worker_aumid = aumid.clone();
        match std::thread::Builder::new()
            .name("campus-toast".to_owned())
            .spawn(move || worker_loop(rx, worker_status, worker_aumid))
        {
            Ok(_) => Self {
                status,
                tx: Some(tx),
            },
            Err(error) => {
                mark_unavailable(&status, format!("创建通知线程失败：{error}"));
                Self { status, tx: None }
            }
        }
    }
}

#[cfg(windows)]
fn env_dir(name: &str) -> Option<PathBuf> {
    std::env::var_os(name).map(PathBuf::from)
}

#[cfg(windows)]
fn mark_unavailable(status: &Mutex<ToastStatus>, reason: String) {
    let apply = |status: &mut ToastStatus| {
        status.available = false;
        status.reason = Some(reason.clone());
    };
    match status.lock() {
        Ok(mut guard) => apply(&mut guard),
        Err(poisoned) => apply(&mut poisoned.into_inner()),
    }
}

#[cfg(windows)]
fn set_process_aumid(aumid: &str) -> Result<(), String> {
    use windows::Win32::UI::Shell::SetCurrentProcessExplicitAppUserModelID;
    use windows::core::PCWSTR;

    let wide: Vec<u16> = aumid.encode_utf16().chain(std::iter::once(0)).collect();
    // SAFETY: `wide` 以 NUL 结尾且在调用期间保持存活。
    unsafe { SetCurrentProcessExplicitAppUserModelID(PCWSTR(wide.as_ptr())) }
        .map_err(|error| error.to_string())
}

#[cfg(windows)]
fn worker_loop(rx: Receiver<Job>, status: Arc<Mutex<ToastStatus>>, aumid: String) {
    use windows::UI::Notifications::{NotificationSetting, ToastNotificationManager};
    use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
    use windows::core::HSTRING;

    // SAFETY: 每个线程只初始化一次 COM；失败只记录并退出，不 panic。
    if let Err(error) = unsafe { RoInitialize(RO_INIT_MULTITHREADED) } {
        tracing::warn!(event = "toast_error", error = %error, "初始化 WinRT 失败");
        mark_unavailable(&status, format!("初始化 WinRT 失败：{error}"));
        return;
    }

    let notifier = match ToastNotificationManager::CreateToastNotifierWithId(&HSTRING::from(aumid))
    {
        Ok(notifier) => notifier,
        Err(error) => {
            tracing::warn!(event = "toast_error", error = %error, "创建 ToastNotifier 失败");
            mark_unavailable(&status, format!("创建系统通知器失败：{error}"));
            return;
        }
    };

    match notifier.Setting() {
        Ok(setting) if setting == NotificationSetting::Enabled => {}
        Ok(setting) => {
            let reason = setting_reason(setting);
            tracing::warn!(
                event = "toast_disabled",
                setting = setting.0,
                reason,
                "系统通知被禁用，停止发送（不重试）"
            );
            mark_unavailable(&status, reason.to_owned());
            return;
        }
        // 部分桌面/隐式 AUMID 环境读取 Setting 会返回 0x80070490（找不到元素），
        // 但这不代表通知不可用；只告警，继续尝试 Show。
        Err(error) => {
            tracing::warn!(
                event = "toast_error",
                error = %error,
                "读取系统通知设置失败，继续尝试发送"
            );
        }
    }

    let mut last_warn: Option<Instant> = None;
    while let Ok(job) = rx.recv() {
        match show(&notifier, &job) {
            Ok(()) => {
                if let Some(reply) = job.reply {
                    let _ = reply.send(Ok(()));
                }
            }
            Err(error) => {
                let should_log = last_warn.is_none_or(|at| at.elapsed() >= ERROR_LOG_INTERVAL);
                if should_log {
                    tracing::warn!(
                        event = "toast_error",
                        tag = %job.tag,
                        error = %error,
                        "发送系统通知失败"
                    );
                    last_warn = Some(Instant::now());
                }
                if let Some(reply) = job.reply {
                    let _ = reply.send(Err(error));
                }
            }
        }
    }
}

#[cfg(windows)]
fn setting_reason(setting: windows::UI::Notifications::NotificationSetting) -> &'static str {
    use windows::UI::Notifications::NotificationSetting;

    match setting {
        NotificationSetting::DisabledForApplication => "系统通知已被禁用（DisabledForApplication）",
        NotificationSetting::DisabledForUser => "系统通知已被用户关闭（DisabledForUser）",
        NotificationSetting::DisabledByGroupPolicy => {
            "系统通知被组策略禁用（DisabledByGroupPolicy）"
        }
        _ => "系统通知不可用（未知设置）",
    }
}

#[cfg(windows)]
fn show(notifier: &windows::UI::Notifications::ToastNotifier, job: &Job) -> Result<(), String> {
    use windows::Data::Xml::Dom::XmlDocument;
    use windows::UI::Notifications::ToastNotification;
    use windows::core::HSTRING;

    let xml = toast_xml(&job.title, &job.body);
    let document = XmlDocument::new().map_err(|error| format!("创建 XML 文档失败：{error}"))?;
    document
        .LoadXml(&HSTRING::from(xml))
        .map_err(|error| format!("装载通知 XML 失败：{error}"))?;
    let toast = ToastNotification::CreateToastNotification(&document)
        .map_err(|error| format!("创建 Toast 失败：{error}"))?;
    toast
        .SetTag(&HSTRING::from(job.tag.as_str()))
        .map_err(|error| format!("设置 Toast tag 失败：{error}"))?;
    toast
        .SetGroup(&HSTRING::from(TOAST_GROUP))
        .map_err(|error| format!("设置 Toast group 失败：{error}"))?;
    notifier
        .Show(&toast)
        .map_err(|error| format!("显示 Toast 失败：{error}"))
}

// ---------------------------------------------------------------- 非 Windows 兜底

#[cfg(not(windows))]
impl Runtime {
    fn initialize() -> Self {
        Self {
            status: Arc::new(Mutex::new(ToastStatus::unavailable(
                "系统通知仅支持 Windows",
            ))),
            tx: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exe() -> PathBuf {
        PathBuf::from("D:/apps/Rook/Rook.exe")
    }

    #[test]
    fn aumid_is_current_exe_path() {
        assert_eq!(
            aumid_for_exe(&exe()).as_deref(),
            Some("D:/apps/Rook/Rook.exe")
        );
    }

    #[test]
    fn aumid_rejects_path_over_128_chars() {
        let long = PathBuf::from("D:/".to_owned() + &"a".repeat(126) + ".exe");
        assert!(long.to_string_lossy().chars().count() > MAX_AUMID_LEN);
        assert_eq!(aumid_for_exe(&long), None);
    }

    #[test]
    fn shortcut_candidates_prefer_appdata_then_programdata() {
        let candidates = shortcut_candidates(
            &exe(),
            Some(Path::new("C:/Users/u/AppData/Roaming")),
            Some(Path::new("C:/ProgramData")),
        );
        assert_eq!(candidates.len(), 2);
        assert!(candidates[0].ends_with("Rook.lnk"));
        assert!(candidates[0].starts_with("C:/Users/u/AppData/Roaming"));
        assert!(candidates[1].starts_with("C:/ProgramData"));
    }

    #[test]
    fn status_available_when_shortcut_exists() {
        let temp = tempfile::tempdir().expect("tempdir");
        let programs = programs_dir(temp.path());
        std::fs::create_dir_all(&programs).expect("create programs");
        let shortcut = programs.join("Rook.lnk");
        std::fs::write(&shortcut, b"").expect("create shortcut");

        let status = resolve_status(&exe(), Some(temp.path()), None);
        assert!(status.available);
        assert_eq!(status.reason, None);
        assert_eq!(status.shortcut, Some(shortcut.display().to_string()));
        assert_eq!(status.aumid.as_deref(), Some("D:/apps/Rook/Rook.exe"));
    }

    #[test]
    fn status_unavailable_without_shortcut() {
        let temp = tempfile::tempdir().expect("tempdir");
        let status = resolve_status(&exe(), Some(temp.path()), None);
        assert!(!status.available);
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("快捷方式")),
            "{:?}",
            status.reason
        );
        assert_eq!(status.shortcut, None);
    }

    #[test]
    fn status_falls_back_to_programdata() {
        let user = tempfile::tempdir().expect("user");
        let machine = tempfile::tempdir().expect("machine");
        let programs = programs_dir(machine.path());
        std::fs::create_dir_all(&programs).expect("create programs");
        std::fs::write(programs.join("Rook.lnk"), b"").expect("shortcut");

        let status = resolve_status(&exe(), Some(user.path()), Some(machine.path()));
        assert!(status.available);
        assert!(
            status
                .shortcut
                .as_deref()
                .is_some_and(|path| path.starts_with(&machine.path().display().to_string()))
        );
    }

    #[test]
    fn status_unavailable_when_aumid_too_long() {
        let long = PathBuf::from("D:/".to_owned() + &"b".repeat(126) + ".exe");
        let status = resolve_status(&long, Some(Path::new("C:/Users/u/AppData/Roaming")), None);
        assert!(!status.available);
        assert_eq!(status.aumid, None);
        assert!(
            status
                .reason
                .as_deref()
                .is_some_and(|reason| reason.contains("128"))
        );
    }

    #[test]
    fn xml_escapes_special_chars() {
        let xml = toast_xml("<任务 & 配置>", "\"引号\" 与 '单引号'");
        assert!(xml.contains("&lt;任务 &amp; 配置&gt;"), "{xml}");
        assert!(
            xml.contains("&quot;引号&quot; 与 &apos;单引号&apos;"),
            "{xml}"
        );
        assert!(!xml.contains("<任务"));
    }

    #[test]
    fn xml_truncates_by_chars() {
        let title = "标".repeat(TITLE_MAX_CHARS + 20);
        let body = "文".repeat(BODY_MAX_CHARS + 20);
        let xml = toast_xml(&title, &body);
        assert!(xml.contains(&"标".repeat(TITLE_MAX_CHARS - 1)));
        assert!(xml.contains(&"文".repeat(BODY_MAX_CHARS - 1)));
        assert!(xml.contains('…'));
        assert!(!xml.contains(&"标".repeat(TITLE_MAX_CHARS + 1)));
    }

    #[test]
    fn truncate_keeps_short_input() {
        assert_eq!(truncate_chars("短", 8), "短");
        assert_eq!(truncate_chars("abcdef", 4), "abc…");
        assert_eq!(truncate_chars("abcdef", 0), "");
    }

    #[test]
    fn tag_is_sanitized_and_short() {
        assert_eq!(sanitize_tag("crash_loop"), "crash_loop");
        assert_eq!(sanitize_tag("需要 人工<处理>"), "notice");
        assert_eq!(sanitize_tag("a b/c"), "abc");
        assert_eq!(sanitize_tag(&"x".repeat(80)).len(), 32);
    }

    #[test]
    fn status_serializes_camel_case() {
        let status = ToastStatus {
            available: false,
            reason: Some("原因".to_owned()),
            aumid: Some("D:/x.exe".to_owned()),
            shortcut: None,
            last_error: Some("错误".to_owned()),
        };
        let value = serde_json::to_value(&status).expect("serialize");
        assert_eq!(value["available"], false);
        assert_eq!(value["reason"], "原因");
        assert_eq!(value["aumid"], "D:/x.exe");
        assert!(value["shortcut"].is_null());
        assert_eq!(value["lastError"], "错误");
    }
}
