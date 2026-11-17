//! M2-6：运行时集成测试（`rook_lib::runtime` 公开 API + 真实夹具）。
//!
//! 覆盖：
//! - 自然退出 0 / 崩溃退出 7（日志落盘、退出原因、PID 清理）；
//! - `start → running → stop` 生命周期与幂等停止；
//! - 大输出 + 小轮转阈值（日志落盘与归档上限）；
//! - 端口冲突在 spawn 前被拒绝；
//! - CJK/空格/引号/空参数、工作目录、环境变量透传；
//! - 心跳文件（PID/序号/JSON）；
//! - 孤儿检测：根进程退出但孙进程长驻 → 强制清树；drop 运行时 → 整棵树消失。
//!
//! 约定：所有等待都有超时；没有 Node 时打印原因并跳过；只按 PID 检查存活，
//! 绝不按进程名结束无关进程。夹具路径见 `scripts/fixtures/`。

#![cfg(windows)]

mod support;

use std::ffi::OsString;
use std::fs;
use std::path::Path;
use std::time::Duration;

use rook_lib::config::Rotation;
use rook_lib::logs::LogParams;
use rook_lib::runtime::{ExitReason, RuntimeStage, TaskRuntime, TaskSpec, TaskStatus};
use support::*;

/// 构造"用 Node 跑夹具"的任务。
fn fixture_task(id: &str, node: &Path, logs_dir: &Path, args: Vec<String>) -> TaskSpec {
    let mut task = TaskSpec::new(id, node.as_os_str().to_owned(), logs_params(logs_dir));
    task.args = args;
    task
}

/// 日志目录里的归档文件数（`current.log` 除外）。
fn count_archives(dir: &Path) -> usize {
    fs::read_dir(dir)
        .expect("读取日志目录")
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            name.ends_with(".log") && name != "current.log"
        })
        .count()
}

/// 读取心跳 JSON（文件可能正在被 rename 替换，解析失败返回 `None`）。
fn read_heartbeat(path: &Path) -> Option<serde_json::Value> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

// ---------------------------------------------------------------- 退出语义

#[test]
fn fixture_exit_zero_logs_and_stops_naturally() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(fixture_task(
        "it-exit-0",
        &node,
        dir.path(),
        vec![fixture_arg("exit-code.mjs"), "0".into(), "零 号".into()],
    ));

    runtime.start().expect("启动 exit-code 夹具");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.status, TaskStatus::Stopped);
    assert_eq!(snapshot.exit_code, Some(0));
    assert_eq!(snapshot.exit_reason, Some(ExitReason::Natural));
    assert_eq!(snapshot.pid, None, "停止后不应残留 PID");
    assert!(
        snapshot.last_error.is_none(),
        "自然退出不应有 last_error：{:?}",
        snapshot.last_error
    );
    assert!(
        snapshot.logs.written_bytes > 0,
        "日志应已落盘：{:?}",
        snapshot.logs
    );
    assert_eq!(
        snapshot.logs.dropped_bytes, 0,
        "小输出不应丢字节：{:?}",
        snapshot.logs
    );

    let log = read_log(&snapshot.log_path);
    let text = String::from_utf8_lossy(&log);
    assert!(
        contains(&log, "exit-code start marker=零 号".as_bytes()),
        "stdout 应保留 CJK/空格参数：{text}"
    );
    assert!(contains(&log, b"exit-code done code=0"), "{text}");
    assert!(
        contains(&log, b"exit-code stderr marker="),
        "stderr 应合并进同一份日志：{text}"
    );
}

#[test]
fn fixture_exit_seven_records_crash_code() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(fixture_task(
        "it-exit-7",
        &node,
        dir.path(),
        vec![fixture_arg("exit-code.mjs"), "7".into()],
    ));

    runtime.start().expect("启动 exit-code 夹具");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.exit_code, Some(7), "崩溃退出码应原样记录");
    assert_eq!(snapshot.exit_reason, Some(ExitReason::Natural));
    assert_eq!(snapshot.pid, None);
    assert!(snapshot.last_error.is_none());

    let log = read_log(&snapshot.log_path);
    assert!(contains(&log, b"exit-code done code=7"));
}

// ---------------------------------------------------------------- 生命周期

#[test]
fn start_running_stop_flushes_logs_and_is_idempotent() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let runtime = TaskRuntime::new(fixture_task(
        "it-lifecycle",
        &node,
        dir.path(),
        vec![
            fixture_arg("long-run.mjs"),
            "60000".into(),
            "生命周期".into(),
        ],
    ));

    let started = runtime.start().expect("启动长驻夹具");
    assert_eq!(started.status, TaskStatus::Running, "启动后应直接 running");
    wait_status(&runtime, TaskStatus::Running);

    let running = runtime.snapshot();
    assert!(
        running.pid.is_some_and(|pid| pid > 0),
        "运行中应有根进程 PID"
    );
    assert!(running.instance_id.is_some(), "运行中应有实例 id");
    assert!(running.started_at.is_some(), "运行中应有启动时间");

    // 先等首行与至少一次 tick，确保停止前的日志不会丢。
    wait_log(&running.log_path, "long-run ready", |bytes| {
        contains(bytes, b"long-run ready")
    });
    wait_log(&running.log_path, "long-run tick", |bytes| {
        contains(bytes, b"long-run tick")
    });

    let stopped = runtime.stop(Duration::from_secs(10)).expect("停止长驻夹具");
    assert_eq!(stopped.status, TaskStatus::Stopped);
    assert_eq!(stopped.pid, None);
    assert_eq!(stopped.exit_reason, Some(ExitReason::UserStop));
    // 默认 graceful：脚本没有注册 SIGBREAK，控制台默认处理程序以
    // 0xC000013A 结束；成功判据是根退出/树空，不看退出码。
    assert!(stopped.exit_code.is_some(), "停止后应记录退出码");
    assert!(stopped.last_error.is_none());
    assert!(
        stopped.logs.written_bytes > 0,
        "停止后统计应为最终值：{:?}",
        stopped.logs
    );

    let log = read_log(&stopped.log_path);
    assert!(
        contains(&log, b"long-run ready"),
        "停止前写入的日志不应丢失：{}",
        String::from_utf8_lossy(&log)
    );

    // 幂等：再次 stop 立即返回同一结果，不产生新实例。
    let again = runtime.stop(Duration::from_secs(2)).expect("重复停止");
    assert_eq!(again.status, TaskStatus::Stopped);
    assert_eq!(again.exit_code, stopped.exit_code);
    assert_eq!(again.exit_reason, Some(ExitReason::UserStop));
    assert_eq!(again.instance_id, stopped.instance_id);
}

// ---------------------------------------------------------------- 日志轮转

#[test]
fn bulk_output_rotates_with_small_threshold() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");

    // 故意把阈值调到 4KB、只保留 3 个文件：约 160KB 输出会触发多次轮转。
    let mut params: LogParams = logs_params(dir.path());
    params.rotation = Rotation::Size;
    params.max_file_bytes = 4 * 1024;
    params.max_files = 3;

    let mut task = TaskSpec::new("it-bulk", node.as_os_str().to_owned(), params);
    task.args = vec![fixture_arg("bulk-output.mjs"), "2000".into(), "64".into()];

    let runtime = TaskRuntime::new(task);
    runtime.start().expect("启动大输出夹具");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(
        snapshot.exit_code,
        Some(0),
        "夹具应写完正常退出（背压不应阻塞）：{:?}",
        snapshot.logs
    );
    assert!(
        snapshot.logs.written_bytes > 100 * 1024,
        "大部分字节应落盘：{:?}",
        snapshot.logs
    );
    assert!(
        snapshot.logs.rotations >= 1,
        "4KB 阈值下应至少轮转一次：{:?}",
        snapshot.logs
    );
    assert_eq!(snapshot.logs.io_errors, 0, "轮转不应产生 IO 错误");
    assert!(snapshot.log_path.exists(), "current.log 永不删除");

    let archives = count_archives(dir.path());
    assert!(
        (1..=2).contains(&archives),
        "max_files=3 时归档最多 2 个，实际 {archives}"
    );
    // 最后一次写入可能恰好触发轮转：数据已完整归档，current.log 可以为空。
    // 只要 current 或任一归档仍有内容，就说明轮转没有丢数据。
    let current_bytes = fs::metadata(&snapshot.log_path)
        .expect("current.log 元数据")
        .len();
    assert!(
        current_bytes > 0 || archives > 0,
        "轮转后 current.log 与归档不能同时为空"
    );
}

// ---------------------------------------------------------------- 端口冲突

#[test]
fn occupied_port_is_rejected_before_spawn() {
    let Some(node) = node() else {
        return;
    };
    let holder_dir = tempfile::tempdir().expect("监听夹具临时目录");
    let port = free_port();

    let holder = TaskRuntime::new(fixture_task(
        "it-port-holder",
        &node,
        holder_dir.path(),
        vec![
            fixture_arg("port-listen.mjs"),
            port.to_string(),
            "60000".into(),
        ],
    ));
    holder.start().expect("启动端口监听夹具");
    wait_status(&holder, TaskStatus::Running);
    let holder_log = holder.snapshot().log_path;
    wait_log(&holder_log, "listening", |bytes| {
        contains(bytes, format!("listening 127.0.0.1:{port}").as_bytes())
    });

    // 第二个任务用同一端口；若端口预检失效就会真的创建进程并写日志。
    let blocked_dir = tempfile::tempdir().expect("被阻止任务临时目录");
    let mut blocked = fixture_task(
        "it-port-conflict",
        &node,
        blocked_dir.path(),
        vec![fixture_arg("exit-code.mjs"), "0".into()],
    );
    blocked.ports = vec![port];

    let runtime = TaskRuntime::new(blocked);
    let error = runtime.start().expect_err("端口被占用时必须拒绝启动");
    assert_eq!(error.stage(), RuntimeStage::PortCheck);
    assert!(
        error.message().contains(&port.to_string()),
        "错误应包含端口号：{error}"
    );

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.status, TaskStatus::Stopped);
    assert_eq!(snapshot.pid, None);
    assert!(
        snapshot
            .last_error
            .as_ref()
            .is_some_and(|last| last.stage() == RuntimeStage::PortCheck),
        "last_error 应记录端口预检阶段：{:?}",
        snapshot.last_error
    );
    assert!(
        !snapshot.log_path.exists(),
        "端口冲突时不得创建进程/日志：{}",
        snapshot.log_path.display()
    );

    holder.stop(Duration::from_secs(10)).expect("停止监听夹具");
}

// ---------------------------------------------------------------- 参数/目录

#[test]
fn cjk_space_args_working_dir_and_env_reach_fixture() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");

    // 工作目录与日志目录都带 CJK/空格，验证路径传递不依赖 ANSI 代码页。
    let work = dir.path().join("工作 目录 with space");
    fs::create_dir_all(&work).expect("创建含 CJK/空格的工作目录");
    let log_dir = dir.path().join("日志 目录");

    let mut task = fixture_task(
        "it-args",
        &node,
        &log_dir,
        vec![
            fixture_arg("echo-args.mjs"),
            "中文 参数".into(),
            "a b\tc".into(),
            "引号\"和\\反斜杠".into(),
            "结尾空格 ".into(),
            String::new(),
        ],
    );
    task.working_dir = Some(work.clone());
    task.env = vec![(OsString::from("CS_FIXTURE_MARK"), OsString::from("环境 值"))];

    let runtime = TaskRuntime::new(task);
    runtime.start().expect("启动参数回显夹具");
    wait_status(&runtime, TaskStatus::Stopped);

    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.exit_code, Some(0));

    let log = read_log(&snapshot.log_path);
    let text = String::from_utf8(log).expect("夹具输出应为 UTF-8");
    assert!(text.contains("argc=5"), "参数个数不对：\n{text}");
    assert!(text.contains("argv=0:中文 参数"), "{text}");
    assert!(text.contains("argv=1:a b\tc"), "制表符应保留：{text}");
    assert!(
        text.contains("argv=2:引号\"和\\反斜杠"),
        "引号/反斜杠应原样到达：{text}"
    );
    assert!(
        text.contains("argv=3:结尾空格 \n"),
        "结尾空格不应被裁剪：{text}"
    );
    assert!(text.contains("argv=4:\n"), "空参数应保留为空参数：{text}");
    assert!(
        text.contains("cwd=") && text.contains("工作 目录 with space"),
        "工作目录应为配置值：{text}"
    );
    assert!(
        text.contains("env:CS_FIXTURE_MARK=环境 值"),
        "环境变量应覆盖生效：{text}"
    );
}

// ---------------------------------------------------------------- 心跳

#[test]
fn heartbeat_file_tracks_running_pid() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let beat = dir.path().join("心跳 文件.json");

    let runtime = TaskRuntime::new(fixture_task(
        "it-heartbeat",
        &node,
        dir.path(),
        vec![
            fixture_arg("heartbeat.mjs"),
            beat.display().to_string(),
            "100".into(),
            "0".into(),
        ],
    ));

    runtime.start().expect("启动心跳夹具");
    wait_status(&runtime, TaskStatus::Running);

    // 心跳是 JSON（不是纯 PID 文件），等它出现合法内容后再取 PID。
    let mut beat_pid = None;
    wait_for(&format!("心跳文件 {}", beat.display()), || {
        beat_pid = read_heartbeat(&beat)
            .and_then(|value| value.get("pid").and_then(serde_json::Value::as_u64))
            .and_then(|pid| u32::try_from(pid).ok());
        beat_pid.is_some()
    });
    let beat_pid = beat_pid.expect("wait_for 返回时心跳应含 PID");
    let running = runtime.snapshot();
    assert_eq!(Some(beat_pid), running.pid, "心跳 PID 应与运行时根进程一致");

    // 序号持续增长，证明心跳在运行期间不断刷新。
    wait_for("心跳序号 >= 3", || {
        read_heartbeat(&beat)
            .and_then(|value| value.get("seq").and_then(serde_json::Value::as_u64))
            .is_some_and(|seq| seq >= 3)
    });

    let value = read_heartbeat(&beat).expect("心跳应是合法 JSON");
    assert_eq!(value["status"], "ok");
    assert_eq!(value["pid"].as_u64(), Some(u64::from(beat_pid)));

    let stopped = runtime.stop(Duration::from_secs(10)).expect("停止心跳夹具");
    assert_eq!(stopped.exit_reason, Some(ExitReason::UserStop));
}

// ---------------------------------------------------------------- 孤儿检测

#[test]
fn root_exit_with_live_grandchild_forces_tree_kill() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let pid_file = dir.path().join("grandchild.pid");

    // 启动器用 cmd：`start /b` 派生的孙进程不随启动器退出而结束，
    // 因此根进程退出后 Job 里仍有活动进程（这正是要强制清树的场景）。
    let mut task = TaskSpec::new("it-tree-launcher", "cmd.exe", logs_params(dir.path()));
    task.args = vec![
        "/c".into(),
        fixture_arg("spawn-child.cmd"),
        node.display().to_string(),
        pid_file.display().to_string(),
        "60000".into(),
    ];

    let runtime = TaskRuntime::new(task);
    runtime.start().expect("启动 cmd 启动器夹具");
    let grandchild = wait_pid_file(&pid_file);
    assert!(
        process_alive(grandchild),
        "孙进程 {grandchild} 应已启动（PID 文件已写入）"
    );

    // 根进程（cmd）退出后，监控线程等 tree_wait（默认 2s）再强制清树。
    wait_status(&runtime, TaskStatus::Stopped);
    let snapshot = runtime.snapshot();
    assert_eq!(snapshot.exit_code, Some(0), "启动器应正常退出");
    assert_eq!(
        snapshot.tree_empty,
        Some(true),
        "收尾后 Job 内不应有活动进程：{:?}",
        snapshot
    );
    assert!(
        snapshot.forced_kill,
        "根进程退出但孙进程仍在，应记录强制清树：{:?}",
        snapshot
    );

    wait_gone(grandchild, Duration::from_secs(5), "根进程退出后的孙进程");
}

#[test]
fn dropping_runtime_reaps_grandchild_tree() {
    let Some(node) = node() else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let pid_file = dir.path().join("grandchild.pid");

    let runtime = TaskRuntime::new(fixture_task(
        "it-tree-drop",
        &node,
        dir.path(),
        vec![
            fixture_arg("spawn-child.mjs"),
            pid_file.display().to_string(),
            "60000".into(),
        ],
    ));

    runtime.start().expect("启动派生孙进程夹具");
    wait_status(&runtime, TaskStatus::Running);

    let grandchild = wait_pid_file(&pid_file);
    let parent = runtime.snapshot().pid.expect("运行中应有根进程 PID");
    assert!(process_alive(parent), "根进程 {parent} 应存活");
    assert!(process_alive(grandchild), "孙进程 {grandchild} 应存活");

    // Drop 等价于 stop(DROP_TIMEOUT)：终止整棵树后关闭 Job。
    drop(runtime);

    wait_gone(parent, Duration::from_secs(5), "drop 后的根进程");
    wait_gone(grandchild, Duration::from_secs(5), "drop 后的孙进程");
}
