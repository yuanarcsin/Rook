//! Win32 薄封装：句柄 RAII、Job Object、管道、进程属性列表。
//!
//! 所有 `unsafe` 都收在这一层，上层（`process::Child`）保持 safe。
//! 句柄所有权规则：
//! - `OwnedHandle` 是唯一所有者，Drop 时 `CloseHandle`；
//! - Job 句柄用非继承句柄创建（`CreateJobObjectW` 传 NULL 安全属性），
//!   并且只通过 `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 白名单决定子进程能继承什么；
//! - 管道读端在父进程内**清除继承位**，写端保持可继承（否则无法进白名单）。

use std::ffi::{OsStr, c_void};
use std::mem::{size_of, size_of_val};
use std::os::windows::ffi::OsStrExt;
use std::ptr;
use std::sync::{Arc, Mutex};

use windows::Win32::Foundation::{
    CloseHandle, GENERIC_READ, HANDLE, HANDLE_FLAG_INHERIT, HANDLE_FLAGS, SetHandleInformation,
    WAIT_OBJECT_0, WAIT_TIMEOUT,
};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING,
};
use windows::Win32::System::Console::{
    AttachConsole, CTRL_BREAK_EVENT, FreeConsole, GenerateConsoleCtrlEvent, SetConsoleCtrlHandler,
};
use windows::Win32::System::JobObjects::{
    CreateJobObjectW, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE, JOBOBJECT_BASIC_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JobObjectBasicAccountingInformation,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT,
    DeleteProcThreadAttributeList, EXTENDED_STARTUPINFO_PRESENT, GetExitCodeProcess,
    InitializeProcThreadAttributeList, LPPROC_THREAD_ATTRIBUTE_LIST,
    PROC_THREAD_ATTRIBUTE_HANDLE_LIST, PROC_THREAD_ATTRIBUTE_JOB_LIST, PROCESS_CREATION_FLAGS,
    ResumeThread, STARTF_USESTDHANDLES, STARTUPINFOEXW, TerminateProcess,
    UpdateProcThreadAttribute, WaitForSingleObject,
};
use windows::core::{BOOL, Error as WinError, PCWSTR};

use super::error::{SpawnError, Stage, win32_code};

/// 拥有所有权的 Win32 句柄。
pub(crate) struct OwnedHandle(HANDLE);

impl OwnedHandle {
    pub(crate) fn new(handle: HANDLE) -> Self {
        Self(handle)
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.0
    }

    /// 放弃所有权，交给调用方（例如转成 `std::fs::File`）。
    pub(crate) fn into_raw(self) -> HANDLE {
        let handle = self.0;
        std::mem::forget(self);
        handle
    }
}

impl Drop for OwnedHandle {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // 关闭失败无法恢复，也不该在 Drop 里 panic。
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

// SAFETY: Win32 句柄是进程范围内的内核对象引用，没有线程亲和性；
// `WaitForSingleObject`/`QueryInformationJobObject`/`TerminateJobObject` 等都可以
// 从任意线程调用。唯一的所有权由 `OwnedHandle`（必要时再包 `Arc`）管理，
// `Arc` 保证句柄在最后一个引用释放前不会被关闭，因此不存在跨线程关闭后使用。
unsafe impl Send for OwnedHandle {}
unsafe impl Sync for OwnedHandle {}

/// stdout/stderr 管道：父进程保留读端，子进程继承写端。
pub(crate) struct PipePair {
    pub(crate) read: OwnedHandle,
    pub(crate) write: OwnedHandle,
}

impl PipePair {
    pub(crate) fn new() -> Result<Self, SpawnError> {
        let attributes = inheritable_attributes();
        let mut read = HANDLE::default();
        let mut write = HANDLE::default();
        unsafe { CreatePipe(&mut read, &mut write, Some(&attributes), 0) }
            .map_err(|error| SpawnError::win32(Stage::CreatePipe, &error))?;

        let read = OwnedHandle::new(read);
        let write = OwnedHandle::new(write);
        // 读端只属于父进程：即使 bInheritHandles=TRUE 也不应被子进程拿到。
        unsafe { SetHandleInformation(read.raw(), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
            .map_err(|error| SpawnError::win32(Stage::CreatePipe, &error))?;
        Ok(Self { read, write })
    }
}

/// 用 NUL 作为 stdin：脚本读 stdin 时立即 EOF，不会挂起等待输入。
pub(crate) fn open_nul_stdin() -> Result<OwnedHandle, SpawnError> {
    let path = wide_nul(OsStr::new("NUL"));
    let attributes = inheritable_attributes();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path.as_ptr()),
            GENERIC_READ.0,
            FILE_SHARE_MODE(FILE_SHARE_READ.0 | FILE_SHARE_WRITE.0),
            Some(&attributes),
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    }
    .map_err(|error| SpawnError::win32(Stage::CreateStdin, &error))?;
    Ok(OwnedHandle::new(handle))
}

/// Job Object：`KILL_ON_JOB_CLOSE` 保证最后一个 Job 句柄关闭时回收整棵进程树。
///
/// `Clone` 只增加引用计数：退出监控线程与 `request_stop` 共享同一个句柄，
/// 只要还有一个持有者，Job 就不会因句柄关闭而被提前回收。
#[derive(Clone)]
pub(crate) struct Job {
    handle: Arc<OwnedHandle>,
}

impl Job {
    pub(crate) fn create() -> Result<Self, SpawnError> {
        // 安全属性为 NULL：句柄不可继承。Job 句柄绝不能落到子进程手里，
        // 否则“最后一个句柄关闭才杀树”的语义会失效。
        let handle = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
            .map_err(|error| SpawnError::win32(Stage::CreateJob, &error))?;
        let job = Self {
            handle: Arc::new(OwnedHandle::new(handle)),
        };

        let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        info.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job.raw(),
                JobObjectExtendedLimitInformation,
                (&raw const info).cast(),
                size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
        }
        .map_err(|error| SpawnError::win32(Stage::ConfigureJob, &error))?;
        Ok(job)
    }

    pub(crate) fn raw(&self) -> HANDLE {
        self.handle.raw()
    }

    /// Job 内当前活动进程数（根进程退出后仍可查询）。
    pub(crate) fn active_processes(&self) -> Result<u32, SpawnError> {
        let mut info = JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe {
            QueryInformationJobObject(
                Some(self.raw()),
                JobObjectBasicAccountingInformation,
                (&raw mut info).cast(),
                size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,
                None,
            )
        }
        .map_err(|error| SpawnError::win32(Stage::QueryJob, &error))?;
        Ok(info.ActiveProcesses)
    }

    pub(crate) fn terminate(&self, exit_code: u32) -> Result<(), SpawnError> {
        unsafe { TerminateJobObject(self.raw(), exit_code) }
            .map_err(|error| SpawnError::win32(Stage::TerminateJob, &error))
    }
}

/// `PROC_THREAD_ATTRIBUTE_LIST` 的拥有者；Drop 时释放。
pub(crate) struct AttributeList {
    /// 属性列表本体必须保持地址稳定；`Vec` 的堆缓冲在结构体移动时不变。
    _buffer: Vec<usize>,
    /// 属性值的后备存储。`UpdateProcThreadAttribute` 只保存指针，
    /// 文档要求值一直存活到属性列表销毁（即 CreateProcessW 返回之后），
    /// 因此不能把 `[HANDLE]` 放在临时栈数组里。
    _values: Vec<Vec<HANDLE>>,
    ptr: LPPROC_THREAD_ATTRIBUTE_LIST,
}

impl AttributeList {
    /// 按最多 `count` 个属性初始化。
    pub(crate) fn new(count: u32) -> Result<Self, SpawnError> {
        let mut size = 0usize;
        // 第一次调用只取所需字节数，按文档预期失败（ERROR_INSUFFICIENT_BUFFER）。
        let _ = unsafe { InitializeProcThreadAttributeList(None, count, None, &mut size) };
        if size == 0 {
            return Err(SpawnError::with_code(
                Stage::AttributeList,
                87,
                "InitializeProcThreadAttributeList 未返回所需大小",
            ));
        }

        let words = size.div_ceil(size_of::<usize>());
        let mut buffer = vec![0usize; words];
        let ptr = LPPROC_THREAD_ATTRIBUTE_LIST(buffer.as_mut_ptr().cast());
        unsafe { InitializeProcThreadAttributeList(Some(ptr), count, None, &mut size) }
            .map_err(|error| SpawnError::win32(Stage::AttributeList, &error))?;
        Ok(Self {
            _buffer: buffer,
            _values: Vec::new(),
            ptr,
        })
    }

    pub(crate) fn ptr(&self) -> LPPROC_THREAD_ATTRIBUTE_LIST {
        self.ptr
    }

    /// 原子入 Job（Win10 1809+ 的 `PROC_THREAD_ATTRIBUTE_JOB_LIST`）。
    pub(crate) fn set_job_list(&mut self, job: HANDLE) -> Result<(), WinError> {
        let value = self.push_value(&[job]);
        unsafe {
            UpdateProcThreadAttribute(
                self.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
                Some(value.cast()),
                size_of::<HANDLE>(),
                None,
                None,
            )
        }
    }

    /// 只允许白名单里的句柄被继承。
    pub(crate) fn set_handle_list(&mut self, handles: &[HANDLE]) -> Result<(), WinError> {
        let size = size_of_val(handles);
        let value = self.push_value(handles);
        unsafe {
            UpdateProcThreadAttribute(
                self.ptr,
                0,
                PROC_THREAD_ATTRIBUTE_HANDLE_LIST as usize,
                Some(value.cast()),
                size,
                None,
                None,
            )
        }
    }

    /// 把属性值移入后备存储，返回指向它的稳定指针。
    fn push_value(&mut self, handles: &[HANDLE]) -> *const c_void {
        self._values.push(handles.to_vec());
        // `Vec<Vec<HANDLE>>` 增长只移动外层 Vec 结构，内层堆缓冲不会移动。
        self._values.last().expect("刚 push 过").as_ptr().cast()
    }
}

impl Drop for AttributeList {
    fn drop(&mut self) {
        unsafe { DeleteProcThreadAttributeList(self.ptr) };
    }
}

/// `STARTUPINFOEXW`：`STARTF_USESTDHANDLES` + 属性列表。
pub(crate) fn startup_info(
    attributes: &AttributeList,
    stdin: HANDLE,
    stdout: HANDLE,
    stderr: HANDLE,
) -> STARTUPINFOEXW {
    let mut startup = STARTUPINFOEXW::default();
    startup.StartupInfo.cb = size_of::<STARTUPINFOEXW>() as u32;
    startup.StartupInfo.dwFlags = STARTF_USESTDHANDLES;
    startup.StartupInfo.hStdInput = stdin;
    startup.StartupInfo.hStdOutput = stdout;
    startup.StartupInfo.hStdError = stderr;
    startup.lpAttributeList = attributes.ptr();
    startup
}

/// `CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP |
/// EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`
///
/// `CREATE_NEW_PROCESS_GROUP` 是优雅停止的前置条件：只有根进程拥有独立进程组，
/// `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)` 才能定向投递而不波及其它进程
/// （见 docs/research/spike-s3-graceful-stop.md §4.3）。`CREATE_NO_WINDOW` 与它可叠加，
/// 子进程仍有隐藏控制台，事件可达。
pub(crate) fn creation_flags() -> PROCESS_CREATION_FLAGS {
    CREATE_SUSPENDED
        | CREATE_NO_WINDOW
        | CREATE_NEW_PROCESS_GROUP
        | EXTENDED_STARTUPINFO_PRESENT
        | CREATE_UNICODE_ENVIRONMENT
}

/// 控制台 attach 是**进程级**状态：同一时刻只能附加到一个控制台，
/// 并发调用 `AttachConsole`/`FreeConsole` 会互相踩踏（spike §3.3 C14）。
/// 所有控制台信号发送都必须先拿这把锁。
static CONSOLE_LOCK: Mutex<()> = Mutex::new(());

/// 控制台信号发送结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConsoleSignal {
    /// `GenerateConsoleCtrlEvent` 返回成功。
    Delivered,
    /// 目标已退出/没有控制台等可降级情形；调用方应直接走 Job 兜底。
    Skipped(String),
}

/// 向根进程所在的进程组发送 `CTRL_BREAK_EVENT`。
///
/// 固定序列（缺一不可，见 spike §4.3）：
/// 1. `SetConsoleCtrlHandler(NULL, TRUE)`：监督器自己忽略 Ctrl+C；
/// 2. `FreeConsole()`：先脱离当前控制台（错误可忽略）；
/// 3. `AttachConsole(pid)`：附加到目标进程的控制台；
/// 4. `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid)`：只发 CTRL_BREAK，
///    禁止 `CTRL_C_EVENT` 广播（会打死监督器）；
/// 5. `FreeConsole()`：恢复无控制台状态。
///
/// 只发 CTRL_BREAK、不分配可见控制台。`AttachConsole`/事件发送遇到
/// “进程已退出/无控制台”类错误时返回 [`ConsoleSignal::Skipped`]，
/// 由调用方降级为 `TerminateJobObject`；其它错误返回可读的 [`SpawnError`]。
pub(crate) fn send_ctrl_break(pid: u32) -> Result<ConsoleSignal, SpawnError> {
    if pid == 0 {
        return Ok(ConsoleSignal::Skipped(
            "目标 pid 为 0，无法定位进程组".to_owned(),
        ));
    }

    let _guard = CONSOLE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // 保护自身：即使后续调用意外收到广播，也不会被默认处理程序打死。
    unsafe { SetConsoleCtrlHandler(None, true) }
        .map_err(|error| SpawnError::win32(Stage::ConsoleSignal, &error))?;

    // GUI 监督者通常没有控制台；`FreeConsole` 失败（无控制台）是正常情况。
    let _ = unsafe { FreeConsole() };

    if let Err(error) = unsafe { AttachConsole(pid) } {
        let code = win32_code(&error);
        // 失败时保持“无控制台”状态，避免下一次 attach 报 err=5。
        let _ = unsafe { FreeConsole() };
        if degradable_console_error(code) {
            return Ok(ConsoleSignal::Skipped(format!(
                "AttachConsole({pid}) 失败（Win32 错误 {code}）：{}",
                error.message()
            )));
        }
        return Err(SpawnError::win32(Stage::ConsoleSignal, &error));
    }

    let generated = unsafe { GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT, pid) };
    let _ = unsafe { FreeConsole() };

    match generated {
        Ok(()) => Ok(ConsoleSignal::Delivered),
        Err(error) => {
            let code = win32_code(&error);
            if degradable_console_error(code) {
                Ok(ConsoleSignal::Skipped(format!(
                    "GenerateConsoleCtrlEvent({pid}) 失败（Win32 错误 {code}）：{}",
                    error.message()
                )))
            } else {
                Err(SpawnError::win32(Stage::ConsoleSignal, &error))
            }
        }
    }
}

/// 目标已退出/没有控制台/会话不匹配等：不值得当作停止失败，直接走 Job 兜底。
fn degradable_console_error(code: u32) -> bool {
    // 5 = ERROR_ACCESS_DENIED（已附加或跨会话）；6 = ERROR_INVALID_HANDLE
    // （目标没有控制台/已退出）；31 = ERROR_GEN_FAILURE（进程组已消失）。
    matches!(code, 5 | 6 | 31)
}

/// 等待进程句柄；返回 `true` 表示已退出，`false` 表示超时。
pub(crate) fn wait_process(process: HANDLE, millis: u32) -> Result<bool, SpawnError> {
    let event = unsafe { WaitForSingleObject(process, millis) };
    if event == WAIT_OBJECT_0 {
        Ok(true)
    } else if event == WAIT_TIMEOUT {
        Ok(false)
    } else {
        Err(SpawnError::win32(
            Stage::WaitProcess,
            &WinError::from_thread(),
        ))
    }
}

pub(crate) fn exit_code(process: HANDLE) -> Result<u32, SpawnError> {
    let mut code = 0u32;
    unsafe { GetExitCodeProcess(process, &mut code) }
        .map_err(|error| SpawnError::win32(Stage::WaitProcess, &error))?;
    Ok(code)
}

pub(crate) fn resume_thread(thread: HANDLE) -> Result<(), SpawnError> {
    let previous = unsafe { ResumeThread(thread) };
    if previous == u32::MAX {
        Err(SpawnError::win32(
            Stage::ResumeThread,
            &WinError::from_thread(),
        ))
    } else {
        Ok(())
    }
}

pub(crate) fn terminate_process(process: HANDLE, exit_code: u32) {
    let _ = unsafe { TerminateProcess(process, exit_code) };
}

/// 转成 UTF-16 + NUL 的宽字符串。
pub(crate) fn wide_nul(value: &OsStr) -> Vec<u16> {
    let mut wide: Vec<u16> = value.encode_wide().collect();
    wide.push(0);
    wide
}

fn inheritable_attributes() -> SECURITY_ATTRIBUTES {
    SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: ptr::null_mut(),
        bInheritHandle: BOOL(1),
    }
}

/// 测试辅助：句柄是否可继承。
#[cfg(test)]
pub(crate) fn is_inheritable(handle: HANDLE) -> bool {
    let mut flags = 0u32;
    unsafe { windows::Win32::Foundation::GetHandleInformation(handle, &mut flags) }
        .expect("GetHandleInformation");
    flags & HANDLE_FLAG_INHERIT.0 != 0
}
