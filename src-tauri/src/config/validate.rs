//! 校验与归一化。
//!
//! 严重级别划分（与 docs/02 §6 对齐）：
//!
//! | 级别 | 触发条件 | 结果 |
//! | --- | --- | --- |
//! | 结构错误（[`ConfigError`]） | TOML 语法/类型/未知字段、version、重复 id、枚举取值、数值范围与关系、列表长度 | 整份配置拒绝加载 |
//! | 任务错误（[`Severity::Error`]） | command 找不到、working_dir 不存在、health 必填缺失/非法、env 覆盖保留变量、ports 越界、panel_url 非法、路径展开失败 | 该任务 invalid，其余任务照常加载 |
//! | 警告（[`Severity::Warning`]） | process 粗粒度监督、needs_human_status 配在非 heartbeat、expect_body_contains 配在非 http、已废弃的 schedule.start_in_window/stop_outside、显式设置预留字段（app.language/theme、notifications 三个开关、tasks.elevated） | 配置照常生效 |
//!
//! 行/列定位是启发式的：在对应 `[[tasks]]` 块内找形如 `key = ...` 的行；
//! 找不到时退化为 1:1，绝不因为定位失败而影响校验结果。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use super::error::{ConfigError, ConfigIssue, Severity};
use super::expand::{EnvVars, expand};
use super::model::*;
use super::raw::*;
use crate::schedule::{Weekday, Window, parse_time};

/// 顶层（非任务）校验结果。
pub(crate) struct Document {
    pub version: u32,
    pub app: AppConfig,
    pub supervisor: SupervisorConfig,
    pub autostart: AutostartConfig,
    pub notifications: NotificationsConfig,
    pub logs: LogsConfig,
}

/// 源文本索引：把字段名映射回行/列。
struct Source<'a> {
    text: &'a str,
    lines: Vec<&'a str>,
    /// 每个 `[[tasks]]` 块的行范围（0 起，左闭右开）。
    task_blocks: Vec<(usize, usize)>,
}

impl<'a> Source<'a> {
    fn new(text: &'a str) -> Self {
        let lines: Vec<&str> = text.lines().collect();
        let starts: Vec<usize> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.trim_start().starts_with("[[tasks]]"))
            .map(|(index, _)| index)
            .collect();
        let task_blocks = starts
            .iter()
            .enumerate()
            .map(|(position, &start)| {
                (
                    start,
                    starts.get(position + 1).copied().unwrap_or(lines.len()),
                )
            })
            .collect();
        Self {
            text,
            lines,
            task_blocks,
        }
    }

    fn line_col(&self, byte: usize) -> (usize, usize) {
        let bytes = self.text.as_bytes();
        let upto = byte.min(bytes.len());
        let mut line = 1;
        let mut line_start = 0;
        for (index, byte) in bytes[..upto].iter().enumerate() {
            if *byte == b'\n' {
                line += 1;
                line_start = index + 1;
            }
        }
        let col = self.text[line_start..upto].chars().count() + 1;
        (line, col)
    }

    fn locate(&self, key: &str, block: Option<usize>) -> Option<(usize, usize)> {
        let (start, end) = match block {
            Some(index) => self
                .task_blocks
                .get(index)
                .copied()
                .unwrap_or((0, self.lines.len())),
            None => (0, self.lines.len()),
        };
        self.locate_in(start, end, key)
    }

    fn locate_any(&self, key: &str) -> Option<(usize, usize)> {
        self.locate_in(0, self.lines.len(), key)
    }

    fn locate_in(&self, start: usize, end: usize, key: &str) -> Option<(usize, usize)> {
        for index in start..end.min(self.lines.len()) {
            let line = self.lines[index];
            let trimmed = line.trim_start();
            if trimmed.starts_with('#') {
                continue;
            }
            let Some(rest) = trimmed.strip_prefix(key) else {
                continue;
            };
            if !rest.trim_start().starts_with('=') {
                continue;
            }
            let col = line[..line.len() - trimmed.len()].chars().count() + 1;
            return Some((index + 1, col));
        }
        None
    }
}

/// 把 serde/TOML 层的错误转成带行/列的结构错误。
pub(crate) fn describe_toml_error(file: &Path, text: &str, error: &toml::de::Error) -> ConfigError {
    let source = Source::new(text);
    let (line, col) = error
        .span()
        .map(|span| source.line_col(span.start))
        .or_else(|| unknown_field_name(error.message()).and_then(|name| source.locate_any(&name)))
        .unwrap_or((1, 1));

    ConfigError {
        file: file.to_path_buf(),
        line,
        col,
        path: String::new(),
        message: error.message().to_owned(),
    }
}

fn unknown_field_name(message: &str) -> Option<String> {
    let marker = "unknown field `";
    let start = message.find(marker)? + marker.len();
    let rest = &message[start..];
    let end = rest.find('`')?;
    Some(rest[..end].to_owned())
}

/// 校验器：持有源文本、文件路径、配置目录与环境变量。
pub(crate) struct Validator<'a> {
    src: Source<'a>,
    file: &'a Path,
    config_dir: &'a Path,
    env: &'a dyn EnvVars,
}

impl<'a> Validator<'a> {
    pub(crate) fn new(
        text: &'a str,
        file: &'a Path,
        config_dir: &'a Path,
        env: &'a dyn EnvVars,
    ) -> Self {
        Self {
            src: Source::new(text),
            file,
            config_dir,
            env,
        }
    }

    /// 校验 `version` 与顶层表。
    pub(crate) fn document(&self, raw: &RawConfig) -> Result<Document, ConfigError> {
        let Some(version) = raw.version else {
            return Err(self.err(
                None,
                "version",
                "version",
                "缺少 version（当前 schema 版本为 2）",
            ));
        };
        if version != SCHEMA_VERSION {
            return Err(self.err(
                None,
                "version",
                "version",
                format!(
                    "不支持的配置版本 {version}（当前仅支持 2，请升级 {}）",
                    crate::brand::DISPLAY_NAME
                ),
            ));
        }

        // 2026-11-04：`start_minimized` 已废弃；保留 raw 字段以兼容旧配置，
        // 这里读取一次后忽略，避免 `deny_unknown_fields` 让旧文件加载失败。
        let _ = raw.app.start_minimized;
        let app = AppConfig {
            language: self.enum_field(
                raw.app.language.as_deref(),
                Language::ZhCn,
                None,
                "language",
                "app.language",
            )?,
            theme: self.enum_field(
                raw.app.theme.as_deref(),
                Theme::System,
                None,
                "theme",
                "app.theme",
            )?,
            confirm_exit: raw.app.confirm_exit.unwrap_or(true),
            data_dir: self.optional_path(
                raw.app.data_dir.as_deref(),
                None,
                "data_dir",
                "app.data_dir",
            )?,
        };

        let supervisor = SupervisorConfig {
            start_stagger_ms: self.int(
                raw.supervisor.start_stagger_ms,
                800,
                (0, i64::MAX),
                None,
                "start_stagger_ms",
                "supervisor.start_stagger_ms",
            )? as u64,
            shutdown_timeout_ms: self.int(
                raw.supervisor.shutdown_timeout_ms,
                8000,
                (0, i64::MAX),
                None,
                "shutdown_timeout_ms",
                "supervisor.shutdown_timeout_ms",
            )? as u64,
            config_poll_interval_ms: self.int(
                raw.supervisor.config_poll_interval_ms,
                1000,
                (0, i64::MAX),
                None,
                "config_poll_interval_ms",
                "supervisor.config_poll_interval_ms",
            )? as u64,
            watch_config: raw.supervisor.watch_config.unwrap_or(true),
            apply_changes: self.enum_field(
                raw.supervisor.apply_changes.as_deref(),
                ApplyChanges::Restart,
                None,
                "apply_changes",
                "supervisor.apply_changes",
            )?,
            resume_grace_ms: self.int(
                raw.supervisor.resume_grace_ms,
                20_000,
                (0, i64::MAX),
                None,
                "resume_grace_ms",
                "supervisor.resume_grace_ms",
            )? as u64,
        };

        let autostart = AutostartConfig {
            enabled: raw.autostart.enabled.unwrap_or(true),
            mode: self.enum_field(
                raw.autostart.mode.as_deref(),
                AutostartMode::RunKey,
                None,
                "mode",
                "autostart.mode",
            )?,
        };

        let notifications = NotificationsConfig {
            enabled: raw.notifications.enabled.unwrap_or(true),
            crash_loop: raw.notifications.crash_loop.unwrap_or(true),
            health_degraded: raw.notifications.health_degraded.unwrap_or(true),
            recovered: raw.notifications.recovered.unwrap_or(false),
        };

        let logs = LogsConfig {
            core_level: self.enum_field(
                raw.logs.core_level.as_deref(),
                CoreLevel::Info,
                None,
                "core_level",
                "logs.core_level",
            )?,
            retention_days: self.int(
                raw.logs.retention_days,
                7,
                (0, u32::MAX as i64),
                None,
                "retention_days",
                "logs.retention_days",
            )? as u32,
            max_total_mb: self.int(
                raw.logs.max_total_mb,
                500,
                (0, u32::MAX as i64),
                None,
                "max_total_mb",
                "logs.max_total_mb",
            )? as u32,
        };

        Ok(Document {
            version: version as u32,
            app,
            supervisor,
            autostart,
            notifications,
            logs,
        })
    }

    /// 预留字段警告：能解析、能校验，但当前版本没有任何消费点。
    ///
    /// 只在字段**显式出现**时告警——默认模板与最小配置里这些字段都是注释，
    /// 全部告警会让默认配置满屏 warning。任务级字段限定在对应 `[[tasks]]` 块内定位。
    pub(crate) fn reserved_issues(&self, raw: &RawConfig) -> Vec<ConfigIssue> {
        let mut issues = Vec::new();
        if raw.app.language.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                None,
                None,
                "language",
                "app.language",
                "app.language 为预留字段：当前界面仅中文，修改不会改变语言",
            ));
        }
        if raw.app.theme.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                None,
                None,
                "theme",
                "app.theme",
                "app.theme 为预留字段：界面跟随系统浅色，修改不生效",
            ));
        }
        if raw.notifications.crash_loop.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                None,
                None,
                "crash_loop",
                "notifications.crash_loop",
                "notifications.crash_loop 为预留字段：当前熔断通知固定发送，修改不生效",
            ));
        }
        if raw.notifications.health_degraded.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                None,
                None,
                "health_degraded",
                "notifications.health_degraded",
                "notifications.health_degraded 为预留字段：健康降级通知固定发送，修改不生效",
            ));
        }
        if raw.notifications.recovered.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                None,
                None,
                "recovered",
                "notifications.recovered",
                "notifications.recovered 为预留字段：恢复通知固定不发送，修改不生效",
            ));
        }
        for (index, task) in raw.tasks.iter().enumerate() {
            if task.elevated != Some(true) {
                continue;
            }
            let task_id = task
                .id
                .as_deref()
                .map(str::trim)
                .filter(|id| !id.is_empty());
            issues.push(self.issue(
                Severity::Warning,
                Some(index),
                task_id,
                "elevated",
                &format!("tasks[{index}].elevated"),
                "elevated 尚未实现：任务将以当前用户权限启动，不会提权",
            ));
        }
        issues
    }

    /// 校验并构建任务列表。结构错误直接返回，任务级问题追加到 issues。
    pub(crate) fn tasks(
        &self,
        raw: &RawConfig,
        data_dir: &Path,
        resume_grace_ms: u64,
    ) -> Result<(Vec<TaskConfig>, Vec<ConfigIssue>), ConfigError> {
        // id 是任务的身份，缺失或重复属于结构错误。
        let mut seen: BTreeMap<&str, usize> = BTreeMap::new();
        for (index, task) in raw.tasks.iter().enumerate() {
            let path = format!("tasks[{index}].id");
            let Some(id) = task.id.as_deref().map(str::trim) else {
                return Err(self.err(Some(index), "id", &path, "缺少 id（必填，不可变）"));
            };
            if id.is_empty() {
                return Err(self.err(Some(index), "id", &path, "id 不能为空"));
            }
            if let Some(previous) = seen.insert(id, index) {
                return Err(self.err(
                    Some(index),
                    "id",
                    &path,
                    format!("任务 id \"{id}\" 与第 {} 个任务重复", previous + 1),
                ));
            }
        }

        let mut issues = Vec::new();
        let mut tasks = Vec::with_capacity(raw.tasks.len());
        for (index, task) in raw.tasks.iter().enumerate() {
            tasks.push(self.build_task(task, index, data_dir, resume_grace_ms, &mut issues)?);
        }
        Ok((tasks, issues))
    }

    fn build_task(
        &self,
        raw: &RawTask,
        index: usize,
        data_dir: &Path,
        resume_grace_ms: u64,
        issues: &mut Vec<ConfigIssue>,
    ) -> Result<TaskConfig, ConfigError> {
        let block = Some(index);
        let id = raw
            .id
            .as_deref()
            .map(str::trim)
            .unwrap_or_default()
            .to_owned();
        let path = |suffix: &str| format!("tasks[{index}].{suffix}");
        let task_id = Some(id.as_str());

        if !is_valid_id(&id) {
            issues.push(self.issue(
                Severity::Error,
                block,
                task_id,
                "id",
                &path("id"),
                "id 只能使用小写字母、数字、- 和 _，以字母或数字开头，长度 1–64",
            ));
        }

        let name = match raw.name.as_deref().map(str::trim) {
            None | Some("") => {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "name",
                    &path("name"),
                    "缺少 name（必填，1–64 字符）",
                ));
                id.clone()
            }
            Some(name) => {
                if name.chars().count() > 64 {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "name",
                        &path("name"),
                        "name 过长（最多 64 字符）",
                    ));
                }
                name.to_owned()
            }
        };

        let command = match raw.command.as_deref() {
            None => {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "command",
                    &path("command"),
                    "缺少 command（必填）",
                ));
                String::new()
            }
            Some(raw_command) => match expand(raw_command, self.env) {
                Err(error) => {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "command",
                        &path("command"),
                        error.to_string(),
                    ));
                    raw_command.to_owned()
                }
                Ok(expanded) => {
                    if let Err(message) = self.check_command(&expanded) {
                        issues.push(self.issue(
                            Severity::Error,
                            block,
                            task_id,
                            "command",
                            &path("command"),
                            message,
                        ));
                    }
                    expanded
                }
            },
        };

        let working_dir = match raw.working_dir.as_deref() {
            None => {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "working_dir",
                    &path("working_dir"),
                    "缺少 working_dir（必填）",
                ));
                self.config_dir.to_path_buf()
            }
            Some(raw_dir) => match expand(raw_dir, self.env) {
                Err(error) => {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "working_dir",
                        &path("working_dir"),
                        error.to_string(),
                    ));
                    self.config_dir.to_path_buf()
                }
                Ok(expanded) => {
                    let dir = absolute_from(self.config_dir, expanded.trim());
                    if !dir.is_dir() {
                        issues.push(self.issue(
                            Severity::Error,
                            block,
                            task_id,
                            "working_dir",
                            &path("working_dir"),
                            format!("working_dir 不存在或不是目录：{}", dir.display()),
                        ));
                    }
                    dir
                }
            },
        };

        let args = match &raw.args {
            None => Vec::new(),
            Some(args) => args
                .iter()
                .filter_map(|arg| match expand(arg, self.env) {
                    Ok(expanded) => Some(expanded),
                    Err(error) => {
                        issues.push(self.issue(
                            Severity::Error,
                            block,
                            task_id,
                            "args",
                            &path("args"),
                            error.to_string(),
                        ));
                        None
                    }
                })
                .collect(),
        };

        let mut env: BTreeMap<String, String> = BTreeMap::new();
        if let Some(raw_env) = &raw.env {
            for (key, value) in raw_env {
                if key.to_ascii_uppercase().starts_with("CAMPUS_SENTRY_") {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "env",
                        &path("env"),
                        format!("环境变量 {key} 使用了核心保留前缀 CAMPUS_SENTRY_"),
                    ));
                    continue;
                }
                if env
                    .keys()
                    .any(|existing| existing.eq_ignore_ascii_case(key))
                {
                    continue;
                }
                match expand(value, self.env) {
                    Ok(expanded) => {
                        env.insert(key.clone(), expanded);
                    }
                    Err(error) => {
                        issues.push(self.issue(
                            Severity::Error,
                            block,
                            task_id,
                            "env",
                            &path("env"),
                            error.to_string(),
                        ));
                    }
                }
            }
        }

        let mut ports: Vec<u16> = Vec::new();
        if let Some(raw_ports) = &raw.ports {
            if raw_ports.len() > 32 {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "ports",
                    &path("ports"),
                    format!("ports 最多 32 个（当前 {} 个）", raw_ports.len()),
                ));
            }
            for value in raw_ports {
                if !(1..=65535).contains(value) {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "ports",
                        &path("ports"),
                        format!("端口 {value} 超出范围 1–65535"),
                    ));
                    continue;
                }
                let port = *value as u16;
                if !ports.contains(&port) {
                    ports.push(port);
                }
            }
        }

        let panel_url = match raw.panel_url.as_deref() {
            None => None,
            Some(raw_url) => match expand(raw_url, self.env) {
                Err(error) => {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "panel_url",
                        &path("panel_url"),
                        error.to_string(),
                    ));
                    None
                }
                Ok(expanded) => {
                    let trimmed = expanded.trim();
                    if trimmed.is_empty() {
                        None
                    } else {
                        match validate_url(trimmed) {
                            Ok(()) => Some(trimmed.to_owned()),
                            Err(reason) => {
                                issues.push(self.issue(
                                    Severity::Error,
                                    block,
                                    task_id,
                                    "panel_url",
                                    &path("panel_url"),
                                    format!("panel_url 非法：{reason}"),
                                ));
                                None
                            }
                        }
                    }
                }
            },
        };

        let env_proxy = self.enum_field(
            raw.env_proxy.as_deref(),
            EnvProxy::BypassLocalhost,
            block,
            "env_proxy",
            &path("env_proxy"),
        )?;

        let health = self.health(&raw.health, index, &id, data_dir, issues)?;
        let restart = self.restart(&raw.restart, index)?;
        let stop = self.stop(&raw.stop, index)?;
        let logs = self.task_logs(&raw.logs, index, &id, data_dir, issues)?;
        let power = self.power(&raw.power, index, resume_grace_ms)?;
        let limits = LimitsConfig {
            max_rss_mb: self.int(
                raw.limits.max_rss_mb,
                0,
                (0, 1_048_576),
                block,
                "max_rss_mb",
                &path("limits.max_rss_mb"),
            )? as u32,
            max_cpu_percent: self.int(
                raw.limits.max_cpu_percent,
                0,
                (0, 10_000),
                block,
                "max_cpu_percent",
                &path("limits.max_cpu_percent"),
            )? as u32,
        };
        let plugin = self.plugin_ref(raw, index, issues);
        let schedule = self.schedule(&raw.schedule, index, &id, issues, raw.auto_start);

        Ok(TaskConfig {
            id,
            name,
            enabled: raw.enabled.unwrap_or(true),
            // 缺省按文件顺序：同 order 的任务由快照的稳定排序保持文件顺序。
            order: raw.order.unwrap_or(index as i64),
            command,
            args,
            working_dir,
            auto_start: raw.auto_start.unwrap_or(false),
            elevated: raw.elevated.unwrap_or(false),
            notify: raw.notify.unwrap_or(true),
            env,
            ports,
            env_proxy,
            panel_url,
            plugin,
            schedule,
            health,
            restart,
            stop,
            logs,
            power,
            limits,
        })
    }

    fn health(
        &self,
        raw: &RawHealth,
        index: usize,
        id: &str,
        data_dir: &Path,
        issues: &mut Vec<ConfigIssue>,
    ) -> Result<HealthConfig, ConfigError> {
        let block = Some(index);
        let task_id = Some(id);
        let path = |suffix: &str| format!("tasks[{index}].health.{suffix}");

        let kind = self.enum_field(
            raw.kind.as_deref(),
            HealthKind::Process,
            block,
            "type",
            &path("type"),
        )?;
        let interval_ms = self.int(
            raw.interval_ms,
            10_000,
            (1000, 300_000),
            block,
            "interval_ms",
            &path("interval_ms"),
        )? as u64;
        let timeout_ms = self.int(
            raw.timeout_ms,
            2000,
            (100, 30_000),
            block,
            "timeout_ms",
            &path("timeout_ms"),
        )? as u64;
        let failure_threshold = self.int(
            raw.failure_threshold,
            3,
            (1, 10),
            block,
            "failure_threshold",
            &path("failure_threshold"),
        )? as u32;
        let startup_grace_ms = self.int(
            raw.startup_grace_ms,
            20_000,
            (0, i64::MAX),
            block,
            "startup_grace_ms",
            &path("startup_grace_ms"),
        )? as u64;

        let mut expect_status: Vec<u16> = Vec::new();
        for value in raw.expect_status.iter().flatten() {
            if !(100..=599).contains(value) {
                return Err(self.err(
                    block,
                    "expect_status",
                    &path("expect_status"),
                    format!("HTTP 状态码 {value} 超出范围 100–599"),
                ));
            }
            if expect_status.len() >= 32 {
                return Err(self.err(
                    block,
                    "expect_status",
                    &path("expect_status"),
                    "expect_status 最多 32 个",
                ));
            }
            let code = *value as u16;
            if !expect_status.contains(&code) {
                expect_status.push(code);
            }
        }
        if expect_status.is_empty() {
            expect_status.push(200);
        }

        let expect_body_contains = raw
            .expect_body_contains
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned);
        if let Some(body) = &expect_body_contains
            && body.chars().count() > 256
        {
            return Err(self.err(
                block,
                "expect_body_contains",
                &path("expect_body_contains"),
                "expect_body_contains 最多 256 字符",
            ));
        }

        let mut needs_human_status: Vec<String> = Vec::new();
        for status in raw.needs_human_status.iter().flatten() {
            let status = status.trim();
            if status.is_empty() {
                return Err(self.err(
                    block,
                    "needs_human_status",
                    &path("needs_human_status"),
                    "needs_human_status 不能包含空字符串",
                ));
            }
            if needs_human_status.len() >= 16 {
                return Err(self.err(
                    block,
                    "needs_human_status",
                    &path("needs_human_status"),
                    "needs_human_status 最多 16 个",
                ));
            }
            if !needs_human_status.iter().any(|item| item == status) {
                needs_human_status.push(status.to_owned());
            }
        }

        let max_age_ms = match raw.max_age_ms {
            None => None,
            Some(value) => Some(self.int(
                Some(value),
                0,
                (1000, i64::MAX),
                block,
                "max_age_ms",
                &path("max_age_ms"),
            )? as u64),
        };

        let mut url = None;
        let mut heartbeat_path = None;
        match kind {
            HealthKind::Http => match raw.url.as_deref() {
                None => issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "url",
                    &path("url"),
                    "http 健康探测缺少 url",
                )),
                Some(raw_url) => match expand(raw_url, self.env) {
                    Err(error) => issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "url",
                        &path("url"),
                        error.to_string(),
                    )),
                    Ok(expanded) => {
                        if expanded.to_ascii_lowercase().starts_with("http://") {
                            url = Some(expanded);
                        } else {
                            issues.push(self.issue(
                                Severity::Error,
                                block,
                                task_id,
                                "url",
                                &path("url"),
                                "健康探测 url 必须以 http:// 开头（v1 不支持 https）",
                            ));
                        }
                    }
                },
            },
            HealthKind::Heartbeat => {
                if raw.max_age_ms.is_none() {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "max_age_ms",
                        &path("max_age_ms"),
                        "heartbeat 健康探测缺少 max_age_ms（无法判断心跳新鲜度）",
                    ));
                }
                let default_path = data_dir.join("heartbeats").join(format!("{id}.hb"));
                heartbeat_path = Some(match raw.path.as_deref() {
                    None => default_path,
                    Some(raw_path) => match expand(raw_path, self.env) {
                        Ok(expanded) => absolute_from(self.config_dir, expanded.trim()),
                        Err(error) => {
                            issues.push(self.issue(
                                Severity::Error,
                                block,
                                task_id,
                                "path",
                                &path("path"),
                                error.to_string(),
                            ));
                            default_path
                        }
                    },
                });
            }
            HealthKind::Process => {
                issues.push(self.issue(
                    Severity::Warning,
                    block,
                    task_id,
                    "type",
                    &path("type"),
                    "health.type = process：只做进程级监督（粗粒度）",
                ));
            }
        }

        if !needs_human_status.is_empty() && kind != HealthKind::Heartbeat {
            issues.push(self.issue(
                Severity::Warning,
                block,
                task_id,
                "needs_human_status",
                &path("needs_human_status"),
                format!(
                    "needs_human_status 仅在 heartbeat 模式生效，当前 type = \"{}\"，已忽略",
                    match kind {
                        HealthKind::Http => "http",
                        HealthKind::Heartbeat => "heartbeat",
                        HealthKind::Process => "process",
                    }
                ),
            ));
        }
        if expect_body_contains.is_some() && kind != HealthKind::Http {
            issues.push(self.issue(
                Severity::Warning,
                block,
                task_id,
                "expect_body_contains",
                &path("expect_body_contains"),
                "expect_body_contains 仅在 http 模式生效，已忽略",
            ));
        }

        Ok(HealthConfig {
            kind,
            interval_ms,
            timeout_ms,
            failure_threshold,
            startup_grace_ms,
            url,
            expect_status,
            expect_body_contains,
            path: heartbeat_path,
            max_age_ms,
            needs_human_status,
        })
    }

    fn restart(&self, raw: &RawRestart, index: usize) -> Result<RestartConfig, ConfigError> {
        let block = Some(index);
        let path = |suffix: &str| format!("tasks[{index}].restart.{suffix}");

        let mode = self.enum_field(
            raw.mode.as_deref(),
            RestartMode::OnFailure,
            block,
            "mode",
            &path("mode"),
        )?;
        let initial_delay_ms = self.int(
            raw.initial_delay_ms,
            3000,
            (0, 600_000),
            block,
            "initial_delay_ms",
            &path("initial_delay_ms"),
        )? as u64;
        let max_delay_ms = self.int(
            raw.max_delay_ms,
            60_000,
            (0, 600_000),
            block,
            "max_delay_ms",
            &path("max_delay_ms"),
        )? as u64;
        if max_delay_ms < initial_delay_ms {
            return Err(self.err(
                block,
                "max_delay_ms",
                &path("max_delay_ms"),
                format!(
                    "max_delay_ms（{max_delay_ms}）不能小于 initial_delay_ms（{initial_delay_ms}）"
                ),
            ));
        }
        let multiplier = self.float_field(
            raw.multiplier,
            2.0,
            (1.0, 10.0),
            block,
            "multiplier",
            &path("multiplier"),
        )?;
        let jitter = self.float_field(
            raw.jitter,
            0.2,
            (0.0, 0.5),
            block,
            "jitter",
            &path("jitter"),
        )?;
        let max_consecutive_failures = self.int(
            raw.max_consecutive_failures,
            5,
            (0, 1000),
            block,
            "max_consecutive_failures",
            &path("max_consecutive_failures"),
        )? as u32;
        let stable_after_ms = self.int(
            raw.stable_after_ms,
            60_000,
            (0, i64::MAX),
            block,
            "stable_after_ms",
            &path("stable_after_ms"),
        )? as u64;
        let window_ms = self.int(
            raw.window_ms,
            600_000,
            (0, 86_400_000),
            block,
            "window_ms",
            &path("window_ms"),
        )? as u64;
        let max_starts_in_window = self.int(
            raw.max_starts_in_window,
            6,
            (0, 1000),
            block,
            "max_starts_in_window",
            &path("max_starts_in_window"),
        )? as u32;

        let mut pause_on_exit_codes: Vec<u32> = Vec::new();
        for value in raw.pause_on_exit_codes.iter().flatten() {
            if !(0..=255).contains(value) {
                return Err(self.err(
                    block,
                    "pause_on_exit_codes",
                    &path("pause_on_exit_codes"),
                    format!("退出码 {value} 超出范围 0–255"),
                ));
            }
            if pause_on_exit_codes.len() >= 32 {
                return Err(self.err(
                    block,
                    "pause_on_exit_codes",
                    &path("pause_on_exit_codes"),
                    "pause_on_exit_codes 最多 32 个",
                ));
            }
            let code = *value as u32;
            if !pause_on_exit_codes.contains(&code) {
                pause_on_exit_codes.push(code);
            }
        }

        Ok(RestartConfig {
            mode,
            initial_delay_ms,
            max_delay_ms,
            multiplier,
            jitter,
            max_consecutive_failures,
            stable_after_ms,
            window_ms,
            max_starts_in_window,
            pause_on_exit_codes,
        })
    }

    fn stop(&self, raw: &RawStop, index: usize) -> Result<StopConfig, ConfigError> {
        let block = Some(index);
        let path = |suffix: &str| format!("tasks[{index}].stop.{suffix}");

        Ok(StopConfig {
            method: self.enum_field(
                raw.method.as_deref(),
                StopMethod::Graceful,
                block,
                "method",
                &path("method"),
            )?,
            grace_ms: self.int(
                raw.grace_ms,
                2500,
                (100, 60_000),
                block,
                "grace_ms",
                &path("grace_ms"),
            )? as u64,
            timeout_ms: self.int(
                raw.timeout_ms,
                8000,
                (100, 60_000),
                block,
                "timeout_ms",
                &path("timeout_ms"),
            )? as u64,
        })
    }

    fn task_logs(
        &self,
        raw: &RawTaskLogs,
        index: usize,
        id: &str,
        data_dir: &Path,
        issues: &mut Vec<ConfigIssue>,
    ) -> Result<TaskLogsConfig, ConfigError> {
        let block = Some(index);
        let task_id = Some(id);
        let path = |suffix: &str| format!("tasks[{index}].logs.{suffix}");

        let rotation = self.enum_field(
            raw.rotation.as_deref(),
            Rotation::Size,
            block,
            "rotation",
            &path("rotation"),
        )?;
        let max_file_mb = self.int(
            raw.max_file_mb,
            10,
            (1, 1024),
            block,
            "max_file_mb",
            &path("max_file_mb"),
        )? as u32;
        let max_files = self.int(
            raw.max_files,
            5,
            (1, 50),
            block,
            "max_files",
            &path("max_files"),
        )? as u32;
        let max_days = self.int(
            raw.max_days,
            7,
            (1, 365),
            block,
            "max_days",
            &path("max_days"),
        )? as u32;
        let encoding = self.enum_field(
            raw.encoding.as_deref(),
            Encoding::Auto,
            block,
            "encoding",
            &path("encoding"),
        )?;

        let default_dir = data_dir.join("logs").join(id);
        let dir = match raw.dir.as_deref().map(str::trim) {
            None | Some("") => default_dir,
            Some(raw_dir) => match expand(raw_dir, self.env) {
                Ok(expanded) => absolute_from(self.config_dir, expanded.trim()),
                Err(error) => {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        task_id,
                        "dir",
                        &path("dir"),
                        error.to_string(),
                    ));
                    default_dir
                }
            },
        };

        Ok(TaskLogsConfig {
            dir,
            rotation,
            max_file_mb,
            max_files,
            max_days,
            encoding,
        })
    }

    fn power(
        &self,
        raw: &RawPower,
        index: usize,
        resume_grace_ms: u64,
    ) -> Result<PowerConfig, ConfigError> {
        Ok(PowerConfig {
            resume_grace_ms: self.int(
                raw.resume_grace_ms,
                resume_grace_ms as i64,
                (0, 300_000),
                Some(index),
                "resume_grace_ms",
                &format!("tasks[{index}].power.resume_grace_ms"),
            )? as u64,
        })
    }

    /// 插件清单导入时写入的来源字段：三者要么都不写，要么 id + manifest 都有效。
    fn plugin_ref(
        &self,
        raw: &RawTask,
        index: usize,
        issues: &mut Vec<ConfigIssue>,
    ) -> Option<PluginRef> {
        let any = raw.plugin_id.is_some()
            || raw.plugin_manifest.is_some()
            || raw.plugin_version.is_some();
        if !any {
            return None;
        }
        let block = Some(index);
        let base = format!("tasks[{index}]");
        let id = raw
            .plugin_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty());
        let Some(id) = id else {
            issues.push(
                self.issue(
                    Severity::Error,
                    block,
                    Some(
                        raw.id
                            .as_deref()
                            .map(str::trim)
                            .filter(|id| !id.is_empty())
                            .unwrap_or(""),
                    ),
                    "plugin_id",
                    &format!("{base}.plugin_id"),
                    "写了 plugin_manifest/plugin_version 但缺少 plugin_id",
                ),
            );
            return None;
        };
        if !is_valid_id(id) {
            issues.push(self.issue(
                Severity::Error,
                block,
                Some(id),
                "plugin_id",
                &format!("{base}.plugin_id"),
                "plugin_id 只能使用小写字母、数字、- 和 _，以字母或数字开头，长度 1–64",
            ));
        }
        let task_id = raw.id.as_deref().map(str::trim).unwrap_or("");
        if id != task_id {
            issues.push(self.issue(
                Severity::Error,
                block,
                Some(task_id),
                "plugin_id",
                &format!("{base}.plugin_id"),
                format!("plugin_id（{id}）必须与任务 id（{task_id}）一致"),
            ));
        }
        let manifest = match raw.plugin_manifest.as_deref() {
            None => {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    Some(id),
                    "plugin_manifest",
                    &format!("{base}.plugin_manifest"),
                    "缺少 plugin_manifest（插件任务的清单绝对路径）",
                ));
                return None;
            }
            Some(raw_path) => match expand(raw_path, self.env) {
                Ok(expanded) => {
                    let path = absolute_from(self.config_dir, expanded.trim());
                    if !path.is_file() {
                        issues.push(self.issue(
                            Severity::Error,
                            block,
                            Some(id),
                            "plugin_manifest",
                            &format!("{base}.plugin_manifest"),
                            format!("插件清单不存在：{}", path.display()),
                        ));
                    }
                    path
                }
                Err(error) => {
                    issues.push(self.issue(
                        Severity::Error,
                        block,
                        Some(id),
                        "plugin_manifest",
                        &format!("{base}.plugin_manifest"),
                        error.to_string(),
                    ));
                    return None;
                }
            },
        };
        let version = raw
            .plugin_version
            .as_deref()
            .map(str::trim)
            .filter(|version| !version.is_empty())
            .map(str::to_owned);
        Some(PluginRef {
            id: id.to_owned(),
            manifest,
            version,
        })
    }

    /// 校验每周运行时段；坏时段只让任务 invalid，不拖垮整份配置。
    fn schedule(
        &self,
        raw: &RawSchedule,
        index: usize,
        id: &str,
        issues: &mut Vec<ConfigIssue>,
        task_auto_start: Option<bool>,
    ) -> ScheduleConfig {
        let block = Some(index);
        let task_id = Some(id);
        let base = format!("tasks[{index}].schedule");
        let enabled = raw.enabled.unwrap_or(false);
        let mut windows = Vec::new();
        for (window_index, window) in raw.windows.iter().enumerate() {
            let window_base = format!("{base}.windows[{window_index}]");
            let mut days = Vec::new();
            match window.days.as_deref() {
                None | Some([]) => issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "days",
                    &format!("{window_base}.days"),
                    "缺少 days（至少一个 mon..sun）",
                )),
                Some(raw_days) => {
                    for raw_day in raw_days {
                        match Weekday::parse(raw_day) {
                            Some(day) => {
                                if !days.contains(&day) {
                                    days.push(day);
                                }
                            }
                            None => issues.push(self.issue(
                                Severity::Error,
                                block,
                                task_id,
                                "days",
                                &format!("{window_base}.days"),
                                format!(
                                    "非法星期 \"{raw_day}\"（只接受 mon/tue/wed/thu/fri/sat/sun）"
                                ),
                            )),
                        }
                    }
                    days.sort_by_key(|day| {
                        Weekday::ALL
                            .iter()
                            .position(|candidate| candidate == day)
                            .unwrap_or(usize::MAX)
                    });
                }
            }

            let start = window.start.as_deref().and_then(parse_time);
            let end = window.end.as_deref().and_then(parse_time);
            if start.is_none() {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "start",
                    &format!("{window_base}.start"),
                    "缺少或非法的 start（HH:MM）",
                ));
            }
            if end.is_none() {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "end",
                    &format!("{window_base}.end"),
                    "缺少或非法的 end（HH:MM）",
                ));
            }
            let (Some(start), Some(end)) = (start, end) else {
                continue;
            };
            if start == end {
                issues.push(self.issue(
                    Severity::Error,
                    block,
                    task_id,
                    "end",
                    &format!("{window_base}.end"),
                    "start 与 end 不能相同（如需全天请拆成两段）",
                ));
                continue;
            }
            if days.is_empty() {
                continue;
            }
            windows.push(Window { days, start, end });
        }

        if enabled && windows.is_empty() {
            issues.push(self.issue(
                Severity::Error,
                block,
                task_id,
                "windows",
                &format!("{base}.windows"),
                "schedule.enabled=true 但没有可用的运行时段",
            ));
        }
        if enabled && task_auto_start == Some(true) {
            issues.push(self.issue(
                Severity::Warning,
                block,
                task_id,
                "auto_start",
                &format!("tasks[{index}].auto_start"),
                "启用运行时段后，任务级 auto_start 由时段接管；建议设为 false 避免开机即启动",
            ));
        }
        // 2026-11-05 用户架构修正（一个时钟、一个开关）：start_in_window /
        // stop_outside 只保留兼容解析，引擎完全忽略；出现即警告，但绝不让配置
        // invalid。v2（2026-11-15）：字段已从 `ScheduleConfig` 删除，这里只保留
        // warning，原值不再进入模型。
        if raw.start_in_window.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                block,
                task_id,
                "start_in_window",
                &format!("{base}.start_in_window"),
                "start_in_window 已废弃（2026-11-05）：进入窗口固定自动启动，字段被忽略；可安全删除",
            ));
        }
        if raw.stop_outside.is_some() {
            issues.push(self.issue(
                Severity::Warning,
                block,
                task_id,
                "stop_outside",
                &format!("{base}.stop_outside"),
                "stop_outside 已废弃（2026-11-05）：离开窗口固定停止任何运行实例，字段被忽略；可安全删除",
            ));
        }
        ScheduleConfig { enabled, windows }
    }

    fn err(
        &self,
        block: Option<usize>,
        key: &str,
        path: &str,
        message: impl Into<String>,
    ) -> ConfigError {
        let (line, col) = self.src.locate(key, block).unwrap_or((1, 1));
        ConfigError {
            file: self.file.to_path_buf(),
            line,
            col,
            path: path.to_owned(),
            message: message.into(),
        }
    }

    fn issue(
        &self,
        severity: Severity,
        block: Option<usize>,
        task_id: Option<&str>,
        key: &str,
        path: &str,
        message: impl Into<String>,
    ) -> ConfigIssue {
        let location = self.src.locate(key, block);
        ConfigIssue {
            severity,
            task_id: task_id.map(str::to_owned),
            path: path.to_owned(),
            message: message.into(),
            line: location.map(|(line, _)| line),
            col: location.map(|(_, col)| col),
        }
    }

    fn int(
        &self,
        raw: Option<i64>,
        default: i64,
        range: (i64, i64),
        block: Option<usize>,
        key: &str,
        path: &str,
    ) -> Result<i64, ConfigError> {
        let value = raw.unwrap_or(default);
        if value < range.0 || value > range.1 {
            let message = if range.1 == i64::MAX {
                format!("取值 {value} 不能小于 {}", range.0)
            } else {
                format!("取值 {value} 超出范围 {}–{}", range.0, range.1)
            };
            return Err(self.err(block, key, path, message));
        }
        Ok(value)
    }

    fn float_field(
        &self,
        raw: Option<f64>,
        default: f64,
        range: (f64, f64),
        block: Option<usize>,
        key: &str,
        path: &str,
    ) -> Result<f64, ConfigError> {
        let value = raw.unwrap_or(default);
        if !(range.0..=range.1).contains(&value) {
            return Err(self.err(
                block,
                key,
                path,
                format!("取值 {value} 超出范围 {}–{}", range.0, range.1),
            ));
        }
        Ok(value)
    }

    fn enum_field<T: StrEnum>(
        &self,
        raw: Option<&str>,
        default: T,
        block: Option<usize>,
        key: &str,
        path: &str,
    ) -> Result<T, ConfigError> {
        match raw {
            None => Ok(default),
            Some(text) => T::parse(text).ok_or_else(|| {
                self.err(
                    block,
                    key,
                    path,
                    format!("未知取值 \"{text}\"（期望 {}）", T::VARIANTS.join(" | ")),
                )
            }),
        }
    }

    fn optional_path(
        &self,
        raw: Option<&str>,
        block: Option<usize>,
        key: &str,
        path: &str,
    ) -> Result<Option<PathBuf>, ConfigError> {
        let Some(raw) = raw else {
            return Ok(None);
        };
        let expanded =
            expand(raw, self.env).map_err(|error| self.err(block, key, path, error.to_string()))?;
        let trimmed = expanded.trim();
        if trimmed.is_empty() {
            return Ok(None);
        }
        Ok(Some(absolute_from(self.config_dir, trimmed)))
    }

    /// 命令可执行性检查：绝对/相对路径必须存在；裸名称走 PATH（含 PATHEXT）。
    fn check_command(&self, command: &str) -> Result<(), String> {
        let trimmed = command.trim();
        if trimmed.is_empty() {
            return Err("command 为空".to_owned());
        }
        if trimmed.contains('"') {
            return Err("command 不能包含引号；请把参数放进 args".to_owned());
        }
        if looks_like_path(trimmed) {
            let path = absolute_from(self.config_dir, trimmed);
            if path.is_file() {
                return Ok(());
            }
            let mut message = format!("找不到可执行文件：{}", path.display());
            if trimmed.contains(' ') {
                message.push_str("；command 只写可执行文件，参数请放进 args");
            }
            return Err(message);
        }
        if self.find_in_path(trimmed).is_some() {
            return Ok(());
        }
        let mut message = format!("在 PATH 中找不到命令 \"{trimmed}\"");
        if trimmed.contains(' ') {
            message.push_str("；command 只写可执行文件，参数请放进 args");
        }
        Err(message)
    }

    fn find_in_path(&self, name: &str) -> Option<PathBuf> {
        let path_var = self.env.var("PATH")?;
        let extensions = self.executable_extensions();
        for dir in path_var.split(';') {
            let dir = dir.trim().trim_matches('"');
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
            for extension in &extensions {
                let with_extension = Path::new(dir).join(format!("{name}{extension}"));
                if with_extension.is_file() {
                    return Some(with_extension);
                }
            }
        }
        None
    }

    fn executable_extensions(&self) -> Vec<String> {
        let raw = self
            .env
            .var("PATHEXT")
            .unwrap_or_else(|| ".COM;.EXE;.BAT;.CMD".to_owned());
        raw.split(';')
            .map(str::trim)
            .filter(|extension| !extension.is_empty())
            .map(str::to_ascii_lowercase)
            .collect()
    }
}

fn absolute_from(base: &Path, value: &str) -> PathBuf {
    let path = Path::new(value);
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        base.join(path)
    }
}

fn looks_like_path(command: &str) -> bool {
    command.contains('/') || command.contains('\\') || has_drive_prefix(command)
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

fn is_valid_id(id: &str) -> bool {
    let bytes = id.as_bytes();
    if bytes.is_empty() || bytes.len() > 64 {
        return false;
    }
    if !(bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit()) {
        return false;
    }
    bytes[1..].iter().all(|byte| {
        byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-' || *byte == b'_'
    })
}

fn validate_url(url: &str) -> Result<(), String> {
    let lower = url.to_ascii_lowercase();
    let rest = lower
        .strip_prefix("http://")
        .or_else(|| lower.strip_prefix("https://"))
        .ok_or_else(|| "必须以 http:// 或 https:// 开头".to_owned())?;
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() {
        return Err("缺少主机名".to_owned());
    }
    if authority.contains(' ') || authority.contains('\\') {
        return Err("主机名包含非法字符".to_owned());
    }
    Ok(())
}
