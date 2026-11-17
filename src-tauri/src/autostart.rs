//! M5-1：开机自启（HKCU Run）。
//!
//! 只实现 `run_key` 模式：写
//! `HKCU\Software\Microsoft\Windows\CurrentVersion\Run` 的 `Rook` 值
//!
//! 显示名已改为 Rook；注册表值与 exe/配置文件保持 Rook 以兼容已安装版本。
//! = `"<exe 绝对路径>" --autostart`。`scheduled_task` 属于 v1.1（见 docs/06 §4.1），
//! 这里只降级为 warn 并保持现状。
//!
//! 设计：
//! - [`desired`] / [`decide`] 是纯逻辑，单元测试覆盖取值、引号、幂等与降级；
//! - [`RunKeyStore`] 抽象注册表读写，测试用内存实现，**不在测试中改用户注册表**；
//! - 失败只 warn（[`reconcile_for_startup`]），绝不阻止应用启动。

use std::io;

use serde::Serialize;

use crate::config::{AutostartConfig, AutostartMode};

/// Run 键下的值名。
pub const VALUE_NAME: &str = crate::brand::AUTOSTART_VALUE;

/// 旧版（CampusSentry）的 Run 值名，只用于一次性清理。
pub const LEGACY_VALUE_NAME: &str = crate::brand::LEGACY_AUTOSTART_VALUE;

/// 当前用户 Run 键（相对 HKCU）。
pub const RUN_KEY_PATH: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";

/// 自启动命令行末尾的参数：不打开窗口、不弹通知。
pub const AUTOSTART_ARG: &str = "--autostart";

/// 期望的注册状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Desired {
    /// 应存在该值（内容即 Run 值字符串）。
    Register(String),
    /// 不应存在该值。
    Remove,
    /// v1 不实现的模式：保持现状。
    Unsupported(AutostartMode),
}

/// 生成 Run 值：exe 绝对路径始终加引号（路径含空格时也正确），后接 `--autostart`。
pub fn command_line(exe: &std::path::Path) -> String {
    format!("\"{}\" {AUTOSTART_ARG}", exe.display())
}

/// 由配置与当前 exe 推导期望状态。
///
/// - `enabled = false` → 删除（无论模式）；
/// - `enabled = true` + `run_key` → 注册；
/// - `enabled = true` + `scheduled_task` → 降级（v1.1）；
/// - `enabled = true` + `disabled` → 删除（等价于不注册）。
pub fn desired(config: &AutostartConfig, exe: &std::path::Path) -> Desired {
    if !config.enabled {
        return Desired::Remove;
    }
    match config.mode {
        AutostartMode::RunKey => Desired::Register(command_line(exe)),
        AutostartMode::ScheduledTask => Desired::Unsupported(AutostartMode::ScheduledTask),
        AutostartMode::Disabled => Desired::Remove,
    }
}

/// 单个值需要执行的动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// 写入期望值（新建或覆盖旧路径）。
    Write,
    /// 删除现有值。
    Delete,
    /// 已是期望状态，不写注册表。
    Nothing,
    /// v1 不支持的降级：什么都不做。
    Unsupported,
}

/// 比较期望与实际值（`None` = 值不存在）。
pub fn decide(desired: &Desired, actual: Option<&str>) -> Decision {
    match desired {
        Desired::Register(value) => match actual {
            // 幂等：相同值不重写。
            Some(current) if current == value => Decision::Nothing,
            _ => Decision::Write,
        },
        Desired::Remove => match actual {
            Some(_) => Decision::Delete,
            None => Decision::Nothing,
        },
        Desired::Unsupported(_) => Decision::Unsupported,
    }
}

/// 只读状态视图（`get_autostart_status` 的返回值）。
///
/// 字段与 `src/lib/api.ts` 的 `AutostartStatus` 一一对应；只描述现状，
/// 不触发任何注册表写操作。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AutostartStatus {
    /// 配置中的 `autostart.enabled`。
    pub enabled: bool,
    /// 配置中的 `autostart.mode`（`run_key` / `scheduled_task` / `disabled`）。
    pub mode: String,
    /// 按配置与当前 exe 推导出的期望 Run 值；不需要注册时为 `None`。
    pub expected_value: Option<String>,
    /// 注册表中实际读到的 Run 值；值不存在或读取失败时为 `None`。
    pub actual_value: Option<String>,
    /// 注册表现状是否与配置期望一致；读取失败时为 `false`。
    pub matches: bool,
    /// 注册表中是否存在该值；读取失败时为 `false`。
    pub registered: bool,
    /// 取 exe 或读取注册表失败时的说明；正常为 `None`。
    pub error: Option<String>,
}

impl AutostartStatus {
    /// 配置尚未成功加载时的兜底视图：只说明原因，不猜测注册表状态。
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self {
            enabled: false,
            mode: AutostartMode::Disabled.as_str().to_owned(),
            expected_value: None,
            actual_value: None,
            matches: false,
            registered: false,
            error: Some(message.into()),
        }
    }
}

/// 纯逻辑：比较配置期望与注册表实际值，生成只读状态。
///
/// `exe` 为 `current_exe` 的结果，`actual` 为 Run 键读取结果；两者失败都只写入
/// [`AutostartStatus::error`]，不 panic，也不写注册表。
pub fn status(
    config: &AutostartConfig,
    exe: Result<&std::path::Path, String>,
    actual: Result<Option<String>, String>,
) -> AutostartStatus {
    let mut error = None;

    let actual_value = match actual {
        Ok(value) => value,
        Err(message) => {
            error.get_or_insert(message);
            None
        }
    };

    let (expected_value, matches) = match exe {
        Ok(exe) => {
            let desired = desired(config, exe);
            let expected = match &desired {
                Desired::Register(value) => Some(value.clone()),
                Desired::Remove | Desired::Unsupported(_) => None,
            };
            // 读取失败时现状未知，不能报告"一致"。
            let matches =
                error.is_none() && decide(&desired, actual_value.as_deref()) == Decision::Nothing;
            (expected, matches)
        }
        Err(message) => {
            error.get_or_insert(message);
            (None, false)
        }
    };

    AutostartStatus {
        enabled: config.enabled,
        mode: config.mode.as_str().to_owned(),
        expected_value,
        registered: actual_value.is_some(),
        actual_value,
        matches,
        error,
    }
}

/// 同步结果（用于日志与测试断言）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 新建了值。
    Registered,
    /// 值已存在但内容不同（通常是 exe 路径变化），已覆盖。
    Updated,
    /// 删除了值。
    Removed,
    /// 期望注册且内容已一致。
    AlreadyCurrent,
    /// 期望不注册且确实不存在。
    AlreadyAbsent,
    /// `scheduled_task` 降级，未改动注册表。
    Unsupported,
}

/// 注册表读写的抽象；测试用内存实现，避免触碰用户注册表。
pub trait RunKeyStore {
    /// 读取当前值；`None` 表示键或值不存在。
    fn read(&self) -> io::Result<Option<String>>;
    fn write(&self, value: &str) -> io::Result<()>;
    fn delete(&self) -> io::Result<()>;
}

/// 把注册表对齐到期望状态；只有内容不同才写。
pub fn reconcile<S: RunKeyStore + ?Sized>(store: &S, desired: &Desired) -> io::Result<Outcome> {
    let actual = store.read()?;
    match decide(desired, actual.as_deref()) {
        Decision::Write => {
            let Desired::Register(value) = desired else {
                // decide 只在 Register 时返回 Write，这里不可达。
                return Ok(Outcome::AlreadyAbsent);
            };
            store.write(value)?;
            Ok(if actual.is_some() {
                Outcome::Updated
            } else {
                Outcome::Registered
            })
        }
        Decision::Delete => {
            store.delete()?;
            Ok(Outcome::Removed)
        }
        Decision::Nothing => Ok(match desired {
            Desired::Register(_) => Outcome::AlreadyCurrent,
            _ => Outcome::AlreadyAbsent,
        }),
        Decision::Unsupported => Ok(Outcome::Unsupported),
    }
}

/// 启动时按配置同步自启动。
///
/// - dev profile 直接跳过：开发实例不应污染生产 Run 键；
/// - 任何失败（取 exe 路径、读写注册表）只 warn，不影响启动；
/// - 返回 `None` 表示未执行（dev/取路径失败/读写失败）。
pub fn reconcile_for_startup(config: &AutostartConfig, is_dev: bool) -> Option<Outcome> {
    if is_dev {
        tracing::debug!("dev profile 跳过开机自启同步（避免污染生产 Run 键）");
        return None;
    }
    let exe = match std::env::current_exe() {
        Ok(exe) => exe,
        Err(error) => {
            tracing::warn!(error = %error, "取当前 exe 路径失败，跳过开机自启同步");
            return None;
        }
    };
    let desired = desired(config, &exe);

    #[cfg(windows)]
    {
        match reconcile(&WindowsRunKey::new(), &desired) {
            Ok(outcome) => {
                log_outcome(outcome, &desired, &exe);
                Some(outcome)
            }
            Err(error) => {
                tracing::warn!(error = %error, "同步开机自启失败（不影响启动）");
                None
            }
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (desired, exe);
        None
    }
}

fn log_outcome(outcome: Outcome, desired: &Desired, exe: &std::path::Path) {
    match outcome {
        Outcome::Registered => tracing::info!(
            value = %command_line(exe),
            "已注册开机自启（HKCU Run）"
        ),
        Outcome::Updated => tracing::info!(
            value = %command_line(exe),
            "开机自启路径已更新"
        ),
        Outcome::Removed => tracing::info!("已取消开机自启（删除 HKCU Run 值）"),
        Outcome::AlreadyCurrent => tracing::debug!("开机自启已是最新，未重写注册表"),
        Outcome::AlreadyAbsent => tracing::debug!("开机自启未注册，与配置一致"),
        Outcome::Unsupported => tracing::warn!(
            mode = desired_mode(desired),
            "autostart.mode=scheduled_task 在 v1 暂不支持，保持现有 Run 键不变（v1.1 实现）"
        ),
    }
}

fn desired_mode(desired: &Desired) -> &'static str {
    match desired {
        Desired::Register(_) => AutostartMode::RunKey.as_str(),
        Desired::Remove => AutostartMode::Disabled.as_str(),
        Desired::Unsupported(mode) => mode.as_str(),
    }
}

/// 真实 HKCU Run 键（仅 Windows）；值名可指定，便于清理旧版遗留值。
#[cfg(windows)]
pub struct WindowsRunKey {
    value_name: &'static str,
}

#[cfg(windows)]
impl WindowsRunKey {
    /// 当前版本的值名（`Rook`）。
    pub fn new() -> Self {
        Self {
            value_name: VALUE_NAME,
        }
    }

    /// 旧版遗留值名（`CampusSentry`），只用于删除。
    pub fn legacy() -> Self {
        Self {
            value_name: LEGACY_VALUE_NAME,
        }
    }
}

#[cfg(windows)]
impl Default for WindowsRunKey {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(windows)]
impl RunKeyStore for WindowsRunKey {
    fn read(&self) -> io::Result<Option<String>> {
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_QUERY_VALUE, REG_SZ, REG_VALUE_TYPE, RegOpenKeyExW,
            RegQueryValueExW,
        };
        use windows::core::HSTRING;

        let mut key = HKEY(std::ptr::null_mut());
        let key_path = HSTRING::from(RUN_KEY_PATH);
        let status = unsafe {
            RegOpenKeyExW(
                HKEY_CURRENT_USER,
                &key_path,
                None,
                KEY_QUERY_VALUE,
                &mut key,
            )
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(win32_error("打开 Run 键", status));
        }
        let guard = KeyGuard(key);

        let name = HSTRING::from(self.value_name);
        let mut kind = REG_VALUE_TYPE::default();
        let mut size = 0u32;
        let status = unsafe {
            RegQueryValueExW(guard.0, &name, None, Some(&mut kind), None, Some(&mut size))
        };
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(None);
        }
        if status != ERROR_SUCCESS {
            return Err(win32_error("查询 Run 值", status));
        }
        if kind != REG_SZ || size == 0 {
            // 非字符串或空值按"不存在"处理：随后会被按期望覆盖。
            return Ok(None);
        }

        let mut buffer = vec![0u8; size as usize];
        let mut read_size = size;
        let status = unsafe {
            RegQueryValueExW(
                guard.0,
                &name,
                None,
                Some(&mut kind),
                Some(buffer.as_mut_ptr()),
                Some(&mut read_size),
            )
        };
        if status != ERROR_SUCCESS {
            return Err(win32_error("读取 Run 值", status));
        }
        buffer.truncate(read_size as usize);
        let wide: Vec<u16> = buffer
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        let text = String::from_utf16_lossy(&wide);
        Ok(Some(text.trim_end_matches('\0').to_owned()))
    }

    fn write(&self, value: &str) -> io::Result<()> {
        use windows::Win32::Foundation::ERROR_SUCCESS;
        use windows::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_WRITE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCreateKeyExW,
            RegSetValueExW,
        };
        use windows::core::{HSTRING, PCWSTR};

        let mut key = HKEY(std::ptr::null_mut());
        let key_path = HSTRING::from(RUN_KEY_PATH);
        let status = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                &key_path,
                None,
                PCWSTR::null(),
                REG_OPTION_NON_VOLATILE,
                KEY_WRITE,
                None,
                &mut key,
                None,
            )
        };
        if status != ERROR_SUCCESS {
            return Err(win32_error("打开/创建 Run 键", status));
        }
        let guard = KeyGuard(key);

        // REG_SZ 需要 UTF-16 且以 NUL 结尾。
        let mut wide: Vec<u16> = value.encode_utf16().collect();
        wide.push(0);
        let bytes =
            unsafe { std::slice::from_raw_parts(wide.as_ptr().cast::<u8>(), wide.len() * 2) };
        let name = HSTRING::from(self.value_name);
        let status = unsafe { RegSetValueExW(guard.0, &name, None, REG_SZ, Some(bytes)) };
        if status != ERROR_SUCCESS {
            return Err(win32_error("写入 Run 值", status));
        }
        Ok(())
    }

    fn delete(&self) -> io::Result<()> {
        use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
        use windows::Win32::System::Registry::{
            HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, RegDeleteValueW, RegOpenKeyExW,
        };
        use windows::core::HSTRING;

        let mut key = HKEY(std::ptr::null_mut());
        let key_path = HSTRING::from(RUN_KEY_PATH);
        let status =
            unsafe { RegOpenKeyExW(HKEY_CURRENT_USER, &key_path, None, KEY_SET_VALUE, &mut key) };
        if status == ERROR_FILE_NOT_FOUND {
            // Run 键不存在 ⇒ 值也不存在。
            return Ok(());
        }
        if status != ERROR_SUCCESS {
            return Err(win32_error("打开 Run 键", status));
        }
        let guard = KeyGuard(key);

        let name = HSTRING::from(self.value_name);
        let status = unsafe { RegDeleteValueW(guard.0, &name) };
        if status == ERROR_SUCCESS || status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        Err(win32_error("删除 Run 值", status))
    }
}

/// 保证 `HKEY` 在所有提前返回路径上被关闭。
#[cfg(windows)]
struct KeyGuard(windows::Win32::System::Registry::HKEY);

#[cfg(windows)]
impl Drop for KeyGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Registry::RegCloseKey(self.0);
        }
    }
}

#[cfg(windows)]
fn win32_error(action: &str, status: windows::Win32::Foundation::WIN32_ERROR) -> io::Error {
    io::Error::other(format!("{action}失败：Win32 错误码 {}", status.0))
}

#[cfg(test)]
mod tests {
    use std::cell::{Cell, RefCell};
    use std::path::Path;

    use super::*;

    /// 内存版 Run 键：只记录状态与调用次数，不触碰真实注册表。
    #[derive(Default)]
    struct FakeStore {
        value: RefCell<Option<String>>,
        writes: Cell<usize>,
        deletes: Cell<usize>,
        read_fails: Cell<bool>,
    }

    impl FakeStore {
        fn with_value(value: &str) -> Self {
            let store = Self::default();
            *store.value.borrow_mut() = Some(value.to_owned());
            store
        }
    }

    impl RunKeyStore for FakeStore {
        fn read(&self) -> io::Result<Option<String>> {
            if self.read_fails.get() {
                return Err(io::Error::other("模拟读取失败"));
            }
            Ok(self.value.borrow().clone())
        }

        fn write(&self, value: &str) -> io::Result<()> {
            self.writes.set(self.writes.get() + 1);
            *self.value.borrow_mut() = Some(value.to_owned());
            Ok(())
        }

        fn delete(&self) -> io::Result<()> {
            self.deletes.set(self.deletes.get() + 1);
            *self.value.borrow_mut() = None;
            Ok(())
        }
    }

    fn config(enabled: bool, mode: AutostartMode) -> AutostartConfig {
        AutostartConfig { enabled, mode }
    }

    const EXE: &str = r"C:\Program Files\Rook\Rook.exe";

    #[test]
    fn run_key_path_and_value_name_match_docs() {
        assert_eq!(
            RUN_KEY_PATH,
            r"Software\Microsoft\Windows\CurrentVersion\Run"
        );
        assert_eq!(VALUE_NAME, "Rook");
        assert_eq!(AUTOSTART_ARG, "--autostart");
    }

    #[test]
    fn command_line_quotes_path_with_spaces() {
        assert_eq!(
            command_line(Path::new(EXE)),
            r#""C:\Program Files\Rook\Rook.exe" --autostart"#
        );
    }

    #[test]
    fn command_line_quotes_path_without_spaces() {
        assert_eq!(
            command_line(Path::new(r"D:\tools\Rook.exe")),
            r#""D:\tools\Rook.exe" --autostart"#
        );
    }

    #[test]
    fn enabled_run_key_wants_registration() {
        let desired = desired(&config(true, AutostartMode::RunKey), Path::new(EXE));
        assert_eq!(desired, Desired::Register(command_line(Path::new(EXE))));
    }

    #[test]
    fn disabled_wants_removal_regardless_of_mode() {
        for mode in [
            AutostartMode::RunKey,
            AutostartMode::ScheduledTask,
            AutostartMode::Disabled,
        ] {
            assert_eq!(
                desired(&config(false, mode), Path::new(EXE)),
                Desired::Remove
            );
        }
    }

    #[test]
    fn mode_disabled_wants_removal() {
        assert_eq!(
            desired(&config(true, AutostartMode::Disabled), Path::new(EXE)),
            Desired::Remove
        );
    }

    #[test]
    fn scheduled_task_mode_degrades() {
        let desired = desired(&config(true, AutostartMode::ScheduledTask), Path::new(EXE));
        assert_eq!(desired, Desired::Unsupported(AutostartMode::ScheduledTask));
        assert_eq!(decide(&desired, Some("whatever")), Decision::Unsupported);
        assert_eq!(decide(&desired, None), Decision::Unsupported);
    }

    #[test]
    fn decision_covers_all_cases() {
        let register = Desired::Register("v".to_owned());
        assert_eq!(decide(&register, None), Decision::Write);
        assert_eq!(decide(&register, Some("v")), Decision::Nothing);
        assert_eq!(decide(&register, Some("old")), Decision::Write);
        assert_eq!(decide(&Desired::Remove, Some("v")), Decision::Delete);
        assert_eq!(decide(&Desired::Remove, None), Decision::Nothing);
    }

    #[test]
    fn registers_when_absent() {
        let store = FakeStore::default();
        let desired = desired(&config(true, AutostartMode::RunKey), Path::new(EXE));
        assert_eq!(reconcile(&store, &desired).unwrap(), Outcome::Registered);
        assert_eq!(store.writes.get(), 1);
        assert_eq!(
            store.value.borrow().as_deref(),
            Some(command_line(Path::new(EXE)).as_str())
        );
    }

    #[test]
    fn same_value_is_not_rewritten() {
        let value = command_line(Path::new(EXE));
        let store = FakeStore::with_value(&value);
        let desired = desired(&config(true, AutostartMode::RunKey), Path::new(EXE));
        assert_eq!(
            reconcile(&store, &desired).unwrap(),
            Outcome::AlreadyCurrent
        );
        assert_eq!(store.writes.get(), 0);
        assert_eq!(store.deletes.get(), 0);
    }

    #[test]
    fn exe_path_change_updates_value() {
        let store = FakeStore::with_value(r#""C:\old\Rook.exe" --autostart"#);
        let desired = desired(&config(true, AutostartMode::RunKey), Path::new(EXE));
        assert_eq!(reconcile(&store, &desired).unwrap(), Outcome::Updated);
        assert_eq!(store.writes.get(), 1);
        assert_eq!(
            store.value.borrow().as_deref(),
            Some(command_line(Path::new(EXE)).as_str())
        );
    }

    #[test]
    fn disabled_deletes_existing_value() {
        let store = FakeStore::with_value("anything");
        let desired = desired(&config(false, AutostartMode::RunKey), Path::new(EXE));
        assert_eq!(reconcile(&store, &desired).unwrap(), Outcome::Removed);
        assert_eq!(store.deletes.get(), 1);
        assert!(store.value.borrow().is_none());
    }

    #[test]
    fn disabled_without_value_is_noop() {
        let store = FakeStore::default();
        let desired = desired(&config(false, AutostartMode::RunKey), Path::new(EXE));
        assert_eq!(reconcile(&store, &desired).unwrap(), Outcome::AlreadyAbsent);
        assert_eq!(store.deletes.get(), 0);
    }

    #[test]
    fn scheduled_task_leaves_registry_untouched() {
        let store = FakeStore::with_value(r#""C:\old\Rook.exe" --autostart"#);
        let desired = desired(&config(true, AutostartMode::ScheduledTask), Path::new(EXE));
        assert_eq!(reconcile(&store, &desired).unwrap(), Outcome::Unsupported);
        assert_eq!(store.writes.get(), 0);
        assert_eq!(store.deletes.get(), 0);
        assert_eq!(
            store.value.borrow().as_deref(),
            Some(r#""C:\old\Rook.exe" --autostart"#)
        );
    }

    #[test]
    fn read_failure_propagates() {
        let store = FakeStore::default();
        store.read_fails.set(true);
        let desired = desired(&config(true, AutostartMode::RunKey), Path::new(EXE));
        assert!(reconcile(&store, &desired).is_err());
    }

    #[test]
    fn dev_profile_skips_registration() {
        // dev 下即使配置要求注册也不执行（不触碰注册表）。
        assert_eq!(
            reconcile_for_startup(&config(true, AutostartMode::RunKey), true),
            None
        );
    }

    #[test]
    fn status_reports_registered_when_value_matches() {
        let value = command_line(Path::new(EXE));
        let view = status(
            &config(true, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Ok(Some(value.clone())),
        );
        assert!(view.enabled);
        assert_eq!(view.mode, "run_key");
        assert_eq!(view.expected_value.as_deref(), Some(value.as_str()));
        assert_eq!(view.actual_value.as_deref(), Some(value.as_str()));
        assert!(view.matches);
        assert!(view.registered);
        assert!(view.error.is_none());
    }

    #[test]
    fn status_reports_path_mismatch() {
        let old = r#""C:\old\Rook.exe" --autostart"#;
        let view = status(
            &config(true, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Ok(Some(old.to_owned())),
        );
        assert!(view.registered);
        assert!(!view.matches);
        assert_eq!(view.actual_value.as_deref(), Some(old));
        assert_eq!(
            view.expected_value.as_deref(),
            Some(command_line(Path::new(EXE)).as_str())
        );
        assert!(view.error.is_none());
    }

    #[test]
    fn status_reports_not_registered() {
        let view = status(
            &config(true, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Ok(None),
        );
        assert!(!view.registered);
        assert!(!view.matches);
        assert!(view.expected_value.is_some());
        assert!(view.actual_value.is_none());
        assert!(view.error.is_none());
    }

    #[test]
    fn status_disabled_matches_absent_value() {
        let view = status(
            &config(false, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Ok(None),
        );
        assert!(!view.enabled);
        assert!(view.matches);
        assert!(!view.registered);
        assert!(view.expected_value.is_none());
    }

    #[test]
    fn status_disabled_with_leftover_value_is_mismatch() {
        let view = status(
            &config(false, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Ok(Some(r#""C:\old\Rook.exe" --autostart"#.to_owned())),
        );
        assert!(view.registered);
        assert!(!view.matches);
        assert!(view.expected_value.is_none());
    }

    #[test]
    fn status_scheduled_task_has_no_expected_value() {
        let view = status(
            &config(true, AutostartMode::ScheduledTask),
            Ok(Path::new(EXE)),
            Ok(None),
        );
        assert_eq!(view.mode, "scheduled_task");
        assert!(view.expected_value.is_none());
        assert!(!view.matches);
        assert!(!view.registered);
    }

    #[test]
    fn status_exe_error_is_reported_without_panic() {
        let view = status(
            &config(true, AutostartMode::RunKey),
            Err("取当前 exe 路径失败".to_owned()),
            Ok(Some("whatever".to_owned())),
        );
        assert_eq!(view.error.as_deref(), Some("取当前 exe 路径失败"));
        assert!(view.expected_value.is_none());
        assert!(!view.matches);
        // 注册表读取成功时仍如实展示实际值。
        assert!(view.registered);
        assert_eq!(view.actual_value.as_deref(), Some("whatever"));
    }

    #[test]
    fn status_read_error_is_reported_without_panic() {
        let view = status(
            &config(true, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Err("读取 Run 键失败".to_owned()),
        );
        assert_eq!(view.error.as_deref(), Some("读取 Run 键失败"));
        assert!(!view.registered);
        assert!(!view.matches);
        assert!(view.actual_value.is_none());
    }

    #[test]
    fn unavailable_status_only_fills_error() {
        let view = AutostartStatus::unavailable("配置未加载");
        assert!(!view.enabled);
        assert_eq!(view.mode, "disabled");
        assert!(view.expected_value.is_none());
        assert!(view.actual_value.is_none());
        assert!(!view.matches);
        assert!(!view.registered);
        assert_eq!(view.error.as_deref(), Some("配置未加载"));
    }

    #[test]
    fn status_serializes_camel_case() {
        let value = serde_json::to_value(status(
            &config(true, AutostartMode::RunKey),
            Ok(Path::new(EXE)),
            Ok(Some(command_line(Path::new(EXE)))),
        ))
        .expect("序列化开机自启状态");

        for key in [
            "enabled",
            "mode",
            "expectedValue",
            "actualValue",
            "matches",
            "registered",
            "error",
        ] {
            assert!(value.get(key).is_some(), "缺少字段 {key}");
        }
        assert!(value.get("expected_value").is_none());
        assert!(value.get("actual_value").is_none());
        assert_eq!(value["mode"], "run_key");
        assert_eq!(value["matches"], true);
    }
}
