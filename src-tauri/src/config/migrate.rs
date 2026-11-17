//! 配置 schema 的逐版本迁移。
//!
//! v2 与 v1 的配置字段相同，只提升版本号。加载迁移发生在内存中，原文件不会在
//! 加载时被静默改写；下次通过编辑器保存时才由 `editor::commit` 写入 v2。
//! 运行状态文件 `state.json` 不随配置升版：其 schema 仍为 1，任务 id 保留即可。

use std::fs;
use std::path::{Path, PathBuf};

use super::model::SCHEMA_VERSION;

/// 把已知旧版本迁移到当前 schema；字段全部保留。
pub fn migrate_value(mut value: toml::Value, from: i64) -> Result<toml::Value, String> {
    match from {
        2 => Ok(value),
        1 => {
            let table = value
                .as_table_mut()
                .ok_or_else(|| "配置迁移失败：顶层不是 TOML 表".to_owned())?;
            table.insert("version".to_owned(), toml::Value::Integer(SCHEMA_VERSION));
            Ok(value)
        }
        _ => Err(format!("不支持的配置迁移来源版本 {from}")),
    }
}

/// 迁移前的备份路径：同目录 `<文件名>.v{from}.bak`。
pub fn backup_path(path: &Path, from: i64) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(format!(".v{from}.bak"));
    path.with_file_name(name)
}

/// 迁移前尽力保留原始文件；已有备份不覆盖，失败只告警，不阻止加载。
pub fn backup_before_migrate(path: &Path, from: i64) {
    let backup = backup_path(path, from);
    if backup.exists() {
        tracing::warn!(
            config = %path.display(),
            backup = %backup.display(),
            "配置迁移备份已存在，保留原备份并继续加载"
        );
        return;
    }
    if let Err(error) = fs::copy(path, &backup) {
        tracing::warn!(
            config = %path.display(),
            backup = %backup.display(),
            %error,
            "配置迁移前备份失败（继续加载）"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrates_v1_version_and_preserves_every_other_key() {
        let value: toml::Value =
            toml::from_str("version = 1\nname = \"demo\"\n\n[tasks.restart]\nwindow_ms = 600000\n")
                .expect("解析 v1");

        let migrated = migrate_value(value, 1).expect("迁移 v1");
        assert_eq!(
            migrated.get("version").and_then(toml::Value::as_integer),
            Some(2)
        );
        assert_eq!(
            migrated.get("name").and_then(toml::Value::as_str),
            Some("demo")
        );
        assert_eq!(
            migrated
                .get("tasks")
                .and_then(toml::Value::as_table)
                .and_then(|tasks| tasks.get("restart"))
                .and_then(toml::Value::as_table)
                .and_then(|restart| restart.get("window_ms"))
                .and_then(toml::Value::as_integer),
            Some(600_000)
        );
    }

    #[test]
    fn current_version_is_returned_unchanged() {
        let value: toml::Value = toml::from_str("version = 2\nname = \"demo\"\n").expect("解析 v2");
        assert_eq!(migrate_value(value.clone(), 2), Ok(value));
    }

    #[test]
    fn unsupported_source_version_is_an_error() {
        let value: toml::Value = toml::from_str("version = 3\n").expect("解析 v3");
        let error = migrate_value(value, 3).expect_err("v3 不应迁移");
        assert!(error.contains('3'), "{error}");
    }

    #[test]
    fn backup_name_contains_source_version() {
        assert_eq!(
            backup_path(Path::new("config.toml"), 1),
            PathBuf::from("config.toml.v1.bak")
        );
    }
}
