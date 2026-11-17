//! 可执行文件解析：绝对/相对路径 + PATH/PATHEXT 查找。
//!
//! 与 `CreateProcessW` 不同，这里**显式**完成查找并把绝对路径交给 `lpApplicationName`：
//! - 结果可预测、可测试，不受父进程当前目录/`App Paths` 注册表影响；
//! - PATH 中的空项（Windows 解释为当前目录）被跳过，避免误执行当前目录里的同名程序；
//! - 带路径分隔符或盘符的 program 先按原样查，再按 PATHEXT 补扩展名。
//!
//! `.bat`/`.cmd` 不是可执行映像，`CreateProcessW` 会以 ERROR_BAD_EXE_FORMAT 失败；
//! 需要 cmd 解释器的场景由调用方显式启动 `cmd.exe /C ...`（M2-1 不隐式改写命令行）。

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_INVALID_PARAMETER};

use super::error::{SpawnError, Stage};

/// 默认 PATHEXT（环境里没有时）。
const DEFAULT_PATHEXT: &str = ".COM;.EXE;.BAT;.CMD";

/// 解析 program 为存在的文件路径。
///
/// - `base_dir`：相对路径（含分隔符）的基准目录；`None` 表示用当前目录。
/// - `path_var` / `pathext`：显式传入便于测试；`None` 分别表示空 PATH 与默认 PATHEXT。
pub fn resolve_program(
    program: &OsStr,
    base_dir: Option<&Path>,
    path_var: Option<&OsStr>,
    pathext: Option<&OsStr>,
) -> Result<PathBuf, SpawnError> {
    let raw = program.to_string_lossy();
    let name = raw.trim();
    if name.is_empty() {
        return Err(invalid("command 为空"));
    }
    if name.contains('"') {
        return Err(invalid("command 不能包含引号；请把参数放进 args"));
    }
    if name.contains('\0') {
        return Err(invalid("command 不能包含 NUL"));
    }

    let extensions = executable_extensions(pathext);

    if looks_like_path(name) {
        let path = Path::new(name);
        let candidate = if path.is_absolute() {
            path.to_path_buf()
        } else {
            base_dir.unwrap_or_else(|| Path::new(".")).join(path)
        };
        if let Some(found) = find_with_extensions(&candidate, &extensions) {
            return Ok(found);
        }
        return Err(SpawnError::with_code(
            Stage::ResolveProgram,
            ERROR_FILE_NOT_FOUND.0,
            format!("找不到可执行文件：{}", candidate.display()),
        ));
    }

    if let Some(path_var) = path_var {
        for dir in path_var.to_string_lossy().split(';') {
            // 引号包裹的 PATH 项（安装器常见）与首尾空白都去掉；空项跳过。
            let dir = dir.trim().trim_matches('"');
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join(name);
            if let Some(found) = find_with_extensions(&candidate, &extensions) {
                return Ok(found);
            }
        }
    }

    Err(SpawnError::with_code(
        Stage::ResolveProgram,
        ERROR_FILE_NOT_FOUND.0,
        format!("在 PATH 中找不到命令 \"{name}\""),
    ))
}

fn invalid(message: &str) -> SpawnError {
    SpawnError::with_code(Stage::ResolveProgram, ERROR_INVALID_PARAMETER.0, message)
}

fn find_with_extensions(path: &Path, extensions: &[String]) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    for extension in extensions {
        let mut os = path.as_os_str().to_os_string();
        os.push(extension);
        let candidate = PathBuf::from(os);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn executable_extensions(pathext: Option<&OsStr>) -> Vec<String> {
    let raw = pathext
        .map(|value| value.to_string_lossy().into_owned())
        .unwrap_or_else(|| DEFAULT_PATHEXT.to_owned());
    raw.split(';')
        .map(str::trim)
        .filter(|extension| !extension.is_empty())
        .map(str::to_owned)
        .collect()
}

fn looks_like_path(value: &str) -> bool {
    value.contains('/') || value.contains('\\') || has_drive_prefix(value)
}

fn has_drive_prefix(value: &str) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}
