//! 配置子系统的单元测试：不依赖 Tauri/GUI，也不碰真实用户目录。

use std::fs;
use std::path::{Path, PathBuf};

use super::*;

/// 最小可用配置：只有 version，没有任何任务。
const MINIMAL: &str = "version = 1\n";

/// 临时目录 + 假 PATH + 假 APPDATA/LOCALAPPDATA 的测试夹具。
struct Fx {
    _tmp: tempfile::TempDir,
    root: PathBuf,
    bin: PathBuf,
    env: MapEnv,
}

impl Fx {
    fn new() -> Self {
        let tmp = tempfile::tempdir().expect("创建临时目录");
        let root = tmp.path().to_path_buf();
        let bin = root.join("bin");
        fs::create_dir_all(&bin).expect("创建 bin");
        let env = MapEnv::new()
            .with("PATH", bin.to_string_lossy().to_string())
            .with("PATHEXT", ".COM;.EXE;.BAT;.CMD")
            .with(
                "APPDATA",
                root.join("roaming").to_string_lossy().to_string(),
            )
            .with(
                "LOCALAPPDATA",
                root.join("local").to_string_lossy().to_string(),
            )
            .with("CAMPUS_TEST_ROOT", root.to_string_lossy().to_string());
        Self {
            _tmp: tmp,
            root,
            bin,
            env,
        }
    }

    /// 在假 PATH 里放一个空的可执行文件。
    fn exe(&self, name: &str) {
        fs::write(self.bin.join(name), b"").expect("写假可执行文件");
    }

    fn dir(&self, relative: &str) -> PathBuf {
        let path = self.root.join(relative);
        fs::create_dir_all(&path).expect("创建目录");
        path
    }

    /// TOML 里用正斜杠，避免反斜杠被当成转义。
    fn toml_path(&self, path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    fn write_config(&self, source: &str) -> PathBuf {
        let path = self.root.join("Rook.toml");
        fs::write(&path, source).expect("写配置文件");
        path
    }

    fn load(&self, source: &str) -> Result<LoadedConfig, LoadError> {
        let path = self.write_config(source);
        self.load_path(&path)
    }

    fn load_path(&self, path: &Path) -> Result<LoadedConfig, LoadError> {
        let local = self.root.join("local");
        load_file(&LoadContext {
            config_path: path,
            data_dir_override: None,
            local_appdata: Some(&local),
            env: &self.env,
        })
    }

    fn load_with_data_dir(&self, source: &str, data_dir: &Path) -> Result<LoadedConfig, LoadError> {
        let path = self.write_config(source);
        let local = self.root.join("local");
        load_file(&LoadContext {
            config_path: &path,
            data_dir_override: Some(data_dir),
            local_appdata: Some(&local),
            env: &self.env,
        })
    }
}

fn structural(error: LoadError) -> ConfigError {
    match error {
        LoadError::Invalid(error) => error,
        other => panic!("期望结构错误，实际是：{other:?}"),
    }
}

fn task_error<'a>(loaded: &'a LoadedConfig, task_id: &str) -> &'a ConfigIssue {
    loaded
        .errors()
        .find(|issue| issue.task_id.as_deref() == Some(task_id))
        .unwrap_or_else(|| panic!("任务 {task_id} 没有任务级错误：{:?}", loaded.issues))
}

/// 按字段路径取问题（不区分严重级别）。
fn issue_with_path<'a>(loaded: &'a LoadedConfig, path: &str) -> &'a ConfigIssue {
    loaded
        .issues
        .iter()
        .find(|issue| issue.path == path)
        .unwrap_or_else(|| panic!("没有问题 path = {path}：{:?}", loaded.issues))
}

/// 单个任务的最小 TOML 片段（id 固定为 "t"）。
fn one_task(command: &str, working_dir: &str) -> String {
    format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "测试任务"
command = "{command}"
working_dir = "{working_dir}"
"#
    )
}

fn full_example(fx: &Fx) -> String {
    let ykt = fx.toml_path(&fx.dir("ykt"));
    let zhs = fx.toml_path(&fx.dir("zhs"));
    let net = fx.toml_path(&fx.dir("net"));
    format!(
        r#"
version = 1

[app]
start_minimized = true

[autostart]
enabled = true
mode = "run_key"

[[tasks]]
id = "ykt-gateway"
name = "雨课堂网关"
command = "node"
args = ["server/index.js"]
working_dir = "{ykt}"
auto_start = true
env = {{ PORT = "3300" }}

  [tasks.health]
  type = "http"
  url = "http://127.0.0.1:3300/api/sites"
  interval_ms = 10000
  timeout_ms = 2000
  failure_threshold = 3
  startup_grace_ms = 20000
  expect_body_contains = '{{"ok":true'
  needs_human_status = ["login_required"]

  [tasks.restart]
  max_consecutive_failures = 5

  [tasks.logs]
  max_file_mb = 10
  max_files = 5

[[tasks]]
id = "zhs-gateway"
name = "智慧树签到网关"
command = "node"
args = ["server/index.js"]
working_dir = "{zhs}"
auto_start = true
env = {{ PORT = "3400" }}

  [tasks.health]
  type = "http"
  url = "http://127.0.0.1:3400/api/status"
  interval_ms = 15000
  failure_threshold = 3
  startup_grace_ms = 20000
  expect_body_contains = '{{"ok":true'

  [tasks.logs]
  max_file_mb = 10
  max_files = 5

[[tasks]]
id = "campus-net"
name = "校园网自动重连"
command = "python"
args = ["reconnect.py"]
working_dir = "{net}"
auto_start = true
elevated = true
env_proxy = "strip"

  [tasks.health]
  type = "heartbeat"
  path = "%LOCALAPPDATA%/Rook/heartbeats/campus-net.hb"
  interval_ms = 5000
  timeout_ms = 1000
  max_age_ms = 30000
  needs_human_status = ["login_required", "blocked"]

  [tasks.restart]
  mode = "on-failure"
  initial_delay_ms = 5000
  max_delay_ms = 300000
  max_consecutive_failures = 10
  stable_after_ms = 300000
  pause_on_exit_codes = [2]

  [tasks.power]
  resume_grace_ms = 60000

  [tasks.logs]
  rotation = "daily"
  max_days = 14
"#
    )
}

#[test]
fn minimal_config_uses_documented_defaults() {
    let fx = Fx::new();
    let loaded = fx.load(MINIMAL).expect("最小配置应可加载");

    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    assert_eq!(loaded.config.version, 2);
    assert_eq!(loaded.config.app.language, Language::ZhCn);
    assert_eq!(loaded.config.app.theme, Theme::System);
    assert!(loaded.config.app.confirm_exit);
    assert_eq!(loaded.config.app.data_dir, None);

    assert_eq!(loaded.config.supervisor.start_stagger_ms, 800);
    assert_eq!(loaded.config.supervisor.shutdown_timeout_ms, 8000);
    assert_eq!(loaded.config.supervisor.config_poll_interval_ms, 1000);
    assert!(loaded.config.supervisor.watch_config);
    assert_eq!(
        loaded.config.supervisor.apply_changes,
        ApplyChanges::Restart
    );
    assert_eq!(loaded.config.supervisor.resume_grace_ms, 20_000);

    assert!(loaded.config.autostart.enabled);
    assert_eq!(loaded.config.autostart.mode, AutostartMode::RunKey);
    assert!(loaded.config.notifications.enabled);
    assert!(!loaded.config.notifications.recovered);
    assert_eq!(loaded.config.logs.core_level, CoreLevel::Info);
    assert_eq!(loaded.config.logs.retention_days, 7);
    assert_eq!(loaded.config.logs.max_total_mb, 500);

    assert!(loaded.config.tasks.is_empty());
    assert_eq!(loaded.data_dir, fx.root.join("local").join("Rook"));
    assert!(loaded.data_dir.is_dir());
    assert_eq!(loaded.config_path, fx.root.join("Rook.toml"));
}

#[test]
fn watch_config_can_be_disabled_and_revision_matches_file_bytes() {
    let fx = Fx::new();
    let source = "version = 1\n\n[supervisor]\nwatch_config = false\n";
    let loaded = fx.load(source).expect("配置应可加载");
    assert!(!loaded.config.supervisor.watch_config);
    assert_eq!(
        loaded.revision,
        crate::config::revision_of(source.as_bytes())
    );
}

#[test]
fn stop_defaults_to_graceful_with_documented_grace() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let work = fx.toml_path(&fx.dir("work"));
    let source = format!(
        "{}\n  [tasks.stop]\n  timeout_ms = 5000\n",
        one_task("node", &work)
    );

    let loaded = fx.load(&source).expect("配置应可加载");
    assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
    let task = loaded.config.task("t").expect("任务 t");
    assert_eq!(
        task.stop.method,
        StopMethod::Graceful,
        "默认必须是 graceful"
    );
    assert_eq!(task.stop.grace_ms, 2500);
    assert_eq!(task.stop.timeout_ms, 5000);
}

#[test]
fn stop_method_accepts_both_values_and_rejects_unknown() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let work = fx.toml_path(&fx.dir("work"));

    for (text, expected) in [
        ("graceful", StopMethod::Graceful),
        ("terminate", StopMethod::Terminate),
    ] {
        let source = format!(
            "{}\n  [tasks.stop]\n  method = \"{text}\"\n",
            one_task("node", &work)
        );
        let loaded = fx.load(&source).expect("合法 method 应可加载");
        assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
        assert_eq!(loaded.config.task("t").unwrap().stop.method, expected);
    }

    let source = format!(
        "{}\n  [tasks.stop]\n  method = \"console_ctrl_break\"\n",
        one_task("node", &work)
    );
    let error = structural(fx.load(&source).expect_err("非法 method 必须报错"));
    assert_eq!(error.path, "tasks[0].stop.method");
    assert!(
        error.message.contains("graceful") && error.message.contains("terminate"),
        "错误应列出合法取值：{}",
        error.message
    );
}

#[test]
fn stop_grace_ms_boundaries_are_structural() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let work = fx.toml_path(&fx.dir("work"));

    for value in [100, 2500, 60_000] {
        let source = format!(
            "{}\n  [tasks.stop]\n  grace_ms = {value}\n",
            one_task("node", &work)
        );
        let loaded = fx.load(&source).expect("边界内 grace_ms 应可加载");
        assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
        assert_eq!(loaded.config.task("t").unwrap().stop.grace_ms, value);
    }

    for value in [99, 60_001] {
        let source = format!(
            "{}\n  [tasks.stop]\n  grace_ms = {value}\n",
            one_task("node", &work)
        );
        let error = structural(fx.load(&source).expect_err("越界 grace_ms 必须报错"));
        assert_eq!(error.path, "tasks[0].stop.grace_ms");
    }
}

#[test]
fn limits_default_to_zero_meaning_unlimited() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let work = fx.toml_path(&fx.dir("work"));
    let loaded = fx.load(&one_task("node", &work)).expect("最小任务应可加载");

    let task = loaded.config.task("t").expect("任务应存在");
    assert_eq!(task.limits.max_rss_mb, 0);
    assert_eq!(task.limits.max_cpu_percent, 0);
    assert_eq!(task.limits, LimitsConfig::default());
}

#[test]
fn limits_boundaries_are_structural() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let work = fx.toml_path(&fx.dir("work"));

    // 0 = 不限制，与上边界都合法。
    for (rss, cpu) in [(0, 0), (512, 250), (1_048_576, 10_000)] {
        let source = format!(
            "{}\n  [tasks.limits]\n  max_rss_mb = {rss}\n  max_cpu_percent = {cpu}\n",
            one_task("node", &work)
        );
        let loaded = fx.load(&source).expect("边界内 limits 应可加载");
        assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
        let task = loaded.config.task("t").expect("任务应存在");
        assert_eq!(task.limits.max_rss_mb, rss);
        assert_eq!(task.limits.max_cpu_percent, cpu);
    }

    for (key, value) in [
        ("max_rss_mb", -1),
        ("max_rss_mb", 1_048_577),
        ("max_cpu_percent", -1),
        ("max_cpu_percent", 10_001),
    ] {
        let source = format!(
            "{}\n  [tasks.limits]\n  {key} = {value}\n",
            one_task("node", &work)
        );
        let error = structural(fx.load(&source).expect_err("越界 limits 必须报错"));
        assert_eq!(error.path, format!("tasks[0].limits.{key}"));
    }
}

#[test]
fn full_example_with_three_tasks_loads() {
    let fx = Fx::new();
    fx.exe("node.exe");
    fx.exe("python.exe");
    let source = full_example(&fx);

    let loaded = fx.load(&source).expect("完整示例应可加载");
    assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
    // http 任务上配 needs_human_status 只给警告（docs 示例本身如此）；
    // campus-net 写了 elevated = true，B5 起也会多一条预留字段警告。
    assert_eq!(loaded.warnings().count(), 2, "{:?}", loaded.issues);

    let ids: Vec<&str> = loaded
        .config
        .tasks
        .iter()
        .map(|task| task.id.as_str())
        .collect();
    assert_eq!(ids, ["ykt-gateway", "zhs-gateway", "campus-net"]);

    let ykt = loaded.config.task("ykt-gateway").expect("ykt 任务");
    assert_eq!(ykt.health.kind, HealthKind::Http);
    assert_eq!(
        ykt.health.url.as_deref(),
        Some("http://127.0.0.1:3300/api/sites")
    );
    assert_eq!(ykt.health.expect_status, vec![200]);
    assert_eq!(ykt.health.needs_human_status, vec!["login_required"]);
    assert_eq!(ykt.restart.max_consecutive_failures, 5);
    // C1 默认：10 分钟内最多 6 次启动（旧配置缺字段时同样生效）。
    assert_eq!(ykt.restart.window_ms, 600_000);
    assert_eq!(ykt.restart.max_starts_in_window, 6);
    assert_eq!(ykt.restart.mode, RestartMode::OnFailure);
    assert_eq!(ykt.logs.max_files, 5);
    assert_eq!(ykt.logs.rotation, Rotation::Size);
    // 未显式配置 max_days：默认保留 7 天。
    assert_eq!(ykt.logs.max_days, 7);
    assert_eq!(ykt.env.get("PORT").map(String::as_str), Some("3300"));
    assert_eq!(ykt.env_proxy, EnvProxy::BypassLocalhost);
    assert!(ykt.auto_start);
    assert_eq!(ykt.health.timeout_ms, 2000);

    let net = loaded.config.task("campus-net").expect("campus-net 任务");
    assert_eq!(net.health.kind, HealthKind::Heartbeat);
    assert_eq!(net.health.max_age_ms, Some(30_000));
    assert_eq!(net.restart.pause_on_exit_codes, vec![2]);
    assert_eq!(net.power.resume_grace_ms, 60_000);
    assert_eq!(net.logs.rotation, Rotation::Daily);
    assert_eq!(net.logs.max_days, 14);
    assert_eq!(net.env_proxy, EnvProxy::Strip);
    assert!(net.elevated);
    let expected_heartbeat = fx
        .root
        .join("local")
        .join("Rook")
        .join("heartbeats")
        .join("campus-net.hb");
    assert_eq!(
        net.health.path.as_deref(),
        Some(expected_heartbeat.as_path())
    );
    // 默认日志目录：<data>/logs/<id>
    assert_eq!(
        net.logs.dir,
        loaded.data_dir.join("logs").join("campus-net")
    );
}

#[test]
fn duplicate_task_id_is_structural() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "dup"
name = "第一个"
command = "node"
working_dir = "{dir}"

[[tasks]]
id = "dup"
name = "第二个"
command = "node"
working_dir = "{dir}"
"#
    );

    let error = structural(fx.load(&source).unwrap_err());
    assert!(error.message.contains("重复"), "{}", error.message);
    assert!(error.line > 1, "{error:?}");
    assert!(error.path.contains("tasks[1]"), "{}", error.path);
}

#[test]
fn unknown_field_is_structural() {
    let fx = Fx::new();
    let source = "version = 1\n\n[app]\nlangauge = \"zh-CN\"\n";
    let error = structural(fx.load(source).unwrap_err());
    assert!(error.message.contains("unknown field"), "{}", error.message);
    assert_eq!(error.line, 4, "{error:?}");
    assert_eq!(error.col, 1, "{error:?}");
}

#[test]
fn unsupported_version_is_structural() {
    let fx = Fx::new();
    let error = structural(fx.load("version = 3\n").unwrap_err());
    assert!(
        error.message.contains("不支持的配置版本 3"),
        "{}",
        error.message
    );
    assert_eq!(error.line, 1);

    let error = structural(fx.load("").unwrap_err());
    assert!(error.message.contains("缺少 version"), "{}", error.message);
}

#[test]
fn v1_config_migrates_to_v2_with_backup() {
    let fx = Fx::new();
    fx.exe("node");
    let source = "version = 1\n\n[[tasks]]\nid = \"alpha\"\nname = \"甲\"\ncommand = \"node\"\n";
    let path = fx.write_config(source);
    let loaded = fx.load_path(&path).expect("v1 应迁移后加载");
    assert_eq!(loaded.config.version, 2);
    assert_eq!(loaded.config.task("alpha").expect("任务").id, "alpha");

    let backup = fx.root.join("Rook.toml.v1.bak");
    assert!(backup.exists(), "应生成 .v1.bak");
    assert_eq!(fs::read_to_string(&backup).expect("读备份"), source);
}

#[test]
fn syntax_error_reports_line_and_column() {
    let fx = Fx::new();
    let source = "version = 1\n\n[app]\nstart_minimized = \n";
    let error = structural(fx.load(source).unwrap_err());
    assert_eq!(error.line, 4, "{error:?}");
    assert!(error.col >= 1, "{error:?}");
    assert!(!error.message.is_empty());
    assert_eq!(error.file, fx.root.join("Rook.toml"));
}

#[test]
fn type_error_is_structural() {
    let fx = Fx::new();
    let source = "version = 1\n\n[supervisor]\nstart_stagger_ms = \"800\"\n";
    let error = structural(fx.load(source).unwrap_err());
    assert!(error.message.contains("invalid type"), "{}", error.message);
    assert_eq!(error.line, 4, "{error:?}");
}

#[test]
fn invalid_health_type_is_structural() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.health]
  type = "htttp"
"#
    );

    let error = structural(fx.load(&source).unwrap_err());
    assert!(error.message.contains("htttp"), "{}", error.message);
    assert!(
        error.message.contains("http | heartbeat | process"),
        "{}",
        error.message
    );
    assert!(error.line > 1, "{error:?}");
}

#[test]
fn restart_window_defaults_match_docs() {
    // C1：旧配置缺字段时的默认值（10 分钟 / 6 次），与 docs/02 §4.3 一致。
    let defaults = RestartConfig::default();
    assert_eq!(defaults.window_ms, 600_000);
    assert_eq!(defaults.max_starts_in_window, 6);
}

#[test]
fn restart_relationship_violation_is_structural() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.restart]
  initial_delay_ms = 10000
  max_delay_ms = 1000
"#
    );

    let error = structural(fx.load(&source).unwrap_err());
    assert!(error.message.contains("max_delay_ms"), "{}", error.message);
    assert_eq!(error.line, 12, "{error:?}");
}

#[test]
fn restart_ranges_are_structural() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let build = |restart: &str| {
        format!(
            r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.restart]
{restart}
"#
        )
    };

    let error = structural(fx.load(&build("  multiplier = 12.0")).unwrap_err());
    assert!(error.message.contains("超出范围 1–10"), "{}", error.message);

    let error = structural(fx.load(&build("  jitter = 0.7")).unwrap_err());
    assert!(
        error.message.contains("超出范围 0–0.5"),
        "{}",
        error.message
    );

    // 整数写法的 multiplier 也接受（2 == 2.0）。
    let loaded = fx
        .load(&build("  multiplier = 2"))
        .expect("整数 multiplier");
    assert_eq!(loaded.config.tasks[0].restart.multiplier, 2.0);

    // C1：窗口字段 0 合法（= 不限）。
    let loaded = fx
        .load(&build("  window_ms = 0\n  max_starts_in_window = 0"))
        .expect("0 = 不限");
    assert_eq!(loaded.config.tasks[0].restart.window_ms, 0);
    assert_eq!(loaded.config.tasks[0].restart.max_starts_in_window, 0);

    let error = structural(fx.load(&build("  window_ms = 86400001")).unwrap_err());
    assert!(
        error.message.contains("超出范围 0–86400000"),
        "{}",
        error.message
    );
    let error = structural(
        fx.load(&build("  max_starts_in_window = 1001"))
            .unwrap_err(),
    );
    assert!(
        error.message.contains("超出范围 0–1000"),
        "{}",
        error.message
    );

    // 缺省时默认 600000 / 6。
    let loaded = fx.load(&build("  jitter = 0.2")).expect("默认窗口");
    assert_eq!(loaded.config.tasks[0].restart.window_ms, 600_000);
    assert_eq!(loaded.config.tasks[0].restart.max_starts_in_window, 6);
}

#[test]
fn missing_command_is_task_error() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let good_dir = fx.toml_path(&fx.dir("good"));
    let bad_dir = fx.toml_path(&fx.dir("bad"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "good"
name = "正常"
command = "node"
working_dir = "{good_dir}"

[[tasks]]
id = "bad"
name = "坏命令"
command = "definitely-not-installed-xyz"
working_dir = "{bad_dir}"
"#
    );

    let loaded = fx.load(&source).expect("任务级错误不应影响整份配置");
    assert_eq!(loaded.config.tasks.len(), 2);
    assert!(loaded.is_task_valid("good"));
    assert!(!loaded.is_task_valid("bad"));

    let issue = task_error(&loaded, "bad");
    assert_eq!(issue.severity, Severity::Error);
    assert!(issue.message.contains("PATH"), "{}", issue.message);
    assert!(issue.line.is_some());
    assert_eq!(issue.path, "tasks[1].command");
}

#[test]
fn command_with_arguments_is_task_error() {
    let fx = Fx::new();
    let dir = fx.toml_path(&fx.dir("wd"));
    let loaded = fx
        .load(&one_task("node server/index.js", &dir))
        .expect("任务级错误");

    let issue = task_error(&loaded, "t");
    assert!(issue.message.contains("args"), "{}", issue.message);
}

#[test]
fn missing_working_dir_is_task_error() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let missing = fx.toml_path(&fx.root.join("does-not-exist"));
    let loaded = fx.load(&one_task("node", &missing)).expect("任务级错误");

    let issue = task_error(&loaded, "t");
    assert!(
        issue.message.contains("working_dir 不存在"),
        "{}",
        issue.message
    );
    assert_eq!(issue.path, "tasks[0].working_dir");
}

#[test]
fn reserved_env_prefix_is_task_error() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"
env = {{ CAMPUS_SENTRY_TASK_ID = "x", PORT = "1" }}
"#
    );

    let loaded = fx.load(&source).expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(
        issue.message.contains("CAMPUS_SENTRY_"),
        "{}",
        issue.message
    );
    // 合法变量仍然保留。
    assert_eq!(
        loaded.config.tasks[0].env.get("PORT").map(String::as_str),
        Some("1")
    );
}

#[test]
fn ports_out_of_range_is_task_error() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"
ports = [70000, 3300, 3300]
"#
    );

    let loaded = fx.load(&source).expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(issue.message.contains("70000"), "{}", issue.message);
    // 越界的被剔除，重复的被去重。
    assert_eq!(loaded.config.tasks[0].ports, vec![3300]);
}

#[test]
fn invalid_panel_url_is_task_error() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"
panel_url = "ftp://127.0.0.1/"
"#
    );

    let loaded = fx.load(&source).expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(
        issue.message.contains("panel_url 非法"),
        "{}",
        issue.message
    );
    assert_eq!(loaded.config.tasks[0].panel_url, None);

    let source = source.replace("ftp://127.0.0.1/", "https://127.0.0.1:8080/");
    let loaded = fx.load(&source).expect("合法 panel_url");
    assert!(loaded.errors().next().is_none(), "{:?}", loaded.issues);
    assert_eq!(
        loaded.config.tasks[0].panel_url.as_deref(),
        Some("https://127.0.0.1:8080/")
    );
}

#[test]
fn needs_human_status_outside_heartbeat_is_warning() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.health]
  type = "process"
  needs_human_status = ["login_required"]
"#
    );

    let loaded = fx.load(&source).expect("警告不应阻断加载");
    assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
    assert!(loaded.is_task_valid("t"));
    assert!(
        loaded
            .warnings()
            .any(|issue| issue.path.ends_with("needs_human_status")),
        "{:?}",
        loaded.issues
    );
    assert!(
        loaded
            .warnings()
            .any(|issue| issue.message.contains("process")),
        "{:?}",
        loaded.issues
    );
}

/// 2026-11-05：start_in_window / stop_outside 已废弃——出现只发 Warning、
/// 绝不让配置 invalid；v2（2026-11-15）起不再进入 `ScheduleConfig`，仅保留
/// 兼容解析 + warning。
#[test]
fn deprecated_schedule_flags_warn_but_do_not_invalidate() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.schedule]
  enabled = true
  start_in_window = false
  stop_outside = false

  [[tasks.schedule.windows]]
  days = ["mon"]
  start = "08:00"
  end = "09:00"
"#
    );

    let loaded = fx.load(&source).expect("废弃字段不应阻断加载");
    assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
    assert!(loaded.is_task_valid("t"));
    assert_eq!(
        loaded
            .warnings()
            .filter(|issue| issue.path.ends_with("start_in_window"))
            .count(),
        1,
        "{:?}",
        loaded.issues
    );
    assert_eq!(
        loaded
            .warnings()
            .filter(|issue| issue.path.ends_with("stop_outside"))
            .count(),
        1,
        "{:?}",
        loaded.issues
    );
    assert!(
        loaded
            .warnings()
            .all(|issue| issue.severity == Severity::Warning),
    );
    let schedule = &loaded.config.tasks[0].schedule;
    assert!(schedule.enabled);
    assert_eq!(schedule.windows.len(), 1, "窗口仍照常解析");
}

/// B5：预留字段只在**显式出现**时发 warning——配置照常加载、任务仍 valid。
#[test]
fn reserved_fields_warn_without_invalidating_config() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[app]
language = "zh-CN"
theme = "dark"

[notifications]
crash_loop = false
health_degraded = false
recovered = true

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"
elevated = true
"#
    );

    let loaded = fx.load(&source).expect("预留字段不应阻断加载");
    assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);
    assert!(loaded.is_task_valid("t"), "{:?}", loaded.issues);
    assert_eq!(loaded.config.app.theme, Theme::Dark, "字段仍照常解析");

    for path in [
        "app.language",
        "app.theme",
        "notifications.crash_loop",
        "notifications.health_degraded",
        "notifications.recovered",
    ] {
        let issue = issue_with_path(&loaded, path);
        assert_eq!(issue.severity, Severity::Warning, "{path}");
        assert!(issue.task_id.is_none(), "{path}: {:?}", issue.task_id);
        assert!(
            issue.message.contains("预留字段"),
            "{path}: {}",
            issue.message
        );
    }

    let elevated = issue_with_path(&loaded, "tasks[0].elevated");
    assert_eq!(elevated.severity, Severity::Warning);
    assert_eq!(elevated.task_id.as_deref(), Some("t"));
    assert!(elevated.message.contains("未实现"), "{}", elevated.message);
}

/// 未显式设置（或显式设为 false）时不应产生 reserved warning，避免默认噪音。
#[test]
fn reserved_fields_absent_or_false_produce_no_warning() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[app]
confirm_exit = false

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"
elevated = false
"#
    );

    let loaded = fx.load(&source).expect("合法配置");
    for path in [
        "app.language",
        "app.theme",
        "notifications.crash_loop",
        "notifications.health_degraded",
        "notifications.recovered",
        "tasks[0].elevated",
    ] {
        assert!(
            !loaded.issues.iter().any(|issue| issue.path == path),
            "{path}: {:?}",
            loaded.issues
        );
    }
}

/// 定位到字段所在行/列（不是 1:1 兜底）。
#[test]
fn reserved_issue_locates_field() {
    let fx = Fx::new();
    let loaded = fx
        .load("version = 1\n\n[app]\ntheme = \"dark\"\n")
        .expect("合法配置");
    let theme = issue_with_path(&loaded, "app.theme");
    assert_eq!((theme.line, theme.col), (Some(4), Some(1)));
}

#[test]
fn http_health_requires_http_url() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let build = |health: &str| {
        format!(
            r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.health]
{health}
"#
        )
    };

    let loaded = fx.load(&build("  type = \"http\"")).expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(issue.message.contains("缺少 url"), "{}", issue.message);

    let loaded = fx
        .load(&build(
            "  type = \"http\"\n  url = \"https://127.0.0.1/api\"",
        ))
        .expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(issue.message.contains("http://"), "{}", issue.message);
}

#[test]
fn heartbeat_requires_max_age() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.health]
  type = "heartbeat"
"#
    );

    let loaded = fx.load(&source).expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(issue.message.contains("max_age_ms"), "{}", issue.message);
}

#[test]
fn expands_env_vars_and_rejects_unknown() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let wd = fx.dir("expanded");

    let loaded = fx
        .load(&one_task("node", "%CAMPUS_TEST_ROOT%/expanded"))
        .expect("应可展开");
    assert_eq!(loaded.config.tasks[0].working_dir, wd);

    let loaded = fx
        .load(&one_task("node", "%NO_SUCH_VAR_XYZ%/expanded"))
        .expect("任务级错误");
    let issue = task_error(&loaded, "t");
    assert!(
        issue.message.contains("未知环境变量 %NO_SUCH_VAR_XYZ%"),
        "{}",
        issue.message
    );
}

#[test]
fn relative_paths_resolve_against_config_dir() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let wd = fx.dir("sub/wd");
    let source = r#"
version = 1

[app]
data_dir = "data"

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "sub/wd"

  [tasks.health]
  type = "heartbeat"
  path = "hb/t.hb"
  max_age_ms = 30000

  [tasks.logs]
  dir = "mylogs"
"#;

    let loaded = fx.load(source).expect("相对路径应可解析");
    assert_eq!(loaded.errors().count(), 0, "{:?}", loaded.issues);

    let task = &loaded.config.tasks[0];
    assert_eq!(task.working_dir, wd);
    assert_eq!(loaded.data_dir, fx.root.join("data"));
    assert_eq!(task.health.path, Some(fx.root.join("hb").join("t.hb")));
    assert_eq!(task.logs.dir, fx.root.join("mylogs"));
}

#[test]
fn app_data_dir_and_logs_default_paths() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let wd = fx.toml_path(&fx.dir("wd"));
    let source = format!(
        r#"
version = 1

[app]
data_dir = "mydata"

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{wd}"
"#
    );

    let loaded = fx.load(&source).expect("应可加载");
    assert_eq!(loaded.data_dir, fx.root.join("mydata"));
    assert_eq!(
        loaded.config.tasks[0].logs.dir,
        fx.root.join("mydata").join("logs").join("t")
    );

    // --data-dir 优先于 [app].data_dir
    let override_dir = fx.root.join("override");
    let loaded = fx
        .load_with_data_dir(&source, &override_dir)
        .expect("应可加载");
    assert_eq!(loaded.data_dir, override_dir);
    assert_eq!(
        loaded.config.tasks[0].logs.dir,
        override_dir.join("logs").join("t")
    );
}

#[test]
fn default_template_is_valid_and_never_overwritten() {
    let fx = Fx::new();
    let path = fx.root.join("cfg").join("config.toml");

    assert!(write_default_config(&path).expect("首次写入"));
    let first = fs::read_to_string(&path).expect("读取模板");
    assert_eq!(first, default_config_template());
    assert!(first.contains("version = 2"));
    assert!(first.contains("[[tasks]]"));

    // 第二次不覆盖，内容保持不变。
    assert!(!write_default_config(&path).expect("再次写入"));
    assert_eq!(fs::read_to_string(&path).expect("读取模板"), first);

    // 模板本身是合法配置，且不含任务。
    let loaded = fx.load_path(&path).expect("模板应可加载");
    assert!(loaded.issues.is_empty(), "{:?}", loaded.issues);
    assert!(loaded.config.tasks.is_empty());
}

#[test]
fn config_path_priority() {
    let fx = Fx::new();
    let exe_dir = fx.dir("exe");
    let appdata = fx.dir("roaming");
    let explicit = fx.root.join("explicit.toml");
    fs::write(&explicit, MINIMAL).expect("写显式配置");

    // 1) --config 最高，即使便携文件存在也用它。
    let portable = exe_dir.join(PORTABLE_CONFIG_FILE);
    fs::write(&portable, MINIMAL).expect("写便携配置");
    assert_eq!(
        resolve_config_path(Some(&explicit), &exe_dir, Some(&appdata)).expect("显式路径"),
        explicit
    );

    // 2) <exe_dir>/Rook.toml
    assert_eq!(
        resolve_config_path(None, &exe_dir, Some(&appdata)).expect("便携路径"),
        portable
    );

    // 3) %APPDATA%/Rook/config.toml，缺则创建默认模板
    fs::remove_file(&portable).expect("删除便携配置");
    let resolved = resolve_config_path(None, &exe_dir, Some(&appdata)).expect("安装版路径");
    assert_eq!(
        resolved,
        appdata.join(APP_DIR_NAME).join(INSTALLED_CONFIG_FILE)
    );
    assert!(resolved.is_file());
    assert_eq!(
        fs::read_to_string(&resolved).expect("读取默认配置"),
        default_config_template()
    );

    // 4) 没有 APPDATA 也没有便携文件 → 明确报错
    assert!(resolve_config_path(None, &exe_dir, None).is_err());
}

#[test]
fn data_dir_priority() {
    let fx = Fx::new();
    let local = fx.dir("local");
    let explicit = fx.root.join("d1");
    let configured = fx.root.join("d2");

    assert_eq!(
        resolve_data_dir(Some(&explicit), Some(&configured), Some(&local)).expect("显式优先"),
        explicit
    );
    assert_eq!(
        resolve_data_dir(None, Some(&configured), Some(&local)).expect("配置优先"),
        configured
    );
    assert_eq!(
        resolve_data_dir(None, None, Some(&local)).expect("回退 LOCALAPPDATA"),
        local.join(APP_DIR_NAME)
    );
    assert!(resolve_data_dir(None, None, None).is_err());
}

#[test]
fn expect_status_accepts_single_int_and_list() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let dir = fx.toml_path(&fx.dir("wd"));
    let build = |status: &str| {
        format!(
            r#"
version = 1

[[tasks]]
id = "t"
name = "T"
command = "node"
working_dir = "{dir}"

  [tasks.health]
  type = "http"
  url = "http://127.0.0.1:3300/api"
  expect_status = {status}
"#
        )
    };

    let loaded = fx.load(&build("204")).expect("单个整数");
    assert_eq!(loaded.config.tasks[0].health.expect_status, vec![204]);

    let loaded = fx.load(&build("[200, 204, 204]")).expect("整数数组");
    assert_eq!(loaded.config.tasks[0].health.expect_status, vec![200, 204]);

    let error = structural(fx.load(&build("700")).unwrap_err());
    assert!(error.message.contains("100–599"), "{}", error.message);
}

#[test]
fn load_uses_process_environment_and_overrides() {
    let fx = Fx::new();
    fx.exe("node.exe");
    let wd = fx.dir("wd");
    let config = fx.root.join("explicit.toml");
    let source = one_task(&fx.toml_path(&fx.bin.join("node.exe")), &fx.toml_path(&wd));
    fs::write(&config, source).expect("写配置");

    let data = fx.root.join("data");
    let loaded = load(&Overrides {
        config: Some(&config),
        data_dir: Some(&data),
    })
    .expect("按进程环境加载");

    assert_eq!(loaded.config_path, config);
    assert_eq!(loaded.data_dir, data);
    assert!(loaded.errors().next().is_none(), "{:?}", loaded.issues);
    assert_eq!(loaded.config.tasks.len(), 1);
}

#[test]
fn missing_file_is_io_error() {
    let fx = Fx::new();
    let missing = fx.root.join("nope.toml");
    match fx.load_path(&missing) {
        Err(LoadError::Io { path, .. }) => assert_eq!(path, missing),
        other => panic!("期望 I/O 错误，实际：{other:?}"),
    }
}
