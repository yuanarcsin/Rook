//! 配置错误类型与"问题"条目。
//!
//! 两类问题严格区分：
//! - [`ConfigError`]：结构错误，整份配置不可用（带文件、行、列）。
//! - [`ConfigIssue`]：任务级错误或警告，配置照常加载。

use std::fmt;
use std::path::PathBuf;

use serde::Serialize;

/// 问题严重级别。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub enum Severity {
    /// 任务级错误：该任务 invalid，不能启动，其余任务不受影响。
    Error,
    /// 提示：配置照常生效。
    Warning,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
        }
    }
}

/// 任务级错误或警告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ConfigIssue {
    pub severity: Severity,
    /// 出问题的任务 id；顶层字段为 `None`。
    pub task_id: Option<String>,
    /// 字段路径，如 `tasks[0].health.url`。
    pub path: String,
    pub message: String,
    /// 行号（1 起），定位失败时为 `None`。
    pub line: Option<usize>,
    /// 列号（1 起），定位失败时为 `None`。
    pub col: Option<usize>,
}

impl fmt::Display for ConfigIssue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.line, self.col) {
            (Some(line), Some(col)) => write!(f, "{}:{line}:{col}: ", self.path),
            _ => write!(f, "{}: ", self.path),
        }?;
        write!(f, "{}", self.message)?;
        if let Some(task_id) = &self.task_id {
            write!(f, "（任务 {task_id}）")?;
        }
        Ok(())
    }
}

/// 结构错误：整份配置不可用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    pub file: PathBuf,
    /// 行号（1 起）。
    pub line: usize,
    /// 列号（1 起）。
    pub col: usize,
    /// 字段路径；serde 层错误可能为空。
    pub path: String,
    pub message: String,
}

impl ConfigError {
    /// 生成 `file:line:col` 前缀，便于直接拼进界面/日志。
    pub fn location(&self) -> String {
        format!("{}:{}:{}", self.file.display(), self.line, self.col)
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.location(), self.message)?;
        if !self.path.is_empty() {
            write!(f, "（字段 {}）", self.path)?;
        }
        Ok(())
    }
}

impl std::error::Error for ConfigError {}

/// 加载配置时的错误。
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// 读写配置/数据目录失败。
    #[error("配置 I/O 失败：{path}：{source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    /// 结构错误：整份配置不可用。
    #[error("{0}")]
    Invalid(#[from] ConfigError),
}

impl LoadError {
    pub(crate) fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    /// 结构错误详情；`Io` 错误返回 `None`。
    pub fn structural(&self) -> Option<&ConfigError> {
        match self {
            Self::Invalid(error) => Some(error),
            Self::Io { .. } => None,
        }
    }
}
