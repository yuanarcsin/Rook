//! `%VAR%` 环境变量展开。
//!
//! Windows 的环境变量名大小写不敏感，因此查找时先精确匹配、再忽略大小写匹配。
//! `%%` 表示字面量 `%`；未闭合的 `%` 按字面量保留；未知变量是错误。

use std::collections::BTreeMap;
use std::fmt;

/// 环境变量来源。生产用 [`SystemEnv`]，测试用 [`MapEnv`]。
pub trait EnvVars {
    fn var(&self, name: &str) -> Option<String>;
}

/// 进程环境。
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemEnv;

impl EnvVars for SystemEnv {
    fn var(&self, name: &str) -> Option<String> {
        if let Some(value) = std::env::var_os(name) {
            return Some(value.to_string_lossy().into_owned());
        }
        std::env::vars_os()
            .find(|(key, _)| key.to_string_lossy().eq_ignore_ascii_case(name))
            .map(|(_, value)| value.to_string_lossy().into_owned())
    }
}

/// 固定的环境变量集合（键不区分大小写）。
#[derive(Debug, Clone, Default)]
pub struct MapEnv {
    vars: BTreeMap<String, String>,
}

impl MapEnv {
    pub fn new() -> Self {
        Self::default()
    }

    /// 链式设置，便于测试构造。
    pub fn with(mut self, name: &str, value: impl Into<String>) -> Self {
        self.set(name, value);
        self
    }

    pub fn set(&mut self, name: &str, value: impl Into<String>) {
        self.vars.insert(name.to_ascii_uppercase(), value.into());
    }
}

impl EnvVars for MapEnv {
    fn var(&self, name: &str) -> Option<String> {
        self.vars.get(&name.to_ascii_uppercase()).cloned()
    }
}

/// 未知变量。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnknownVar(pub String);

impl fmt::Display for UnknownVar {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "未知环境变量 %{}%", self.0)
    }
}

/// 展开 `%VAR%`。未知变量返回 [`UnknownVar`]，由调用方决定是结构错误还是任务级错误。
pub fn expand(input: &str, env: &dyn EnvVars) -> Result<String, UnknownVar> {
    if !input.contains('%') {
        return Ok(input.to_owned());
    }

    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find('%') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let Some(end) = after.find('%') else {
            // 未闭合的 % 按字面量保留，避免用户写 "100%" 时被迫转义。
            out.push_str(&rest[start..]);
            return Ok(out);
        };

        let name = &after[..end];
        if name.is_empty() {
            out.push('%');
        } else {
            match env.var(name) {
                Some(value) => out.push_str(&value),
                None => return Err(UnknownVar(name.to_owned())),
            }
        }
        rest = &after[end + 1..];
    }

    out.push_str(rest);
    Ok(out)
}
