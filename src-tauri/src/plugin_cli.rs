//! 无头插件注册（`--import-plugin`）：在 Tauri/托盘启动前把清单导入或同步到配置。
//!
//! 为什么单独成模块：
//! - 真实测试/首次部署时可以先注册 ykt、zhs、CampusNetwork，再双击打开应用；
//! - 不创建窗口/托盘，不触发任务启动，适合脚本化与静默验收；
//! - 复用已有的 plugin::import_plugin / sync_plugin，保留注释、revision 冲突检测与完整校验。

use std::path::{Path, PathBuf};

use crate::config::{self, plugin};

/// 一次清单注册的结果（用于命令行日志/测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportReport {
    pub manifest: PathBuf,
    pub task_id: String,
    /// 配置文件内容是否真的变化。
    pub changed: bool,
    /// true 表示任务已存在、走了同步更新；false 表示新建任务。
    pub synced: bool,
}

/// 把多个清单依次导入/同步到同一份配置。
///
/// - 任务不存在：新建 `[[tasks]]`；
/// - 任务已存在（同 plugin.id）：按清单拥有的字段同步，保留用户偏好；
/// - 每次写入后使用返回的新 revision，避免多个清单之间互相触发冲突；
/// - 空列表是 no-op；不启动任何任务。
pub fn import_plugins(
    config_path: &Path,
    manifests: &[PathBuf],
    data_dir: Option<&Path>,
) -> Result<Vec<ImportReport>, String> {
    if manifests.is_empty() {
        return Ok(Vec::new());
    }
    if !config_path.is_file() {
        config::write_default_config(config_path)
            .map_err(|error| format!("创建默认配置失败：{error}"))?;
    }

    let loaded = config::load(&config::Overrides {
        config: Some(config_path),
        data_dir,
    })
    .map_err(|error| format!("加载配置失败：{error}"))?;
    let mut revision = config::read_config_text(config_path)
        .map_err(|error| format!("读取配置 revision 失败：{error}"))?
        .revision;
    let mut existing: Vec<String> = loaded
        .config
        .tasks
        .iter()
        .map(|task| task.id.clone())
        .collect();
    let mut reports = Vec::with_capacity(manifests.len());

    for manifest in manifests {
        let preview = plugin::load_preview(manifest, &existing)
            .map_err(|error| format!("预览 {} 失败：{error}", manifest.display()))?;
        let (outcome, synced) = match preview.existing_task_id.as_deref() {
            Some(task_id) => (
                plugin::sync_plugin(config_path, task_id, &revision, data_dir),
                true,
            ),
            None => (
                plugin::import_plugin(config_path, manifest, &revision, data_dir),
                false,
            ),
        };
        let outcome =
            outcome.map_err(|error| format!("注册 {} 失败：{error}", manifest.display()))?;
        revision = outcome.revision.clone();
        if !existing.iter().any(|id| id == &outcome.task_id) {
            existing.push(outcome.task_id.clone());
        }
        reports.push(ImportReport {
            manifest: manifest.clone(),
            task_id: outcome.task_id,
            changed: outcome.changed,
            synced,
        });
    }

    Ok(reports)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_manifest(dir: &Path) -> PathBuf {
        let path = dir.join(plugin::MANIFEST_NAME);
        let text = r#"schema = 1

[plugin]
id = "demo"
name = "演示脚本"
version = "0.1.0"

[run]
command = "node"
args = ["server.js"]
working_dir = "."

[health]
type = "heartbeat"
interval_ms = 5000
max_age_ms = 30000
startup_grace_ms = 20000
"#;
        std::fs::write(&path, text).expect("写清单");
        path
    }

    #[test]
    fn imports_then_syncs_idempotently() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "version = 1").expect("写配置");
        let plugin_dir = temp.path().join("demo");
        std::fs::create_dir_all(&plugin_dir).expect("插件目录");
        let manifest = write_manifest(&plugin_dir);

        let first = import_plugins(
            &config_path,
            std::slice::from_ref(&manifest),
            Some(temp.path()),
        )
        .expect("首次导入");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].task_id, "demo");
        assert!(first[0].changed);
        assert!(!first[0].synced);
        let text = std::fs::read_to_string(&config_path).expect("读配置");
        assert!(text.contains(r#"id = "demo""#), "{text}");

        let second =
            import_plugins(&config_path, &[manifest], Some(temp.path())).expect("重复导入走同步");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].task_id, "demo");
        assert!(second[0].synced);
        assert!(!second[0].changed);
    }

    #[test]
    fn empty_manifest_list_is_noop() {
        let temp = tempfile::tempdir().expect("tempdir");
        let config_path = temp.path().join("config.toml");
        std::fs::write(&config_path, "version = 1").expect("写配置");
        let reports = import_plugins(&config_path, &[], Some(temp.path())).expect("no-op");
        assert!(reports.is_empty());
        assert_eq!(
            std::fs::read_to_string(&config_path).expect("读配置"),
            "version = 1"
        );
    }
}
