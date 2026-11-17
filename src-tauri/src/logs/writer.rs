//! 单写入线程的文件层：追加、`size`/`daily` 轮转、保留清理、写失败降级。
//!
//! 所有方法都在写入线程内调用，不需要额外加锁；读线程只通过通道提交字节块。
//!
//! # 不丢字节的约定
//!
//! - 轮转前先 `flush` + 关闭句柄（Windows 不允许重命名仍被打开的文件）；
//! - 重命名失败时重新打开 `current.log` 继续写，已写内容仍在原文件里；
//! - 写失败只关闭句柄并按 [`LogParams::reopen_interval`] 节流重开，期间丢弃的
//!   字节计入 `dropped_bytes`，写入线程本身不退出。

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use chrono::{Local, NaiveDate, Utc};

use super::{Counters, LogParams};
use crate::config::Rotation;

/// 当前活动文件名。
pub const CURRENT_LOG: &str = "current.log";

const SECONDS_PER_DAY: u64 = 86_400;

/// 一次轮转产生的归档类型（决定命名规则）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Size,
    Daily,
}

/// 任务日志的写入器。
pub(crate) struct Writer {
    params: LogParams,
    counters: Arc<Counters>,
    /// 当前文件句柄；`None` 表示降级状态（写失败/打开失败），等待节流后重开。
    file: Option<File>,
    /// 当前文件已写字节数（用于 size 轮转）。
    current_bytes: u64,
    /// 当前文件所属的本地日期（用于 daily 轮转）。
    date: NaiveDate,
    /// 上次打开失败/写失败的时间，用于节流重开。
    last_reopen: Option<Instant>,
    /// 上次轮转失败的时间，避免每次写入都重试。
    last_rotation_failure: Option<Instant>,
}

impl Writer {
    /// 打开（或创建）`current.log` 并执行一次保留清理。
    pub(crate) fn new(params: LogParams, counters: Arc<Counters>) -> Self {
        let mut writer = Self {
            params,
            counters,
            file: None,
            current_bytes: 0,
            date: Local::now().date_naive(),
            last_reopen: None,
            last_rotation_failure: None,
        };
        writer.open_current();
        writer.cleanup();
        writer
    }

    /// 写入一块原始字节。任何 IO 失败都只降级，不向调用方返回错误。
    pub(crate) fn write_chunk(&mut self, chunk: &[u8]) {
        if chunk.is_empty() {
            return;
        }
        self.ensure_open();

        if self.params.rotation == Rotation::Daily
            && self.file.is_some()
            && Local::now().date_naive() != self.date
        {
            self.rotate_daily();
        }

        if self.file.is_none() {
            self.counters.add_dropped(chunk.len());
            return;
        }

        let mut offset = 0usize;
        let mut failed = false;
        while offset < chunk.len() {
            let result = match self.file.as_mut() {
                Some(file) => file.write(&chunk[offset..]),
                None => {
                    failed = true;
                    break;
                }
            };
            match result {
                Ok(0) => {
                    failed = true;
                    break;
                }
                Ok(written) => {
                    offset += written;
                    self.current_bytes += written as u64;
                    self.counters.add_written(written);
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(_) => {
                    failed = true;
                    break;
                }
            }
        }

        if failed {
            let remaining = chunk.len() - offset;
            if remaining > 0 {
                self.counters.add_dropped(remaining);
            }
            self.counters.add_io_error();
            // 关闭句柄，等待节流后重开；期间继续排空管道并计数。
            self.file = None;
            self.last_reopen = Some(Instant::now());
        }

        if self.file.is_some()
            && self.params.rotation == Rotation::Size
            && self.params.max_file_bytes > 0
            && self.current_bytes >= self.params.max_file_bytes
        {
            self.rotate_size();
        }
    }

    /// 刷盘并关闭文件；写入线程退出前调用。
    pub(crate) fn finish(&mut self) {
        if let Some(mut file) = self.file.take() {
            let _ = file.flush();
        }
    }

    /// 测试辅助：伪造当前文件所属日期，验证跨天轮转。
    #[cfg(test)]
    pub(crate) fn force_date(&mut self, date: NaiveDate) {
        self.date = date;
    }

    fn current_path(&self) -> PathBuf {
        self.params.current_path()
    }

    /// 打开 `current.log`（追加）；失败时进入降级状态并计数。
    fn open_current(&mut self) {
        let path = self.current_path();
        match OpenOptions::new().create(true).append(true).open(&path) {
            Ok(file) => {
                // 上次运行留下的内容也计入 size 阈值。
                self.current_bytes = file.metadata().map(|meta| meta.len()).unwrap_or(0);
                self.date = Local::now().date_naive();
                self.file = Some(file);
                self.last_reopen = None;
            }
            Err(error) => {
                self.counters.add_io_error();
                self.file = None;
                self.last_reopen = Some(Instant::now());
                tracing::warn!(
                    path = %path.display(),
                    %error,
                    "打开任务日志文件失败，日志写入降级"
                );
            }
        }
    }

    /// 降级状态下按节流间隔尝试重开；目录可能被外部删除，顺带尝试重建。
    fn ensure_open(&mut self) {
        if self.file.is_some() {
            return;
        }
        if self
            .last_reopen
            .is_some_and(|at| at.elapsed() < self.params.reopen_interval)
        {
            return;
        }
        let _ = fs::create_dir_all(&self.params.dir);
        self.open_current();
    }

    fn rotation_throttled(&self) -> bool {
        self.last_rotation_failure
            .is_some_and(|at| at.elapsed() < self.params.reopen_interval)
    }

    /// `size` 轮转：归档 `current.log` 后重开新文件。
    fn rotate_size(&mut self) {
        if self.current_bytes == 0 || self.rotation_throttled() {
            return;
        }
        let Some(file) = self.file.take() else {
            return;
        };
        // 先刷盘再关闭：Windows 上必须先释放句柄才能 rename。
        let mut file = file;
        let _ = file.flush();
        drop(file);

        let current = self.current_path();
        let archive = self.unique_archive_path(ArchiveKind::Size);
        if let Err(error) = fs::rename(&current, &archive) {
            self.counters.add_io_error();
            self.last_rotation_failure = Some(Instant::now());
            tracing::warn!(
                current = %current.display(),
                archive = %archive.display(),
                %error,
                "任务日志轮转失败，继续写 current.log"
            );
            // 保留旧文件继续写，字节不丢。
            self.open_current();
            return;
        }

        self.counters.add_rotation();
        self.current_bytes = 0;
        self.open_current();
        self.cleanup();
    }

    /// `daily` 轮转：本地日期变化时归档旧文件。
    fn rotate_daily(&mut self) {
        if self.current_bytes == 0 {
            // 空文件直接跟随新日期，不产生空归档。
            self.date = Local::now().date_naive();
            return;
        }
        if self.rotation_throttled() {
            return;
        }
        let Some(file) = self.file.take() else {
            return;
        };
        let mut file = file;
        let _ = file.flush();
        drop(file);

        let current = self.current_path();
        let archive = self.unique_archive_path(ArchiveKind::Daily);
        if let Err(error) = fs::rename(&current, &archive) {
            self.counters.add_io_error();
            self.last_rotation_failure = Some(Instant::now());
            tracing::warn!(
                current = %current.display(),
                archive = %archive.display(),
                %error,
                "任务日志跨天轮转失败，继续写 current.log"
            );
            self.open_current();
            return;
        }

        self.counters.add_rotation();
        self.current_bytes = 0;
        self.open_current();
        self.cleanup();
    }

    /// 生成不与现有文件冲突的归档路径。
    fn unique_archive_path(&self, kind: ArchiveKind) -> PathBuf {
        match kind {
            ArchiveKind::Size => {
                let base = Utc::now().format("%Y%m%d-%H%M%S").to_string();
                for seq in 1..=999u32 {
                    let path = self.params.dir.join(format!("{base}-{seq:03}.log"));
                    if !path.exists() {
                        return path;
                    }
                }
                // 同一秒轮转超过 999 次（极端情况）：退化为毫秒时间戳。
                self.params
                    .dir
                    .join(format!("{base}-{}.log", Utc::now().timestamp_millis()))
            }
            ArchiveKind::Daily => {
                let base = self.date.format("%Y%m%d").to_string();
                let path = self.params.dir.join(format!("{base}.log"));
                if !path.exists() {
                    return path;
                }
                for seq in 1..=999u32 {
                    let path = self.params.dir.join(format!("{base}-{seq:03}.log"));
                    if !path.exists() {
                        return path;
                    }
                }
                self.params
                    .dir
                    .join(format!("{base}-{}.log", Utc::now().timestamp_millis()))
            }
        }
    }

    /// 按配置清理归档；失败只记录，不影响写入。
    fn cleanup(&self) {
        if let Err(error) = cleanup_archives(
            &self.params.dir,
            self.params.rotation,
            self.params.max_files,
            self.params.max_days,
        ) {
            tracing::warn!(
                dir = %self.params.dir.display(),
                %error,
                "清理任务日志归档失败"
            );
        }
    }
}

/// 清理任务日志目录中的归档。
///
/// - `size` 模式按 `max_files`（含 `current.log`）删除最旧归档；
/// - 所有模式都按 `max_days` 与 mtime 删除过期归档；
/// - `current.log`、无法识别的文件（脚本自建等）永不删除。
///
/// 返回删除的文件数；单个文件删除失败只记录警告并继续。
pub fn cleanup_archives(
    dir: &Path,
    rotation: Rotation,
    max_files: u32,
    max_days: u32,
) -> io::Result<usize> {
    let now = SystemTime::now();
    let mut archives = collect_archives(dir)?;
    // 归档名带 UTC 时间戳，字典序即时间序。
    archives.sort_by(|left, right| left.0.cmp(&right.0));

    let mut removed = 0usize;
    if rotation == Rotation::Size {
        // max_files 含 current.log，归档最多保留 max_files - 1 个。
        let keep = max_files.saturating_sub(1) as usize;
        if archives.len() > keep {
            let excess = archives.len() - keep;
            for (_, path, _) in archives.drain(..excess) {
                if remove_archive(&path) {
                    removed += 1;
                }
            }
        }
    }

    if max_days > 0 {
        let retention = Duration::from_secs(u64::from(max_days) * SECONDS_PER_DAY);
        let cutoff = now.checked_sub(retention).unwrap_or(UNIX_EPOCH);
        for (_, path, modified) in &archives {
            if modified.is_some_and(|modified| modified < cutoff) && remove_archive(path) {
                removed += 1;
            }
        }
    }

    Ok(removed)
}

/// 只按保留天数清理任务日志归档（全局每日清理使用）。
///
/// 与 [`cleanup_archives`] 的区别：不执行 `max_files` 配额，只按归档文件的
/// mtime 删除超过 `retention_days` 的文件；`current.log`、脚本自建文件保留。
/// `retention_days == 0` 表示不做年龄清理（与 `[tasks.logs].max_days` 语义一致）。
pub fn cleanup_expired_archives(dir: &Path, retention_days: u32) -> io::Result<usize> {
    if retention_days == 0 {
        return Ok(0);
    }
    let cutoff = SystemTime::now()
        .checked_sub(Duration::from_secs(
            u64::from(retention_days) * SECONDS_PER_DAY,
        ))
        .unwrap_or(UNIX_EPOCH);
    let mut removed = 0usize;
    for (_, path, modified) in collect_archives(dir)? {
        if modified.is_some_and(|modified| modified < cutoff) && remove_archive(&path) {
            removed += 1;
        }
    }
    Ok(removed)
}

/// 收集目录中符合归档命名规则的文件（跳过 `current.log` 与脚本自建文件）。
fn collect_archives(dir: &Path) -> io::Result<Vec<(String, PathBuf, Option<SystemTime>)>> {
    let mut archives = Vec::new();
    for entry in fs::read_dir(dir)? {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == CURRENT_LOG || !is_archive_name(&name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        archives.push((name, entry.path(), metadata.modified().ok()));
    }
    Ok(archives)
}

fn remove_archive(path: &Path) -> bool {
    match fs::remove_file(path) {
        Ok(()) => true,
        // 并发清理（每日全局清理 vs 写入线程轮转后清理）已删掉：不算失败。
        Err(error) if error.kind() == io::ErrorKind::NotFound => false,
        Err(error) => {
            tracing::warn!(path = %path.display(), %error, "删除过期任务日志失败");
            false
        }
    }
}

/// 是否是本模块产生的归档名（`size`: `YYYYMMDD-HHMMSS-NNN.log`；
/// `daily`: `YYYYMMDD.log` / `YYYYMMDD-NNN.log`）。
pub(crate) fn is_archive_name(name: &str) -> bool {
    is_size_archive(name) || is_daily_archive(name)
}

fn is_size_archive(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    let bytes = stem.as_bytes();
    // 8 位日期 + '-' + 6 位时间 + '-' + 至少 3 位序号。
    if bytes.len() < 8 + 1 + 6 + 1 + 3 {
        return false;
    }
    bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..15].iter().all(u8::is_ascii_digit)
        && bytes[15] == b'-'
        && bytes[16..].iter().all(u8::is_ascii_digit)
}

fn is_daily_archive(name: &str) -> bool {
    let Some(stem) = name.strip_suffix(".log") else {
        return false;
    };
    let bytes = stem.as_bytes();
    if bytes.len() == 8 {
        return bytes.iter().all(u8::is_ascii_digit);
    }
    bytes.len() >= 8 + 1 + 3
        && bytes[..8].iter().all(u8::is_ascii_digit)
        && bytes[8] == b'-'
        && bytes[9..].iter().all(u8::is_ascii_digit)
}
