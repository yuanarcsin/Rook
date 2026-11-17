//! 原始反序列化结构：字段全部可缺失（`Option`），并开启 `deny_unknown_fields`。
//!
//! 这里刻意使用 `i64` / `f64` / `String` 而不是目标类型，好处是：
//! - 类型错误由 serde 统一报出（带 span），与"数值越界"分开处理；
//! - 负数能被识别并给出明确的范围错误，而不是溢出成巨大的 `u64`。

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};

/// 接受整数或浮点（`multiplier = 2` 与 `2.0` 都合法）。
fn de_opt_f64<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<f64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Num {
        Float(f64),
        Int(i64),
    }

    Ok(
        Option::<Num>::deserialize(deserializer)?.map(|num| match num {
            Num::Float(value) => value,
            Num::Int(value) => value as f64,
        }),
    )
}

/// 接受整数或整数数组（`expect_status = 200` / `= [200]`）。
fn de_opt_int_list<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<Vec<i64>>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum OneOrMany {
        One(i64),
        Many(Vec<i64>),
    }

    Ok(
        Option::<OneOrMany>::deserialize(deserializer)?.map(|value| match value {
            OneOrMany::One(single) => vec![single],
            OneOrMany::Many(many) => many,
        }),
    )
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawConfig {
    pub version: Option<i64>,
    #[serde(default)]
    pub app: RawApp,
    #[serde(default)]
    pub supervisor: RawSupervisor,
    #[serde(default)]
    pub autostart: RawAutostart,
    #[serde(default)]
    pub notifications: RawNotifications,
    #[serde(default)]
    pub logs: RawLogs,
    #[serde(default)]
    pub tasks: Vec<RawTask>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawApp {
    pub language: Option<String>,
    pub theme: Option<String>,
    /// 2026-11-04 起废弃：保留字段只为兼容旧配置；手动启动总是显示窗口。
    pub start_minimized: Option<bool>,
    pub confirm_exit: Option<bool>,
    pub data_dir: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSupervisor {
    pub start_stagger_ms: Option<i64>,
    pub shutdown_timeout_ms: Option<i64>,
    pub config_poll_interval_ms: Option<i64>,
    pub watch_config: Option<bool>,
    pub apply_changes: Option<String>,
    pub resume_grace_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawAutostart {
    pub enabled: Option<bool>,
    pub mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawNotifications {
    pub enabled: Option<bool>,
    pub crash_loop: Option<bool>,
    pub health_degraded: Option<bool>,
    pub recovered: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLogs {
    pub core_level: Option<String>,
    pub retention_days: Option<i64>,
    pub max_total_mb: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTask {
    pub id: Option<String>,
    pub name: Option<String>,
    pub enabled: Option<bool>,
    /// 展示顺序；缺省按 `[[tasks]]` 在文件中的顺序（C7）。
    pub order: Option<i64>,
    pub command: Option<String>,
    pub args: Option<Vec<String>>,
    pub working_dir: Option<String>,
    pub auto_start: Option<bool>,
    pub elevated: Option<bool>,
    pub notify: Option<bool>,
    pub ports: Option<Vec<i64>>,
    pub env_proxy: Option<String>,
    pub panel_url: Option<String>,
    pub env: Option<BTreeMap<String, String>>,
    /// 插件清单导入时写入的元数据；手工任务为 None。
    pub plugin_id: Option<String>,
    pub plugin_manifest: Option<String>,
    pub plugin_version: Option<String>,
    #[serde(default)]
    pub schedule: RawSchedule,
    #[serde(default)]
    pub health: RawHealth,
    #[serde(default)]
    pub restart: RawRestart,
    #[serde(default)]
    pub stop: RawStop,
    #[serde(default)]
    pub logs: RawTaskLogs,
    #[serde(default)]
    pub power: RawPower,
    #[serde(default)]
    pub limits: RawLimits,
}

/// `[tasks.limits]`：资源守卫（C6）；`None`/0 = 不限制。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawLimits {
    pub max_rss_mb: Option<i64>,
    pub max_cpu_percent: Option<i64>,
}

/// `[tasks.schedule]`：每周运行时段（2026-11-04 用户决策）。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawSchedule {
    pub enabled: Option<bool>,
    pub start_in_window: Option<bool>,
    pub stop_outside: Option<bool>,
    #[serde(default)]
    pub windows: Vec<RawScheduleWindow>,
}

/// `[[tasks.schedule.windows]]`；`start > end` 表示跨午夜。
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawScheduleWindow {
    pub days: Option<Vec<String>>,
    pub start: Option<String>,
    pub end: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawHealth {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub interval_ms: Option<i64>,
    pub timeout_ms: Option<i64>,
    pub failure_threshold: Option<i64>,
    pub startup_grace_ms: Option<i64>,
    pub url: Option<String>,
    #[serde(default, deserialize_with = "de_opt_int_list")]
    pub expect_status: Option<Vec<i64>>,
    pub expect_body_contains: Option<String>,
    pub path: Option<String>,
    pub max_age_ms: Option<i64>,
    pub needs_human_status: Option<Vec<String>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawRestart {
    pub mode: Option<String>,
    pub initial_delay_ms: Option<i64>,
    pub max_delay_ms: Option<i64>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    pub multiplier: Option<f64>,
    #[serde(default, deserialize_with = "de_opt_f64")]
    pub jitter: Option<f64>,
    pub max_consecutive_failures: Option<i64>,
    pub stable_after_ms: Option<i64>,
    /// 启动窗口（毫秒）；0 = 不限。
    pub window_ms: Option<i64>,
    /// 窗口内最多启动次数；0 = 不限。
    pub max_starts_in_window: Option<i64>,
    pub pause_on_exit_codes: Option<Vec<i64>>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawStop {
    pub method: Option<String>,
    pub grace_ms: Option<i64>,
    pub timeout_ms: Option<i64>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawTaskLogs {
    pub dir: Option<String>,
    pub rotation: Option<String>,
    pub max_file_mb: Option<i64>,
    pub max_files: Option<i64>,
    pub max_days: Option<i64>,
    pub encoding: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RawPower {
    pub resume_grace_ms: Option<i64>,
}
