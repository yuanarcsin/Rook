//! M2-1：Windows 进程启动器（CreateProcessW + Job Object + 管道）。
//!
//! 目标是为 M2 后续的退出检测、日志采集与状态机提供可独立测试的地基：
//! **不依赖 Tauri**、不写日志、不接 Engine/UI。
//!
//! # 启动流程
//!
//! 1. [`resolve_program`]：绝对/相对路径或 PATH/PATHEXT 查找，得到确定的可执行文件；
//! 2. Job Object：`CreateJobObjectW` + `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`，
//!    句柄不可继承，生命周期等于子进程实例；
//! 3. stdout/stderr 各一条管道（父留读端并清除继承位），stdin 指向 NUL；
//! 4. `STARTUPINFOEXW` + `PROC_THREAD_ATTRIBUTE_HANDLE_LIST` 限制继承句柄；
//! 5. `PROC_THREAD_ATTRIBUTE_JOB_LIST` 原子入 Job（Win10 1809+）；
//!    属性设置失败时降级为 `CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread`，
//!    原因记录在 [`JobMode::Fallback`]；
//! 6. `CreateProcessW`（`CREATE_SUSPENDED | CREATE_NO_WINDOW | CREATE_NEW_PROCESS_GROUP |
//!    EXTENDED_STARTUPINFO_PRESENT | CREATE_UNICODE_ENVIRONMENT`）→ 降级时入 Job → `ResumeThread`；
//! 7. 父进程关闭写端与 stdin，返回 [`Child`]。
//!
//! # 生命周期
//!
//! - [`Child::wait_for_exit`] 只等**根进程**（`WaitForSingleObject` + `GetExitCodeProcess`）；
//! - [`Child::active_processes`] 查询 Job 内活动进程数，供 M2-2 判断“根退出但树未空”；
//! - [`Child::terminate`] 用 `TerminateJobObject` 立即终止整棵树；
//! - [`ExitWatcher::request_stop`] 支持优雅停止（`CTRL_BREAK_EVENT` + 宽限 +
//!   `TerminateJobObject` 兜底），进程统一以 `CREATE_NEW_PROCESS_GROUP` 启动；
//! - [`Child`] Drop 时关闭 Job 句柄；`KILL_ON_JOB_CLOSE` 会回收仍在运行的进程树。
//!
//! # 未覆盖
//!
//! 不做日志读取/轮转（M2-3）。端口预检是只读旁路（M2-3b），见 [`check_tcp_ports`]。

pub mod args;
mod env;
mod error;
/// C6：资源守卫的只读采样与连续超限判定。
pub mod limits;
mod path;
mod ports;
#[cfg(test)]
mod tests;
mod watch;
mod win32;

use std::ffi::OsString;
use std::os::windows::io::{FromRawHandle, RawHandle};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use windows::Win32::System::JobObjects::AssignProcessToJobObject;
use windows::Win32::System::Threading::{CreateProcessW, PROCESS_INFORMATION, STARTUPINFOW};
use windows::core::{PCWSTR, PWSTR};

pub use args::{build_command_line, quote_arg};
pub use env::build_environment_block;
pub use error::{SpawnError, Stage};
pub use path::resolve_program;
pub use ports::{
    IpFamily, PortCheckError, PortCheckReport, PortCheckStage, PortListener, PortStatus,
    check_tcp_ports,
};
pub use watch::{ExitEvent, ExitWatcher, StopError, StopMode, StopReport, WatchOptions};

use win32::{
    AttributeList, Job, OwnedHandle, PipePair, creation_flags, exit_code, open_nul_stdin,
    resume_thread, startup_info, terminate_process, wait_process,
};

/// 启动一个进程所需的信息。
#[derive(Debug, Clone, Default)]
pub struct SpawnRequest {
    /// 可执行文件：绝对/相对路径，或 PATH 中的裸名字（如 `cmd.exe`）。
    pub program: OsString,
    /// 参数（不含 program 本身），按 `CommandLineToArgvW` 规则加引号。
    pub args: Vec<String>,
    /// 工作目录；`None` 表示继承父进程当前目录。
    pub working_dir: Option<PathBuf>,
    /// 环境变量覆盖（大小写不敏感，覆盖继承值；同名项以这里为准）。
    pub env: Vec<(OsString, OsString)>,
}

impl SpawnRequest {
    /// 只指定 program，其余为空。
    pub fn new(program: impl Into<OsString>) -> Self {
        Self {
            program: program.into(),
            ..Self::default()
        }
    }
}

/// 子进程进入 Job 的方式。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobMode {
    /// `PROC_THREAD_ATTRIBUTE_JOB_LIST` 原子入 Job：进程创建时已在 Job 内，没有孤儿窗口。
    Atomic,
    /// 属性设置失败后的降级：`CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread`。
    Fallback {
        /// 降级原因（含 Win32 错误码），写入日志/诊断。
        reason: String,
    },
}

impl JobMode {
    /// 是否走了原子入 Job。
    pub fn is_atomic(&self) -> bool {
        matches!(self, Self::Atomic)
    }
}

/// 根进程退出状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExitStatus {
    pub code: u32,
}

impl ExitStatus {
    /// 退出码为 0。
    pub fn success(&self) -> bool {
        self.code == 0
    }
}

/// stdout/stderr 的父进程读端。
///
/// M2-3 用 [`PipeReader::into_file`] 转成 `std::fs::File` 后交给读取线程；
/// 也可以拿 [`PipeReader::as_raw_handle`] 自行 `ReadFile`。
pub struct PipeReader {
    handle: OwnedHandle,
}

impl PipeReader {
    fn new(handle: OwnedHandle) -> Self {
        Self { handle }
    }

    /// 裸句柄；所有权仍在本对象，调用方不得关闭。
    pub fn as_raw_handle(&self) -> RawHandle {
        self.handle.raw().0
    }

    /// 转成 `std::fs::File`（转移句柄所有权）。
    pub fn into_file(self) -> std::fs::File {
        let raw = self.handle.into_raw();
        // SAFETY: 句柄由本对象独占，转换后不再有第二个所有者。
        unsafe { std::fs::File::from_raw_handle(raw.0) }
    }
}

/// 已启动的进程实例（根进程 + Job + 管道读端）。
///
/// 根进程与 Job 句柄用 `Arc` 持有：M2-2 的 [`ExitWatcher`] 接管后，
/// 等待线程与停止调用共享同一份句柄，句柄不会在仍被使用时关闭。
pub struct Child {
    process: Arc<OwnedHandle>,
    job: Job,
    pid: u32,
    stdout: Option<PipeReader>,
    stderr: Option<PipeReader>,
    mode: JobMode,
    exit: Option<ExitStatus>,
}

impl Child {
    /// 根进程 PID（不是 Job id）。
    pub fn pid(&self) -> u32 {
        self.pid
    }

    /// 入 Job 的方式。
    pub fn job_mode(&self) -> &JobMode {
        &self.mode
    }

    /// stdout 读端；`take_stdout` 之后为 `None`。
    pub fn take_stdout(&mut self) -> Option<PipeReader> {
        self.stdout.take()
    }

    /// stderr 读端；`take_stderr` 之后为 `None`。
    pub fn take_stderr(&mut self) -> Option<PipeReader> {
        self.stderr.take()
    }

    /// 已缓存的退出状态（尚未确认退出时为 `None`）。
    pub fn exit_status(&self) -> Option<ExitStatus> {
        self.exit
    }

    /// 等待根进程退出；超时返回 `Ok(None)`，不杀进程。
    pub fn wait_for_exit(&mut self, timeout: Duration) -> Result<Option<ExitStatus>, SpawnError> {
        if let Some(status) = self.exit {
            return Ok(Some(status));
        }
        // u32::MAX 是 INFINITE，必须避开；49 天足够长。
        let millis = timeout.as_millis().min(u128::from(u32::MAX - 1)) as u32;
        if !wait_process(self.process.raw(), millis)? {
            return Ok(None);
        }
        let status = ExitStatus {
            code: exit_code(self.process.raw())?,
        };
        self.exit = Some(status);
        Ok(Some(status))
    }

    /// 立即终止 Job 内所有进程（`TerminateJobObject`，不可被脚本处理）。
    pub fn terminate(&mut self) -> Result<(), SpawnError> {
        self.job.terminate(1)
    }

    /// Job 内当前活动进程数：根进程退出后仍可查询，用于确认进程树是否清空。
    pub fn active_processes(&self) -> Result<u32, SpawnError> {
        self.job.active_processes()
    }

    /// 测试辅助：Job 句柄是否可继承（必须为 `false`）。
    #[cfg(test)]
    fn job_handle_is_inheritable(&self) -> bool {
        win32::is_inheritable(self.job.raw())
    }
}

/// 取 env 覆盖里最后一个同名项（大小写不敏感）。
fn override_value(overrides: &[(OsString, OsString)], name: &str) -> Option<OsString> {
    overrides
        .iter()
        .rev()
        .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
        .map(|(_, value)| value.clone())
}

/// 启动进程。
pub fn spawn(request: SpawnRequest) -> Result<Child, SpawnError> {
    spawn_with(request, true)
}

/// `try_job_list=false` 用于测试降级路径（`AssignProcessToJobObject`）。
fn spawn_with(request: SpawnRequest, try_job_list: bool) -> Result<Child, SpawnError> {
    if request.program.is_empty() {
        return Err(SpawnError::with_code(
            Stage::ResolveProgram,
            87,
            "command 为空",
        ));
    }
    if request.args.iter().any(|arg| arg.contains('\0')) {
        return Err(SpawnError::with_code(
            Stage::CreateProcess,
            87,
            "参数不能包含 NUL",
        ));
    }

    // 任务自己的 env 覆盖优先：脚本看到哪个 PATH，就用哪个 PATH 解析命令。
    let path_var = override_value(&request.env, "PATH").or_else(|| std::env::var_os("PATH"));
    let pathext = override_value(&request.env, "PATHEXT").or_else(|| std::env::var_os("PATHEXT"));
    let program = resolve_program(
        &request.program,
        request.working_dir.as_deref(),
        path_var.as_deref(),
        pathext.as_deref(),
    )?;

    // Job 先建：即使后面任何一步失败，Drop 的 Job 也会兜底回收。
    let job = Job::create()?;

    let stdout = PipePair::new()?;
    let stderr = PipePair::new()?;
    let stdin = open_nul_stdin()?;

    // 两个属性：JOB_LIST + HANDLE_LIST。若 JOB_LIST 不被支持（旧系统/受限环境），
    // 属性列表仍可用于 HANDLE_LIST。
    let mut attributes = AttributeList::new(2)?;
    attributes
        .set_handle_list(&[stdin.raw(), stdout.write.raw(), stderr.write.raw()])
        .map_err(|error| SpawnError::win32(Stage::SetHandleList, &error))?;

    let mode = if !try_job_list {
        JobMode::Fallback {
            reason: "测试强制走 suspend → assign → resume 路径".to_owned(),
        }
    } else {
        match attributes.set_job_list(job.raw()) {
            Ok(()) => JobMode::Atomic,
            Err(error) => JobMode::Fallback {
                reason: format!(
                    "PROC_THREAD_ATTRIBUTE_JOB_LIST 失败（Win32 错误 {}）：{}",
                    error::win32_code(&error),
                    error.message()
                ),
            },
        }
    };
    let atomic = mode.is_atomic();

    let mut command_line = build_command_line(program.as_os_str(), &request.args);
    let environment = build_environment_block(&request.env);
    let application = win32::wide_nul(program.as_os_str());
    let current_dir = request
        .working_dir
        .as_deref()
        .map(|dir| win32::wide_nul(dir.as_os_str()));
    let startup = startup_info(
        &attributes,
        stdin.raw(),
        stdout.write.raw(),
        stderr.write.raw(),
    );
    let mut process_info = PROCESS_INFORMATION::default();

    let created = unsafe {
        CreateProcessW(
            PCWSTR(application.as_ptr()),
            Some(PWSTR(command_line.as_mut_ptr())),
            None,
            None,
            true,
            creation_flags(),
            Some(environment.as_ptr().cast()),
            current_dir
                .as_deref()
                .map_or(PCWSTR::null(), |dir| PCWSTR(dir.as_ptr())),
            (&raw const startup).cast::<STARTUPINFOW>(),
            &mut process_info,
        )
    };
    if let Err(error) = created {
        return Err(SpawnError::win32(Stage::CreateProcess, &error));
    }

    let process = Arc::new(OwnedHandle::new(process_info.hProcess));
    let thread = OwnedHandle::new(process_info.hThread);
    let pid = process_info.dwProcessId;

    if !atomic {
        // 降级：进程仍处于挂起状态，此时入 Job 不存在“已跑起来但未入 Job”的窗口。
        if let Err(error) = unsafe { AssignProcessToJobObject(job.raw(), process.raw()) } {
            terminate_process(process.raw(), 1);
            return Err(SpawnError::win32(Stage::AssignJob, &error));
        }
    }

    if let Err(error) = resume_thread(thread.raw()) {
        // ResumeThread 失败时进程永远挂起，必须主动回收。
        let _ = job.terminate(1);
        return Err(error);
    }
    drop(thread);

    // 父进程必须关掉写端和 stdin：否则子进程退出后读端也等不到 EOF。
    let PipePair {
        read: stdout_read,
        write: stdout_write,
    } = stdout;
    let PipePair {
        read: stderr_read,
        write: stderr_write,
    } = stderr;
    drop(stdout_write);
    drop(stderr_write);
    drop(stdin);

    Ok(Child {
        process,
        job,
        pid,
        stdout: Some(PipeReader::new(stdout_read)),
        stderr: Some(PipeReader::new(stderr_read)),
        mode,
        exit: None,
    })
}
