//! M3-6a：配置热重载的字段分类与决策（纯逻辑，不依赖 Tauri/Engine/UI）。
//!
//! docs/02 §7.3 把配置变化分成几类，Engine 只负责按结论执行；本模块回答：
//!
//! - [`classify`]：这次重载改了哪些字段，属于"立即生效 / 重建监督 / 需要新进程 /
//!   只影响下次启动"的哪几类；
//! - [`decide`]：结合实例是否在运行与 `[supervisor].apply_changes`，得出
//!   "什么都不做 / 只重建监督状态 / 立即重启 / 标记 restart_pending"。
//!
//! 分类刻意按字段而不是整份 `TaskConfig` 比较：`id` 变化视为删除+新建
//! （docs/02 §7.3 末行），`enabled` 变化由 Engine 的 B3/B13 停止收敛处理，
//! 都不属于本模块的"进程字段"。
//!
//! 不启动进程、不读写文件、不调度定时器；全部可在单测里用假配置复现。

use std::collections::{HashMap, HashSet};

use crate::config::{ApplyChanges, TaskConfig};

/// 一次重载对单个任务的影响分类（docs/02 §7.3）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ConfigChange {
    /// 立即生效、不需要重建实例或监视器：
    /// `name` / `notify` / `panel_url` / `logs.encoding` /
    /// `logs.rotation` + `max_*`。
    ///
    /// 日志采集器的轮转参数在启动时固定，运行中实例沿用旧参数（下一次启动
    /// 采用新值）；这里不重启进程，避免为轮转参数打断长时间任务。
    pub immediate: bool,
    /// `health.*` / `restart.*` / `power.resume_grace_ms`：立即重建运行中实例的
    /// HealthMonitor 与 RestartMachine，重置探测调度与失败计数（不重启进程）。
    ///
    /// `power.resume_grace_ms` 是 [`HealthPolicy`] 的输入，必须重建监视器才
    /// 真正生效；它与 health.* 一样属于"立即生效、不重启进程"。
    pub supervision: bool,
    /// 需要新进程才能生效：`command` / `args` / `working_dir` / `env` /
    /// `elevated` / `ports` / `env_proxy` / `logs.dir` / `stop.*`。
    pub process: bool,
    /// `auto_start`：不影响当前运行，只影响下次监督器启动。
    pub auto_start: bool,
}

impl ConfigChange {
    /// 没有任何变化（等价于整份任务配置相同，或只改了 `enabled`）。
    pub fn is_empty(&self) -> bool {
        !(self.immediate || self.supervision || self.process || self.auto_start)
    }
}

/// 按字段比较两版任务配置，返回变化分类。
///
/// `previous` 是上一次成功加载的配置，`current` 是本次加载的配置；两者 id 相同。
pub fn classify(previous: &TaskConfig, current: &TaskConfig) -> ConfigChange {
    ConfigChange {
        immediate: previous.name != current.name
            || previous.notify != current.notify
            || previous.panel_url != current.panel_url
            || previous.logs.encoding != current.logs.encoding
            || previous.logs.rotation != current.logs.rotation
            || previous.logs.max_file_mb != current.logs.max_file_mb
            || previous.logs.max_files != current.logs.max_files
            || previous.logs.max_days != current.logs.max_days,
        supervision: previous.health != current.health
            || previous.restart != current.restart
            || previous.power != current.power,
        process: previous.command != current.command
            || previous.args != current.args
            || previous.working_dir != current.working_dir
            || previous.env != current.env
            || previous.elevated != current.elevated
            || previous.ports != current.ports
            || previous.env_proxy != current.env_proxy
            || previous.logs.dir != current.logs.dir
            || previous.stop != current.stop,
        auto_start: previous.auto_start != current.auto_start,
    }
}

/// 重载后对单个任务该做什么。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReconcileDecision {
    /// 不需要动作（无变化，或实例已停止、下次启动自然用新配置）。
    Keep,
    /// 只重建健康监视器/重启状态机，不重启进程。
    RefreshSupervision,
    /// 立即停止旧实例并用新配置启动（`apply_changes=restart`）。
    RestartNow,
    /// 保持旧进程，标记 `restart_pending`（`apply_changes=next_start`）。
    RestartNextStart,
}

/// 由变化分类 + 运行状态 + `apply_changes` 推导动作。
///
/// 进程相关字段的优先级高于监督字段：需要换进程时，重启本身就会重建
/// 监视器与状态机，不必再单独刷新（`RestartNow` / `RestartNextStart`）。
/// 实例已停止时一切变化都等下次启动，避免重载把已停止的任务拉起来。
pub fn decide(change: &ConfigChange, running: bool, apply: ApplyChanges) -> ReconcileDecision {
    if !running {
        return ReconcileDecision::Keep;
    }
    if change.process {
        return match apply {
            ApplyChanges::Restart => ReconcileDecision::RestartNow,
            ApplyChanges::NextStart => ReconcileDecision::RestartNextStart,
        };
    }
    if change.supervision {
        return ReconcileDecision::RefreshSupervision;
    }
    ReconcileDecision::Keep
}

/// 重载/命令结束后的配置比较基线。
///
/// 不在 in-flight 的任务写回本次加载的配置；仍在执行命令的任务保留上一版
/// 基线，等它自己的命令结束、`resync_after_reload` 真正比较并执行后再更新。
/// 否则 in-flight 期间被跳过的配置变化会在下一次无关重载时被重复判定
///（重复重启 / restart_pending 永远不清）。
///
/// 已从配置删除、但命令仍在执行的任务同样保留旧条目：删除收敛要等命令结束
/// 后由 resync 处理，提前丢掉基线会让这次删除不再被比较到。
pub fn update_baseline(
    previous: &HashMap<String, TaskConfig>,
    mut current: HashMap<String, TaskConfig>,
    in_flight: &HashSet<String>,
) -> HashMap<String, TaskConfig> {
    for id in in_flight {
        if let Some(old) = previous.get(id) {
            current.insert(id.clone(), old.clone());
        }
    }
    current
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, HashMap, HashSet};
    use std::path::PathBuf;

    use super::*;
    use crate::config::{
        Encoding, EnvProxy, HealthConfig, HealthKind, PowerConfig, RestartConfig, RestartMode,
        Rotation, StopConfig, TaskLogsConfig,
    };

    /// 测试基线：字段全部显式给出，便于逐项改动后比较分类。
    fn task() -> TaskConfig {
        TaskConfig {
            id: "alpha".to_owned(),
            name: "甲任务".to_owned(),
            enabled: true,
            order: 0,
            command: "node".to_owned(),
            args: vec!["server.js".to_owned()],
            working_dir: PathBuf::from("C:/work/alpha"),
            auto_start: true,
            elevated: false,
            notify: true,
            env: BTreeMap::new(),
            ports: vec![3300],
            env_proxy: EnvProxy::BypassLocalhost,
            panel_url: Some("http://127.0.0.1:3300/".to_owned()),
            plugin: None,
            schedule: crate::config::ScheduleConfig::default(),
            health: HealthConfig {
                kind: HealthKind::Http,
                url: Some("http://127.0.0.1:3300/health".to_owned()),
                ..HealthConfig::default()
            },
            restart: RestartConfig::default(),
            stop: StopConfig::default(),
            logs: TaskLogsConfig {
                dir: PathBuf::from("C:/logs/alpha"),
                rotation: Rotation::Size,
                max_file_mb: 10,
                max_files: 5,
                max_days: 14,
                encoding: Encoding::Auto,
            },
            power: PowerConfig::default(),
            limits: Default::default(),
        }
    }

    fn classify_one(mutate: impl FnOnce(&mut TaskConfig)) -> ConfigChange {
        let previous = task();
        let mut current = previous.clone();
        mutate(&mut current);
        classify(&previous, &current)
    }

    fn assert_only(change: ConfigChange, field: &str) {
        let expected = match field {
            "immediate" => ConfigChange {
                immediate: true,
                ..ConfigChange::default()
            },
            "supervision" => ConfigChange {
                supervision: true,
                ..ConfigChange::default()
            },
            "process" => ConfigChange {
                process: true,
                ..ConfigChange::default()
            },
            "auto_start" => ConfigChange {
                auto_start: true,
                ..ConfigChange::default()
            },
            other => panic!("未知分类 {other}"),
        };
        assert_eq!(change, expected);
    }

    #[test]
    fn identical_config_has_no_change() {
        let change = classify(&task(), &task());
        assert!(change.is_empty());
        assert_eq!(
            decide(&change, true, ApplyChanges::Restart),
            ReconcileDecision::Keep
        );
    }

    #[test]
    fn display_fields_take_effect_immediately() {
        assert_only(
            classify_one(|task| task.name = "乙任务".to_owned()),
            "immediate",
        );
        assert_only(classify_one(|task| task.notify = false), "immediate");
        assert_only(classify_one(|task| task.panel_url = None), "immediate");
        assert_only(
            classify_one(|task| task.logs.encoding = Encoding::Gb18030),
            "immediate",
        );
        assert_only(
            classify_one(|task| task.logs.rotation = Rotation::Daily),
            "immediate",
        );
        assert_only(classify_one(|task| task.logs.max_file_mb = 20), "immediate");
        assert_only(classify_one(|task| task.logs.max_files = 8), "immediate");
        assert_only(classify_one(|task| task.logs.max_days = 30), "immediate");
    }

    #[test]
    fn health_and_restart_rebuild_supervision_only() {
        assert_only(
            classify_one(|task| task.health.interval_ms = 5_000),
            "supervision",
        );
        assert_only(
            classify_one(|task| task.health.expect_body_contains = Some("{\"ok\":true".to_owned())),
            "supervision",
        );
        assert_only(
            classify_one(|task| task.health.needs_human_status = vec!["login_required".to_owned()]),
            "supervision",
        );
        assert_only(
            classify_one(|task| task.restart.mode = RestartMode::Always),
            "supervision",
        );
        assert_only(
            classify_one(|task| task.restart.max_consecutive_failures = 10),
            "supervision",
        );
        assert_only(
            classify_one(|task| task.restart.pause_on_exit_codes = vec![2]),
            "supervision",
        );
        // power.resume_grace_ms 是健康策略输入：立即生效但不重启进程。
        assert_only(
            classify_one(|task| task.power.resume_grace_ms = 60_000),
            "supervision",
        );
    }

    #[test]
    fn process_fields_need_a_new_process() {
        assert_only(
            classify_one(|task| task.command = "python".to_owned()),
            "process",
        );
        assert_only(
            classify_one(|task| task.args = vec!["other.js".to_owned()]),
            "process",
        );
        assert_only(
            classify_one(|task| task.working_dir = PathBuf::from("C:/work/beta")),
            "process",
        );
        assert_only(
            classify_one(|task| {
                task.env.insert("PORT".to_owned(), "3400".to_owned());
            }),
            "process",
        );
        assert_only(classify_one(|task| task.elevated = true), "process");
        assert_only(classify_one(|task| task.ports = vec![3400]), "process");
        assert_only(
            classify_one(|task| task.env_proxy = EnvProxy::Strip),
            "process",
        );
        assert_only(
            classify_one(|task| task.logs.dir = PathBuf::from("C:/logs/beta")),
            "process",
        );
        assert_only(
            classify_one(|task| task.stop.timeout_ms = 12_000),
            "process",
        );
    }

    #[test]
    fn auto_start_and_enabled_are_not_process_changes() {
        assert_only(classify_one(|task| task.auto_start = false), "auto_start");

        // enabled 由 B3/B13 的停止收敛处理，不属于这里的任何一类。
        let enabled_change = classify_one(|task| task.enabled = false);
        assert!(enabled_change.is_empty());
    }

    #[test]
    fn stopped_instances_never_restart_on_reload() {
        let change = classify_one(|task| task.command = "python".to_owned());
        assert_eq!(
            decide(&change, false, ApplyChanges::Restart),
            ReconcileDecision::Keep
        );
        assert_eq!(
            decide(&change, false, ApplyChanges::NextStart),
            ReconcileDecision::Keep
        );
    }

    #[test]
    fn apply_changes_decides_between_restart_and_next_start() {
        let process = classify_one(|task| task.args = vec!["next.js".to_owned()]);
        assert_eq!(
            decide(&process, true, ApplyChanges::Restart),
            ReconcileDecision::RestartNow
        );
        assert_eq!(
            decide(&process, true, ApplyChanges::NextStart),
            ReconcileDecision::RestartNextStart
        );

        let supervision = classify_one(|task| task.health.interval_ms = 3_000);
        assert_eq!(
            decide(&supervision, true, ApplyChanges::Restart),
            ReconcileDecision::RefreshSupervision
        );
        assert_eq!(
            decide(&supervision, true, ApplyChanges::NextStart),
            ReconcileDecision::RefreshSupervision
        );

        let immediate = classify_one(|task| task.name = "改名".to_owned());
        assert_eq!(
            decide(&immediate, true, ApplyChanges::Restart),
            ReconcileDecision::Keep
        );
    }

    #[test]
    fn process_change_wins_over_supervision_change() {
        let change = classify_one(|task| {
            task.command = "python".to_owned();
            task.health.interval_ms = 4_000;
        });
        assert!(change.process && change.supervision);
        assert_eq!(
            decide(&change, true, ApplyChanges::Restart),
            ReconcileDecision::RestartNow
        );
    }

    #[test]
    fn baseline_keeps_in_flight_config_and_updates_finished_tasks() {
        let old = task();
        let mut new = task();
        new.args = vec!["next.js".to_owned()];
        let previous = HashMap::from([(old.id.clone(), old.clone())]);
        let current = HashMap::from([(new.id.clone(), new.clone())]);

        // 命令仍在执行：保留旧基线，命令结束后的 resync 才能发现这次变化。
        let in_flight = HashSet::from([old.id.clone()]);
        let kept = update_baseline(&previous, current.clone(), &in_flight);
        assert_eq!(kept.get("alpha").map(|task| &task.args), Some(&old.args));

        // 命令已结束：写回当前配置，下一次无关重载不再重复判定。
        let updated = update_baseline(&previous, current, &HashSet::new());
        assert_eq!(updated.get("alpha").map(|task| &task.args), Some(&new.args));
    }

    #[test]
    fn baseline_handles_added_and_removed_tasks() {
        let old = task();
        let previous = HashMap::from([(old.id.clone(), old.clone())]);

        // 新增任务不在 in-flight：直接进入基线。
        let mut added = task();
        added.id = "beta".to_owned();
        let mut current = HashMap::from([("beta".to_owned(), added.clone())]);
        let updated = update_baseline(&previous, current.clone(), &HashSet::new());
        assert!(updated.contains_key("beta"));
        assert!(!updated.contains_key("alpha"));

        // 删除任务仍在 in-flight：保留旧条目，等命令结束再收敛。
        current.remove("beta");
        let in_flight = HashSet::from([old.id.clone()]);
        let kept = update_baseline(&previous, current, &in_flight);
        assert!(kept.contains_key("alpha"));
    }

    #[test]
    fn baseline_uses_current_for_new_in_flight_task() {
        // 命令开始时任务尚不在上一版基线（首次加载）：没有旧值可保留。
        let mut current = task();
        current.id = "gamma".to_owned();
        let current = HashMap::from([("gamma".to_owned(), current.clone())]);
        let in_flight = HashSet::from(["gamma".to_owned()]);
        let baseline = update_baseline(&HashMap::new(), current, &in_flight);
        assert_eq!(
            baseline.get("gamma").map(|task| task.id.as_str()),
            Some("gamma")
        );
    }
}
