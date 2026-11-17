//! 配置热重载文件监听（2026-11-05）。
//!
//! 目标行为（docs/02 §7.1）：
//! - 监听**配置文件所在目录**，而不是只监听文件本身；编辑器“写临时文件再
//!   rename”的原子保存也能触发；
//! - 300–500ms 去抖，连续保存只回调一次；
//! - `notify` 创建/监听失败（权限、网络盘、不支持）自动回退到
//!   `[supervisor].config_poll_interval_ms` 的 mtime+size 轮询；
//! - 自身保存抑制由调用方用内容 revision（SHA-256）完成：本模块只负责
//!   “发现文件可能变化”，不做解析、不碰 Engine 锁、不启动/停止任务。
//!
//! 本模块不依赖 Tauri；Engine 在回调里读取文件、校验并决定是否应用。

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::Serialize;

use super::LoadError;

/// 去抖静默窗口：最后一次文件事件后等这么久再回调。
const DEBOUNCE_QUIET: Duration = Duration::from_millis(400);
/// 去抖硬上限：事件持续不断时，最多等这么久也必须回调一次。
const DEBOUNCE_MAX: Duration = Duration::from_millis(800);
/// 轮询模式下检测到变化后的稳定等待步长。
const POLL_DEBOUNCE_STEP: Duration = Duration::from_millis(100);
/// 等待线程停止时的最大睡眠切片：不持锁、不做文件 I/O，只是尽快响应 stop。
const STOP_SLEEP_SLICE: Duration = Duration::from_millis(250);

/// 配置监听状态（只读投影进快照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WatchState {
    Disabled,
    Watching,
    Polling,
    Error,
}

impl WatchState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Disabled => "disabled",
            Self::Watching => "watching",
            Self::Polling => "polling",
            Self::Error => "error",
        }
    }
}

/// 监听错误视图：配置文件内容错误带行/列；监听/轮询错误用 1:1 占位。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWatchErrorView {
    pub file: String,
    pub line: usize,
    pub col: usize,
    pub message: String,
}

/// 快照里的只读配置监听状态（`configWatch`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigWatchView {
    /// 运行时是否启用了监听（`[supervisor].watch_config`）。
    pub enabled: bool,
    /// `"disabled"` / `"watching"` / `"polling"` / `"error"`。
    pub state: String,
    /// 最近一次成功重载的时刻（Unix epoch 毫秒）。
    pub last_reload_at: Option<i64>,
    /// 最近一次失败（内容非法或监听/轮询失败）；成功重载后清空。
    pub last_error: Option<ConfigWatchErrorView>,
}

impl Default for ConfigWatchView {
    fn default() -> Self {
        Self {
            enabled: false,
            state: WatchState::Disabled.as_str().to_owned(),
            last_reload_at: None,
            last_error: None,
        }
    }
}

struct WatchInner {
    enabled: bool,
    interval: Duration,
    state: WatchState,
    last_reload_at: Option<i64>,
    last_error: Option<ConfigWatchErrorView>,
    /// 最近一次成功加载/重载的文件内容 revision（自身保存抑制）。
    applied_revision: Option<String>,
}

/// 监听状态：watcher 线程与 Engine 共享。
///
/// 只保护内存字段，锁内不做文件 I/O；`view()` 供 Engine 在 refresh 时复制进快照。
pub struct ConfigWatchState {
    inner: Mutex<WatchInner>,
}

impl Default for ConfigWatchState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(WatchInner {
                enabled: false,
                interval: Duration::from_millis(1000),
                state: WatchState::Disabled,
                last_reload_at: None,
                last_error: None,
                applied_revision: None,
            }),
        }
    }
}

impl ConfigWatchState {
    fn lock(&self) -> MutexGuard<'_, WatchInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 按当前配置启用/关闭并更新回退轮询周期。
    pub fn configure(&self, enabled: bool, interval: Duration) {
        let mut inner = self.lock();
        inner.enabled = enabled;
        inner.interval = interval;
        if !enabled {
            inner.state = WatchState::Disabled;
            inner.last_error = None;
        } else if matches!(inner.state, WatchState::Disabled | WatchState::Error) {
            inner.state = WatchState::Watching;
            inner.last_error = None;
        }
    }

    /// 运行时是否启用。
    pub fn enabled(&self) -> bool {
        self.lock().enabled
    }

    /// 回退轮询周期。
    pub fn poll_interval(&self) -> Duration {
        self.lock().interval
    }

    /// 最近一次成功加载的 revision。
    pub fn applied_revision(&self) -> Option<String> {
        self.lock().applied_revision.clone()
    }

    pub fn set_applied_revision(&self, revision: impl Into<String>) {
        self.lock().applied_revision = Some(revision.into());
    }

    /// 成功重载：记录时刻并清除上一次错误。
    pub fn record_reload(&self, at_millis: i64) {
        let mut inner = self.lock();
        inner.last_reload_at = Some(at_millis);
        inner.last_error = None;
        if inner.enabled && inner.state == WatchState::Error {
            inner.state = WatchState::Watching;
        }
    }

    /// 记录一次内容/校验错误；同一错误重复出现返回 `false`（调用方据此去重日志）。
    pub fn record_error(
        &self,
        file: impl Into<String>,
        line: usize,
        col: usize,
        message: impl Into<String>,
    ) -> bool {
        let view = ConfigWatchErrorView {
            file: file.into(),
            line,
            col,
            message: message.into(),
        };
        let mut inner = self.lock();
        if inner.last_error.as_ref() == Some(&view) {
            return false;
        }
        inner.last_error = Some(view);
        true
    }

    /// 把配置加载错误转成监听错误。
    pub fn record_load_error(&self, error: &LoadError) -> bool {
        match error {
            LoadError::Invalid(error) => self.record_error(
                error.file.to_string_lossy().into_owned(),
                error.line,
                error.col,
                error.message.clone(),
            ),
            LoadError::Io { path, source } => self.record_error(
                path.to_string_lossy().into_owned(),
                1,
                1,
                format!("配置 I/O 失败：{source}"),
            ),
        }
    }

    /// notify 监听已就绪。
    fn set_watching(&self) {
        let mut inner = self.lock();
        inner.state = WatchState::Watching;
        inner.last_error = None;
    }

    /// 回退到轮询；返回状态/错误是否有变化（用于日志去重）。
    fn set_polling(&self, path: &Path, message: &str) -> bool {
        let view = ConfigWatchErrorView {
            file: path.to_string_lossy().into_owned(),
            line: 1,
            col: 1,
            message: message.to_owned(),
        };
        let mut inner = self.lock();
        let changed =
            inner.state != WatchState::Polling || inner.last_error.as_ref() != Some(&view);
        inner.state = WatchState::Polling;
        inner.last_error = Some(view);
        changed
    }

    /// 致命错误：线程未能启动或监听目录不可用且无法轮询。
    fn set_error(&self, path: &Path, message: impl Into<String>) {
        let mut inner = self.lock();
        inner.state = WatchState::Error;
        inner.last_error = Some(ConfigWatchErrorView {
            file: path.to_string_lossy().into_owned(),
            line: 1,
            col: 1,
            message: message.into(),
        });
    }

    /// 关闭监听状态（线程停止/应用退出时）。
    pub fn disable(&self) {
        let mut inner = self.lock();
        inner.enabled = false;
        inner.state = WatchState::Disabled;
        inner.last_error = None;
    }

    /// 生成快照视图。
    pub fn view(&self) -> ConfigWatchView {
        let inner = self.lock();
        let state = if inner.enabled {
            inner.state
        } else {
            WatchState::Disabled
        };
        ConfigWatchView {
            enabled: inner.enabled,
            state: state.as_str().to_owned(),
            last_reload_at: inner.last_reload_at,
            last_error: inner.last_error.clone(),
        }
    }
}

/// 监听目标与回退参数。
#[derive(Debug, Clone)]
pub struct WatchSpec {
    /// 当前配置文件路径（监听其父目录）。
    pub config_path: PathBuf,
    /// `--data-dir` 覆盖项；用同一份字节重新加载时保持一致。
    pub data_dir_override: Option<PathBuf>,
    /// 回退轮询周期（已由 Engine 做下限/上限收敛）。
    pub poll_interval: Duration,
}

/// 监听线程句柄；`Drop` 会请求停止并（非本线程时）等待线程退出。
pub struct ConfigWatchHandle {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl ConfigWatchHandle {
    /// 请求停止并等待线程退出。若在监听线程内部调用，只请求停止、不 join。
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(handle) = self.thread.take()
            && handle.thread().id() != thread::current().id()
        {
            let _ = handle.join();
        }
    }

    /// 只请求停止，不 join（在持有 Engine 锁时安全调用）。
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// 线程是否仍在运行（测试/诊断用）。
    pub fn is_running(&self) -> bool {
        self.thread.is_some() && !self.stop.load(Ordering::SeqCst)
    }

    /// 是否已请求停止或线程未启动。
    pub fn is_stopped(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.thread.is_none()
    }
}

impl Drop for ConfigWatchHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 启动自动监听（notify，失败回退轮询）。
pub fn spawn<F>(spec: WatchSpec, state: Arc<ConfigWatchState>, on_change: F) -> ConfigWatchHandle
where
    F: FnMut() + Send + 'static,
{
    spawn_with_mode(spec, state, on_change, WatchMode::Auto)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // `Poll` 仅测试用于强制走轮询路径。
enum WatchMode {
    Auto,
    Poll,
}

fn spawn_with_mode<F>(
    spec: WatchSpec,
    state: Arc<ConfigWatchState>,
    on_change: F,
    mode: WatchMode,
) -> ConfigWatchHandle
where
    F: FnMut() + Send + 'static,
{
    let stop = Arc::new(AtomicBool::new(false));
    if !state.enabled() {
        // 配置关闭：不创建任何线程。
        stop.store(true, Ordering::SeqCst);
        return ConfigWatchHandle { stop, thread: None };
    }

    let thread_stop = Arc::clone(&stop);
    let thread_state = Arc::clone(&state);
    let spawned = thread::Builder::new()
        .name("cs-config-watch".to_owned())
        .spawn(move || run(spec, thread_state, on_change, thread_stop, mode));

    match spawned {
        Ok(handle) => ConfigWatchHandle {
            stop,
            thread: Some(handle),
        },
        Err(error) => {
            state.set_error(Path::new(""), format!("创建配置监听线程失败：{error}"));
            tracing::error!(
                event = "config.watch_error",
                error = %error,
                "创建配置监听线程失败，自动重载不可用"
            );
            ConfigWatchHandle { stop, thread: None }
        }
    }
}

fn run<F>(
    spec: WatchSpec,
    state: Arc<ConfigWatchState>,
    mut on_change: F,
    stop: Arc<AtomicBool>,
    mode: WatchMode,
) where
    F: FnMut() + Send + 'static,
{
    if mode == WatchMode::Auto {
        match notify_loop(&spec, &state, &stop, &mut on_change) {
            Ok(()) => return,
            Err(failure) => {
                let changed = state.set_polling(&spec.config_path, &failure.message);
                if changed {
                    tracing::warn!(
                        event = "config.watch_error",
                        mode = "poll",
                        path = %spec.config_path.display(),
                        error = %failure.message,
                        "文件监听不可用，回退 mtime+size 轮询"
                    );
                }
                tracing::info!(
                    event = "config.watch_started",
                    mode = "poll",
                    path = %spec.config_path.display(),
                    interval_ms = spec.poll_interval.as_millis() as u64,
                    "配置监听已回退为轮询"
                );
            }
        }
    } else {
        state.set_polling(&spec.config_path, "配置监听使用轮询模式");
        tracing::info!(
            event = "config.watch_started",
            mode = "poll",
            path = %spec.config_path.display(),
            interval_ms = spec.poll_interval.as_millis() as u64,
            "配置监听已启动（轮询）"
        );
    }

    poll_loop(&spec, &stop, &mut on_change);
}

struct WatchFailure {
    message: String,
}

impl WatchFailure {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

fn notify_loop<F>(
    spec: &WatchSpec,
    state: &ConfigWatchState,
    stop: &AtomicBool,
    on_change: &mut F,
) -> Result<(), WatchFailure>
where
    F: FnMut(),
{
    let dir = parent_dir(&spec.config_path);
    let (tx, rx) = mpsc::channel();
    let mut watcher = RecommendedWatcher::new(tx, Config::default())
        .map_err(|error| WatchFailure::new(format!("创建文件监听失败：{error}")))?;
    watcher
        .watch(&dir, RecursiveMode::NonRecursive)
        .map_err(|error| WatchFailure::new(format!("监听目录 {} 失败：{error}", dir.display())))?;

    state.set_watching();
    tracing::info!(
        event = "config.watch_started",
        mode = "notify",
        path = %spec.config_path.display(),
        dir = %dir.display(),
        "配置目录监听已启动"
    );

    let mut debounce = Debounce::default();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Ok(());
        }
        let timeout = debounce
            .next_timeout(DEBOUNCE_QUIET, DEBOUNCE_MAX)
            .unwrap_or(Duration::from_millis(200));
        match rx.recv_timeout(timeout) {
            Ok(Ok(event)) => {
                if event_is_relevant(&event, &spec.config_path) {
                    debounce.note(Instant::now());
                }
            }
            Ok(Err(error)) => {
                return Err(WatchFailure::new(format!("文件监听错误：{error}")));
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return Ok(()),
        }
        if debounce.ready(Instant::now(), DEBOUNCE_QUIET, DEBOUNCE_MAX) {
            debounce.clear();
            on_change();
        }
    }
}

fn poll_loop<F>(spec: &WatchSpec, stop: &AtomicBool, on_change: &mut F)
where
    F: FnMut(),
{
    let mut last = fingerprint(&spec.config_path);
    loop {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        sleep_with_stop(spec.poll_interval, stop);
        if stop.load(Ordering::SeqCst) {
            return;
        }

        let current = fingerprint(&spec.config_path);
        if current == last {
            continue;
        }

        // 检测到 mtime/size 变化：等文件稳定（连续保存只回调一次）。
        let mut latest = current;
        let started = Instant::now();
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            sleep_with_stop(POLL_DEBOUNCE_STEP, stop);
            let next = fingerprint(&spec.config_path);
            if next == latest || started.elapsed() >= DEBOUNCE_MAX {
                latest = next;
                break;
            }
            latest = next;
        }

        last = latest;
        on_change();
    }
}

fn sleep_with_stop(total: Duration, stop: &AtomicBool) {
    let Some(deadline) = Instant::now().checked_add(total) else {
        return;
    };
    while Instant::now() < deadline {
        if stop.load(Ordering::SeqCst) {
            return;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        thread::sleep(remaining.min(STOP_SLEEP_SLICE));
    }
}

fn parent_dir(path: &Path) -> PathBuf {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf()
}

fn event_is_relevant(event: &Event, target: &Path) -> bool {
    if matches!(event.kind, EventKind::Access(_)) {
        return false;
    }
    let Some(target_name) = target.file_name() else {
        return true;
    };
    event
        .paths
        .iter()
        .any(|path| path == target || path.file_name() == Some(target_name))
}

/// 轮询指纹：mtime + size（与需求一致，不做内容 hash，避免每次轮询读整份文件）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileFingerprint {
    pub len: u64,
    pub modified: Option<SystemTime>,
}

/// 读取文件指纹；文件不存在/不可读时返回 `None`。
pub fn fingerprint(path: &Path) -> Option<FileFingerprint> {
    let metadata = fs::metadata(path).ok()?;
    Some(FileFingerprint {
        len: metadata.len(),
        modified: metadata.modified().ok(),
    })
}

/// 观察到的 revision 与已应用 revision 不同才需要重载（自身保存抑制）。
pub fn should_reload(applied: Option<&str>, observed: &str) -> bool {
    applied != Some(observed)
}

/// 当前 Unix epoch 毫秒。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

/// 去抖窗口：静默 `quiet` 后触发；事件持续不断时最多等 `max`。
#[derive(Debug, Default)]
struct Debounce {
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Debounce {
    fn note(&mut self, now: Instant) {
        self.first.get_or_insert(now);
        self.last = Some(now);
    }

    fn ready(&self, now: Instant, quiet: Duration, max: Duration) -> bool {
        let Some(first) = self.first else {
            return false;
        };
        let last = self.last.unwrap_or(first);
        now.saturating_duration_since(last) >= quiet || now.saturating_duration_since(first) >= max
    }

    fn next_timeout(&self, quiet: Duration, max: Duration) -> Option<Duration> {
        let first = self.first?;
        let last = self.last.unwrap_or(first);
        let now = Instant::now();
        let quiet_left = quiet.saturating_sub(now.saturating_duration_since(last));
        let max_left = max.saturating_sub(now.saturating_duration_since(first));
        Some(quiet_left.min(max_left).max(Duration::from_millis(20)))
    }

    fn clear(&mut self) {
        self.first = None;
        self.last = None;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::AtomicUsize;

    use super::*;

    #[test]
    fn debounce_waits_for_quiet_window() {
        let mut debounce = Debounce::default();
        let start = Instant::now();
        debounce.note(start);
        assert!(!debounce.ready(
            start + Duration::from_millis(399),
            DEBOUNCE_QUIET,
            DEBOUNCE_MAX
        ));
        assert!(debounce.ready(start + DEBOUNCE_QUIET, DEBOUNCE_QUIET, DEBOUNCE_MAX));
    }

    #[test]
    fn debounce_has_hard_cap_under_continuous_events() {
        let mut debounce = Debounce::default();
        let start = Instant::now();
        debounce.note(start);
        // 每隔 100ms 来一次事件，静默窗口永远不到，但硬上限必须触发。
        debounce.note(start + Duration::from_millis(700));
        assert!(!debounce.ready(
            start + Duration::from_millis(799),
            DEBOUNCE_QUIET,
            DEBOUNCE_MAX
        ));
        assert!(debounce.ready(start + DEBOUNCE_MAX, DEBOUNCE_QUIET, DEBOUNCE_MAX));
    }

    #[test]
    fn fingerprint_compares_size_and_mtime() {
        let a = FileFingerprint {
            len: 10,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
        };
        let b = FileFingerprint {
            len: 10,
            modified: Some(UNIX_EPOCH + Duration::from_secs(2)),
        };
        let c = FileFingerprint {
            len: 11,
            modified: Some(UNIX_EPOCH + Duration::from_secs(1)),
        };
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_eq!(a, a);
    }

    #[test]
    fn same_revision_is_suppressed() {
        assert!(!should_reload(Some("abc"), "abc"));
        assert!(should_reload(Some("abc"), "def"));
        assert!(should_reload(None, "def"));
    }

    #[test]
    fn content_error_is_recorded_without_losing_watch_state() {
        let state = ConfigWatchState::default();
        state.configure(true, Duration::from_millis(500));
        state.set_watching();
        let error = LoadError::Invalid(crate::config::ConfigError {
            file: PathBuf::from("C:/Rook.toml"),
            line: 3,
            col: 7,
            path: "supervisor.watch_config".to_owned(),
            message: "类型错误".to_owned(),
        });

        assert!(state.record_load_error(&error));
        // 同一错误重复出现不再触发日志/广播。
        assert!(!state.record_load_error(&error));
        let view = state.view();
        assert_eq!(view.state, "watching");
        let last = view.last_error.expect("应记录错误");
        assert_eq!((last.line, last.col), (3, 7));

        // 下一次成功重载清除错误。
        state.record_reload(1);
        let view = state.view();
        assert!(view.last_error.is_none());
        assert_eq!(view.last_reload_at, Some(1));
    }

    #[test]
    fn disabled_state_is_reported_and_does_not_spawn() {
        let state = Arc::new(ConfigWatchState::default());
        state.configure(false, Duration::from_millis(500));
        let view = state.view();
        assert!(!view.enabled);
        assert_eq!(view.state, "disabled");

        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let handle = spawn_with_mode(
            WatchSpec {
                config_path: PathBuf::from("C:/does/not/matter.toml"),
                data_dir_override: None,
                poll_interval: Duration::from_millis(500),
            },
            Arc::clone(&state),
            move || {
                hits_clone.fetch_add(1, Ordering::SeqCst);
            },
            WatchMode::Auto,
        );
        assert!(!handle.is_running());
        assert_eq!(hits.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn polling_detects_external_change_and_debounces() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("Rook.toml");
        fs::write(&path, "version = 1\n").expect("写配置");

        let state = Arc::new(ConfigWatchState::default());
        state.configure(true, Duration::from_millis(30));
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let mut handle = spawn_with_mode(
            WatchSpec {
                config_path: path.clone(),
                data_dir_override: None,
                poll_interval: Duration::from_millis(30),
            },
            Arc::clone(&state),
            move || {
                hits_clone.fetch_add(1, Ordering::SeqCst);
            },
            WatchMode::Poll,
        );
        // 等轮询线程完成首轮指纹基线，避免“写入早于基线”造成漏检。
        thread::sleep(Duration::from_millis(120));

        // 两次快速写入应只触发一次回调。
        fs::write(&path, "version = 1\n# a\n").expect("第一次外部写入");
        thread::sleep(Duration::from_millis(20));
        fs::write(&path, "version = 1\n# ab\n").expect("第二次外部写入");

        assert!(
            wait_until(|| hits.load(Ordering::SeqCst) >= 1, Duration::from_secs(3)),
            "轮询应检测到外部修改"
        );
        thread::sleep(Duration::from_millis(700));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "连续写入应被去抖合并");
        handle.stop();
    }

    #[test]
    fn valid_change_applies_and_invalid_change_is_rejected() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("Rook.toml");
        fs::write(&path, "version = 1\n").expect("写配置");

        let state = Arc::new(ConfigWatchState::default());
        state.configure(true, Duration::from_millis(30));
        let applied = Arc::new(AtomicUsize::new(0));
        let rejected = Arc::new(AtomicUsize::new(0));
        let applied_clone = Arc::clone(&applied);
        let rejected_clone = Arc::clone(&rejected);
        let read_path = path.clone();
        let data_dir = dir.path().to_path_buf();
        let mut handle = spawn_with_mode(
            WatchSpec {
                config_path: path.clone(),
                data_dir_override: Some(data_dir.clone()),
                poll_interval: Duration::from_millis(30),
            },
            Arc::clone(&state),
            move || {
                let bytes = match fs::read(&read_path) {
                    Ok(bytes) => bytes,
                    Err(_) => {
                        rejected_clone.fetch_add(1, Ordering::SeqCst);
                        return;
                    }
                };
                let env = crate::config::MapEnv::new();
                let ctx = crate::config::LoadContext {
                    config_path: read_path.as_path(),
                    data_dir_override: Some(data_dir.as_path()),
                    local_appdata: None,
                    env: &env,
                };
                match crate::config::load_bytes(&ctx, &bytes) {
                    Ok(_) => applied_clone.fetch_add(1, Ordering::SeqCst),
                    Err(_) => rejected_clone.fetch_add(1, Ordering::SeqCst),
                };
            },
            WatchMode::Poll,
        );
        // 等轮询线程完成首轮指纹基线，避免“写入早于基线”造成漏检。
        thread::sleep(Duration::from_millis(120));

        // 合法外部修改：应用一次。
        fs::write(&path, "version = 1\n# valid\n").expect("合法写入");
        assert!(
            wait_until(
                || applied.load(Ordering::SeqCst) >= 1,
                Duration::from_secs(3)
            ),
            "合法修改应被加载"
        );
        thread::sleep(Duration::from_millis(700));
        assert_eq!(applied.load(Ordering::SeqCst), 1);

        // 非法 TOML：只记录错误，不增加已应用计数。
        fs::write(&path, "version = ").expect("非法写入");
        assert!(
            wait_until(
                || rejected.load(Ordering::SeqCst) >= 1,
                Duration::from_secs(3)
            ),
            "非法修改应被拒绝"
        );
        thread::sleep(Duration::from_millis(700));
        assert_eq!(applied.load(Ordering::SeqCst), 1, "非法配置不得应用");
        handle.stop();
    }

    #[cfg(windows)]
    #[test]
    fn notify_detects_directory_change_and_debounces() {
        let dir = tempfile::tempdir().expect("临时目录");
        let path = dir.path().join("Rook.toml");
        fs::write(&path, "version = 1\n").expect("写配置");

        let state = Arc::new(ConfigWatchState::default());
        state.configure(true, Duration::from_millis(500));
        let hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = Arc::clone(&hits);
        let mut handle = spawn_with_mode(
            WatchSpec {
                config_path: path.clone(),
                data_dir_override: None,
                poll_interval: Duration::from_millis(500),
            },
            Arc::clone(&state),
            move || {
                hits_clone.fetch_add(1, Ordering::SeqCst);
            },
            WatchMode::Auto,
        );

        assert!(
            wait_until(
                || matches!(state.view().state.as_str(), "watching" | "polling"),
                Duration::from_secs(3)
            ),
            "监听线程应进入 watching/polling"
        );

        fs::write(&path, "version = 1\n# a\n").expect("第一次外部写入");
        thread::sleep(Duration::from_millis(50));
        fs::write(&path, "version = 1\n# ab\n").expect("第二次外部写入");

        assert!(
            wait_until(|| hits.load(Ordering::SeqCst) >= 1, Duration::from_secs(3)),
            "目录监听应检测到外部修改"
        );
        thread::sleep(Duration::from_millis(700));
        assert_eq!(hits.load(Ordering::SeqCst), 1, "连续写入应被去抖合并");
        handle.stop();
    }

    fn wait_until(mut predicate: impl FnMut() -> bool, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if predicate() {
                return true;
            }
            thread::sleep(Duration::from_millis(20));
        }
        predicate()
    }
}
