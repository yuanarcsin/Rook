//! M4-1：`current.log` 尾部读取（供 UI 日志查看器）。
//!
//! 只做**只读**尾部读取，不整文件加载：
//!
//! - 按字节上限从文件尾部定位起点，最多读 [`MAX_TAIL_BYTES`]；
//! - 起点落在行中间时丢弃该残行（避免把半行当成完整行显示）；
//! - 再按行数上限保留最后 N 行，标记 `truncated`；
//! - 解码固定为 UTF-8 lossy：GBK 输出会显示替换字符并带 `warning`，
//!   不引入 encoding crate（docs/07 §5 的编码切换留给后续里程碑）。
//!
//! 路径解析只接受配置里的日志目录：调用方用 [`current_log_path`] 拼出
//! `<dir>/current.log`；C3 的归档浏览用 [`list_log_files`] 列出目录内容，再由
//! [`is_log_file_name`] 白名单约束可读文件名，前端无法传任意路径。

use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::CURRENT_LOG;
use super::writer::is_archive_name;

/// `tail_lines` 缺省值（docs/05：日志预览最近 200 行）。
pub const DEFAULT_TAIL_LINES: usize = 200;
/// `tail_lines` 上限：一次最多返回 2000 行。
pub const MAX_TAIL_LINES: usize = 2_000;
/// `max_bytes` 上限：一次最多读取 256KB。
pub const MAX_TAIL_BYTES: usize = 256 * 1024;

/// 一次尾部读取的结果（与 `TaskLogView` 的字段一一对应）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogTail {
    /// 按文件顺序的最后若干行（不含换行符）。
    pub lines: Vec<String>,
    /// 是否因行数或字节上限丢弃了更早的内容。
    pub truncated: bool,
    /// 文件总字节数（不是本次读取的字节数）。
    pub total_bytes: u64,
    /// 文件修改时间（Unix epoch 毫秒）；取不到时为 `None`。
    pub modified_at: Option<i64>,
    /// 解码时是否出现替换字符（U+FFFD），提示可能是 GBK 等非 UTF-8 输出。
    pub has_replacement: bool,
}

/// 把配置中的日志目录解析成当前活动文件路径。
///
/// 只接受目录，不接受完整文件路径：调用方永远从 `[tasks.logs].dir` 派生，
/// 前端无法指定任意文件。
pub fn current_log_path(dir: &Path) -> PathBuf {
    dir.join(CURRENT_LOG)
}

/// 任务日志目录中的一个可浏览文件：`current.log` 或轮转后的归档。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogFileEntry {
    /// 文件名（`current.log` 或归档名，不含目录）。
    pub name: String,
    /// 文件字节数。
    pub size: u64,
    /// 修改时间（Unix epoch 毫秒）；取不到时为 `None`。
    pub modified_at: Option<i64>,
    /// 是否是当前活动文件。
    pub current: bool,
}

/// 归档列表上限（docs/19 §C3）：历史文件很多时也不让下拉框无限增长。
const MAX_ARCHIVE_FILES: usize = 50;

/// 是否是可浏览的日志文件名：`current.log` 或本模块产生的归档。
///
/// 只接受**裸文件名**：绝对路径、带分隔符或 `..` 的名字都不满足命名规则，
/// 因此不能用它越出任务日志目录。
pub fn is_log_file_name(name: &str) -> bool {
    name == CURRENT_LOG || is_archive_name(name)
}

/// 列出目录中可浏览的日志文件：`current.log` + 归档。
///
/// - 目录不存在（任务从未启动过）返回空列表，不当作错误；
/// - 脚本自建文件（`notes.txt`、`script-child.log` 等）忽略；
/// - 排序：`current.log` 恒在最前，归档按 mtime 倒序（缺 mtime 排后），
///   最多返回 [`MAX_ARCHIVE_FILES`] 个归档。
pub fn list_log_files(dir: &Path) -> io::Result<Vec<LogFileEntry>> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };

    let mut current = None;
    let mut archives = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !is_log_file_name(&name) {
            continue;
        }
        let Ok(metadata) = entry.metadata() else {
            continue;
        };
        if !metadata.is_file() {
            continue;
        }
        let is_current = name == CURRENT_LOG;
        let item = LogFileEntry {
            name,
            size: metadata.len(),
            modified_at: metadata.modified().ok().and_then(epoch_millis),
            current: is_current,
        };
        if is_current {
            current = Some(item);
        } else {
            archives.push(item);
        }
    }

    // mtime 倒序：最新归档在最前；缺少 mtime 的排在后面，用文件名保证稳定。
    archives.sort_by(|left, right| {
        match (left.modified_at, right.modified_at) {
            (Some(left_at), Some(right_at)) => right_at.cmp(&left_at),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        }
        .then_with(|| left.name.cmp(&right.name))
    });
    archives.truncate(MAX_ARCHIVE_FILES);

    let mut files = Vec::with_capacity(archives.len() + 1);
    if let Some(item) = current {
        files.push(item);
    }
    files.extend(archives);
    Ok(files)
}

/// 归一化查询参数：缺省 200 行 / 256KB，上限 2000 行 / 256KB。
///
/// `0` 按缺省处理（避免"0 行"这种无意义请求），负数在类型层面已经排除。
pub fn normalize_limits(tail_lines: Option<usize>, max_bytes: Option<usize>) -> (usize, usize) {
    let lines = match tail_lines {
        None | Some(0) => DEFAULT_TAIL_LINES,
        Some(value) => value.min(MAX_TAIL_LINES),
    };
    let bytes = match max_bytes {
        None | Some(0) => MAX_TAIL_BYTES,
        Some(value) => value.min(MAX_TAIL_BYTES),
    };
    (lines, bytes)
}

/// 读取文件尾部：最多 `max_bytes` 字节、最后 `tail_lines` 行。
///
/// 文件不存在时返回 [`io::ErrorKind::NotFound`]，由命令层转成明确错误。
pub fn read_tail(
    path: &Path,
    tail_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> io::Result<LogTail> {
    let (line_limit, byte_limit) = normalize_limits(tail_lines, max_bytes);
    let mut file = File::open(path)?;
    let metadata = file.metadata()?;
    let total_bytes = metadata.len();
    let modified_at = metadata.modified().ok().and_then(epoch_millis);

    let start = total_bytes.saturating_sub(byte_limit as u64);
    // 起点前一个字节用于判断是否正好落在行首（CR/LF 边界）。
    let mut at_boundary = true;
    let mut previous = None;
    if start > 0 {
        file.seek(SeekFrom::Start(start - 1))?;
        let mut byte = [0u8; 1];
        file.read_exact(&mut byte)?;
        previous = Some(byte[0]);
        at_boundary = matches!(byte[0], b'\n' | b'\r');
    }

    file.seek(SeekFrom::Start(start))?;
    let want = (total_bytes - start).min(byte_limit as u64) as usize;
    let mut raw = Vec::with_capacity(want);
    file.take(total_bytes - start).read_to_end(&mut raw)?;

    let decoded = String::from_utf8_lossy(&raw);
    let has_replacement = decoded.contains('\u{FFFD}');
    let body = trim_partial_line(&decoded, start, at_boundary, previous);

    let mut lines: Vec<String> = split_lines(body).into_iter().map(str::to_owned).collect();
    let mut truncated = start > 0;
    if lines.len() > line_limit {
        lines.drain(..lines.len() - line_limit);
        truncated = true;
    }

    Ok(LogTail {
        lines,
        truncated,
        total_bytes,
        modified_at,
        has_replacement,
    })
}

/// 丢弃从文件中间开始时的残行（以及 CRLF 被切开时残留的 `\n`）。
fn trim_partial_line(text: &str, start: u64, at_boundary: bool, previous: Option<u8>) -> &str {
    if start == 0 {
        return text;
    }
    if at_boundary {
        // 前一个字节是 `\r` 时，起点可能正好落在 `\r\n` 的 `\n` 上。
        if previous == Some(b'\r') {
            return text.strip_prefix('\n').unwrap_or(text);
        }
        return text;
    }
    let Some(position) = text.find(['\n', '\r']) else {
        // 整段都在同一行中间：没有完整行可显示。
        return "";
    };
    let rest = &text[position..];
    rest.strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\n'))
        .or_else(|| rest.strip_prefix('\r'))
        .unwrap_or(rest)
}

/// 按 `\n` / `\r\n` / 单独 `\r` 切行；结尾换行不产生空行。
fn split_lines(text: &str) -> Vec<&str> {
    let bytes = text.as_bytes();
    let mut lines = Vec::new();
    let mut start = 0usize;
    let mut index = 0usize;
    while index < bytes.len() {
        match bytes[index] {
            b'\n' => {
                lines.push(&text[start..index]);
                index += 1;
                start = index;
            }
            b'\r' => {
                lines.push(&text[start..index]);
                index += 1;
                if index < bytes.len() && bytes[index] == b'\n' {
                    index += 1;
                }
                start = index;
            }
            _ => index += 1,
        }
    }
    if start < text.len() {
        lines.push(&text[start..]);
    }
    lines
}

fn epoch_millis(time: SystemTime) -> Option<i64> {
    let millis = time.duration_since(UNIX_EPOCH).ok()?.as_millis();
    i64::try_from(millis).ok()
}
