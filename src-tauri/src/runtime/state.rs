//! 运行时对外暴露的状态、错误与事件。
//!
//! 这里只有值类型：不持有线程或 Win32 句柄，可以 `Clone` 后跨线程传递。
//! 后续 Engine 可以直接把 [`RuntimeSnapshot`] 合并进全局快照，或把
//! [`RuntimeEvent`] 转发到前端；本层不依赖 Tauri。

use std::fmt;
use std::path::PathBuf;
use std::time::{Instant, SystemTime};

use crate::logs::LogStats;

/// 任务运行状态。
///
/// 正常迁移只有一条主线：
/// `stopped → starting → running → stopping → stopped`。
/// 启动失败时从 `starting` 直接回到 `stopped`（原因见 `last_error`）；
/// 进程自然退出时从 `running` 直接回到 `stopped`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// 没有进程；可以再次启动。
    Stopped,
    /// 正在做端口预检/创建进程/日志/监控，尚未就绪。
    Starting,
    /// 进程已启动，日志与退出监控均已就绪。
    Running,
    /// 已请求停止，等待进程退出与日志收尾。
    Stopping,
}

impl TaskStatus {
    /// 稳定的英文标识（快照/日志/前端共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
        }
    }
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 本次实例结束的原因。
///
/// M2-4a 只负责如实记录；M3 的自动重启/通知据此区分“用户主动停止”与
/// “进程自行退出”，避免把用户操作当成故障。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitReason {
    /// 进程自行退出（退出码可能为 0 或非 0）。
    Natural,
    /// 通过 [`super::TaskRuntime::stop`] 请求停止。
    UserStop,
}

impl ExitReason {
    /// 稳定的英文标识（快照/日志/前端共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Natural => "natural",
            Self::UserStop => "user_stop",
        }
    }
}

impl fmt::Display for ExitReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 运行时失败阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeStage {
    /// 启动前的 TCP 端口预检（查询失败或端口已被监听）。
    PortCheck,
    /// 解析命令或 `CreateProcessW`。
    Spawn,
    /// 启动日志采集（建目录/建线程）。
    Logs,
    /// 启动退出监控或任务完成线程。
    Watch,
    /// `ExitWatcher::request_stop` 或等待收尾超时。
    Stop,
    /// 当前状态不允许该操作（例如运行中再次启动）。
    InvalidState,
}

impl RuntimeStage {
    /// 稳定的英文标识（命令错误 DTO/前端共用）。
    pub fn key(self) -> &'static str {
        match self {
            Self::PortCheck => "port_check",
            Self::Spawn => "spawn",
            Self::Logs => "logs",
            Self::Watch => "watch",
            Self::Stop => "stop",
            Self::InvalidState => "invalid_state",
        }
    }

    /// 面向日志/界面的中文阶段名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PortCheck => "端口预检",
            Self::Spawn => "启动进程",
            Self::Logs => "启动日志采集",
            Self::Watch => "启动退出监控",
            Self::Stop => "停止任务",
            Self::InvalidState => "任务状态检查",
        }
    }
}

impl fmt::Display for RuntimeStage {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// 运行时错误：阶段 + 可选错误码 + 说明。
///
/// `code` 是底层原始错误码（Win32 或 `CreateProcessW` 的码），端口预检失败时
/// 也可能为空。上层据此做结构化诊断，而不是只看到一段字符串。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeError {
    /// 失败阶段。
    pub stage: RuntimeStage,
    /// 原始错误码；没有对应系统码时为 `None`。
    pub code: Option<u32>,
    /// 面向日志/界面的说明。
    pub message: String,
}

impl RuntimeError {
    pub(crate) fn new(stage: RuntimeStage, code: Option<u32>, message: impl Into<String>) -> Self {
        Self {
            stage,
            code,
            message: message.into(),
        }
    }

    /// 失败阶段。
    pub fn stage(&self) -> RuntimeStage {
        self.stage
    }

    /// 原始错误码。
    pub fn code(&self) -> Option<u32> {
        self.code
    }

    /// 错误说明。
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for RuntimeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(f, "{}失败（错误码 {}）：{}", self.stage, code, self.message),
            None => write!(f, "{}失败：{}", self.stage, self.message),
        }
    }
}

impl std::error::Error for RuntimeError {}

/// 一次读取的完整任务快照。
///
/// 字段都是复制/克隆出来的值：拿到快照后不再持有运行时锁，也不会阻止
/// 后续状态变化。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeSnapshot {
    /// 任务 id（与 `TaskSpec::id` 一致）。
    pub id: String,
    /// 当前状态。
    pub status: TaskStatus,
    /// 当前根进程 PID；`stopped` 时为 `None`。
    pub pid: Option<u32>,
    /// 最近一次启动分配的实例 id；启动失败时也会保留本次尝试的 id。
    pub instance_id: Option<String>,
    /// 最近一次成功启动的时间；`stopped` 后仍保留，便于诊断。
    pub started_at: Option<SystemTime>,
    /// 最近一次实例的根进程退出码；未退出或启动失败时为 `None`。
    pub exit_code: Option<u32>,
    /// 最近一次实例的结束原因；启动失败时为 `None`。
    pub exit_reason: Option<ExitReason>,
    /// 退出事件发出时 Job 内是否已无活动进程。
    pub tree_empty: Option<bool>,
    /// 退出前是否因树未空而强制 `TerminateJobObject`。
    pub forced_kill: bool,
    /// 最近一次启动失败的原因；成功启动后清空。
    pub last_error: Option<RuntimeError>,
    /// 日志采集统计；运行中为实时值，停止后为最终值。
    pub logs: LogStats,
    /// 当前活动日志文件（`<dir>/current.log`）。
    pub log_path: PathBuf,
}

/// 运行时事件（可选）。
///
/// 通过 [`super::TaskRuntime::with_events`] 注册 `Sender` 后，每次状态迁移都会
/// 发送一个事件。发送是尽力而为的：接收端已关闭不会影响任务本身。
#[derive(Debug, Clone)]
pub enum RuntimeEvent {
    /// 已分配实例并进入 `starting`（尚未做端口预检）。
    Starting(RuntimeSnapshot),
    /// 进程、日志与退出监控均已就绪。
    ///
    /// 携带实例 id 与进程创建成功时刻（单调时钟），Engine 用后者作为
    /// `startup_grace` 的基线；`heartbeat_cleanup_error` 记录启动前删除旧心跳
    /// 文件的失败原因（成功/未配置为 `None`，不阻止启动）。
    Running {
        /// 本次实例 id（与 `snapshot.instance_id` 相同）。
        instance_id: String,
        /// 进程创建成功时的单调时钟时刻。
        started_instant: Instant,
        /// 启动前清理旧心跳失败的原因。
        heartbeat_cleanup_error: Option<String>,
        /// 进入 `running` 时的完整快照。
        snapshot: RuntimeSnapshot,
    },
    /// 收到停止请求，进入 `stopping`。
    Stopping(RuntimeSnapshot),
    /// 已停止（自然退出或用户停止）。
    Stopped(RuntimeSnapshot),
    /// 启动失败，已回滚到 `stopped`。
    StartFailed(RuntimeSnapshot),
}

impl RuntimeEvent {
    /// 事件对应的快照。
    pub fn snapshot(&self) -> &RuntimeSnapshot {
        match self {
            Self::Starting(snapshot)
            | Self::Running { snapshot, .. }
            | Self::Stopping(snapshot)
            | Self::Stopped(snapshot)
            | Self::StartFailed(snapshot) => snapshot,
        }
    }

    /// 事件对应的状态。
    pub fn status(&self) -> TaskStatus {
        self.snapshot().status
    }
}
