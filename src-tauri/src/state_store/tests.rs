//! state_store 单元测试：不依赖真实用户目录，全部在临时目录内完成。
//!
//! 覆盖：roundtrip、缺失、损坏 JSON/结构、未知版本、未知字段、原子写失败、
//! 内容不变不写、时间与枚举序列化、父目录创建、schema 校验。

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::*;

fn tempdir() -> tempfile::TempDir {
    tempfile::tempdir().expect("创建临时目录")
}

fn state_path(dir: &tempfile::TempDir) -> PathBuf {
    dir.path().join(STATE_FILE_NAME)
}

fn ts(text: &str) -> Timestamp {
    Timestamp::parse(text).expect("合法 RFC-3339 时间")
}

fn sample_state() -> PersistedState {
    let mut state = PersistedState::default();
    state.tasks.insert(
        "ykt-gateway".to_owned(),
        TaskState {
            desired_override: Some(DesiredOverride::Paused),
            consecutive_failures: 3,
            total_starts: 12,
            total_restarts: 2,
            last_exit_code: Some(7),
            last_exit_at: Some(ts("2026-11-02T04:40:00Z")),
            last_start_at: Some(ts("2026-11-02T04:00:00Z")),
            last_reason: Some("process_exit".to_owned()),
            paused_reason: Some(PausedReason::CrashLoop),
            next_retry_at: Some(ts("2026-11-02T04:50:00Z")),
            last_status: Some(LastStatus::Paused),
            needs_human: NeedsHumanState {
                active: true,
                source: Some("heartbeat".to_owned()),
                message: Some("login_required".to_owned()),
                since: Some(ts("2026-11-02T04:40:00Z")),
            },
            schedule_window: None,
            schedule_suppressed: None,
        },
    );
    state.window = WindowState {
        width: 1280,
        height: 800,
        maximized: true,
    };
    state.ui.last_selected = Some("ykt-gateway".to_owned());
    state
}

fn write_text(path: &Path, text: &str) {
    fs::write(path, text).expect("写入测试文件");
}

fn read_json(path: &Path) -> Value {
    let bytes = fs::read(path).expect("读取状态文件");
    serde_json::from_slice(&bytes).expect("状态文件是合法 JSON")
}

/// 目录下残留的 `*.tmp` 文件名（用于断言失败路径清理）。
fn temp_files(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .expect("列出临时目录")
        .map(|entry| {
            entry
                .expect("目录项")
                .file_name()
                .to_string_lossy()
                .into_owned()
        })
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    names.sort();
    names
}

#[test]
fn roundtrip_preserves_state() {
    let dir = tempdir();
    let path = state_path(&dir);
    let sample = sample_state();

    assert_eq!(
        save(&path, &sample).expect("保存应成功"),
        SaveOutcome::Written
    );
    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded, "{}", loaded.status);
    assert_eq!(loaded.state, sample);
    assert!(loaded.is_usable());
    assert!(temp_files(dir.path()).is_empty(), "不应留下临时文件");

    // 磁盘格式：UTF-8、可读缩进、末尾换行（便于 diff/手工排查）。
    let text = fs::read_to_string(&path).expect("状态文件是 UTF-8");
    assert!(text.ends_with('\n'));
    assert!(text.contains("\n  \"tasks\": {"), "2 空格缩进：{text}");
}

#[test]
fn missing_file_returns_default_state() {
    let dir = tempdir();
    let path = state_path(&dir);

    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Missing);
    assert_eq!(loaded.state, PersistedState::default());
    assert!(loaded.is_usable());
    assert!(!loaded.needs_attention());
    assert!(!path.exists(), "load 不得创建状态文件");
}

#[test]
fn app_session_roundtrips_and_old_state_gets_defaults() {
    let dir = tempdir();
    let path = state_path(&dir);

    let state = PersistedState {
        app_session: AppSession {
            started_at: Some(ts("2026-11-05T01:00:00Z")),
            clean_exit: true,
            last_exit_reason: Some("user".to_owned()),
            last_exit_at: Some(ts("2026-11-05T02:00:00Z")),
            last_version: Some("0.1.0".to_owned()),
        },
        ..PersistedState::default()
    };
    assert_eq!(
        save(&path, &state).expect("保存应成功"),
        SaveOutcome::Written
    );
    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded);
    assert_eq!(loaded.state.app_session, state.app_session);

    // 旧版 state 没有 app_session：必须能读，且默认 clean_exit=false。
    write_text(
        &path,
        r#"{"schema":1,"tasks":{},"window":{"width":900,"height":600},"ui":{}}"#,
    );
    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded);
    assert_eq!(loaded.state.app_session, AppSession::default());
    assert!(!loaded.state.app_session.clean_exit);
    assert!(loaded.state.app_session.started_at.is_none());
}

#[test]
fn corrupt_json_returns_default_state_without_touching_file() {
    let dir = tempdir();
    let path = state_path(&dir);
    let original = "{ 这不是 JSON";
    write_text(&path, original);

    let loaded = load(&path);
    assert!(
        matches!(loaded.status, LoadStatus::Corrupted { .. }),
        "{}",
        loaded.status
    );
    assert_eq!(loaded.state, PersistedState::default());
    assert!(loaded.needs_attention());
    assert_eq!(
        fs::read_to_string(&path).expect("损坏文件仍可读"),
        original,
        "load 不得改写损坏文件"
    );
}

#[test]
fn wrong_shape_is_reported_as_corrupt() {
    let dir = tempdir();
    let path = state_path(&dir);

    for raw in [
        "[]",
        "\"state\"",
        r#"{"schema":1,"tasks":[]}"#,
        r#"{"schema":"one","tasks":{}}"#,
        r#"{"schema":1,"tasks":{"t":{"desired_override":"running"}}}"#,
        r#"{"schema":1,"window":"big"}"#,
        r#"{"schema":1,"tasks":{"t":{"last_exit_at":"昨天"}}}"#,
    ] {
        write_text(&path, raw);
        let loaded = load(&path);
        assert!(
            matches!(loaded.status, LoadStatus::Corrupted { .. }),
            "{raw} 应判为损坏，实际：{}",
            loaded.status
        );
        assert_eq!(loaded.state, PersistedState::default());
    }
}

#[test]
fn unknown_schema_returns_default_state() {
    let dir = tempdir();
    let path = state_path(&dir);
    // 未知版本即使带着未来结构（tasks 为数组）也应先按版本拒绝，而不是报结构损坏。
    write_text(
        &path,
        r#"{"schema":2,"tasks":[],"future":{"window":{"width":1}}}"#,
    );

    let loaded = load(&path);
    assert_eq!(
        loaded.status,
        LoadStatus::UnsupportedSchema { found: 2 },
        "{}",
        loaded.status
    );
    assert_eq!(loaded.state, PersistedState::default());
    assert!(loaded.state.tasks.is_empty());
    assert!(loaded.needs_attention());
    assert_eq!(
        fs::read_to_string(&path).expect("原文件可读"),
        r#"{"schema":2,"tasks":[],"future":{"window":{"width":1}}}"#,
        "load 不得改写未知版本文件"
    );
}

#[test]
fn unknown_fields_are_ignored() {
    let dir = tempdir();
    let path = state_path(&dir);
    let raw = json!({
        "schema": 1,
        "future_root": {"x": 1},
        "tasks": {
            "ykt-gateway": {
                "desired_override": "paused",
                "consecutive_failures": 2,
                "future_task": [1, 2, 3],
                "needs_human": {"active": false, "future_flag": true}
            }
        },
        "window": {"width": 900, "height": 600, "maximized": true, "x": 100},
        "ui": {"last_selected": "ykt-gateway", "future": null}
    });
    write_text(&path, &raw.to_string());

    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded, "{}", loaded.status);
    let task = loaded.state.task("ykt-gateway").expect("任务状态存在");
    assert_eq!(task.desired_override, Some(DesiredOverride::Paused));
    assert_eq!(task.consecutive_failures, 2);
    // 缺失字段用默认值。
    assert_eq!(task.total_starts, 0);
    assert_eq!(task.total_restarts, 0);
    assert_eq!(task.last_status, None);
    assert_eq!(task.needs_human, NeedsHumanState::default());
    assert_eq!(
        loaded.state.window,
        WindowState {
            width: 900,
            height: 600,
            maximized: true,
        }
    );
    assert_eq!(
        loaded.state.ui.last_selected.as_deref(),
        Some("ykt-gateway")
    );

    // 写回时未知字段被丢弃：state 是派生视图，不承诺保留未知字段。
    assert_eq!(
        save(&path, &loaded.state).expect("保存应成功"),
        SaveOutcome::Written
    );
    let text = fs::read_to_string(&path).expect("状态文件是 UTF-8");
    assert!(!text.contains("future_root"), "{text}");
    assert!(!text.contains("future_task"), "{text}");
    assert!(!text.contains("future_flag"), "{text}");
}

#[test]
fn atomic_write_failure_cleans_up_temp_file() {
    let dir = tempdir();
    let path = state_path(&dir);
    // 用目录冒充状态文件：写临时文件成功，但最后 rename 到目录必然失败。
    fs::create_dir(&path).expect("创建冒充状态文件的目录");

    let error = save(&path, &sample_state()).expect_err("目标为目录时保存应失败");
    assert!(matches!(error, StateStoreError::Io { .. }), "{error}");
    assert!(
        path.is_dir(),
        "失败的原子替换不应破坏目标目录：{}",
        path.display()
    );
    assert!(
        temp_files(dir.path()).is_empty(),
        "失败后必须清理临时文件，实际残留：{:?}",
        temp_files(dir.path())
    );
}

#[test]
fn unchanged_state_is_not_rewritten() {
    let dir = tempdir();
    let path = state_path(&dir);
    let state = sample_state();

    assert_eq!(
        save(&path, &state).expect("保存应成功"),
        SaveOutcome::Written
    );
    let first = fs::read(&path).expect("读取首次写入内容");
    assert_eq!(
        save(&path, &state).expect("保存应成功"),
        SaveOutcome::Unchanged
    );
    assert_eq!(fs::read(&path).expect("读取第二次内容"), first);

    // 内容真正变化时必须写盘。
    let mut changed = state.clone();
    changed.task_mut("ykt-gateway").consecutive_failures += 1;
    assert_eq!(
        save(&path, &changed).expect("保存应成功"),
        SaveOutcome::Written
    );
    assert_ne!(fs::read(&path).expect("读取修改后内容"), first);
    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded);
    assert_eq!(
        loaded
            .state
            .task("ykt-gateway")
            .expect("任务状态存在")
            .consecutive_failures,
        4
    );
}

#[test]
fn serializes_times_enums_and_explicit_nulls() {
    let dir = tempdir();
    let path = state_path(&dir);
    assert_eq!(
        save(&path, &sample_state()).expect("保存应成功"),
        SaveOutcome::Written
    );

    let value = read_json(&path);
    assert_eq!(value["schema"], json!(1));
    let task = &value["tasks"]["ykt-gateway"];
    assert_eq!(task["desired_override"], json!("paused"));
    assert_eq!(task["consecutive_failures"], json!(3));
    assert_eq!(task["total_starts"], json!(12));
    assert_eq!(task["total_restarts"], json!(2));
    assert_eq!(task["paused_reason"], json!("crash_loop"));
    assert_eq!(task["last_status"], json!("paused"));
    assert_eq!(task["last_exit_code"], json!(7));
    assert_eq!(task["last_exit_at"], json!("2026-11-02T04:40:00Z"));
    assert_eq!(task["last_start_at"], json!("2026-11-02T04:00:00Z"));
    assert_eq!(task["next_retry_at"], json!("2026-11-02T04:50:00Z"));
    assert_eq!(task["last_reason"], json!("process_exit"));
    assert_eq!(task["needs_human"]["active"], json!(true));
    assert_eq!(task["needs_human"]["source"], json!("heartbeat"));
    assert_eq!(task["needs_human"]["message"], json!("login_required"));
    assert_eq!(task["needs_human"]["since"], json!("2026-11-02T04:40:00Z"));
    assert_eq!(
        value["window"],
        json!({"width": 1280, "height": 800, "maximized": true})
    );
    assert_eq!(value["ui"]["last_selected"], json!("ykt-gateway"));

    // 默认任务也显式写 null/0/false（不做 skip_serializing_if），便于人工排查。
    let mut defaults = PersistedState::default();
    defaults.task_mut("idle");
    assert_eq!(
        save(&path, &defaults).expect("保存应成功"),
        SaveOutcome::Written
    );
    let value = read_json(&path);
    let task = &value["tasks"]["idle"];
    assert!(task["desired_override"].is_null());
    assert!(task["paused_reason"].is_null());
    assert!(task["next_retry_at"].is_null());
    assert!(task["last_status"].is_null());
    assert!(task["last_exit_code"].is_null());
    assert!(task["last_exit_at"].is_null());
    assert_eq!(task["consecutive_failures"], json!(0));
    assert_eq!(task["needs_human"]["active"], json!(false));
    assert!(task["needs_human"]["since"].is_null());
}

#[test]
fn timestamps_accept_offsets_and_normalize_to_utc() {
    let dir = tempdir();
    let path = state_path(&dir);
    write_text(
        &path,
        r#"{"schema":1,"tasks":{"t":{
            "last_exit_at":"2026-11-02T12:40:00.250+08:00",
            "last_start_at":"2026-11-02T04:00:00Z",
            "needs_human":{"active":true,"source":"exit_code","since":"2026-11-02T12:40:00.250+08:00"}
        }}}"#,
    );

    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded, "{}", loaded.status);
    let task = loaded.state.task("t").expect("任务状态存在");
    assert_eq!(
        task.last_exit_at,
        Some(ts("2026-11-02T04:40:00.250Z")),
        "带偏移的时间应归一化为 UTC"
    );
    assert_eq!(task.needs_human.since, Some(ts("2026-11-02T04:40:00.250Z")));

    assert_eq!(
        save(&path, &loaded.state).expect("保存应成功"),
        SaveOutcome::Written
    );
    let value = read_json(&path);
    assert_eq!(
        value["tasks"]["t"]["last_exit_at"],
        json!("2026-11-02T04:40:00.250Z")
    );
}

#[test]
fn unknown_paused_reason_and_status_are_preserved() {
    let dir = tempdir();
    let path = state_path(&dir);
    write_text(
        &path,
        r#"{"schema":1,"tasks":{"t":{
            "desired_override":"stopped",
            "paused_reason":"future_reason",
            "last_status":"future_status"
        }}}"#,
    );

    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded, "{}", loaded.status);
    let task = loaded.state.task("t").expect("任务状态存在");
    assert_eq!(
        task.paused_reason,
        Some(PausedReason::Other("future_reason".to_owned()))
    );
    assert_eq!(
        task.last_status,
        Some(LastStatus::Other("future_status".to_owned()))
    );
    assert_eq!(task.desired_override, Some(DesiredOverride::Stopped));

    assert_eq!(
        save(&path, &loaded.state).expect("保存应成功"),
        SaveOutcome::Written
    );
    let value = read_json(&path);
    assert_eq!(value["tasks"]["t"]["paused_reason"], json!("future_reason"));
    assert_eq!(value["tasks"]["t"]["last_status"], json!("future_status"));
}

#[test]
fn save_creates_missing_parent_directories() {
    let dir = tempdir();
    let path = dir.path().join("nested").join("data").join(STATE_FILE_NAME);

    assert_eq!(
        save(&path, &sample_state()).expect("保存应成功"),
        SaveOutcome::Written
    );
    assert!(path.is_file());
    let loaded = load(&path);
    assert_eq!(loaded.status, LoadStatus::Loaded);
    assert_eq!(loaded.state, sample_state());
}

#[test]
fn save_rejects_unknown_schema() {
    let dir = tempdir();
    let path = state_path(&dir);
    let state = PersistedState {
        schema: 2,
        ..PersistedState::default()
    };

    let error = save(&path, &state).expect_err("未知 schema 应拒绝写盘");
    assert!(
        matches!(error, StateStoreError::UnsupportedSchema { found: 2 }),
        "{error}"
    );
    assert!(!path.exists(), "拒绝写盘时不应创建文件");
}

#[test]
fn state_store_wrapper_roundtrip() {
    let dir = tempdir();
    let store = StateStore::in_dir(dir.path());
    assert_eq!(store.path(), path_in(dir.path()));
    assert_eq!(store.path(), state_path(&dir));

    assert_eq!(
        store.save(&sample_state()).expect("保存应成功"),
        SaveOutcome::Written
    );
    let loaded = store.load();
    assert_eq!(loaded.status, LoadStatus::Loaded);
    assert_eq!(loaded.state, sample_state());

    let explicit = StateStore::new(dir.path().join("custom-state.json"));
    assert_eq!(explicit.path(), dir.path().join("custom-state.json"));
    assert_eq!(
        explicit.save(&sample_state()).expect("保存应成功"),
        SaveOutcome::Written
    );
    assert_eq!(explicit.load().state, sample_state());
}
