//! 进程子系统测试：参数引号/路径/环境块单测 + 真实 Windows 进程集成测试。
//!
//! 集成测试真的启动 `cmd.exe`，因此只在 Windows 上编译（模块本身即 `cfg(windows)`）。

use std::ffi::{OsStr, OsString};
use std::io::Read;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

use super::args::{build_command_line, quote_arg};
use super::env::build_environment_block;
use super::path::resolve_program;
use super::ports::{
    IpFamily, PortCheckError, PortCheckStage, RawListener, check_tcp_ports, check_tcp_ports_with,
};
use super::{
    Child, ExitEvent, ExitWatcher, JobMode, PipeReader, SpawnRequest, Stage, StopMode,
    WatchOptions, spawn, spawn_with,
};

// ---------------------------------------------------------------- 参数引号

#[test]
fn quote_keeps_plain_argument() {
    assert_eq!(quote_arg("plain"), "plain");
    assert_eq!(
        quote_arg("C:\\Windows\\System32\\cmd.exe"),
        "C:\\Windows\\System32\\cmd.exe"
    );
}

#[test]
fn quote_empty_argument() {
    assert_eq!(quote_arg(""), "\"\"");
}

#[test]
fn quote_argument_with_spaces_and_tabs() {
    assert_eq!(quote_arg("a b"), "\"a b\"");
    assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    assert_eq!(quote_arg(" a "), "\" a \"");
}

#[test]
fn quote_embedded_quote() {
    assert_eq!(quote_arg("a\"b"), "\"a\\\"b\"");
}

#[test]
fn quote_trailing_backslash_only_when_quoted() {
    // 不需要引号时反斜杠原样保留。
    assert_eq!(quote_arg("a\\b"), "a\\b");
    assert_eq!(quote_arg("C:\\dir\\"), "C:\\dir\\");

    // 需要引号时，闭引号前的反斜杠加倍。
    assert_eq!(quote_arg("a b\\"), "\"a b\\\\\"");
    assert_eq!(
        quote_arg("C:\\dir with space\\"),
        "\"C:\\dir with space\\\\\""
    );
}

#[test]
fn quote_backslashes_before_quote() {
    // 输入 a + 2 个反斜杠 + " + b：2n+1 = 5 个反斜杠 + 字面引号。
    let input = "a\\\\\"b";
    let expected = format!("\"a{}\"b\"", "\\".repeat(5));
    assert_eq!(quote_arg(input), expected);
}

#[test]
fn quote_cjk_argument() {
    assert_eq!(quote_arg("中文 参数"), "\"中文 参数\"");
    assert_eq!(quote_arg("中文参数"), "中文参数");
}

#[test]
fn build_command_line_quotes_program_and_args() {
    let program = PathBuf::from("C:\\Program Files\\Demo\\tool.exe");
    let args = vec!["--dir".to_owned(), "C:\\a b\\".to_owned(), String::new()];
    let wide = build_command_line(program.as_os_str(), &args);
    assert_eq!(*wide.last().unwrap(), 0);
    let line = String::from_utf16(&wide[..wide.len() - 1]).unwrap();
    assert_eq!(
        line,
        "\"C:\\Program Files\\Demo\\tool.exe\" --dir \"C:\\a b\\\\\" \"\""
    );
}

// ---------------------------------------------------------------- PATH 解析

#[test]
fn resolve_bare_name_uses_path_and_pathext() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("mytool.EXE");
    std::fs::write(&exe, b"").unwrap();

    let path_var = OsString::from(dir.path().as_os_str());
    let found = resolve_program(
        OsStr::new("mytool"),
        None,
        Some(path_var.as_os_str()),
        Some(OsStr::new(".COM;.EXE;.BAT")),
    )
    .unwrap();
    assert_eq!(found, exe);
}

#[test]
fn resolve_absolute_path_with_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let spaced = dir.path().join("dir with space");
    std::fs::create_dir(&spaced).unwrap();
    let exe = spaced.join("tool.exe");
    std::fs::write(&exe, b"").unwrap();

    let found = resolve_program(exe.as_os_str(), None, None, None).unwrap();
    assert_eq!(found, exe);
}

#[test]
fn resolve_relative_path_against_base_dir() {
    let dir = tempfile::tempdir().unwrap();
    let sub = dir.path().join("sub");
    std::fs::create_dir(&sub).unwrap();
    let exe = sub.join("tool.exe");
    std::fs::write(&exe, b"").unwrap();

    let found = resolve_program(OsStr::new("sub\\tool.exe"), Some(dir.path()), None, None).unwrap();
    assert_eq!(found, exe);
}

#[test]
fn resolve_quoted_path_entry() {
    let dir = tempfile::tempdir().unwrap();
    let exe = dir.path().join("quoted.exe");
    std::fs::write(&exe, b"").unwrap();

    let path_var = OsString::from(format!("\"{}\"", dir.path().display()));
    let found = resolve_program(
        OsStr::new("quoted"),
        None,
        Some(path_var.as_os_str()),
        Some(OsStr::new(".EXE")),
    )
    .unwrap();
    // PATHEXT 里的扩展名大小写可能与磁盘上的文件名不同（Windows 文件系统不区分大小写）。
    assert!(
        found
            .as_os_str()
            .to_string_lossy()
            .eq_ignore_ascii_case(&exe.to_string_lossy())
    );
}

#[test]
fn resolve_missing_command_reports_stage_and_code() {
    let error = resolve_program(
        OsStr::new("rook-definitely-missing"),
        None,
        Some(OsStr::new("")),
        None,
    )
    .unwrap_err();
    assert_eq!(error.stage(), Stage::ResolveProgram);
    assert_eq!(error.code(), 2);
    assert!(error.to_string().contains("在 PATH 中找不到命令"));
}

// ---------------------------------------------------------------- 环境块

#[test]
fn env_block_merges_case_insensitively() {
    let overrides = vec![
        (OsString::from("Rook_Test_Key"), OsString::from("first")),
        (OsString::from("rook_test_key"), OsString::from("second")),
    ];
    let block = build_environment_block(&overrides);
    assert_eq!(block.last(), Some(&0));
    assert_eq!(block[block.len() - 2], 0, "环境块必须以双 NUL 结束");

    let entries = parse_env_block(&block);
    let matches: Vec<&str> = entries
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case("rook_test_key"))
        .map(|(_, value)| value.as_str())
        .collect();
    assert_eq!(matches, vec!["second"]);
}

#[test]
fn env_block_is_sorted_case_insensitively() {
    let block = build_environment_block(&[]);
    let entries = parse_env_block(&block);
    let keys: Vec<String> = entries
        .iter()
        .map(|(key, _)| key.to_ascii_uppercase())
        .collect();
    let mut sorted = keys.clone();
    sorted.sort();
    assert_eq!(keys, sorted);
}

#[test]
fn env_block_preserves_unicode_values() {
    let overrides = vec![(OsString::from("Rook_Unicode"), OsString::from("中文 值"))];
    let block = build_environment_block(&overrides);
    let entries = parse_env_block(&block);
    let value = entries
        .iter()
        .find(|(key, _)| key == "Rook_Unicode")
        .map(|(_, value)| value.clone());
    assert_eq!(value.as_deref(), Some("中文 值"));
}

fn parse_env_block(block: &[u16]) -> Vec<(String, String)> {
    let mut entries = Vec::new();
    let mut start = 0usize;
    for (index, unit) in block.iter().enumerate() {
        if *unit != 0 {
            continue;
        }
        if index == start {
            break;
        }
        let entry = String::from_utf16(&block[start..index]).unwrap();
        if let Some((key, value)) = entry.split_once('=') {
            entries.push((key.to_owned(), value.to_owned()));
        }
        start = index + 1;
    }
    entries
}

// ---------------------------------------------------------------- 真实进程

fn cmd(args: &[&str]) -> SpawnRequest {
    SpawnRequest {
        program: OsString::from("cmd.exe"),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        ..SpawnRequest::default()
    }
}

#[test]
fn spawn_returns_exit_code() {
    let mut child = spawn(cmd(&["/C", "exit", "7"])).expect("启动 cmd.exe");
    assert!(child.pid() > 0);
    assert!(
        !child.job_handle_is_inheritable(),
        "Job 句柄不能被子进程继承"
    );
    assert!(
        child.job_mode().is_atomic(),
        "正常路径应使用 PROC_THREAD_ATTRIBUTE_JOB_LIST，实际为 {:?}",
        child.job_mode()
    );

    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待进程")
        .expect("cmd 应已退出");
    assert_eq!(status.code, 7);
    assert!(!status.success());
}

#[test]
fn fallback_assigns_job_after_suspend() {
    // 强制走 CREATE_SUSPENDED → AssignProcessToJobObject → ResumeThread。
    let mut child = spawn_with(cmd(&["/C", "exit", "9"]), false).expect("降级启动 cmd.exe");
    assert!(matches!(child.job_mode(), JobMode::Fallback { .. }));
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待进程")
        .expect("cmd 应已退出");
    assert_eq!(status.code, 9);

    let mut drained = false;
    for _ in 0..40 {
        if child.active_processes().expect("查询 Job") == 0 {
            drained = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(drained, "降级路径也必须把进程放进 Job");
}

#[test]
fn spawn_captures_stdout() {
    let mut child = spawn(cmd(&["/C", "echo", "hello"])).expect("启动 cmd.exe");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待进程")
        .expect("cmd 应已退出");
    assert_eq!(status.code, 0);

    let mut stdout = child.take_stdout().expect("stdout 读端").into_file();
    let mut text = String::new();
    stdout.read_to_string(&mut text).expect("读取 stdout");
    assert!(text.contains("hello"), "stdout = {text:?}");
}

#[test]
fn spawn_applies_environment_overrides() {
    let mut request = cmd(&["/C", "echo %CAMPUS_SENTRY_TEST_ENV%"]);
    request.env = vec![(
        OsString::from("CAMPUS_SENTRY_TEST_ENV"),
        OsString::from("env-value-ok"),
    )];

    let mut child = spawn(request).expect("启动 cmd.exe");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待进程")
        .expect("cmd 应已退出");
    assert!(status.success());

    let mut stdout = child.take_stdout().expect("stdout 读端").into_file();
    let mut text = String::new();
    stdout.read_to_string(&mut text).expect("读取 stdout");
    assert!(text.contains("env-value-ok"), "stdout = {text:?}");
}

#[test]
fn spawn_applies_working_directory() {
    let dir = tempfile::tempdir().expect("临时目录");
    let spaced = dir.path().join("dir with space");
    std::fs::create_dir(&spaced).expect("创建目录");

    let mut request = cmd(&["/C", "cd"]);
    request.working_dir = Some(spaced.clone());
    let mut child = spawn(request).expect("启动 cmd.exe");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待进程")
        .expect("cmd 应已退出");
    assert!(status.success());

    let mut stdout = child.take_stdout().expect("stdout 读端").into_file();
    let mut text = String::new();
    stdout.read_to_string(&mut text).expect("读取 stdout");
    assert!(
        text.trim()
            .eq_ignore_ascii_case(&spaced.display().to_string()),
        "cd = {text:?}, expected {:?}",
        spaced
    );
}

#[test]
fn wait_for_exit_returns_none_on_timeout() {
    let mut child = spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe");
    let timed_out = child
        .wait_for_exit(Duration::from_millis(50))
        .expect("等待超时不应报错");
    assert!(timed_out.is_none(), "长进程不应在 50ms 内退出");
    assert!(child.exit_status().is_none());

    child.terminate().expect("清理进程树");
    assert!(
        child
            .wait_for_exit(Duration::from_secs(5))
            .expect("等待退出")
            .is_some(),
        "terminate 后根进程应退出"
    );
}

#[test]
fn terminate_stops_long_running_process() {
    let mut child = spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe");
    std::thread::sleep(Duration::from_millis(300));
    assert!(child.exit_status().is_none(), "长进程不应已退出");

    child.terminate().expect("TerminateJobObject");
    let status = child
        .wait_for_exit(Duration::from_secs(5))
        .expect("等待进程")
        .expect("terminate 后 5s 内必须结束");
    assert_eq!(status.code, 1, "TerminateJobObject 使用退出码 1");
}

#[test]
fn terminate_reclaims_process_tree() {
    // cmd 会把 ping 作为子进程启动，Job 内应有 2 个活动进程。
    let mut child = spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe");

    let mut saw_tree = false;
    for _ in 0..100 {
        if child.active_processes().expect("查询 Job") >= 2 {
            saw_tree = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(saw_tree, "应观察到 cmd + ping 两个活动进程");

    child.terminate().expect("TerminateJobObject");
    assert!(
        child
            .wait_for_exit(Duration::from_secs(5))
            .expect("等待根进程")
            .is_some(),
        "根进程应已结束"
    );

    let mut drained = false;
    for _ in 0..100 {
        if child.active_processes().expect("查询 Job") == 0 {
            drained = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(drained, "Job 内仍有活动进程，进程树未被回收");
}

#[test]
fn drop_closes_job_and_kills_running_tree() {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let child = spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe");
    let pid = child.pid();

    // 等 ping 子进程出现，确认 Job 里是一棵树而不是单个进程。
    for _ in 0..100 {
        if child.active_processes().expect("查询 Job") >= 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }

    let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }.expect("OpenProcess");
    // 不调用 terminate：Drop 关闭最后一个 Job 句柄，KILL_ON_JOB_CLOSE 必须终止整棵树。
    drop(child);

    let event = unsafe { WaitForSingleObject(process, 5000) };
    unsafe {
        let _ = CloseHandle(process);
    }
    assert_eq!(event, WAIT_OBJECT_0, "关闭 Job 句柄后根进程应被终止");
}

#[test]
fn repeated_spawn_and_drop_does_not_leak() {
    for _ in 0..2 {
        let mut child = spawn(cmd(&["/C", "exit", "0"])).expect("启动 cmd.exe");
        assert!(
            child
                .wait_for_exit(Duration::from_secs(10))
                .expect("等待进程")
                .is_some()
        );
        let mut drained = false;
        for _ in 0..40 {
            if child.active_processes().expect("查询 Job") == 0 {
                drained = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(25));
        }
        assert!(drained, "退出后 Job 应清空");
        drop(child);
    }
}

// ---------------------------------------------------------------- 退出监控（M2-2）

/// 监控中的实例：watcher 负责退出语义，管道读端由测试持有，避免子进程写 stdout 时
/// 读到已关闭的管道（那会干扰“进程是否还活着”的断言）。
struct Watched {
    watcher: ExitWatcher,
    events: Receiver<ExitEvent>,
    _stdout: Option<PipeReader>,
    _stderr: Option<PipeReader>,
}

fn watch(child: Child, instance_id: &str, options: WatchOptions) -> Watched {
    let mut child = child;
    let stdout = child.take_stdout();
    let stderr = child.take_stderr();
    let (sender, events) = mpsc::channel();
    let watcher =
        ExitWatcher::start(instance_id, child, sender, options).expect("启动退出监控线程");
    Watched {
        watcher,
        events,
        _stdout: stdout,
        _stderr: stderr,
    }
}

#[test]
fn watcher_reports_zero_exit_with_empty_tree() {
    let watched = watch(
        spawn(cmd(&["/C", "exit", "0"])).expect("启动 cmd.exe"),
        "inst-zero",
        WatchOptions::default(),
    );

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(10))
        .expect("10s 内应收到退出事件");
    assert_eq!(event.instance_id, "inst-zero");
    assert_eq!(event.exit_code, 0);
    assert!(event.tree_empty, "cmd 退出后 Job 应为空");
    assert!(!event.forced_tree_kill, "正常退出不应强制清树");
    assert!(!event.stop_requested);
    assert!(
        event.tree_wait_ms < 2_000,
        "树已空时不应等满 tree_wait，实际 {}ms",
        event.tree_wait_ms
    );
}

#[test]
fn watcher_reports_nonzero_exit_code() {
    let watched = watch(
        spawn(cmd(&["/C", "exit", "7"])).expect("启动 cmd.exe"),
        "inst-seven",
        WatchOptions::default(),
    );

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(10))
        .expect("10s 内应收到退出事件");
    assert_eq!(event.instance_id, "inst-seven");
    assert_eq!(event.exit_code, 7);
    assert!(event.tree_empty);
    assert!(!event.forced_tree_kill);
    assert!(!event.stop_requested);
}

#[test]
fn watcher_stops_long_process_within_timeout() {
    // 显式 terminate：验证旧行为仍在毫秒级完成（graceful 路径见下方的信号测试）。
    let watched = watch(
        spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe"),
        "inst-stop",
        WatchOptions {
            method: StopMode::Terminate,
            ..WatchOptions::default()
        },
    );
    std::thread::sleep(Duration::from_millis(300));
    assert!(!watched.watcher.stop_requested());

    let status = watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("5s 内应停止根进程");
    assert_eq!(status.code, 1, "TerminateJobObject 使用退出码 1");

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("停止后 5s 内应收到退出事件");
    assert_eq!(event.instance_id, "inst-stop");
    assert!(event.stop_requested, "事件应标记停止请求");
    assert_eq!(event.exit_code, 1);
    assert!(event.tree_empty, "停止后整棵树都应为空");
    assert!(!event.forced_tree_kill, "停止本身不应触发超时强杀");
}

#[test]
fn repeated_stop_is_idempotent_and_emits_single_event() {
    let watched = watch(
        spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe"),
        "inst-idempotent",
        WatchOptions {
            method: StopMode::Terminate,
            ..WatchOptions::default()
        },
    );
    std::thread::sleep(Duration::from_millis(200));

    watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("第一次停止");
    watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("重复停止应幂等");

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("应收到退出事件");
    assert!(event.stop_requested);

    // 同一实例只发一次：第二次停止不能产生第二个事件。
    match watched.events.try_recv() {
        Err(TryRecvError::Empty | TryRecvError::Disconnected) => {}
        Ok(extra) => panic!("同一实例不应发出第二个事件：{extra:?}"),
    }
}

#[test]
fn watcher_emits_nothing_before_exit() {
    let watched = watch(
        spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe"),
        "inst-quiet",
        WatchOptions {
            method: StopMode::Terminate,
            ..WatchOptions::default()
        },
    );

    // 无退出、无停止时，事件通道必须保持安静（timeout 参数生效）。
    match watched.events.recv_timeout(Duration::from_millis(300)) {
        Err(RecvTimeoutError::Timeout) => {}
        other => panic!("退出前不应有事件，实际 {other:?}"),
    }
    assert!(!watched.watcher.stop_requested());

    watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("清理长进程");
    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("停止后应有事件");
    assert!(event.stop_requested);
}

#[test]
fn root_exit_with_live_child_forces_tree_kill() {
    // `cmd /C start /B ping ...`：cmd 启动 ping 后立即退出（exit 0），ping 留在 Job 中。
    // 先等根退出并确认 ping 已存在，再开始监控，避免“杀根时子进程还没启动”的时序竞争；
    // 监控线程仍然从“根已退出、树非空”开始，走完整的树空等待 + 强制清树路径。
    let mut child = spawn(cmd(&[
        "/C",
        "start",
        "",
        "/B",
        "ping",
        "-n",
        "30",
        "127.0.0.1",
    ]))
    .expect("启动 cmd.exe");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待根进程")
        .expect("cmd 应已退出");
    assert_eq!(status.code, 0, "start /B 后 cmd 应立即退出");

    let mut saw_child = false;
    for _ in 0..100 {
        if child.active_processes().expect("查询 Job") >= 1 {
            saw_child = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(saw_child, "cmd 退出后 ping 应仍在 Job 中");

    let options = WatchOptions {
        tree_wait: Duration::from_millis(500),
        ..WatchOptions::default()
    };
    let watched = watch(child, "inst-tree", options);

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(10))
        .expect("10s 内应收到退出事件");
    assert_eq!(event.exit_code, 0);
    assert!(
        event.forced_tree_kill,
        "根退出后 Job 仍非空，必须标记强制清树"
    );
    assert!(event.tree_empty, "强制清树后 Job 应为空");
    assert!(
        event.tree_wait_ms >= 450,
        "应至少等满 tree_wait，实际 {}ms",
        event.tree_wait_ms
    );
    assert!(!event.stop_requested);
}

#[test]
fn stop_after_root_exit_still_kills_lingering_tree() {
    // 根 cmd 已退出，但 ping 仍在 Job 中；request_stop 必须主动清掉这棵树，
    // 不能因为“根已退出”就跳过 TerminateJobObject。
    let mut child = spawn(cmd(&[
        "/C",
        "start",
        "",
        "/B",
        "ping",
        "-n",
        "30",
        "127.0.0.1",
    ]))
    .expect("启动 cmd.exe");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待根进程")
        .expect("cmd 应已退出");
    assert_eq!(status.code, 0);

    let mut saw_child = false;
    for _ in 0..100 {
        if child.active_processes().expect("查询 Job") >= 1 {
            saw_child = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(saw_child, "cmd 退出后 ping 应仍在 Job 中");

    // tree_wait 设得很长；如果 request_stop 没有清树，只能在 5s 后才靠超时强杀。
    let watched = watch(
        child,
        "inst-stop-tree",
        WatchOptions {
            tree_wait: Duration::from_secs(5),
            ..WatchOptions::default()
        },
    );
    let started = std::time::Instant::now();
    let exited = watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("停止已退出根的进程树");
    assert_eq!(exited.code, 0, "根进程在停止前已按 exit 0 退出");

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(2))
        .expect("request_stop 后应在 2s 内收到事件");
    assert!(event.stop_requested);
    assert!(event.tree_empty, "request_stop 应清空 Job 树");
    assert!(
        !event.forced_tree_kill,
        "主动停止不应等到 tree_wait 超时，实际 {:?}",
        started.elapsed()
    );
}

#[test]
fn events_are_tagged_with_their_instance_id() {
    let (sender, events) = mpsc::channel();

    let mut first = spawn(cmd(&["/C", "exit", "0"])).expect("启动 cmd.exe");
    let _first_pipes = (first.take_stdout(), first.take_stderr());
    let _first_watcher =
        ExitWatcher::start("first", first, sender.clone(), WatchOptions::default())
            .expect("启动第一个监控");

    let mut second = spawn(cmd(&["/C", "exit", "7"])).expect("启动 cmd.exe");
    let _second_pipes = (second.take_stdout(), second.take_stderr());
    let _second_watcher = ExitWatcher::start("second", second, sender, WatchOptions::default())
        .expect("启动第二个监控");

    let mut collected = Vec::new();
    for _ in 0..2 {
        collected.push(
            events
                .recv_timeout(Duration::from_secs(10))
                .expect("两个实例都应产生事件"),
        );
    }

    let first_event = collected
        .iter()
        .find(|event| event.instance_id == "first")
        .expect("应收到 first 的事件");
    let second_event = collected
        .iter()
        .find(|event| event.instance_id == "second")
        .expect("应收到 second 的事件");
    assert_eq!(first_event.exit_code, 0);
    assert_eq!(second_event.exit_code, 7);
}

#[test]
fn drop_stops_running_tree_and_joins_thread() {
    let watched = watch(
        spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe"),
        "inst-drop",
        WatchOptions::default(),
    );
    std::thread::sleep(Duration::from_millis(200));

    let started = std::time::Instant::now();
    drop(watched.watcher);
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "Drop 不应阻塞超过 stop_timeout，实际 {:?}",
        started.elapsed()
    );

    // Drop 触发的停止同样只产生一个事件，并且标记了 stop_requested。
    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("Drop 后应收到退出事件");
    assert!(event.stop_requested);
    assert_eq!(event.exit_code, 1);
}

// ---------------------------------------------------------------- 优雅停止

#[test]
fn creation_flags_include_new_process_group() {
    use windows::Win32::System::Threading::{
        CREATE_NEW_PROCESS_GROUP, CREATE_NO_WINDOW, CREATE_SUSPENDED,
    };

    let flags = super::win32::creation_flags();
    assert!(
        flags.contains(CREATE_NEW_PROCESS_GROUP),
        "优雅停止要求子进程使用独立进程组，实际 flags = {flags:?}"
    );
    // 与 CREATE_NO_WINDOW 叠加（隐藏控制台仍然存在，CTRL_BREAK 可达）。
    assert!(flags.contains(CREATE_NO_WINDOW));
    assert!(flags.contains(CREATE_SUSPENDED));
}

/// 定位 Node 夹具；没有 node 时返回 `None`（测试跳过，不失败）。
fn node_fixture(name: &str) -> Option<PathBuf> {
    let probe = std::process::Command::new("node").arg("--version").output();
    if !probe.as_ref().is_ok_and(|output| output.status.success()) {
        eprintln!("跳过：未找到 node");
        return None;
    }
    Some(
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../scripts/fixtures")
            .join(name),
    )
}

fn fixture_request(fixture: &Path, marker: &Path) -> SpawnRequest {
    SpawnRequest {
        program: OsString::from("node"),
        args: vec![
            fixture.to_string_lossy().into_owned(),
            marker.to_string_lossy().into_owned(),
        ],
        ..SpawnRequest::default()
    }
}

/// 等脚本写出 ready 标记（handler 注册完成），再发信号；否则存在窄窗口
/// 把脚本打死（spike §4.1 #10）。
fn wait_ready(path: &Path) {
    for _ in 0..200 {
        if path.is_file() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("夹具未在 5s 内写出 ready 标记：{}", path.display());
}

#[test]
fn graceful_stop_delivers_ctrl_break_and_script_exits_cleanly() {
    let Some(fixture) = node_fixture("graceful-exit.mjs") else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let marker = dir.path().join("signal.marker");
    let child = spawn(fixture_request(&fixture, &marker)).expect("启动 graceful-exit 夹具");
    wait_ready(&dir.path().join("signal.marker.ready"));

    let watched = watch(
        child,
        "inst-graceful",
        WatchOptions {
            method: StopMode::Graceful,
            grace: Duration::from_secs(2),
            ..WatchOptions::default()
        },
    );

    let started = Instant::now();
    let status = watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("优雅停止应在超时内完成");
    let elapsed = started.elapsed();

    assert_eq!(status.code, 0, "脚本应自行清理并以 0 退出");
    assert!(marker.is_file(), "脚本必须收到 SIGBREAK 并写出 marker");
    assert!(
        elapsed < Duration::from_secs(2),
        "应在宽限内退出，实际 {elapsed:?}"
    );

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("应收到退出事件");
    assert!(event.stop_requested);
    assert!(event.tree_empty, "脚本退出后 Job 应为空（无孤儿）");
    let report = event.stop.as_ref().expect("停止事件应带诊断");
    assert_eq!(report.method, StopMode::Graceful);
    assert!(!report.forced, "脚本配合时不应走强制兜底");
    assert!(report.signal_error.is_none(), "{report:?}");
}

#[test]
fn graceful_stop_falls_back_to_job_kill_when_script_ignores_signal() {
    let Some(fixture) = node_fixture("graceful-ignore.mjs") else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let marker = dir.path().join("ignore.marker");
    let child = spawn(fixture_request(&fixture, &marker)).expect("启动 graceful-ignore 夹具");
    wait_ready(&dir.path().join("ignore.marker.ready"));

    let grace = Duration::from_millis(800);
    let watched = watch(
        child,
        "inst-ignore",
        WatchOptions {
            method: StopMode::Graceful,
            grace,
            ..WatchOptions::default()
        },
    );

    let started = Instant::now();
    let status = watched
        .watcher
        .request_stop(Duration::from_secs(6))
        .expect("兜底强杀后应完成停止");
    let elapsed = started.elapsed();

    // 脚本收到了信号（写 marker）但拒绝退出，必须等满宽限后强杀。
    assert!(
        dir.path().join("ignore.marker.signal").is_file(),
        "脚本应已收到 SIGBREAK"
    );
    assert!(elapsed >= grace, "必须等满宽限才兜底，实际 {elapsed:?}");
    assert!(
        elapsed < Duration::from_secs(5),
        "兜底后应尽快结束，实际 {elapsed:?}"
    );
    assert_eq!(status.code, 1, "TerminateJobObject 使用退出码 1");

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("应收到退出事件");
    assert!(event.tree_empty, "兜底强杀后 Job 应为空（无孤儿）");
    let report = event.stop.as_ref().expect("停止事件应带诊断");
    assert_eq!(report.method, StopMode::Graceful);
    assert!(report.forced, "不配合的脚本必须标记 forced");
    assert!(!report.grace.is_zero());
}

#[test]
fn terminate_stop_is_immediate_and_not_marked_forced() {
    let Some(fixture) = node_fixture("graceful-ignore.mjs") else {
        return;
    };
    let dir = tempfile::tempdir().expect("临时目录");
    let marker = dir.path().join("terminate.marker");
    let child = spawn(fixture_request(&fixture, &marker)).expect("启动 graceful-ignore 夹具");
    wait_ready(&dir.path().join("terminate.marker.ready"));

    let watched = watch(
        child,
        "inst-terminate",
        WatchOptions {
            method: StopMode::Terminate,
            grace: Duration::from_secs(2),
            ..WatchOptions::default()
        },
    );

    let started = Instant::now();
    let status = watched
        .watcher
        .request_stop(Duration::from_secs(5))
        .expect("terminate 应立即完成");
    let elapsed = started.elapsed();

    assert!(
        elapsed < Duration::from_secs(1),
        "terminate 不应等待宽限，实际 {elapsed:?}"
    );
    assert_eq!(status.code, 1);

    let event = watched
        .events
        .recv_timeout(Duration::from_secs(5))
        .expect("应收到退出事件");
    assert!(event.tree_empty);
    let report = event.stop.as_ref().expect("停止事件应带诊断");
    assert_eq!(report.method, StopMode::Terminate);
    assert!(!report.forced, "terminate 是配置行为，不算兜底");
    assert!(
        !dir.path().join("terminate.marker.signal").is_file(),
        "terminate 不应给脚本发信号"
    );
}

// ---------------------------------------------------------------- 端口预检

fn bind_v4() -> (TcpListener, u16) {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定 127.0.0.1:0");
    let port = listener.local_addr().expect("读取本地地址").port();
    (listener, port)
}

fn candidate_free_port() -> u16 {
    let (listener, port) = bind_v4();
    drop(listener);
    port
}

#[test]
fn port_check_reports_own_listener() {
    let (listener, port) = bind_v4();
    let report = check_tcp_ports(&[port]).expect("查询 TCP 表");

    let status = report.get(port).expect("报告应包含被查询的端口");
    assert!(status.is_occupied(), "已监听的端口必须判定为占用");
    assert_eq!(status.pids(), vec![std::process::id()]);

    let own = status
        .listeners
        .iter()
        .find(|listener| listener.pid == std::process::id())
        .expect("应找到当前进程的监听项");
    assert_eq!(own.family, IpFamily::V4);
    assert_eq!(own.local_addr, IpAddr::V4(Ipv4Addr::LOCALHOST));
    assert_eq!(own.port, port);

    let expected = std::env::current_exe()
        .expect("当前测试可执行文件")
        .file_name()
        .expect("文件名")
        .to_string_lossy()
        .into_owned();
    let actual = own.process_name.as_deref().expect("自身进程名应可读");
    assert!(
        actual.eq_ignore_ascii_case(&expected),
        "进程名应为 {expected}，实际 {actual}"
    );
    eprintln!("端口 {port} 被 pid {}（{actual}）监听：{own}", own.pid);

    drop(listener);
}

#[test]
fn port_check_reports_wildcard_listener() {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("绑定 0.0.0.0:0");
    let port = listener.local_addr().expect("读取本地地址").port();

    let report = check_tcp_ports(&[port]).expect("查询 TCP 表");
    let status = report.get(port).expect("报告应包含被查询的端口");
    assert!(status.is_occupied());
    assert!(
        status
            .listeners
            .iter()
            .any(|listener| listener.local_addr == IpAddr::V4(Ipv4Addr::UNSPECIFIED)),
        "通配监听地址应为 0.0.0.0，实际 {:?}",
        status.listeners
    );

    drop(listener);
}

#[test]
fn port_check_reports_unused_port_as_free() {
    // 端口从 bind 到 drop 之间有极小窗口可能被别人抢走，失败时换端口重试。
    for _ in 0..5 {
        let port = candidate_free_port();
        let report = check_tcp_ports(&[port]).expect("查询 TCP 表");
        let status = report.get(port).expect("报告应包含被查询的端口");
        if !status.is_occupied() {
            assert!(status.listeners.is_empty());
            return;
        }
    }
    panic!("连续 5 个候选端口都被占用，无法验证空闲判定");
}

#[test]
fn port_check_dedups_and_preserves_input_order() {
    let (listener_a, port_a) = bind_v4();
    let (listener_b, port_b) = bind_v4();

    let report = check_tcp_ports(&[port_b, port_a, port_b]).expect("查询 TCP 表");
    let ports: Vec<u16> = report.entries().iter().map(|entry| entry.port).collect();
    assert_eq!(ports, vec![port_b, port_a], "重复端口只保留首次出现");
    assert!(report.get(port_a).expect("应包含 A").is_occupied());
    assert!(report.get(port_b).expect("应包含 B").is_occupied());

    drop((listener_a, listener_b));
}

#[test]
fn port_check_ignores_established_connections() {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("绑定监听端口");
    let addr = listener.local_addr().expect("读取监听地址");
    let client = TcpStream::connect(addr).expect("连接监听端口");
    let (server, _) = listener.accept().expect("接受连接");
    let client_port = client.local_addr().expect("客户端本地地址").port();

    let report = check_tcp_ports(&[client_port]).expect("查询 TCP 表");
    let status = report.get(client_port).expect("报告应包含客户端端口");
    assert!(
        !status.is_occupied(),
        "ESTABLISHED 连接不应被算作监听：{:?}",
        status.listeners
    );

    drop((client, server, listener));
}

#[test]
fn port_check_reports_ipv6_listener_when_available() {
    let Ok(listener) = TcpListener::bind((Ipv6Addr::LOCALHOST, 0)) else {
        eprintln!("跳过：本机无法绑定 ::1");
        return;
    };
    let port = listener.local_addr().expect("读取本地地址").port();

    match check_tcp_ports(&[port]) {
        Ok(report) => {
            let status = report.get(port).expect("报告应包含被查询的端口");
            assert!(status.is_occupied());
            let own = status
                .listeners
                .iter()
                .find(|item| item.family == IpFamily::V6 && item.pid == std::process::id())
                .expect("应找到当前进程的 IPv6 监听项");
            assert!(own.local_addr.is_ipv6());
            assert_eq!(own.port, port);
            eprintln!("IPv6 端口 {port} 被 pid {} 监听：{own}", own.pid);
        }
        Err(error) if error.stage() == PortCheckStage::QueryIpv6Table => {
            eprintln!("跳过：IPv6 表查询不可用（Win32 {}）", error.code());
        }
        Err(error) => panic!("IPv6 端口检查失败：{error}"),
    }

    drop(listener);
}

#[test]
fn port_check_empty_input_skips_queries() {
    let mut calls = 0;
    let report = check_tcp_ports_with(&[], |_| {
        calls += 1;
        Ok(Vec::new())
    })
    .expect("空输入应直接成功");
    assert!(report.entries().is_empty());
    assert_eq!(calls, 0, "空输入不应查询系统 TCP 表");
}

#[test]
fn port_check_keeps_occupied_when_process_name_unavailable() {
    // PID 不存在时 OpenProcess 必然失败，模拟“权限不足/进程已退出”的降级。
    let fake_pid = u32::MAX - 1;
    let listener = RawListener {
        family: IpFamily::V4,
        local_addr: IpAddr::V4(Ipv4Addr::LOCALHOST),
        scope_id: 0,
        port: 65_533,
        pid: fake_pid,
    };

    let report = check_tcp_ports_with(&[65_533], |family| match family {
        IpFamily::V4 => Ok(vec![listener.clone()]),
        IpFamily::V6 => Ok(Vec::new()),
    })
    .expect("注入查询应成功");

    let status = report.get(65_533).expect("报告应包含被查询的端口");
    assert!(status.is_occupied(), "进程名不可读不影响占用判定");
    assert_eq!(status.pids(), vec![fake_pid]);
    assert_eq!(status.listeners[0].process_name, None);
}

#[test]
fn port_check_propagates_ipv4_query_failure() {
    let error = check_tcp_ports_with(&[80], |_| {
        Err(PortCheckError::new(
            PortCheckStage::QueryIpv4Table,
            5,
            "拒绝访问",
        ))
    })
    .expect_err("查询失败必须返回错误，不能当成空闲");

    assert_eq!(error.stage(), PortCheckStage::QueryIpv4Table);
    assert_eq!(error.code(), 5);
    assert!(error.to_string().contains("拒绝访问"));
}

#[test]
fn port_check_propagates_ipv6_query_failure() {
    let error = check_tcp_ports_with(&[80], |family| match family {
        IpFamily::V4 => Ok(Vec::new()),
        IpFamily::V6 => Err(PortCheckError::new(
            PortCheckStage::QueryIpv6Table,
            50,
            "不支持",
        )),
    })
    .expect_err("IPv6 查询失败同样必须返回错误");

    assert_eq!(error.stage(), PortCheckStage::QueryIpv6Table);
    assert_eq!(error.code(), 50);
}
