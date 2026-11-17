//! 进程探针：按 PID 显式查询存活。
//!
//! 进程退出本身由 M2 的退出监控负责；这里只回答"这个 PID 现在还活着吗"，
//! 供 Engine 在 process 模式或诊断时按需调用。PID 复用由调用方结合实例
//! id/退出事件判断，本探针不做额外校验。

use std::sync::Arc;
use std::time::Instant;

use super::state::{FailureKind, HealthProbe, ProbeReport};

/// PID 存活查询接口（可注入，默认使用 Win32）。
pub trait ProcessQuery: Send + Sync {
    /// `Ok(true)` = 进程仍存在；`Ok(false)` = 已退出/PID 不存在；
    /// `Err` = 查询本身失败。
    fn is_alive(&self, pid: u32) -> Result<bool, String>;
}

/// 默认 Win32 查询：`OpenProcess(PROCESS_SYNCHRONIZE)` + 零超时等待。
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemProcessQuery;

impl ProcessQuery for SystemProcessQuery {
    fn is_alive(&self, pid: u32) -> Result<bool, String> {
        query_is_alive(pid)
    }
}

/// 进程探针。
pub struct ProcessProbe {
    pid: u32,
    query: Arc<dyn ProcessQuery>,
}

impl ProcessProbe {
    /// 使用系统查询。
    pub fn new(pid: u32) -> Self {
        Self {
            pid,
            query: Arc::new(SystemProcessQuery),
        }
    }

    /// 注入自定义查询（测试/诊断）。
    pub fn with_query(pid: u32, query: Arc<dyn ProcessQuery>) -> Self {
        Self { pid, query }
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }
}

impl HealthProbe for ProcessProbe {
    fn probe(&mut self, _now: Instant) -> ProbeReport {
        let started = Instant::now();
        match self.query.is_alive(self.pid) {
            Ok(true) => ProbeReport::healthy(started.elapsed()),
            Ok(false) => ProbeReport::failed(
                FailureKind::ProcessExited,
                format!("PID {} 已退出或不存在", self.pid),
                started.elapsed(),
            ),
            Err(message) => ProbeReport::failed(
                FailureKind::ProcessQuery,
                format!("查询 PID {} 失败：{message}", self.pid),
                started.elapsed(),
            ),
        }
    }
}

#[cfg(windows)]
fn query_is_alive(pid: u32) -> Result<bool, String> {
    use windows::Win32::Foundation::{
        CloseHandle, ERROR_ACCESS_DENIED, ERROR_INVALID_PARAMETER, WAIT_OBJECT_0, WAIT_TIMEOUT,
    };
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };
    use windows::core::Error as WinError;

    // PID 0 是 Idle/System 伪进程，不能作为普通进程查询。
    if pid == 0 {
        return Ok(false);
    }

    let handle = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(handle) => handle,
        Err(error) => {
            let code = win32_code(&error);
            if code == ERROR_INVALID_PARAMETER.0 {
                return Ok(false);
            }
            if code == ERROR_ACCESS_DENIED.0 {
                // 进程存在但受保护（系统进程/其他用户），按存活处理。
                return Ok(true);
            }
            return Err(format!(
                "OpenProcess 失败（Win32 错误 {code}）：{}",
                error.message()
            ));
        }
    };

    let event = unsafe { WaitForSingleObject(handle, 0) };
    let _ = unsafe { CloseHandle(handle) };
    if event == WAIT_OBJECT_0 {
        Ok(false)
    } else if event == WAIT_TIMEOUT {
        Ok(true)
    } else {
        Err(format!(
            "WaitForSingleObject 失败：{}",
            WinError::from_thread().message()
        ))
    }
}

/// Win32 错误码：`windows::core::Error` 对 Win32 失败返回 `HRESULT_FROM_WIN32`。
#[cfg(windows)]
fn win32_code(error: &windows::core::Error) -> u32 {
    let hresult = error.code().0 as u32;
    if hresult & 0xFFFF_0000 == 0x8007_0000 {
        hresult & 0xFFFF
    } else {
        hresult
    }
}

#[cfg(not(windows))]
fn query_is_alive(_pid: u32) -> Result<bool, String> {
    Err("process 探针仅支持 Windows".to_owned())
}
