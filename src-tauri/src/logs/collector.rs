//! 采集器：两条管道读线程 + 单写入线程 + 统计/生命周期。
//!
//! 读线程使用 `PeekNamedPipe` 轮询而不是阻塞 `ReadFile`：
//!
//! - 有数据时只读已就绪的部分，不会卡在管道上；
//! - 没有数据时短暂 sleep 并检查停止标志，因此 Drop 能在
//!   [`LogParams::poll_interval`] 内结束线程并关闭读端；
//! - 子进程退出后 `PeekNamedPipe` 返回 `ERROR_BROKEN_PIPE`，读线程正常收尾。
//!
//! 读到的块通过有界通道交给写入线程；通道满时**丢弃并计数**而不是阻塞，
//! 保证慢磁盘/坏磁盘不会反压子进程。写入线程是唯一的文件写入者，因此
//! stdout/stderr 的合并顺序就是块到达顺序。

use std::io::Read;
use std::os::windows::io::AsRawHandle;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, SyncSender, TrySendError};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::Pipes::PeekNamedPipe;

use super::writer::Writer;
use super::{Counters, LogError, LogParams, LogStats};
use crate::process::PipeReader;

/// `ERROR_BROKEN_PIPE`：写端全部关闭（子进程退出）。
const ERROR_BROKEN_PIPE: u32 = 109;
/// `ERROR_PIPE_NOT_CONNECTED`：管道已断开。
const ERROR_PIPE_NOT_CONNECTED: u32 = 233;

/// 读线程、写入线程与调用方共享的状态。
struct Shared {
    stop: AtomicBool,
    counters: Arc<Counters>,
    /// 尚未结束的线程数；减到 0 时通知等待方。
    remaining: Mutex<usize>,
    finished: Condvar,
}

impl Shared {
    fn new() -> Self {
        Self {
            stop: AtomicBool::new(false),
            counters: Arc::new(Counters::default()),
            // 写入线程先计数；每个读线程启动前再加 1。
            remaining: Mutex::new(1),
            finished: Condvar::new(),
        }
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// 线程退出前调用：计数减一，最后一个线程唤醒等待方。
    fn finish_one(&self) {
        let mut remaining = self
            .remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining = remaining.saturating_sub(1);
        if *remaining == 0 {
            self.finished.notify_all();
        }
    }

    fn add_thread(&self) {
        let mut remaining = self
            .remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining += 1;
    }

    fn remove_thread(&self) {
        let mut remaining = self
            .remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining = remaining.saturating_sub(1);
    }

    /// 有界等待全部线程结束；返回 `true` 表示已全部结束。
    fn wait_done(&self, timeout: Duration) -> bool {
        let remaining = self
            .remaining
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let (remaining, _) = self
            .finished
            .wait_timeout_while(remaining, timeout, |remaining| *remaining > 0)
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *remaining == 0
    }
}

/// 一个实例的日志采集句柄。
///
/// 由 [`LogCollector::start`] 创建；持有读线程与写入线程的 join 句柄。
/// 子进程退出、管道 EOF 后调用 [`LogCollector::join`] 拿到最终统计；
/// 提前 drop 会通知线程退出并做有界 join。
pub struct LogCollector {
    shared: Arc<Shared>,
    readers: Vec<JoinHandle<()>>,
    writer: Option<JoinHandle<()>>,
    stop_timeout: Duration,
    joined: bool,
}

impl LogCollector {
    /// 启动采集：创建目录、写入线程与两个读线程（没有对应管道就跳过）。
    ///
    /// 目录创建失败返回 [`LogError::CreateDir`]；单个日志文件打不开**不**失败，
    /// 而是进入降级状态（`dropped_bytes`/`io_errors` 增长），任务照常运行。
    pub fn start(
        stdout: Option<PipeReader>,
        stderr: Option<PipeReader>,
        params: LogParams,
    ) -> Result<Self, LogError> {
        if stdout.is_none() && stderr.is_none() {
            return Err(LogError::NoSources);
        }
        std::fs::create_dir_all(&params.dir).map_err(|source| LogError::CreateDir {
            path: params.dir.clone(),
            source,
        })?;

        let shared = Arc::new(Shared::new());
        let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(params.queue_chunks.max(1));

        let writer_shared = Arc::clone(&shared);
        let writer_params = params.clone();
        let writer = thread::Builder::new()
            .name("cs-log-write".to_owned())
            .spawn(move || {
                let mut writer = Writer::new(writer_params, Arc::clone(&writer_shared.counters));
                while let Ok(chunk) = receiver.recv() {
                    writer.write_chunk(&chunk);
                }
                writer.finish();
                writer_shared.finish_one();
            })
            .map_err(|source| LogError::SpawnThread {
                role: "写入",
                source,
            })?;

        let mut readers = Vec::new();
        for (role, pipe) in [("stdout", stdout), ("stderr", stderr)] {
            let Some(pipe) = pipe else { continue };
            shared.add_thread();
            let reader_shared = Arc::clone(&shared);
            let reader_sender = sender.clone();
            let reader_params = params.clone();
            let spawned = thread::Builder::new()
                .name(format!("cs-log-{role}"))
                .spawn(move || {
                    read_loop(pipe, reader_sender, &reader_shared, &reader_params);
                    reader_shared.finish_one();
                });
            match spawned {
                Ok(handle) => readers.push(handle),
                Err(source) => {
                    // 回滚计数并停止已启动的线程；写入线程会在所有发送端释放后退出。
                    shared.remove_thread();
                    shared.request_stop();
                    drop(sender);
                    let done = shared.wait_done(Duration::from_secs(1));
                    for handle in readers {
                        let _ = handle.join();
                    }
                    if done {
                        let _ = writer.join();
                    }
                    return Err(LogError::SpawnThread { role, source });
                }
            }
        }
        // 调用方不再持有发送端：所有读线程结束后写入线程才能看到断开并收尾。
        drop(sender);

        Ok(Self {
            shared,
            readers,
            writer: Some(writer),
            stop_timeout: params.stop_timeout,
            joined: false,
        })
    }

    /// 当前统计快照（可在运行中随时读取）。
    pub fn stats(&self) -> LogStats {
        self.shared.counters.snapshot()
    }

    /// 有界等待所有采集线程结束；返回 `true` 表示已全部退出。
    ///
    /// 正常路径下读线程在管道 EOF 后结束、写入线程在排空队列后结束。
    pub fn wait(&self, timeout: Duration) -> bool {
        self.shared.wait_done(timeout)
    }

    /// 等待所有线程结束并返回最终统计。
    ///
    /// 会阻塞到管道 EOF（子进程退出/管道写端关闭）。若调用方不打算等子进程，
    /// 直接 drop 句柄即可。
    pub fn join(mut self) -> LogStats {
        self.joined = true;
        for reader in std::mem::take(&mut self.readers) {
            let _ = reader.join();
        }
        if let Some(writer) = self.writer.take() {
            let _ = writer.join();
        }
        self.shared.counters.snapshot()
    }
}

impl Drop for LogCollector {
    fn drop(&mut self) {
        if self.joined {
            return;
        }
        self.shared.request_stop();
        if self.shared.wait_done(self.stop_timeout) {
            for reader in self.readers.drain(..) {
                let _ = reader.join();
            }
            if let Some(writer) = self.writer.take() {
                let _ = writer.join();
            }
        }
        // 超时则放弃 join：线程持有 `Arc`，会在管道关闭/写调用返回后自行退出；
        // Drop 不能无限阻塞调用方。
    }
}

/// 读线程主体：轮询管道 → 读取就绪字节 → 尝试入队。
fn read_loop(pipe: PipeReader, sender: SyncSender<Vec<u8>>, shared: &Shared, params: &LogParams) {
    let mut file = pipe.into_file();
    let handle = HANDLE(file.as_raw_handle());
    let mut buffer = vec![0u8; params.chunk_bytes.max(1)];

    loop {
        if shared.stopped() {
            break;
        }

        let mut available = 0u32;
        // SAFETY: 句柄由 `file` 独占且在本函数内保持有效；只查询可读字节数，
        // 不传缓冲区，因此不会发生写入。
        let peeked = unsafe { PeekNamedPipe(handle, None, 0, None, Some(&mut available), None) };
        if let Err(error) = peeked {
            let code = win32_code(&error);
            if code != ERROR_BROKEN_PIPE && code != ERROR_PIPE_NOT_CONNECTED {
                shared.counters.add_read_error();
            }
            break;
        }

        if available == 0 {
            thread::sleep(params.poll_interval);
            continue;
        }

        let want = (available as usize).min(buffer.len());
        match file.read(&mut buffer[..want]) {
            Ok(0) => break,
            Ok(read) => {
                shared.counters.add_read(read);
                let chunk = buffer[..read].to_vec();
                match sender.try_send(chunk) {
                    Ok(()) => {}
                    Err(TrySendError::Full(chunk)) => {
                        // 写入跟不上：丢弃而不是阻塞读线程，子进程不受影响。
                        shared.counters.add_queue_drop();
                        shared.counters.add_dropped(chunk.len());
                    }
                    Err(TrySendError::Disconnected(_)) => break,
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(_) => {
                shared.counters.add_read_error();
                break;
            }
        }
    }
}

/// 把 windows-rs 的 HRESULT 还原成 Win32 错误码（与 `process::error` 同一规则）。
fn win32_code(error: &windows::core::Error) -> u32 {
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}
