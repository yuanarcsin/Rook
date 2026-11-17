//! 每周运行时段（2026-11-04 用户决策；2026-11-05 架构修正）。
//!
//! 语义（与 `docs/15-plugin-spec.md` / ADR-030 一致）：
//! - 只支持本地时间的每周重复时段；不做通用 cron，不做节假日/一次性日历；
//! - `start > end` 表示跨午夜，`days` 指开始那一天；
//! - 多个时段取并集；时段内重叠允许；
//! - 本模块只做纯时间计算，不读配置、不碰进程、不依赖 Tauri。
//!
//! 2026-11-05 架构修正（一个时钟、一个开关）：`[tasks.schedule]` 是唯一时钟，
//! `schedule.enabled=true` 时应用是唯一权威——进入窗口自动启动一次，离开窗口
//! 无条件停止任何存活实例；配置里的 `start_in_window`/`stop_outside` 只保留
//! 兼容，引擎完全忽略。决策纯逻辑见 [`window`]；`CAMPUS_SENTRY_WINDOW_END`
//! 的计算见 [`window_end_rfc3339`]。
//!
//! 本文件由 backend-core 实现完整逻辑与测试；接口保持稳定，改动需同步
//! `config/raw.rs`、`config/model.rs`、`snapshot.rs` 与 `src/lib/api.ts`。

use chrono::{
    Datelike, Local, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, TimeDelta, TimeZone,
    Timelike, Weekday as ChronoWeekday,
};

pub mod window;

/// 一周中的一天（TOML 里用 `mon`..`sun`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Weekday {
    Mon,
    Tue,
    Wed,
    Thu,
    Fri,
    Sat,
    Sun,
}

impl Weekday {
    /// 周一至周日，顺序固定。
    pub const ALL: [Weekday; 7] = [
        Weekday::Mon,
        Weekday::Tue,
        Weekday::Wed,
        Weekday::Thu,
        Weekday::Fri,
        Weekday::Sat,
        Weekday::Sun,
    ];

    /// 配置/快照中的稳定小写英文取值。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mon => "mon",
            Self::Tue => "tue",
            Self::Wed => "wed",
            Self::Thu => "thu",
            Self::Fri => "fri",
            Self::Sat => "sat",
            Self::Sun => "sun",
        }
    }

    /// 解析 `mon`..`sun`（大小写不敏感）；其他返回 `None`。
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "mon" => Some(Self::Mon),
            "tue" => Some(Self::Tue),
            "wed" => Some(Self::Wed),
            "thu" => Some(Self::Thu),
            "fri" => Some(Self::Fri),
            "sat" => Some(Self::Sat),
            "sun" => Some(Self::Sun),
            _ => None,
        }
    }

    fn chrono(self) -> ChronoWeekday {
        match self {
            Self::Mon => ChronoWeekday::Mon,
            Self::Tue => ChronoWeekday::Tue,
            Self::Wed => ChronoWeekday::Wed,
            Self::Thu => ChronoWeekday::Thu,
            Self::Fri => ChronoWeekday::Fri,
            Self::Sat => ChronoWeekday::Sat,
            Self::Sun => ChronoWeekday::Sun,
        }
    }
}

/// 一个每周运行时段。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Window {
    /// 生效的星期；空表示永不生效（校验层会拒绝空列表）。
    pub days: Vec<Weekday>,
    /// 开始时刻（本地时间）。
    pub start: NaiveTime,
    /// 结束时刻（本地时间）；`end <= start` 时跨到次日。
    pub end: NaiveTime,
}

impl Window {
    /// `end <= start` 表示跨到次日；`end == start` 视为 24 小时。
    ///
    /// 配置校验层会拒绝 `end == start`，这里仍按"跨到次日"兜底，
    /// 保证纯逻辑模块面对任意输入都有确定行为。
    fn crosses_midnight(&self) -> bool {
        self.end <= self.start
    }

    /// 以 `start_date` 为开始日算出这次时段出现的绝对起止时间。
    fn occurrence(&self, start_date: NaiveDate) -> (NaiveDateTime, NaiveDateTime) {
        let start = start_date.and_time(self.start);
        let end_date = if self.crosses_midnight() {
            start_date + TimeDelta::days(1)
        } else {
            start_date
        };
        (start, end_date.and_time(self.end))
    }
}

/// 一组时段（取并集）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Schedule {
    pub windows: Vec<Window>,
}

/// 当前命中的并集区间及其绝对起止时间。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ActiveWindow {
    /// 产生并集区间开始时刻的窗口在 `Schedule::windows` 中的下标。
    pub index: usize,
    /// 并集区间开始（重叠/相邻窗口已合并）。
    pub start: NaiveDateTime,
    /// 并集区间结束（含跨午夜）。
    pub end: NaiveDateTime,
}

impl Schedule {
    pub fn is_empty(&self) -> bool {
        self.windows.is_empty()
    }

    /// `now` 命中的并集区间。
    ///
    /// 区间语义：`start` 含、`end` 不含。重叠或首尾相接的时段会合并成
    /// 一个连续区间，`start`/`end` 用**并集边界**（内部边界不产生启停动作，
    /// 与 docs/15 §8.3 一致）；`index` 是产生该区间开始时刻的窗口下标。
    pub fn active(&self, now: NaiveDateTime) -> Option<ActiveWindow> {
        let (from, to) = self.horizon(now);
        let (start, end) = self
            .merged_intervals(from, to)
            .into_iter()
            .find(|(start, end)| *start <= now && now < *end)?;
        let index = self
            .windows
            .iter()
            .position(|window| window.occurrence(start.date()).0 == start)
            .unwrap_or(0);
        Some(ActiveWindow { index, start, end })
    }

    /// 从 `now` 之后（严格大于）最近一次并集区间开始。
    pub fn next_start(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        let (from, to) = self.horizon(now);
        self.merged_intervals(from, to)
            .into_iter()
            .map(|(start, _)| start)
            .find(|start| *start > now)
    }

    /// 从 `now` 之后（严格大于）最近一次并集区间结束。
    ///
    /// 当前在区间内时返回**本区间**的结束（并集结束）；不在区间内时返回
    /// 下一次区间的结束。跨午夜窗口的结束落在开始日次日。
    pub fn next_end(&self, now: NaiveDateTime) -> Option<NaiveDateTime> {
        let (from, to) = self.horizon(now);
        let intervals = self.merged_intervals(from, to);
        if let Some((_, end)) = intervals
            .iter()
            .find(|(start, end)| *start <= now && now < *end)
        {
            return Some(*end);
        }
        intervals
            .into_iter()
            .map(|(_, end)| end)
            .find(|end| *end > now)
    }

    /// 把 [from, to] 内每个窗口出现展开成绝对区间，并按并集合并
    /// （重叠或首尾相接都算同一连续运行区间）。
    fn merged_intervals(
        &self,
        from: NaiveDate,
        to: NaiveDate,
    ) -> Vec<(NaiveDateTime, NaiveDateTime)> {
        let mut raw: Vec<(NaiveDateTime, NaiveDateTime)> = Vec::new();
        let mut date = from;
        while date <= to {
            for window in &self.windows {
                if day_matches(date, &window.days) {
                    raw.push(window.occurrence(date));
                }
            }
            date += TimeDelta::days(1);
        }
        raw.sort();
        let mut merged: Vec<(NaiveDateTime, NaiveDateTime)> = Vec::with_capacity(raw.len());
        for (start, end) in raw {
            match merged.last_mut() {
                Some(last) if start <= last.1 => {
                    if end > last.1 {
                        last.1 = end;
                    }
                }
                _ => merged.push((start, end)),
            }
        }
        merged
    }

    /// 计算并集区间所需的日期窗口：
    /// 回看 1 天（覆盖昨天的跨午夜时段），向前 8 天（每个星期至少出现一次）。
    fn horizon(&self, now: NaiveDateTime) -> (NaiveDate, NaiveDate) {
        let today = now.date();
        (today - TimeDelta::days(1), today + TimeDelta::days(8))
    }

    /// 当前时段的稳定 key（用于持久化"用户已在本时段内停止"），
    /// 例如 `2026-11-04T07:50`。不在时段内返回 `None`。
    pub fn window_key(&self, now: NaiveDateTime) -> Option<String> {
        self.active(now)
            .map(|active| active.start.format("%Y-%m-%dT%H:%M").to_string())
    }
}

/// 解析 `HH:MM`（也接受 `H:MM`）；非法返回 `None`。
pub fn parse_time(text: &str) -> Option<NaiveTime> {
    NaiveTime::parse_from_str(text.trim(), "%H:%M").ok()
}

/// 格式化为 `HH:MM`。
pub fn format_time(time: NaiveTime) -> String {
    format!("{:02}:{:02}", time.hour(), time.minute())
}

/// 当前本地时间（naive local）。
pub fn now_local() -> NaiveDateTime {
    Local::now().naive_local()
}

/// 本地 naive 时间 → 带时区偏移的 RFC3339（秒精度）。
///
/// 例：`2026-11-05T09:58:00+08:00`。用于注入 `CAMPUS_SENTRY_WINDOW_END`，
/// 脚本可以直接 `datetime.fromisoformat` 解析。DST 重复的本地时间取较早者
/// （中国无 DST，仅作兜底）；无法映射时返回 `None`（调用方不注入）。
pub fn format_local_rfc3339(datetime: NaiveDateTime) -> Option<String> {
    Local
        .from_local_datetime(&datetime)
        .earliest()
        .map(|value| value.to_rfc3339_opts(SecondsFormat::Secs, false))
}

/// `CAMPUS_SENTRY_WINDOW_END` 的取值（2026-11-05 用户决策）。
///
/// - `enabled=false`：不注入（脚本按自己的开关工作）；
/// - 没有配置时段：不注入（无结束时间可承诺）；
/// - 当前在并集窗口内：本并集区间的结束时刻；
/// - 当前在窗口外：下一次并集区间的结束时刻（即“下一个必须停止的点”）；
/// - 跨午夜时结束落在开始日次日；本地时间带时区偏移。
pub fn window_end_rfc3339(enabled: bool, windows: &[Window], now: NaiveDateTime) -> Option<String> {
    if !enabled || windows.is_empty() {
        return None;
    }
    let schedule = Schedule {
        windows: windows.to_vec(),
    };
    schedule.next_end(now).and_then(format_local_rfc3339)
}

/// 本地 naive 时间 → Unix epoch 毫秒；处理 DST 边界（中国无 DST，仍做兜底）。
pub fn to_epoch_millis(datetime: NaiveDateTime) -> Option<i64> {
    Local
        .from_local_datetime(&datetime)
        .earliest()
        .map(|value| value.timestamp_millis())
}

/// 判断日期是否是某个星期的开始日；供实现/测试使用。
fn day_matches(date: NaiveDate, days: &[Weekday]) -> bool {
    let weekday = date.weekday();
    days.iter().any(|day| day.chrono() == weekday)
}

#[cfg(test)]
mod tests {
    use super::*;
    use Weekday::{Fri, Mon, Sat, Sun, Thu, Tue, Wed};
    use chrono::Offset;

    fn dt(date: &str, hour: u32, minute: u32) -> NaiveDateTime {
        let date = NaiveDate::parse_from_str(date, "%Y-%m-%d").expect("日期");
        date.and_time(NaiveTime::from_hms_opt(hour, minute, 0).expect("时间"))
    }

    fn window(days: &[Weekday], start: &str, end: &str) -> Window {
        Window {
            days: days.to_vec(),
            start: parse_time(start).expect("start 应合法"),
            end: parse_time(end).expect("end 应合法"),
        }
    }

    fn schedule(windows: Vec<Window>) -> Schedule {
        Schedule { windows }
    }

    #[test]
    fn weekday_window_is_active_during_business_hour() {
        let schedule = schedule(vec![window(&[Mon, Tue, Wed, Thu, Fri], "07:50", "08:20")]);

        // 2026-11-04 是周三。
        let now = dt("2026-11-04", 8, 0);
        let active = schedule.active(now).expect("应命中工作日时段");
        assert_eq!(active.index, 0);
        assert_eq!(active.start, dt("2026-11-04", 7, 50));
        assert_eq!(active.end, dt("2026-11-04", 8, 20));
        assert_eq!(
            schedule.window_key(now).as_deref(),
            Some("2026-11-04T07:50")
        );
    }

    #[test]
    fn weekend_window_is_inactive_and_next_start_is_monday() {
        let schedule = schedule(vec![window(&[Mon, Tue, Wed, Thu, Fri], "07:50", "08:20")]);

        // 2026-11-07 是周六。
        let saturday = dt("2026-11-07", 8, 0);
        assert!(schedule.active(saturday).is_none());
        assert!(schedule.window_key(saturday).is_none());
        assert_eq!(
            schedule.next_start(saturday),
            Some(dt("2026-11-09", 7, 50)),
            "周六之后的下一次开始是下周一"
        );
        assert_eq!(schedule.next_end(saturday), Some(dt("2026-11-09", 8, 20)));
    }

    #[test]
    fn weekend_only_window_is_active_on_saturday() {
        let schedule = schedule(vec![window(&[Sat, Sun], "09:00", "11:00")]);

        assert!(schedule.active(dt("2026-11-07", 10, 0)).is_some());
        // 周三不在周末时段内。
        assert!(schedule.active(dt("2026-11-04", 10, 0)).is_none());
        assert_eq!(
            schedule.next_start(dt("2026-11-04", 10, 0)),
            Some(dt("2026-11-07", 9, 0))
        );
    }

    #[test]
    fn cross_midnight_window_belongs_to_start_day() {
        // 周五 22:00 → 周六 02:00，`days` 指开始日（周五）。
        let schedule = schedule(vec![window(&[Fri], "22:00", "02:00")]);

        let friday_night = dt("2026-11-06", 23, 0);
        let active = schedule.active(friday_night).expect("周五深夜应命中");
        assert_eq!(active.start, dt("2026-11-06", 22, 0));
        assert_eq!(active.end, dt("2026-11-07", 2, 0));
        assert_eq!(
            schedule.next_end(friday_night),
            Some(dt("2026-11-07", 2, 0))
        );

        // 周六凌晨仍属于周五开始的时段。
        let saturday_early = dt("2026-11-07", 1, 0);
        let active = schedule.active(saturday_early).expect("跨午夜后仍应命中");
        assert_eq!(active.start, dt("2026-11-06", 22, 0));
        assert_eq!(
            schedule.window_key(saturday_early).as_deref(),
            Some("2026-11-06T22:00")
        );
        // 周六凌晨的下一次开始是下周五。
        assert_eq!(
            schedule.next_start(saturday_early),
            Some(dt("2026-11-13", 22, 0))
        );

        // 02:00 是结束边界，不含在时段内。
        assert!(schedule.active(dt("2026-11-07", 2, 0)).is_none());
    }

    #[test]
    fn sunday_night_spills_into_monday() {
        let schedule = schedule(vec![window(&[Sun], "23:00", "01:00")]);
        // 2026-11-08 周日。
        let monday_early = dt("2026-11-09", 0, 30);
        let active = schedule.active(monday_early).expect("周一凌晨仍在时段内");
        assert_eq!(active.start, dt("2026-11-08", 23, 0));
        assert_eq!(active.end, dt("2026-11-09", 1, 0));
    }

    #[test]
    fn boundaries_are_start_inclusive_end_exclusive() {
        let schedule = schedule(vec![window(&Weekday::ALL, "08:00", "09:00")]);

        assert!(
            schedule.active(dt("2026-11-04", 8, 0)).is_some(),
            "start 含"
        );
        assert!(
            schedule.active(dt("2026-11-04", 9, 0)).is_none(),
            "end 不含"
        );
        assert!(schedule.active(dt("2026-11-04", 7, 59)).is_none());
        assert!(schedule.active(dt("2026-11-04", 8, 59)).is_some());
    }

    #[test]
    fn empty_schedule_never_matches() {
        let schedule = Schedule::default();
        assert!(schedule.is_empty());

        let now = dt("2026-11-04", 8, 0);
        assert!(schedule.active(now).is_none());
        assert!(schedule.next_start(now).is_none());
        assert!(schedule.next_end(now).is_none());
        assert!(schedule.window_key(now).is_none());
    }

    #[test]
    fn window_with_empty_days_never_matches() {
        // 校验层会拒绝空 days；纯逻辑层保持"永不生效"的确定行为。
        let schedule = schedule(vec![window(&[], "08:00", "09:00")]);
        assert!(schedule.active(dt("2026-11-04", 8, 30)).is_none());
        assert!(schedule.next_start(dt("2026-11-04", 8, 30)).is_none());
        assert!(schedule.next_end(dt("2026-11-04", 8, 30)).is_none());
    }

    #[test]
    fn next_start_is_strictly_after_now() {
        let schedule = schedule(vec![window(&[Mon], "08:00", "09:00")]);

        // now 恰好等于 start 时，下一次开始是下周一同一时刻。
        let at_start = dt("2026-11-09", 8, 0);
        assert_eq!(schedule.next_start(at_start), Some(dt("2026-11-16", 8, 0)));

        let before = dt("2026-11-09", 7, 59);
        assert_eq!(schedule.next_start(before), Some(at_start));

        // 时段内也取下一次。
        assert_eq!(
            schedule.next_start(dt("2026-11-09", 8, 30)),
            Some(dt("2026-11-16", 8, 0))
        );
    }

    #[test]
    fn next_end_is_strictly_after_now() {
        let schedule = schedule(vec![window(&[Mon], "08:00", "09:00")]);

        assert_eq!(
            schedule.next_end(dt("2026-11-09", 9, 0)),
            Some(dt("2026-11-16", 9, 0))
        );
        assert_eq!(
            schedule.next_end(dt("2026-11-09", 8, 30)),
            Some(dt("2026-11-09", 9, 0))
        );
        assert_eq!(
            schedule.next_end(dt("2026-11-09", 8, 0)),
            Some(dt("2026-11-09", 9, 0))
        );
    }

    #[test]
    fn next_end_after_current_window_finished_returns_next_week() {
        // 回归：本次时段的 end 已过时，不能直接 break 返回 None，
        // 必须继续看后续匹配日（下周一 11:00）。
        let weekly = schedule(vec![window(&[Mon], "09:00", "11:00")]);

        assert_eq!(
            weekly.next_end(dt("2026-11-09", 12, 0)),
            Some(dt("2026-11-16", 11, 0))
        );
        assert_eq!(
            weekly.next_end(dt("2026-11-09", 11, 0)),
            Some(dt("2026-11-16", 11, 0)),
            "end 是开区间：等于 now 时取下一次"
        );

        // 多天时段同样要看后续匹配日：Mon/Wed，周三 12:00 之后的下一次结束是下周一。
        let multi = schedule(vec![window(&[Mon, Wed], "09:00", "11:00")]);
        assert_eq!(
            multi.next_end(dt("2026-11-04", 12, 0)),
            Some(dt("2026-11-09", 11, 0))
        );

        // 跨午夜时段：周五 22:00-02:00，周六 03:00（已结束）→ 下周六 02:00。
        let overnight = schedule(vec![window(&[Fri], "22:00", "02:00")]);
        assert_eq!(
            overnight.next_end(dt("2026-11-07", 3, 0)),
            Some(dt("2026-11-14", 2, 0))
        );
    }

    #[test]
    fn overlapping_windows_merge_and_keep_union_boundaries() {
        let schedule = schedule(vec![
            window(&[Mon], "09:00", "12:00"),
            window(&[Mon], "08:00", "11:00"),
        ]);

        // 并集区间是 08:00–12:00，窗口内部边界（09:00/11:00）不产生启停动作。
        let active = schedule
            .active(dt("2026-11-09", 9, 30))
            .expect("重叠区间应命中");
        assert_eq!(active.index, 1, "index 指向产生并集开始时刻的窗口");
        assert_eq!(active.start, dt("2026-11-09", 8, 0));
        assert_eq!(active.end, dt("2026-11-09", 12, 0));
        assert_eq!(
            schedule.window_key(dt("2026-11-09", 9, 30)).as_deref(),
            Some("2026-11-09T08:00")
        );
        assert_eq!(
            schedule.next_end(dt("2026-11-09", 9, 30)),
            Some(dt("2026-11-09", 12, 0)),
            "next_end 是并集结束，不是某个窗口的结束"
        );
        assert_eq!(
            schedule.next_start(dt("2026-11-09", 9, 30)),
            Some(dt("2026-11-16", 8, 0)),
            "区间内的窗口开始不算下一次开始"
        );

        // 11:30 仍属于同一个并集区间，key 不变。
        let active = schedule.active(dt("2026-11-09", 11, 30)).expect("仍应命中");
        assert_eq!(active.index, 1);
        assert_eq!(active.start, dt("2026-11-09", 8, 0));
        assert_eq!(active.end, dt("2026-11-09", 12, 0));
        assert_eq!(
            schedule.window_key(dt("2026-11-09", 11, 30)).as_deref(),
            Some("2026-11-09T08:00")
        );
    }

    #[test]
    fn multiple_windows_are_union() {
        let schedule = schedule(vec![
            window(&[Mon], "08:00", "10:00"),
            window(&[Mon], "12:00", "14:00"),
        ]);

        assert!(schedule.active(dt("2026-11-09", 8, 30)).is_some());
        assert!(
            schedule.active(dt("2026-11-09", 11, 0)).is_none(),
            "间隙不生效"
        );
        assert!(schedule.active(dt("2026-11-09", 12, 30)).is_some());
        assert_eq!(
            schedule.next_start(dt("2026-11-09", 11, 0)),
            Some(dt("2026-11-09", 12, 0))
        );
        assert_eq!(
            schedule.next_end(dt("2026-11-09", 11, 0)),
            Some(dt("2026-11-09", 14, 0))
        );
    }

    #[test]
    fn adjacent_windows_merge_into_one_interval() {
        let schedule = schedule(vec![
            window(&[Wed], "08:00", "09:00"),
            window(&[Wed], "09:00", "10:00"),
        ]);
        let active = schedule.active(dt("2026-11-04", 9, 0)).expect("无缝衔接");
        assert_eq!(active.index, 0, "并集从第一个窗口开始");
        assert_eq!(active.start, dt("2026-11-04", 8, 0));
        assert_eq!(active.end, dt("2026-11-04", 10, 0));
        assert_eq!(
            schedule.window_key(dt("2026-11-04", 9, 0)).as_deref(),
            Some("2026-11-04T08:00")
        );
        assert_eq!(
            schedule.next_end(dt("2026-11-04", 9, 0)),
            Some(dt("2026-11-04", 10, 0))
        );
        assert_eq!(
            schedule.next_start(dt("2026-11-04", 9, 0)),
            Some(dt("2026-11-11", 8, 0)),
            "区间内部边界不是下一次开始"
        );
    }

    #[test]
    fn union_window_key_is_stable_for_user_stop_suppression() {
        // docs/15 §8.3 例：上午/下午两个首尾相接的窗口 = 一个连续区间。
        let schedule = schedule(vec![
            window(&[Mon, Tue, Wed, Thu, Fri], "07:50", "12:10"),
            window(&[Mon, Tue, Wed, Thu, Fri], "12:10", "18:10"),
        ]);

        for (hour, minute) in [(7, 50), (8, 30), (11, 59), (12, 10), (13, 0), (18, 9)] {
            let now = dt("2026-11-04", hour, minute);
            assert_eq!(
                schedule.window_key(now).as_deref(),
                Some("2026-11-04T07:50"),
                "{hour:02}:{minute:02} 应属于同一个并集区间"
            );
            assert_eq!(
                schedule.next_end(now),
                Some(dt("2026-11-04", 18, 10)),
                "并集结束固定为 18:10"
            );
        }
        assert!(schedule.active(dt("2026-11-04", 18, 10)).is_none());
        assert_eq!(
            schedule.next_start(dt("2026-11-04", 13, 0)),
            Some(dt("2026-11-05", 7, 50))
        );
    }

    #[test]
    fn weekday_parse_and_display_roundtrip() {
        for day in Weekday::ALL {
            assert_eq!(Weekday::parse(day.as_str()), Some(day));
        }
        assert_eq!(Weekday::parse("MON"), Some(Mon));
        assert_eq!(Weekday::parse(" SUN "), Some(Sun));
        // 只接受三字母缩写（配置文档约定 mon..sun）。
        assert_eq!(Weekday::parse(" sunday "), None);
        assert_eq!(Weekday::parse("monday"), None);
        assert_eq!(Weekday::parse(""), None);
    }

    #[test]
    fn time_parse_and_format() {
        let created = parse_time("07:50").expect("HH:MM");
        assert_eq!(format_time(created), "07:50");
        assert_eq!(
            parse_time("7:50"),
            Some(NaiveTime::from_hms_opt(7, 50, 0).expect("时间")),
            "H:MM 也应接受"
        );
        assert_eq!(
            parse_time(" 08:05 "),
            Some(NaiveTime::from_hms_opt(8, 5, 0).expect("时间"))
        );
        assert_eq!(parse_time("24:00"), None);
        assert_eq!(parse_time("08:60"), None);
        assert_eq!(parse_time("8"), None);
        assert_eq!(parse_time(""), None);
    }

    #[test]
    fn local_time_helpers_are_consistent() {
        let value = dt("2026-11-04", 12, 0);
        let millis = to_epoch_millis(value).expect("本地时间应可映射到 epoch");
        assert!(millis > 0);
        // naive_local 与 Local::now() 同一时区；只验证不 panic 且落在合理范围。
        let now = now_local();
        assert!(now.year() >= 2024);
    }

    // ------------------------------------------------------- CAMPUS_SENTRY_WINDOW_END

    /// 本地 RFC3339：同一天时刻 + 当前时区偏移，秒精度，不带 Z。
    #[test]
    fn format_local_rfc3339_keeps_offset_and_seconds() {
        use chrono::DateTime;
        let value = dt("2026-11-05", 9, 58);
        let text = format_local_rfc3339(value).expect("本地时间应可格式化");
        assert_eq!(text.len(), 25, "秒精度 + 6 字符偏移：{text}");
        assert!(text.contains('T'), "{text}");
        assert!(!text.ends_with('Z'), "必须是带偏移的本地时间：{text}");
        let parsed = DateTime::parse_from_rfc3339(&text).expect("RFC3339 可解析");
        assert_eq!(parsed.naive_local(), value);
        let expected = Local
            .from_local_datetime(&value)
            .earliest()
            .expect("本地时间");
        assert_eq!(
            parsed.offset().fix().local_minus_utc(),
            expected.offset().fix().local_minus_utc()
        );
    }

    /// 窗口内：返回当前并集结束（拒绝“下一次”语义）。
    #[test]
    fn window_end_inside_uses_current_union_end() {
        let schedule = schedule(vec![
            window(&[Mon], "08:00", "10:00"),
            window(&[Mon], "09:00", "12:00"),
        ]);
        let now = dt("2026-11-09", 9, 30);
        let text = window_end_rfc3339(true, &schedule.windows, now).expect("窗口内应有结束时间");
        assert_eq!(
            text,
            format_local_rfc3339(dt("2026-11-09", 12, 0)).expect("格式化")
        );
        assert!(
            text.starts_with("2026-11-09T12:00:00"),
            "必须是并集结束时刻：{text}"
        );
    }

    /// 窗口外：返回下一次并集结束（跨天/跨周都取最近一次）。
    #[test]
    fn window_end_outside_uses_next_union_end() {
        let schedule = schedule(vec![window(&[Mon], "08:00", "10:00")]);
        let now = dt("2026-11-09", 12, 30);
        let text = window_end_rfc3339(true, &schedule.windows, now).expect("应有下一次结束");
        assert_eq!(
            text,
            format_local_rfc3339(dt("2026-11-16", 10, 0)).expect("格式化")
        );

        // 周一 07:00（窗口开始前）→ 当天 10:00。
        let text = window_end_rfc3339(true, &schedule.windows, dt("2026-11-09", 7, 0))
            .expect("应有下一次结束");
        assert_eq!(
            text,
            format_local_rfc3339(dt("2026-11-09", 10, 0)).expect("格式化")
        );
    }

    /// 跨午夜：窗口内返回次日结束；窗口外返回下一次跨午夜结束。
    #[test]
    fn window_end_handles_cross_midnight() {
        let schedule = schedule(vec![window(&[Fri], "22:00", "02:00")]);
        let inside =
            window_end_rfc3339(true, &schedule.windows, dt("2026-11-06", 23, 0)).expect("窗口内");
        assert_eq!(
            inside,
            format_local_rfc3339(dt("2026-11-07", 2, 0)).expect("格式化")
        );
        let outside =
            window_end_rfc3339(true, &schedule.windows, dt("2026-11-07", 3, 0)).expect("窗口外");
        assert_eq!(
            outside,
            format_local_rfc3339(dt("2026-11-14", 2, 0)).expect("格式化")
        );
    }

    /// 无 windows 或未启用：不注入（None），与引擎行为一致。
    #[test]
    fn window_end_skips_disabled_and_empty_schedule() {
        let schedule = schedule(vec![window(&[Mon], "08:00", "10:00")]);
        let now = dt("2026-11-09", 9, 0);
        assert!(
            window_end_rfc3339(false, &schedule.windows, now).is_none(),
            "schedule 未启用不注入"
        );
        assert!(
            window_end_rfc3339(true, &[], now).is_none(),
            "没有时段不注入"
        );
        assert!(
            window_end_rfc3339(false, &[], now).is_none(),
            "未启用且无时段不注入"
        );
    }
}
