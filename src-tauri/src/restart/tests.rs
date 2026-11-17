//! M3-2 测试：docs/03 §13 的重启相关向量 + 退避/熔断边界。
//!
//! 所有时间都是显式 [`Instant`]，随机源固定为 0.5（抖动因子 = 1），
//! 因此延迟断言精确、不依赖 sleep 或真实时钟。

use std::time::{Duration, Instant};

use super::{
    Decision, Intent, RandomSource, Reason, RestartEvent, RestartMachine, RestartPolicy,
    RestartStatus,
};
use crate::config::{RestartConfig, RestartMode};

/// 固定随机数：0.5 → `1 + jitter * (2*0.5 - 1) = 1`，延迟等于基准值。
struct FixedRandom(f64);

impl RandomSource for FixedRandom {
    fn next_unit(&mut self) -> f64 {
        self.0
    }
}

type Machine = RestartMachine<FixedRandom>;

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn policy(mode: RestartMode) -> RestartPolicy {
    // 多数向量只关心退避/熔断，显式关闭启动窗口限速（C1 另有专门测试）。
    RestartPolicy::from_config(&RestartConfig {
        mode,
        window_ms: 0,
        max_starts_in_window: 0,
        ..RestartConfig::default()
    })
}

/// 打开启动窗口限速：`window_ms` 内最多 `max` 次启动。
fn windowed_policy(mode: RestartMode, window_ms: u64, max: u32) -> RestartPolicy {
    RestartPolicy::from_config(&RestartConfig {
        mode,
        window_ms,
        max_starts_in_window: max,
        ..RestartConfig::default()
    })
}

fn machine_with(policy: RestartPolicy) -> Machine {
    RestartMachine::with_random(policy, FixedRandom(0.5))
}

fn machine(mode: RestartMode) -> Machine {
    machine_with(policy(mode))
}

fn start_running(machine: &mut Machine, now: Instant) {
    assert_eq!(
        machine.handle(RestartEvent::StartRequested, now),
        Decision::StartNow {
            reason: Reason::StartRequested
        }
    );
    assert_eq!(
        machine.handle(RestartEvent::Started, now),
        Decision::Stay {
            reason: Reason::Spawned
        }
    );
    assert_eq!(machine.status(), RestartStatus::Running);
}

/// 在 `now` 处触发 tick 并启动，返回新的 `Started` 时刻。
fn retry(machine: &mut Machine, now: Instant) -> Instant {
    assert_eq!(
        machine.handle(RestartEvent::Tick, now),
        Decision::StartNow {
            reason: Reason::BackoffElapsed
        }
    );
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(
        machine.handle(RestartEvent::Started, now),
        Decision::Stay {
            reason: Reason::Spawned
        }
    );
    now
}

/// 制造一次异常退出 → backoff → 重试，返回重启后的 `Started` 时刻。
fn crash_and_restart(machine: &mut Machine, now: Instant) -> (Instant, Duration) {
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        now,
    );
    let Decision::Backoff { at, delay, .. } = decision else {
        panic!("异常退出应进入 backoff，实际：{decision:?}");
    };
    (retry(machine, at), delay)
}

// ------------------------------------------------------------ 基本迁移

#[test]
fn stopped_start_spawn_running() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    assert_eq!(machine.status(), RestartStatus::Stopped);
    assert_eq!(machine.intent(), Intent::Stopped);

    assert_eq!(
        machine.handle(RestartEvent::StartRequested, base),
        Decision::StartNow {
            reason: Reason::StartRequested
        }
    );
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert_eq!(machine.intent(), Intent::Running);
    assert_eq!(machine.consecutive_failures(), 0);

    let spawned_at = base + ms(50);
    assert_eq!(
        machine.handle(RestartEvent::Started, spawned_at),
        Decision::Stay {
            reason: Reason::Spawned
        }
    );
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.running_since(), Some(spawned_at));
}

#[test]
fn repeated_start_is_idempotent_but_clears_needs_human() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(RestartEvent::StartRequested, base + ms(10)),
        Decision::Stay {
            reason: Reason::StartRequested
        }
    );
    assert_eq!(machine.status(), RestartStatus::Running);

    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(20),
    );
    assert!(machine.needs_human().is_some());
    assert_eq!(
        machine.handle(RestartEvent::StartRequested, base + ms(30)),
        Decision::Stay {
            reason: Reason::StartRequested
        }
    );
    assert!(machine.needs_human().is_none());
    assert_eq!(machine.status(), RestartStatus::Running);
}

// ------------------------------------------------------------ 退避/熔断

#[test]
fn abnormal_exit_backs_off_then_restarts() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    let exit_at = base + ms(1000);
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        exit_at,
    );
    assert_eq!(
        decision,
        Decision::Backoff {
            at: exit_at + ms(3000),
            delay: ms(3000),
            reason: Reason::ProcessExit,
        }
    );
    assert_eq!(machine.status(), RestartStatus::Backoff);
    assert_eq!(machine.consecutive_failures(), 1);
    assert_eq!(machine.next_retry_at(), Some(exit_at + ms(3000)));
    assert_eq!(machine.remaining(exit_at), Some(ms(3000)));
    assert_eq!(machine.remaining(exit_at + ms(1000)), Some(ms(2000)));

    // 未到期：不启动。
    assert_eq!(
        machine.handle(RestartEvent::Tick, exit_at + ms(2999)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
    assert_eq!(machine.status(), RestartStatus::Backoff);

    // 到期：立即重试。
    assert_eq!(
        machine.handle(RestartEvent::Tick, exit_at + ms(3000)),
        Decision::StartNow {
            reason: Reason::BackoffElapsed
        }
    );
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(machine.remaining(exit_at + ms(3000)), None);
}

#[test]
fn five_consecutive_failures_pause_without_timer() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    let mut now = base + ms(1000);

    for failure in 1..=5 {
        let decision = machine.handle(
            RestartEvent::Exited {
                code: 1,
                clean: false,
            },
            now,
        );
        if failure < 5 {
            let Decision::Backoff { at, delay, .. } = decision else {
                panic!("第 {failure} 次失败应进入 backoff，实际：{decision:?}");
            };
            assert_eq!(delay, ms(3000 * (1 << (failure - 1))));
            assert_eq!(machine.consecutive_failures(), failure);
            now = retry(&mut machine, at);
        } else {
            assert_eq!(
                decision,
                Decision::Paused {
                    reason: Reason::CrashLoop
                }
            );
        }
    }

    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.intent(), Intent::Paused);
    assert_eq!(machine.consecutive_failures(), 5);
    assert_eq!(machine.paused_reason(), Some(Reason::CrashLoop));
    assert_eq!(machine.next_retry_at(), None);
    assert!(
        !machine
            .handle(RestartEvent::Tick, now + ms(3_600_000))
            .is_restart()
    );
    assert_eq!(
        machine.handle(RestartEvent::Tick, now + ms(3_600_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn paused_crash_loop_resume_clears_counters() {
    let base = Instant::now();
    let policy = RestartPolicy {
        max_consecutive_failures: 1,
        ..policy(RestartMode::OnFailure)
    };
    let mut machine = machine_with(policy);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 1,
                clean: false
            },
            base + ms(100)
        ),
        Decision::Paused {
            reason: Reason::CrashLoop
        }
    );

    let decision = machine.handle(RestartEvent::StartRequested, base + ms(200));
    assert_eq!(
        decision,
        Decision::StartNow {
            reason: Reason::StartRequested
        }
    );
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert_eq!(machine.intent(), Intent::Running);
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.paused_reason(), None);
}

#[test]
fn zero_max_failures_never_breaks_the_loop() {
    let base = Instant::now();
    let policy = RestartPolicy {
        max_consecutive_failures: 0,
        ..policy(RestartMode::OnFailure)
    };
    let mut machine = machine_with(policy);
    start_running(&mut machine, base);
    let mut now = base + ms(100);

    for _ in 0..20 {
        let (started, _) = crash_and_restart(&mut machine, now);
        now = started + ms(10);
    }

    assert_eq!(machine.consecutive_failures(), 20);
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.paused_reason(), None);

    // 第 21 次失败仍然只是 backoff。
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        now,
    );
    assert!(matches!(decision, Decision::Backoff { .. }));
    assert_eq!(machine.consecutive_failures(), 21);
}

// ------------------------------------------------------------ 用户意图

#[test]
fn user_stop_blocks_every_automatic_restart() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(RestartEvent::StopRequested, base + ms(100)),
        Decision::Stopped {
            reason: Reason::UserStop
        }
    );
    assert_eq!(machine.status(), RestartStatus::Stopped);
    assert_eq!(machine.intent(), Intent::Stopped);
    assert_eq!(machine.next_retry_at(), None);

    for (event, reason) in [
        (
            RestartEvent::Exited {
                code: 1,
                clean: false,
            },
            Reason::ProcessExit,
        ),
        (RestartEvent::HealthFailed, Reason::HealthFailure),
        (RestartEvent::Tick, Reason::Tick),
        (RestartEvent::StartFailed, Reason::StartupFailure),
    ] {
        assert_eq!(
            machine.handle(event, base + ms(10_000)),
            Decision::Stay { reason }
        );
        assert_eq!(machine.status(), RestartStatus::Stopped);
    }

    // 用户显式启动仍然有效。
    assert_eq!(
        machine.handle(RestartEvent::StartRequested, base + ms(20_000)),
        Decision::StartNow {
            reason: Reason::StartRequested
        }
    );
}

#[test]
fn user_stop_clears_needs_human_and_keeps_stopped() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(100),
    );

    assert_eq!(
        machine.handle(RestartEvent::StopRequested, base + ms(200)),
        Decision::Stopped {
            reason: Reason::UserStop
        }
    );
    assert!(machine.needs_human().is_none());
    assert_eq!(machine.intent(), Intent::Stopped);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn stop_during_backoff_cancels_timer() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        base + ms(100),
    );
    assert!(matches!(decision, Decision::Backoff { .. }));

    assert_eq!(
        machine.handle(RestartEvent::StopRequested, base + ms(200)),
        Decision::Stopped {
            reason: Reason::UserStop
        }
    );
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(machine.status(), RestartStatus::Stopped);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(3_600_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

// ------------------------------------------------------------ 模式语义

#[test]
fn on_failure_clean_exit_stops_without_restart() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 0,
                clean: true
            },
            base + ms(1000)
        ),
        Decision::Stopped {
            reason: Reason::CleanExit
        }
    );
    assert_eq!(machine.status(), RestartStatus::Stopped);
    assert_eq!(machine.intent(), Intent::Stopped);
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(machine.consecutive_failures(), 0);

    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn always_restarts_clean_exit_without_counting_failure() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::Always);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 0,
                clean: true
            },
            base + ms(1000)
        ),
        Decision::Backoff {
            at: base + ms(4000),
            delay: ms(3000),
            reason: Reason::CleanExit,
        }
    );
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.status(), RestartStatus::Backoff);
}

#[test]
fn always_clean_exit_does_not_trip_breaker() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::Always);
    start_running(&mut machine, base);
    let mut now = base + ms(1000);

    // 4 次异常退出（max = 5），仍在运行。
    for _ in 0..4 {
        let (started, _) = crash_and_restart(&mut machine, now);
        now = started + ms(10);
    }
    assert_eq!(machine.consecutive_failures(), 4);

    // 第 5 次是 clean exit：不熔断、不计失败，按 n=4 退避。
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 0,
            clean: true,
        },
        now,
    );
    assert_eq!(
        decision,
        Decision::Backoff {
            at: now + ms(24_000),
            delay: ms(24_000),
            reason: Reason::CleanExit,
        }
    );
    assert_eq!(machine.consecutive_failures(), 4);
    assert_eq!(machine.status(), RestartStatus::Backoff);
}

#[test]
fn never_pauses_on_any_exit() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::Never);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 0,
                clean: true
            },
            base + ms(100)
        ),
        Decision::Paused {
            reason: Reason::CrashNoRestart
        }
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.intent(), Intent::Paused);
    assert_eq!(machine.paused_reason(), Some(Reason::CrashNoRestart));
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.next_retry_at(), None);
}

#[test]
fn start_failure_backs_off_and_counts() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    machine.handle(RestartEvent::StartRequested, base);
    assert_eq!(
        machine.handle(RestartEvent::StartFailed, base + ms(500)),
        Decision::Backoff {
            at: base + ms(3500),
            delay: ms(3000),
            reason: Reason::StartupFailure,
        }
    );
    assert_eq!(machine.consecutive_failures(), 1);
    assert_eq!(machine.status(), RestartStatus::Backoff);
}

// ------------------------------------------------------------ 稳定运行

#[test]
fn stable_after_clears_failures_only_after_elapsed() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    let (started_at, _) = crash_and_restart(&mut machine, base + ms(1000));
    assert_eq!(machine.consecutive_failures(), 1);

    assert_eq!(
        machine.handle(RestartEvent::StableReached, started_at + ms(59_999)),
        Decision::Stay {
            reason: Reason::Stable
        }
    );
    assert_eq!(machine.consecutive_failures(), 1);
    assert_eq!(
        machine.handle(RestartEvent::Tick, started_at + ms(59_999)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );

    assert_eq!(
        machine.handle(RestartEvent::StableReached, started_at + ms(60_000)),
        Decision::Stable {
            reason: Reason::Stable
        }
    );
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.status(), RestartStatus::Running);
}

#[test]
fn tick_also_clears_failures_after_stable_window() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    let (started_at, _) = crash_and_restart(&mut machine, base + ms(1000));

    assert_eq!(
        machine.handle(RestartEvent::Tick, started_at + ms(60_000)),
        Decision::Stable {
            reason: Reason::Stable
        }
    );
    assert_eq!(machine.consecutive_failures(), 0);
}

#[test]
fn stable_clears_failures_even_with_needs_human() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    let (started_at, _) = crash_and_restart(&mut machine, base + ms(1000));
    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        started_at + ms(100),
    );
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.consecutive_failures(), 1);

    assert_eq!(
        machine.handle(RestartEvent::StableReached, started_at + ms(60_000)),
        Decision::Stable {
            reason: Reason::Stable
        }
    );
    assert_eq!(machine.consecutive_failures(), 0);
    assert!(machine.needs_human().is_some());
}

// ------------------------------------------------------------ needs_human

#[test]
fn needs_human_keeps_running_process_and_suppresses_restart() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(
            RestartEvent::NeedsHumanObserved {
                source: "heartbeat".to_owned(),
                message: "login_required".to_owned(),
            },
            base + ms(100)
        ),
        Decision::Stay {
            reason: Reason::NeedsHuman
        }
    );
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.intent(), Intent::Running);
    assert_eq!(machine.next_retry_at(), None);
    let needs_human = machine.needs_human().expect("应置位 needs_human");
    assert_eq!(needs_human.source, "heartbeat");
    assert_eq!(needs_human.message, "login_required");
    assert_eq!(needs_human.since, base + ms(100));

    // 进程退出：paused(needs_human)，不计失败、不排定时器。
    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 1,
                clean: false
            },
            base + ms(200)
        ),
        Decision::Paused {
            reason: Reason::NeedsHuman
        }
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.paused_reason(), Some(Reason::NeedsHuman));
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn heartbeat_recovery_clears_flag_without_auto_start() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(100),
    );
    assert_eq!(
        machine.handle(RestartEvent::NeedsHumanCleared, base + ms(200)),
        Decision::Stay {
            reason: Reason::NeedsHumanCleared
        }
    );
    assert!(machine.needs_human().is_none());
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.intent(), Intent::Running);

    // 若进程已退出（paused needs_human），恢复心跳只回到 stopped。
    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(300),
    );
    machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        base + ms(400),
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(
        machine.handle(RestartEvent::NeedsHumanCleared, base + ms(500)),
        Decision::Stopped {
            reason: Reason::NeedsHumanCleared
        }
    );
    assert_eq!(machine.status(), RestartStatus::Stopped);
    assert_eq!(machine.intent(), Intent::Stopped);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn needs_human_during_backoff_cancels_timer() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    assert!(matches!(
        machine.handle(
            RestartEvent::Exited {
                code: 1,
                clean: false
            },
            base + ms(100)
        ),
        Decision::Backoff { .. }
    ));

    assert_eq!(
        machine.handle(
            RestartEvent::NeedsHumanObserved {
                source: "heartbeat".to_owned(),
                message: "login_required".to_owned(),
            },
            base + ms(200)
        ),
        Decision::Paused {
            reason: Reason::NeedsHuman
        }
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(machine.consecutive_failures(), 1);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn health_failure_with_needs_human_does_not_count() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(100),
    );

    assert_eq!(
        machine.handle(RestartEvent::HealthFailed, base + ms(200)),
        Decision::Paused {
            reason: Reason::NeedsHuman
        }
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.next_retry_at(), None);
}

#[test]
fn pause_on_exit_code_sets_needs_human_without_failure() {
    let base = Instant::now();
    let policy = RestartPolicy {
        pause_on_exit_codes: vec![2],
        ..policy(RestartMode::OnFailure)
    };
    let mut machine = machine_with(policy);
    start_running(&mut machine, base);

    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 2,
                clean: false
            },
            base + ms(100)
        ),
        Decision::Paused {
            reason: Reason::NeedsHuman
        }
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.paused_reason(), Some(Reason::NeedsHuman));
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.next_retry_at(), None);
    let needs_human = machine.needs_human().expect("应置位 needs_human");
    assert_eq!(needs_human.source, "exit_code");
    assert!(needs_human.message.contains('2'));

    assert_eq!(
        machine.handle(RestartEvent::NeedsHumanCleared, base + ms(200)),
        Decision::Stopped {
            reason: Reason::NeedsHumanCleared
        }
    );
    assert_eq!(machine.status(), RestartStatus::Stopped);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn pause_on_exit_code_after_user_stop_is_ignored() {
    let base = Instant::now();
    let policy = RestartPolicy {
        pause_on_exit_codes: vec![2],
        ..policy(RestartMode::OnFailure)
    };
    let mut machine = machine_with(policy);
    start_running(&mut machine, base);
    machine.handle(RestartEvent::StopRequested, base + ms(100));

    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 2,
                clean: false
            },
            base + ms(200)
        ),
        Decision::Stay {
            reason: Reason::ProcessExit
        }
    );
    assert!(machine.needs_human().is_none());
    assert_eq!(machine.status(), RestartStatus::Stopped);
}

#[test]
fn acknowledge_with_exited_process_starts_and_resets() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(100),
    );
    machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        base + ms(200),
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.consecutive_failures(), 0);

    assert_eq!(
        machine.handle(RestartEvent::AcknowledgeNeedsHuman, base + ms(300)),
        Decision::StartNow {
            reason: Reason::NeedsHumanCleared
        }
    );
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert_eq!(machine.intent(), Intent::Running);
    assert_eq!(machine.consecutive_failures(), 0);
    assert_eq!(machine.paused_reason(), None);
    assert!(machine.needs_human().is_none());
}

#[test]
fn acknowledge_with_live_process_only_clears_flag() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + ms(100),
    );

    assert_eq!(
        machine.handle(RestartEvent::AcknowledgeNeedsHuman, base + ms(200)),
        Decision::Stay {
            reason: Reason::NeedsHumanCleared
        }
    );
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.intent(), Intent::Running);
    assert!(machine.needs_human().is_none());
}

// ------------------------------------------------------------ 健康失败/迟到事件

#[test]
fn health_failures_share_the_crash_counter() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    let decision = machine.handle(RestartEvent::HealthFailed, base + ms(100));
    assert_eq!(
        decision,
        Decision::Backoff {
            at: base + ms(3100),
            delay: ms(3000),
            reason: Reason::HealthFailure,
        }
    );
    assert_eq!(machine.consecutive_failures(), 1);

    let started_at = retry(&mut machine, base + ms(3100));
    assert_eq!(
        machine.handle(RestartEvent::HealthFailed, started_at + ms(100)),
        Decision::Backoff {
            at: started_at + ms(6100),
            delay: ms(6000),
            reason: Reason::HealthFailure,
        }
    );
    assert_eq!(machine.consecutive_failures(), 2);
}

#[test]
fn late_exit_and_started_events_are_ignored() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    assert!(matches!(
        machine.handle(
            RestartEvent::Exited {
                code: 1,
                clean: false
            },
            base + ms(100)
        ),
        Decision::Backoff { .. }
    ));
    // 重复退出：不重复计数。
    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 1,
                clean: false
            },
            base + ms(200)
        ),
        Decision::Stay {
            reason: Reason::ProcessExit
        }
    );
    assert_eq!(machine.consecutive_failures(), 1);

    // 未到 backoff 到期就收到 Started：忽略，不进入 running。
    assert_eq!(
        machine.handle(RestartEvent::Started, base + ms(300)),
        Decision::Stay {
            reason: Reason::Spawned
        }
    );
    assert_eq!(machine.status(), RestartStatus::Backoff);

    // 健康失败在 backoff 中同样忽略。
    assert_eq!(
        machine.handle(RestartEvent::HealthFailed, base + ms(400)),
        Decision::Stay {
            reason: Reason::HealthFailure
        }
    );
    assert_eq!(machine.consecutive_failures(), 1);
}

#[test]
fn snapshot_exposes_timer_and_reason() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);
    machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        base + ms(100),
    );

    let snapshot = machine.snapshot(base + ms(1000));
    assert_eq!(snapshot.status, RestartStatus::Backoff);
    assert_eq!(snapshot.intent, Intent::Running);
    assert_eq!(snapshot.consecutive_failures, 1);
    assert_eq!(snapshot.next_retry_at, Some(base + ms(3100)));
    assert_eq!(snapshot.remaining, Some(ms(2100)));
    assert_eq!(snapshot.paused_reason, None);
    assert!(snapshot.needs_human.is_none());
    assert_eq!(snapshot.last_reason, Some(Reason::ProcessExit));
}

#[test]
fn decision_is_restart_classification() {
    let base = Instant::now();
    assert!(
        Decision::StartNow {
            reason: Reason::StartRequested
        }
        .is_restart()
    );
    assert!(
        Decision::Backoff {
            at: base,
            delay: ms(100),
            reason: Reason::ProcessExit,
        }
        .is_restart()
    );
    assert!(
        !Decision::Stopped {
            reason: Reason::CleanExit
        }
        .is_restart()
    );
    assert!(
        !Decision::Paused {
            reason: Reason::CrashLoop
        }
        .is_restart()
    );
    assert!(
        !Decision::Stable {
            reason: Reason::Stable
        }
        .is_restart()
    );
}
/// Q3：启动宽限到期时进程仍是 Running，Engine 先 stop 再上报 StartFailed，
/// 状态机也必须按启动失败计数并进入 backoff/熔断。
#[test]
fn startup_failure_while_running_counts_once() {
    let base = Instant::now();
    let mut machine = machine(RestartMode::OnFailure);
    start_running(&mut machine, base);

    let decision = machine.handle(RestartEvent::StartFailed, base);
    let Decision::Backoff { delay, reason, .. } = decision else {
        panic!("启动失败应进入 backoff：{decision:?}");
    };
    assert_eq!(reason, Reason::StartupFailure);
    assert_eq!(machine.consecutive_failures(), 1);
    assert!(delay >= ms(3000));

    // 重复上报（事件 + 调度器兜底）不得重复计数。
    assert_eq!(
        machine.handle(RestartEvent::StartFailed, base),
        Decision::Stay {
            reason: Reason::StartupFailure
        }
    );
    assert_eq!(machine.consecutive_failures(), 1);
}

/// Q2：达到 max_consecutive_failures 后直接 paused(crash_loop)，不留退避定时器。
#[test]
fn circuit_breaker_pauses_without_backoff_timer() {
    let mut machine = machine_with(RestartPolicy {
        max_consecutive_failures: 2,
        ..policy(RestartMode::OnFailure)
    });
    let base = Instant::now();
    start_running(&mut machine, base);
    let first = machine.handle(
        RestartEvent::Exited {
            code: 7,
            clean: false,
        },
        base,
    );
    assert!(matches!(first, Decision::Backoff { .. }));
    let at = match first {
        Decision::Backoff { at, .. } => at,
        _ => unreachable!("上方已断言"),
    };

    let restarted_at = at;
    assert!(matches!(
        machine.handle(RestartEvent::Tick, restarted_at),
        Decision::StartNow { .. }
    ));
    assert_eq!(
        machine.handle(RestartEvent::Started, restarted_at),
        Decision::Stay {
            reason: Reason::Spawned
        }
    );
    let second = machine.handle(
        RestartEvent::Exited {
            code: 7,
            clean: false,
        },
        restarted_at + ms(100),
    );
    assert_eq!(
        second,
        Decision::Paused {
            reason: Reason::CrashLoop
        }
    );
    assert_eq!(machine.next_retry_at(), None);
    assert_eq!(machine.consecutive_failures(), 2);
    assert_eq!(
        machine.handle(RestartEvent::Tick, base + ms(10_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

/// M3-6b：休眠唤醒后平移计时器，退避剩余时间不变（睡眠不算退避到期）。
#[test]
fn shift_timers_preserves_backoff_remaining_across_sleep() {
    let mut machine = machine(RestartMode::Always);
    let base = Instant::now();
    start_running(&mut machine, base);

    let exit_at = base + ms(1000);
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        exit_at,
    );
    assert!(matches!(decision, Decision::Backoff { .. }));
    assert_eq!(machine.next_retry_at(), Some(exit_at + ms(3000)));

    // 睡眠 8 小时后唤醒：单调计时器整体平移，剩余时间仍是 3s。
    let slept = Duration::from_secs(8 * 3600);
    machine.shift_timers(slept);
    let resumed_at = exit_at + slept;
    assert_eq!(machine.next_retry_at(), Some(resumed_at + ms(3000)));
    assert_eq!(machine.remaining(resumed_at), Some(ms(3000)));
    assert_eq!(machine.consecutive_failures(), 1);

    // 平移后仍未到期：不启动；到平移后的时刻才重试。
    assert_eq!(
        machine.handle(RestartEvent::Tick, resumed_at + ms(2999)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
    assert_eq!(
        machine.handle(RestartEvent::Tick, resumed_at + ms(3000)),
        Decision::StartNow {
            reason: Reason::BackoffElapsed
        }
    );
}

/// M3-6b：stable_after 窗口同样平移，睡眠不能把失败计数提前清零。
#[test]
fn shift_timers_preserves_stable_window() {
    let mut machine = machine(RestartMode::Always);
    let base = Instant::now();

    let _ = machine.handle(RestartEvent::StartRequested, base);
    let _ = machine.handle(RestartEvent::Started, base + ms(100));
    assert_eq!(machine.running_since(), Some(base + ms(100)));

    let slept = Duration::from_secs(3600);
    machine.shift_timers(slept);
    assert_eq!(machine.running_since(), Some(base + ms(100) + slept));

    // 零时长平移是空操作。
    machine.shift_timers(Duration::ZERO);
    assert_eq!(machine.running_since(), Some(base + ms(100) + slept));
}

// ------------------------------------------------------------ 启动窗口限速（C1）

/// 1s 固定退避 + 不限熔断，便于精确驱动启动窗口。
fn budget_policy(window_ms: u64, max_starts: u32) -> RestartPolicy {
    RestartPolicy {
        initial_delay: ms(1000),
        max_delay: ms(1000),
        multiplier: 1.0,
        jitter: 0.0,
        max_consecutive_failures: 0,
        ..windowed_policy(RestartMode::OnFailure, window_ms, max_starts)
    }
}

/// 一次异常退出 → 返回 backoff 到期时刻。
fn crash_backoff(machine: &mut Machine, now: Instant) -> Instant {
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 1,
            clean: false,
        },
        now,
    );
    let Decision::Backoff { at, .. } = decision else {
        panic!("异常退出应进入 backoff，实际：{decision:?}");
    };
    at
}

#[test]
fn start_window_pauses_when_budget_exhausted() {
    let base = Instant::now();
    let mut machine = machine_with(budget_policy(10_000, 3));
    start_running(&mut machine, base);
    assert_eq!(machine.recent_start_count(), 1);

    let mut now = base + ms(1000);
    for starts in 2..=3u32 {
        let at = crash_backoff(&mut machine, now);
        now = retry(&mut machine, at);
        assert_eq!(machine.recent_start_count(), starts as usize);
    }

    // 窗口内已有 3 次启动：第 4 次不再启动，直接 paused(restart_budget)。
    let at = crash_backoff(&mut machine, now);
    assert_eq!(
        machine.handle(RestartEvent::Tick, at),
        Decision::Paused {
            reason: Reason::RestartBudget
        }
    );
    assert_eq!(machine.status(), RestartStatus::Paused);
    assert_eq!(machine.intent(), Intent::Paused);
    assert_eq!(machine.paused_reason(), Some(Reason::RestartBudget));
    assert_eq!(machine.next_retry_at(), None);
    // 窗口历史不参与熔断计数：暂停时计数仍是崩溃次数，且可持久化。
    assert_eq!(machine.consecutive_failures(), 3);
    assert_eq!(machine.seed().consecutive_failures, 3);

    assert_eq!(
        machine.handle(RestartEvent::Tick, at + ms(60_000)),
        Decision::Stay {
            reason: Reason::Tick
        }
    );
}

#[test]
fn start_window_slides_and_allows_start_after_expiry() {
    let base = Instant::now();
    let mut machine = machine_with(budget_policy(5000, 3));
    start_running(&mut machine, base);

    // 3 次启动：base、base+2000、base+3000。
    let at = crash_backoff(&mut machine, base + ms(1000));
    retry(&mut machine, at);
    let at = crash_backoff(&mut machine, base + ms(2000));
    retry(&mut machine, at);
    assert_eq!(machine.recent_start_count(), 3);

    // base+5000 时 base 那次已滑出窗口（窗口为半开区间 (now-window, now]），
    // 窗口内只剩 2 次，允许第 4 次启动。
    let at = crash_backoff(&mut machine, base + ms(4000));
    assert_eq!(at, base + ms(5000));
    assert_eq!(
        machine.handle(RestartEvent::Tick, at),
        Decision::StartNow {
            reason: Reason::BackoffElapsed
        }
    );
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert_eq!(
        machine.handle(RestartEvent::Started, at),
        Decision::Stay {
            reason: Reason::Spawned
        }
    );
    assert_eq!(machine.recent_start_count(), 3);
}

#[test]
fn zero_max_starts_in_window_never_throttles() {
    let base = Instant::now();
    let mut machine = machine_with(budget_policy(600_000, 0));
    start_running(&mut machine, base);

    let mut now = base + ms(100);
    for _ in 0..20 {
        let (started, _) = crash_and_restart(&mut machine, now);
        now = started + ms(10);
    }

    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.paused_reason(), None);
    // 不限速时不记录窗口历史，避免无意义增长。
    assert_eq!(machine.recent_start_count(), 0);
}

#[test]
fn user_start_clears_start_window() {
    let base = Instant::now();
    let mut machine = machine_with(budget_policy(10_000, 2));
    start_running(&mut machine, base);
    let at = crash_backoff(&mut machine, base + ms(1000));
    let started = retry(&mut machine, at);
    assert_eq!(machine.recent_start_count(), 2);

    // 再崩一次进入 backoff，然后用户显式 Start：清空窗口历史并立即启动。
    crash_backoff(&mut machine, started + ms(100));
    assert_eq!(
        machine.handle(RestartEvent::StartRequested, started + ms(200)),
        Decision::StartNow {
            reason: Reason::StartRequested
        }
    );
    assert_eq!(machine.recent_start_count(), 0);
    machine.handle(RestartEvent::Started, started + ms(200));
    assert_eq!(machine.recent_start_count(), 1);

    // 若历史未清零，此时窗口内已有 3 次启动（≥ 2），本次会被暂停。
    let at = crash_backoff(&mut machine, started + ms(1200));
    assert_eq!(
        machine.handle(RestartEvent::Tick, at),
        Decision::StartNow {
            reason: Reason::BackoffElapsed
        }
    );
}

#[test]
fn user_stop_and_schedule_stop_clear_start_window() {
    let base = Instant::now();
    let mut machine = machine_with(budget_policy(10_000, 3));
    start_running(&mut machine, base);
    let at = crash_backoff(&mut machine, base + ms(1000));
    retry(&mut machine, at);
    assert_eq!(machine.recent_start_count(), 2);

    assert_eq!(
        machine.handle(RestartEvent::StopRequested, base + ms(2100)),
        Decision::Stopped {
            reason: Reason::UserStop
        }
    );
    assert_eq!(machine.recent_start_count(), 0);

    start_running(&mut machine, base + ms(2200));
    assert_eq!(machine.recent_start_count(), 1);
    assert_eq!(
        machine.handle(RestartEvent::ScheduleStopRequested, base + ms(2300)),
        Decision::Stopped {
            reason: Reason::ScheduleStop
        }
    );
    assert_eq!(machine.recent_start_count(), 0);
}
