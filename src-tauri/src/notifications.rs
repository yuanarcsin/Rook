//! 通知降级与节流（M3-5b，纯逻辑，不依赖 Tauri）。
//!
//! 只回答一个问题：**这次状态迁移要不要提醒用户，提醒几条**。真正的投递
//! （`notification:raised` 事件 / 托盘 tooltip / 核心日志）由 Engine 完成，
//! 因此本模块可以在 `cargo test` 里直接编译与测试。
//!
//! # 只发需要人介入的
//!
//! 与 docs/05 §8 对齐，只有"自动恢复不了、必须有人动手"的迁移才生成通知：
//!
//! | 事件 | 类型 | 级别 |
//! | --- | --- | --- |
//! | 进入 needs_human（扫码/登录/凭据/退出码） | [`NotificationKind::NeedsHuman`] | 中 |
//! | 熔断 paused(crash_loop) | [`NotificationKind::CrashLoop`] | 高 |
//! | 窗口限速 paused(restart_budget) | [`NotificationKind::RestartBudget`] | 高 |
//! | 启动失败且不会自动重启（paused 非熔断） | [`NotificationKind::StartFailed`] | 高 |
//! | 停止超时升级（进程可能仍在运行） | [`NotificationKind::StopTimeout`] | 高 |
//! | 配置加载失败 | [`NotificationKind::ConfigError`] | 高 |
//!
//! 正常退避重启、健康失败触发的重启、用户主动启停、应用启动完成都不生成通知；
//! 这些状态由托盘聚合颜色与 tooltip 表达。
//!
//! # 节流与去重
//!
//! - **同状态迁移去重**：进入某状态只发一次；恢复到正常监督后再次进入可以再发
//!   （[`NotificationCenter::clear`] / [`NotificationCenter::clear_task`]）；
//! - **同类节流**：同一任务 5 分钟内最多 1 条同类通知（docs/05 §8）；
//! - **`notify=false` 抑制**：任务级开关优先，不记录状态；
//! - **批量摘要**：[`NotificationCenter::drain`] 一次取出多条时合并为一条
//!   "N 个任务需要处理"，避免多任务同时异常时刷屏。

use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::Serialize;

/// 同一任务同类通知的最短间隔（docs/05 §8：5 分钟）。
pub const THROTTLE_WINDOW: Duration = Duration::from_secs(300);

/// 批量摘要最多列出的任务名数量。
const BATCH_NAME_LIMIT: usize = 5;

/// 需要人介入的通知类型（稳定英文标识用于事件负载与日志）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum NotificationKind {
    /// 进入 needs_human（心跳 status / 退出码 / 停止超时以外的来源）。
    NeedsHuman,
    /// 熔断：连续失败达到上限，paused(crash_loop)。
    CrashLoop,
    /// 启动窗口限速：窗口内启动次数超过上限，paused(restart_budget)。
    RestartBudget,
    /// 启动失败且不会自动重启（如 `restart.mode=never`）。
    StartFailed,
    /// 停止超时升级：进程可能仍在运行。
    StopTimeout,
    /// 配置加载失败（结构错误）。
    ConfigError,
}

impl NotificationKind {
    /// 全部类型（用于遍历清理/测试）。
    pub const ALL: [Self; 6] = [
        Self::NeedsHuman,
        Self::CrashLoop,
        Self::RestartBudget,
        Self::StartFailed,
        Self::StopTimeout,
        Self::ConfigError,
    ];

    /// 稳定的英文标识（事件负载/日志/前端共用）。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::NeedsHuman => "needs_human",
            Self::CrashLoop => "crash_loop",
            Self::RestartBudget => "restart_budget",
            Self::StartFailed => "start_failed",
            Self::StopTimeout => "stop_timeout",
            Self::ConfigError => "config_error",
        }
    }

    /// 通知标题（短，够扫一眼）。
    pub fn title(self) -> &'static str {
        match self {
            Self::NeedsHuman => "需要人工处理",
            Self::CrashLoop => "任务已暂停（连续失败）",
            Self::RestartBudget => "任务已暂停（重启过于频繁）",
            Self::StartFailed => "任务启动失败",
            Self::StopTimeout => "任务停止超时",
            Self::ConfigError => "配置加载失败",
        }
    }

    /// 通知级别：needs_human 为中，其余为高（docs/05 §8）。
    pub fn level(self) -> NotificationLevel {
        match self {
            Self::NeedsHuman => NotificationLevel::Medium,
            Self::CrashLoop
            | Self::RestartBudget
            | Self::StartFailed
            | Self::StopTimeout
            | Self::ConfigError => NotificationLevel::High,
        }
    }
}

/// 通知级别；`High > Medium`，合并摘要取最高级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum NotificationLevel {
    /// 需要人工处理（扫码/登录）。
    Medium,
    /// 需要处理（暂停/配置错误/停止超时）。
    High,
}

impl NotificationLevel {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Medium => "medium",
            Self::High => "high",
        }
    }
}

/// 一条已经决定要发出的通知（`notification:raised` 的事件负载）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RaisedNotification {
    /// [`NotificationKind::as_str`]。
    pub kind: &'static str,
    pub level: NotificationLevel,
    pub title: String,
    /// 发生了什么 + 影响范围 + 下一步（docs/05 §7.3）。
    pub message: String,
    /// 任务级通知的任务 id；配置错误为 `None`。
    pub task_id: Option<String>,
    pub task_name: Option<String>,
    /// 合并摘要覆盖的条目数；单条为 1。
    pub count: usize,
    /// 生成时刻，Unix epoch 毫秒。
    pub at: i64,
}

/// 任务的通知元信息（`notify=false` 时整条抑制）。
#[derive(Debug, Clone, Copy)]
pub struct TaskNotice<'a> {
    pub id: &'a str,
    pub name: &'a str,
    pub notify: bool,
}

/// 单任务单类型的节流/去重状态。
#[derive(Debug, Clone, Default)]
struct Entry {
    /// 当前是否处于该异常状态（同状态迁移去重）。
    active: bool,
    /// 最近一次实际发出的时刻（节流窗口）。
    last_raised: Option<Instant>,
}

/// 通知决策器：节流 + 去重 + 批量摘要。
///
/// 只做内存计算，不 emit、不做 I/O；Engine 在锁内调用 [`raise`](Self::raise)，
/// 释放锁后调用 [`drain`](Self::drain) 并投递。
#[derive(Debug, Clone)]
pub struct NotificationCenter {
    window: Duration,
    entries: HashMap<(String, NotificationKind), Entry>,
    /// 最近一次已通知的配置错误指纹（同一错误不重复）。
    config_fingerprint: Option<String>,
    /// 最近一次配置错误通知时刻（无任务可归属，按单槽节流）。
    config_last_raised: Option<Instant>,
    pending: Vec<RaisedNotification>,
}

impl Default for NotificationCenter {
    fn default() -> Self {
        Self::new(THROTTLE_WINDOW)
    }
}

impl NotificationCenter {
    /// 用指定节流窗口构造（测试注入短窗口）。
    pub fn new(window: Duration) -> Self {
        Self {
            window,
            entries: HashMap::new(),
            config_fingerprint: None,
            config_last_raised: None,
            pending: Vec::new(),
        }
    }

    /// 任务进入某类异常状态；返回是否真的生成了一条通知。
    ///
    /// - `notify=false` 直接返回 `false`，不记录状态（用户之后打开开关仍能收到）；
    /// - 同一状态重复进入只发一次；
    /// - 距上次同类通知不足窗口时抑制，但仍记为 active，避免窗口过后补发。
    pub fn raise(
        &mut self,
        now: Instant,
        at: i64,
        task: TaskNotice<'_>,
        kind: NotificationKind,
        message: String,
    ) -> bool {
        if !task.notify {
            return false;
        }
        let entry = self.entries.entry((task.id.to_owned(), kind)).or_default();
        if entry.active {
            return false;
        }
        entry.active = true;
        let throttled = entry
            .last_raised
            .is_some_and(|last| now.saturating_duration_since(last) < self.window);
        if throttled {
            return false;
        }
        entry.last_raised = Some(now);
        self.pending.push(RaisedNotification {
            kind: kind.as_str(),
            level: kind.level(),
            title: kind.title().to_owned(),
            message,
            task_id: Some(task.id.to_owned()),
            task_name: Some(task.name.to_owned()),
            count: 1,
            at,
        });
        true
    }

    /// 恢复到正常监督（running / 用户重新启动）后清除某类状态，允许再次进入时通知。
    ///
    /// 只清 `active`，保留节流时间戳：短时间内反复进出仍受 5 分钟窗口约束。
    pub fn clear(&mut self, task_id: &str, kind: NotificationKind) {
        if let Some(entry) = self.entries.get_mut(&(task_id.to_owned(), kind)) {
            entry.active = false;
        }
    }

    /// 清除一个任务的全部通知状态（任务恢复运行/被用户显式重新启动）。
    pub fn clear_task(&mut self, task_id: &str) {
        for kind in NotificationKind::ALL {
            self.clear(task_id, kind);
        }
    }

    /// 配置结构错误：只在错误指纹变化时通知一次。
    ///
    /// 反复重载同一个坏文件不会重复提醒；修好后 [`clear_config_error`](Self::clear_config_error)，
    /// 再次损坏（指纹不同）会重新通知，但仍受 5 分钟窗口约束。
    pub fn raise_config_error(
        &mut self,
        now: Instant,
        at: i64,
        fingerprint: &str,
        message: String,
    ) -> bool {
        if self.config_fingerprint.as_deref() == Some(fingerprint) {
            return false;
        }
        self.config_fingerprint = Some(fingerprint.to_owned());
        let throttled = self
            .config_last_raised
            .is_some_and(|last| now.saturating_duration_since(last) < self.window);
        if throttled {
            return false;
        }
        self.config_last_raised = Some(now);
        self.pending.push(RaisedNotification {
            kind: NotificationKind::ConfigError.as_str(),
            level: NotificationKind::ConfigError.level(),
            title: NotificationKind::ConfigError.title().to_owned(),
            message,
            task_id: None,
            task_name: None,
            count: 1,
            at,
        });
        true
    }

    /// 配置恢复正常：清掉指纹，下次损坏重新通知。
    pub fn clear_config_error(&mut self) {
        self.config_fingerprint = None;
    }

    /// 取出待投递的通知；多条同时到期时合并为一条摘要。
    pub fn drain(&mut self) -> Vec<RaisedNotification> {
        let items = std::mem::take(&mut self.pending);
        if items.len() <= 1 {
            return items;
        }
        vec![merge_batch(&items)]
    }

    /// 待投递条数（测试/诊断用）。
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }
}

/// 多条同时异常合并为一条摘要（"3 个任务需要处理"）。
fn merge_batch(items: &[RaisedNotification]) -> RaisedNotification {
    let count = items.len();
    // `High > Medium`：合并后按最严重的一条定级。
    let level = items
        .iter()
        .map(|item| item.level)
        .max()
        .unwrap_or(NotificationLevel::Medium);
    let kind = items
        .iter()
        .find(|item| item.level == level)
        .map_or(NotificationKind::NeedsHuman.as_str(), |item| item.kind);
    let names: Vec<&str> = items
        .iter()
        .filter_map(|item| item.task_name.as_deref())
        .take(BATCH_NAME_LIMIT)
        .collect();
    let detail = if names.is_empty() {
        "请打开面板查看详情。".to_owned()
    } else {
        format!("{} 等。请打开面板查看详情。", names.join("、"))
    };
    RaisedNotification {
        kind,
        level,
        title: "多个任务需要处理".to_owned(),
        message: format!("{count} 个任务需要处理：{detail}"),
        task_id: None,
        task_name: None,
        count,
        at: items.last().map_or(0, |item| item.at),
    }
}

/// 当前 Unix 毫秒（仅用于展示时间，不参与节流判定）。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as i64)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(id: &str) -> TaskNotice<'_> {
        TaskNotice {
            id,
            name: "任务",
            notify: true,
        }
    }

    fn center() -> NotificationCenter {
        NotificationCenter::new(THROTTLE_WINDOW)
    }

    fn raise(
        center: &mut NotificationCenter,
        now: Instant,
        id: &str,
        kind: NotificationKind,
    ) -> bool {
        center.raise(now, 0, notice(id), kind, format!("{id} {kind:?}"))
    }

    /// 进入只发一次；同一状态的重复事件不再生成通知。
    #[test]
    fn same_state_raises_once() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::CrashLoop));
        assert!(!raise(&mut center, now, "a", NotificationKind::CrashLoop));
        assert!(!raise(
            &mut center,
            now + Duration::from_secs(600),
            "a",
            NotificationKind::CrashLoop
        ));
        assert_eq!(center.pending_len(), 1);
    }

    /// 恢复后再次进入可以再发（前提是已过节流窗口）。
    #[test]
    fn reentry_after_recovery_raises_again() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::CrashLoop));
        center.clear("a", NotificationKind::CrashLoop);
        assert!(!raise(
            &mut center,
            now + Duration::from_secs(60),
            "a",
            NotificationKind::CrashLoop
        ));
        center.clear("a", NotificationKind::CrashLoop);
        assert!(raise(
            &mut center,
            now + Duration::from_secs(301),
            "a",
            NotificationKind::CrashLoop
        ));
        assert_eq!(center.pending_len(), 2);
    }

    /// 同一任务 5 分钟内同类通知最多一条；不同任务互不影响。
    #[test]
    fn throttle_is_per_task_and_kind() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
        center.clear("a", NotificationKind::NeedsHuman);
        assert!(!raise(
            &mut center,
            now + Duration::from_secs(299),
            "a",
            NotificationKind::NeedsHuman
        ));
        // 同类但不同任务：不受影响。
        assert!(raise(&mut center, now, "b", NotificationKind::NeedsHuman));
        // 同任务但不同类：不受影响。
        assert!(raise(&mut center, now, "a", NotificationKind::StopTimeout));
        assert_eq!(center.pending_len(), 3);
    }

    /// `notify=false` 完全抑制，且不记录状态（之后打开开关仍能收到）。
    #[test]
    fn notify_disabled_suppresses() {
        let mut center = center();
        let now = Instant::now();
        let muted = TaskNotice {
            id: "a",
            name: "任务",
            notify: false,
        };
        assert!(!center.raise(now, 0, muted, NotificationKind::NeedsHuman, "x".to_owned()));
        assert_eq!(center.pending_len(), 0);
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
    }

    /// 多任务同时异常合并为一条摘要，级别取最高，条目数保留。
    #[test]
    fn batch_merges_into_summary() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
        assert!(raise(&mut center, now, "b", NotificationKind::CrashLoop));
        assert!(raise(&mut center, now, "c", NotificationKind::NeedsHuman));
        let drained = center.drain();
        assert_eq!(drained.len(), 1);
        let summary = &drained[0];
        assert_eq!(summary.count, 3);
        assert_eq!(summary.level, NotificationLevel::High);
        assert_eq!(summary.kind, NotificationKind::CrashLoop.as_str());
        assert_eq!(summary.title, "多个任务需要处理");
        assert!(
            summary.message.starts_with("3 个任务需要处理"),
            "{}",
            summary.message
        );
        assert!(summary.task_id.is_none());
        assert_eq!(center.pending_len(), 0);
    }

    /// 单条不合并，保留任务信息与级别。
    #[test]
    fn single_notification_keeps_task() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
        let drained = center.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].task_id.as_deref(), Some("a"));
        assert_eq!(drained[0].count, 1);
        assert_eq!(drained[0].level, NotificationLevel::Medium);
        assert!(center.drain().is_empty());
    }

    /// 配置错误：同一指纹只发一次；恢复后再次损坏（指纹不同）可再发。
    #[test]
    fn config_error_dedupes_by_fingerprint() {
        let mut center = center();
        let now = Instant::now();
        assert!(center.raise_config_error(now, 0, "f1", "配置加载失败：f1".to_owned()));
        assert!(!center.raise_config_error(now, 0, "f1", "配置加载失败：f1".to_owned()));
        center.clear_config_error();
        assert!(!center.raise_config_error(
            now + Duration::from_secs(10),
            0,
            "f2",
            "配置加载失败：f2".to_owned()
        ));
        center.clear_config_error();
        assert!(center.raise_config_error(
            now + Duration::from_secs(301),
            0,
            "f3",
            "配置加载失败：f3".to_owned()
        ));
        assert_eq!(center.pending_len(), 2);
    }

    // ------------------------------------------------ needs_human 只通知一次（回归）

    /// 连续 blocked：同一 needs_human 状态反复上报只生成一条通知。
    #[test]
    fn needs_human_repeated_blocked_notifies_once() {
        let mut center = center();
        let now = Instant::now();
        for tick in 0..20 {
            assert_eq!(
                raise(
                    &mut center,
                    now + Duration::from_secs(tick),
                    "a",
                    NotificationKind::NeedsHuman
                ),
                tick == 0,
                "第 {tick} 次重复 blocked 不应再通知"
            );
        }
        let drained = center.drain();
        assert_eq!(drained.len(), 1);
        assert_eq!(drained[0].kind, NotificationKind::NeedsHuman.as_str());
        assert_eq!(drained[0].count, 1);
        assert_eq!(center.pending_len(), 0);
    }

    /// blocked → ok → blocked（5 分钟节流窗口内）：恢复清除 active，但不重复通知。
    #[test]
    fn needs_human_reentry_within_throttle_window_is_silent() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
        assert_eq!(center.drain().len(), 1);
        // 恢复：Engine 在 needs_human 清除/实例就绪时调用 clear（clear_task 同理）。
        center.clear("a", NotificationKind::NeedsHuman);
        assert!(!raise(
            &mut center,
            now + Duration::from_secs(60),
            "a",
            NotificationKind::NeedsHuman
        ));
        assert!(!raise(
            &mut center,
            now + Duration::from_secs(299),
            "a",
            NotificationKind::NeedsHuman
        ));
        assert_eq!(center.pending_len(), 0, "节流窗口内不得重复提醒");
    }

    /// 恢复后再进入、且已过 5 分钟：允许再次通知（节流不是永久抑制）。
    #[test]
    fn needs_human_reentry_after_throttle_notifies_again() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
        assert_eq!(center.drain().len(), 1);
        center.clear("a", NotificationKind::NeedsHuman);
        assert!(raise(
            &mut center,
            now + Duration::from_secs(301),
            "a",
            NotificationKind::NeedsHuman
        ));
        assert_eq!(center.pending_len(), 1);
    }

    /// 用户显式重试（clear_task）后再次 blocked：恢复受 5 分钟节流约束。
    #[test]
    fn needs_human_after_clear_task_respects_throttle() {
        let mut center = center();
        let now = Instant::now();
        assert!(raise(&mut center, now, "a", NotificationKind::NeedsHuman));
        center.clear_task("a");
        assert!(!raise(
            &mut center,
            now + Duration::from_secs(120),
            "a",
            NotificationKind::NeedsHuman
        ));
        center.clear_task("a");
        assert!(raise(
            &mut center,
            now + Duration::from_secs(302),
            "a",
            NotificationKind::NeedsHuman
        ));
        assert_eq!(center.pending_len(), 2);
    }

    /// 类型元数据与 docs/05 §8 的级别一致。
    #[test]
    fn kind_metadata_matches_docs() {
        assert_eq!(
            NotificationKind::NeedsHuman.level(),
            NotificationLevel::Medium
        );
        for kind in [
            NotificationKind::CrashLoop,
            NotificationKind::RestartBudget,
            NotificationKind::StartFailed,
            NotificationKind::StopTimeout,
            NotificationKind::ConfigError,
        ] {
            assert_eq!(kind.level(), NotificationLevel::High);
        }
        assert_eq!(NotificationKind::CrashLoop.as_str(), "crash_loop");
        assert_eq!(NotificationLevel::High.as_str(), "high");
        assert_eq!(NotificationKind::ALL.len(), 6);
    }
}
