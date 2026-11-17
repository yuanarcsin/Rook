//! 配置文件与数据目录的路径解析。
//!
//! 配置：`--config` > `<exe_dir>/Rook.toml` > `%APPDATA%/Rook/config.toml`（缺则创建）。
//! 数据：`--data-dir` > `[app].data_dir` > `%LOCALAPPDATA%/Rook`。
//!
//! 兼容：便携目录里只有旧的 `CampusSentry.toml` 时仍读它（不复制、不改名，
//! 避免只读安装目录写失败）；`%APPDATA%` 的旧目录由 `crate::migrate_identity` 改名。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use super::error::LoadError;
use super::template::write_default_config;

/// 便携版配置文件名。
pub const PORTABLE_CONFIG_FILE: &str = crate::brand::PORTABLE_CONFIG_FILE;
/// 安装版配置文件名。
pub const INSTALLED_CONFIG_FILE: &str = "config.toml";
/// `%APPDATA%` / `%LOCALAPPDATA%` 下的子目录名。
pub const APP_DIR_NAME: &str = crate::brand::APP_DIR_NAME;

/// 解析配置文件路径；返回的路径保证存在（`--config` 除外，交给读取时报错）。
pub fn resolve_config_path(
    explicit: Option<&Path>,
    exe_dir: &Path,
    appdata: Option<&Path>,
) -> Result<PathBuf, LoadError> {
    if let Some(path) = explicit {
        return Ok(path.to_path_buf());
    }

    let portable = exe_dir.join(PORTABLE_CONFIG_FILE);
    if portable.is_file() {
        return Ok(portable);
    }
    // 兼容旧便携版：只有 CampusSentry.toml 时照旧读它。
    let legacy_portable = exe_dir.join(crate::brand::LEGACY_PORTABLE_CONFIG_FILE);
    if legacy_portable.is_file() {
        return Ok(legacy_portable);
    }

    let Some(appdata) = appdata else {
        return Err(LoadError::io(
            "%APPDATA%",
            io::Error::new(
                io::ErrorKind::NotFound,
                "无法确定配置目录：APPDATA 未设置，请用 --config 指定配置文件",
            ),
        ));
    };

    let path = appdata.join(APP_DIR_NAME).join(INSTALLED_CONFIG_FILE);
    if !path.is_file() {
        // 首次运行：写默认模板；write_default_config 不会覆盖已有文件。
        write_default_config(&path)?;
    }
    Ok(path)
}

/// 解析数据目录并确保其存在。
pub fn resolve_data_dir(
    explicit: Option<&Path>,
    configured: Option<&Path>,
    local_appdata: Option<&Path>,
) -> Result<PathBuf, LoadError> {
    let dir = if let Some(path) = explicit {
        path.to_path_buf()
    } else if let Some(path) = configured {
        path.to_path_buf()
    } else if let Some(base) = local_appdata {
        base.join(APP_DIR_NAME)
    } else {
        return Err(LoadError::io(
            "%LOCALAPPDATA%",
            io::Error::new(
                io::ErrorKind::NotFound,
                "无法确定数据目录：LOCALAPPDATA 未设置，请用 --data-dir 指定",
            ),
        ));
    };

    fs::create_dir_all(&dir).map_err(|error| LoadError::io(&dir, error))?;
    Ok(dir)
}
