//! 重启策略与退避计算。
//!
//! 策略是 [`RestartConfig`] 的归一化只读视图：进入状态机后不再读配置。
//! 退避公式与抖动完全由参数和注入的随机数决定，不读时钟、不分配。

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::config::{RestartConfig, RestartMode};

/// 退避下限（docs/03 §6.2）。
pub const MIN_DELAY_MS: u64 = 100;

/// 抖动上限（配置校验范围 `0 - 0.5`）。
pub const MAX_JITTER: f64 = 0.5;

/// 归一化后的重启策略。
///
/// 字段与 [`RestartConfig`] 一一对应，但时间已转成 [`Duration`]，并对
/// 抖动/倍率做了防御性收敛，保证 [`backoff_delay`] 不会产生 NaN 或负数。
#[derive(Debug, Clone, PartialEq)]
pub struct RestartPolicy {
    pub mode: RestartMode,
    /// 第一次失败后的基准延迟。
    pub initial_delay: Duration,
    /// 基准延迟上限（抖动后可以略微超过）。
    pub max_delay: Duration,
    /// 每次失败后的指数倍率，`>= 1`。
    pub multiplier: f64,
    /// 均匀抖动幅度，`0..=0.5`。
    pub jitter: f64,
    /// `0` = 不熔断。
    pub max_consecutive_failures: u32,
    /// 稳定运行多久后清零 `consecutive_failures`。
    pub stable_after: Duration,
    /// 启动窗口长度；`0` = 不限（`max_starts_in_window` 也失效）。
    pub window: Duration,
    /// 窗口内最多启动次数；`0` = 不限。
    pub max_starts_in_window: u32,
    /// 命中这些退出码时进入 paused(needs_human)，不重启、不计失败。
    pub pause_on_exit_codes: Vec<u32>,
}

impl RestartPolicy {
    /// 从配置模型生成；配置层已校验范围，这里再做一次收敛。
    pub fn from_config(config: &RestartConfig) -> Self {
        let initial_delay = Duration::from_millis(config.initial_delay_ms);
        let max_delay = Duration::from_millis(config.max_delay_ms).max(initial_delay);
        let multiplier = if config.multiplier.is_finite() && config.multiplier >= 1.0 {
            config.multiplier
        } else {
            1.0
        };
        let jitter = if config.jitter.is_finite() {
            config.jitter.clamp(0.0, MAX_JITTER)
        } else {
            0.0
        };

        Self {
            mode: config.mode,
            initial_delay,
            max_delay,
            multiplier,
            jitter,
            max_consecutive_failures: config.max_consecutive_failures,
            stable_after: Duration::from_millis(config.stable_after_ms),
            // 窗口上限 24h（配置层已校验）；0 保持"不限"，不做下限收敛。
            window: Duration::from_millis(config.window_ms),
            max_starts_in_window: config.max_starts_in_window,
            pause_on_exit_codes: config.pause_on_exit_codes.clone(),
        }
    }

    /// 第 `failures` 次失败（`>= 1`）的退避延迟。
    ///
    /// `unit` 是 `[0, 1)` 的均匀随机数；越界会被夹到 `[0, 1]`，
    /// 因此 `0.0`/`1.0` 可以直接用来测试抖动下界/上界。
    pub fn delay_for(&self, failures: u32, unit: f64) -> Duration {
        backoff_delay(self, failures, unit)
    }
}

impl Default for RestartPolicy {
    fn default() -> Self {
        Self::from_config(&RestartConfig::default())
    }
}

/// 退避公式（docs/03 §6.2）：
///
/// ```text
/// n     = max(failures, 1)
/// base  = min(initial * multiplier^(n-1), max)
/// delay = max(base * (1 + jitter * (2*unit - 1)), 100ms)
/// ```
///
/// 抖动只作用于基准值，所以结果可以略高于 `max_delay`（与文档示例一致）。
pub fn backoff_delay(policy: &RestartPolicy, failures: u32, unit: f64) -> Duration {
    let n = failures.max(1);
    let initial_ms = policy.initial_delay.as_secs_f64() * 1000.0;
    let max_ms = policy.max_delay.as_secs_f64() * 1000.0;

    let mut base_ms = initial_ms * policy.multiplier.powi((n - 1) as i32);
    if !base_ms.is_finite() {
        base_ms = max_ms;
    }
    let base_ms = base_ms.min(max_ms).max(0.0);

    let unit = if unit.is_finite() {
        unit.clamp(0.0, 1.0)
    } else {
        0.5
    };
    let scale = 1.0 + policy.jitter * (2.0 * unit - 1.0);
    let delay_ms = (base_ms * scale).max(MIN_DELAY_MS as f64);

    // `as u64` 在 Rust 中对超界浮点是饱和转换。
    Duration::from_millis(delay_ms.round() as u64)
}

/// 均匀随机源：只返回 `[0, 1)`。
///
/// 状态机不依赖 `rand`；生产用 [`SystemRandom`]，测试注入固定/序列值。
pub trait RandomSource {
    fn next_unit(&mut self) -> f64;
}

/// 基于时间种子的 xorshift64* 随机源（非密码学）。
#[derive(Debug, Clone)]
pub struct SystemRandom {
    state: u64,
}

impl SystemRandom {
    pub fn new() -> Self {
        let seed = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos() as u64)
            .unwrap_or(0x9E37_79B9_7F4A_7C15);
        Self::from_seed(seed)
    }

    /// 固定种子（诊断/复现）。
    pub fn from_seed(seed: u64) -> Self {
        // xorshift 的状态不能为 0。
        Self { state: seed | 1 }
    }
}

impl Default for SystemRandom {
    fn default() -> Self {
        Self::new()
    }
}

impl RandomSource for SystemRandom {
    fn next_unit(&mut self) -> f64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        let value = x.wrapping_mul(0x2545_F491_4F6C_DD1D);
        // 53 位尾数，结果严格小于 1。
        (value >> 11) as f64 / (1u64 << 53) as f64
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy(jitter: f64) -> RestartPolicy {
        RestartPolicy {
            jitter,
            ..RestartPolicy::default()
        }
    }

    #[test]
    fn default_policy_matches_config_defaults() {
        let policy = RestartPolicy::default();
        assert_eq!(policy.mode, RestartMode::OnFailure);
        assert_eq!(policy.initial_delay, Duration::from_millis(3000));
        assert_eq!(policy.max_delay, Duration::from_millis(60_000));
        assert_eq!(policy.multiplier, 2.0);
        assert_eq!(policy.jitter, 0.2);
        assert_eq!(policy.max_consecutive_failures, 5);
        assert_eq!(policy.stable_after, Duration::from_millis(60_000));
        assert_eq!(policy.window, Duration::from_millis(600_000));
        assert_eq!(policy.max_starts_in_window, 6);
        assert!(policy.pause_on_exit_codes.is_empty());
    }

    #[test]
    fn from_config_normalizes_invalid_values() {
        let config = RestartConfig {
            initial_delay_ms: 5000,
            max_delay_ms: 1000,
            multiplier: 0.5,
            jitter: 0.9,
            ..RestartConfig::default()
        };
        let policy = RestartPolicy::from_config(&config);
        assert_eq!(policy.initial_delay, Duration::from_millis(5000));
        // max 不得小于 initial。
        assert_eq!(policy.max_delay, Duration::from_millis(5000));
        assert_eq!(policy.multiplier, 1.0);
        assert_eq!(policy.jitter, MAX_JITTER);

        let nan = RestartPolicy::from_config(&RestartConfig {
            multiplier: f64::NAN,
            jitter: f64::NAN,
            ..RestartConfig::default()
        });
        assert_eq!(nan.multiplier, 1.0);
        assert_eq!(nan.jitter, 0.0);
    }

    #[test]
    fn exponential_backoff_matches_docs_table() {
        let policy = policy(0.0);
        for (failures, expected_ms) in [
            (1, 3000),
            (2, 6000),
            (3, 12_000),
            (4, 24_000),
            (5, 48_000),
            (6, 60_000),
            (20, 60_000),
        ] {
            assert_eq!(
                policy.delay_for(failures, 0.5),
                Duration::from_millis(expected_ms),
                "第 {failures} 次失败"
            );
        }
    }

    #[test]
    fn jitter_bounds_are_testable() {
        let max_jitter_policy = policy(0.5);
        // unit=0 → 1 - jitter = 0.5 倍；unit=1 → 1 + jitter = 1.5 倍。
        assert_eq!(
            max_jitter_policy.delay_for(1, 0.0),
            Duration::from_millis(1500)
        );
        assert_eq!(
            max_jitter_policy.delay_for(1, 1.0),
            Duration::from_millis(4500)
        );
        // 越界值夹到边界。
        assert_eq!(
            max_jitter_policy.delay_for(1, -3.0),
            Duration::from_millis(1500)
        );
        assert_eq!(
            max_jitter_policy.delay_for(1, 3.0),
            Duration::from_millis(4500)
        );

        // 抖动作用于基准值，允许超过 max_delay；0.5 是配置上限。
        assert_eq!(
            max_jitter_policy.delay_for(6, 0.0),
            Duration::from_millis(30_000)
        );
        assert_eq!(
            max_jitter_policy.delay_for(6, 1.0),
            Duration::from_millis(90_000)
        );

        // 默认 jitter = 0.2 时与 docs/03 §6.2 示例一致（48–72s）。
        let default_jitter = policy(0.2);
        assert_eq!(
            default_jitter.delay_for(6, 0.0),
            Duration::from_millis(48_000)
        );
        assert_eq!(
            default_jitter.delay_for(6, 1.0),
            Duration::from_millis(72_000)
        );
    }

    #[test]
    fn delay_has_100ms_floor() {
        let policy = RestartPolicy {
            initial_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(10),
            jitter: 0.0,
            ..RestartPolicy::default()
        };
        assert_eq!(
            policy.delay_for(1, 0.0),
            Duration::from_millis(MIN_DELAY_MS)
        );
        assert_eq!(
            policy.delay_for(1, 0.5),
            Duration::from_millis(MIN_DELAY_MS)
        );
    }

    #[test]
    fn multiplier_one_keeps_initial_delay() {
        let policy = RestartPolicy {
            multiplier: 1.0,
            jitter: 0.0,
            ..RestartPolicy::default()
        };
        assert_eq!(policy.delay_for(1, 0.0), Duration::from_millis(3000));
        assert_eq!(policy.delay_for(9, 0.0), Duration::from_millis(3000));
    }

    #[test]
    fn system_random_stays_in_unit_interval() {
        let mut random = SystemRandom::from_seed(0);
        for _ in 0..10_000 {
            let value = random.next_unit();
            assert!((0.0..1.0).contains(&value), "{value}");
        }
    }
}
