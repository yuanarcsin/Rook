//! M2-4a 测试：真实 `cmd.exe` 进程 + 临时日志目录。
//!
//! 所有等待都有超时；长进程用 `ping -n 30 127.0.0.1`（约 29 秒），
//! 测试结束前一定会 stop 或由运行时 Drop 回收进程树。

use std::collections::{BTreeMap, HashSet};
use std::ffi::OsString;
use std::fs;
use std::net::TcpListener;
use std::path::Path;
use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use super::coordinator::{
    ReloadGate, UserAction, apply_runtime_snapshot, apply_user_intent, auto_start_allowed,
    manual_restart_allowed, manual_start_allowed, prune_removed_tasks,
};
use super::state::{
    ExitReason, RuntimeError, RuntimeEvent, RuntimeSnapshot, RuntimeStage, TaskStatus,
};
use super::supervision::{
    AutoStartInput, alive_status_for_rebuild, build_health_monitor, count_restart_success,
    ensure_heartbeat_dir, instance_matches, machine_tick_due, plan_auto_start, probe_due,
    readiness_deadline_reached, readiness_first_healthy, readiness_plan, reconcile_action,
    replay_alive_machine, restore_seed, running_instance_accepted, should_log_stop_retry,
    starting_instance_accepted, stop_retry_delay, sync_state,
};
use super::task::{InstanceContext, StartReason, TaskRuntime, TaskSpec};
use crate::config::{
    Encoding, EnvProxy, HealthConfig, HealthKind, PowerConfig, RestartConfig, Rotation, StopConfig,
    TaskConfig, TaskLogsConfig,
};
use crate::health::HealthState;
use crate::logs::{LogParams, LogStats};
use crate::restart::{
    Decision, Intent, NeedsHuman, RandomSource, Reason, RestartEvent, RestartMachine,
    RestartPolicy, RestartStatus,
};
use crate::state_store::{
    DesiredOverride, LastStatus, NeedsHumanState, PausedReason, PersistedState, TaskState,
    Timestamp,
};

const WAIT: Duration = Duration::from_secs(10);
const LONG_ARGS: &[&str] = &["/C", "ping", "-n", "30", "127.0.0.1"];

fn logs_params(dir: &Path) -> LogParams {
    let mut params = LogParams::new(dir);
    params.poll_interval = Duration::from_millis(5);
    params.stop_timeout = Duration::from_secs(5);
    params
}

/// M3-4b：固定随机数（0.5 → 抖动因子 1），与 restart 测试一致。
#[derive(Clone)]
struct FixedRandom(f64);

impl RandomSource for FixedRandom {
    fn next_unit(&mut self) -> f64 {
        self.0
    }
}

fn supervision_machine(policy: RestartPolicy) -> RestartMachine<FixedRandom> {
    RestartMachine::with_random(policy, FixedRandom(0.5))
}

/// M3-4b 测试用最小任务配置（其余取默认值）。
fn supervision_task(id: &str) -> TaskConfig {
    TaskConfig {
        id: id.to_owned(),
        name: format!("任务 {id}"),
        enabled: true,
        order: 0,
        command: "cmd.exe".to_owned(),
        args: Vec::new(),
        working_dir: std::path::PathBuf::from("C:/work"),
        auto_start: true,
        elevated: false,
        notify: true,
        env: BTreeMap::new(),
        ports: Vec::new(),
        env_proxy: EnvProxy::Inherit,
        panel_url: None,
        plugin: None,
        schedule: crate::config::ScheduleConfig::default(),
        health: HealthConfig::default(),
        restart: RestartConfig::default(),
        stop: StopConfig::default(),
        logs: TaskLogsConfig {
            dir: std::path::PathBuf::from("C:/logs"),
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

fn spec(id: &str, dir: &Path, args: &[&str]) -> TaskSpec {
    let mut task = TaskSpec::new(id, "cmd.exe", logs_params(dir));
    task.args = args.iter().map(|arg| (*arg).to_owned()).collect();
    task
}

fn wait_for(what: &str, mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if predicate() {
            return;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert!(predicate(), "等待超时：{what}");
}

fn wait_status(runtime: &TaskRuntime, expected: TaskStatus) {
    wait_for(&format!("状态变为 {}", expected.as_str()), || {
        runtime.status() == expected
    });
}

fn read_log(path: &Path) -> Vec<u8> {
    fs::read(path).unwrap_or_else(|error| panic!("读取日志 {} 失败：{error}", path.display()))
}

/// 等日志文件出现指定字节（运行中读取；写入端允许共享读）。
fn wait_log(path: &Path, what: &str, mut predicate: impl FnMut(&[u8]) -> bool) {
    wait_for(&format!("日志满足 {what}"), || {
        fs::read(path).is_ok_and(|bytes| predicate(&bytes))
    });
}

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

fn count(haystack: &[u8], needle: &[u8]) -> usize {
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

/// 解析 `cmd /C set > dump` 的输出：键统一大写，保留原值。
fn parse_env_dump(path: &Path) -> BTreeMap<String, String> {
    let bytes = fs::read(path)
        .unwrap_or_else(|error| panic!("读取环境 dump {} 失败：{error}", path.display()));
    let text = String::from_utf8_lossy(&bytes);
    let mut vars = BTreeMap::new();
    for line in text.lines() {
        if let Some((key, value)) = line.split_once('=')
            && !key.is_empty()
            && !key.starts_with('=')
        {
            vars.insert(key.to_ascii_uppercase(), value.to_owned());
        }
    }
    vars
}

/// 等文件至少出现 `expected` 行，返回全部行（用于逐次启动追加写）。
fn wait_lines(path: &Path, expected: usize) -> Vec<String> {
    wait_for(
        &format!("{} 至少有 {expected} 行", path.display()),
        || {
            fs::read_to_string(path)
                .map(|text| text.lines().count() >= expected)
                .unwrap_or(false)
        },
    );
    fs::read_to_string(path)
        .expect("读取追加输出文件")
        .lines()
        // cmd 的 `echo value >> file` 会在重定向前保留一个空格，测试里去掉行尾空白。
        .map(|line| line.trim_end().to_owned())
        .collect()
}

// ---------------------------------------------------------------- 线程安全

#[test]
fn runtime_types_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}

    assert_send_sync::<TaskRuntime>();
    assert_send_sync::<super::state::RuntimeSnapshot>();
    assert_send_sync::<super::state::RuntimeEvent>();
    assert_send_sync::<super::state::RuntimeError>();
}

// ---------------------------------------------------------------- 自然退出

#[test]
fn cmd_exit_zero_finishes_stopped_with_log() {
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(spec("exit-zero", dir.path(), &["/C", "echo", "hello"]));

    runtime.start().expect("启动 cmd.exe");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.status, TaskStatus::Stopped);
    assert_eq!(snapshot.exit_code, Some(0));
    assert_eq!(snapshot.exit_reason, Some(ExitReason::Natural));
    assert_eq!(snapshot.pid, None);
    assert!(
        snapshot.last_error.is_none(),
        "自然退出不应有 last_error：{:?}",
        snapshot.last_error
    );

    let log = read_log(&snapshot.log_path);
    assert!(
        contains(&log, b"hello"),
        "日志应包含 hello，实际：{}",
        String::from_utf8_lossy(&log)
    );
    assert!(
        snapshot.logs.written_bytes > 0,
        "日志应已落盘：{:?}",
        snapshot.logs
    );
}

#[test]
fn cmd_exit_seven_records_code_and_log() {
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(spec(
        "exit-seven",
        dir.path(),
        &["/C", "echo", "bye", "&", "exit", "7"],
    ));

    runtime.start().expect("启动 cmd.exe");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.status, TaskStatus::Stopped);
    assert_eq!(snapshot.exit_code, Some(7));
    assert_eq!(snapshot.exit_reason, Some(ExitReason::Natural));
    assert_eq!(snapshot.pid, None);
    assert!(snapshot.last_error.is_none());

    let log = read_log(&snapshot.log_path);
    assert!(
        contains(&log, b"bye"),
        "日志应包含 bye，实际：{}",
        String::from_utf8_lossy(&log)
    );
}

// ---------------------------------------------------------------- 长进程/停止

#[test]
fn long_process_runs_then_stops_and_logs_flush() {
    let dir = tempfile::tempdir().expect("临时目录");
    let args = ["/C", "echo", "alpha", "&", "ping", "-n", "30", "127.0.0.1"];
    let runtime = TaskRuntime::new(spec("long-stop", dir.path(), &args));

    runtime.start().expect("启动长进程");
    wait_status(&runtime, TaskStatus::Running);
    let running = runtime.snapshot();
    assert!(running.pid.is_some_and(|pid| pid > 0), "运行中应有 PID");
    assert!(running.instance_id.is_some(), "运行中应有实例 id");
    assert!(running.started_at.is_some(), "运行中应有启动时间");
    // `start` 返回时进程可能还没写出第一行；先等日志出现，再停止。
    wait_log(&running.log_path, "alpha", |bytes| {
        contains(bytes, b"alpha")
    });

    let stopped = runtime.stop(Duration::from_secs(10)).expect("停止长进程");
    assert_eq!(stopped.status, TaskStatus::Stopped);
    assert_eq!(stopped.pid, None);
    assert_eq!(stopped.exit_reason, Some(ExitReason::UserStop));
    // 默认 graceful：cmd 收到 CTRL_BREAK 后退出；成功判据是根退出/树空，
    // 不看退出码（控制台默认处理程序可能给出 0xC000013A）。
    assert!(stopped.exit_code.is_some(), "停止后应记录退出码");
    assert!(stopped.last_error.is_none());
    assert!(
        stopped.logs.written_bytes > 0,
        "停止后日志统计应为最终值：{:?}",
        stopped.logs
    );

    let log = read_log(&stopped.log_path);
    assert!(
        contains(&log, b"alpha"),
        "停止前写入的日志不应丢失，实际：{}",
        String::from_utf8_lossy(&log)
    );

    // 幂等：再次 stop 立即返回同一结果。
    let again = runtime.stop(Duration::from_secs(2)).expect("重复停止");
    assert_eq!(again.status, TaskStatus::Stopped);
    assert_eq!(again.exit_code, stopped.exit_code);
    assert_eq!(again.exit_reason, Some(ExitReason::UserStop));
}

#[test]
fn restart_changes_instance_id_and_keeps_process_and_logs() {
    let dir = tempfile::tempdir().expect("临时目录");
    let args = ["/C", "echo", "run", "&", "ping", "-n", "30", "127.0.0.1"];
    let runtime = TaskRuntime::new(spec("restart", dir.path(), &args));

    runtime.start().expect("首次启动");
    wait_status(&runtime, TaskStatus::Running);
    let first = runtime.snapshot();
    assert!(first.pid.is_some_and(|pid| pid > 0));
    wait_log(&first.log_path, "第一次 run", |bytes| {
        count(bytes, b"run") >= 1
    });

    let restarted = runtime.restart(Duration::from_secs(10)).expect("重启");
    assert_ne!(
        restarted.instance_id, first.instance_id,
        "restart 必须分配新的实例 id"
    );

    wait_status(&runtime, TaskStatus::Running);
    let second = runtime.snapshot();
    assert_ne!(second.instance_id, first.instance_id);
    assert!(
        second.pid.is_some_and(|pid| pid > 0),
        "重启后应重新拿到 PID"
    );
    wait_log(&second.log_path, "两次 run", |bytes| {
        count(bytes, b"run") >= 2
    });

    let stopped = runtime.stop(Duration::from_secs(10)).expect("停止");
    assert_eq!(stopped.exit_reason, Some(ExitReason::UserStop));
    let log = read_log(&stopped.log_path);
    assert!(
        count(&log, b"run") >= 2,
        "两次启动都应写入日志，实际：{}",
        String::from_utf8_lossy(&log)
    );
}

// ---------------------------------------------------------------- 启动失败

#[test]
fn occupied_port_fails_before_spawn() {
    let dir = tempfile::tempdir().expect("临时目录");
    let listener = TcpListener::bind(("127.0.0.1", 0)).expect("绑定临时端口");
    let port = listener.local_addr().expect("本地地址").port();

    let marker = dir.path().join("spawned.txt");
    let marker_text = marker.to_string_lossy().into_owned();
    let args = ["/C", "echo", "spawned", ">", marker_text.as_str()];
    let mut task = spec("port-conflict", dir.path(), &args);
    task.ports = vec![port];

    let runtime = TaskRuntime::new(task);
    let error = runtime.start().expect_err("端口被占用时必须拒绝启动");
    assert_eq!(error.stage, RuntimeStage::PortCheck);
    assert!(
        error.message.contains(&port.to_string()),
        "错误应包含端口号：{error}"
    );

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.status, TaskStatus::Stopped);
    assert_eq!(snapshot.pid, None);
    assert!(
        snapshot
            .last_error
            .as_ref()
            .is_some_and(|error| error.stage == RuntimeStage::PortCheck)
    );
    assert!(!marker.exists(), "端口冲突时不得创建进程");

    // 给“错误的启动”留出时间：如果真 spawn 了，重定向会创建 marker。
    thread::sleep(Duration::from_millis(200));
    assert!(!marker.exists(), "端口冲突时不得创建进程（延迟检查）");
    drop(listener);
}

#[test]
fn missing_command_reports_spawn_stage_and_code() {
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(TaskSpec::new(
        "missing",
        "campus-sentry-missing-command-xyz.exe",
        logs_params(dir.path()),
    ));

    let error = runtime.start().expect_err("缺失命令必须启动失败");
    assert_eq!(error.stage, RuntimeStage::Spawn);
    assert_eq!(error.code, Some(2), "ERROR_FILE_NOT_FOUND");
    assert!(
        error.message.contains("解析可执行文件"),
        "错误应包含失败阶段：{error}"
    );

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.status, TaskStatus::Stopped);
    assert_eq!(snapshot.pid, None);
    let last = snapshot.last_error.expect("应记录 last_error");
    assert_eq!(last.stage, RuntimeStage::Spawn);
    assert_eq!(last.code, Some(2));
    assert!(
        !snapshot.log_path.exists(),
        "启动失败不应创建日志文件：{}",
        snapshot.log_path.display()
    );
}

#[test]
fn start_while_running_is_rejected() {
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(spec("double-start", dir.path(), LONG_ARGS));

    runtime.start().expect("启动");
    wait_status(&runtime, TaskStatus::Running);

    let error = runtime.start().expect_err("运行中再次启动必须失败");
    assert_eq!(error.stage, RuntimeStage::InvalidState);
    assert_eq!(runtime.status(), TaskStatus::Running);

    runtime.stop(Duration::from_secs(10)).expect("停止");
}

// ---------------------------------------------------------------- 多任务/事件

#[test]
fn two_tasks_run_and_stop_without_interference() {
    let dir_alpha = tempfile::tempdir().expect("alpha 临时目录");
    let dir_beta = tempfile::tempdir().expect("beta 临时目录");
    let alpha_args = ["/C", "echo", "alpha", "&", "ping", "-n", "30", "127.0.0.1"];
    let beta_args = ["/C", "echo", "beta", "&", "ping", "-n", "30", "127.0.0.1"];

    let alpha = Arc::new(TaskRuntime::new(spec(
        "task-alpha",
        dir_alpha.path(),
        &alpha_args,
    )));
    let beta = Arc::new(TaskRuntime::new(spec(
        "task-beta",
        dir_beta.path(),
        &beta_args,
    )));

    alpha.start().expect("启动 alpha");
    beta.start().expect("启动 beta");
    wait_status(&alpha, TaskStatus::Running);
    wait_status(&beta, TaskStatus::Running);
    assert_ne!(
        alpha.snapshot().instance_id,
        beta.snapshot().instance_id,
        "两个任务应有不同的实例 id"
    );
    let alpha_log = alpha.snapshot().log_path;
    let beta_log = beta.snapshot().log_path;
    wait_log(&alpha_log, "alpha", |bytes| contains(bytes, b"alpha"));
    wait_log(&beta_log, "beta", |bytes| contains(bytes, b"beta"));

    thread::scope(|scope| {
        let alpha = Arc::clone(&alpha);
        let beta = Arc::clone(&beta);
        let first = scope.spawn(move || alpha.stop(Duration::from_secs(10)));
        let second = scope.spawn(move || beta.stop(Duration::from_secs(10)));
        first.join().expect("alpha 停止线程").expect("停止 alpha");
        second.join().expect("beta 停止线程").expect("停止 beta");
    });

    let stopped_alpha = alpha.snapshot();
    let stopped_beta = beta.snapshot();
    assert_eq!(stopped_alpha.status, TaskStatus::Stopped);
    assert_eq!(stopped_beta.status, TaskStatus::Stopped);
    assert_eq!(stopped_alpha.exit_reason, Some(ExitReason::UserStop));
    assert_eq!(stopped_beta.exit_reason, Some(ExitReason::UserStop));

    let log_alpha = read_log(&stopped_alpha.log_path);
    let log_beta = read_log(&stopped_beta.log_path);
    assert!(
        contains(&log_alpha, b"alpha") && !contains(&log_alpha, b"beta"),
        "alpha 日志不应混入 beta：{}",
        String::from_utf8_lossy(&log_alpha)
    );
    assert!(
        contains(&log_beta, b"beta") && !contains(&log_beta, b"alpha"),
        "beta 日志不应混入 alpha：{}",
        String::from_utf8_lossy(&log_beta)
    );
}

#[test]
fn event_channel_reports_lifecycle_in_order() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (sender, receiver) = mpsc::channel();
    let runtime = TaskRuntime::with_events(spec("events", dir.path(), LONG_ARGS), sender);

    runtime.start().expect("启动");
    wait_status(&runtime, TaskStatus::Running);
    runtime.stop(Duration::from_secs(10)).expect("停止");

    let mut statuses = Vec::new();
    loop {
        match receiver.recv_timeout(Duration::from_secs(5)) {
            Ok(event) => {
                let status = event.status();
                statuses.push(status);
                if status == TaskStatus::Stopped {
                    break;
                }
            }
            Err(error) => panic!("等待事件超时：{error}"),
        }
    }

    assert_eq!(
        statuses,
        vec![
            TaskStatus::Starting,
            TaskStatus::Running,
            TaskStatus::Stopping,
            TaskStatus::Stopped,
        ]
    );
    assert!(matches!(
        receiver.try_recv(),
        Err(mpsc::TryRecvError::Empty)
    ));
}

#[test]
fn start_failure_emits_start_failed_event() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (sender, receiver) = mpsc::channel();
    let runtime = TaskRuntime::with_events(
        TaskSpec::new(
            "missing-event",
            "campus-sentry-missing-command-xyz.exe",
            logs_params(dir.path()),
        ),
        sender,
    );

    runtime.start().expect_err("缺失命令必须启动失败");

    let starting = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("Starting 事件");
    assert_eq!(starting.status(), TaskStatus::Starting);
    let failed = receiver
        .recv_timeout(Duration::from_secs(5))
        .expect("StartFailed 事件");
    assert!(matches!(failed, RuntimeEvent::StartFailed(_)));
    assert_eq!(failed.status(), TaskStatus::Stopped);
}

// ---------------------------------------------------------------- 实例环境注入

fn assert_single_env_entry(dump: &Path, key: &str) {
    let text = fs::read_to_string(dump).expect("环境 dump 是 UTF-8");
    let count = text
        .lines()
        .filter(|line| {
            line.split_once('=')
                .is_some_and(|(name, _)| name.eq_ignore_ascii_case(key))
        })
        .count();
    assert_eq!(count, 1, "{key} 应只出现一次");
}

/// 自定义 context：保留变量全部注入、旧心跳删除、实例上下文被记录。
#[test]
fn start_with_context_injects_reserved_environment_and_cleans_heartbeat() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let heartbeat = dir.path().join("heartbeats").join("task.hb");
    fs::create_dir_all(heartbeat.parent().expect("心跳父目录")).expect("创建心跳目录");
    fs::write(&heartbeat, b"{\"stale\":true}").expect("写入旧心跳");
    let data_dir = dir.path().join("data");
    let config = dir.path().join("CampusSentry.toml");
    let dump = dir.path().join("env.txt");
    let dump_arg = dump.display().to_string();

    let task = spec(
        "env-inject",
        &log_dir,
        &["/C", "set", ">", dump_arg.as_str()],
    );
    let runtime = TaskRuntime::new(task);
    let context = InstanceContext {
        task_name: Some("Env Task".to_owned()),
        heartbeat: Some(heartbeat.clone()),
        data_dir: Some(data_dir.clone()),
        config: Some(config.clone()),
        ..InstanceContext::default()
    };

    let started = runtime
        .start_with_context(&context)
        .expect("启动环境注入任务");
    let instance_id = started.instance_id.clone().expect("运行实例 id");
    wait_status(&runtime, TaskStatus::Stopped);

    let env = parse_env_dump(&dump);
    let heartbeat_text = heartbeat.display().to_string();
    let log_text = log_dir.display().to_string();
    let data_text = data_dir.display().to_string();
    let config_text = config.display().to_string();
    let supervisor_pid = std::process::id().to_string();

    assert_eq!(
        env.get("CAMPUS_SENTRY_TASK_ID").map(String::as_str),
        Some("env-inject")
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_TASK_NAME").map(String::as_str),
        Some("Env Task")
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_INSTANCE_ID").map(String::as_str),
        Some(instance_id.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_HEARTBEAT").map(String::as_str),
        Some(heartbeat_text.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_LOG_DIR").map(String::as_str),
        Some(log_text.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_DATA_DIR").map(String::as_str),
        Some(data_text.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_CONFIG").map(String::as_str),
        Some(config_text.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_SUPERVISOR_PID").map(String::as_str),
        Some(supervisor_pid.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_ELEVATED").map(String::as_str),
        Some("0")
    );

    assert!(!heartbeat.exists(), "启动前应删除旧心跳文件");
    assert!(runtime.heartbeat_cleanup_error().is_none());
    assert!(runtime.instance_started_instant().is_some());
    assert_eq!(runtime.instance_context(), context);
}

/// 任务 env 里的同名项（包括大小写变体）不能覆盖保留变量。
#[test]
fn task_env_cannot_override_reserved_environment() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let heartbeat = dir.path().join("heartbeats").join("reserved.hb");
    fs::create_dir_all(heartbeat.parent().expect("心跳父目录")).expect("创建心跳目录");
    let data_dir = dir.path().join("data");
    let config = dir.path().join("CampusSentry.toml");
    let dump = dir.path().join("env.txt");
    let dump_arg = dump.display().to_string();

    let mut task = spec("reserved", &log_dir, &["/C", "set", ">", dump_arg.as_str()]);
    task.env = vec![
        (
            OsString::from("CAMPUS_SENTRY_TASK_ID"),
            OsString::from("evil-task"),
        ),
        (
            OsString::from("campus_sentry_instance_id"),
            OsString::from("evil-instance"),
        ),
        (
            OsString::from("CAMPUS_SENTRY_HEARTBEAT"),
            OsString::from("C:/evil.hb"),
        ),
        (
            OsString::from("CAMPUS_SENTRY_DATA_DIR"),
            OsString::from("C:/evil-data"),
        ),
        (
            OsString::from("CAMPUS_SENTRY_ELEVATED"),
            OsString::from("1"),
        ),
    ];
    task = task.with_instance_context(InstanceContext {
        task_name: Some("Real Task".to_owned()),
        heartbeat: Some(heartbeat.clone()),
        data_dir: Some(data_dir.clone()),
        config: Some(config.clone()),
        ..InstanceContext::default()
    });

    let runtime = TaskRuntime::new(task);
    let started = runtime.start().expect("启动保留变量测试任务");
    let instance_id = started.instance_id.clone().expect("运行实例 id");
    wait_status(&runtime, TaskStatus::Stopped);

    let env = parse_env_dump(&dump);
    assert_eq!(
        env.get("CAMPUS_SENTRY_TASK_ID").map(String::as_str),
        Some("reserved")
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_INSTANCE_ID").map(String::as_str),
        Some(instance_id.as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_HEARTBEAT").map(String::as_str),
        Some(heartbeat.display().to_string().as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_DATA_DIR").map(String::as_str),
        Some(data_dir.display().to_string().as_str())
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_ELEVATED").map(String::as_str),
        Some("0")
    );
    for key in [
        "CAMPUS_SENTRY_TASK_ID",
        "CAMPUS_SENTRY_INSTANCE_ID",
        "CAMPUS_SENTRY_HEARTBEAT",
        "CAMPUS_SENTRY_DATA_DIR",
        "CAMPUS_SENTRY_ELEVATED",
    ] {
        assert_single_env_entry(&dump, key);
    }
    assert!(!heartbeat.exists(), "启动前应删除旧心跳文件");
}

/// 2026-11-05：每实例必注入 START_REASON / WORK_MODE；WINDOW_END 由 context
/// 提供（仅 schedule.enabled 任务）；任务 env 不能覆盖这三个保留变量。
#[test]
fn start_reason_work_mode_and_window_end_are_injected() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let dump = dir.path().join("authority-env.txt");
    let dump_arg = dump.display().to_string();

    let mut task = spec(
        "authority",
        &log_dir,
        &["/C", "set", ">", dump_arg.as_str()],
    );
    task.env = vec![
        (
            OsString::from("CAMPUS_SENTRY_START_REASON"),
            OsString::from("evil"),
        ),
        (
            OsString::from("campus_sentry_work_mode"),
            OsString::from("idle"),
        ),
        (
            OsString::from("CAMPUS_SENTRY_WINDOW_END"),
            OsString::from("2000-01-01T00:00:00+00:00"),
        ),
    ];
    let runtime = TaskRuntime::new(task);
    let window_end = "2026-11-05T09:58:00+08:00".to_owned();
    let context = InstanceContext {
        task_name: Some("Authority".to_owned()),
        start_reason: StartReason::ScheduleStart,
        window_end: Some(window_end.clone()),
        ..InstanceContext::default()
    };

    runtime.start_with_context(&context).expect("启动注入任务");
    wait_status(&runtime, TaskStatus::Stopped);

    let env = parse_env_dump(&dump);
    assert_eq!(
        env.get("CAMPUS_SENTRY_START_REASON").map(String::as_str),
        Some("schedule_start"),
        "时段自动启动来源"
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_WORK_MODE").map(String::as_str),
        Some("active"),
        "所有实例固定 active"
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_WINDOW_END").map(String::as_str),
        Some(window_end.as_str()),
        "窗口结束时刻原样注入"
    );
    for key in [
        "CAMPUS_SENTRY_START_REASON",
        "CAMPUS_SENTRY_WORK_MODE",
        "CAMPUS_SENTRY_WINDOW_END",
    ] {
        assert_single_env_entry(&dump, key);
    }
    assert_eq!(runtime.instance_context(), context);
}

/// context 没有 WINDOW_END（无时段/未启用）时不注入；默认来源是 user_start。
#[test]
fn absent_window_end_is_not_injected_and_default_reason_is_user_start() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let dump = dir.path().join("default-reason-env.txt");
    let dump_arg = dump.display().to_string();

    let runtime = TaskRuntime::new(spec(
        "default-reason",
        &log_dir,
        &["/C", "set", ">", dump_arg.as_str()],
    ));
    runtime.start().expect("启动");
    wait_status(&runtime, TaskStatus::Stopped);

    let env = parse_env_dump(&dump);
    assert_eq!(
        env.get("CAMPUS_SENTRY_START_REASON").map(String::as_str),
        Some("user_start"),
        "没有显式来源时按手动启动处理"
    );
    assert_eq!(
        env.get("CAMPUS_SENTRY_WORK_MODE").map(String::as_str),
        Some("active")
    );
    assert!(
        !env.contains_key("CAMPUS_SENTRY_WINDOW_END"),
        "无窗口结束时刻时不注入 WINDOW_END"
    );
}

/// 四个稳定取值（脚本契约）与默认值。
#[test]
fn start_reason_strings_are_stable() {
    let pairs = [
        (StartReason::ScheduleStart, "schedule_start"),
        (StartReason::UserStart, "user_start"),
        (StartReason::Restart, "restart"),
        (StartReason::Autostart, "autostart"),
    ];
    for (reason, text) in pairs {
        assert_eq!(reason.as_str(), text);
    }
    assert_eq!(StartReason::ALL.len(), 4);
    assert_eq!(StartReason::default(), StartReason::UserStart);
}

/// Lead 审查发现：start_with_context 后 restart 必须沿用最近 context。
#[test]
fn restart_reuses_latest_context_and_reinjects_heartbeat_and_data_dir() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let heartbeat = dir.path().join("heartbeats").join("restart.hb");
    fs::create_dir_all(heartbeat.parent().expect("心跳父目录")).expect("创建心跳目录");
    let data_dir = dir.path().join("data");
    let config = dir.path().join("CampusSentry.toml");
    let dump = dir.path().join("restart-env.txt");
    let dump_arg = dump.display().to_string();

    let mut task = TaskSpec::new("restart-context", "cmd.exe", logs_params(&log_dir));
    task.args = vec![
        "/C".to_owned(),
        "echo".to_owned(),
        "%CAMPUS_SENTRY_HEARTBEAT%,%CAMPUS_SENTRY_DATA_DIR%,%CAMPUS_SENTRY_INSTANCE_ID%".to_owned(),
        ">>".to_owned(),
        dump_arg,
        "&".to_owned(),
        "ping".to_owned(),
        "-n".to_owned(),
        "30".to_owned(),
        "127.0.0.1".to_owned(),
    ];
    // spec.instance 留空：验证 restart 复用 start_with_context 记录的 context。
    let runtime = TaskRuntime::new(task);
    let context = InstanceContext {
        task_name: Some("Restart Context".to_owned()),
        heartbeat: Some(heartbeat.clone()),
        data_dir: Some(data_dir.clone()),
        config: Some(config.clone()),
        ..InstanceContext::default()
    };

    let first = runtime
        .start_with_context(&context)
        .expect("首次启动（自定义 context）");
    wait_status(&runtime, TaskStatus::Running);
    let first_lines = wait_lines(&dump, 1);
    let first_id = first.instance_id.clone().expect("首次实例 id");
    assert_eq!(first_lines.len(), 1);
    assert_eq!(runtime.instance_context(), context);

    // 模拟上一实例遗留的心跳文件；restart_with_context 必须在 spawn 前删除。
    fs::write(&heartbeat, b"{\"stale\":true}").expect("写入旧心跳");
    let restarted = runtime
        .restart_with_context(Duration::from_secs(10), &context)
        .expect("按 context 重启");
    wait_status(&runtime, TaskStatus::Running);
    let lines = wait_lines(&dump, 2);
    let second_id = restarted.instance_id.clone().expect("重启实例 id");
    assert_ne!(second_id, first_id, "重启必须分配新的实例 id");
    assert!(!heartbeat.exists(), "重启前应删除旧心跳文件");

    let expected_prefix = format!("{},{}", heartbeat.display(), data_dir.display());
    let first_fields: Vec<&str> = lines[0].split(',').collect();
    let second_fields: Vec<&str> = lines[1].split(',').collect();
    assert_eq!(first_fields.len(), 3, "首次注入格式错误：{:?}", lines[0]);
    assert_eq!(second_fields.len(), 3, "重启注入格式错误：{:?}", lines[1]);
    assert_eq!(first_fields[0], heartbeat.display().to_string());
    assert_eq!(first_fields[1], data_dir.display().to_string());
    assert_eq!(first_fields[2], first_id);
    assert_eq!(second_fields[0], heartbeat.display().to_string());
    assert_eq!(second_fields[1], data_dir.display().to_string());
    assert_eq!(second_fields[2], second_id);
    assert!(
        lines[0].starts_with(&expected_prefix),
        "首次应注入 HEARTBEAT/DATA_DIR：{}",
        lines[0]
    );
    assert!(
        lines[1].starts_with(&expected_prefix),
        "重启后仍应注入 HEARTBEAT/DATA_DIR：{}",
        lines[1]
    );
    assert_eq!(runtime.instance_context(), context);

    runtime.stop(Duration::from_secs(10)).expect("停止");
}

/// 连续两次 start（非 restart）也必须得到新的实例 id 与对应环境注入。
#[test]
fn each_start_gets_unique_instance_id_and_environment() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let dump = dir.path().join("unique-env.txt");
    let dump_arg = dump.display().to_string();

    let task = spec(
        "start-unique",
        &log_dir,
        &[
            "/C",
            "echo",
            "%CAMPUS_SENTRY_INSTANCE_ID%",
            ">>",
            dump_arg.as_str(),
        ],
    );
    let runtime = TaskRuntime::new(task);

    let first = runtime.start().expect("第一次启动");
    wait_status(&runtime, TaskStatus::Stopped);
    let first_id = first.instance_id.clone().expect("第一次实例 id");
    let lines = wait_lines(&dump, 1);
    assert_eq!(lines[0], first_id);

    let second = runtime.start().expect("第二次启动");
    wait_status(&runtime, TaskStatus::Stopped);
    let second_id = second.instance_id.clone().expect("第二次实例 id");
    let lines = wait_lines(&dump, 2);
    assert_ne!(second_id, first_id, "每次 start 都应分配新的实例 id");
    assert_eq!(lines[1], second_id);
    assert_eq!(runtime.snapshot().instance_id, Some(second_id));
}

/// Running 事件携带实例 id 与进程创建时刻（startup_grace 基线）。
#[test]
fn running_event_carries_instance_id_and_start_instant() {
    let dir = tempfile::tempdir().expect("临时目录");
    let (sender, receiver) = mpsc::channel();
    let runtime = TaskRuntime::with_events(spec("event-running", dir.path(), LONG_ARGS), sender);

    let before = Instant::now();
    let started = runtime.start().expect("启动长进程");
    let after = Instant::now();
    let expected_id = started.instance_id.clone().expect("运行实例 id");

    let mut running = None;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(200)) {
            Ok(event @ RuntimeEvent::Running { .. }) => {
                running = Some(event);
                break;
            }
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(error) => panic!("事件通道关闭：{error}"),
        }
    }
    let RuntimeEvent::Running {
        instance_id,
        started_instant,
        heartbeat_cleanup_error,
        snapshot,
    } = running.expect("应收到 Running 事件")
    else {
        unreachable!("上方已限定为 Running");
    };

    assert_eq!(instance_id, expected_id);
    assert_eq!(snapshot.instance_id.as_deref(), Some(expected_id.as_str()));
    assert!(
        started_instant >= before && started_instant <= after,
        "started_instant 应落在 start() 调用窗口内"
    );
    assert_eq!(runtime.instance_started_instant(), Some(started_instant));
    assert!(heartbeat_cleanup_error.is_none());

    runtime.stop(Duration::from_secs(10)).expect("停止");
}

/// 旧心跳删除失败（例如路径是目录）只记录，不阻止启动。
#[test]
fn heartbeat_cleanup_failure_is_reported_but_start_succeeds() {
    let dir = tempfile::tempdir().expect("临时目录");
    let log_dir = dir.path().join("logs");
    let heartbeat = dir.path().join("heartbeat-is-a-directory");
    fs::create_dir(&heartbeat).expect("用目录冒充心跳文件");
    let mut task = spec("hb-fail", &log_dir, &["/C", "exit", "0"]);
    task.instance.heartbeat = Some(heartbeat.clone());

    let (sender, receiver) = mpsc::channel();
    let runtime = TaskRuntime::with_events(task, sender);
    runtime.start().expect("清理失败不应阻止启动");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.exit_code, Some(0));
    assert!(snapshot.last_error.is_none());
    assert!(heartbeat.is_dir(), "删除失败时目标应保持原样");
    let error = runtime.heartbeat_cleanup_error().expect("应记录清理失败");
    assert!(
        error.contains(&heartbeat.display().to_string()),
        "错误应包含路径：{error}"
    );

    // Running 事件同样携带失败原因，供 Engine 记日志/提示。
    let mut running_error = None;
    loop {
        let event = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("等待运行时事件");
        if let RuntimeEvent::Running {
            heartbeat_cleanup_error,
            ..
        } = &event
        {
            running_error = heartbeat_cleanup_error.clone();
        }
        if event.status() == TaskStatus::Stopped {
            break;
        }
    }
    let running_error = running_error.expect("Running 事件应携带清理失败原因");
    assert!(running_error.contains(&heartbeat.display().to_string()));
}

/// CJK/空格工作目录 + CJK 参数：启动与日志落盘都不依赖 ANSI 代码页。
#[test]
fn cjk_working_dir_and_args_reach_child() {
    let dir = tempfile::tempdir().expect("临时目录");
    let work = dir.path().join("工作 目录 with space");
    fs::create_dir_all(&work).expect("创建 CJK 工作目录");
    fs::write(work.join("参数 文件.txt"), b"cjk-arg-ok").expect("创建 CJK 参数文件");
    let log_dir = dir.path().join("日志 目录");

    let mut task = TaskSpec::new("cjk", "cmd.exe", logs_params(&log_dir));
    task.working_dir = Some(work);
    task.args = vec![
        "/C".to_owned(),
        "type".to_owned(),
        "参数 文件.txt".to_owned(),
    ];

    let runtime = TaskRuntime::new(task);
    runtime.start().expect("启动 CJK 路径/参数任务");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.exit_code, Some(0));
    let log = read_log(&snapshot.log_path);
    assert!(
        contains(&log, b"cjk-arg-ok"),
        "CJK 工作目录与参数应原样传给子进程：{}",
        String::from_utf8_lossy(&log)
    );
}

// ---------------------------------------------------------------- Engine 协调纯逻辑

fn ts(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("合法时间")
}

fn snapshot_of(status: TaskStatus) -> RuntimeSnapshot {
    RuntimeSnapshot {
        id: "task".to_owned(),
        status,
        pid: None,
        instance_id: Some("inst".to_owned()),
        started_at: None,
        exit_code: None,
        exit_reason: None,
        tree_empty: None,
        forced_kill: false,
        last_error: None,
        logs: LogStats::default(),
        log_path: std::path::PathBuf::from("C:/logs/task/current.log"),
    }
}

/// reload/start 竞态：in-flight 期间 reload 不得替换 runtime；完成后报告代际变化。
#[test]
fn reload_gate_tracks_in_flight_commands_and_generation() {
    let mut gate = ReloadGate::new();
    assert_eq!(gate.generation(), 0);
    assert_eq!(gate.try_begin_command("alpha"), Some(0));
    assert!(gate.is_in_flight("alpha"));
    assert_eq!(
        gate.try_begin_command("alpha"),
        None,
        "同一任务不允许并发命令"
    );

    // 重载代际前进，但 in-flight 标记保持；Engine 的 sync 会跳过替换。
    assert_eq!(gate.reload(), 1);
    assert!(gate.is_in_flight("alpha"));
    assert_eq!(gate.try_begin_command("beta"), Some(1));

    // 命令完成时报告“期间发生过重载”，Engine 据此补一次 sync。
    assert!(gate.finish_command("alpha", 0));
    assert!(!gate.is_in_flight("alpha"));
    assert!(!gate.finish_command("beta", 1));
    assert_eq!(gate.reload(), 2);
}

#[test]
fn manual_actions_respect_enabled_and_valid() {
    assert!(manual_start_allowed(true, true, TaskStatus::Stopped));
    assert!(
        !manual_start_allowed(true, false, TaskStatus::Stopped),
        "enabled=false 不可手动启动"
    );
    assert!(!manual_start_allowed(false, true, TaskStatus::Stopped));
    assert!(!manual_start_allowed(true, true, TaskStatus::Running));
    assert!(manual_restart_allowed(true, true, TaskStatus::Running));
    assert!(manual_restart_allowed(true, true, TaskStatus::Stopped));
    assert!(
        !manual_restart_allowed(true, false, TaskStatus::Running),
        "禁用任务停止后也不能重启"
    );
}

#[test]
fn auto_start_policy_skips_override_paused_and_needs_human() {
    assert!(auto_start_allowed(
        true,
        true,
        false,
        false,
        false,
        Some("stopped")
    ));
    assert!(auto_start_allowed(
        true,
        true,
        false,
        false,
        false,
        Some("running")
    ));
    assert!(!auto_start_allowed(false, true, false, false, false, None));
    assert!(!auto_start_allowed(true, false, false, false, false, None));
    assert!(
        !auto_start_allowed(true, true, true, false, false, None),
        "用户 override 存在时不自动启动"
    );
    assert!(
        !auto_start_allowed(true, true, false, true, false, None),
        "paused 记忆存在时不自动启动"
    );
    assert!(
        !auto_start_allowed(true, true, false, false, true, None),
        "needs_human 置位时不自动启动"
    );
    assert!(!auto_start_allowed(
        true,
        true,
        false,
        false,
        false,
        Some("paused")
    ));
    assert!(!auto_start_allowed(
        true,
        true,
        false,
        false,
        false,
        Some("backoff")
    ));
}

#[test]
fn resume_shift_keeps_expired_timers_and_suppressed_tasks_out_of_start() {
    use super::power::{resume_deadline, shift_timer};

    // 睡眠 1 小时（QPC 照走）：睡眠前已过期的退避/自动启动整体后移，
    // 唤醒后不会"一醒就到期"立刻重启（QA-M3.6b 阻断 2）。
    let resume_at = Instant::now();
    let slept = Duration::from_secs(3600);
    let grace = Duration::from_secs(20);
    let expired = resume_at - Duration::from_secs(600);
    let shifted = resume_deadline(Some(expired), slept, resume_at, grace).expect("应平移");
    assert!(shifted >= resume_at + grace);
    assert_eq!(shift_timer(Some(expired), slept), Some(expired + slept));

    // 平移只改时间不改意图：paused/needs_human/override 仍不会被拉起。
    assert!(!auto_start_allowed(
        true,
        true,
        true,
        false,
        false,
        Some("stopped")
    ));
    assert!(!auto_start_allowed(
        true,
        true,
        false,
        true,
        false,
        Some("stopped")
    ));
    assert!(!auto_start_allowed(
        true,
        true,
        false,
        false,
        true,
        Some("stopped")
    ));
    assert!(auto_start_allowed(
        true,
        true,
        false,
        false,
        false,
        Some("stopped")
    ));
}

#[test]
fn user_intent_updates_persisted_override_and_failure_budget() {
    let mut state = TaskState {
        desired_override: Some(DesiredOverride::Paused),
        paused_reason: Some(PausedReason::CrashLoop),
        consecutive_failures: 5,
        next_retry_at: Some(ts("2026-11-02T04:50:00Z")),
        needs_human: NeedsHumanState {
            active: true,
            source: Some("heartbeat".to_owned()),
            message: Some("login_required".to_owned()),
            since: Some(ts("2026-11-02T04:40:00Z")),
        },
        ..TaskState::default()
    };

    apply_user_intent(&mut state, UserAction::Start);
    assert_eq!(state.desired_override, None);
    assert_eq!(state.paused_reason, None);
    assert_eq!(state.consecutive_failures, 0);
    assert_eq!(state.next_retry_at, None);
    assert!(!state.needs_human.active);

    // Stop 保留失败计数（docs/03 §6.3），写入 stopped override。
    state.consecutive_failures = 3;
    apply_user_intent(&mut state, UserAction::Stop);
    assert_eq!(state.desired_override, Some(DesiredOverride::Stopped));
    assert_eq!(state.consecutive_failures, 3);
    assert_eq!(state.paused_reason, None);
    assert!(!state.needs_human.active);

    // Restart = 用户显式要它运行（Q1 裁定）：清除 override、失败计数与 needs_human。
    let mut restart = TaskState {
        desired_override: Some(DesiredOverride::Paused),
        consecutive_failures: 4,
        paused_reason: Some(PausedReason::NeedsHuman),
        needs_human: NeedsHumanState {
            active: true,
            ..NeedsHumanState::default()
        },
        ..TaskState::default()
    };
    apply_user_intent(&mut restart, UserAction::Restart);
    assert_eq!(restart.desired_override, None);
    assert_eq!(restart.consecutive_failures, 0);
    assert_eq!(restart.paused_reason, None);
}

#[test]
fn runtime_snapshot_updates_last_status_and_counts_start_once_per_instance() {
    let started_at = ts("2026-11-02T04:40:00Z");
    let mut state = TaskState::default();
    let running = snapshot_of(TaskStatus::Running);

    assert!(apply_runtime_snapshot(&mut state, &running, started_at));
    assert_eq!(state.last_status, Some(LastStatus::Running));
    assert_eq!(state.total_starts, 1);
    assert_eq!(state.last_start_at, Some(started_at));

    // 重复 Running 快照（命令结果 + 事件各一次）不重复计数。
    assert!(!apply_runtime_snapshot(&mut state, &running, started_at));
    assert_eq!(state.total_starts, 1);

    let exited_at = ts("2026-11-02T04:41:00Z");
    let mut stopped = snapshot_of(TaskStatus::Stopped);
    stopped.exit_code = Some(7);
    stopped.exit_reason = Some(ExitReason::Natural);
    assert!(apply_runtime_snapshot(&mut state, &stopped, exited_at));
    assert_eq!(state.last_status, Some(LastStatus::Stopped));
    assert_eq!(state.last_exit_code, Some(7));
    assert_eq!(state.last_exit_at, Some(exited_at));
    assert_eq!(state.last_reason.as_deref(), Some("natural"));

    // 下一次实例再次 running 时重新计一次启动。
    assert!(apply_runtime_snapshot(&mut state, &running, started_at));
    assert_eq!(state.total_starts, 2);
}

#[test]
fn runtime_snapshot_records_start_failure_stage() {
    let mut state = TaskState::default();
    let mut failed = snapshot_of(TaskStatus::Stopped);
    failed.last_error = Some(RuntimeError::new(
        RuntimeStage::Spawn,
        Some(2),
        "找不到命令".to_owned(),
    ));

    assert!(apply_runtime_snapshot(
        &mut state,
        &failed,
        ts("2026-11-02T04:40:00Z")
    ));
    assert_eq!(state.last_status, Some(LastStatus::Stopped));
    assert_eq!(state.last_reason.as_deref(), Some("start_failed:spawn"));
}

#[test]
fn prune_removed_tasks_clears_only_unknown_ids() {
    let mut state = PersistedState::default();
    state.task_mut("keep").consecutive_failures = 2;
    state.task_mut("gone").consecutive_failures = 1;
    let known: HashSet<String> = ["keep".to_owned()].into_iter().collect();

    let removed = prune_removed_tasks(&mut state, &known);
    assert_eq!(removed, vec!["gone".to_owned()]);
    assert!(state.task("keep").is_some());
    assert!(state.task("gone").is_none());
    assert_eq!(state.task("keep").expect("保留项").consecutive_failures, 2);
}

// ---------------------------------------------------------------- 配置映射

/// `TaskConfig` → `TaskSpec` 的纯映射（不启动进程）。
#[test]
fn task_spec_from_config_maps_all_fields() {
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use crate::config::{
        Encoding, EnvProxy, HealthConfig, HealthKind, PowerConfig, RestartConfig, Rotation,
        StopConfig, StopMethod, TaskConfig, TaskLogsConfig,
    };

    let mut task = TaskConfig {
        id: "alpha".to_owned(),
        name: "甲任务".to_owned(),
        enabled: true,
        order: 0,
        command: "cmd.exe".to_owned(),
        args: vec!["/C".to_owned(), "echo hi".to_owned()],
        working_dir: PathBuf::from("C:/work/alpha"),
        auto_start: false,
        elevated: false,
        notify: false,
        env: BTreeMap::from([
            ("FOO".to_owned(), "bar".to_owned()),
            ("PATH".to_owned(), "C:/bin".to_owned()),
        ]),
        ports: vec![3000, 3001],
        env_proxy: EnvProxy::Inherit,
        panel_url: None,
        plugin: None,
        schedule: crate::config::ScheduleConfig::default(),
        health: HealthConfig::default(),
        restart: RestartConfig::default(),
        stop: StopConfig {
            method: StopMethod::Terminate,
            grace_ms: 1500,
            timeout_ms: 1234,
        },
        logs: TaskLogsConfig {
            dir: PathBuf::from("C:/logs/alpha"),
            rotation: Rotation::Daily,
            max_file_mb: 7,
            max_files: 3,
            max_days: 5,
            encoding: Encoding::Auto,
        },
        power: PowerConfig::default(),
        limits: Default::default(),
    };

    let spec = TaskSpec::from_config(&task);

    assert_eq!(spec.id, "alpha");
    assert_eq!(spec.instance.task_name.as_deref(), Some("甲任务"));
    assert_eq!(spec.instance.heartbeat, None);
    assert_eq!(spec.instance.data_dir, None);
    assert_eq!(spec.instance.config, None);
    assert_eq!(spec.program, OsString::from("cmd.exe"));
    assert_eq!(spec.args, ["/C", "echo hi"]);
    assert_eq!(
        spec.working_dir.as_deref(),
        Some(Path::new("C:/work/alpha"))
    );
    assert_eq!(
        spec.env,
        vec![
            (OsString::from("FOO"), OsString::from("bar")),
            (OsString::from("PATH"), OsString::from("C:/bin")),
        ]
    );
    assert_eq!(spec.ports, [3000, 3001]);

    // 日志参数来自 `[tasks.logs]`，未配置项取采集器默认值。
    assert_eq!(spec.logs.dir, PathBuf::from("C:/logs/alpha"));
    assert_eq!(spec.logs.rotation, Rotation::Daily);
    assert_eq!(spec.logs.max_file_bytes, 7 * 1024 * 1024);
    assert_eq!(spec.logs.max_files, 3);
    assert_eq!(spec.logs.max_days, 5);

    // `[tasks.stop]` 的 method/grace/timeout 都映射到监控参数。
    assert_eq!(spec.watch.stop_timeout, Duration::from_millis(1234));
    assert_eq!(spec.watch.method, crate::process::StopMode::Terminate);
    assert_eq!(spec.watch.grace, Duration::from_millis(1500));
    assert_eq!(
        spec.watch.tree_wait,
        crate::process::WatchOptions::default().tree_wait
    );

    // heartbeat 模式才把 health.path 映射为 CAMPUS_SENTRY_HEARTBEAT 来源。
    task.health.kind = HealthKind::Heartbeat;
    task.health.path = Some(PathBuf::from("C:/data/heartbeats/alpha.hb"));
    let beat_spec = TaskSpec::from_config(&task);
    assert_eq!(
        beat_spec.instance.heartbeat,
        Some(PathBuf::from("C:/data/heartbeats/alpha.hb"))
    );
    assert_eq!(beat_spec.instance.task_name.as_deref(), Some("甲任务"));

    // Engine 入口：由配置 + 数据目录/配置路径补齐绝对路径。
    let tmp = tempfile::tempdir().expect("临时目录");
    let data_dir = tmp.path().join("data");
    let config_path = tmp.path().join("CampusSentry.toml");
    let full = TaskSpec::from_config_with_paths(&task, &data_dir, &config_path);
    assert_eq!(full.instance.task_name.as_deref(), Some("甲任务"));
    assert_eq!(
        full.instance.heartbeat.as_deref(),
        Some(Path::new("C:/data/heartbeats/alpha.hb"))
    );
    assert_eq!(full.instance.data_dir.as_deref(), Some(data_dir.as_path()));
    assert_eq!(full.instance.config.as_deref(), Some(config_path.as_path()));
    assert!(
        full.instance
            .data_dir
            .as_ref()
            .is_some_and(|path| path.is_absolute()),
        "DATA_DIR 必须是绝对路径"
    );
    assert!(
        full.instance
            .config
            .as_ref()
            .is_some_and(|path| path.is_absolute()),
        "CONFIG 必须是绝对路径"
    );

    // heartbeat 未配置 path：默认 <data_dir>/heartbeats/<id>.hb（绝对路径）。
    task.health.path = None;
    let defaulted = TaskSpec::from_config_with_paths(&task, &data_dir, &config_path);
    let expected = data_dir.join("heartbeats").join("alpha.hb");
    assert_eq!(
        defaulted.instance.heartbeat.as_deref(),
        Some(expected.as_path())
    );
    assert!(
        defaulted
            .instance
            .heartbeat
            .as_ref()
            .is_some_and(|path| path.is_absolute())
    );

    // 非 heartbeat 任务不注入 HEARTBEAT，但仍补齐 DATA_DIR/CONFIG。
    task.health.kind = HealthKind::Process;
    let process = TaskSpec::from_config_with_paths(&task, &data_dir, &config_path);
    assert_eq!(process.instance.heartbeat, None);
    assert_eq!(
        process.instance.data_dir.as_deref(),
        Some(data_dir.as_path())
    );

    // 调用方给相对路径时也归一化为绝对路径（脚本契约要求绝对路径）。
    let relative = TaskSpec::from_config_with_paths(
        &task,
        Path::new("relative-data"),
        Path::new("relative-config.toml"),
    );
    assert!(
        relative
            .instance
            .data_dir
            .as_ref()
            .is_some_and(|path| path.is_absolute())
    );
    assert!(
        relative
            .instance
            .config
            .as_ref()
            .is_some_and(|path| path.is_absolute())
    );
}

// ---------------------------------------------------------------- M3-4b 监督纯逻辑

#[test]
fn heartbeat_dir_is_created_and_write_checked() {
    let dir = tempfile::tempdir().expect("临时目录");
    let heartbeat = dir.path().join("heartbeats").join("task.hb");
    let context = InstanceContext {
        heartbeat: Some(heartbeat.clone()),
        ..InstanceContext::default()
    };
    ensure_heartbeat_dir(&context).expect("应创建心跳目录");
    assert!(heartbeat.parent().expect("父目录").is_dir());

    // 非 heartbeat 任务：无路径直接通过。
    ensure_heartbeat_dir(&InstanceContext::default()).expect("无心跳路径应直接通过");

    // 父路径被文件占用：必须报错（启动前检查失败），不得静默。
    let blocked = dir.path().join("blocked");
    fs::write(&blocked, b"x").expect("占用父路径");
    let bad = InstanceContext {
        heartbeat: Some(blocked.join("task.hb")),
        ..InstanceContext::default()
    };
    let error = ensure_heartbeat_dir(&bad).expect_err("父路径为文件时必须报错");
    assert!(error.contains("blocked"), "{error}");
}

#[test]
fn health_monitor_builds_for_process_and_requires_http_url() {
    let task = supervision_task("alive");
    let mut monitor = build_health_monitor(
        &task,
        &InstanceContext::default(),
        std::process::id(),
        "inst-1",
        Instant::now(),
    )
    .expect("process 模式应能构造监视器");
    let verdict = monitor.check(Instant::now());
    assert_eq!(verdict.state, HealthState::Healthy);

    let mut http = supervision_task("http");
    http.health.kind = HealthKind::Http;
    http.health.url = None;
    assert!(
        build_health_monitor(
            &http,
            &InstanceContext::default(),
            1,
            "inst-2",
            Instant::now()
        )
        .is_none(),
        "http 模式缺 url 时不应构造监视器"
    );
}

#[test]
fn restore_seed_maps_paused_needs_human_and_backoff() {
    let task = supervision_task("restore");
    let mono_now = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");

    let mut state = TaskState {
        consecutive_failures: 4,
        paused_reason: Some(PausedReason::CrashLoop),
        last_status: Some(LastStatus::Paused),
        ..TaskState::default()
    };
    let seed = restore_seed(&task, Some(&state), mono_now, wall_now);
    assert_eq!(seed.consecutive_failures, 4);
    assert!(seed.paused);
    assert_eq!(seed.paused_reason, Some(Reason::CrashLoop));
    assert_eq!(seed.next_retry_at, None);

    state.desired_override = None;
    state.paused_reason = None;
    state.last_status = Some(LastStatus::Backoff);
    state.next_retry_at = Some(Timestamp::parse("2026-11-02T04:41:00Z").expect("合法时间"));
    let seed = restore_seed(&task, Some(&state), mono_now, wall_now);
    assert!(seed.next_retry_at.is_some());
    assert!(seed.next_retry_at.expect("退避到期") >= mono_now + Duration::from_secs(59));

    // 已过期：至少 initial_delay（默认 3000ms）后再启动（Q12）。
    state.next_retry_at = Some(Timestamp::parse("2026-11-02T04:39:00Z").expect("合法时间"));
    let seed = restore_seed(&task, Some(&state), mono_now, wall_now);
    assert_eq!(
        seed.next_retry_at,
        Some(mono_now + Duration::from_millis(3000))
    );

    let needs_human = TaskState {
        needs_human: NeedsHumanState {
            active: true,
            source: Some("heartbeat".to_owned()),
            message: Some("login_required".to_owned()),
            since: Some(wall_now),
        },
        last_status: Some(LastStatus::Paused),
        ..TaskState::default()
    };
    let seed = restore_seed(&task, Some(&needs_human), mono_now, wall_now);
    let needs = seed.needs_human.expect("应恢复 needs_human");
    assert_eq!(needs.source, "heartbeat");
    assert_eq!(needs.message, "login_required");
}

#[test]
fn state_machine_seed_restores_paused_and_backoff_state() {
    let mono_now = Instant::now();
    let policy = RestartPolicy::from_config(&RestartConfig::default());
    let paused = RestartMachine::restore(
        policy.clone(),
        FixedRandom(0.5),
        crate::restart::RestartSeed {
            consecutive_failures: 3,
            paused: true,
            paused_reason: Some(Reason::CrashLoop),
            needs_human: None,
            next_retry_at: None,
        },
    );
    assert_eq!(paused.status(), RestartStatus::Paused);
    assert_eq!(paused.paused_reason(), Some(Reason::CrashLoop));
    assert_eq!(paused.consecutive_failures(), 3);
    assert!(matches!(
        paused.clone().handle(RestartEvent::Tick, mono_now),
        Decision::Stay { .. }
    ));

    let retry_at = mono_now + Duration::from_secs(5);
    let mut backoff = RestartMachine::restore(
        policy.clone(),
        FixedRandom(0.5),
        crate::restart::RestartSeed {
            consecutive_failures: 2,
            paused: false,
            paused_reason: None,
            needs_human: None,
            next_retry_at: Some(retry_at),
        },
    );
    assert_eq!(backoff.status(), RestartStatus::Backoff);
    assert!(matches!(
        backoff.clone().handle(RestartEvent::Tick, mono_now),
        Decision::Stay { .. }
    ));
    assert!(matches!(
        backoff.handle(RestartEvent::Tick, retry_at),
        Decision::StartNow { .. }
    ));

    let needs_human = RestartMachine::restore(
        policy,
        FixedRandom(0.5),
        crate::restart::RestartSeed {
            consecutive_failures: 0,
            paused: false,
            paused_reason: None,
            needs_human: Some(crate::restart::NeedsHuman {
                source: "restored".to_owned(),
                message: "需要人工".to_owned(),
                since: mono_now,
            }),
            next_retry_at: None,
        },
    );
    assert_eq!(needs_human.status(), RestartStatus::Paused);
    assert_eq!(needs_human.paused_reason(), Some(Reason::NeedsHuman));
    let snapshot = needs_human.snapshot(mono_now);
    let mut state = TaskState::default();
    assert!(sync_state(
        &mut state,
        &snapshot,
        mono_now,
        Timestamp::now()
    ));
    assert!(state.needs_human.active);
    assert_eq!(state.paused_reason, Some(PausedReason::NeedsHuman));
    assert_eq!(state.last_status, Some(LastStatus::Paused));
}

#[test]
fn replay_alive_machine_keeps_live_needs_human_running() {
    let mono_now = Instant::now();
    let policy = RestartPolicy::from_config(&RestartConfig::default());
    let needs = NeedsHuman {
        source: "heartbeat".to_owned(),
        message: "登录失效".to_owned(),
        since: mono_now,
    };
    // QA C：restore 看到 needs_human 会强制 Paused，但实例其实仍在运行。
    let mut machine = RestartMachine::restore(
        policy,
        FixedRandom(0.5),
        crate::restart::RestartSeed {
            needs_human: Some(needs.clone()),
            ..Default::default()
        },
    );
    assert_eq!(machine.status(), RestartStatus::Paused);

    assert!(replay_alive_machine(
        &mut machine,
        RestartStatus::Running,
        Some(needs),
        mono_now
    ));
    assert_eq!(machine.status(), RestartStatus::Running);
    assert_eq!(machine.intent(), Intent::Running);
    assert!(machine.needs_human().is_some());
    assert_eq!(machine.paused_reason(), None);
}

#[test]
fn replay_alive_machine_replays_starting_and_rejects_stopped() {
    let mono_now = Instant::now();
    let policy = RestartPolicy::from_config(&RestartConfig::default());
    let mut machine = supervision_machine(policy.clone());
    assert!(replay_alive_machine(
        &mut machine,
        RestartStatus::Starting,
        None,
        mono_now
    ));
    assert_eq!(machine.status(), RestartStatus::Starting);
    assert!(replay_alive_machine(
        &mut machine,
        RestartStatus::Running,
        None,
        mono_now
    ));
    assert_eq!(machine.status(), RestartStatus::Running);

    let mut stopped = supervision_machine(policy);
    assert!(!replay_alive_machine(
        &mut stopped,
        RestartStatus::Stopped,
        None,
        mono_now
    ));
    assert_eq!(stopped.status(), RestartStatus::Stopped);
}

#[test]
fn alive_status_keeps_starting_until_machine_ready() {
    // runtime 已 spawn 成功（Running），但状态机仍在启动宽限内：必须保留 Starting，
    // 否则配置重建会跳过 readiness 与 startup_grace（QA C）。
    assert_eq!(
        alive_status_for_rebuild(RestartStatus::Starting, Some(TaskStatus::Running)),
        Some(RestartStatus::Starting)
    );
    assert_eq!(
        alive_status_for_rebuild(RestartStatus::Starting, Some(TaskStatus::Starting)),
        Some(RestartStatus::Starting)
    );
    // 状态机已 Running 才按运行时状态恢复；needs_human 暂停的活进程交给调用方重放。
    assert_eq!(
        alive_status_for_rebuild(RestartStatus::Running, Some(TaskStatus::Running)),
        Some(RestartStatus::Running)
    );
    assert_eq!(
        alive_status_for_rebuild(RestartStatus::Paused, Some(TaskStatus::Running)),
        Some(RestartStatus::Running)
    );
    assert_eq!(
        alive_status_for_rebuild(RestartStatus::Running, Some(TaskStatus::Stopped)),
        None
    );
    assert_eq!(
        alive_status_for_rebuild(RestartStatus::Stopped, Some(TaskStatus::Stopped)),
        None
    );
}

#[test]
fn sync_state_is_idempotent_after_first_write() {
    let mono_now = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
    let policy = RestartPolicy::from_config(&RestartConfig::default());
    let machine = RestartMachine::restore(
        policy,
        FixedRandom(0.5),
        crate::restart::RestartSeed {
            consecutive_failures: 2,
            paused: false,
            paused_reason: None,
            needs_human: None,
            next_retry_at: Some(mono_now + Duration::from_secs(5)),
        },
    );
    let snapshot = machine.snapshot(mono_now);
    let mut state = TaskState::default();
    assert!(sync_state(&mut state, &snapshot, mono_now, wall_now));
    assert_eq!(state.consecutive_failures, 2);
    assert_eq!(
        state.next_retry_at,
        Some(Timestamp::new(
            wall_now.datetime() + chrono::Duration::seconds(5)
        ))
    );
    assert_eq!(state.last_status, Some(LastStatus::Backoff));
    assert!(!sync_state(&mut state, &snapshot, mono_now, wall_now));
}

#[test]
fn auto_start_plan_applies_stagger_and_skips() {
    let base = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
    let task_a = supervision_task("a");
    let task_b = supervision_task("b");
    let mut disabled = supervision_task("disabled");
    disabled.enabled = false;
    let mut manual = supervision_task("manual");
    manual.auto_start = false;
    let override_state = TaskState {
        desired_override: Some(DesiredOverride::Stopped),
        ..TaskState::default()
    };
    let paused_state = TaskState {
        paused_reason: Some(PausedReason::CrashLoop),
        last_status: Some(LastStatus::Paused),
        ..TaskState::default()
    };
    let needs_state = TaskState {
        needs_human: NeedsHumanState {
            active: true,
            ..NeedsHumanState::default()
        },
        ..TaskState::default()
    };
    let mut backoff_task = supervision_task("backoff");
    backoff_task.restart.initial_delay_ms = 3000;
    let backoff_state = TaskState {
        last_status: Some(LastStatus::Backoff),
        next_retry_at: Some(Timestamp::parse("2026-11-02T04:41:00Z").expect("合法时间")),
        ..TaskState::default()
    };

    let inputs = [
        AutoStartInput {
            task: &task_a,
            state: None,
        },
        AutoStartInput {
            task: &task_b,
            state: None,
        },
        AutoStartInput {
            task: &disabled,
            state: None,
        },
        AutoStartInput {
            task: &manual,
            state: None,
        },
        AutoStartInput {
            task: &task_a,
            state: Some(&override_state),
        },
        AutoStartInput {
            task: &task_a,
            state: Some(&paused_state),
        },
        AutoStartInput {
            task: &task_a,
            state: Some(&needs_state),
        },
        AutoStartInput {
            task: &backoff_task,
            state: Some(&backoff_state),
        },
    ];
    let plan = plan_auto_start(&inputs, base, Duration::from_millis(100), wall_now);
    assert_eq!(plan.candidates.len(), 3, "{:?}", plan.candidates);
    assert_eq!(plan.candidates[0].task_id, "a");
    assert_eq!(plan.candidates[0].due, base);
    assert_eq!(plan.candidates[1].task_id, "b");
    assert_eq!(plan.candidates[1].due, base + Duration::from_millis(100));
    assert_eq!(plan.candidates[2].task_id, "backoff");
    assert!(plan.candidates[2].restored_backoff);
    assert!(
        plan.candidates[2].due >= base + Duration::from_secs(60),
        "{:?}",
        plan.candidates[2]
    );
    let reasons: Vec<&str> = plan.skipped.iter().map(|(_, reason)| *reason).collect();
    assert!(reasons.contains(&"disabled"));
    assert!(reasons.contains(&"auto_start_disabled"));
    assert!(reasons.contains(&"desired_override"));
    assert!(reasons.contains(&"paused"));
    assert!(reasons.contains(&"needs_human"));
}

#[test]
fn instance_and_probe_filters_are_strict() {
    assert!(instance_matches(Some("i1"), Some("i1")));
    assert!(!instance_matches(Some("i1"), Some("i2")));
    assert!(!instance_matches(None, Some("i1")));
    assert!(!instance_matches(Some("i1"), None));

    let now = Instant::now();
    assert!(probe_due(Some(now), now));
    assert!(probe_due(Some(now - Duration::from_millis(1)), now));
    assert!(!probe_due(Some(now + Duration::from_secs(1)), now));
    assert!(!probe_due(None, now));

    // backoff 未到期不 tick，到期才 tick。
    let policy = RestartPolicy::from_config(&RestartConfig::default());
    let mut machine = supervision_machine(policy);
    let base = Instant::now();
    let _ = machine.handle(RestartEvent::StartRequested, base);
    let _ = machine.handle(RestartEvent::Started, base);
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 7,
            clean: false,
        },
        base,
    );
    let Decision::Backoff { at, .. } = decision else {
        panic!("异常退出应进入 backoff：{decision:?}");
    };
    assert!(!machine_tick_due(&machine, base));
    assert!(machine_tick_due(&machine, at));
}

#[test]
fn heartbeat_monitor_detects_needs_human_and_clear() {
    let dir = tempfile::tempdir().expect("临时目录");
    let heartbeat = dir.path().join("heartbeat.hb");
    let mut task = supervision_task("hb");
    task.health.kind = HealthKind::Heartbeat;
    task.health.max_age_ms = Some(5000);
    task.health.needs_human_status = vec!["login_required".to_owned()];
    let context = InstanceContext {
        heartbeat: Some(heartbeat.clone()),
        ..InstanceContext::default()
    };
    let started = Instant::now();
    fs::write(
        &heartbeat,
        br#"{"instance":"inst-hb","seq":1,"status":"login_required"}"#,
    )
    .expect("写心跳");
    let mut monitor =
        build_health_monitor(&task, &context, 1, "inst-hb", started).expect("heartbeat 监视器");
    let verdict = monitor.check(started + Duration::from_millis(10));
    assert_eq!(
        verdict.needs_human.as_deref(),
        Some("login_required"),
        "{verdict:?}"
    );
    assert!(verdict.needs_human_changed);

    fs::write(
        &heartbeat,
        br#"{"instance":"inst-hb","seq":2,"status":"ok"}"#,
    )
    .expect("更新心跳");
    let verdict = monitor.check(started + Duration::from_millis(20));
    assert_eq!(verdict.needs_human, None);
    assert!(verdict.needs_human_changed);
}

/// 真实进程的"崩溃退出 → 退避到期重启 → 用户停止"闭环（Engine 因
/// cfg(not(test)) 不可直接单测，这里用真实 TaskRuntime + RestartMachine 驱动）。
#[test]
fn crash_exit_is_restarted_then_user_stop_ends_loop() {
    let dir = tempfile::tempdir().expect("临时目录");
    let policy = RestartPolicy::from_config(&RestartConfig {
        mode: crate::config::RestartMode::OnFailure,
        initial_delay_ms: 100,
        max_delay_ms: 100,
        multiplier: 1.0,
        jitter: 0.0,
        max_consecutive_failures: 5,
        stable_after_ms: 60_000,
        window_ms: 0,
        max_starts_in_window: 0,
        pause_on_exit_codes: Vec::new(),
    });
    let mut machine = supervision_machine(policy);
    let runtime = TaskRuntime::new(spec("crash-restart", dir.path(), &["/C", "exit", "7"]));

    let base = Instant::now();
    assert!(matches!(
        machine.handle(RestartEvent::StartRequested, base),
        Decision::StartNow { .. }
    ));
    let first = runtime.start().expect("首次启动");
    let first_id = first.instance_id.clone().expect("首次实例 id");
    wait_status(&runtime, TaskStatus::Stopped);

    let exited_at = Instant::now();
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 7,
            clean: false,
        },
        exited_at,
    );
    let Decision::Backoff { at, .. } = decision else {
        panic!("异常退出应进入 backoff：{decision:?}");
    };
    assert!(at >= exited_at + Duration::from_millis(100));

    thread::sleep(Duration::from_millis(160));
    assert!(matches!(
        machine.handle(RestartEvent::Tick, Instant::now()),
        Decision::StartNow { .. }
    ));
    let second = runtime.start().expect("自动重启");
    let second_id = second.instance_id.clone().expect("第二次实例 id");
    assert_ne!(second_id, first_id, "自动重启必须分配新实例 id");
    wait_status(&runtime, TaskStatus::Stopped);

    assert_eq!(
        machine.handle(RestartEvent::StopRequested, Instant::now()),
        Decision::Stopped {
            reason: Reason::UserStop
        }
    );
    let stopped = runtime.stop(Duration::from_secs(5)).expect("停止");
    assert_eq!(stopped.status, TaskStatus::Stopped);
    assert!(matches!(
        machine.handle(
            RestartEvent::Tick,
            Instant::now() + Duration::from_secs(120)
        ),
        Decision::Stay { .. }
    ));
}

/// 同一闭环的 Node 夹具版本（scripts/fixtures/exit-code.mjs）；无 Node 时跳过。
#[test]
fn node_fixture_crash_restarts_until_user_stop() {
    let node = std::process::Command::new("node").arg("--version").output();
    if !node.as_ref().is_ok_and(|output| output.status.success()) {
        eprintln!("跳过：未找到 node");
        return;
    }
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("../scripts/fixtures/exit-code.mjs");
    let dir = tempfile::tempdir().expect("临时目录");
    let mut task = TaskSpec::new("fixture-restart", "node", logs_params(dir.path()));
    task.args = vec![
        fixture.to_string_lossy().into_owned(),
        "7".to_owned(),
        "自动 重启".to_owned(),
    ];
    let runtime = TaskRuntime::new(task);

    let policy = RestartPolicy::from_config(&RestartConfig {
        mode: crate::config::RestartMode::OnFailure,
        initial_delay_ms: 100,
        max_delay_ms: 100,
        multiplier: 1.0,
        jitter: 0.0,
        max_consecutive_failures: 5,
        stable_after_ms: 60_000,
        window_ms: 0,
        max_starts_in_window: 0,
        pause_on_exit_codes: Vec::new(),
    });
    let mut machine = supervision_machine(policy);

    let base = Instant::now();
    assert!(matches!(
        machine.handle(RestartEvent::StartRequested, base),
        Decision::StartNow { .. }
    ));
    let first = runtime.start().expect("首次启动夹具");
    wait_status(&runtime, TaskStatus::Stopped);

    let exited_at = Instant::now();
    assert!(matches!(
        machine.handle(
            RestartEvent::Exited {
                code: 7,
                clean: false,
            },
            exited_at
        ),
        Decision::Backoff { .. }
    ));
    thread::sleep(Duration::from_millis(160));
    assert!(matches!(
        machine.handle(RestartEvent::Tick, Instant::now()),
        Decision::StartNow { .. }
    ));
    let second = runtime.start().expect("自动重启夹具");
    assert_ne!(second.instance_id, first.instance_id);
    wait_status(&runtime, TaskStatus::Stopped);

    let log = read_log(&runtime.snapshot().log_path);
    assert!(
        count(&log, b"exit-code done code=7") >= 2,
        "两次崩溃都应写入日志：{}",
        String::from_utf8_lossy(&log)
    );

    assert_eq!(
        machine.handle(RestartEvent::StopRequested, Instant::now()),
        Decision::Stopped {
            reason: Reason::UserStop
        }
    );
    assert_eq!(
        runtime.stop(Duration::from_secs(5)).expect("停止").status,
        TaskStatus::Stopped
    );
    assert!(matches!(
        machine.handle(
            RestartEvent::Tick,
            Instant::now() + Duration::from_secs(120)
        ),
        Decision::Stay { .. }
    ));
}

// ---------------------------------------------------------------- QA-M3.4b 回归

#[test]
fn readiness_plan_process_waits_startup_grace() {
    let started = Instant::now();
    let grace = Duration::from_millis(500);
    let process = readiness_plan(HealthKind::Process, started, grace);
    assert!(!process.await_first_healthy);
    assert_eq!(process.deadline, Some(started + grace));
    assert!(!readiness_deadline_reached(
        process,
        started + Duration::from_millis(499)
    ));
    assert!(readiness_deadline_reached(process, started + grace));
    assert!(!readiness_first_healthy(process, HealthState::Healthy));

    for kind in [HealthKind::Heartbeat, HealthKind::Http] {
        let plan = readiness_plan(kind, started, grace);
        assert!(plan.await_first_healthy);
        assert_eq!(plan.deadline, Some(started + grace));
        assert!(!readiness_deadline_reached(
            plan,
            started + Duration::from_millis(499)
        ));
        assert!(readiness_deadline_reached(plan, started + grace));
        assert!(!readiness_first_healthy(plan, HealthState::Starting));
        assert!(readiness_first_healthy(plan, HealthState::Healthy));
    }
}

#[test]
fn stable_after_starts_from_started_not_spawn() {
    let base = Instant::now();
    let policy = RestartPolicy {
        initial_delay: Duration::from_millis(1),
        max_delay: Duration::from_millis(1),
        stable_after: Duration::from_millis(100),
        ..RestartPolicy::from_config(&RestartConfig::default())
    };
    let mut machine = supervision_machine(policy);
    assert!(matches!(
        machine.handle(RestartEvent::StartRequested, base),
        Decision::StartNow { .. }
    ));
    // 就绪前（Starting）即使时间很久也不触发 stable。
    assert!(matches!(
        machine.handle(RestartEvent::Tick, base + Duration::from_secs(10)),
        Decision::Stay { .. }
    ));
    assert_eq!(machine.status(), RestartStatus::Starting);

    // 产生一次失败 → backoff → 重启；此时才 Started（就绪）。
    let _ = machine.handle(RestartEvent::Started, base);
    let decision = machine.handle(
        RestartEvent::Exited {
            code: 7,
            clean: false,
        },
        base + Duration::from_millis(10),
    );
    let Decision::Backoff { at, .. } = decision else {
        panic!("异常退出应进入 backoff：{decision:?}");
    };
    assert!(matches!(
        machine.handle(RestartEvent::Tick, at),
        Decision::StartNow { .. }
    ));
    let started_at = at;
    let _ = machine.handle(RestartEvent::Started, started_at);
    assert!(matches!(
        machine.handle(RestartEvent::Tick, started_at + Duration::from_millis(99)),
        Decision::Stay { .. }
    ));
    assert!(matches!(
        machine.handle(RestartEvent::Tick, started_at + Duration::from_millis(100)),
        Decision::Stable { .. }
    ));
}

#[test]
fn exit_before_started_uses_q3_classification() {
    let base = Instant::now();
    // on-failure + clean exit 在 Starting 阶段 → clean_exit → stopped。
    let mut machine = supervision_machine(RestartPolicy::from_config(&RestartConfig::default()));
    assert!(matches!(
        machine.handle(RestartEvent::StartRequested, base),
        Decision::StartNow { .. }
    ));
    assert_eq!(
        machine.handle(
            RestartEvent::Exited {
                code: 0,
                clean: true,
            },
            base + Duration::from_millis(10)
        ),
        Decision::Stopped {
            reason: Reason::CleanExit
        }
    );
    assert_eq!(machine.consecutive_failures(), 0);
}

#[test]
fn instance_accept_rejects_out_of_order_events() {
    // Running 必须严格匹配 Some(当前实例)，None 不再放行。
    assert!(running_instance_accepted(Some("i1"), Some("i1")));
    assert!(!running_instance_accepted(Some("i1"), Some("i2")));
    assert!(!running_instance_accepted(None, Some("i1")));
    assert!(!running_instance_accepted(Some("i1"), None));
    // 旧 Running 晚于 Stopped（current=None）被丢弃。
    assert!(!running_instance_accepted(None, Some("old")));

    // Starting 只在 expect_start、event 是运行时最新实例、且既非当前也非
    // previous 实例时接受。
    assert!(starting_instance_accepted(
        None,
        None,
        Some("new"),
        Some("new"),
        true
    ));
    assert!(starting_instance_accepted(
        Some("old"),
        Some("older"),
        Some("new"),
        Some("new"),
        true
    ));
    assert!(!starting_instance_accepted(
        Some("new"),
        None,
        Some("new"),
        Some("new"),
        false
    ));
    assert!(!starting_instance_accepted(
        Some("old"),
        None,
        Some("old"),
        Some("old"),
        true
    ));
    assert!(!starting_instance_accepted(
        None,
        None,
        Some("new"),
        Some("new"),
        false
    ));
    assert!(!starting_instance_accepted(None, None, None, None, true));
}

#[test]
fn starting_instance_accepted_drops_stale_event_when_current_is_none() {
    // N1：旧 Starting 积压，手动 Start 已把 current 清成 None；运行时的最新
    // 实例是 None（新 runtime 尚未 start）或已经是新实例 —— 两种情况都必须
    // 丢弃旧事件，否则会把旧 id 写回 current 并吞掉新实例的 Starting。
    assert!(!starting_instance_accepted(
        None,
        None,
        None,
        Some("old"),
        true
    ));
    assert!(!starting_instance_accepted(
        None,
        None,
        Some("new"),
        Some("old"),
        true
    ));
}

#[test]
fn starting_instance_accepted_drops_stale_event_after_stopped() {
    // Stopped/StartFailed 已把旧实例 id 记入 previous；即使运行时快照仍保留
    // 该 id（latest == event），迟到 Starting 也不能复活旧实例。
    assert!(!starting_instance_accepted(
        None,
        Some("old"),
        Some("old"),
        Some("old"),
        true
    ));
    // 旧 id 与 previous 相同但 latest 已前移，同样拒绝。
    assert!(!starting_instance_accepted(
        None,
        Some("old"),
        Some("new"),
        Some("old"),
        true
    ));
}

#[test]
fn starting_instance_accepted_accepts_new_instance_after_old_one_started() {
    // 旧 Starting 已先到（current=old），新 runtime 已建立并发出新 Starting：
    // 新事件必须被接受，否则新进程没有 monitor、状态机卡在 Starting。
    assert!(starting_instance_accepted(
        Some("old"),
        Some("old"),
        Some("new"),
        Some("new"),
        true
    ));
    // 首启：current/previous 均为空，latest 与事件一致。
    assert!(starting_instance_accepted(
        None,
        None,
        Some("first"),
        Some("first"),
        true
    ));
}

#[test]
fn starting_instance_accepted_ignores_duplicate_starting() {
    // 同一实例重复 Starting 直接忽略，避免清空已建立的 monitor。
    assert!(!starting_instance_accepted(
        Some("i1"),
        None,
        Some("i1"),
        Some("i1"),
        true
    ));
    // 重复事件即使 expect_start 已被清掉也必须拒绝。
    assert!(!starting_instance_accepted(
        Some("i1"),
        None,
        Some("i1"),
        Some("i1"),
        false
    ));
}

#[test]
fn reconcile_action_covers_delete_disable_and_keep() {
    use super::supervision::ConfigReconcile;
    assert_eq!(
        reconcile_action(true, true, Some(TaskStatus::Running)),
        ConfigReconcile::Keep
    );
    assert_eq!(
        reconcile_action(true, false, Some(TaskStatus::Running)),
        ConfigReconcile::StopAndKeep
    );
    assert_eq!(
        reconcile_action(false, true, Some(TaskStatus::Running)),
        ConfigReconcile::StopAndRemove
    );
    // 已停止的删除/失效任务仍需清理资源（B3）。
    assert_eq!(
        reconcile_action(false, true, Some(TaskStatus::Stopped)),
        ConfigReconcile::StopAndRemove
    );
    assert_eq!(
        reconcile_action(true, false, Some(TaskStatus::Stopped)),
        ConfigReconcile::Keep
    );
    assert_eq!(
        reconcile_action(false, true, None),
        ConfigReconcile::StopAndRemove
    );
    assert_eq!(reconcile_action(true, true, None), ConfigReconcile::Keep);
}

#[test]
fn stop_retry_schedule_is_bounded_and_log_limited() {
    assert_eq!(stop_retry_delay(0), Duration::from_secs(1));
    assert_eq!(stop_retry_delay(1), Duration::from_secs(1));
    assert_eq!(stop_retry_delay(2), Duration::from_secs(2));
    assert_eq!(stop_retry_delay(3), Duration::from_secs(4));
    assert_eq!(stop_retry_delay(10), Duration::from_secs(4));
    assert!(should_log_stop_retry(1));
    assert!(should_log_stop_retry(2));
    assert!(!should_log_stop_retry(3));
    assert!(should_log_stop_retry(4));
    assert!(!should_log_stop_retry(5));
    assert!(should_log_stop_retry(8));
}

#[test]
fn needs_human_since_is_preserved_and_sync_is_idempotent() {
    let mono_now = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
    let policy = RestartPolicy::from_config(&RestartConfig::default());
    let mut machine = supervision_machine(policy);
    let base = Instant::now();
    let _ = machine.handle(RestartEvent::StartRequested, base);
    let _ = machine.handle(RestartEvent::Started, base);
    let _ = machine.handle(
        RestartEvent::NeedsHumanObserved {
            source: "heartbeat".to_owned(),
            message: "login_required".to_owned(),
        },
        base + Duration::from_secs(1),
    );
    let snapshot = machine.snapshot(base + Duration::from_secs(1));
    let mut state = TaskState::default();
    assert!(sync_state(&mut state, &snapshot, mono_now, wall_now));
    let since = state.needs_human.since.expect("应写入 since");
    assert_eq!(state.needs_human.source.as_deref(), Some("heartbeat"));

    // 同一 active/source/message 再同步：保留 since，且不再报告 changed。
    assert!(!sync_state(
        &mut state,
        &snapshot,
        mono_now,
        Timestamp::parse("2026-11-02T04:41:00Z").expect("合法时间")
    ));
    assert_eq!(state.needs_human.since, Some(since));
}

#[test]
fn paused_reason_stable_enums_are_not_cleared() {
    let mono_now = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
    for reason in [
        PausedReason::UserPause,
        PausedReason::ElevationUnavailable,
        PausedReason::Other("future_reason".to_owned()),
    ] {
        let state = TaskState {
            paused_reason: Some(reason.clone()),
            desired_override: Some(DesiredOverride::Paused),
            ..TaskState::default()
        };
        let seed = restore_seed(
            &supervision_task("paused"),
            Some(&state),
            mono_now,
            wall_now,
        );
        let machine = RestartMachine::restore(
            RestartPolicy::from_config(&RestartConfig::default()),
            FixedRandom(0.5),
            seed,
        );
        assert_eq!(machine.status(), RestartStatus::Paused);
        let mut synced = state.clone();
        sync_state(&mut synced, &machine.snapshot(mono_now), mono_now, wall_now);
        assert_eq!(
            synced.paused_reason,
            Some(reason.clone()),
            "稳定 paused_reason 不得被清成 None"
        );
        // 第二次同步无变化。
        assert!(!sync_state(
            &mut synced,
            &machine.snapshot(mono_now),
            mono_now,
            wall_now
        ));
    }
}

#[test]
fn restored_needs_human_since_is_preserved_on_sync() {
    let mono_now = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
    let original_since = Timestamp::parse("2026-11-02T03:00:00Z").expect("合法时间");
    let state = TaskState {
        needs_human: NeedsHumanState {
            active: true,
            source: Some("heartbeat".to_owned()),
            message: Some("login_required".to_owned()),
            since: Some(original_since),
        },
        ..TaskState::default()
    };
    let seed = restore_seed(
        &supervision_task("restored-human"),
        Some(&state),
        mono_now,
        wall_now,
    );
    let machine = RestartMachine::restore(
        RestartPolicy::from_config(&RestartConfig::default()),
        FixedRandom(0.5),
        seed,
    );
    let mut synced = state.clone();
    sync_state(&mut synced, &machine.snapshot(mono_now), mono_now, wall_now);
    assert_eq!(synced.needs_human.since, Some(original_since));
    assert!(synced.needs_human.active);
    // 状态机没有新迁移：第二次同步不产生变更。
    let snapshot = machine.snapshot(mono_now);
    let mut again = synced.clone();
    assert!(!sync_state(&mut again, &snapshot, mono_now, wall_now));
    assert_eq!(again.needs_human.since, Some(original_since));
}

#[test]
fn restart_success_counter_only_counts_ready_restarts() {
    let mut state = TaskState::default();
    assert!(!count_restart_success(&mut state, false));
    assert_eq!(state.total_restarts, 0);
    assert!(count_restart_success(&mut state, true));
    assert_eq!(state.total_restarts, 1);
    assert!(count_restart_success(&mut state, true));
    assert_eq!(state.total_restarts, 2);
}

#[test]
fn paused_reason_alone_suppresses_auto_start() {
    let now = Instant::now();
    let wall_now = Timestamp::parse("2026-11-02T04:40:00Z").expect("合法时间");
    let task = supervision_task("paused-only");
    for reason in [
        PausedReason::UserPause,
        PausedReason::ElevationUnavailable,
        PausedReason::Other("future_reason".to_owned()),
    ] {
        let state = TaskState {
            paused_reason: Some(reason),
            ..TaskState::default()
        };
        let plan = plan_auto_start(
            &[AutoStartInput {
                task: &task,
                state: Some(&state),
            }],
            now,
            Duration::from_millis(100),
            wall_now,
        );
        assert!(plan.candidates.is_empty(), "paused 不应自动启动");
        assert_eq!(plan.skipped, vec![("paused-only".to_owned(), "paused")]);
    }
}
