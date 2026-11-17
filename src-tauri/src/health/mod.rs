//! M3-1：三档健康探测与评估核心（不依赖 Tauri/Engine/UI）。
//!
//! - [`ProcessProbe`]：按 PID 显式查询存活（process 模式/诊断）；
//! - [`HeartbeatProbe`]：JSON 或单行 ISO-8601，内容变化 + `max_age` 判新鲜；
//! - [`HttpProbe`]：`GET http://…`，不跟随 3xx，状态码 + body 断言；
//! - [`HealthEvaluator`]/[`HealthMonitor`]：启动宽限、连续失败阈值、成功清零、
//!   needs_human 正交标志、suspend/resume/reset_observation。
//!
//! 本层不调度探测、不重启进程、不写任务日志；Engine（M3-4b）的调度线程
//! 按 `interval_ms` 在锁外调用 [`HealthMonitor::check`]，再回锁按 instance_id
//! 应用结果。所有时间由调用方传入单调时钟，探针的文件读取与 HTTP 传输均可
//! 注入，测试不需要真实网络/教务。

mod heartbeat;
mod http;
mod process;
mod state;

#[cfg(test)]
mod tests;

pub use heartbeat::{
    FsHeartbeatSource, HEARTBEAT_LIMIT, HeartbeatProbe, HeartbeatReport, HeartbeatSource,
    HeartbeatSpec,
};
pub use http::{
    BODY_LIMIT, HttpProbe, HttpRequest, HttpResponse, HttpSpec, HttpTransport, HttpTransportError,
    TcpHttpTransport,
};
pub use process::{ProcessProbe, ProcessQuery, SystemProcessQuery};
pub use state::{
    BusinessSignal, FailureKind, HealthEvaluator, HealthFailure, HealthMonitor, HealthPolicy,
    HealthProbe, HealthState, HealthVerdict, HealthWarning, ProbeOutcome, ProbeReport,
};
