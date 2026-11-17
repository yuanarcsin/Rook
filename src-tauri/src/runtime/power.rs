//! M3-6b：系统休眠/唤醒与时钟跳变（纯逻辑 + Windows 通知接线）。
//!
//! 本模块回答两个问题，都不依赖 Engine/Tauri：
//!
//! 1. 当前处于 running / suspended / resumed 哪个阶段（[`PowerTracker`]）；
//! 2. 一次 tick 的墙钟增量与单调时钟增量偏差是否超过阈值（时钟跳变，
//!    见 docs/03 §9：> 5s 视为 NTP 校正/手动改时间）。
//!
//! 计时器的平移规则也在本模块（[`shift_timer`] / [`resume_deadline`]）：
//! **睡眠期间单调计时器一律冻结**，唤醒后保持剩余时间不变，并额外给一轮
//! `power.resume_grace_ms`——休眠时长既不算健康失败，也不算退避已到期。
//!
//! Windows 接线用 [`PowerWiring`]：`PowerRegisterSuspendResumeNotification` +
//! `DEVICE_NOTIFY_CALLBACK`（无窗口、无消息循环），回调只写 [`PowerSignalSink`]
//! 的原子标志，由调度线程在下一轮 tick 取走。注册失败只返回错误说明，调用方
//! 降级为纯时钟跳变检测。

use std::sync::Arc;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use windows::Win32::System::Power::{DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY};

/// 时钟跳变阈值：墙钟增量与单调增量偏差超过它才算跳变（docs/03 §9）。
pub const CLOCK_JUMP_THRESHOLD: Duration = Duration::from_secs(5);

/// 系统电源阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PowerState {
    /// 正常运行（含刚启动、从未收到休眠事件）。
    Running,
    /// 已收到 suspend 事件，计时器冻结中。
    Suspended,
    /// 已从休眠恢复；下一次 tick 回到 `Running`。
    Resumed,
}

/// 时钟跳变方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClockJumpDirection {
    /// 墙钟向前跳（时间被调晚 / NTP 前跳）。
    Forward,
    /// 墙钟向后跳（时间被调早 / NTP 后跳）。
    Backward,
}

impl ClockJumpDirection {
    /// 稳定的英文标识（日志共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Forward => "forward",
            Self::Backward => "backward",
        }
    }
}

/// 一次时钟跳变：方向 + 偏差幅度。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ClockJump {
    pub direction: ClockJumpDirection,
    /// 偏差绝对值（墙钟增量与单调增量之差）。
    pub magnitude: Duration,
}

impl ClockJump {
    /// 带符号的秒数（向前为正），仅用于日志。
    pub fn signed_seconds(self) -> f64 {
        let magnitude = self.magnitude.as_secs_f64();
        match self.direction {
            ClockJumpDirection::Forward => magnitude,
            ClockJumpDirection::Backward => -magnitude,
        }
    }
}

/// 比较一段区间两端的墙钟/单调增量，检测区间内是否发生时钟跳变。
///
/// 用于在途健康探测：探测开始与结束之间若系统时间被调/NTP 跳变，
/// 单靠 Engine epoch 与回调 generation 无法发现（时钟跳变没有回调），
/// 必须在应用结果前用两端样本再判一次。
pub fn clock_skew_between(
    start_mono: Instant,
    start_wall: SystemTime,
    end_mono: Instant,
    end_wall: SystemTime,
    threshold: Duration,
) -> Option<ClockJump> {
    let (forward, wall_delta) = match end_wall.duration_since(start_wall) {
        Ok(delta) => (true, delta),
        Err(error) => (false, error.duration()),
    };
    let mono_delta = end_mono.saturating_duration_since(start_mono);
    let skew_millis = if forward {
        signed_millis(wall_delta) - signed_millis(mono_delta)
    } else {
        -signed_millis(wall_delta) - signed_millis(mono_delta)
    };
    if skew_millis.unsigned_abs() <= threshold.as_millis() {
        return None;
    }
    Some(ClockJump {
        direction: if skew_millis > 0 {
            ClockJumpDirection::Forward
        } else {
            ClockJumpDirection::Backward
        },
        magnitude: Duration::from_millis(skew_millis.unsigned_abs() as u64),
    })
}

/// 回调线程捕获的事件时刻（单调 + 墙钟）。
///
/// 同轮取到 suspend+resume 时必须用各自捕获的时刻，而不是同一个 `now`：
/// 否则 `on_resume` 算出的单调增量为 0，已到期的退避/自动启动不会平移，
/// 唤醒后会立刻误重启（QA-M3.6b 阻断 2）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerMoment {
    pub mono: Instant,
    pub wall: SystemTime,
}

/// 一次电源事件的代际快照：引擎侧 epoch（含时钟跳变）+ 回调侧 generation。
///
/// 在途探测在取出时记录一份，回锁应用前与当前值比较；不一致说明探测跨越了
/// 休眠/唤醒/时钟跳变，结果必须丢弃（QA-M3.6b 阻断 1）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PowerStamp {
    /// Engine 侧代际：suspend/resume/时钟跳变时递增。
    pub epoch: u64,
    /// 回调侧代际：每次 suspend/resume 通知递增（无需调度线程参与）。
    pub generation: u64,
}

impl PowerStamp {
    /// 探测结果是否已跨越电源/时钟事件（`suspended` 为当前冻结状态）。
    pub fn is_stale_from(self, current: Self, suspended: bool) -> bool {
        suspended || self.epoch != current.epoch || self.generation != current.generation
    }
}

/// 一次唤醒的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PowerWake {
    /// 睡眠时长（墙钟差，用于日志）。
    pub slept: Duration,
    /// 单调时钟在睡眠期间实际走过的时长；计时器按它平移。
    ///
    /// Windows 的 QPC 是否跨 S3 继续计数并不稳定，所以不能用墙钟差直接
    /// 平移计时器：只有单调时钟真的走过的那部分才需要补回来。
    pub mono_elapsed: Duration,
}

#[derive(Debug, Clone, Copy)]
struct Sample {
    mono: Instant,
    wall: SystemTime,
}

/// 休眠/唤醒/时钟跳变跟踪器（Engine 全局一份）。
#[derive(Debug, Clone)]
pub struct PowerTracker {
    threshold: Duration,
    state: PowerState,
    /// suspend 时刻的基线；resume 用它算睡眠时长。
    suspended_at: Option<Sample>,
    /// 上一次 tick 的基线；用于时钟跳变检测。
    last: Option<Sample>,
    last_jump: Option<ClockJump>,
}

impl Default for PowerTracker {
    fn default() -> Self {
        Self::new(CLOCK_JUMP_THRESHOLD)
    }
}

impl PowerTracker {
    pub fn new(threshold: Duration) -> Self {
        Self {
            threshold,
            state: PowerState::Running,
            suspended_at: None,
            last: None,
            last_jump: None,
        }
    }

    pub fn state(&self) -> PowerState {
        self.state
    }

    pub fn is_suspended(&self) -> bool {
        self.state == PowerState::Suspended
    }

    /// 最近一次检测到的时钟跳变（诊断用；不会因后续 tick 清空）。
    pub fn last_jump(&self) -> Option<ClockJump> {
        self.last_jump
    }

    /// 收到 suspend 事件：冻结计时器基线。重复事件返回 `false`。
    pub fn on_suspend(&mut self, mono: Instant, wall: SystemTime) -> bool {
        if self.state == PowerState::Suspended {
            return false;
        }
        self.state = PowerState::Suspended;
        self.suspended_at = Some(Sample { mono, wall });
        true
    }

    /// 收到 resume 事件：返回睡眠时长与需要平移的单调时长。
    ///
    /// 没收到过 suspend（通知丢失/应用刚启动）时仍然返回一份零时长结果，
    /// 让调用方照常给 resume 宽限——宁可多给一次宽限，也不要把睡眠算成失败。
    pub fn on_resume(&mut self, mono: Instant, wall: SystemTime) -> PowerWake {
        let sample = self.suspended_at.take();
        self.state = PowerState::Resumed;
        // 唤醒时刻重设 tick 基线：否则第一轮 tick 会把睡眠当成时钟跳变。
        self.last = Some(Sample { mono, wall });
        match sample {
            Some(sample) => PowerWake {
                slept: wall_elapsed(sample.wall, wall),
                mono_elapsed: mono.saturating_duration_since(sample.mono),
            },
            None => PowerWake {
                slept: Duration::ZERO,
                mono_elapsed: Duration::ZERO,
            },
        }
    }

    /// 每轮 tick 调用：推进基线并检测时钟跳变。
    ///
    /// 冻结期间不做检测（唤醒时由 [`on_resume`](Self::on_resume) 统一处理）；
    /// 唤醒后的第一轮 tick 因为基线已重设，不会误报。
    pub fn on_tick(&mut self, mono: Instant, wall: SystemTime) -> Option<ClockJump> {
        if self.state == PowerState::Suspended {
            return None;
        }
        let previous = self.last.replace(Sample { mono, wall });
        if self.state == PowerState::Resumed {
            self.state = PowerState::Running;
        }
        let previous = previous?;

        // 偏差 = 墙钟增量 − 单调增量（墙钟倒退时两项相加取负）。
        let (forward, wall_delta) = match wall.duration_since(previous.wall) {
            Ok(delta) => (true, delta),
            Err(error) => (false, error.duration()),
        };
        let mono_delta = mono.saturating_duration_since(previous.mono);
        let skew_millis = if forward {
            signed_millis(wall_delta) - signed_millis(mono_delta)
        } else {
            -signed_millis(wall_delta) - signed_millis(mono_delta)
        };
        if skew_millis.unsigned_abs() <= self.threshold.as_millis() {
            return None;
        }
        let jump = ClockJump {
            direction: if skew_millis > 0 {
                ClockJumpDirection::Forward
            } else {
                ClockJumpDirection::Backward
            },
            magnitude: Duration::from_millis(skew_millis.unsigned_abs() as u64),
        };
        self.last_jump = Some(jump);
        Some(jump)
    }
}

/// 单调计时器平移：冻结 `delta` 时长，保持剩余时间不变。
pub fn shift_timer(at: Option<Instant>, delta: Duration) -> Option<Instant> {
    if delta.is_zero() {
        return at;
    }
    at.map(|at| at.checked_add(delta).unwrap_or(at))
}

/// 唤醒/时钟跳变后的下一次探测或宽限截止时间。
///
/// 取 `deadline + slept` 与 `now + grace` 的较大者：既保留睡眠前的剩余时间，
/// 也保证宽限期内不会再被判定失败（docs/03 §9 / docs/04 §5）。
pub fn resume_deadline(
    deadline: Option<Instant>,
    slept: Duration,
    now: Instant,
    grace: Duration,
) -> Option<Instant> {
    let grace_until = now.checked_add(grace).unwrap_or(now);
    deadline.map(|at| shift_timer(Some(at), slept).unwrap_or(at).max(grace_until))
}

fn signed_millis(duration: Duration) -> i128 {
    duration.as_millis() as i128
}

fn wall_elapsed(from: SystemTime, to: SystemTime) -> Duration {
    match to.duration_since(from) {
        Ok(delta) => delta,
        Err(error) => error.duration(),
    }
}

/// 进程级单调时钟基准：把 `Instant` 折算成可放进 `AtomicU64` 的毫秒。
///
/// 回调可能在不同系统线程上执行，无法共享 `Instant`；用同一个基准换算后
/// 取走方可以还原出可比较的时刻。
fn mono_base() -> Instant {
    static BASE: OnceLock<Instant> = OnceLock::new();
    *BASE.get_or_init(Instant::now)
}

fn mono_millis(at: Instant) -> u64 {
    at.saturating_duration_since(mono_base()).as_millis() as u64
}

fn mono_from_millis(millis: u64) -> Instant {
    mono_base()
        .checked_add(Duration::from_millis(millis))
        .unwrap_or_else(Instant::now)
}

fn wall_millis(wall: SystemTime) -> i64 {
    match wall.duration_since(SystemTime::UNIX_EPOCH) {
        Ok(delta) => delta.as_millis() as i64,
        Err(error) => -(error.duration().as_millis() as i64),
    }
}

fn wall_from_millis(millis: i64) -> SystemTime {
    if millis >= 0 {
        SystemTime::UNIX_EPOCH + Duration::from_millis(millis as u64)
    } else {
        SystemTime::UNIX_EPOCH - Duration::from_millis(millis.unsigned_abs())
    }
}

/// 回调写入、调度线程取走的电源事件标志。
///
/// 回调可能运行在系统线程上，因此只用原子量，不持锁、不分配：
/// - `generation`：每次通知递增，供在途探测判新旧（无锁可读）；
/// - `suspended`：电平状态（suspend 置位、resume 清除），不随 `take` 清空，
///   让调度线程在任何时刻都能判断"当前是否睡眠中"；
/// - 各事件的单调/墙钟毫秒：同轮取到 suspend+resume 时仍能算出真实睡眠时长。
#[derive(Debug, Default)]
pub struct PowerSignalSink {
    suspend: AtomicBool,
    resume: AtomicBool,
    suspended: AtomicBool,
    generation: AtomicU64,
    suspend_mono_ms: AtomicU64,
    suspend_wall_ms: AtomicI64,
    resume_mono_ms: AtomicU64,
    resume_wall_ms: AtomicI64,
}

impl PowerSignalSink {
    /// 记录一次 suspend 事件（回调线程）。
    pub fn notify_suspend(&self) {
        self.suspend_mono_ms
            .store(mono_millis(Instant::now()), Ordering::Relaxed);
        self.suspend_wall_ms
            .store(wall_millis(SystemTime::now()), Ordering::Relaxed);
        self.suspended.store(true, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.suspend.store(true, Ordering::Release);
    }

    /// 记录一次 resume 事件（回调线程）。
    pub fn notify_resume(&self) {
        self.resume_mono_ms
            .store(mono_millis(Instant::now()), Ordering::Relaxed);
        self.resume_wall_ms
            .store(wall_millis(SystemTime::now()), Ordering::Relaxed);
        self.suspended.store(false, Ordering::Relaxed);
        self.generation.fetch_add(1, Ordering::AcqRel);
        self.resume.store(true, Ordering::Release);
    }

    /// 回调侧代际（每次 suspend/resume 递增）。
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// 是否还有未取走的事件。
    pub fn pending(&self) -> bool {
        self.suspend.load(Ordering::Acquire) || self.resume.load(Ordering::Acquire)
    }

    /// 当前是否处于睡眠中（resume 之前一直为真）。
    pub fn is_suspended(&self) -> bool {
        self.suspended.load(Ordering::Acquire)
    }

    /// 取走并清空全部待处理事件。
    pub fn take(&self) -> PowerSignals {
        let suspend = self.suspend.swap(false, Ordering::AcqRel);
        let resume = self.resume.swap(false, Ordering::AcqRel);
        PowerSignals {
            suspend: suspend.then(|| self.suspend_moment()),
            resume: resume.then(|| self.resume_moment()),
        }
    }

    fn suspend_moment(&self) -> PowerMoment {
        PowerMoment {
            mono: mono_from_millis(self.suspend_mono_ms.load(Ordering::Acquire)),
            wall: wall_from_millis(self.suspend_wall_ms.load(Ordering::Acquire)),
        }
    }

    fn resume_moment(&self) -> PowerMoment {
        PowerMoment {
            mono: mono_from_millis(self.resume_mono_ms.load(Ordering::Acquire)),
            wall: wall_from_millis(self.resume_wall_ms.load(Ordering::Acquire)),
        }
    }
}

/// 一轮 tick 取到的电源事件（suspend 先于 resume 处理）。
///
/// 每个事件都带回调时刻：调度线程用 `suspend_at.mono` / `resume_at.mono`
/// 分别推进 [`PowerTracker`]，同轮到达时也能得到真实的单调增量。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PowerSignals {
    pub suspend: Option<PowerMoment>,
    pub resume: Option<PowerMoment>,
}

impl PowerSignals {
    pub fn is_empty(self) -> bool {
        self.suspend.is_none() && self.resume.is_none()
    }
}

/// Engine 持有的电源通知接线。
///
/// 注册失败时 `signals` 为 `None`，Engine 退化为纯时钟跳变检测；
/// 结构体 Drop 时注销通知（应用退出路径）。
#[derive(Default)]
pub struct PowerWiring {
    signals: Option<Arc<PowerSignalSink>>,
    /// 只用于持有注册句柄：Drop 时注销通知（字段本身不被读取）。
    #[allow(dead_code)]
    registration: Option<SuspendResumeRegistration>,
    error: Option<String>,
}

impl PowerWiring {
    /// 注册 Windows 休眠/唤醒通知；失败时返回可记录的说明，不阻止启动。
    pub fn install() -> Self {
        let sink = Arc::new(PowerSignalSink::default());
        match SuspendResumeRegistration::register(Arc::clone(&sink)) {
            Ok(registration) => Self {
                signals: Some(sink),
                registration: Some(registration),
                error: None,
            },
            Err(error) => Self {
                signals: None,
                registration: None,
                error: Some(error),
            },
        }
    }

    /// 测试用：不注册系统通知的空接线。
    ///
    /// 行为等价于“注册失败后只退化为时钟跳变检测”，不产生任何系统调用。
    #[cfg(test)]
    pub fn for_test() -> Self {
        Self {
            signals: None,
            registration: None,
            error: None,
        }
    }

    /// 注册失败原因（供 Engine 记 warn）。
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    /// 取走回调线程累积的事件；未注册时恒为空。
    pub fn take_signals(&self) -> PowerSignals {
        self.signals
            .as_ref()
            .map(|sink| sink.take())
            .unwrap_or_default()
    }

    /// 回调侧代际（未注册时恒为 0）。无锁，可在持有 Engine 锁时读取。
    pub fn generation(&self) -> u64 {
        self.signals.as_ref().map_or(0, |sink| sink.generation())
    }

    /// 是否有尚未处理的 suspend/resume 通知。
    pub fn pending(&self) -> bool {
        self.signals.as_ref().is_some_and(|sink| sink.pending())
    }

    /// 当前是否处于睡眠中（resume 通知到达前一直为真）。
    pub fn is_suspended(&self) -> bool {
        self.signals
            .as_ref()
            .is_some_and(|sink| sink.is_suspended())
    }
}

/// `PowerRegisterSuspendResumeNotification` 的注册句柄。
struct SuspendResumeRegistration {
    handle: HPOWERNOTIFY,
    /// 订阅参数必须保持稳定地址直到注销；Box 保证堆地址不变。
    #[allow(dead_code)]
    params: Box<DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS>,
    /// 回调 context 指向它，必须活到注销之后。
    #[allow(dead_code)]
    sink: Arc<PowerSignalSink>,
}

// 安全性：结构体只持有堆地址稳定的 `Box`、共享的 `Arc` 与不透明的 isize 句柄；
// 跨线程移动不会让任何指针失效，而 `Engine` 本身由 `Mutex` 串行访问。
unsafe impl Send for SuspendResumeRegistration {}

impl SuspendResumeRegistration {
    fn register(sink: Arc<PowerSignalSink>) -> Result<Self, String> {
        use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
        use windows::Win32::System::Power::{
            DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS, HPOWERNOTIFY,
            PowerRegisterSuspendResumeNotification,
        };
        use windows::Win32::UI::WindowsAndMessaging::DEVICE_NOTIFY_CALLBACK;

        let context = Arc::as_ptr(&sink) as *mut core::ffi::c_void;
        let mut params = Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
            Callback: Some(power_callback),
            Context: context,
        });
        let recipient = HANDLE(params.as_mut() as *mut _ as *mut core::ffi::c_void);
        let mut raw: *mut core::ffi::c_void = core::ptr::null_mut();

        // SAFETY：recipient 指向本函数内 Box 的 DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS，
        // 该 Box 随后由返回值持有，地址在注销前保持稳定。
        let result = unsafe {
            PowerRegisterSuspendResumeNotification(
                DEVICE_NOTIFY_CALLBACK,
                recipient,
                &mut raw as *mut *mut core::ffi::c_void,
            )
        };
        if result != ERROR_SUCCESS {
            return Err(format!(
                "PowerRegisterSuspendResumeNotification 失败：Win32 错误 {}",
                result.0
            ));
        }
        Ok(Self {
            handle: HPOWERNOTIFY(raw as isize),
            params,
            sink,
        })
    }
}

impl Drop for SuspendResumeRegistration {
    fn drop(&mut self) {
        use windows::Win32::System::Power::PowerUnregisterSuspendResumeNotification;

        if self.handle.is_invalid() {
            return;
        }
        // SAFETY：句柄来自本结构体的成功注册，且只注销一次。
        let result = unsafe { PowerUnregisterSuspendResumeNotification(self.handle) };
        if result.0 != 0 {
            tracing::warn!(code = result.0, "注销休眠/唤醒通知失败");
        }
    }
}

/// `DEVICE_NOTIFY_CALLBACK` 回调：只写原子标志，立即返回。
///
/// # Safety
/// `context` 必须是 [`SuspendResumeRegistration`] 持有的 `Arc<PowerSignalSink>`
/// 裸指针，并在注销前一直有效。
unsafe extern "system" fn power_callback(
    context: *const core::ffi::c_void,
    event_type: u32,
    _setting: *const core::ffi::c_void,
) -> u32 {
    use windows::Win32::UI::WindowsAndMessaging::{
        PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND, PBT_APMSUSPEND,
    };

    if context.is_null() {
        return 0;
    }
    // SAFETY：由注册方保证 context 指向存活的 PowerSignalSink。
    let sink = unsafe { &*(context as *const PowerSignalSink) };
    match event_type {
        PBT_APMSUSPEND => sink.notify_suspend(),
        PBT_APMRESUMEAUTOMATIC | PBT_APMRESUMESUSPEND => sink.notify_resume(),
        _ => {}
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> (Instant, SystemTime) {
        (
            Instant::now(),
            SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000),
        )
    }

    fn ms(value: u64) -> Duration {
        Duration::from_millis(value)
    }

    #[test]
    fn tracker_starts_running_and_normal_ticks_do_not_jump() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        assert_eq!(tracker.state(), PowerState::Running);

        assert_eq!(tracker.on_tick(mono, wall), None);
        // 墙钟与单调时钟同步推进 250ms：没有跳变。
        let jump = tracker.on_tick(mono + ms(250), wall + ms(250));
        assert_eq!(jump, None);
        assert_eq!(tracker.state(), PowerState::Running);
        assert!(tracker.last_jump().is_none());
    }

    #[test]
    fn small_skew_below_threshold_is_not_a_jump() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        // 偏差 4.9s < 5s 阈值：容忍（NTP 微调/调度抖动）。
        let jump = tracker.on_tick(mono + ms(100), wall + ms(5_000));
        assert_eq!(jump, None);
    }

    #[test]
    fn forward_clock_jump_is_reported_with_magnitude() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        let jump = tracker
            .on_tick(mono + ms(250), wall + Duration::from_secs(3600))
            .expect("应检测到正向跳变");
        assert_eq!(jump.direction, ClockJumpDirection::Forward);
        assert_eq!(jump.magnitude, Duration::from_secs(3600) - ms(250));
        assert!(jump.signed_seconds() > 0.0);
        assert_eq!(tracker.last_jump(), Some(jump));
    }

    #[test]
    fn backward_clock_jump_is_reported_with_magnitude() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        let jump = tracker
            .on_tick(mono + ms(250), wall - Duration::from_secs(3600))
            .expect("应检测到负向跳变");
        assert_eq!(jump.direction, ClockJumpDirection::Backward);
        assert_eq!(jump.magnitude, Duration::from_secs(3600) + ms(250));
        assert!(jump.signed_seconds() < 0.0);
    }

    #[test]
    fn suspend_freezes_state_and_resume_reports_sleep() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        assert!(tracker.on_suspend(mono + ms(250), wall + ms(250)));
        assert_eq!(tracker.state(), PowerState::Suspended);
        assert!(tracker.is_suspended());
        // 重复 suspend 幂等。
        assert!(!tracker.on_suspend(mono + ms(300), wall + ms(300)));

        // 冻结期间不检测跳变（睡眠本身不应被当成时钟跳变）。
        assert_eq!(
            tracker.on_tick(
                mono + Duration::from_secs(3600),
                wall + Duration::from_secs(3600)
            ),
            None
        );

        let wake = tracker.on_resume(
            mono + Duration::from_secs(3600),
            wall + Duration::from_secs(3600),
        );
        assert_eq!(wake.slept, Duration::from_secs(3600) - ms(250));
        assert_eq!(wake.mono_elapsed, Duration::from_secs(3600) - ms(250));
        assert_eq!(tracker.state(), PowerState::Resumed);
    }

    #[test]
    fn first_tick_after_resume_is_not_a_clock_jump() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);
        tracker.on_suspend(mono, wall);

        let resume_mono = mono + Duration::from_secs(8 * 3600);
        let resume_wall = wall + Duration::from_secs(8 * 3600);
        tracker.on_resume(resume_mono, resume_wall);

        // 唤醒后的第一轮 tick：基线已重设，睡眠时长不会被当成跳变。
        assert_eq!(
            tracker.on_tick(resume_mono + ms(250), resume_wall + ms(250)),
            None
        );
        assert_eq!(tracker.state(), PowerState::Running);
        assert!(tracker.last_jump().is_none());
    }

    #[test]
    fn resume_without_suspend_still_returns_zero_wake() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        // 通知丢失/应用刚启动：仍返回零时长结果，调用方照常给宽限。
        let wake = tracker.on_resume(mono + ms(250), wall + ms(250));
        assert_eq!(wake.slept, Duration::ZERO);
        assert_eq!(wake.mono_elapsed, Duration::ZERO);
        assert_eq!(tracker.state(), PowerState::Resumed);
    }

    #[test]
    fn shift_timer_preserves_remaining_time() {
        let (mono, _) = base();
        let deadline = mono + Duration::from_secs(30);

        // 睡眠 1 小时（单调时钟也走了 1 小时）：平移后剩余时间不变。
        let shifted = shift_timer(Some(deadline), Duration::from_secs(3600)).unwrap();
        assert_eq!(shifted, deadline + Duration::from_secs(3600));
        assert_eq!(
            shifted.saturating_duration_since(mono + Duration::from_secs(3600)),
            Duration::from_secs(30)
        );

        assert_eq!(shift_timer(None, Duration::from_secs(3600)), None);
        assert_eq!(shift_timer(Some(deadline), Duration::ZERO), Some(deadline));
    }

    #[test]
    fn resume_deadline_never_earlier_than_grace() {
        let (mono, _) = base();
        let now = mono + Duration::from_secs(3600);
        let grace = Duration::from_secs(20);

        // 探测本已到期（睡眠前就过期）：唤醒后延后到 now + grace。
        let due = resume_deadline(Some(mono), Duration::from_secs(3600), now, grace);
        assert_eq!(due, Some(now + grace));

        // 探测还差 60s：保留剩余时间（晚于宽限截止）。
        let later = mono + Duration::from_secs(3660);
        let due = resume_deadline(Some(later), Duration::from_secs(3600), now, grace);
        assert_eq!(due, Some(later + Duration::from_secs(3600)));

        assert_eq!(
            resume_deadline(None, Duration::from_secs(1), now, grace),
            None
        );
    }

    #[test]
    fn sink_collects_both_signals_and_clears() {
        let sink = PowerSignalSink::default();
        assert!(sink.take().is_empty());

        sink.notify_suspend();
        let signals = sink.take();
        assert!(signals.suspend.is_some());
        assert!(signals.resume.is_none());
        assert!(sink.take().is_empty());

        sink.notify_resume();
        sink.notify_suspend();
        let signals = sink.take();
        assert!(signals.suspend.is_some());
        assert!(signals.resume.is_some());
        assert!(sink.take().is_empty());
    }

    #[test]
    fn sink_generation_advances_and_suspended_flag_is_level_triggered() {
        let sink = PowerSignalSink::default();
        assert_eq!(sink.generation(), 0);
        assert!(!sink.pending());
        assert!(!sink.is_suspended());

        sink.notify_suspend();
        assert_eq!(sink.generation(), 1);
        assert!(sink.pending());
        assert!(sink.is_suspended());
        // 取走事件不清除电平状态：调度线程仍能判断"正在睡眠"。
        let signals = sink.take();
        assert!(signals.suspend.is_some());
        assert!(!sink.pending());
        assert!(sink.is_suspended());

        sink.notify_resume();
        assert_eq!(sink.generation(), 2);
        assert!(!sink.is_suspended());
    }

    #[test]
    fn stamp_detects_crossed_power_events() {
        let base = PowerStamp {
            epoch: 3,
            generation: 7,
        };
        assert!(!base.is_stale_from(base, false));
        assert!(base.is_stale_from(base, true));
        assert!(base.is_stale_from(
            PowerStamp {
                epoch: 4,
                generation: 7
            },
            false
        ));
        assert!(base.is_stale_from(
            PowerStamp {
                epoch: 3,
                generation: 8
            },
            false
        ));
    }

    #[test]
    fn coalesced_signals_keep_distinct_moments() {
        let sink = PowerSignalSink::default();
        sink.notify_suspend();
        // 模拟真实睡眠：两次回调之间必然有可观测的时间差。
        std::thread::sleep(ms(20));
        sink.notify_resume();

        let signals = sink.take();
        let suspend = signals.suspend.expect("应保留 suspend 时刻");
        let resume = signals.resume.expect("应保留 resume 时刻");
        assert!(resume.mono > suspend.mono);
        assert!(resume.wall > suspend.wall);

        // 同一轮里按各自捕获时刻推进：单调增量非零，计时器才会真正平移。
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);
        tracker.on_suspend(suspend.mono, suspend.wall);
        let wake = tracker.on_resume(resume.mono, resume.wall);
        assert!(wake.mono_elapsed >= ms(15));
        assert!(wake.slept >= ms(15));
        assert_eq!(tracker.state(), PowerState::Resumed);
    }

    #[test]
    fn coalesced_suspend_resume_shifts_expired_backoff() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        // 睡眠前 30s 到期的退避；睡眠 1 小时（QPC 照走）。
        let deadline = mono + Duration::from_secs(30);
        let suspend_at = mono + Duration::from_secs(1);
        let resume_at = suspend_at + Duration::from_secs(3600);
        tracker.on_suspend(suspend_at, wall + Duration::from_secs(1));
        let wake = tracker.on_resume(resume_at, wall + Duration::from_secs(3601));

        assert_eq!(wake.mono_elapsed, Duration::from_secs(3600));
        let shifted = shift_timer(Some(deadline), wake.mono_elapsed).expect("应平移");
        // 平移后剩余时间保持不变，而不是"立即到期"。
        assert_eq!(
            shifted.saturating_duration_since(resume_at),
            Duration::from_secs(29)
        );
        assert!(shifted > resume_at);

        let auto_start = resume_deadline(
            Some(deadline),
            wake.mono_elapsed,
            resume_at,
            Duration::from_secs(20),
        )
        .expect("应平移");
        assert!(auto_start > resume_at);
    }

    #[test]
    fn resume_only_signal_keeps_zero_wake() {
        let sink = PowerSignalSink::default();
        sink.notify_resume();
        let signals = sink.take();
        assert!(signals.suspend.is_none());
        let resume = signals.resume.expect("应保留 resume 时刻");

        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);
        let wake = tracker.on_resume(resume.mono, resume.wall);
        assert_eq!(wake.mono_elapsed, Duration::ZERO);
        assert_eq!(wake.slept, Duration::ZERO);
        assert_eq!(tracker.state(), PowerState::Resumed);
    }

    #[test]
    fn duplicate_suspend_keeps_first_moment() {
        let mut tracker = PowerTracker::default();
        let (mono, wall) = base();
        tracker.on_tick(mono, wall);

        assert!(tracker.on_suspend(mono + ms(100), wall + ms(100)));
        // 重复 suspend 幂等，且不覆盖首次时刻（否则睡眠时长会被截短）。
        assert!(!tracker.on_suspend(mono + ms(900), wall + ms(900)));
        let wake = tracker.on_resume(mono + ms(1_100), wall + ms(1_100));
        assert_eq!(wake.mono_elapsed, ms(1_000));
    }

    #[test]
    fn no_signals_is_empty() {
        let sink = PowerSignalSink::default();
        let signals = sink.take();
        assert!(signals.is_empty());
        assert_eq!(signals, PowerSignals::default());
    }

    #[test]
    fn probe_span_detects_clock_jump_without_callback() {
        let (mono, wall) = base();
        // 探测期间墙钟向前跳 1 小时：两端差值远超阈值。
        let jump = clock_skew_between(
            mono,
            wall,
            mono + ms(200),
            wall + Duration::from_secs(3_600),
            CLOCK_JUMP_THRESHOLD,
        )
        .expect("应检测到探测期间的时钟跳变");
        assert_eq!(jump.direction, ClockJumpDirection::Forward);
        // 正常探测（墙钟与单调同步）不误报。
        assert_eq!(
            clock_skew_between(
                mono,
                wall,
                mono + ms(250),
                wall + ms(250),
                CLOCK_JUMP_THRESHOLD,
            ),
            None
        );
        // 负向跳变同样检测。
        let jump = clock_skew_between(
            mono,
            wall,
            mono + ms(200),
            wall - Duration::from_secs(3_600),
            CLOCK_JUMP_THRESHOLD,
        )
        .expect("应检测到负向时钟跳变");
        assert_eq!(jump.direction, ClockJumpDirection::Backward);
    }
}
