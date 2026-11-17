//! 进程启动错误：统一携带失败阶段与 Win32 错误码。
//!
//! 每个错误都回答两个问题：**在哪一步失败**（[`Stage`]）和**系统给了什么码**
//! （[`SpawnError::code`]）。上层（M2-2/M2-3）据此区分“命令找不到”“Job 属性不支持”
//! “CreateProcess 被拒”等情况，而不是只看到一个字符串。

use std::fmt;

use windows::core::Error as WinError;

/// 启动/等待/停止流程中的失败阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    /// 解析 program：PATH/PATHEXT 查找或相对路径拼接。
    ResolveProgram,
    /// `CreateJobObjectW`。
    CreateJob,
    /// `SetInformationJobObject`（KILL_ON_JOB_CLOSE）。
    ConfigureJob,
    /// `CreatePipe`。
    CreatePipe,
    /// 打开 NUL 作为 stdin。
    CreateStdin,
    /// `InitializeProcThreadAttributeList`。
    AttributeList,
    /// `UpdateProcThreadAttribute(PROC_THREAD_ATTRIBUTE_HANDLE_LIST)`。
    SetHandleList,
    /// `CreateProcessW`。
    CreateProcess,
    /// 降级路径上的 `AssignProcessToJobObject`。
    AssignJob,
    /// `ResumeThread`。
    ResumeThread,
    /// `WaitForSingleObject` / `GetExitCodeProcess`。
    WaitProcess,
    /// `QueryInformationJobObject`。
    QueryJob,
    /// `TerminateJobObject`。
    TerminateJob,
    /// 控制台信号：`AttachConsole` / `GenerateConsoleCtrlEvent`。
    ConsoleSignal,
    /// 创建退出监控线程。
    SpawnWatcher,
}

impl Stage {
    /// 面向日志/界面中文阶段名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::ResolveProgram => "解析可执行文件",
            Self::CreateJob => "创建 Job Object",
            Self::ConfigureJob => "配置 Job Object",
            Self::CreatePipe => "创建管道",
            Self::CreateStdin => "打开 NUL 作为 stdin",
            Self::AttributeList => "初始化进程属性列表",
            Self::SetHandleList => "设置句柄白名单",
            Self::CreateProcess => "CreateProcessW",
            Self::AssignJob => "AssignProcessToJobObject",
            Self::ResumeThread => "ResumeThread",
            Self::WaitProcess => "等待进程退出",
            Self::QueryJob => "查询 Job 信息",
            Self::TerminateJob => "TerminateJobObject",
            Self::ConsoleSignal => "发送控制台信号",
            Self::SpawnWatcher => "创建退出监控线程",
        }
    }
}

impl fmt::Display for Stage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 进程子系统错误：阶段 + Win32 错误码 + 说明。
///
/// `code` 是**原始 Win32 错误码**（如 2 = ERROR_FILE_NOT_FOUND、5 = ERROR_ACCESS_DENIED），
/// 不是 HRESULT；windows-rs 的 `Error::code()` 会被还原成 Win32 码。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnError {
    pub stage: Stage,
    pub code: u32,
    pub message: String,
}

impl SpawnError {
    /// 从 windows-rs 错误构造。
    pub(crate) fn win32(stage: Stage, error: &WinError) -> Self {
        Self {
            stage,
            code: win32_code(error),
            message: error.message(),
        }
    }

    /// 构造不来自 Win32 调用的错误（例如 PATH 查找失败）。
    pub(crate) fn with_code(stage: Stage, code: u32, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            message: message.into(),
        }
    }

    /// 失败阶段。
    pub fn stage(&self) -> Stage {
        self.stage
    }

    /// 原始 Win32 错误码。
    pub fn code(&self) -> u32 {
        self.code
    }
}

impl fmt::Display for SpawnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{}失败（Win32 错误 {}）：{}",
            self.stage, self.code, self.message
        )
    }
}

impl std::error::Error for SpawnError {}

/// 把 windows-rs 的 HRESULT 还原成 Win32 错误码。
///
/// `Error::from_thread` 用 `HRESULT::from_win32` 包装 GetLastError，
/// 形如 `0x8007_00xx`；其余（HRESULT/COM）错误原样返回。
pub(crate) fn win32_code(error: &WinError) -> u32 {
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}
