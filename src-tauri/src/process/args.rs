//! Windows 命令行引号：与 `CommandLineToArgvW` 的解析规则互逆。
//!
//! 规则（Microsoft Learn "Parsing C++ command-line arguments"）：
//! - 参数以空格/制表符分隔；双引号包裹可包含空白；
//! - 引号前的 `2n` 个反斜杠 → `n` 个字面反斜杠；引号前的 `2n+1` 个 → `n` 个反斜杠 + 字面引号；
//! - 不在引号前的反斜杠按字面保留；
//! - 空参数必须写成 `""`。
//!
//! 因此“只在需要时加引号，并且在闭引号前把反斜杠加倍”是关键：
//! `C:\dir with space\` 必须变成 `"C:\dir with space\\"`，否则结尾的反斜杠会转义闭引号。

use std::ffi::OsStr;

/// 给单个参数加引号（必要时）。返回的字符串可直接拼进命令行。
pub fn quote_arg(arg: &str) -> String {
    if arg.is_empty() {
        return "\"\"".to_owned();
    }
    // 没有空白/引号时原样输出：反斜杠不需要加倍。
    if !arg
        .chars()
        .any(|ch| matches!(ch, ' ' | '\t' | '\n' | '\u{0b}' | '"'))
    {
        return arg.to_owned();
    }

    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for ch in arg.chars() {
        match ch {
            '\\' => backslashes += 1,
            '"' => {
                // 字面引号：前面的反斜杠加倍，再补一个转义引号的反斜杠。
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            _ => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(ch);
                backslashes = 0;
            }
        }
    }
    // 闭引号前的尾随反斜杠必须加倍，否则会转义闭引号。
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

/// 构造传给 `CreateProcessW` 的 UTF-16 命令行（含结尾 NUL）。
///
/// 第一个 token 是 program（同时作为子进程的 argv[0]）；`lpApplicationName` 另行传入，
/// 因此这里不做 PATH 查找。
pub fn build_command_line(program: &OsStr, args: &[String]) -> Vec<u16> {
    let program = program.to_string_lossy();
    let mut line = quote_arg(&program);
    for arg in args {
        line.push(' ');
        line.push_str(&quote_arg(arg));
    }
    let mut wide: Vec<u16> = line.encode_utf16().collect();
    wide.push(0);
    wide
}
