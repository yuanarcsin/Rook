//! 核心日志（M1-5）：文件输出、按天轮转、保留期清理、panic 记录、级别热调整。
//!
//! 设计要点：
//! - 路径来自 provisional 数据目录（见 [`crate::data_dir`]），固定为 `<data>/core/rook.log`；
//!   跨天时把当天文件改名为 `rook.log.YYYY-MM-DD`，正在写的那份路径始终不变。
//! - 写日志失败绝不阻止启动：目录/文件打不开就退回 stderr，配置与托盘照常工作。
//! - 默认 `info`；`RUST_LOG` 存在且可解析时以它为准，配置里的 `[logs].core_level` 只在没有
//!   `RUST_LOG` 时通过 reload 生效。
//! - panic 时先写 `<data>/core/crash-<timestamp>.log`（内容/位置/版本），再交给默认 hook。
//!
//! 本模块不依赖 Tauri，可在测试中直接验证清理与级别映射。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Datelike, Days, Local, NaiveDate, SecondsFormat, Utc};
use tracing::field::{Field, Visit};
use tracing::level_filters::LevelFilter;
use tracing::{Event, Subscriber};
use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer, Registry, reload};

use crate::cli::CliArgs;
use crate::config::{self, CoreLevel, Overrides};
use crate::data_dir;

/// 数据目录下的核心日志子目录。
pub const CORE_DIR: &str = "core";
/// 当前正在写的日志文件名（轮转后归档为 `<名字>.<日期>`）。
pub const CORE_LOG_FILE: &str = crate::brand::CORE_LOG_FILE;
/// 结构化事件日志（JSONL）：一行一个 tracing 事件，供 agent/诊断工具查询。
pub const EVENTS_LOG_FILE: &str = "events.jsonl";
/// `[logs].retention_days` 的默认值（7 天），配置加载前先用它清理一次。
pub const DEFAULT_RETENTION_DAYS: u32 = 7;

/// 全局日志句柄：持有写入线程的 guard 与级别 reload 句柄。
#[derive(Clone)]
pub struct Logging {
    inner: Arc<Inner>,
}

struct Inner {
    /// 日志目录；`None` 表示目录不可用（输出退回 stderr）。
    dir: Option<PathBuf>,
    filter: reload::Handle<EnvFilter, Registry>,
    /// `RUST_LOG` 生效时，配置不再覆盖级别。
    env_override: bool,
    /// 非阻塞写入线程的 guard：最后一个句柄释放时刷盘退出。
    /// 用 `Mutex<Option<_>>` 包裹，退出流程可主动取走并 drop（刷盘并等待写线程）。
    _guard: std::sync::Mutex<Option<WorkerGuard>>,
    /// JSONL 事件写入线程的 guard。
    _events_guard: std::sync::Mutex<Option<WorkerGuard>>,
    /// 事件日志目录（与核心日志同目录）；不可用时为 None。
    events_dir: Option<PathBuf>,
}

/// 初始化全局订阅者并安装 panic hook；任何失败都只降级，不返回错误。
pub fn init(args: &CliArgs) -> Logging {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let local_appdata = std::env::var_os("LOCALAPPDATA").map(PathBuf::from);
    let provisional = match data_dir::provisional(args, &cwd, local_appdata.as_deref()) {
        Ok(data_dir) => Some(data_dir),
        Err(message) => {
            eprintln!("[rook] {message}");
            None
        }
    };
    // 2026-11-05：把 `[app].data_dir` 也用于核心日志/JSONL 事件日志。
    // 日志必须在配置校验之前初始化，所以这里只做一次轻量 load；失败则回退
    // provisional 目录，不阻止启动。dev profile 固定用 `.dev-data`，避免污染仓库内配置。
    let data_dir = if args.is_dev() {
        provisional
    } else {
        match config::load(&Overrides {
            config: args.config.as_deref(),
            data_dir: args.data_dir.as_deref(),
        }) {
            Ok(loaded) => Some(loaded.data_dir),
            Err(_) => provisional,
        }
    };
    let dir = match data_dir {
        Some(data_dir) => {
            let core = data_dir.join(CORE_DIR);
            match fs::create_dir_all(&core) {
                Ok(()) => Some(core),
                Err(error) => {
                    eprintln!("[rook] 无法创建日志目录 {}：{error}", core.display());
                    None
                }
            }
        }
        None => None,
    };

    let (writer, guard) = match &dir {
        Some(dir) => match DailyFile::open(dir) {
            Ok(file) => tracing_appender::non_blocking(file),
            Err(error) => {
                eprintln!(
                    "[rook] 无法打开日志文件 {}：{error}",
                    dir.join(CORE_LOG_FILE).display()
                );
                tracing_appender::non_blocking(io::stderr())
            }
        },
        None => tracing_appender::non_blocking(io::stderr()),
    };
    // 2026-11-04：结构化 JSONL 事件日志。与人类可读核心日志同目录、同一天
    // 轮转节奏；写失败只退回 sink，不影响核心日志与监督。
    let (events_writer, events_guard) = match &dir {
        Some(dir) => match DailyFile::open_named(dir, EVENTS_LOG_FILE) {
            Ok(file) => tracing_appender::non_blocking(file),
            Err(error) => {
                eprintln!(
                    "[rook] 无法打开事件日志 {}：{error}",
                    dir.join(EVENTS_LOG_FILE).display()
                );
                tracing_appender::non_blocking(io::sink())
            }
        },
        None => tracing_appender::non_blocking(io::sink()),
    };
    let events_layer = JsonEventLayer::new(events_writer);

    let (filter, env_override) = match std::env::var_os("RUST_LOG") {
        Some(_) => match EnvFilter::try_from_default_env() {
            Ok(filter) => (filter, true),
            Err(error) => {
                eprintln!("[rook] RUST_LOG 无法解析，回退到 info：{error}");
                (default_filter(), false)
            }
        },
        None => (default_filter(), false),
    };

    let (filter_layer, handle) = reload::Layer::new(filter);
    let subscriber = tracing_subscriber::registry()
        .with(filter_layer)
        .with(
            tracing_subscriber::fmt::layer()
                .with_writer(writer)
                .with_ansi(false),
        )
        .with(events_layer);
    if let Err(error) = subscriber.try_init() {
        eprintln!("[rook] 日志订阅者初始化失败：{error}");
    }

    install_panic_hook(dir.clone());

    let logging = Logging {
        inner: Arc::new(Inner {
            dir: dir.clone(),
            filter: handle,
            env_override,
            _guard: std::sync::Mutex::new(Some(guard)),
            _events_guard: std::sync::Mutex::new(Some(events_guard)),
            events_dir: dir,
        }),
    };
    logging.cleanup_core_logs(DEFAULT_RETENTION_DAYS);
    logging
}

/// 配置里的 `[logs].core_level` 对应的 tracing 级别。
pub fn level_from_core(level: CoreLevel) -> LevelFilter {
    match level {
        CoreLevel::Error => LevelFilter::ERROR,
        CoreLevel::Warn => LevelFilter::WARN,
        CoreLevel::Info => LevelFilter::INFO,
        CoreLevel::Debug => LevelFilter::DEBUG,
        CoreLevel::Trace => LevelFilter::TRACE,
    }
}

fn default_filter() -> EnvFilter {
    EnvFilter::new(level_from_core(CoreLevel::Info).to_string())
}

impl Logging {
    /// 测试用：不落盘、不装订阅者的 `Logging`。
    ///
    /// 只给 `Engine` 的单元测试提供字段占位：`dir`/`events_dir` 为 `None`，
    /// 级别调整与清理都是空操作。不触碰磁盘、不安装全局订阅者。
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        let (_, filter) = reload::Layer::new(EnvFilter::new("off"));
        Self {
            inner: Arc::new(Inner {
                dir: None,
                filter,
                env_override: false,
                _guard: Mutex::new(None),
                _events_guard: Mutex::new(None),
                events_dir: None,
            }),
        }
    }

    /// 事件日志目录（核心日志同目录）；不可用时为 None。
    pub fn events_dir(&self) -> Option<&Path> {
        self.inner.events_dir.as_deref()
    }

    /// 按配置调整核心日志级别；`RUST_LOG` 优先时不覆盖。
    pub fn apply_core_level(&self, level: CoreLevel) {
        if self.inner.env_override {
            tracing::debug!("RUST_LOG 已设置，忽略配置中的 core_level");
            return;
        }
        let filter = EnvFilter::new(level_from_core(level).to_string());
        if let Err(error) = self.inner.filter.reload(filter) {
            tracing::warn!("调整 core_level 失败：{error}");
        }
    }

    /// 按保留天数清理归档日志（核心/事件归档 + 崩溃日志）；失败只记一条警告。
    pub fn cleanup_core_logs(&self, retention_days: u32) {
        let Some(dir) = &self.inner.dir else {
            return;
        };
        match cleanup_old_logs(dir, retention_days, Local::now().date_naive()) {
            Ok(0) => {}
            Ok(removed) => tracing::info!(
                event = "logs.cleanup",
                scope = "core",
                removed,
                retention_days,
                "清理过期核心/事件/崩溃日志"
            ),
            Err(error) => tracing::warn!(%error, "清理核心日志失败"),
        }
    }

    /// 刷盘并等待非阻塞写线程结束。
    ///
    /// 只在应用退出流程最后调用一次：取走并释放两个 `WorkerGuard`，
    /// `tracing_appender` 会在 guard drop 时 flush 队列并 join 写线程。
    /// 之后继续写 tracing 不会落盘（进程即将 `app.exit(0)`）。
    pub fn flush(&self) {
        if let Ok(mut guard) = self.inner._guard.lock() {
            let _ = guard.take();
        }
        if let Ok(mut guard) = self.inner._events_guard.lock() {
            let _ = guard.take();
        }
    }
}

/// 删除 `<dir>` 下日期早于 `today - retention_days` 的核心日志、JSONL 事件归档与
/// `crash-*.log`。
///
/// 当前正在写的 `rook.log` / `events.jsonl` 与无法识别的文件一律保留。
/// 崩溃日志优先按文件名里的日期判断，解析失败时退回 mtime；两者都没有则保留。
pub fn cleanup_old_logs(dir: &Path, retention_days: u32, today: NaiveDate) -> io::Result<usize> {
    // 与 `[logs].retention_days` 的文档语义一致：0 表示不按日期清理。
    if retention_days == 0 {
        return Ok(0);
    }
    let cutoff = today
        .checked_sub_days(Days::new(u64::from(retention_days)))
        .unwrap_or(NaiveDate::MIN);

    let mut removed = 0;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let date = if is_crash_log(&name) {
            crash_log_date(&name).or_else(|| file_date(&entry))
        } else {
            archive_date(&name).or_else(|| archive_date_named(&name, EVENTS_LOG_FILE))
        };
        let Some(date) = date else {
            continue;
        };
        if date < cutoff {
            match fs::remove_file(entry.path()) {
                Ok(()) => removed += 1,
                Err(error) => {
                    tracing::warn!("删除过期日志 {} 失败：{error}", entry.path().display());
                }
            }
        }
    }
    Ok(removed)
}

/// 从 `rook.log.YYYY-MM-DD` 里解析归档日期。
pub(crate) fn archive_date(name: &str) -> Option<NaiveDate> {
    archive_date_named(name, CORE_LOG_FILE)
}

/// 从 `<prefix>.YYYY-MM-DD` 里解析归档日期。
pub(crate) fn archive_date_named(name: &str, prefix: &str) -> Option<NaiveDate> {
    let suffix = name.strip_prefix(prefix)?.strip_prefix('.')?;
    NaiveDate::parse_from_str(suffix, "%Y-%m-%d").ok()
}

/// 是否是 panic hook 写出的崩溃日志（`crash-<时间戳>.log`）。
pub(crate) fn is_crash_log(name: &str) -> bool {
    name.starts_with("crash-") && name.ends_with(".log")
}

/// 从 `crash-YYYYMMDD-HHMMSS.log` 里解析日期；命名不符合约定时返回 None。
pub(crate) fn crash_log_date(name: &str) -> Option<NaiveDate> {
    let stamp = name.strip_prefix("crash-")?.strip_suffix(".log")?;
    chrono::NaiveDateTime::parse_from_str(stamp, "%Y%m%d-%H%M%S")
        .ok()
        .map(|value| value.date())
}

/// 文件 mtime 对应的本地日期；元数据不可读时返回 None。
fn file_date(entry: &fs::DirEntry) -> Option<NaiveDate> {
    let modified = entry.metadata().ok()?.modified().ok()?;
    Some(chrono::DateTime::<Local>::from(modified).date_naive())
}

/// 按本地日期轮转的写入器：当前文件固定，跨天时归档旧文件。
struct DailyFile {
    dir: PathBuf,
    /// 当前文件名（核心日志或 JSONL 事件日志）。
    name: &'static str,
    date: NaiveDate,
    /// 当前文件句柄；轮转时先置空关闭，Windows 才允许 rename 已打开的文件。
    file: Option<File>,
}

impl DailyFile {
    fn open(dir: &Path) -> io::Result<Self> {
        Self::open_named(dir, CORE_LOG_FILE)
    }

    fn open_named(dir: &Path, name: &'static str) -> io::Result<Self> {
        let date = Local::now().date_naive();
        let file = open_append(&dir.join(name))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            name,
            date,
            file: Some(file),
        })
    }

    /// 日期变化时把当前文件改名为 `<名字>.<旧日期>`，再重新打开当前文件。
    fn rotate_if_needed(&mut self) -> io::Result<()> {
        let today = Local::now().date_naive();
        if today == self.date {
            return Ok(());
        }
        let base = self.dir.join(self.name);
        let archived = self.dir.join(format!(
            "{}.{:04}-{:02}-{:02}",
            self.name,
            self.date.year(),
            self.date.month(),
            self.date.day()
        ));
        // Windows 不允许 rename 仍被打开的文件；先关闭再改名。
        self.file.take();
        if !archived.exists() {
            if let Err(error) = fs::rename(&base, &archived) {
                self.file = Some(open_append(&base)?);
                return Err(error);
            }
        } else {
            // 目标已存在（时钟回拨/同日重启）：把当前内容并入归档，避免丢失。
            let mut source = File::open(&base)?;
            let mut target = open_append(&archived)?;
            io::copy(&mut source, &mut target)?;
            target.flush()?;
            drop(source);
            fs::remove_file(&base)?;
        }
        self.file = Some(open_append(&base)?);
        self.date = today;
        Ok(())
    }
}

impl Write for DailyFile {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.rotate_if_needed()?;
        match self.file.as_mut() {
            Some(file) => file.write(buf),
            None => Err(io::Error::other("日志文件句柄未打开")),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(file) => file.flush(),
            None => Ok(()),
        }
    }
}

/// 结构化事件日志尾部（`read_event_log` 命令使用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventLogTail {
    pub path: PathBuf,
    pub lines: Vec<String>,
    pub truncated: bool,
    pub total_bytes: u64,
}

/// 读取 `<core>/events.jsonl` 的尾部；新一天当前文件为空时回退到最近归档。
///
/// `tail_lines` / `max_bytes` 由调用方钳制；返回的 `lines` 是原始 JSONL 行。
pub fn read_event_log_tail(
    dir: &Path,
    tail_lines: usize,
    max_bytes: usize,
) -> io::Result<EventLogTail> {
    let path = latest_event_file(dir)?;
    let total_bytes = fs::metadata(&path)?.len();
    let max_bytes = max_bytes.max(1) as u64;
    let limit = max_bytes.min(total_bytes);
    let mut file = File::open(&path)?;
    if limit > 0 {
        file.seek(SeekFrom::End(-(limit as i64)))?;
    }
    let mut bytes = Vec::with_capacity(limit as usize);
    file.take(limit).read_to_end(&mut bytes)?;
    let text = String::from_utf8_lossy(&bytes);
    let mut lines: Vec<String> = text.lines().map(str::to_owned).collect();
    let mut truncated = limit < total_bytes;
    if tail_lines > 0 && lines.len() > tail_lines {
        let skip = lines.len() - tail_lines;
        lines.drain(..skip);
        truncated = true;
    }
    Ok(EventLogTail {
        path,
        lines,
        truncated,
        total_bytes,
    })
}

/// 当前或最近一天的 `events.jsonl`。
fn latest_event_file(dir: &Path) -> io::Result<PathBuf> {
    let current = dir.join(EVENTS_LOG_FILE);
    if current.is_file() && fs::metadata(&current)?.len() > 0 {
        return Ok(current);
    }
    let prefix = format!("{EVENTS_LOG_FILE}.");
    let mut best: Option<(String, PathBuf)> = None;
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with(&prefix) {
            continue;
        }
        if best.as_ref().is_none_or(|(previous, _)| name > *previous) {
            best = Some((name, entry.path()));
        }
    }
    if let Some((_, path)) = best {
        return Ok(path);
    }
    if current.is_file() {
        return Ok(current);
    }
    Err(io::Error::new(
        io::ErrorKind::NotFound,
        format!("事件日志不存在：{}", current.display()),
    ))
}

/// JSONL 事件层：一行一个 tracing 事件，字段稳定、便于 agent/grep 查询。
struct JsonEventLayer {
    writer: Mutex<tracing_appender::non_blocking::NonBlocking>,
}

impl JsonEventLayer {
    fn new(writer: tracing_appender::non_blocking::NonBlocking) -> Self {
        Self {
            writer: Mutex::new(writer),
        }
    }
}

impl<S: Subscriber> Layer<S> for JsonEventLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let metadata = event.metadata();
        let mut visitor = JsonVisitor::default();
        event.record(&mut visitor);
        let value = event_json(
            Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true),
            metadata.level().as_str(),
            metadata.target(),
            visitor.message,
            visitor.fields,
        );
        if let Ok(mut writer) = self.writer.lock() {
            let _ = writeln!(writer, "{value}");
        }
    }
}

/// 构造一行 JSONL 事件。
///
/// - 固定字段：`ts` / `level` / `target` / `message` / `fields`（均保留，兼容
///   EventLogViewer 与 agent 查询）；
/// - `event` 从 tracing 字段中提取为顶层字段，用于稳定的事件命名
///   （如 `task.start`）；未声明 `event` 的日志点不输出该字段；
/// - `fields` 使用 `serde_json::Map`（默认 BTreeMap），键按字典序输出，保证
///   同一调用点的字段顺序稳定；
/// - `message` 永远是字符串（缺失时为空串），JSON 由 serde 保证单行转义。
fn event_json(
    ts: String,
    level: &str,
    target: &str,
    message: Option<String>,
    mut fields: serde_json::Map<String, serde_json::Value>,
) -> serde_json::Value {
    let event = fields.remove("event").map(|value| match value {
        serde_json::Value::String(text) => text,
        other => other.to_string(),
    });
    let mut object = serde_json::Map::new();
    object.insert("ts".to_owned(), serde_json::Value::String(ts));
    object.insert(
        "level".to_owned(),
        serde_json::Value::String(level.to_ascii_lowercase()),
    );
    object.insert(
        "target".to_owned(),
        serde_json::Value::String(target.to_owned()),
    );
    object.insert(
        "message".to_owned(),
        serde_json::Value::String(message.unwrap_or_default()),
    );
    object.insert(
        "fields".to_owned(),
        serde_json::Value::Object(std::mem::take(&mut fields)),
    );
    if let Some(event) = event {
        object.insert("event".to_owned(), serde_json::Value::String(event));
    }
    serde_json::Value::Object(object)
}

#[derive(Default)]
struct JsonVisitor {
    message: Option<String>,
    fields: serde_json::Map<String, serde_json::Value>,
}

impl JsonVisitor {
    fn record(&mut self, field: &Field, value: serde_json::Value) {
        if field.name() == "message" {
            self.message = match value {
                serde_json::Value::String(text) => Some(unquote_debug(text)),
                other => Some(other.to_string()),
            };
        } else {
            self.fields.insert(field.name().to_owned(), value);
        }
    }
}

impl Visit for JsonVisitor {
    fn record_f64(&mut self, field: &Field, value: f64) {
        self.record(field, serde_json::Value::from(value));
    }

    fn record_i64(&mut self, field: &Field, value: i64) {
        self.record(field, serde_json::Value::from(value));
    }

    fn record_u64(&mut self, field: &Field, value: u64) {
        self.record(field, serde_json::Value::from(value));
    }

    fn record_bool(&mut self, field: &Field, value: bool) {
        self.record(field, serde_json::Value::Bool(value));
    }

    fn record_str(&mut self, field: &Field, value: &str) {
        self.record(field, serde_json::Value::String(value.to_owned()));
    }

    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.record(field, serde_json::Value::String(format!("{value:?}")));
    }
}

/// tracing 的 `?` 值会带引号；消息字段去掉外层引号，便于阅读。
fn unquote_debug(text: String) -> String {
    if text.len() >= 2 && text.starts_with('"') && text.ends_with('"') {
        text[1..text.len() - 1].to_owned()
    } else {
        text
    }
}

fn open_append(path: &Path) -> io::Result<File> {
    OpenOptions::new().create(true).append(true).open(path)
}

/// 安装 panic hook：先写崩溃文件，再调用默认 hook（控制台/调试器行为不变）。
fn install_panic_hook(dir: Option<PathBuf>) {
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(dir) = &dir
            && let Err(error) = write_crash_log(dir, info)
        {
            eprintln!("[rook] 写崩溃日志失败：{error}");
        }
        default_hook(info);
    }));
}

/// 写 `<dir>/crash-<timestamp>.log`，返回文件路径。
fn write_crash_log(dir: &Path, info: &std::panic::PanicHookInfo<'_>) -> io::Result<PathBuf> {
    let payload = info.payload();
    let message = if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_owned()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "<非字符串 panic 负载>".to_owned()
    };
    let location = info
        .location()
        .map(|loc| format!("{}:{}:{}", loc.file(), loc.line(), loc.column()))
        .unwrap_or_else(|| "<未知位置>".to_owned());
    let thread = std::thread::current();
    let thread_name = thread.name().unwrap_or("<未命名线程>");

    let now = Local::now();
    let path = dir.join(format!("crash-{}.log", now.format("%Y%m%d-%H%M%S")));
    let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
    writeln!(file, "时间：{}", now.to_rfc3339())?;
    writeln!(file, "版本：{}", env!("CARGO_PKG_VERSION"))?;
    writeln!(file, "panic：{message}")?;
    writeln!(file, "位置：{location}")?;
    writeln!(file, "线程：{thread_name}")?;
    file.flush()?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn date(text: &str) -> NaiveDate {
        NaiveDate::parse_from_str(text, "%Y-%m-%d").expect("测试日期")
    }

    #[test]
    fn core_level_maps_to_tracing_levels() {
        assert_eq!(level_from_core(CoreLevel::Error), LevelFilter::ERROR);
        assert_eq!(level_from_core(CoreLevel::Warn), LevelFilter::WARN);
        assert_eq!(level_from_core(CoreLevel::Info), LevelFilter::INFO);
        assert_eq!(level_from_core(CoreLevel::Debug), LevelFilter::DEBUG);
        assert_eq!(level_from_core(CoreLevel::Trace), LevelFilter::TRACE);
        // 默认级别是 info。
        assert_eq!(
            level_from_core(CoreLevel::Info).to_string(),
            LevelFilter::INFO.to_string()
        );
    }

    #[test]
    fn cleanup_removes_only_expired_archives() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        for name in [
            "rook.log",
            "rook.log.2026-10-01",
            "rook.log.2026-10-18",
            "rook.log.2026-10-19",
            "rook.log.2026-11-02",
            "events.jsonl.2026-10-01",
            "events.jsonl.2026-11-01",
            "crash-20261001-120000.log",
            "notes.txt",
        ] {
            fs::write(dir.join(name), b"x").expect("写测试文件");
        }

        let removed = cleanup_old_logs(dir, 14, date("2026-11-02")).expect("清理");
        assert_eq!(
            removed, 4,
            "应删掉 10-01 的核心日志与 10-01 事件归档、10-18 核心日志、10-01 崩溃日志"
        );

        assert!(!dir.join("rook.log.2026-10-01").exists());
        assert!(!dir.join("rook.log.2026-10-18").exists());
        assert!(dir.join("rook.log.2026-10-19").exists());
        assert!(dir.join("rook.log.2026-11-02").exists());
        assert!(!dir.join("events.jsonl.2026-10-01").exists());
        assert!(dir.join("events.jsonl.2026-11-01").exists());
        // 当前文件与无关文件保留；过期崩溃日志按文件名日期删除。
        assert!(dir.join("rook.log").exists());
        assert!(!dir.join("crash-20261001-120000.log").exists());
        assert!(dir.join("notes.txt").exists());
    }

    #[test]
    fn cleanup_keeps_everything_within_retention() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        fs::write(dir.join("rook.log.2026-11-01"), b"x").expect("写测试文件");

        let removed = cleanup_old_logs(dir, 14, date("2026-11-02")).expect("清理");
        assert_eq!(removed, 0);
        assert!(dir.join("rook.log.2026-11-01").exists());
    }

    #[test]
    fn default_retention_is_seven_days_and_keeps_current_files() {
        assert_eq!(DEFAULT_RETENTION_DAYS, 7);
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        // 2026-11-05 往前 7 天的截止日是 2026-10-29：10-28 删除、10-29 保留。
        for name in [
            "rook.log",
            "rook.log.2026-10-28",
            "rook.log.2026-10-29",
            "events.jsonl",
            "events.jsonl.2026-10-28",
            "events.jsonl.2026-10-29",
            "crash-20261028-120000.log",
        ] {
            fs::write(dir.join(name), b"x").expect("写测试文件");
        }

        let removed =
            cleanup_old_logs(dir, DEFAULT_RETENTION_DAYS, date("2026-11-05")).expect("清理");
        assert_eq!(removed, 3, "应删除 10-28 的核心日志、事件归档与崩溃日志");
        assert!(!dir.join("rook.log.2026-10-28").exists());
        assert!(!dir.join("events.jsonl.2026-10-28").exists());
        assert!(dir.join("rook.log.2026-10-29").exists());
        assert!(dir.join("events.jsonl.2026-10-29").exists());
        // current.log / events.jsonl 永不参与日期清理；8 天前的崩溃日志被删。
        assert!(dir.join("rook.log").exists());
        assert!(dir.join("events.jsonl").exists());
        assert!(!dir.join("crash-20261028-120000.log").exists());
    }

    #[test]
    fn zero_retention_disables_date_cleanup() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        for name in [
            "rook.log.2026-01-01",
            "events.jsonl.2026-01-01",
            "crash-20260101-120000.log",
        ] {
            fs::write(dir.join(name), b"x").expect("写测试文件");
        }

        let removed = cleanup_old_logs(dir, 0, date("2026-11-05")).expect("清理");
        assert_eq!(removed, 0, "retention_days=0 不按日期删除");
        for name in [
            "rook.log.2026-01-01",
            "events.jsonl.2026-01-01",
            "crash-20260101-120000.log",
        ] {
            assert!(dir.join(name).exists(), "{name} 应保留");
        }
    }

    #[test]
    fn crash_logs_use_name_or_mtime_and_keep_unknown_files() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        for name in [
            "crash-20261028-120000.log", // 8 天前：删
            "crash-20261029-120000.log", // 7 天内：留
            "crash-manual.log",          // 名字无法解析，mtime 30 天前：删
            "notes.txt",
        ] {
            fs::write(dir.join(name), b"x").expect("写测试文件");
        }
        let thirty_days = std::time::Duration::from_secs(30 * 24 * 60 * 60);
        let file = fs::OpenOptions::new()
            .write(true)
            .open(dir.join("crash-manual.log"))
            .expect("打开崩溃日志");
        file.set_modified(std::time::SystemTime::now() - thirty_days)
            .expect("设置 mtime");

        let removed =
            cleanup_old_logs(dir, DEFAULT_RETENTION_DAYS, date("2026-11-05")).expect("清理");
        assert_eq!(removed, 2, "按文件名日期与 mtime 各删一个");
        assert!(!dir.join("crash-20261028-120000.log").exists());
        assert!(dir.join("crash-20261029-120000.log").exists());
        assert!(!dir.join("crash-manual.log").exists());
        assert!(dir.join("notes.txt").exists(), "无法识别的文件不删");
    }

    #[test]
    fn archive_date_only_accepts_rotated_core_logs() {
        assert_eq!(
            archive_date("rook.log.2026-11-02"),
            Some(date("2026-11-02"))
        );
        assert_eq!(archive_date("rook.log"), None);
        assert_eq!(archive_date("crash-20261102-120000.log"), None);
        assert_eq!(archive_date("rook.log.2026-11"), None);
        assert_eq!(archive_date("other.log.2026-11-02"), None);
    }

    #[test]
    fn daily_file_writes_to_stable_path() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let mut file = DailyFile::open(tmp.path()).expect("打开日志");
        file.write_all(b"hello\n").expect("写入");
        file.flush().expect("刷盘");
        drop(file);

        let content = fs::read_to_string(tmp.path().join(CORE_LOG_FILE)).expect("读取日志");
        assert_eq!(content, "hello\n");
    }

    #[test]
    fn daily_file_rotates_and_archives_old_content() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let mut file = DailyFile::open(tmp.path()).expect("打开日志");
        file.write_all(b"old\n").expect("写入旧日志");
        file.flush().expect("刷盘");

        let yesterday = Local::now()
            .date_naive()
            .checked_sub_days(Days::new(1))
            .expect("昨天");
        file.date = yesterday;
        file.write_all(b"new\n").expect("跨天写入");
        file.flush().expect("刷盘");
        drop(file);

        let archive = tmp
            .path()
            .join(format!("{CORE_LOG_FILE}.{}", yesterday.format("%Y-%m-%d")));
        assert_eq!(
            fs::read_to_string(archive).expect("读取归档"),
            "old\n",
            "跨天前的日志应归档"
        );
        assert_eq!(
            fs::read_to_string(tmp.path().join(CORE_LOG_FILE)).expect("读取当前日志"),
            "new\n",
            "当前文件只保留跨天后的日志"
        );
    }

    #[test]
    fn panic_hook_writes_crash_file() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let previous = std::panic::take_hook();
        install_panic_hook(Some(tmp.path().to_path_buf()));
        let panicked = std::panic::catch_unwind(|| panic!("测试崩溃"));
        std::panic::set_hook(previous);
        assert!(panicked.is_err(), "catch_unwind 应捕获 panic");

        let crash = fs::read_dir(tmp.path())
            .expect("读取目录")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name()
                    .is_some_and(|name| name.to_string_lossy().starts_with("crash-"))
            })
            .expect("应写入崩溃日志");
        let content = fs::read_to_string(crash).expect("读取崩溃日志");
        assert!(content.contains("测试崩溃"), "{content}");
        assert!(content.contains(env!("CARGO_PKG_VERSION")), "{content}");
        assert!(content.contains("logging.rs"), "{content}");
    }
}

#[cfg(test)]
mod event_log_tests {
    use super::*;

    #[test]
    fn reads_tail_lines_and_marks_truncated() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        let mut text = String::new();
        for index in 0..20 {
            text.push_str(&format!("{{\"seq\":{index}}}\n"));
        }
        fs::write(dir.join(EVENTS_LOG_FILE), text).expect("写事件日志");

        let tail = read_event_log_tail(dir, 5, 1024 * 1024).expect("读尾部");
        assert_eq!(tail.lines.len(), 5);
        assert_eq!(tail.lines[4], "{\"seq\":19}");
        assert!(tail.truncated);
        assert_eq!(tail.path, dir.join(EVENTS_LOG_FILE));
    }

    #[test]
    fn falls_back_to_latest_archive_when_current_empty() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let dir = tmp.path();
        fs::write(dir.join(EVENTS_LOG_FILE), b"").expect("空当前文件");
        fs::write(
            dir.join(format!("{EVENTS_LOG_FILE}.2026-11-03")),
            b"{\"old\":1}\n",
        )
        .expect("归档");
        fs::write(
            dir.join(format!("{EVENTS_LOG_FILE}.2026-11-04")),
            b"{\"new\":1}\n",
        )
        .expect("归档");
        let tail = read_event_log_tail(dir, 10, 1024).expect("读尾部");
        assert_eq!(tail.lines, vec!["{\"new\":1}".to_owned()]);
    }

    #[test]
    fn missing_event_log_reports_not_found() {
        let tmp = tempfile::tempdir().expect("临时目录");
        let error = read_event_log_tail(tmp.path(), 10, 1024).expect_err("应当失败");
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn event_json_has_stable_fields_and_top_level_event() {
        let mut fields = serde_json::Map::new();
        fields.insert("task".to_owned(), serde_json::json!("ykt-gateway"));
        fields.insert("event".to_owned(), serde_json::json!("task.start"));
        fields.insert("pid".to_owned(), serde_json::json!(4242));
        let value = event_json(
            "2026-11-05T04:40:00.123Z".to_owned(),
            "INFO",
            "rook::engine",
            Some("任务进程已启动".to_owned()),
            fields,
        );
        let line = value.to_string();

        // 单行、合法 JSON，固定字段齐全。
        assert!(!line.contains('\n'), "{line}");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("合法 JSON");
        assert_eq!(parsed["ts"], "2026-11-05T04:40:00.123Z");
        assert_eq!(parsed["level"], "info");
        assert_eq!(parsed["target"], "rook::engine");
        assert_eq!(parsed["message"], "任务进程已启动");
        assert_eq!(parsed["event"], "task.start");
        // event 从 fields 提到顶层，不重复；其余字段保留。
        assert!(parsed["fields"].get("event").is_none(), "{parsed}");
        assert_eq!(parsed["fields"]["task"], "ykt-gateway");
        assert_eq!(parsed["fields"]["pid"], 4242);
        // 键顺序稳定（serde_json 默认 BTreeMap：字典序）。
        assert!(
            line.starts_with(r#"{"event":"task.start","fields":{"pid":4242,"task":"ykt-gateway"}"#),
            "{line}"
        );

        // 消息/字段中的换行必须被 JSON 转义，仍保持单行。
        let mut fields = serde_json::Map::new();
        fields.insert("detail".to_owned(), serde_json::json!("第一行\n第二行"));
        let value = event_json(
            "2026-11-05T04:40:00.123Z".to_owned(),
            "warn",
            "rook::engine",
            Some("多行\n消息".to_owned()),
            fields,
        );
        let line = value.to_string();
        assert!(!line.contains('\n'), "{line}");
        let parsed: serde_json::Value = serde_json::from_str(&line).expect("合法 JSON");
        assert_eq!(parsed["message"], "多行\n消息");
        assert_eq!(parsed["fields"]["detail"], "第一行\n第二行");
    }

    #[test]
    fn event_json_without_event_keeps_compatible_shape() {
        let value = event_json(
            "2026-11-05T04:40:00.000Z".to_owned(),
            "warn",
            "rook::tray",
            None,
            serde_json::Map::new(),
        );
        let parsed: serde_json::Value = serde_json::from_str(&value.to_string()).expect("JSON");
        assert!(parsed.get("event").is_none());
        assert_eq!(parsed["message"], "");
        assert_eq!(parsed["fields"], serde_json::json!({}));
    }
}
