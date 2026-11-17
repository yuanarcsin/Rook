//! M3-1 测试：临时心跳文件 + 本机 `TcpListener`。
//!
//! 所有时间推进都是显式 `Instant`，不 sleep；HTTP 服务线程只监听
//! 127.0.0.1:0，等待均有超时/固定睡眠上限。

use std::fs;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use super::{
    BusinessSignal, FailureKind, HEARTBEAT_LIMIT, HealthEvaluator, HealthMonitor, HealthPolicy,
    HealthProbe, HealthState, HeartbeatProbe, HeartbeatSpec, HttpProbe, HttpRequest, HttpSpec,
    HttpTransport, HttpTransportError, ProbeOutcome, ProbeReport, TcpHttpTransport,
};

fn ms(value: u64) -> Duration {
    Duration::from_millis(value)
}

fn heartbeat_spec(path: &Path, instance: &str, max_age_ms: u64) -> HeartbeatSpec {
    HeartbeatSpec {
        path: path.to_path_buf(),
        instance_id: instance.to_owned(),
        max_age: ms(max_age_ms),
        needs_human_status: vec!["login_required".to_owned(), "blocked".to_owned()],
    }
}

fn write_heartbeat(path: &Path, instance: &str, seq: i64, status: &str) {
    write_heartbeat_detail(path, instance, seq, status, "test");
}

fn write_heartbeat_detail(path: &Path, instance: &str, seq: i64, status: &str, detail: &str) {
    let payload = format!(
        r#"{{"instance":"{instance}","seq":{seq},"ts":"2026-11-02T04:40:00Z","status":"{status}","detail":"{detail}"}}"#
    );
    fs::write(path, payload).expect("写心跳文件");
}

fn healthy() -> ProbeReport {
    ProbeReport::healthy(Duration::ZERO)
}

fn failed() -> ProbeReport {
    ProbeReport::failed(FailureKind::HttpConnect, "测试失败", Duration::ZERO)
}

// ---------------------------------------------------------------- process

#[test]
fn process_probe_current_and_missing_pid() {
    let mut current = super::ProcessProbe::new(std::process::id());
    assert_eq!(current.probe(Instant::now()).outcome, ProbeOutcome::Healthy);

    let mut missing = super::ProcessProbe::new(0xFFFF_FFFC);
    match missing.probe(Instant::now()).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::ProcessExited);
        }
        other => panic!("不存在的 PID 应为失败，实际：{other:?}"),
    }
}

#[test]
fn process_probe_query_error_is_classified() {
    struct Broken;
    impl super::ProcessQuery for Broken {
        fn is_alive(&self, _pid: u32) -> Result<bool, String> {
            Err("模拟查询失败".to_owned())
        }
    }

    let mut probe = super::ProcessProbe::with_query(42, std::sync::Arc::new(Broken));
    match probe.probe(Instant::now()).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::ProcessQuery);
            assert!(failure.message.contains("模拟查询失败"));
        }
        other => panic!("查询错误应为失败，实际：{other:?}"),
    }
}

// ---------------------------------------------------------------- heartbeat

#[test]
fn heartbeat_healthy_tracks_content_change() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    assert_eq!(probe.probe(base).outcome, ProbeOutcome::Pending);

    write_heartbeat(&path, "inst-a", 1, "ok");
    let report = probe.probe(base + ms(100));
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    let heartbeat = report.heartbeat.expect("心跳细节");
    assert!(heartbeat.seen);
    assert!(heartbeat.changed);
    assert_eq!(heartbeat.seq, Some(1));

    let report = probe.probe(base + ms(2000));
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert!(!report.heartbeat.expect("心跳细节").changed);

    write_heartbeat(&path, "inst-a", 2, "ok");
    let report = probe.probe(base + ms(3000));
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert!(report.heartbeat.expect("心跳细节").changed);
}

#[test]
fn heartbeat_missing_is_pending_then_fails_after_seen() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    assert_eq!(probe.probe(base).outcome, ProbeOutcome::Pending);

    write_heartbeat(&path, "inst-a", 1, "ok");
    assert_eq!(probe.probe(base + ms(100)).outcome, ProbeOutcome::Healthy);

    fs::remove_file(&path).expect("删除心跳");
    match probe.probe(base + ms(200)).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HeartbeatMissing);
        }
        other => panic!("心跳缺失应为失败，实际：{other:?}"),
    }
}

#[test]
fn heartbeat_old_instance_is_rejected() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "new-instance", 6000), base);

    write_heartbeat(&path, "old-instance", 99, "ok");
    match probe.probe(base + ms(100)).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HeartbeatInstanceMismatch);
        }
        other => panic!("旧实例心跳应被拒绝，实际：{other:?}"),
    }

    write_heartbeat(&path, "new-instance", 1, "ok");
    assert_eq!(probe.probe(base + ms(200)).outcome, ProbeOutcome::Healthy);
}

#[test]
fn heartbeat_unchanged_content_becomes_stale() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    write_heartbeat(&path, "inst-a", 1, "ok");
    assert_eq!(probe.probe(base).outcome, ProbeOutcome::Healthy);

    match probe.probe(base + ms(6001)).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HeartbeatStale);
        }
        other => panic!("内容未变化应过期，实际：{other:?}"),
    }
}

#[test]
fn heartbeat_timestamp_only_is_accepted() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    fs::write(&path, "2026-11-02T04:40:00Z\n").expect("写时间戳心跳");
    let report = probe.probe(base);
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert!(report.heartbeat.expect("心跳细节").seen);
}

#[test]
fn heartbeat_parse_error_is_reported() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    fs::write(&path, "{not json").expect("写坏心跳");
    match probe.probe(base).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HeartbeatParse);
        }
        other => panic!("坏 JSON 应解析失败，实际：{other:?}"),
    }
}

#[test]
fn heartbeat_source_error_is_classified() {
    struct Failing;
    impl super::HeartbeatSource for Failing {
        fn read(&mut self, _path: &Path) -> io::Result<Vec<u8>> {
            Err(io::Error::new(io::ErrorKind::PermissionDenied, "模拟拒绝"))
        }
    }

    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::with_source(
        heartbeat_spec(&path, "inst-a", 6000),
        base,
        Box::new(Failing),
    );
    match probe.probe(base).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HeartbeatRead);
        }
        other => panic!("读取错误应为失败，实际：{other:?}"),
    }
}

#[test]
fn heartbeat_oversized_file_is_parse_failure_with_warning() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    fs::write(&path, vec![b'x'; HEARTBEAT_LIMIT + 1]).expect("写超限心跳");
    let report = probe.probe(base);
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HeartbeatParse);
            assert!(failure.message.contains("上限"), "{}", failure.message);
        }
        other => panic!("超限心跳应失败，实际：{other:?}"),
    }
    assert!(report.warnings.iter().any(|warning| matches!(
        warning,
        super::HealthWarning::HeartbeatTooLarge { limit } if *limit == HEARTBEAT_LIMIT
    )));
    assert!(report.heartbeat.is_some());
}

#[test]
fn heartbeat_seq_not_increasing_warns_but_stays_healthy() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    write_heartbeat_detail(&path, "inst-a", 5, "ok", "first");
    assert_eq!(probe.probe(base).outcome, ProbeOutcome::Healthy);

    write_heartbeat_detail(&path, "inst-a", 5, "ok", "second");
    let report = probe.probe(base + ms(100));
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert!(report.warnings.iter().any(|warning| matches!(
        warning,
        super::HealthWarning::SequenceNotIncreasing {
            previous: 5,
            current: 5
        }
    )));
}

#[test]
fn heartbeat_startup_grace_ignores_pending_then_times_out() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let policy = HealthPolicy::new(3, ms(1000), ms(500));
    let mut evaluator = HealthEvaluator::new(policy);
    evaluator.start(base);
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    let report = probe.probe(base + ms(100));
    let verdict = evaluator.observe(&report, base + ms(100));
    assert_eq!(verdict.state, HealthState::Starting);
    assert_eq!(verdict.consecutive_failures, 0);

    let report = probe.probe(base + ms(1500));
    let verdict = evaluator.observe(&report, base + ms(1500));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);
    assert_eq!(verdict.consecutive_failures, 0);
    assert_eq!(
        verdict.failure.expect("启动失败原因").kind,
        FailureKind::StartupTimeout
    );
}

#[test]
fn heartbeat_success_within_grace_becomes_healthy() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let policy = HealthPolicy::new(3, ms(1000), ms(500));
    let mut evaluator = HealthEvaluator::new(policy);
    evaluator.start(base);
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    write_heartbeat(&path, "inst-a", 1, "ok");
    let report = probe.probe(base + ms(200));
    let verdict = evaluator.observe(&report, base + ms(200));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert!(!verdict.startup_failure);
}

#[test]
fn heartbeat_needs_human_is_orthogonal() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let policy = HealthPolicy::new(3, ms(1000), ms(500));
    let mut evaluator = HealthEvaluator::new(policy);
    evaluator.start(base);
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    write_heartbeat(&path, "inst-a", 1, "login_required");
    let report = probe.probe(base + ms(100));
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert_eq!(
        report.business,
        BusinessSignal::NeedsHuman("login_required".to_owned())
    );
    let verdict = evaluator.observe(&report, base + ms(100));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(verdict.needs_human.as_deref(), Some("login_required"));
    assert_eq!(verdict.consecutive_failures, 0);

    write_heartbeat(&path, "inst-a", 2, "ok");
    let report = probe.probe(base + ms(200));
    let verdict = evaluator.observe(&report, base + ms(200));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(verdict.needs_human, None);
    assert!(verdict.needs_human_changed);
}

#[test]
fn heartbeat_stale_status_does_not_clear_needs_human() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let policy = HealthPolicy::new(3, ms(1000), ms(500));
    let mut evaluator = HealthEvaluator::new(policy);
    evaluator.start(base);
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    write_heartbeat(&path, "inst-a", 1, "login_required");
    let report = probe.probe(base);
    evaluator.observe(&report, base);

    let report = probe.probe(base + ms(6001));
    assert!(matches!(
        report.outcome,
        ProbeOutcome::Unhealthy(ref failure) if failure.kind == FailureKind::HeartbeatStale
    ));
    assert_eq!(report.business, BusinessSignal::NotReported);
    let verdict = evaluator.observe(&report, base + ms(6001));
    assert_eq!(verdict.needs_human.as_deref(), Some("login_required"));
}

#[test]
fn heartbeat_reset_observation_avoids_false_stale() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let mut probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);

    write_heartbeat(&path, "inst-a", 1, "ok");
    assert_eq!(probe.probe(base).outcome, ProbeOutcome::Healthy);

    probe.reset_observation(base + ms(60_000));
    let report = probe.probe(base + ms(60_000));
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert_eq!(
        report.heartbeat.expect("心跳细节").age,
        Some(Duration::ZERO)
    );
}

// ---------------------------------------------------------------- http

fn http_response(status: &str, body: &str) -> Vec<u8> {
    format!(
        "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
    .into_bytes()
}

fn spawn_http(response: Vec<u8>) -> (SocketAddr, thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定测试端口");
    let addr = listener.local_addr().expect("测试地址");
    let handle = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            let _ = stream.write_all(&response);
        }
    });
    (addr, handle)
}

fn http_spec(addr: SocketAddr, expect_status: Vec<u16>, body: Option<&str>) -> HttpSpec {
    HttpSpec {
        url: format!("http://{addr}/healthz"),
        timeout: Duration::from_secs(2),
        expect_status,
        expect_body_contains: body.map(str::to_owned),
    }
}

#[test]
fn http_200_with_body_is_healthy() {
    let (addr, server) = spawn_http(http_response("200 OK", r#"{"ok":true}"#));
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], Some(r#"{"ok":true"#)));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert_eq!(report.http_status, Some(200));
}

#[test]
fn http_503_is_failure() {
    let (addr, server) = spawn_http(http_response("503 Service Unavailable", "busy"));
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    assert_eq!(report.http_status, Some(503));
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => assert_eq!(failure.kind, FailureKind::HttpStatus),
        other => panic!("503 应为失败，实际：{other:?}"),
    }
}

#[test]
fn http_body_mismatch_is_failure() {
    let (addr, server) = spawn_http(http_response("200 OK", r#"{"ok":false}"#));
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], Some(r#"{"ok":true"#)));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HttpBodyMismatch);
        }
        other => panic!("body 不匹配应为失败，实际：{other:?}"),
    }
}

#[test]
fn http_3xx_is_not_followed() {
    let response = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    assert_eq!(report.http_status, Some(302));
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => assert_eq!(failure.kind, FailureKind::HttpStatus),
        other => panic!("302 不在 expect_status 时应失败，实际：{other:?}"),
    }
}

#[test]
fn http_3xx_is_healthy_when_expected() {
    let response = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_vec();
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![302], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert_eq!(report.http_status, Some(302));
}

#[test]
fn http_chunked_body_is_decoded() {
    let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nA\r\n{\"ok\":true\r\n0\r\n\r\n".to_vec();
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], Some(r#"{"ok":true"#)));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
}

#[test]
fn http_chunked_huge_chunk_size_is_protocol_error() {
    // 16 个 f 在 64 位平台解析为 usize::MAX；修复前会加法溢出/切片越界 panic。
    let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nffffffffffffffff\r\npayload"
        .to_vec();
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => assert_eq!(failure.kind, FailureKind::HttpProtocol),
        other => panic!("超大 chunk 大小应协议失败，实际：{other:?}"),
    }
}

#[test]
fn http_chunked_missing_terminator_is_protocol_error() {
    let response = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\nA\r\n{\"ok\":true\r\n"
        .to_vec();
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], Some(r#"{"ok":true"#)));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => assert_eq!(failure.kind, FailureKind::HttpProtocol),
        other => panic!("缺少终止块应协议失败，实际：{other:?}"),
    }
}

#[test]
fn http_short_content_length_body_is_protocol_error() {
    // 声明 10 字节只发 3 字节就关闭：修复前会静默返回 3 字节 body 并判健康。
    let response =
        b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\nConnection: close\r\n\r\nabc".to_vec();
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => assert_eq!(failure.kind, FailureKind::HttpProtocol),
        other => panic!("响应体不完整应协议失败，实际：{other:?}"),
    }
}

#[test]
fn http_chunked_raw_limit_without_terminator_is_protocol_error() {
    let mut response =
        b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n".to_vec();
    // 每块只有 1 字节：解码量远小于 BODY_LIMIT，但原始数据超过 256 KiB 上限且无终止块。
    for _ in 0..60_000 {
        response.extend_from_slice(b"1\r\na\r\n");
    }
    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![200], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => assert_eq!(failure.kind, FailureKind::HttpProtocol),
        other => panic!("chunked 原始数据超限应协议失败，实际：{other:?}"),
    }
}

/// 断言无 body 状态：传输层成功且 body 为空，探针按 expect_status 判健康。
fn assert_bodyless_http(response: Vec<u8>, status: u16) {
    let (addr, server) = spawn_http(response.clone());
    let mut transport = TcpHttpTransport;
    let http = transport
        .get(&HttpRequest {
            url: format!("http://{addr}/healthz"),
            timeout: Duration::from_secs(2),
        })
        .expect("无 body 状态不应因 Content-Length 误报协议错误");
    server.join().expect("HTTP 服务线程");
    assert_eq!(http.status, status);
    assert!(http.body.is_empty(), "无 body 状态应返回空 body");

    let (addr, server) = spawn_http(response);
    let mut probe = HttpProbe::new(http_spec(addr, vec![status], None));
    let report = probe.probe(Instant::now());
    server.join().expect("HTTP 服务线程");
    assert_eq!(report.outcome, ProbeOutcome::Healthy);
    assert_eq!(report.http_status, Some(status));
}

#[test]
fn http_304_with_content_length_is_bodyless() {
    // 304 的 Content-Length 描述选中表示的大小，不是响应体；不能按长度等待 body。
    let response =
        b"HTTP/1.1 304 Not Modified\r\nContent-Length: 1234\r\nConnection: close\r\n\r\n".to_vec();
    assert_bodyless_http(response, 304);
}

#[test]
fn http_204_with_content_length_is_bodyless() {
    // 204 按 RFC 9110 没有消息体；即使带上 Content-Length 也不能等待 body。
    let response =
        b"HTTP/1.1 204 No Content\r\nContent-Length: 10\r\nConnection: close\r\n\r\n".to_vec();
    assert_bodyless_http(response, 204);
}

#[test]
fn http_1xx_with_content_length_is_bodyless() {
    // 1xx 同样没有消息体；Content-Length 不能卡住完整性检查。
    let response =
        b"HTTP/1.1 100 Continue\r\nContent-Length: 5\r\nConnection: close\r\n\r\n".to_vec();
    assert_bodyless_http(response, 100);
}

#[test]
fn http_connection_refused_is_connect_error() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定临时端口");
    let addr = listener.local_addr().expect("临时地址");
    drop(listener);

    // 本机安全软件可能让 RST 延迟约 2s 才返回，超时留足余量以验证"拒绝连接"分类。
    let mut spec = http_spec(addr, vec![200], None);
    spec.timeout = Duration::from_secs(5);
    let mut probe = HttpProbe::new(spec);
    match probe.probe(Instant::now()).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HttpConnect);
        }
        other => panic!("拒绝连接应为失败，实际：{other:?}"),
    }
}

#[test]
fn http_timeout_is_reported() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("绑定慢端口");
    let addr = listener.local_addr().expect("慢端口地址");
    let server = thread::spawn(move || {
        if let Ok((mut stream, _)) = listener.accept() {
            let mut buffer = [0u8; 1024];
            let _ = stream.read(&mut buffer);
            thread::sleep(Duration::from_millis(400));
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n");
        }
    });

    let mut spec = http_spec(addr, vec![200], None);
    spec.timeout = ms(150);
    let mut probe = HttpProbe::new(spec);
    let report = probe.probe(Instant::now());
    server.join().expect("慢服务线程");
    match report.outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HttpTimeout);
        }
        other => panic!("超时应为失败，实际：{other:?}"),
    }
}

#[test]
fn http_transport_is_injectable() {
    struct TimeoutTransport;
    impl HttpTransport for TimeoutTransport {
        fn get(
            &mut self,
            _request: &super::HttpRequest,
        ) -> Result<super::HttpResponse, HttpTransportError> {
            Err(HttpTransportError::Timeout)
        }
    }

    let spec = HttpSpec {
        url: "http://127.0.0.1:1/healthz".to_owned(),
        timeout: ms(100),
        expect_status: vec![200],
        expect_body_contains: None,
    };
    let mut probe = HttpProbe::with_transport(spec, Box::new(TimeoutTransport));
    match probe.probe(Instant::now()).outcome {
        ProbeOutcome::Unhealthy(failure) => {
            assert_eq!(failure.kind, FailureKind::HttpTimeout);
        }
        other => panic!("注入超时应为失败，实际：{other:?}"),
    }
}

// ---------------------------------------------------------------- evaluator

#[test]
fn evaluator_threshold_and_success_clears_failures() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(3, Duration::ZERO, Duration::ZERO));
    evaluator.start(base);
    assert_eq!(
        evaluator.observe(&healthy(), base).state,
        HealthState::Healthy
    );

    let verdict = evaluator.observe(&failed(), base + ms(1));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 1);
    assert!(verdict.state_changed);

    let verdict = evaluator.observe(&failed(), base + ms(2));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 2);

    let verdict = evaluator.observe(&failed(), base + ms(3));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert_eq!(verdict.consecutive_failures, 3);
    assert!(verdict.state_changed);

    let verdict = evaluator.observe(&healthy(), base + ms(4));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(verdict.consecutive_failures, 0);
    assert!(verdict.failure.is_none());
}

#[test]
fn evaluator_startup_grace_ignores_failures_until_first_success() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, ms(1000), Duration::ZERO));
    evaluator.start(base);

    let verdict = evaluator.observe(&failed(), base + ms(100));
    assert_eq!(verdict.state, HealthState::Starting);
    assert_eq!(verdict.consecutive_failures, 0);

    let verdict = evaluator.observe(&healthy(), base + ms(200));
    assert_eq!(verdict.state, HealthState::Healthy);

    let verdict = evaluator.observe(&failed(), base + ms(300));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 1);

    let verdict = evaluator.observe(&failed(), base + ms(400));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert_eq!(verdict.consecutive_failures, 2);
}

#[test]
fn evaluator_startup_timeout_after_grace() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(3, ms(1000), Duration::ZERO));
    evaluator.start(base);

    let verdict = evaluator.observe(&failed(), base + ms(1000));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);
    assert_eq!(verdict.consecutive_failures, 0);
    assert_eq!(
        verdict.failure.expect("启动失败原因").kind,
        FailureKind::StartupTimeout
    );
}

#[test]
fn evaluator_reset_during_startup_extends_grace() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, ms(1000), ms(500)));
    evaluator.start(base);

    // 启动宽限内发生时钟跳变/唤醒：宽限从 reset 时刻重新起算，不误判启动失败。
    evaluator.reset_observation(base + ms(800));
    let verdict = evaluator.observe(&failed(), base + ms(1200));
    assert_eq!(verdict.state, HealthState::Starting);
    assert_eq!(verdict.consecutive_failures, 0);

    let verdict = evaluator.observe(&failed(), base + ms(1300));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);
}

#[test]
fn evaluator_reset_after_startup_failure_does_not_revive_starting() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, ms(1000), ms(500)));
    evaluator.start(base);

    let verdict = evaluator.observe(&failed(), base + ms(1001));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);

    // 启动失败是终态：reset 不能把宽限续到未来、把状态复活成 Starting。
    evaluator.reset_observation(base + ms(2000));
    let verdict = evaluator.verdict();
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);

    let verdict = evaluator.observe(&failed(), base + ms(2100));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);
    assert_eq!(verdict.consecutive_failures, 0);
    assert_eq!(
        verdict.failure.expect("启动失败原因").kind,
        FailureKind::StartupTimeout
    );
}

#[test]
fn evaluator_resume_after_startup_failure_does_not_revive_starting() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, ms(1000), ms(500)));
    evaluator.start(base);
    let verdict = evaluator.observe(&failed(), base + ms(1001));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);

    evaluator.suspend(base + ms(1100));
    // 休眠 8 小时后唤醒：启动宽限不能被休眠时长"续"到未来。
    let resume_at = base + ms(8 * 60 * 60 * 1000);
    evaluator.resume(resume_at);
    assert!(evaluator.verdict().startup_failure);

    let verdict = evaluator.observe(&failed(), resume_at + ms(100));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert!(verdict.startup_failure);
}

#[test]
fn evaluator_suspend_freezes_and_resume_grace_does_not_count() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, Duration::ZERO, ms(1000)));
    evaluator.start(base);
    evaluator.observe(&healthy(), base);

    evaluator.suspend(base + ms(100));
    let verdict = evaluator.observe(&failed(), base + ms(200));
    assert!(verdict.suspended);
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(verdict.consecutive_failures, 0);

    let verdict = evaluator.resume(base + ms(5000));
    assert!(!verdict.suspended);

    let verdict = evaluator.observe(&failed(), base + ms(5100));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 0);

    let verdict = evaluator.observe(&failed(), base + ms(5200));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 0);

    let verdict = evaluator.observe(&failed(), base + ms(6100));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 1);

    let verdict = evaluator.observe(&failed(), base + ms(6200));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert_eq!(verdict.consecutive_failures, 2);
}

#[test]
fn evaluator_reset_observation_enters_grace() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, Duration::ZERO, ms(1000)));
    evaluator.start(base);
    evaluator.observe(&healthy(), base);

    evaluator.reset_observation(base + ms(10_000));
    let verdict = evaluator.observe(&failed(), base + ms(10_100));
    assert_eq!(verdict.state, HealthState::Degraded);
    assert_eq!(verdict.consecutive_failures, 0);
}

#[test]
fn evaluator_per_instance_is_isolated() {
    let base = Instant::now();
    let policy = HealthPolicy::new(2, Duration::ZERO, Duration::ZERO);
    let mut first = HealthEvaluator::new(policy);
    let mut second = HealthEvaluator::new(policy);
    first.start(base);
    second.start(base);

    first.observe(&healthy(), base);
    second.observe(&healthy(), base);

    first.observe(&failed(), base + ms(1));
    let verdict = first.observe(&failed(), base + ms(2));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert_eq!(first.consecutive_failures(), 2);

    let verdict = second.observe(&healthy(), base + ms(2));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(second.consecutive_failures(), 0);
}

#[test]
fn evaluator_needs_human_does_not_block_health_failures() {
    let base = Instant::now();
    let mut evaluator = HealthEvaluator::new(HealthPolicy::new(2, Duration::ZERO, Duration::ZERO));
    evaluator.start(base);

    let report = healthy().with_business(BusinessSignal::NeedsHuman("login_required".to_owned()));
    let verdict = evaluator.observe(&report, base);
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(verdict.needs_human.as_deref(), Some("login_required"));

    evaluator.observe(&failed(), base + ms(1));
    let verdict = evaluator.observe(&failed(), base + ms(2));
    assert_eq!(verdict.state, HealthState::Unhealthy);
    assert_eq!(verdict.needs_human.as_deref(), Some("login_required"));

    evaluator.clear_needs_human();
    assert!(evaluator.needs_human().is_none());
}

// ---------------------------------------------------------------- monitor

#[test]
fn monitor_resume_resets_heartbeat_observation() {
    let dir = tempfile::tempdir().expect("临时目录");
    let path = dir.path().join("task.hb");
    let base = Instant::now();
    let probe = HeartbeatProbe::new(heartbeat_spec(&path, "inst-a", 6000), base);
    let mut monitor = HealthMonitor::new(
        Box::new(probe),
        HealthPolicy::new(3, ms(1000), ms(1000)),
        base,
    );

    write_heartbeat(&path, "inst-a", 1, "ok");
    let verdict = monitor.check(base + ms(100));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert!(monitor.last_report().is_some());

    monitor.suspend(base + ms(200));
    let verdict = monitor.check(base + ms(300));
    assert!(verdict.suspended);
    assert_eq!(verdict.state, HealthState::Healthy);

    // 模拟休眠 8 小时：恢复后心跳内容不变，但 last_change 重新起算。
    let resume_at = base + ms(8 * 60 * 60 * 1000);
    monitor.resume(resume_at);
    let verdict = monitor.check(resume_at + ms(100));
    assert_eq!(verdict.state, HealthState::Healthy);
    assert_eq!(verdict.consecutive_failures, 0);

    // 心跳继续写新内容后恢复正常监督。
    write_heartbeat(&path, "inst-a", 2, "ok");
    let verdict = monitor.check(resume_at + ms(200));
    assert_eq!(verdict.state, HealthState::Healthy);
}

#[test]
fn health_types_are_send_and_sync() {
    fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<super::ProbeReport>();
    assert_send_sync::<super::HealthVerdict>();
    assert_send_sync::<super::HealthFailure>();

    fn assert_send<T: Send>() {}
    assert_send::<HealthEvaluator>();
    assert_send::<HealthMonitor>();
    assert_send::<HeartbeatProbe>();
    assert_send::<HttpProbe>();
    assert_send::<super::ProcessProbe>();
}
