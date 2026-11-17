//! 运行时段 → 启停决策（2026-11-05 用户架构修正）。
//!
//! **一个时钟、一个开关**：\`[tasks.schedule]\` 是唯一时钟，\`schedule.enabled=true\`
//! 时应用是唯一权威。本模块是纯逻辑，回答每轮 tick 对单个任务做**什么动作**：
//!
//! | 观测 | 动作 |
//! | --- | --- |
//! | 进入窗口（out→in / window_key 变化 / 启动后首次观测在窗口内） | 可以启动一次；本窗口被用户 Stop 抑制、或监督状态不允许时不启动 |
//! | 离开窗口（in→out，含休眠跨窗唤醒后的首次观测、并集结束） | 停止任何存活实例（Starting/Running/Stopping 都算运行），不区分谁启动、不因 needs_human 跳过 |
//! | 窗口外稳态（上次观测已在窗口外） | 不动作；手动启动的实例继续运行，直到下一个窗口结束时被停 |
//! | 本进程首次观测已在窗口外 | 不动作；不因时段停止任何已恢复/已手动启动的实例 |
//!
//! 配置里的 \`start_in_window\` / \`stop_outside\` 只保留兼容，引擎完全忽略；
//! \`ScheduleRuntime.schedule_started\` 的 gating 已删除。离开窗口时把持久化的
//! \`state.schedule_window\` 置 None，避免陈旧状态影响下一次启动。
//!
//! 本模块不读配置、不碰进程、不依赖 Tauri，可直接在 \`cargo test\` 中覆盖全部
//! 语义（Engine 只负责把权限/状态算成输入，并执行输出的动作）。

use chrono::NaiveDateTime;

use super::{Schedule, Window};

/// 引擎内存中的每任务时段观测状态（跨 tick 保留；不落盘）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WindowRuntime {
    /// 上一次 tick 是否在时段内；\`None\` = 本进程尚未观测。
    pub in_window: Option<bool>,
    /// 上一次观测到的并集窗口 key。
    pub window_key: Option<String>,
}

/// 本轮对持久化 \`state.schedule_window\` 的处理。
///
/// 该字段只用于诊断/跨重启对照，不参与监督决策；陈旧值不能导致误停。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PersistedWindowChange {
    /// 保持不变（窗口外且没有陈旧值）。
    Keep,
    /// 写入当前窗口 key。
    Set(String),
    /// 离开窗口或启动后首次观测在窗口外：清空陈旧值。
    Clear,
}

/// 本轮时段动作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WindowAction {
    /// 不动作。
    None,
    /// 进入新窗口且允许自动启动：调用方按 \`schedule_start\` 启动一次。
    Start,
    /// 离开窗口且实例存活：调用方按 \`schedule_stop\` 停止实例。
    Stop,
}

/// 一轮时段决策的输入。
///
/// 权限类判断（enabled / paused / needs_human / 监督状态）由 Engine 预先算好，
/// 本模块只负责时间与观测语义，保证可以脱离 Engine 测试。
#[derive(Debug, Clone)]
pub struct WindowTickInput<'a> {
    /// 任务的并集窗口列表。
    pub windows: &'a [Window],
    /// 当前本地时间。
    pub now: NaiveDateTime,
    /// 上一轮观测状态；\`None\` = 本进程第一次观测。
    pub previous: Option<&'a WindowRuntime>,
    /// 持久化的 \`state.schedule_window\`（可能来自上次进程，属陈旧数据）。
    pub persisted_window: Option<&'a str>,
    /// 持久化的 \`state.schedule_suppressed\`：用户在本窗口内 Stop 时记下的 key。
    pub suppressed_key: Option<&'a str>,
    /// 用户显式停止意图（\`desired_override=stopped\`）。
    pub user_stopped: bool,
    /// 进入新窗口后自动启动是否被允许（调度器侧：enabled、非 paused、
    /// 非 needs_human；本窗口用户 Stop 抑制由本函数判断）。
    pub auto_start_permitted: bool,
    /// 监督状态机是否已停止（backoff/starting/running/paused 时不重复启动）。
    pub machine_stopped: bool,
    /// 运行时是否有存活实例（Starting/Running/Stopping 都算运行）。
    pub instance_running: bool,
}

/// 一轮时段决策的输出；Engine 按序执行并回写内存/持久状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowTick {
    pub action: WindowAction,
    /// 本轮观测到的并集窗口 key；窗口外为 \`None\`。
    pub window_key: Option<String>,
    /// 回写到 \`schedule_states\` 的新观测状态。
    pub runtime: WindowRuntime,
    /// 本 tick 是否进入了一个新窗口（out→in / key 变化 / 首次观测在窗口内）。
    pub entered_window: bool,
    /// 是否为本进程第一次观测（用于“启动时窗口外不误停”）。
    pub first_observation: bool,
    /// 持久化 \`state.schedule_window\` 的处理方式。
    pub persisted: PersistedWindowChange,
    /// 进入新窗口：清掉上一次窗口残留的用户 Stop 抑制。
    pub clear_stale_suppression: bool,
    /// 进入新窗口且未被本窗口抑制：清掉上一次残留的 Stop override。
    pub clear_user_stop: bool,
}

/// 计算一轮时段决策（纯函数）。
pub fn plan_window_tick(input: WindowTickInput<'_>) -> WindowTick {
    let schedule = Schedule {
        windows: input.windows.to_vec(),
    };
    let active = schedule.active(input.now);
    let key = active.map(|window| window.start.format("%Y-%m-%dT%H:%M").to_string());
    let was_in = input.previous.and_then(|state| state.in_window);
    let previous_key = input.previous.and_then(|state| state.window_key.as_deref());
    let first_observation = input.previous.is_none() || was_in.is_none();

    match (active, key) {
        (Some(_), Some(key)) => {
            // 进入窗口：首次观测在窗口内、out→in，或并集 key 变化。
            let entered = was_in != Some(true) || previous_key != Some(key.as_str());
            let suppressed = input.suppressed_key == Some(key.as_str());
            let clear_stale_suppression = entered && !suppressed && input.suppressed_key.is_some();
            let clear_user_stop = entered && !suppressed && input.user_stopped;
            let start = entered
                && !suppressed
                && input.auto_start_permitted
                && input.machine_stopped
                && !input.instance_running;
            WindowTick {
                action: if start {
                    WindowAction::Start
                } else {
                    WindowAction::None
                },
                persisted: PersistedWindowChange::Set(key.clone()),
                window_key: Some(key.clone()),
                runtime: WindowRuntime {
                    in_window: Some(true),
                    window_key: Some(key),
                },
                entered_window: entered,
                first_observation,
                clear_stale_suppression,
                clear_user_stop,
            }
        }
        _ => {
            // 离开窗口：只有“上一轮在窗口内、本轮不在”才算离开；
            // 本进程首次观测已在窗口外时不动作（不误停已恢复/手动启动的实例）。
            let left = !first_observation && was_in == Some(true);
            WindowTick {
                action: if left && input.instance_running {
                    WindowAction::Stop
                } else {
                    WindowAction::None
                },
                persisted: if input.persisted_window.is_some() {
                    PersistedWindowChange::Clear
                } else {
                    PersistedWindowChange::Keep
                },
                window_key: None,
                runtime: WindowRuntime {
                    in_window: Some(false),
                    window_key: None,
                },
                entered_window: false,
                first_observation,
                clear_stale_suppression: false,
                clear_user_stop: false,
            }
        }
    }
}

/// 监督器发起的自动启动是否允许（2026-11-05 “应用窗口是唯一时钟”）。
///
/// 引擎在 `scheduler_work` 第 2/3 步（auto_start_at 到期、pending_start/退避
/// 重试/needs_human 恢复）和所有自动启动前调用：
///
/// - 非 schedule 任务：允许（原有 auto_start/退避语义不变）；
/// - `schedule.enabled=true`：只有当前在运行时段内，或用户显式手动启动过该任务
///   （窗口外手动启动的实例仍可被监督器重启）才允许；
/// - 窗口外且无用户启动意图：必须取消，避免应用在窗口外自动开工。
///
/// `user_started` 是进程内标志（state.json 的 `desired_override` 只有
/// stopped/paused，没有 started）：用户手动 Start/Restart 置位、手动 Stop 清除，
/// 应用重启后自然清空——那时不存在存活实例，窗口外也不会自动开工。
pub fn may_supervisor_autostart(
    schedule_enabled: bool,
    in_window: bool,
    user_started: bool,
) -> bool {
    !schedule_enabled || in_window || user_started
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Weekday, parse_time};
    use Weekday::{Fri, Mon, Thu, Tue, Wed};
    use chrono::NaiveDate;

    fn dt(date: &str, hour: u32, minute: u32) -> NaiveDateTime {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").expect("日期");
        date.and_time(chrono::NaiveTime::from_hms_opt(hour, minute, 0).expect("时间"))
    }

    fn window(days: &[Weekday], start: &str, end: &str) -> Window {
        Window {
            days: days.to_vec(),
            start: parse_time(start).expect("start"),
            end: parse_time(end).expect("end"),
        }
    }

    fn runtime(in_window: bool, key: &str) -> WindowRuntime {
        WindowRuntime {
            in_window: Some(in_window),
            window_key: Some(key.to_owned()),
        }
    }

    /// 构造“上一轮已在窗口内”的观测。
    fn inside(key: &str) -> WindowRuntime {
        runtime(true, key)
    }

    fn outside() -> WindowRuntime {
        WindowRuntime {
            in_window: Some(false),
            window_key: None,
        }
    }

    struct Case<'a> {
        windows: &'a [Window],
        now: NaiveDateTime,
        previous: Option<&'a WindowRuntime>,
        persisted: Option<&'a str>,
        suppressed: Option<&'a str>,
        user_stopped: bool,
        permitted: bool,
        machine_stopped: bool,
        running: bool,
    }

    impl Default for Case<'_> {
        fn default() -> Self {
            Self {
                windows: &[],
                now: dt("2026-11-04", 8, 0),
                previous: None,
                persisted: None,
                suppressed: None,
                user_stopped: false,
                permitted: true,
                machine_stopped: true,
                running: false,
            }
        }
    }

    fn run(case: Case<'_>) -> WindowTick {
        plan_window_tick(WindowTickInput {
            windows: case.windows,
            now: case.now,
            previous: case.previous,
            persisted_window: case.persisted,
            suppressed_key: case.suppressed,
            user_stopped: case.user_stopped,
            auto_start_permitted: case.permitted,
            machine_stopped: case.machine_stopped,
            instance_running: case.running,
        })
    }

    /// 2026-11-04 是周三。
    fn weekday_window() -> Vec<Window> {
        vec![window(&[Mon, Tue, Wed, Thu], "07:50", "08:20")]
    }

    #[test]
    fn entering_window_starts_once() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 7, 50),
            previous: Some(&outside()),
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Start);
        assert!(tick.entered_window);
        assert!(!tick.first_observation);
        assert_eq!(tick.window_key.as_deref(), Some("2026-11-04T07:50"));
        assert_eq!(
            tick.persisted,
            PersistedWindowChange::Set("2026-11-04T07:50".to_owned())
        );

        // 同一窗口的下一个 tick：不重复启动（即使实例已退出）。
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 7, 58),
            previous: Some(&inside("2026-11-04T07:50")),
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        assert!(!tick.entered_window);
        assert!(!tick.first_observation);
    }

    #[test]
    fn first_observation_inside_window_starts() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 8, 0),
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Start);
        assert!(tick.first_observation);
        assert!(tick.entered_window);
    }

    #[test]
    fn user_stop_suppresses_restart_in_same_window() {
        let windows = weekday_window();
        let key = "2026-11-04T07:50";
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 8, 5),
            previous: Some(&inside(key)),
            suppressed: Some(key),
            user_stopped: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        assert!(!tick.entered_window);
        assert!(!tick.clear_stale_suppression);
        assert!(!tick.clear_user_stop);
        assert_eq!(tick.runtime, inside(key));
    }

    /// 同一窗口内让用户 Stop 后重新进入（key 不变）也不重启。
    #[test]
    fn user_stop_same_window_never_restarts_even_on_reentry_tick() {
        let windows = weekday_window();
        let key = "2026-11-04T07:50";
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 8, 10),
            previous: Some(&outside()),
            suppressed: Some(key),
            user_stopped: true,
            ..Case::default()
        });
        // key 不同？外部传入的是 outside 且抑制 key 相同：本次属于“进入窗口”，
        // 但本窗口被抑制 → 不启动、也不清除用户 Stop 意图。
        assert_eq!(tick.action, WindowAction::None);
        assert!(tick.entered_window);
        assert!(!tick.clear_stale_suppression);
        assert!(!tick.clear_user_stop);
    }

    /// 下个窗口（新 key）：清除上一次窗口的抑制与 Stop override，并自动启动。
    #[test]
    fn next_window_clears_old_stop_and_starts() {
        let windows = weekday_window();
        let old_key = "2026-11-04T07:50";
        let next_day = dt("2026-11-05", 7, 50);
        let next_key = "2026-11-05T07:50";
        let tick = run(Case {
            windows: &windows,
            now: next_day,
            previous: Some(&inside(old_key)),
            suppressed: Some(old_key),
            user_stopped: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Start);
        assert!(tick.entered_window);
        assert!(tick.clear_stale_suppression);
        assert!(tick.clear_user_stop);
        assert_eq!(tick.window_key.as_deref(), Some(next_key));
    }

    /// 窗口外 Stop（没有 schedule_suppressed，key=None）：下个窗口照常启动。
    #[test]
    fn stop_outside_window_does_not_block_next_window() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-05", 7, 50),
            previous: Some(&outside()),
            suppressed: None,
            user_stopped: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Start);
        assert!(tick.clear_user_stop);
        assert!(!tick.clear_stale_suppression);
    }

    /// 手动启动（不论谁启动）在窗口结束被停。
    #[test]
    fn window_end_stops_any_running_instance() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 8, 20),
            previous: Some(&inside("2026-11-04T07:50")),
            user_stopped: true, // 手动启动会清掉 Stop，这里刻意置位：停止不受影响
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Stop);
        assert!(!tick.entered_window);
        assert_eq!(tick.persisted, PersistedWindowChange::Keep);
        assert_eq!(tick.runtime, outside());
    }

    /// needs_human（或任何暂停）不允许自动启动，但不阻止窗口结束的停止。
    #[test]
    fn window_end_stops_needs_human_live_instance() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 8, 20),
            previous: Some(&inside("2026-11-04T07:50")),
            permitted: false, // needs_human / paused
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Stop);
    }

    /// 窗口外稳态：手动启动的实例一直跑，不因时段被停。
    #[test]
    fn manual_instance_outside_window_is_not_stopped() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 9, 0),
            previous: Some(&outside()),
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
    }

    /// 本进程首次观测已在窗口外：不停止任何已恢复/已手动启动的实例。
    #[test]
    fn first_observation_outside_never_stops_restored_instance() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 9, 0),
            previous: None,
            persisted: Some("2026-11-03T07:50"),
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        assert!(tick.first_observation);
        assert_eq!(tick.persisted, PersistedWindowChange::Clear, "清掉陈旧 key");
        assert_eq!(tick.runtime, outside());
    }

    /// 跨午夜：窗口内的最后一刻不动作，结束边界才停（02:00 end 不含）。
    #[test]
    fn cross_midnight_window_end_stops_at_exact_boundary() {
        let windows = vec![window(&[Fri], "22:00", "02:00")];
        let before = run(Case {
            windows: &windows,
            now: dt("2026-11-07", 1, 59),
            previous: Some(&inside("2026-11-06T22:00")),
            running: true,
            ..Case::default()
        });
        assert_eq!(before.action, WindowAction::None);
        assert_eq!(
            before.window_key.as_deref(),
            Some("2026-11-06T22:00"),
            "跨午夜窗口 key 属于开始日"
        );

        let at_end = run(Case {
            windows: &windows,
            now: dt("2026-11-07", 2, 0),
            previous: Some(&inside("2026-11-06T22:00")),
            running: true,
            ..Case::default()
        });
        assert_eq!(at_end.action, WindowAction::Stop);
    }

    /// 并集边界：重叠/相邻窗口不产生启停动作，只有并集结束才停。
    #[test]
    fn union_boundaries_do_not_restart_or_stop() {
        let windows = vec![
            window(&[Mon], "08:00", "11:00"),
            window(&[Mon], "10:00", "12:00"),
        ];
        let merged_key = "2026-11-09T08:00";
        // 09:00：仍属同一并集，不重复启动（即使实例已退出）。
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-09", 9, 0),
            previous: Some(&inside(merged_key)),
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        assert_eq!(
            tick.persisted,
            PersistedWindowChange::Set(merged_key.to_owned())
        );
        // 11:00（内部边界）仍不动作。
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-09", 11, 0),
            previous: Some(&inside(merged_key)),
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        // 12:00 并集结束 → 停。
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-09", 12, 0),
            previous: Some(&inside(merged_key)),
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Stop);
    }

    /// 休眠跨窗唤醒：上一轮在窗口内、唤醒后已离开 → 停止；不补发历史启动动作。
    #[test]
    fn wake_after_crossing_window_end_stops() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 12, 0), // 睡了 4 小时
            previous: Some(&inside("2026-11-04T07:50")),
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::Stop);
        assert_eq!(tick.persisted, PersistedWindowChange::Keep);
    }

    /// 陈旧状态不误杀：上一轮观测在窗口内但实例已停 → 不停（没有可停对象）；
    /// 陈旧 key 也不导致窗口外启动。
    #[test]
    fn stale_state_does_not_kill_or_start() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 9, 0),
            previous: Some(&inside("2026-11-03T07:50")),
            persisted: Some("2026-11-03T07:50"),
            running: false,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        assert_eq!(tick.persisted, PersistedWindowChange::Clear);
    }

    /// 进入窗口时实例仍在运行（例如手动启动）：不重复启动，也不清除用户 Stop。
    #[test]
    fn entering_window_with_live_instance_does_not_start() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 7, 50),
            previous: Some(&outside()),
            running: true,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
        assert!(tick.entered_window);
        assert_eq!(
            tick.persisted,
            PersistedWindowChange::Set("2026-11-04T07:50".to_owned())
        );
    }

    /// backoff/paused 等监督状态未停止时，进入窗口也不抢启动。
    #[test]
    fn entering_window_respects_machine_state() {
        let windows = weekday_window();
        let tick = run(Case {
            windows: &windows,
            now: dt("2026-11-04", 7, 50),
            previous: Some(&outside()),
            machine_stopped: false,
            ..Case::default()
        });
        assert_eq!(tick.action, WindowAction::None);
    }

    /// 2026-11-05：schedule 任务窗口外不得由监督器自动开工；用户手动启动例外。
    #[test]
    fn supervisor_autostart_requires_window_or_user_intent() {
        assert!(
            may_supervisor_autostart(false, false, false),
            "非 schedule 任务不受时段约束"
        );
        assert!(
            may_supervisor_autostart(true, true, false),
            "在窗口内允许监督器自动重启"
        );
        assert!(
            may_supervisor_autostart(true, false, true),
            "用户手动启动过的任务在窗口外仍可被监督器重启"
        );
        assert!(
            !may_supervisor_autostart(true, false, false),
            "窗口外且无用户启动意图必须取消"
        );
    }
}
