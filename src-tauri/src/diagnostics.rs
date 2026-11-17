//! 诊断包导出（2026-11-04）：把配置/状态/核心日志/JSONL 事件/任务日志/
//! 心跳复制到 <data>/diagnostics/diag-<时间戳>/，供 agent 或用户离线排查。
//!
//! 只读源文件；不修改配置/状态。2026-11-05 起导出的 config.toml 会把任务 env
//! 里**敏感键**的值打码为 `***`（键名大小写不敏感，匹配 password/passwd/pwd/
//! secret/token/cookie/session/uuid/account/username 等），保持 TOML 可读；
//! 任务日志/心跳等其余文件仍原样复制，summary.md 会提醒分享前人工检查。

use std::fmt::Write as _;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use chrono::Local;
use serde::Serialize;

use crate::logging::CORE_DIR;

/// 单个文件超过该大小时跳过（避免诊断包失控）。
const MAX_FILE_BYTES: u64 = 25 * 1024 * 1024;

/// 诊断包保留份数（最近 5 份）；更旧的目录在导出后/启动/每日清理时删除。
pub const KEEP_EXPORTS: usize = 5;

/// 导出结果（命令 DTO）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiagnosticsResult {
    pub path: String,
    pub files: usize,
}

/// 导出诊断包到数据目录；返回目录路径与复制文件数。
pub fn export(data_dir: &Path, config_path: &Path) -> Result<DiagnosticsResult, String> {
    let stamp = Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut out = data_dir.join("diagnostics").join(format!("diag-{stamp}"));
    let mut suffix = 1u32;
    while out.exists() {
        out = data_dir
            .join("diagnostics")
            .join(format!("diag-{stamp}-{suffix}"));
        suffix += 1;
    }
    fs::create_dir_all(&out)
        .map_err(|error| format!("创建诊断目录 {} 失败：{error}", out.display()))?;
    let mut files = 0usize;
    let mut skipped = Vec::new();
    // 配置副本必须脱敏：任务 env 可能含账号/密码/令牌等凭据。
    let mut config_mask_note = None;
    copy_config_sanitized(
        config_path,
        &out.join("config.toml"),
        &mut files,
        &mut skipped,
        &mut config_mask_note,
    )?;
    copy_file_if_exists(
        &data_dir.join("state.json"),
        &out.join("state.json"),
        &mut files,
        &mut skipped,
    )?;

    let core = data_dir.join(CORE_DIR);
    if core.is_dir() {
        copy_dir(&core, &out.join("core"), &mut files, &mut skipped)?;
    }
    let logs = data_dir.join("logs");
    if logs.is_dir() {
        copy_dir(&logs, &out.join("logs"), &mut files, &mut skipped)?;
    }
    let heartbeats = data_dir.join("heartbeats");
    if heartbeats.is_dir() {
        copy_dir(
            &heartbeats,
            &out.join("heartbeats"),
            &mut files,
            &mut skipped,
        )?;
    }

    let mut summary = String::new();
    let _ = writeln!(&mut summary, "# {} 诊断包", crate::brand::DISPLAY_NAME);
    let _ = writeln!(&mut summary);
    let _ = writeln!(&mut summary, "- 版本：{}", env!("CARGO_PKG_VERSION"));
    let _ = writeln!(&mut summary, "- 导出时间：{}", Local::now().to_rfc3339());
    let _ = writeln!(&mut summary, "- 配置文件：{}", config_path.display());
    let _ = writeln!(&mut summary, "- 数据目录：{}", data_dir.display());
    let _ = writeln!(&mut summary, "- 复制文件：{files}");
    let _ = writeln!(&mut summary, "- 跳过（过大/不可读）：{}", skipped.len());
    let _ = writeln!(&mut summary);
    let _ = writeln!(&mut summary, "## 内容");
    let _ = writeln!(
        &mut summary,
        "- config.toml：配置副本（任务 env 敏感值已脱敏为 ***；仍请分享前检查）"
    );
    if let Some(note) = &config_mask_note {
        let _ = writeln!(&mut summary, "  - 注意：{note}");
    }
    let _ = writeln!(&mut summary, "- state.json：监督状态快照");
    let _ = writeln!(
        &mut summary,
        "- core/：人类可读核心日志 + JSONL 事件日志 + 崩溃日志"
    );
    let _ = writeln!(&mut summary, "- logs/<task>/：任务 stdout/stderr 日志");
    let _ = writeln!(&mut summary, "- heartbeats/：脚本心跳文件");
    let _ = writeln!(&mut summary);
    let _ = writeln!(&mut summary, "## 脱敏说明");
    let _ = writeln!(
        &mut summary,
        "任务 env 中键名含 password/passwd/pwd/secret/token/cookie/session/uuid/account/username 等敏感词的取值已替换为 ***。"
    );
    let _ = writeln!(
        &mut summary,
        "任务日志（logs/）可能包含脚本自己打印的凭据或二维码内容，分享前仍需人工检查。"
    );
    let _ = writeln!(&mut summary);
    let _ = writeln!(&mut summary, "## 查询建议");
    let _ = writeln!(&mut summary, "Get-Content core/events.jsonl -Tail 200");
    let _ = writeln!(&mut summary, "Get-Content core/rook.log -Tail 200");
    fs::write(out.join("summary.md"), summary)
        .map_err(|error| format!("写诊断摘要失败：{error}"))?;
    files += 1;

    // 导出成功后只保留最近 5 份；清理失败只 warn，不影响本次导出结果。
    if let Err(error) = cleanup_old_exports(data_dir, KEEP_EXPORTS) {
        tracing::warn!(%error, "清理旧诊断包失败");
    }

    Ok(DiagnosticsResult {
        path: out.to_string_lossy().into_owned(),
        files,
    })
}

/// 删除 `<data>/diagnostics` 下除最近 `keep` 份之外的导出目录。
///
/// 目录名 `diag-<YYYYMMDD-HHMMSS>[-n]` 按字典序即时间序；无法识别的目录/文件
/// 不动。单个目录删除失败只 warn 并继续，返回成功删除的数量。
pub fn cleanup_old_exports(data_dir: &Path, keep: usize) -> io::Result<usize> {
    let root = data_dir.join("diagnostics");
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error),
    };
    let mut exports: Vec<(String, PathBuf)> = Vec::new();
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("diag-") {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        exports.push((name, entry.path()));
    }
    // 最新的排前面；保留前 `keep` 份。
    exports.sort_by(|left, right| right.0.cmp(&left.0));

    let mut removed = 0usize;
    for (_, path) in exports.into_iter().skip(keep) {
        match fs::remove_dir_all(&path) {
            Ok(()) => removed += 1,
            Err(error) => {
                tracing::warn!(path = %path.display(), %error, "删除旧诊断包失败");
            }
        }
    }
    if removed > 0 {
        tracing::info!(
            event = "logs.cleanup",
            scope = "diagnostics",
            removed,
            keep,
            "清理旧诊断包"
        );
    }
    Ok(removed)
}

/// 任务 env 里可能含凭据的键名片段（大小写不敏感的子串匹配）。
///
/// 只匹配明确的凭据语义词，避免把 PORT/HOST 这类普通配置误伤。
const SENSITIVE_ENV_MARKERS: [&str; 15] = [
    "password",
    "passwd",
    "pwd",
    "secret",
    "token",
    "cookie",
    "session",
    "uuid",
    "account",
    "username",
    "credential",
    "auth",
    "apikey",
    "api_key",
    "private_key",
];

/// 键名是否敏感（大小写不敏感）。
pub(crate) fn is_sensitive_env_key(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    SENSITIVE_ENV_MARKERS
        .iter()
        .any(|marker| lower.contains(marker))
}

/// 把配置文本里任务 env 的敏感值替换为 `***`（保留 TOML 结构与注释）。
///
/// 同时支持 `env = { K = "v" }` 内联表与 `[tasks.env]` 子表；非敏感键、
/// 任务其它字段原样保留。解析失败返回 `Err`，调用方退回原样复制。
pub(crate) fn sanitize_config_text(text: &str) -> Result<String, String> {
    let mut document = text
        .parse::<toml_edit::DocumentMut>()
        .map_err(|error| format!("配置 TOML 解析失败：{error}"))?;
    if let Some(tasks) = document
        .get_mut("tasks")
        .and_then(toml_edit::Item::as_array_of_tables_mut)
    {
        for task in tasks.iter_mut() {
            if let Some(env) = task.get_mut("env") {
                mask_env_item(env);
            }
        }
    }
    Ok(document.to_string())
}

/// 对单个 `env` 项（内联表或子表）做敏感值打码。
fn mask_env_item(item: &mut toml_edit::Item) {
    let Some(table) = item.as_table_like_mut() else {
        return;
    };
    for (key, value) in table.iter_mut() {
        if is_sensitive_env_key(key.get()) {
            *value = toml_edit::Item::Value(toml_edit::Value::from("***"));
        }
    }
}

/// 复制配置副本并脱敏；解析/读取失败时退回原样复制并记录原因。
fn copy_config_sanitized(
    source: &Path,
    target: &Path,
    files: &mut usize,
    skipped: &mut Vec<PathBuf>,
    mask_note: &mut Option<String>,
) -> Result<(), String> {
    if !source.is_file() {
        return Ok(());
    }
    let metadata = fs::metadata(source)
        .map_err(|error| format!("读取 {} 元数据失败：{error}", source.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        skipped.push(source.to_path_buf());
        return Ok(());
    }
    let text = match fs::read_to_string(source) {
        Ok(text) => text,
        Err(error) => {
            *mask_note = Some(format!(
                "读取配置失败，config.toml 未脱敏（按原样复制）：{error}"
            ));
            return copy_one(source, target, files, skipped);
        }
    };
    match sanitize_config_text(&text) {
        Ok(sanitized) => {
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
            }
            fs::write(target, sanitized)
                .map_err(|error| format!("写脱敏后的配置 {} 失败：{error}", target.display()))?;
            *files += 1;
            Ok(())
        }
        Err(error) => {
            *mask_note = Some(format!(
                "配置解析失败，config.toml 未脱敏（按原样复制），请人工检查：{error}"
            ));
            copy_one(source, target, files, skipped)
        }
    }
}

fn copy_file_if_exists(
    source: &Path,
    target: &Path,
    files: &mut usize,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !source.is_file() {
        return Ok(());
    }
    copy_one(source, target, files, skipped)
}

fn copy_one(
    source: &Path,
    target: &Path,
    files: &mut usize,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), String> {
    let metadata = fs::metadata(source)
        .map_err(|error| format!("读取 {} 元数据失败：{error}", source.display()))?;
    if metadata.len() > MAX_FILE_BYTES {
        skipped.push(source.to_path_buf());
        return Ok(());
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)
            .map_err(|error| format!("创建 {} 失败：{error}", parent.display()))?;
    }
    fs::copy(source, target).map_err(|error| {
        format!(
            "复制 {} → {} 失败：{error}",
            source.display(),
            target.display()
        )
    })?;
    *files += 1;
    Ok(())
}

/// 递归复制目录；保持相对路径，跳过超大文件与不可读目录。
fn copy_dir(
    source: &Path,
    target: &Path,
    files: &mut usize,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), String> {
    fs::create_dir_all(target)
        .map_err(|error| format!("创建 {} 失败：{error}", target.display()))?;
    let entries = match fs::read_dir(source) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => {
            return Err(format!("读取目录 {} 失败：{error}", source.display()));
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = entry.file_name();
        let destination = target.join(&name);
        let file_type = match entry.file_type() {
            Ok(file_type) => file_type,
            Err(_) => continue,
        };
        if file_type.is_dir() {
            copy_dir(&path, &destination, files, skipped)?;
        } else if file_type.is_file() {
            copy_one(&path, &destination, files, skipped)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CONFIG: &str = r#"version = 1
# 顶部注释必须保留
[app]
confirm_exit = true

[[tasks]]
id = "ykt"
name = "雨课堂网关"
command = "node"
working_dir = "D:/scripts/ykt"
env = { YKT_ACCOUNT = "user@example.com", YKT_PASSWORD = "hunter2", PORT = "3300", api_token = "abc123" }

[[tasks]]
id = "zhs"
name = "智慧树"
command = "python"
working_dir = "D:/scripts/zhs"

[tasks.env]
ZHS_USERNAME = "alice"
ZHS_SESSION = "cookie-value"
PORT = "3301"
"#;

    fn parse_env(text: &str, task: usize) -> toml::map::Map<String, toml::Value> {
        let document: toml::Value = toml::from_str(text).expect("脱敏后仍是合法 TOML");
        document["tasks"][task]["env"]
            .as_table()
            .expect("env 表")
            .clone()
    }

    /// 内联 env 表：敏感键打码，普通键原样保留，注释/其它字段保留。
    #[test]
    fn sanitize_masks_inline_env_table() {
        let sanitized = sanitize_config_text(CONFIG).expect("应能脱敏");
        assert!(sanitized.contains("# 顶部注释必须保留"), "注释应保留");
        assert!(sanitized.contains(r#"command = "node""#));
        assert!(!sanitized.contains("hunter2"), "密码不得出现在诊断副本");
        assert!(!sanitized.contains("abc123"), "token 不得出现");

        let first = parse_env(&sanitized, 0);
        assert_eq!(first["YKT_ACCOUNT"].as_str(), Some("***"));
        assert_eq!(first["YKT_PASSWORD"].as_str(), Some("***"));
        assert_eq!(first["api_token"].as_str(), Some("***"));
        assert_eq!(first["PORT"].as_str(), Some("3300"), "非敏感键不误伤");
    }

    /// `[tasks.env]` 子表形式同样打码；任务名等其它字段不误伤。
    #[test]
    fn sanitize_masks_env_subtable() {
        let sanitized = sanitize_config_text(CONFIG).expect("应能脱敏");
        let second = parse_env(&sanitized, 1);
        assert_eq!(second["ZHS_USERNAME"].as_str(), Some("***"));
        assert_eq!(second["ZHS_SESSION"].as_str(), Some("***"));
        assert_eq!(second["PORT"].as_str(), Some("3301"));

        // name/id/command 之类的非 env 字段不能因为含敏感词被打码。
        assert!(sanitized.contains(r#"name = "雨课堂网关""#));
        assert!(sanitized.contains(r#"id = "ykt""#));
        assert!(sanitized.contains(r#"id = "zhs""#));
    }

    /// 键名匹配大小写不敏感；明显非敏感键不误伤。
    #[test]
    fn sensitive_key_matching_is_case_insensitive_and_precise() {
        for key in [
            "YKT_PASSWORD",
            "ykt_passwd",
            "DB_PWD",
            "client_secret",
            "ACCESS_TOKEN",
            "Cookie",
            "SESSION_ID",
            "CLIENT_UUID",
            "ACCOUNT_NAME",
            "UserName",
            "AWS_CREDENTIAL",
            "api_key",
            "OpenAI_ApiKey",
            "PRIVATE_KEY",
        ] {
            assert!(is_sensitive_env_key(key), "{key} 应视为敏感");
        }
        for key in ["PORT", "HOST", "LOG_LEVEL", "WORK_DIR", "RETRY_COUNT"] {
            assert!(!is_sensitive_env_key(key), "{key} 不应视为敏感");
        }
    }

    /// 非 TOML 文本：返回错误，由导出流程退回原样复制（不崩溃）。
    #[test]
    fn sanitize_rejects_invalid_toml() {
        assert!(sanitize_config_text("not = [valid").is_err());
    }

    /// 端到端：诊断包里的 config.toml 已脱敏，summary 注明脱敏。
    #[test]
    fn export_writes_sanitized_config_and_summary_note() {
        let dir = tempfile::tempdir().expect("临时目录");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).expect("数据目录");
        let config_path = dir.path().join("Rook.toml");
        fs::write(&config_path, CONFIG).expect("写配置");

        let result = export(&data_dir, &config_path).expect("导出诊断包");
        assert!(result.files >= 2, "至少 config + summary：{}", result.files);

        let out = PathBuf::from(&result.path);
        let masked = fs::read_to_string(out.join("config.toml")).expect("读取脱敏配置");
        assert!(masked.contains("***"));
        assert!(!masked.contains("hunter2"));
        assert!(!masked.contains("cookie-value"));

        let summary = fs::read_to_string(out.join("summary.md")).expect("读取摘要");
        assert!(summary.contains("脱敏"), "{summary}");
        assert!(summary.contains("日志"), "摘要应提醒任务日志仍需人工检查");
    }

    /// 导出不存在的配置：不报错，也不生成 config.toml。
    #[test]
    fn export_without_config_file_is_ok() {
        let dir = tempfile::tempdir().expect("临时目录");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).expect("数据目录");
        let result = export(&data_dir, &dir.path().join("missing.toml")).expect("导出");
        assert!(!PathBuf::from(&result.path).join("config.toml").exists());
    }

    /// 连续导出 6 次后只保留最近 5 份诊断目录。
    #[test]
    fn export_keeps_only_latest_five_directories() {
        let dir = tempfile::tempdir().expect("临时目录");
        let data_dir = dir.path().join("data");
        fs::create_dir_all(&data_dir).expect("数据目录");
        let config = dir.path().join("missing.toml");

        let mut paths = Vec::new();
        for _ in 0..6 {
            let result = export(&data_dir, &config).expect("导出诊断包");
            paths.push(PathBuf::from(result.path));
        }

        let exports: Vec<PathBuf> = fs::read_dir(data_dir.join("diagnostics"))
            .expect("读取诊断目录")
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        assert_eq!(exports.len(), 5, "只保留最近 5 份：{exports:?}");
        assert!(!paths[0].exists(), "最早的一份应被删除");
        assert!(paths[5].exists(), "最新的一份必须保留");
    }

    /// 无法识别的目录/文件不参与诊断包清理。
    #[test]
    fn cleanup_keeps_foreign_entries() {
        let dir = tempfile::tempdir().expect("临时目录");
        let data_dir = dir.path().join("data");
        let diagnostics = data_dir.join("diagnostics");
        fs::create_dir_all(diagnostics.join("keep-me")).expect("创建目录");
        fs::create_dir_all(diagnostics.join("diag-20260101-000000")).expect("创建目录");
        fs::create_dir_all(diagnostics.join("diag-20260102-000000")).expect("创建目录");
        fs::write(diagnostics.join("notes.txt"), b"x").expect("写文件");

        let removed = cleanup_old_exports(&data_dir, 1).expect("清理");
        assert_eq!(removed, 1);
        assert!(diagnostics.join("keep-me").exists());
        assert!(!diagnostics.join("diag-20260101-000000").exists());
        assert!(diagnostics.join("diag-20260102-000000").exists());
        assert!(diagnostics.join("notes.txt").exists());
    }
}
