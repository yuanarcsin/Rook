//! 任务日志命令（尾部读取、文件列表）。
//!
//! 命令体保持与拆分前逐字一致：只搬位置，不改行为/文案/stage。

use super::prelude::*;

// ---------------------------------------------------------------- 任务日志（M4-1）

/// 日志尾部读取结果；字段与 `src/lib/api.ts` 的 `TaskLogView` 一一对应。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskLogView {
    pub task_id: String,
    /// 实际读取的文件名（`current.log` 或归档名）。
    pub file_id: String,
    /// 实际读取的文件绝对路径（来自配置 + 经过白名单校验的文件名）。
    pub path: String,
    /// 按文件顺序的最后若干行（不含换行符）。
    pub lines: Vec<String>,
    /// 是否因行数/字节上限只返回了尾部。
    pub truncated: bool,
    /// 文件总字节数。
    pub total_bytes: u64,
    /// 文件修改时间（Unix epoch 毫秒）。
    pub modified_at: Option<i64>,
    /// 解码提示（GBK 等非 UTF-8 输出按替换字符显示）。
    pub warning: Option<String>,
}

impl TaskLogView {
    fn capture(task_id: &str, file_id: &str, path: &Path, tail: logs::LogTail) -> Self {
        Self {
            task_id: task_id.to_owned(),
            file_id: file_id.to_owned(),
            path: path.to_string_lossy().into_owned(),
            lines: tail.lines,
            truncated: tail.truncated,
            total_bytes: tail.total_bytes,
            modified_at: tail.modified_at,
            warning: tail.has_replacement.then(|| {
                "日志包含无法按 UTF-8 解码的字节（可能是 GBK 输出），已用替换字符显示".to_owned()
            }),
        }
    }
}

/// 一个可浏览的日志文件；字段与 `src/lib/api.ts` 的 `LogFileView` 一一对应。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct LogFileView {
    /// 传给 `read_task_log` 的 `fileId`（文件名，不含目录）。
    pub id: String,
    /// 下拉框显示名（当前即文件名）。
    pub name: String,
    pub size: u64,
    pub modified_at: Option<i64>,
    pub current: bool,
}

/// 任务日志文件列表；字段与 `src/lib/api.ts` 的 `TaskLogFilesView` 一一对应。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct TaskLogFilesView {
    pub task_id: String,
    /// `current.log` 在前，归档按 mtime 倒序（最多 50 个）。
    pub files: Vec<LogFileView>,
    /// 默认查看的文件，恒为 `current.log`。
    pub current_file_id: String,
}

/// 日志读取失败时返回 [`IpcError`]（`stage` 为 `config` / `invalid_task` /
/// `invalid_argument` / `log_missing` / `log_read` / `join`）。
impl Engine {
    /// 按当前配置解析任务的日志目录（`[tasks.logs].dir`）。
    ///
    /// 路径只来自配置：任务不存在、配置未加载或任务配置有误时返回明确错误，
    /// 前端无法传入任意目录。
    fn task_log_dir(&self, id: &str) -> Result<PathBuf, IpcError> {
        let Some(loaded) = self.loaded.as_ref() else {
            return Err(IpcError::with_task(
                "config",
                "配置文件尚未成功加载，无法确定任务日志目录",
                Some(id.to_owned()),
            ));
        };
        let Some(task) = loaded.config.task(id) else {
            return Err(IpcError::with_task(
                "invalid_task",
                format!("任务 {id} 不存在"),
                Some(id.to_owned()),
            ));
        };
        if !loaded.is_task_valid(id) {
            return Err(IpcError::with_task(
                "invalid_task",
                format!("任务 {id} 的配置有误，无法确定日志目录"),
                Some(id.to_owned()),
            ));
        }
        Ok(task.logs.dir.clone())
    }

    /// 按当前配置解析任务的 `current.log` 路径。
    fn task_log_path(&self, id: &str) -> Result<PathBuf, IpcError> {
        Ok(self.task_log_dir(id)?.join(logs::CURRENT_LOG))
    }
}

/// 校验前端传入的 `file_id`：只接受裸文件名，且必须是 `current.log` 或归档。
///
/// 绝对路径、含 `/` / `\` / `..` 或不符合归档命名的名字一律拒绝，
/// 保证读取范围不越出任务日志目录。
fn resolve_log_file_name(task_id: &str, file_id: Option<&str>) -> Result<String, IpcError> {
    let Some(name) = file_id else {
        return Ok(logs::CURRENT_LOG.to_owned());
    };
    if logs::is_log_file_name(name) {
        return Ok(name.to_owned());
    }
    Err(IpcError::with_task(
        "invalid_argument",
        format!("不支持的日志文件 {name}：只允许任务日志目录下的 current.log 或归档"),
        Some(task_id.to_owned()),
    ))
}

/// 读取任务日志尾部（最近 N 行 / 最多 256KB）。
///
/// `file_id` 缺省为 `current.log`，也可指定任务目录下的归档名；
/// 只读、无副作用；文件读取在阻塞线程池执行，Engine 锁只用于解析路径，
/// 不会因为大文件/慢磁盘卡住 UI。
#[tauri::command]
pub(crate) async fn read_task_log(
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
    file_id: Option<String>,
    tail_lines: Option<usize>,
    max_bytes: Option<usize>,
) -> Result<TaskLogView, IpcError> {
    // `file_id` 只接受裸文件名：绝对路径/`..` 在进入目录拼接前就被拒绝。
    let file_name = resolve_log_file_name(&task_id, file_id.as_deref())?;

    // 锁只用于解析路径；实际文件读取在阻塞线程池执行，不占用 Engine 锁。
    let engine = Arc::clone(state.inner());
    let path = if file_name == logs::CURRENT_LOG {
        lock(&engine).task_log_path(&task_id)?
    } else {
        lock(&engine).task_log_dir(&task_id)?.join(&file_name)
    };

    let read_path = path.clone();
    let joined = tauri::async_runtime::spawn_blocking(move || {
        logs::read_tail(&read_path, tail_lines, max_bytes)
    })
    .await;

    let tail = joined
        .map_err(|error| {
            IpcError::with_task(
                "join",
                format!("日志读取线程异常结束：{error}"),
                Some(task_id.clone()),
            )
        })?
        .map_err(|error| log_read_error(&task_id, &path, &error))?;
    Ok(TaskLogView::capture(&task_id, &file_name, &path, tail))
}

/// 列出任务的日志文件：`current.log` 在前，归档按 mtime 倒序（最多 50 个）。
///
/// 只读、无副作用；Engine 锁只用于解析目录，目录扫描在阻塞线程池执行。
/// 目录不存在（任务从未启动过）返回空列表，不当作错误。
#[tauri::command]
pub(crate) async fn list_task_logs(
    state: State<'_, Arc<Mutex<Engine>>>,
    task_id: String,
) -> Result<TaskLogFilesView, IpcError> {
    let engine = Arc::clone(state.inner());
    let dir = lock(&engine).task_log_dir(&task_id)?;

    let scan_dir = dir.clone();
    let joined =
        tauri::async_runtime::spawn_blocking(move || logs::list_log_files(&scan_dir)).await;

    let files = joined
        .map_err(|error| {
            IpcError::with_task(
                "join",
                format!("日志文件列表线程异常结束：{error}"),
                Some(task_id.clone()),
            )
        })?
        .map_err(|error| {
            IpcError::with_task(
                "log_read",
                format!("读取任务日志目录 {} 失败：{error}", dir.display()),
                Some(task_id.clone()),
            )
        })?
        .into_iter()
        .map(|entry| LogFileView {
            id: entry.name.clone(),
            name: entry.name,
            size: entry.size,
            modified_at: entry.modified_at,
            current: entry.current,
        })
        .collect();

    Ok(TaskLogFilesView {
        task_id,
        files,
        current_file_id: logs::CURRENT_LOG.to_owned(),
    })
}

fn log_read_error(task_id: &str, path: &Path, error: &std::io::Error) -> IpcError {
    if error.kind() == std::io::ErrorKind::NotFound {
        IpcError::with_task(
            "log_missing",
            format!("日志文件不存在：{}（任务可能尚未启动过）", path.display()),
            Some(task_id.to_owned()),
        )
    } else {
        IpcError::with_task(
            "log_read",
            format!("读取日志文件 {} 失败：{error}", path.display()),
            Some(task_id.to_owned()),
        )
    }
}
