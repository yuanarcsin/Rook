//! 统一 IPC 错误 DTO（A4）。
//!
//! 所有 `#[tauri::command]` 的 reject 值都是 [`IpcError`]：字段固定为
//! `stage` / `code` / `message` / `taskId` / `currentRevision` / `line` / `col`，
//! 不适用的字段一律序列化为 `null`（不使用 `skip_serializing_if`），
//! 前端按 `field: T | null` 读取即可，无需区分 8 套历史形状。
//!
//! `stage` 仍是各命令自己的稳定英文标识，取值与文案不做任何改写。

use serde::Serialize;

/// 命令失败时返回给前端的统一错误 DTO。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IpcError {
    /// 稳定英文阶段标识，如 `"config"` / `"parse"` / `"conflict"` / `"join"`。
    pub stage: String,
    /// 底层系统错误码；没有系统码时为 `None`。
    pub code: Option<u32>,
    /// 面向用户的中文说明。
    pub message: String,
    /// 相关任务 id（如果有）。
    pub task_id: Option<String>,
    /// 冲突时磁盘上的最新 revision；其他阶段为 `None`。
    pub current_revision: Option<String>,
    /// 结构错误的行（1 起）；其他阶段为 `None`。
    ///
    /// 用 `u32` 而不是 `usize`：`Option<usize>` 会因对齐占 16 字节，把整个
    /// DTO 推过 clippy 的 `result_large_err` 阈值；行号不可能超过 `u32`。
    pub line: Option<u32>,
    /// 结构错误的列（1 起）；其他阶段为 `None`。
    pub col: Option<u32>,
}

impl IpcError {
    /// 只有 `stage` + `message` 的错误（配置/草稿/时段/事件日志/诊断等）。
    pub fn new(stage: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            stage: stage.into(),
            code: None,
            message: message.into(),
            task_id: None,
            current_revision: None,
            line: None,
            col: None,
        }
    }

    /// 带任务 id 的错误（日志读取、插件命令）。
    pub fn with_task(
        stage: impl Into<String>,
        message: impl Into<String>,
        task_id: Option<String>,
    ) -> Self {
        Self {
            task_id,
            ..Self::new(stage, message)
        }
    }

    /// 启停/路径类命令错误：带系统码与任务 id。
    pub fn with_code(
        stage: impl Into<String>,
        code: Option<u32>,
        message: impl Into<String>,
        task_id: Option<String>,
    ) -> Self {
        Self {
            code,
            task_id,
            ..Self::new(stage, message)
        }
    }

    /// revision 冲突错误：只填 `current_revision`。
    pub fn conflict(
        stage: impl Into<String>,
        message: impl Into<String>,
        current_revision: impl Into<String>,
    ) -> Self {
        Self {
            current_revision: Some(current_revision.into()),
            ..Self::new(stage, message)
        }
    }

    /// 结构错误：填行/列（两者都可能为 `None`）。
    pub fn parse(
        stage: impl Into<String>,
        message: impl Into<String>,
        line: Option<usize>,
        col: Option<usize>,
    ) -> Self {
        Self {
            line: line.and_then(|value| u32::try_from(value).ok()),
            col: col.and_then(|value| u32::try_from(value).ok()),
            ..Self::new(stage, message)
        }
    }

    /// 运行时错误 → 命令错误（启停命令）。
    #[cfg(windows)]
    pub fn from_runtime(task_id: &str, error: &crate::runtime::RuntimeError) -> Self {
        Self::with_code(
            error.stage().key(),
            error.code(),
            error.message(),
            Some(task_id.to_owned()),
        )
    }

    /// 插件错误 → 命令错误（可附带相关任务 id）。
    pub fn from_plugin(error: crate::config::plugin::PluginError, task_id: Option<String>) -> Self {
        let current_revision = match &error {
            crate::config::plugin::PluginError::Conflict { current_revision } => {
                Some(current_revision.clone())
            }
            _ => None,
        };
        Self {
            current_revision,
            ..Self::with_task(error.stage(), error.to_string(), task_id)
        }
    }
}

/// 编辑器错误（配置文本）→ 统一错误。
impl From<crate::config::EditorError> for IpcError {
    fn from(error: crate::config::EditorError) -> Self {
        use crate::config::EditorError;
        match error {
            EditorError::Conflict { current_revision } => IpcError::conflict(
                "conflict",
                "文件已被外部修改，请重新加载后再保存",
                current_revision,
            ),
            EditorError::Parse {
                message, line, col, ..
            } => IpcError::parse("parse", message, line, col),
            // 恢复备份专用：没有 `.bak` 或备份内容不可用。
            EditorError::BackupMissing { .. } => IpcError::new("backup", error.to_string()),
            EditorError::Read { .. } | EditorError::Write { .. } => {
                IpcError::new(error.stage(), error.to_string())
            }
        }
    }
}

/// 草稿错误 → 统一错误。
impl From<crate::config::DraftError> for IpcError {
    fn from(error: crate::config::DraftError) -> Self {
        use crate::config::DraftError;
        let stage = error.stage();
        match error {
            DraftError::Conflict { current_revision } => IpcError::conflict(
                "conflict",
                "文件已被外部修改，请重新加载后再保存",
                current_revision,
            ),
            DraftError::Parse {
                message, line, col, ..
            }
            | DraftError::Validate {
                message, line, col, ..
            } => IpcError::parse(stage, message, line, col),
            other => IpcError::new(other.stage(), other.to_string()),
        }
    }
}

/// 运行时段编辑错误 → 统一错误。
impl From<crate::config::ScheduleEditError> for IpcError {
    fn from(error: crate::config::ScheduleEditError) -> Self {
        let (line, col) = error.line_col();
        Self {
            current_revision: error.current_revision().map(str::to_owned),
            line: line.and_then(|value| u32::try_from(value).ok()),
            col: col.and_then(|value| u32::try_from(value).ok()),
            ..Self::new(error.stage(), error.to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serializes_all_seven_camel_case_keys_with_nulls() {
        let value = serde_json::to_value(IpcError::new("config", "boom")).unwrap();
        let object = value.as_object().unwrap();
        assert_eq!(object.len(), 7);
        assert_eq!(object["stage"], "config");
        assert_eq!(object["message"], "boom");
        for key in ["code", "taskId", "currentRevision", "line", "col"] {
            assert!(object[key].is_null(), "{key} 应为 null");
        }
        // 不再出现 snake_case 键。
        assert!(!object.contains_key("task_id"));
        assert!(!object.contains_key("current_revision"));
    }

    #[test]
    fn with_code_keeps_code_and_task_id() {
        let error = IpcError::with_code("spawn", Some(740), "启动失败", Some("demo".into()));
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["code"], 740);
        assert_eq!(value["taskId"], "demo");
        assert_eq!(value["stage"], "spawn");
    }

    #[test]
    fn conflict_only_fills_current_revision() {
        let error = IpcError::conflict("conflict", "已过期", "abc");
        let value = serde_json::to_value(&error).unwrap();
        assert_eq!(value["currentRevision"], "abc");
        assert_eq!(value["stage"], "conflict");
        assert!(value["code"].is_null());
        assert!(value["taskId"].is_null());
        assert!(value["line"].is_null());
        assert!(value["col"].is_null());
    }

    #[test]
    fn parse_and_with_task_fill_their_own_fields() {
        let parsed = IpcError::parse("parse", "语法错误", Some(3), Some(7));
        assert_eq!(parsed.line, Some(3));
        assert_eq!(parsed.col, Some(7));
        assert_eq!(parsed.current_revision, None);

        let task = IpcError::with_task("log_read", "读取失败", Some("demo".into()));
        assert_eq!(task.task_id.as_deref(), Some("demo"));
        assert_eq!(task.code, None);
        assert_eq!(task.line, None);
    }
}
