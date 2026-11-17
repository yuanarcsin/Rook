//! 环境块：继承父进程环境 + 覆盖项，构造 `CREATE_UNICODE_ENVIRONMENT` 需要的 UTF-16 块。
//!
//! 格式：一串 `KEY=VALUE\0`，最后再补一个 `\0`。Windows 的环境变量名大小写不敏感，
//! 因此覆盖按 ASCII 大小写折叠匹配；块按名字排序（官方建议，便于系统与调试工具处理）。
//! 以 `=` 开头的驱动器当前目录变量（如 `=C:`）不继承，也不允许覆盖项使用。

use std::ffi::{OsStr, OsString};
use std::os::windows::ffi::OsStrExt;

/// 构造环境块（含结尾双 NUL）。
pub fn build_environment_block(overrides: &[(OsString, OsString)]) -> Vec<u16> {
    let mut entries: Vec<(OsString, OsString)> = Vec::new();
    for (key, value) in std::env::vars_os() {
        if is_valid_key(&key) {
            entries.push((key, value));
        }
    }

    for (key, value) in overrides {
        if !is_valid_key(key) {
            continue;
        }
        match entries
            .iter_mut()
            .find(|(existing, _)| keys_equal(existing, key))
        {
            Some(slot) => slot.1 = value.clone(),
            None => entries.push((key.clone(), value.clone())),
        }
    }

    entries.sort_by_key(|(key, _)| fold_key(key));

    let mut block = Vec::new();
    for (key, value) in &entries {
        block.extend(key.encode_wide());
        block.push(u16::from(b'='));
        block.extend(value.encode_wide());
        block.push(0);
    }
    block.push(0);
    block
}

/// 空名字或含 `=` 的名字无法构成 `KEY=VALUE`，直接忽略。
fn is_valid_key(key: &OsStr) -> bool {
    !key.is_empty() && !key.to_string_lossy().contains('=')
}

fn keys_equal(left: &OsStr, right: &OsStr) -> bool {
    fold_key(left) == fold_key(right)
}

/// 仅对 ASCII 字母做大小写折叠：环境变量名在实践中都是 ASCII，非 ASCII 原样比较。
fn fold_key(key: &OsStr) -> Vec<u16> {
    key.encode_wide()
        .map(|unit| {
            if (u16::from(b'a')..=u16::from(b'z')).contains(&unit) {
                unit - (u16::from(b'a') - u16::from(b'A'))
            } else {
                unit
            }
        })
        .collect()
}
