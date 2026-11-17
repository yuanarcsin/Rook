//! 托盘聚合纯逻辑（从 `tray` 抽出，不依赖 Tauri）。
//!
//! 这里只做"快照 → 级别 + 计数 + 文案"的纯计算：不读全局状态、不做 I/O、
//! 不碰 GUI，因此可以在 `cargo test` 里直接编译与测试（`tray` 模块本身是
//! `cfg(not(test))`，测试二进制不链接 WebView2/托盘代码）。
//!
//! 文案与优先级由 `tray` 复用，修改时必须保持与菜单/tooltip 的展示一致。

use crate::brand::DISPLAY_NAME;
use crate::snapshot::{Snapshot, TaskSnapshot};

/// tooltip 上限：Win32 `szTip` 为 128 个宽字符（含结尾 NUL），这里按字符数截断。
pub(crate) const TOOLTIP_MAX_CHARS: usize = 127;

/// 托盘聚合级别。数值越大越严重，`max` 即"红 > 琥珀 > 绿 > 灰"。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum AggregateLevel {
    /// 无任务，或全部已停止/已禁用。
    #[default]
    Gray,
    /// 至少一个运行中，且没有红/琥珀级任务。
    Green,
    /// 退避重试、启动失败、启动中/停止中。
    Amber,
    /// 暂停、需要人工、配置无效、整份配置错误。
    Red,
}

/// 单个任务的聚合归类；每个任务恰好落进一个计数桶。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum TrayTone {
    /// 运行中且无异常。
    Running,
    /// 任务级配置无效。
    Invalid,
    /// 退避重试 / 启动失败 / 非 0 自然退出 / 启动中、停止中。
    Failing,
    /// 已暂停（含 crash_loop 等暂停原因）。
    Paused,
    /// 等待人工处理（扫码/登录/凭据）。
    NeedsHuman,
    /// 已停止或已禁用，且没有异常。
    Idle,
}

/// 托盘聚合结果：级别 + 互斥计数桶（各桶之和等于任务总数）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct TraySummary {
    level: AggregateLevel,
    total: usize,
    /// 运行中且无异常。
    normal: usize,
    /// 配置无效、退避、启动失败、非 0 退出、启动中/停止中。
    abnormal: usize,
    paused: usize,
    needs_human: usize,
    /// 已停止或已禁用。
    stopped: usize,
    /// 整份配置不可用（结构错误 / 读取失败）。
    config_error: bool,
}

impl TraySummary {
    /// 聚合级别；`tray` 用它决定角标颜色。
    pub(crate) fn level(&self) -> AggregateLevel {
        self.level
    }
}

/// 纯聚合：快照 → 级别与计数，不读全局状态、不做 I/O。
///
/// 级别判定（优先级 红 > 琥珀 > 绿 > 灰）：
/// - 红：整份配置错误，或任一任务 paused / needs_human / 配置无效；
/// - 琥珀：任一任务退避重试、启动失败、非 0 自然退出、启动中/停止中；
/// - 绿：至少一个运行中且没有红/琥珀；
/// - 灰：没有任务，或全部已停止/已禁用。
///
/// 注：`needs_human` 一律计入红级（需求指定），因此"运行中 + 需要人工"也落在红级，
/// 不会退化到琥珀级。
pub(crate) fn aggregate(snapshot: &Snapshot) -> TraySummary {
    let mut summary = TraySummary {
        total: snapshot.tasks.len(),
        config_error: snapshot.config_error.is_some(),
        ..TraySummary::default()
    };
    let mut level = if summary.config_error {
        AggregateLevel::Red
    } else {
        AggregateLevel::Gray
    };

    for task in &snapshot.tasks {
        let task_level = match classify(task) {
            TrayTone::Running => {
                summary.normal += 1;
                AggregateLevel::Green
            }
            TrayTone::Invalid => {
                summary.abnormal += 1;
                AggregateLevel::Red
            }
            TrayTone::Failing => {
                summary.abnormal += 1;
                AggregateLevel::Amber
            }
            TrayTone::Paused => {
                summary.paused += 1;
                AggregateLevel::Red
            }
            TrayTone::NeedsHuman => {
                summary.needs_human += 1;
                AggregateLevel::Red
            }
            TrayTone::Idle => {
                summary.stopped += 1;
                AggregateLevel::Gray
            }
        };
        level = level.max(task_level);
    }

    summary.level = level;
    summary
}

/// 单任务归类：人工 > 暂停 > 配置无效 > 失败 > 运行中 > 空闲。
pub(crate) fn classify(task: &TaskSnapshot) -> TrayTone {
    if task.supervisor.needs_human.active {
        return TrayTone::NeedsHuman;
    }
    if task.supervisor.paused_reason.is_some() {
        return TrayTone::Paused;
    }
    if !task.valid {
        return TrayTone::Invalid;
    }
    if is_failing(task) {
        return TrayTone::Failing;
    }
    match status(task) {
        Some("running") => TrayTone::Running,
        // 尚未稳定：启动中/停止中都按"未就绪"计入异常（琥珀）。
        Some("starting" | "stopping") => TrayTone::Failing,
        _ => TrayTone::Idle,
    }
}

/// 失败判定：监督层退避、最近一次启动失败，或自然退出且退出码非 0。
pub(crate) fn is_failing(task: &TaskSnapshot) -> bool {
    if task.supervisor.last_status.as_deref() == Some("backoff") {
        return true;
    }
    let Some(runtime) = task.runtime.as_ref() else {
        return false;
    };
    runtime.last_error.is_some()
        || (runtime.status == "stopped"
            && runtime.exit_reason.as_deref() == Some("natural")
            && runtime.exit_code.is_some_and(|code| code != 0))
}

/// 运行时状态字符串；没有运行时视图时视为"无状态"（空闲）。
fn status(task: &TaskSnapshot) -> Option<&str> {
    task.runtime.as_ref().map(|runtime| runtime.status.as_str())
}

/// 展示状态 → 菜单文案（A3）。
///
/// 状态本身由快照的 `displayStatus` 决定（后端权威规则），这里只做映射；
/// 未识别的取值统一显示"未知"，避免把内部标识直接暴露给用户。
pub(crate) fn status_label(display: &str) -> &'static str {
    match display {
        "invalid" => "无效",
        "unknown" => "未知",
        "starting" => "启动中",
        "running" => "运行中",
        "stopping" => "停止中",
        "paused" => "已暂停",
        "needs_human" => "需要人工处理",
        "backoff" => "退避重试",
        "disabled" => "已禁用",
        "stopped" => "已停止",
        _ => "未知",
    }
}

pub(crate) fn level_label(level: AggregateLevel) -> &'static str {
    match level {
        AggregateLevel::Red => "需要处理",
        AggregateLevel::Amber => "正在恢复",
        AggregateLevel::Green => "全部正常",
        AggregateLevel::Gray => "无运行任务",
    }
}

/// 计数文案：只列出非零桶，顺序固定为 正常 · 异常 · 暂停 · 需要人工 · 停止。
pub(crate) fn counts_text(summary: &TraySummary) -> String {
    let mut parts: Vec<String> = Vec::with_capacity(5);
    if summary.normal > 0 {
        parts.push(format!("{} 正常", summary.normal));
    }
    if summary.abnormal > 0 {
        parts.push(format!("{} 异常", summary.abnormal));
    }
    if summary.paused > 0 {
        parts.push(format!("{} 暂停", summary.paused));
    }
    if summary.needs_human > 0 {
        parts.push(format!("{} 需要人工", summary.needs_human));
    }
    if summary.stopped > 0 {
        parts.push(format!("{} 停止", summary.stopped));
    }
    parts.join(" · ")
}

/// 菜单顶部摘要项文案：短计数，异常状态有明确说明。
pub(crate) fn menu_summary_text(snapshot: &Snapshot) -> String {
    let summary = aggregate(snapshot);
    if summary.config_error {
        return "配置错误，任务未加载".to_owned();
    }
    if summary.total == 0 {
        return "没有任务".to_owned();
    }
    counts_text(&summary)
}

/// tooltip：级别 + 计数 + 总任务数，超长按字符截断到 [`TOOLTIP_MAX_CHARS`]。
pub(crate) fn tooltip_text(snapshot: &Snapshot) -> String {
    let summary = aggregate(snapshot);
    let text = if summary.config_error {
        format!("{DISPLAY_NAME} · 配置错误 · 无法读取任务，请打开面板")
    } else if summary.total == 0 {
        format!("{DISPLAY_NAME} · 没有任务")
    } else {
        format!(
            "{DISPLAY_NAME} · {} · {} · 共 {} 个任务",
            level_label(summary.level),
            counts_text(&summary),
            summary.total
        )
    };
    truncate_chars(&text, TOOLTIP_MAX_CHARS)
}

/// 按字符（非字节）截断，超长时以 `…` 结尾；结果字符数不超过 `max`。
pub(crate) fn truncate_chars(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_owned();
    }
    let mut truncated: String = text.chars().take(max - 1).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::{
        ConfigErrorView, LogStatsView, NeedsHumanView, RuntimeErrorView, RuntimeView,
        SupervisorView,
    };

    fn task(id: &str) -> TaskSnapshot {
        TaskSnapshot {
            id: id.to_owned(),
            name: id.to_owned(),
            enabled: true,
            command: "node".to_owned(),
            args: Vec::new(),
            working_dir: "C:/work".to_owned(),
            auto_start: false,
            elevated: false,
            notify: false,
            health_type: "process".to_owned(),
            ports: Vec::new(),
            panel_url: None,
            plugin: None,
            schedule: crate::snapshot::ScheduleView::default(),
            stop: crate::snapshot::StopView {
                method: "graceful".to_owned(),
                grace_ms: 2500,
                timeout_ms: 8000,
            },
            valid: true,
            issues: Vec::new(),
            runtime: None,
            supervisor: SupervisorView::default(),
            display_status: "unknown".to_owned(),
            allowed_actions: crate::snapshot::AllowedActions::default(),
        }
    }

    fn runtime(status: &str) -> RuntimeView {
        RuntimeView {
            status: status.to_owned(),
            pid: None,
            instance_id: None,
            started_at: None,
            exit_code: None,
            exit_reason: None,
            last_error: None,
            tree_empty: None,
            forced_kill: false,
            log_path: String::new(),
            log_stats: LogStatsView::default(),
        }
    }

    fn running(id: &str) -> TaskSnapshot {
        let mut task = task(id);
        task.runtime = Some(runtime("running"));
        task
    }

    fn stopped(id: &str) -> TaskSnapshot {
        let mut task = task(id);
        task.runtime = Some(runtime("stopped"));
        task
    }

    fn paused(id: &str) -> TaskSnapshot {
        let mut task = stopped(id);
        task.supervisor.paused_reason = Some("crash_loop".to_owned());
        task
    }

    fn needs_human(id: &str) -> TaskSnapshot {
        let mut task = running(id);
        task.supervisor.needs_human = NeedsHumanView {
            active: true,
            source: Some("heartbeat".to_owned()),
            message: None,
            since: None,
        };
        task
    }

    fn failed(id: &str) -> TaskSnapshot {
        let mut task = stopped(id);
        task.runtime = Some(RuntimeView {
            last_error: Some(RuntimeErrorView {
                stage: "spawn".to_owned(),
                code: None,
                message: "启动失败".to_owned(),
            }),
            ..runtime("stopped")
        });
        task
    }

    fn backoff(id: &str) -> TaskSnapshot {
        let mut task = stopped(id);
        task.supervisor.last_status = Some("backoff".to_owned());
        task
    }

    fn snapshot(tasks: Vec<TaskSnapshot>) -> Snapshot {
        Snapshot {
            app_version: "0.0.0".to_owned(),
            config_path: "C:/Rook.toml".to_owned(),
            config_revision: "rev-test".to_owned(),
            data_dir: Some("C:/data".to_owned()),
            config_error: None,
            issues: Vec::new(),
            tasks,
            confirm_exit: true,
            tray_hint_pending: false,
            config_watch: crate::config::ConfigWatchView::default(),
        }
    }

    /// 红 > 琥珀 > 绿 > 灰：只要有一个暂停任务，全绿的其余任务也不能压成绿。
    #[test]
    fn red_wins_over_running() {
        let summary = aggregate(&snapshot(vec![
            running("a"),
            running("b"),
            paused("c"),
            needs_human("d"),
        ]));
        assert_eq!(summary.level, AggregateLevel::Red);
        assert_eq!(summary.total, 4);
        assert_eq!(summary.normal, 2);
        assert_eq!(summary.paused, 1);
        assert_eq!(summary.needs_human, 1);
        assert_eq!(summary.abnormal, 0);
        assert_eq!(
            summary.normal
                + summary.abnormal
                + summary.paused
                + summary.needs_human
                + summary.stopped,
            summary.total
        );
    }

    /// 配置无效单独触发红级，并计入异常桶。
    #[test]
    fn invalid_task_is_red() {
        let mut invalid = task("bad");
        invalid.valid = false;
        let summary = aggregate(&snapshot(vec![running("a"), invalid]));
        assert_eq!(summary.level, AggregateLevel::Red);
        assert_eq!(summary.abnormal, 1);
        assert_eq!(summary.normal, 1);
    }

    /// 退避重试、启动失败、启动中都属于琥珀级。
    #[test]
    fn failures_are_amber() {
        let mut starting = task("s");
        starting.runtime = Some(runtime("starting"));
        let summary = aggregate(&snapshot(vec![
            running("a"),
            backoff("b"),
            failed("c"),
            starting,
        ]));
        assert_eq!(summary.level, AggregateLevel::Amber);
        assert_eq!(summary.abnormal, 3);
        assert_eq!(summary.normal, 1);
    }

    /// 自然退出且退出码非 0 计入异常；用户停止不算。
    #[test]
    fn natural_nonzero_exit_is_abnormal() {
        let mut crashed = stopped("crash");
        if let Some(runtime) = crashed.runtime.as_mut() {
            runtime.exit_reason = Some("natural".to_owned());
            runtime.exit_code = Some(3);
        }
        let mut user_stop = stopped("user");
        if let Some(runtime) = user_stop.runtime.as_mut() {
            runtime.exit_reason = Some("user_stop".to_owned());
            runtime.exit_code = Some(1);
        }
        let summary = aggregate(&snapshot(vec![crashed, user_stop]));
        assert_eq!(summary.level, AggregateLevel::Amber);
        assert_eq!(summary.abnormal, 1);
        assert_eq!(summary.stopped, 1);
    }

    /// 绿：至少一个运行中且无红/琥珀；灰：无任务或全部停止/禁用。
    #[test]
    fn green_and_gray_levels() {
        assert_eq!(
            aggregate(&snapshot(vec![running("a"), stopped("b")])).level,
            AggregateLevel::Green
        );
        assert_eq!(
            aggregate(&snapshot(vec![stopped("a")])).level,
            AggregateLevel::Gray
        );
        let mut disabled = task("off");
        disabled.enabled = false;
        assert_eq!(
            aggregate(&snapshot(vec![disabled])).level,
            AggregateLevel::Gray
        );
        assert_eq!(aggregate(&snapshot(Vec::new())).level, AggregateLevel::Gray);
    }

    /// `tray` 通过该访问器取角标颜色（测试构建里 `tray` 不编译，需在这里覆盖）。
    #[test]
    fn level_accessor_exposes_aggregate_level() {
        assert_eq!(
            aggregate(&snapshot(vec![running("a")])).level(),
            AggregateLevel::Green
        );
        assert_eq!(
            aggregate(&snapshot(Vec::new())).level(),
            AggregateLevel::Gray
        );
    }

    /// 整份配置错误：即使没有任务也是红级，且有明确文案。
    #[test]
    fn config_error_is_red_with_explicit_text() {
        let mut snapshot = snapshot(Vec::new());
        snapshot.config_error = Some(ConfigErrorView {
            file: "C:/Rook.toml".to_owned(),
            line: 1,
            col: 1,
            path: String::new(),
            message: "配置版本不受支持".to_owned(),
        });

        let summary = aggregate(&snapshot);
        assert_eq!(summary.level, AggregateLevel::Red);
        assert!(summary.config_error);
        assert_eq!(summary.total, 0);
        assert_eq!(menu_summary_text(&snapshot), "配置错误，任务未加载");

        let tooltip = tooltip_text(&snapshot);
        assert!(tooltip.contains("配置错误"), "{tooltip}");
        assert!(tooltip.chars().count() <= TOOLTIP_MAX_CHARS);
    }

    /// tooltip 含各级计数与总任务数，且不超过 127 字符。
    #[test]
    fn tooltip_lists_counts_and_total() {
        let snapshot = snapshot(vec![
            running("a"),
            running("b"),
            running("c"),
            failed("d"),
            paused("e"),
            needs_human("f"),
        ]);
        let tooltip = tooltip_text(&snapshot);
        assert_eq!(
            tooltip,
            format!(
                "{DISPLAY_NAME} · 需要处理 · 3 正常 · 1 异常 · 1 暂停 · 1 需要人工 · 共 6 个任务"
            )
        );
        assert!(tooltip.chars().count() <= TOOLTIP_MAX_CHARS);
        assert_eq!(
            menu_summary_text(&snapshot),
            "3 正常 · 1 异常 · 1 暂停 · 1 需要人工"
        );
    }

    /// 空快照有明确文案。
    #[test]
    fn empty_snapshot_has_explicit_text() {
        let snapshot = snapshot(Vec::new());
        assert_eq!(
            tooltip_text(&snapshot),
            format!("{DISPLAY_NAME} · 没有任务")
        );
        assert_eq!(menu_summary_text(&snapshot), "没有任务");
    }

    /// 任务很多时 tooltip 仍在 127 字符内。
    #[test]
    fn tooltip_respects_limit_for_many_tasks() {
        let tasks = (0..200)
            .map(|index| running(&format!("t{index}")))
            .collect::<Vec<_>>();
        let tooltip = tooltip_text(&snapshot(tasks));
        assert!(tooltip.chars().count() <= TOOLTIP_MAX_CHARS);
        assert!(tooltip.contains("200 正常"));
        assert!(tooltip.contains("共 200 个任务"));
    }

    /// 截断按字符计数（中文不会被切坏），超长以省略号结尾。
    #[test]
    fn truncate_chars_limits_by_chars() {
        let long = "任务状态".repeat(50);
        let truncated = truncate_chars(&long, TOOLTIP_MAX_CHARS);
        assert_eq!(truncated.chars().count(), TOOLTIP_MAX_CHARS);
        assert!(truncated.ends_with('…'));

        assert_eq!(truncate_chars("短文案", TOOLTIP_MAX_CHARS), "短文案");
        assert_eq!(truncate_chars("短文案啊", 3), "短文…");
        assert_eq!(truncate_chars("短文案", 0), "");
    }

    /// A3：10 种权威展示状态都有菜单文案，未识别取值统一"未知"。
    #[test]
    fn status_label_covers_all_display_states() {
        for (display, label) in [
            ("invalid", "无效"),
            ("unknown", "未知"),
            ("starting", "启动中"),
            ("running", "运行中"),
            ("stopping", "停止中"),
            ("paused", "已暂停"),
            ("needs_human", "需要人工处理"),
            ("backoff", "退避重试"),
            ("disabled", "已禁用"),
            ("stopped", "已停止"),
        ] {
            assert_eq!(status_label(display), label, "状态 {display}");
        }
        assert_eq!(status_label(""), "未知");
        assert_eq!(status_label("BOOM"), "未知");
    }
}
