//! M2-3 测试：轮转/清理/降级直接驱动 `Writer`，采集/线程/EOF 用真实 `cmd.exe`。
//!
//! 集成测试只在 Windows 上编译（模块本身即 `cfg(windows)`），所有等待都有超时，
//! 大输出用较小的轮转阈值，避免生成超大文件。

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use chrono::{Days, Local};

use super::writer::{Writer, is_archive_name};
use super::{
    CURRENT_LOG, Counters, DEFAULT_RETENTION_DAYS, DEFAULT_TAIL_LINES, LogCollector, LogError,
    LogParams, MAX_TAIL_BYTES, MAX_TAIL_LINES, cleanup_archives, cleanup_task_logs,
    cleanup_total_size, current_log_path, list_log_files, normalize_limits, read_tail,
};
use crate::config::{Encoding, Rotation, TaskLogsConfig};
use crate::process::{SpawnRequest, spawn};

// ---------------------------------------------------------------- 辅助

fn test_params(dir: &Path) -> LogParams {
    let mut params = LogParams::new(dir);
    params.poll_interval = Duration::from_millis(5);
    params.stop_timeout = Duration::from_secs(5);
    params
}

fn cmd(args: &[&str]) -> SpawnRequest {
    SpawnRequest {
        program: OsString::from("cmd.exe"),
        args: args.iter().map(|arg| (*arg).to_owned()).collect(),
        ..SpawnRequest::default()
    }
}

/// 目录下所有 `.log` 文件，按文件名排序。
fn log_entries(dir: &Path) -> Vec<(String, PathBuf)> {
    let mut entries: Vec<(String, PathBuf)> = fs::read_dir(dir)
        .expect("读取日志目录")
        .filter_map(Result::ok)
        .map(|entry| {
            (
                entry.file_name().to_string_lossy().into_owned(),
                entry.path(),
            )
        })
        .filter(|(_, path)| path.extension().is_some_and(|ext| ext == "log"))
        .collect();
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    entries
}

fn archive_names(dir: &Path) -> Vec<String> {
    log_entries(dir)
        .into_iter()
        .map(|(name, _)| name)
        .filter(|name| name != CURRENT_LOG)
        .collect()
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn set_modified(path: &Path, time: SystemTime) {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .expect("打开文件以修改 mtime");
    file.set_modified(time).expect("设置 mtime");
}

// ---------------------------------------------------------------- 参数

#[test]
fn params_follow_task_logs_config() {
    let logs = TaskLogsConfig {
        dir: PathBuf::from("C:\\data\\logs\\demo"),
        rotation: Rotation::Daily,
        max_file_mb: 7,
        max_files: 4,
        max_days: 30,
        encoding: Encoding::Auto,
    };
    let params = LogParams::from_config(&logs);
    assert_eq!(params.dir, logs.dir);
    assert_eq!(params.rotation, Rotation::Daily);
    assert_eq!(params.max_file_bytes, 7 * 1024 * 1024);
    assert_eq!(params.max_files, 4);
    assert_eq!(params.max_days, 30);
    assert_eq!(params.current_path(), logs.dir.join(CURRENT_LOG));
}

#[test]
fn archive_names_are_recognized() {
    assert!(is_archive_name("20261102-044000-001.log"));
    assert!(is_archive_name("20261102.log"));
    assert!(is_archive_name("20261102-001.log"));
    assert!(!is_archive_name("current.log"));
    assert!(!is_archive_name("notes.txt"));
    assert!(!is_archive_name("2026-11-02.log"));
    assert!(!is_archive_name("20261102-0440-001.log"));
}

// ---------------------------------------------------------------- 轮转

#[test]
fn size_rotation_keeps_all_bytes_and_orders_archives() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut params = LogParams::new(dir);
    params.rotation = Rotation::Size;
    params.max_file_bytes = 100;
    params.max_files = 50;
    params.max_days = 365;
    let counters = Arc::new(Counters::default());
    let mut writer = Writer::new(params, Arc::clone(&counters));

    let mut original = Vec::new();
    for index in 0..10 {
        // 每块 36 字节：阈值 100 字节时每 3 块轮转一次。
        let chunk = format!("chunk-{index:02}-").repeat(4).into_bytes();
        original.extend_from_slice(&chunk);
        writer.write_chunk(&chunk);
    }
    writer.finish();

    let stats = counters.snapshot();
    assert_eq!(stats.written_bytes, original.len() as u64);
    assert_eq!(stats.dropped_bytes, 0);
    assert_eq!(
        stats.rotations, 3,
        "360 字节 / 100 字节阈值应轮转 3 次：{stats:?}"
    );

    let entries = log_entries(dir);
    let archives: Vec<(String, PathBuf)> = entries
        .into_iter()
        .filter(|(name, _)| name != CURRENT_LOG)
        .collect();
    assert_eq!(archives.len(), 3);
    for (name, _) in &archives {
        assert!(is_archive_name(name), "归档命名不符合约定：{name}");
    }

    // 归档按名字（UTC 时间 + seq）排序后与 current.log 拼接，必须完整还原。
    let mut combined = Vec::new();
    for (_, path) in &archives {
        combined.extend(fs::read(path).expect("读取归档"));
    }
    let current = fs::read(dir.join(CURRENT_LOG)).expect("读取 current.log");
    assert!(!current.is_empty(), "轮转后 current.log 应重新写入");
    combined.extend(current);
    assert_eq!(combined, original, "轮转不得丢失或重排字节");
}

#[test]
fn size_rotation_respects_max_files() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut params = LogParams::new(dir);
    params.rotation = Rotation::Size;
    params.max_file_bytes = 20;
    params.max_files = 3;
    params.max_days = 365;
    let counters = Arc::new(Counters::default());
    let mut writer = Writer::new(params, Arc::clone(&counters));

    for _ in 0..10 {
        writer.write_chunk(&[b'x'; 10]);
    }
    writer.finish();

    // max_files 含 current.log：归档最多 2 个。
    let names = archive_names(dir);
    assert_eq!(names.len(), 2, "应只保留最新的 2 个归档：{names:?}");
    assert!(dir.join(CURRENT_LOG).exists(), "current.log 永不删除");
}

#[test]
fn daily_rotation_archives_old_date() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut params = LogParams::new(dir);
    params.rotation = Rotation::Daily;
    params.max_days = 14;
    let counters = Arc::new(Counters::default());
    let mut writer = Writer::new(params, Arc::clone(&counters));

    writer.write_chunk(b"old\n");
    let yesterday = Local::now()
        .date_naive()
        .checked_sub_days(Days::new(1))
        .expect("昨天");
    writer.force_date(yesterday);
    writer.write_chunk(b"new\n");
    writer.finish();

    let archive = dir.join(format!("{}.log", yesterday.format("%Y%m%d")));
    assert_eq!(
        fs::read_to_string(&archive).expect("读取跨天归档"),
        "old\n",
        "跨天前的日志应归档"
    );
    assert_eq!(
        fs::read_to_string(dir.join(CURRENT_LOG)).expect("读取 current.log"),
        "new\n",
        "current.log 只保留跨天后的日志"
    );
}

#[test]
fn none_rotation_never_creates_archives() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut params = LogParams::new(dir);
    params.rotation = Rotation::None;
    params.max_file_bytes = 10;
    let counters = Arc::new(Counters::default());
    let mut writer = Writer::new(params, Arc::clone(&counters));

    for _ in 0..5 {
        writer.write_chunk(&[b'y'; 20]);
    }
    writer.finish();

    assert!(archive_names(dir).is_empty(), "none 模式不轮转");
    assert_eq!(
        fs::metadata(dir.join(CURRENT_LOG))
            .expect("current.log")
            .len(),
        100
    );
    assert_eq!(counters.snapshot().rotations, 0);
}

// ---------------------------------------------------------------- 清理

#[test]
fn max_files_removes_oldest_archives_and_keeps_current() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();
    for day in 1..=6 {
        fs::write(
            dir.join(format!("2026010{day}-000000-001.log")),
            vec![b'x'; day],
        )
        .expect("写归档");
    }
    fs::write(dir.join(CURRENT_LOG), b"current").expect("写 current");

    let removed = cleanup_archives(dir, Rotation::Size, 3, 365).expect("清理");
    assert_eq!(removed, 4, "max_files=3 时应删掉最旧的 4 个归档");
    assert_eq!(
        archive_names(dir),
        vec![
            "20260105-000000-001.log".to_owned(),
            "20260106-000000-001.log".to_owned()
        ]
    );
    assert_eq!(
        fs::read(dir.join(CURRENT_LOG)).expect("current.log"),
        b"current",
        "current.log 永不删除"
    );
}

#[test]
fn max_days_removes_by_mtime_but_keeps_current() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();
    let old = dir.join("20250101-000000-001.log");
    let recent = dir.join("20260101-000000-001.log");
    fs::write(&old, b"old").expect("写旧归档");
    fs::write(&recent, b"recent").expect("写新归档");
    fs::write(dir.join(CURRENT_LOG), b"current").expect("写 current");

    let forty_days = Duration::from_secs(40 * 24 * 60 * 60);
    set_modified(&old, SystemTime::now() - forty_days);

    let removed = cleanup_archives(dir, Rotation::Size, 50, 14).expect("清理");
    assert_eq!(removed, 1);
    assert!(!old.exists(), "超过 max_days 的归档应删除");
    assert!(recent.exists(), "保留期内的归档不能删");
    assert!(dir.join(CURRENT_LOG).exists(), "current.log 永不删除");
}

#[test]
fn default_task_retention_is_seven_days() {
    let params = LogParams::new("C:\\data\\logs\\demo");
    assert_eq!(DEFAULT_RETENTION_DAYS, 7);
    assert_eq!(params.max_days, 7);
}

#[test]
fn cleanup_task_logs_removes_expired_across_directories() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let first = tmp.path().join("logs").join("alpha");
    let second = tmp.path().join("logs").join("beta");
    let third = tmp.path().join("custom-logs");
    for dir in [&first, &second, &third] {
        fs::create_dir_all(dir).expect("创建目录");
    }
    let eight_days = Duration::from_secs(8 * 24 * 60 * 60);
    let six_days = Duration::from_secs(6 * 24 * 60 * 60);
    let mut expired = Vec::new();
    let mut kept = Vec::new();
    for (index, dir) in [&first, &second, &third].into_iter().enumerate() {
        let old = dir.join(format!("2026010{index}-000000-001.log"));
        let recent = dir.join(format!("2026011{index}-000000-001.log"));
        fs::write(&old, b"old").expect("写旧归档");
        fs::write(&recent, b"recent").expect("写新归档");
        set_modified(&old, SystemTime::now() - eight_days);
        set_modified(&recent, SystemTime::now() - six_days);
        fs::write(dir.join(CURRENT_LOG), b"current").expect("写 current");
        expired.push(old);
        kept.push(recent);
    }

    let dirs = vec![
        (first.clone(), DEFAULT_RETENTION_DAYS),
        (second.clone(), DEFAULT_RETENTION_DAYS),
        (third.clone(), DEFAULT_RETENTION_DAYS),
    ];
    let removed = cleanup_task_logs(&dirs);
    assert_eq!(removed, 3, "三个目录各删除一个 8 天前的归档");
    for path in &expired {
        assert!(!path.exists(), "{} 应删除", path.display());
    }
    for path in &kept {
        assert!(path.exists(), "{} 应保留", path.display());
    }
    for dir in [&first, &second, &third] {
        assert!(dir.join(CURRENT_LOG).exists(), "current.log 永不删除");
    }
}

#[test]
fn cleanup_ignores_foreign_files() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();
    for name in [
        CURRENT_LOG,
        "notes.txt",
        "app.log",
        "2026-11-02.log",
        "script-output.json",
    ] {
        fs::write(dir.join(name), b"x").expect("写测试文件");
    }

    let removed = cleanup_archives(dir, Rotation::Size, 1, 1).expect("清理");
    assert_eq!(removed, 0, "无法识别的文件不归核心清理");
    for name in [
        CURRENT_LOG,
        "notes.txt",
        "app.log",
        "2026-11-02.log",
        "script-output.json",
    ] {
        assert!(dir.join(name).exists(), "{name} 不应被删除");
    }
}

const KB: usize = 1024;

fn write_sized(path: &Path, kb: usize) {
    fs::write(path, vec![b'x'; kb * KB]).expect("写测试文件");
}

#[test]
fn total_limit_removes_oldest_archives_and_keeps_active_files() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let data = tmp.path();
    let core = data.join("core");
    let alpha = data.join("logs").join("alpha");
    let beta = data.join("logs").join("beta");
    for dir in [&core, &alpha, &beta] {
        fs::create_dir_all(dir).expect("创建目录");
    }

    // 活动文件（永不删除）：3 × 200KB。
    write_sized(&core.join("rook.log"), 200);
    write_sized(&core.join("events.jsonl"), 200);
    write_sized(&alpha.join(CURRENT_LOG), 200);

    // 归档（最旧优先）：600 + 500 + 400 + 300KB。
    let oldest = core.join("rook.log.2026-10-01");
    let second = core.join("events.jsonl.2026-10-02");
    let third = alpha.join("20261003-000000-001.log");
    let newest = beta.join("20261004-000000-001.log");
    write_sized(&oldest, 600);
    write_sized(&second, 500);
    write_sized(&third, 400);
    write_sized(&newest, 300);

    let now = SystemTime::now();
    let days = |count: u64| now - Duration::from_secs(count * 24 * 60 * 60);
    set_modified(&oldest, days(4));
    set_modified(&second, days(3));
    set_modified(&third, days(2));
    set_modified(&newest, days(1));

    let result = cleanup_total_size(data, 1).expect("总量清理");
    // 归档总量 1.8MB → 上限 1MB：删 600 + 500KB 后剩 700KB。
    assert_eq!(result.removed, 2, "{result:?}");
    assert_eq!(result.removed_bytes, 1_100 * KB as u64);
    assert!(
        result.total_bytes <= 1024 * KB as u64,
        "总量应压到上限内：{result:?}"
    );
    assert!(!oldest.exists(), "最旧归档先删");
    assert!(!second.exists());
    assert!(third.exists(), "未超限前不应继续删除");
    assert!(newest.exists());
    assert!(core.join("rook.log").exists(), "活动文件保留");
    assert!(core.join("events.jsonl").exists());
    assert!(alpha.join(CURRENT_LOG).exists());
}

#[test]
fn total_limit_does_not_delete_foreign_files_or_over_delete() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let data = tmp.path();
    let alpha = data.join("logs").join("alpha");
    fs::create_dir_all(&alpha).expect("创建目录");
    // 只有活动文件与脚本自建文件：没有可删归档。
    write_sized(&alpha.join(CURRENT_LOG), 200);
    write_sized(&alpha.join("script-output.json"), 2_000);

    let result = cleanup_total_size(data, 1).expect("总量清理");
    assert_eq!(result.removed, 0, "{result:?}");
    assert_eq!(result.removed_bytes, 0);
    assert!(alpha.join(CURRENT_LOG).exists());
    assert!(
        alpha.join("script-output.json").exists(),
        "脚本自建文件不删"
    );

    // 上限为 0 表示不限制。
    let unlimited = cleanup_total_size(data, 0).expect("总量清理");
    assert_eq!(unlimited, Default::default());
}

// ---------------------------------------------------------------- 降级

#[test]
fn write_failure_counts_dropped_bytes_and_keeps_writer_alive() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();
    // 把 current.log 变成目录，文件永远打不开：写入必须降级而不是退出。
    fs::create_dir(dir.join(CURRENT_LOG)).expect("占用 current.log");

    let mut params = LogParams::new(dir);
    params.reopen_interval = Duration::from_secs(3600);
    let counters = Arc::new(Counters::default());
    let mut writer = Writer::new(params, Arc::clone(&counters));

    for _ in 0..4 {
        writer.write_chunk(&[b'a'; 100]);
    }
    writer.finish();

    let stats = counters.snapshot();
    assert_eq!(stats.written_bytes, 0);
    assert_eq!(stats.dropped_bytes, 400, "全部字节都应计入丢弃");
    assert!(stats.io_errors >= 1, "打开失败要计入 IO 错误：{stats:?}");
}

#[test]
fn collector_drains_pipe_when_log_file_cannot_be_opened() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();
    fs::create_dir(dir.join(CURRENT_LOG)).expect("占用 current.log");

    let payload = "0123456789".repeat(10);
    let command = format!("for /L %i in (1,1,2000) do @echo {payload}");
    let mut child = spawn(cmd(&["/C", &command])).expect("启动 cmd.exe");

    let mut params = test_params(dir);
    params.reopen_interval = Duration::from_secs(3600);
    let collector =
        LogCollector::start(child.take_stdout(), child.take_stderr(), params).expect("启动采集");

    let status = child
        .wait_for_exit(Duration::from_secs(20))
        .expect("等待子进程")
        .expect("写日志失败不能阻塞子进程");
    assert!(status.success());

    let stats = collector.join();
    assert!(stats.read_bytes > 200_000, "{stats:?}");
    assert_eq!(stats.written_bytes, 0);
    assert_eq!(
        stats.dropped_bytes, stats.read_bytes,
        "读线程必须继续排空管道，所有字节计入丢弃"
    );
    assert!(stats.io_errors >= 1, "{stats:?}");
}

#[test]
fn start_without_sources_is_an_error() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let error = LogCollector::start(None, None, LogParams::new(tmp.path()))
        .err()
        .expect("没有管道应失败");
    assert!(matches!(error, LogError::NoSources), "{error}");
}

#[test]
fn start_reports_create_dir_failure() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let blocker = tmp.path().join("blocker");
    fs::write(&blocker, b"x").expect("写阻塞文件");
    // 父路径是文件：create_dir_all 必然失败。
    let dir = blocker.join("logs");

    let mut child = spawn(cmd(&["/C", "exit", "0"])).expect("启动 cmd.exe");
    let error = LogCollector::start(child.take_stdout(), None, LogParams::new(dir))
        .err()
        .expect("目录创建失败应返回错误");
    match error {
        LogError::CreateDir { path, .. } => {
            assert!(path.ends_with("logs"), "错误应带路径：{path:?}");
        }
        other => panic!("应为 CreateDir，实际 {other}"),
    }
    let _ = child.wait_for_exit(Duration::from_secs(10));
}

// ---------------------------------------------------------------- 采集/生命周期

#[test]
fn collector_merges_stdout_and_stderr_without_transcoding() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut child = spawn(cmd(&["/C", "echo out & echo err 1>&2"])).expect("启动 cmd.exe");
    let collector = LogCollector::start(child.take_stdout(), child.take_stderr(), test_params(dir))
        .expect("启动采集");

    let status = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待子进程")
        .expect("cmd 应退出");
    assert!(status.success());

    let stats = collector.join();
    let bytes = fs::read(dir.join(CURRENT_LOG)).expect("读取 current.log");
    // cmd 的 echo 会保留命令与 `&` 之间的空格：两行分别是 `out \r\n` 与 `err \r\n`。
    assert!(
        find_bytes(&bytes, b"out \r\n").is_some(),
        "stdout 原始 CRLF 应保留：{bytes:?}"
    );
    assert!(
        find_bytes(&bytes, b"err \r\n").is_some(),
        "stderr 原始 CRLF 应保留：{bytes:?}"
    );
    assert_eq!(stats.read_bytes, bytes.len() as u64);
    assert_eq!(stats.written_bytes, stats.read_bytes);
    assert_eq!(stats.dropped_bytes, 0);
}

#[test]
fn large_output_does_not_block_child_and_rotates_without_loss() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut params = test_params(dir);
    params.max_file_bytes = 64 * 1024;
    params.max_files = 50;
    params.max_days = 365;

    // 12000 行 × 102 字节 ≈ 1.2MB。
    let payload = "0123456789".repeat(10);
    let command = format!("for /L %i in (1,1,12000) do @echo {payload}");
    let mut child = spawn(cmd(&["/C", &command])).expect("启动 cmd.exe");
    let collector =
        LogCollector::start(child.take_stdout(), child.take_stderr(), params).expect("启动采集");

    let status = child
        .wait_for_exit(Duration::from_secs(30))
        .expect("等待子进程")
        .expect("大输出不能阻塞子进程");
    assert!(status.success());

    let stats = collector.join();
    assert!(stats.read_bytes > 1_000_000, "{stats:?}");
    assert_eq!(stats.dropped_bytes, 0, "磁盘正常时不应丢弃");
    assert_eq!(stats.written_bytes, stats.read_bytes);
    assert!(stats.rotations >= 10, "1.2MB / 64KB 应轮转多次：{stats:?}");

    let total: u64 = log_entries(dir)
        .iter()
        .map(|(_, path)| fs::metadata(path).expect("metadata").len())
        .sum();
    assert_eq!(
        total, stats.read_bytes,
        "所有落盘文件的总字节数必须等于读取字节数"
    );
}

#[test]
fn join_after_pipe_eof_flushes_without_loss() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut child = spawn(cmd(&["/C", "echo hello"])).expect("启动 cmd.exe");
    let collector = LogCollector::start(child.take_stdout(), child.take_stderr(), test_params(dir))
        .expect("启动采集");
    let _ = child
        .wait_for_exit(Duration::from_secs(10))
        .expect("等待子进程")
        .expect("cmd 应退出");

    let started = Instant::now();
    let stats = collector.join();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "管道 EOF 后 join 不应阻塞，实际 {:?}",
        started.elapsed()
    );

    let bytes = fs::read(dir.join(CURRENT_LOG)).expect("读取 current.log");
    assert!(find_bytes(&bytes, b"hello").is_some(), "{bytes:?}");
    assert_eq!(stats.read_bytes, bytes.len() as u64);
    assert_eq!(stats.written_bytes, stats.read_bytes);
}

#[test]
fn wait_is_false_while_child_runs_and_true_after_stop() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    // echo 先落一行（ping 在重定向时可能缓冲，不能依赖它产生输出）。
    let mut child = spawn(cmd(&["/C", "echo ready & ping -n 30 127.0.0.1"])).expect("启动 cmd.exe");
    let collector = LogCollector::start(child.take_stdout(), child.take_stderr(), test_params(dir))
        .expect("启动采集");

    // 先等 echo 的输出真正进入采集器，避免 terminate 早于读线程启动导致 flaky。
    let deadline = Instant::now() + Duration::from_secs(3);
    while collector.stats().read_bytes == 0 && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        collector.stats().read_bytes > 0,
        "子进程至少应产生一行可读输出"
    );

    assert!(
        !collector.wait(Duration::from_millis(50)),
        "子进程仍在运行时线程不应结束"
    );

    child.terminate().expect("清理进程树");
    let _ = child.wait_for_exit(Duration::from_secs(5));
    let started = Instant::now();
    let stats = collector.join();
    assert!(
        started.elapsed() < Duration::from_secs(5),
        "停止后 join 不应阻塞"
    );
    assert!(stats.read_bytes > 0, "ping 至少输出一行");
}

#[test]
fn drop_returns_quickly_and_releases_file_handles() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    let mut child = spawn(cmd(&["/C", "ping", "-n", "30", "127.0.0.1"])).expect("启动 cmd.exe");
    let collector = LogCollector::start(child.take_stdout(), child.take_stderr(), test_params(dir))
        .expect("启动采集");
    thread::sleep(Duration::from_millis(200));

    let started = Instant::now();
    drop(collector);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "提前 drop 不应无限等待，实际 {:?}",
        started.elapsed()
    );

    child.terminate().expect("清理进程树");
    let _ = child.wait_for_exit(Duration::from_secs(5));
    drop(child);
    fs::remove_dir_all(dir).expect("Drop 后日志文件句柄应已关闭");
}

// ---------------------------------------------------------------- M4-1 尾部读取

fn write_log(dir: &Path, bytes: &[u8]) -> PathBuf {
    let path = dir.join(CURRENT_LOG);
    fs::write(&path, bytes).expect("写入测试日志");
    path
}

#[test]
fn tail_keeps_last_lines_and_marks_truncated() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let content = (1..=10).map(|i| format!("line {i}\n")).collect::<String>();
    let path = write_log(tmp.path(), content.as_bytes());

    let tail = read_tail(&path, Some(3), None).expect("读取尾部");

    assert_eq!(tail.lines, vec!["line 8", "line 9", "line 10"]);
    assert!(tail.truncated, "丢弃了更早的行必须标记 truncated");
    assert_eq!(tail.total_bytes, content.len() as u64);
    assert!(tail.modified_at.is_some(), "应返回文件修改时间");
    assert!(!tail.has_replacement);
}

#[test]
fn tail_keeps_everything_when_it_fits() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let path = write_log(tmp.path(), b"alpha\nbeta\ngamma\n");

    let tail = read_tail(&path, Some(10), None).expect("读取尾部");

    assert_eq!(tail.lines, vec!["alpha", "beta", "gamma"]);
    assert!(!tail.truncated);
}

#[test]
fn tail_applies_line_limit_from_default() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let content = (0..(DEFAULT_TAIL_LINES + 5))
        .map(|i| format!("row {i}\n"))
        .collect::<String>();
    let path = write_log(tmp.path(), content.as_bytes());

    let tail = read_tail(&path, None, None).expect("读取尾部");

    let expected = format!("row {}", DEFAULT_TAIL_LINES + 4);
    assert_eq!(tail.lines.len(), DEFAULT_TAIL_LINES);
    assert_eq!(
        tail.lines.last().map(String::as_str),
        Some(expected.as_str())
    );
    assert!(tail.truncated);
}

#[test]
fn tail_respects_byte_limit_and_drops_partial_line() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let content = b"aaaa\nbbbb\n";
    let path = write_log(tmp.path(), content);

    let tail = read_tail(&path, Some(200), Some(6)).expect("读取尾部");

    // 起点落在 "aaaa" 中间：残行被丢弃，只保留完整行。
    assert_eq!(tail.lines, vec!["bbbb"]);
    assert!(tail.truncated);
    assert_eq!(tail.total_bytes, content.len() as u64);
}

#[test]
fn tail_byte_limit_at_line_boundary_keeps_line() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let path = write_log(tmp.path(), b"aa\nbb\n");

    let tail = read_tail(&path, Some(200), Some(3)).expect("读取尾部");

    assert_eq!(tail.lines, vec!["bb"], "起点正好在行首时不应丢行");
    assert!(tail.truncated);
}

#[test]
fn tail_handles_empty_file() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let path = write_log(tmp.path(), b"");

    let tail = read_tail(&path, None, None).expect("读取尾部");

    assert!(tail.lines.is_empty());
    assert!(!tail.truncated);
    assert_eq!(tail.total_bytes, 0);
}

#[test]
fn tail_splits_crlf_and_lone_cr() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let path = write_log(tmp.path(), b"one\r\ntwo\rthree\n");

    let tail = read_tail(&path, Some(10), None).expect("读取尾部");

    assert_eq!(tail.lines, vec!["one", "two", "three"]);
    assert!(!tail.truncated);
}

#[test]
fn tail_marks_non_utf8_output() {
    let tmp = tempfile::tempdir().expect("临时目录");
    // 0xFF 不是合法 UTF-8（GBK 日志里常见）。
    let path = write_log(tmp.path(), &[0xFF, b'\n', b'o', b'k', b'\n']);

    let tail = read_tail(&path, Some(10), None).expect("读取尾部");

    assert_eq!(tail.lines, vec!["\u{FFFD}", "ok"]);
    assert!(tail.has_replacement, "GBK 输出应给出解码提示");
    assert!(!tail.truncated);
}

#[test]
fn tail_missing_file_is_not_found() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let error = read_tail(&tmp.path().join(CURRENT_LOG), None, None).expect_err("文件不存在应报错");
    assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
}

#[test]
fn limits_are_clamped_to_contract() {
    assert_eq!(
        normalize_limits(None, None),
        (DEFAULT_TAIL_LINES, MAX_TAIL_BYTES)
    );
    assert_eq!(
        normalize_limits(Some(0), Some(0)),
        (DEFAULT_TAIL_LINES, MAX_TAIL_BYTES),
        "0 按缺省处理"
    );
    assert_eq!(
        normalize_limits(Some(MAX_TAIL_LINES + 1), Some(MAX_TAIL_BYTES * 4)),
        (MAX_TAIL_LINES, MAX_TAIL_BYTES)
    );
    assert_eq!(normalize_limits(Some(5), Some(1024)), (5, 1024));
}

#[test]
fn log_path_comes_from_configured_dir() {
    let dir = Path::new("C:/data/logs/alpha");
    let path = current_log_path(dir);
    assert_eq!(path, dir.join(CURRENT_LOG));
    assert!(path.ends_with("current.log"));
}

// ---------------------------------------------------------------- C3 归档浏览

#[test]
fn list_log_files_returns_current_first_then_archives_newest_first() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path();

    fs::write(dir.join(CURRENT_LOG), b"live\n").expect("写 current.log");
    fs::write(dir.join("20260101-120000-001.log"), b"size archive\n").expect("写 size 归档");
    fs::write(dir.join("20260101.log"), b"daily archive\n").expect("写 daily 归档");
    // 脚本自建文件不是本模块产物，不应出现在列表里。
    fs::write(dir.join("notes.txt"), b"ignore\n").expect("写脚本自建文件");
    fs::write(dir.join("script-child.log"), b"ignore\n").expect("写非归档命名的 .log");

    let now = SystemTime::now();
    set_modified(&dir.join(CURRENT_LOG), now);
    set_modified(&dir.join("20260101-120000-001.log"), now);
    set_modified(&dir.join("20260101.log"), now - Duration::from_secs(3_600));

    let files = list_log_files(dir).expect("列出日志文件");
    let names: Vec<&str> = files.iter().map(|file| file.name.as_str()).collect();
    assert_eq!(
        names,
        vec![CURRENT_LOG, "20260101-120000-001.log", "20260101.log"],
        "current.log 在前，归档按 mtime 倒序"
    );
    assert!(files[0].current, "第一个必是 current.log");
    assert_eq!(files[0].size, 5);
    assert!(!files[1].current);
    assert!(files[1].modified_at.is_some());
}

#[test]
fn list_log_files_missing_dir_is_empty() {
    let tmp = tempfile::tempdir().expect("临时目录");
    let dir = tmp.path().join("never-started");

    let files = list_log_files(&dir).expect("目录不存在不是错误");

    assert!(files.is_empty());
}
