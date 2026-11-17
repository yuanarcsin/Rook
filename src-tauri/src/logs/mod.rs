//! M2-3：任务日志采集（管道原始字节 → `current.log`）与轮转/保留/降级。
//!
//! 目标是为 M2-4 的 Engine 接线提供**不依赖 Tauri** 的日志采集器：
//!
//! - 每个实例两条管道（stdout/stderr）各一个读线程，原始字节直接落盘，
//!   不转码、不加时间戳/级别/流前缀（v1 契约，见 docs/07）；
//! - stdout/stderr 合并写同一个 `current.log`，由**单一写入线程**串行化，
//!   顺序即到达顺序；
//! - 支持 `size` / `daily` / `none` 三种轮转，`max_files` / `max_days` 清理，
//!   `current.log` 永不删除；
//! - 写失败不阻塞子进程：读线程继续排空管道，丢弃字节与 IO 错误计入统计；
//! - 子进程退出（管道 EOF）后读线程结束、写入线程刷盘关闭，`join` 不丢已读数据；
//!   调用方提前 drop 句柄时通知读线程退出并做有界 join，避免线程泄漏。
//!
//! # 线程模型
//!
//! ```text
//! stdout 管道 ─► 读线程 ─┐
//!                       ├─► 有界通道（满了就丢弃并计数）─► 写入线程 ─► current.log
//! stderr 管道 ─► 读线程 ─┘
//! ```
//!
//! 读线程用 `PeekNamedPipe` 轮询（见 [`collector`]），因此不会阻塞在 `ReadFile` 上，
//! drop 时能在 [`LogParams::poll_interval`] 内退出并关闭读端。
//!
//! # 未覆盖
//!
//! 不做搜索/编码切换/完整查看器（M4-1 只提供 `current.log` 尾部只读视图）；
//! 全局总量 `max_total_mb` 见 [`cleanup_total_size`]（只清归档，不删活动文件）。

mod collector;
mod reader;
mod writer;

#[cfg(test)]
mod tests;

pub use collector::LogCollector;
pub use reader::{
    DEFAULT_TAIL_LINES, LogFileEntry, LogTail, MAX_TAIL_BYTES, MAX_TAIL_LINES, current_log_path,
    is_log_file_name, list_log_files, normalize_limits, read_tail,
};
pub use writer::{CURRENT_LOG, cleanup_archives, cleanup_expired_archives};

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use self::writer::is_archive_name;
use crate::config::{Rotation, TaskLogsConfig};
use crate::logging;

/// 每次从管道读取并交给写入线程的最大字节数。
pub const DEFAULT_CHUNK_BYTES: usize = 64 * 1024;
/// 读线程与写入线程之间的有界队列长度（按块计）。
pub const DEFAULT_QUEUE_CHUNKS: usize = 64;
/// 空闲时 `PeekNamedPipe` 的轮询间隔；也是 drop 后读线程退出的上限延迟。
pub const DEFAULT_POLL_INTERVAL: Duration = Duration::from_millis(10);
/// 写失败后重开文件/重试轮转的最小间隔（docs/07：目录重建一次/分钟）。
pub const DEFAULT_REOPEN_INTERVAL: Duration = Duration::from_secs(60);
/// Drop 时等待读/写线程结束的上限；超过后放弃 join，不无限阻塞调用方。
pub const DEFAULT_STOP_TIMEOUT: Duration = Duration::from_secs(3);

/// 默认任务日志保留天数（与 `[tasks.logs].max_days` 默认值一致）。
pub const DEFAULT_RETENTION_DAYS: u32 = 7;

/// 一个任务日志采集器的参数。
///
/// 生产路径用 [`LogParams::from_config`] 从 `TaskLogsConfig` 派生；测试可以调小
/// `max_file_bytes` / `queue_chunks` 等阈值，避免生成超大文件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogParams {
    /// 已解析的绝对目录（`<data>/logs/<task_id>` 或配置覆盖值）。
    pub dir: PathBuf,
    pub rotation: Rotation,
    /// `size` 轮转阈值；`0` 表示不按大小轮转。
    pub max_file_bytes: u64,
    /// `size` 轮转下允许的文件总数（含 `current.log`）。
    pub max_files: u32,
    /// 归档保留天数（按 mtime 清理）。
    pub max_days: u32,
    /// 单次读取的块大小。
    pub chunk_bytes: usize,
    /// 读→写队列容量（块数）；队列满时丢弃并计数，保证子进程不被反压。
    pub queue_chunks: usize,
    /// 空闲轮询间隔。
    pub poll_interval: Duration,
    /// 写失败后重开文件/重试轮转的节流间隔。
    pub reopen_interval: Duration,
    /// Drop 时等待线程结束的上限。
    pub stop_timeout: Duration,
}

impl LogParams {
    /// 用默认采集参数指定日志目录。
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self {
            dir: dir.into(),
            rotation: Rotation::Size,
            max_file_bytes: 10 * 1024 * 1024,
            max_files: 5,
            max_days: DEFAULT_RETENTION_DAYS,
            chunk_bytes: DEFAULT_CHUNK_BYTES,
            queue_chunks: DEFAULT_QUEUE_CHUNKS,
            poll_interval: DEFAULT_POLL_INTERVAL,
            reopen_interval: DEFAULT_REOPEN_INTERVAL,
            stop_timeout: DEFAULT_STOP_TIMEOUT,
        }
    }

    /// 按 `[tasks.logs]` 的语义派生参数。
    ///
    /// `encoding` 不参与采集：v1 契约是原样字节，解码由查看器负责。
    pub fn from_config(logs: &TaskLogsConfig) -> Self {
        Self {
            dir: logs.dir.clone(),
            rotation: logs.rotation,
            max_file_bytes: u64::from(logs.max_file_mb) * 1024 * 1024,
            max_files: logs.max_files,
            max_days: logs.max_days,
            ..Self::new(&logs.dir)
        }
    }

    /// 当前活动文件路径（`<dir>/current.log`）。
    pub fn current_path(&self) -> PathBuf {
        self.dir.join(CURRENT_LOG)
    }

    /// 日志目录。
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

/// 批量清理多个任务日志目录的过期归档（启动时与每日全局清理使用）。
///
/// `dirs` 的保留天数由调用方按任务配置决定（`[tasks.logs].max_days`）；目录不存在、
/// 无权限等失败只记录警告并继续，绝不阻塞监督或启动。
///
/// 返回删除的归档文件总数。
pub fn cleanup_task_logs(dirs: &[(PathBuf, u32)]) -> usize {
    let mut removed = 0usize;
    for (dir, retention_days) in dirs {
        match cleanup_expired_archives(dir, *retention_days) {
            Ok(0) => {}
            Ok(count) => {
                removed += count;
                tracing::info!(
                    event = "logs.cleanup",
                    scope = "task",
                    dir = %dir.display(),
                    removed = count,
                    retention_days = *retention_days,
                    "清理过期任务日志归档"
                );
            }
            Err(error) => tracing::warn!(
                dir = %dir.display(),
                %error,
                "清理任务日志归档失败（不影响任务运行）"
            ),
        }
    }
    removed
}

/// 全局总量清理结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TotalCleanup {
    /// 清理后 `<data>/core` + `<data>/logs/*/` 的非活动文件总字节数。
    pub total_bytes: u64,
    /// 本次删除的字节数。
    pub removed_bytes: u64,
    /// 本次删除的文件数。
    pub removed: usize,
}

/// 执行 `[logs].max_total_mb` 全局总量上限：按 mtime 最旧优先删除归档。
///
/// 统计范围是 `<data>/core` 与 `<data>/logs/*` 下的所有非活动文件；以下文件
/// 既不计入总量、也不删除：
///
/// - `core/rook.log`、`core/events.jsonl`（当前正在写）；
/// - 任务目录里的 `current.log`；
/// - 脚本自建/无法识别的文件（`notes.txt`、`*.json` 等）会计入总量，但永不删除。
///
/// 可删的是本模块与核心日志认可的归档：`rook.log.YYYY-MM-DD`、
/// `events.jsonl.YYYY-MM-DD`、`crash-*.log`、任务日志 `YYYYMMDD-*.log` /
/// `YYYYMMDD.log`。`limit_mb == 0` 表示不限制（与配置校验语义一致）。
///
/// 失败只影响本次清理：目录不可读返回 `Err` 由调用方 warn；单个文件删除失败
/// 记录警告并继续。删除到总量 ≤ 上限或没有可删归档为止。
pub fn cleanup_total_size(data_dir: &Path, limit_mb: u32) -> io::Result<TotalCleanup> {
    if limit_mb == 0 {
        return Ok(TotalCleanup::default());
    }
    let limit = u64::from(limit_mb) * 1024 * 1024;
    let mut total = 0u64;
    let mut removable: Vec<(PathBuf, u64, Option<SystemTime>)> = Vec::new();

    let core = data_dir.join(logging::CORE_DIR);
    if core.is_dir() {
        scan_total_dir(
            &core,
            |name| {
                if name == logging::CORE_LOG_FILE || name == logging::EVENTS_LOG_FILE {
                    None
                } else if logging::archive_date(name).is_some()
                    || logging::archive_date_named(name, logging::EVENTS_LOG_FILE).is_some()
                    || logging::is_crash_log(name)
                {
                    Some(true)
                } else {
                    Some(false)
                }
            },
            &mut total,
            &mut removable,
        )?;
    }

    let logs_root = data_dir.join("logs");
    if logs_root.is_dir() {
        for entry in fs::read_dir(&logs_root)? {
            let Ok(entry) = entry else { continue };
            if !entry.file_type().is_ok_and(|kind| kind.is_dir()) {
                continue;
            }
            scan_total_dir(
                &entry.path(),
                |name| {
                    if name == CURRENT_LOG {
                        None
                    } else if is_archive_name(name) {
                        Some(true)
                    } else {
                        Some(false)
                    }
                },
                &mut total,
                &mut removable,
            )?;
        }
    }

    let mut result = TotalCleanup {
        total_bytes: total,
        ..TotalCleanup::default()
    };
    if total <= limit {
        return Ok(result);
    }

    // mtime 升序（最旧优先）；mtime 不可用的按最旧处理，文件名兜底保证稳定。
    removable.sort_by(|left, right| {
        left.2
            .unwrap_or(UNIX_EPOCH)
            .cmp(&right.2.unwrap_or(UNIX_EPOCH))
            .then_with(|| left.0.cmp(&right.0))
    });

    for (path, size, _) in removable {
        if total <= limit {
            break;
        }
        match fs::remove_file(&path) {
            Ok(()) => {
                total = total.saturating_sub(size);
                result.removed += 1;
                result.removed_bytes += size;
            }
            // 并发清理（每日清理 vs 写入线程轮转）已删掉：按已删除计入总量。
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                total = total.saturating_sub(size);
            }
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "删除超限日志归档失败");
            }
        }
    }
    result.total_bytes = total;

    if result.removed > 0 {
        tracing::info!(
            event = "logs.cleanup",
            scope = "total",
            limit_mb,
            removed = result.removed,
            removed_bytes = result.removed_bytes,
            total_bytes = result.total_bytes,
            "日志总量超过上限，已按最旧优先删除归档"
        );
    } else if total > limit {
        tracing::warn!(
            limit_mb,
            total_bytes = total,
            "日志总量仍超过上限，但没有可删除的归档"
        );
    }
    Ok(result)
}

/// 扫描单个目录：`classify` 返回 `None` 表示活动文件（跳过），`Some(true)` 表示
/// 可删除归档，`Some(false)` 表示只计入总量、不可删除。
fn scan_total_dir(
    dir: &Path,
    classify: impl Fn(&str) -> Option<bool>,
    total: &mut u64,
    removable: &mut Vec<(PathBuf, u64, Option<SystemTime>)>,
) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        match classify(&name) {
            None => {}
            Some(deletable) => {
                let size = metadata.len();
                *total += size;
                if deletable {
                    removable.push((entry.path(), size, metadata.modified().ok()));
                }
            }
        }
    }
    Ok(())
}

/// 采集统计快照（只增不减）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct LogStats {
    /// 从管道读到的总字节数。
    pub read_bytes: u64,
    /// 成功写入文件的总字节数。
    pub written_bytes: u64,
    /// 已读但未落盘的字节数（写失败 + 队列满）。
    pub dropped_bytes: u64,
    /// 文件打开/写入/重命名失败次数。
    pub io_errors: u64,
    /// 管道读取/查询失败次数。
    pub read_errors: u64,
    /// 成功轮转次数。
    pub rotations: u64,
    /// 因队列满被丢弃的块数。
    pub queue_drops: u64,
}

/// 线程间共享的原子计数。
#[derive(Default)]
pub(crate) struct Counters {
    read_bytes: AtomicU64,
    written_bytes: AtomicU64,
    dropped_bytes: AtomicU64,
    io_errors: AtomicU64,
    read_errors: AtomicU64,
    rotations: AtomicU64,
    queue_drops: AtomicU64,
}

impl Counters {
    pub(crate) fn add_read(&self, bytes: usize) {
        self.read_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_written(&self, bytes: usize) {
        self.written_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_dropped(&self, bytes: usize) {
        self.dropped_bytes
            .fetch_add(bytes as u64, Ordering::Relaxed);
    }

    pub(crate) fn add_io_error(&self) {
        self.io_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_read_error(&self) {
        self.read_errors.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_rotation(&self) {
        self.rotations.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn add_queue_drop(&self) {
        self.queue_drops.fetch_add(1, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> LogStats {
        LogStats {
            read_bytes: self.read_bytes.load(Ordering::Relaxed),
            written_bytes: self.written_bytes.load(Ordering::Relaxed),
            dropped_bytes: self.dropped_bytes.load(Ordering::Relaxed),
            io_errors: self.io_errors.load(Ordering::Relaxed),
            read_errors: self.read_errors.load(Ordering::Relaxed),
            rotations: self.rotations.load(Ordering::Relaxed),
            queue_drops: self.queue_drops.load(Ordering::Relaxed),
        }
    }
}

/// 启动采集器失败的原因；运行期的写失败只降级为统计，不会返回错误。
#[derive(Debug)]
pub enum LogError {
    /// stdout/stderr 都没有可采集的读端。
    NoSources,
    /// 日志目录创建失败（路径不可写、父路径是文件等）。
    CreateDir {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 读/写线程创建失败。
    SpawnThread {
        role: &'static str,
        source: std::io::Error,
    },
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSources => write!(f, "没有可采集的管道：stdout 与 stderr 都为空"),
            Self::CreateDir { path, source } => {
                write!(f, "创建任务日志目录 {} 失败：{source}", path.display())
            }
            Self::SpawnThread { role, source } => {
                write!(f, "创建日志{role}线程失败：{source}")
            }
        }
    }
}

impl std::error::Error for LogError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::NoSources => None,
            Self::CreateDir { source, .. } | Self::SpawnThread { source, .. } => Some(source),
        }
    }
}
