//! M2-6：进程层集成测试（`rook_lib::process` 公开 API + 真实夹具）。
//!
//! 覆盖：
//! - 启动夹具并拿到 stdout/stderr 与退出码（正常 0 / 崩溃 7）；
//! - `TerminateJobObject` 回收"父进程 + 孙进程"整棵树；
//! - 端口预检能读到真实监听夹具的 PID。
//!
//! 约定：所有等待都有超时；没有 Node 时打印原因并跳过（不 panic、不 flaky）；
//! 只按 PID 检查存活，绝不按进程名结束无关进程。

#![cfg(windows)]

mod support;

use std::io::Read;
use std::time::Duration;

use rook_lib::process::{SpawnRequest, check_tcp_ports, spawn};
use support::*;

/// 构造"用 Node 跑夹具"的启动请求。
fn node_request(node: &std::path::Path, args: Vec<String>) -> SpawnRequest {
    let mut request = SpawnRequest::new(node.as_os_str().to_owned());
    request.args = args;
    request
}

#[test]
fn spawn_captures_stdout_stderr_and_exit_code() {
    let Some(node) = node() else {
        return;
    };

    let mut child = spawn(node_request(
        &node,
        vec![fixture_arg("exit-code.mjs"), "0".into(), "正常 退出".into()],
    ))
    .expect("启动 exit-code 夹具");

    // 先取走读端，再等退出：避免进程写满管道阻塞。
    let mut stdout = child.take_stdout().expect("stdout 读端").into_file();
    let mut stderr = child.take_stderr().expect("stderr 读端").into_file();

    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待夹具退出")
        .expect("夹具应在超时内退出");
    assert_eq!(status.code, 0);
    assert!(status.success());

    let mut out = Vec::new();
    stdout.read_to_end(&mut out).expect("读取 stdout");
    let mut err = Vec::new();
    stderr.read_to_end(&mut err).expect("读取 stderr");

    assert!(
        contains(&out, "exit-code start marker=正常 退出".as_bytes()),
        "stdout 应保留 CJK/空格参数：{}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        contains(&out, b"exit-code done code=0"),
        "stdout 应包含完成行：{}",
        String::from_utf8_lossy(&out)
    );
    assert!(
        contains(&err, "exit-code stderr marker=正常 退出".as_bytes()),
        "stderr 应独立可读：{}",
        String::from_utf8_lossy(&err)
    );
}

#[test]
fn spawn_records_crash_exit_code_seven() {
    let Some(node) = node() else {
        return;
    };

    let mut child = spawn(node_request(
        &node,
        vec![fixture_arg("exit-code.mjs"), "7".into()],
    ))
    .expect("启动 exit-code 夹具");

    let mut stdout = child.take_stdout().expect("stdout 读端").into_file();
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待夹具退出")
        .expect("夹具应在超时内退出");
    assert_eq!(status.code, 7, "崩溃退出码应原样保留");
    assert!(!status.success());

    let mut out = Vec::new();
    stdout.read_to_end(&mut out).expect("读取 stdout");
    assert!(contains(&out, b"exit-code done code=7"));
}

#[test]
fn terminate_kills_grandchild_tree() {
    let Some(node) = node() else {
        return;
    };

    let dir = tempfile::tempdir().expect("临时目录");
    let pid_file = dir.path().join("grandchild.pid");

    let mut child = spawn(node_request(
        &node,
        vec![
            fixture_arg("spawn-child.mjs"),
            pid_file.display().to_string(),
            "60000".into(),
        ],
    ))
    .expect("启动派生孙进程夹具");

    let parent = child.pid();
    let grandchild = wait_pid_file(&pid_file);
    assert!(
        process_alive(grandchild),
        "孙进程 {grandchild} 应已启动（PID 文件已写入）"
    );

    child.terminate().expect("TerminateJobObject");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待根进程退出")
        .expect("根进程应在超时内退出");
    assert_eq!(status.code, 1, "TerminateJobObject 使用退出码 1");

    // 整棵树都应消失；只按 PID 检查，不按进程名。
    wait_gone(parent, Duration::from_secs(5), "terminate 后的根进程");
    wait_gone(grandchild, Duration::from_secs(5), "terminate 后的孙进程");
}

#[test]
fn port_precheck_reports_fixture_listener_pid() {
    let Some(node) = node() else {
        return;
    };

    let port = free_port();
    let mut child = spawn(node_request(
        &node,
        vec![
            fixture_arg("port-listen.mjs"),
            port.to_string(),
            "60000".into(),
        ],
    ))
    .expect("启动端口监听夹具");
    let pid = child.pid();

    wait_for("端口进入 LISTEN", || {
        check_tcp_ports(&[port])
            .map(|report| {
                report
                    .get(port)
                    .is_some_and(rook_lib::process::PortStatus::is_occupied)
            })
            .unwrap_or(false)
    });

    let report = check_tcp_ports(&[port]).expect("查询端口");
    let status = report.get(port).expect("报告应包含被查询端口");
    assert!(status.is_occupied(), "端口 {port} 应被占用");
    assert!(
        status.pids().contains(&pid),
        "占用 PID 应包含夹具 PID {pid}，实际 {:?}",
        status.pids()
    );

    child.terminate().expect("结束监听夹具");
    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待夹具退出");
    assert!(status.is_some(), "夹具应在超时内退出");
}
