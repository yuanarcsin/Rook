//! C6：资源守卫的进程采样与"连续超限"判定。
//!
//! 这里只做两件互不依赖的事：
//!
//! 1. [`sample`]：对给定 PID 做一次**只读**采样（`OpenProcess` +
//!    `GetProcessMemoryInfo` / `GetProcessTimes`），失败一律返回 `None`；
//! 2. [`LimitTracker`]：把"是否超限"的逐次结论折算成"连续 N 次才动手"。
//!
//! 采样失败必须当作"本轮无结论"而不是"正常运行"：进程刚退出、权限不足、
//! PID 复用都会让 `OpenProcess` 失败。引擎侧对此的处理是清空基线，
//! 下次从头开始算，绝不会因为采样失败而停止任务。

use std::time::Instant;

use windows::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::Threading::{
    GetProcessTimes, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
};

/// 一次进程采样。
///
/// `cpu_time_100ns` 是**累计** CPU 时间（内核 + 用户，100ns 单位），
/// 百分比由调用方用两次采样的差值 / 经过时间算出来。
#[derive(Debug, Clone, Copy)]
pub struct ProcessSample {
    /// 采样时刻（单调时钟）。
    pub at: Instant,
    /// 当前工作集（`WorkingSetSize`），字节。
    pub rss_bytes: u64,
    /// 累计 CPU 时间（kernel + user），100ns 单位。
    pub cpu_time_100ns: u64,
}

/// 采样一个进程；任何失败都返回 `None`。
///
/// 需要的权限只有 `PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ`：
/// 前者在当前用户可见的进程上都可用（不要求提权），后者是
/// `GetProcessMemoryInfo` 的必要条件。句柄用 [`HandleGuard`] 保证关闭。
pub fn sample(pid: u32) -> Option<ProcessSample> {
    if pid == 0 {
        return None;
    }
    // SAFETY: 只是打开一个只读查询句柄；失败返回 Err，不产生悬垂指针。
    let handle = unsafe {
        OpenProcess(
            PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_VM_READ,
            false,
            pid,
        )
    }
    .ok()?;
    let guard = HandleGuard(handle);

    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: `counters.cb` 已按结构体实际大小填写，句柄由 guard 持有且有效。
    unsafe {
        GetProcessMemoryInfo(guard.0, &mut counters, counters.cb).ok()?;
    }

    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    // SAFETY: 四个出参都是本栈上的可写 `FILETIME`。
    unsafe {
        GetProcessTimes(guard.0, &mut creation, &mut exit, &mut kernel, &mut user).ok()?;
    }

    Some(ProcessSample {
        at: Instant::now(),
        rss_bytes: counters.WorkingSetSize as u64,
        cpu_time_100ns: filetime_100ns(kernel).saturating_add(filetime_100ns(user)),
    })
}

/// `FILETIME` → 100ns 计数（高低位拼成 u64）。
fn filetime_100ns(time: FILETIME) -> u64 {
    (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime)
}

/// 句柄 RAII：Drop 时 `CloseHandle`，保证任何提前返回都不泄漏句柄。
struct HandleGuard(HANDLE);

impl Drop for HandleGuard {
    fn drop(&mut self) {
        if !self.0.is_invalid() {
            // 关闭失败无法恢复，也不该在 Drop 里 panic。
            let _ = unsafe { CloseHandle(self.0) };
        }
    }
}

/// "连续 N 次超限"计数器。
///
/// 单次峰值（GC、批量导入、启动扫盘）不应杀常驻任务，所以只有连续
/// [`LimitTracker::THRESHOLD`] 次采样都超限才返回 `true`；返回 `true` 时
/// 计数清零，避免停止失败后每一轮都重复触发。
#[derive(Debug, Default)]
pub struct LimitTracker {
    over: u8,
}

impl LimitTracker {
    /// 连续多少次超限才判定为真的超限。
    pub const THRESHOLD: u8 = 3;

    /// 记录一次采样结论；连续第 `THRESHOLD` 次超限返回 `true` 并清零。
    ///
    /// 未超限立即清零（中间恢复过一次就重新计数）。
    pub fn observe(&mut self, over: bool) -> bool {
        if !over {
            self.over = 0;
            return false;
        }
        self.over = self.over.saturating_add(1);
        if self.over >= Self::THRESHOLD {
            self.over = 0;
            return true;
        }
        false
    }

    /// 清零（实例停止/重启、引擎决定停止后调用）。
    pub fn reset(&mut self) {
        self.over = 0;
    }

    /// 当前连续超限次数（测试/诊断用）。
    pub fn over_count(&self) -> u8 {
        self.over
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn two_consecutive_over_samples_do_not_trigger() {
        let mut tracker = LimitTracker::default();
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(true));
        assert_eq!(tracker.over_count(), 2);
    }

    #[test]
    fn third_consecutive_over_sample_triggers_and_clears() {
        let mut tracker = LimitTracker::default();
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(true));
        assert!(tracker.observe(true));
        assert_eq!(tracker.over_count(), 0);
        // 清零后需要重新累计 3 次。
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(true));
        assert!(tracker.observe(true));
    }

    #[test]
    fn recovery_in_between_resets_the_count() {
        let mut tracker = LimitTracker::default();
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(false));
        assert_eq!(tracker.over_count(), 0);
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(true));
        assert!(tracker.observe(true));
    }

    #[test]
    fn reset_clears_partial_count() {
        let mut tracker = LimitTracker::default();
        assert!(!tracker.observe(true));
        assert!(!tracker.observe(true));
        tracker.reset();
        assert_eq!(tracker.over_count(), 0);
        assert!(!tracker.observe(true));
    }

    #[test]
    fn sampling_rss_and_cpu_of_self_succeeds() {
        let pid = std::process::id();
        let first = sample(pid).expect("采样当前进程应成功");
        assert!(first.rss_bytes > 0, "当前进程工作集不应为 0");
        // CPU 时间允许为 0（极短命的测试进程），只校验不会 panic 且可累加。
        assert!(first.cpu_time_100ns < u64::MAX);
        // 再采一次：累计 CPU 时间不应倒退。
        let later = sample(pid).expect("再次采样当前进程应成功");
        assert!(later.cpu_time_100ns >= first.cpu_time_100ns);
    }

    #[test]
    fn sampling_unknown_pid_returns_none() {
        // 0 不是合法 PID；Windows 上 `OpenProcess` 直接失败。
        assert!(sample(0).is_none());
    }
}
