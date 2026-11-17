//! 启动早期的 provisional 数据目录（M1-5）。
//!
//! 优先级：`--data-dir` > `--profile dev`（`<cwd>/.dev-data`）> `%LOCALAPPDATA%/Rook`。
//! 这里**不读配置**（配置还没加载出来），所以 `[app].data_dir` 不参与，也就不会改变
//! core 日志路径；配置层的数据目录解析见 `config::paths::resolve_data_dir`。
//!
//! dev profile 下把 `.dev-data` 作为配置层的 `--data-dir` 覆盖项传下去，
//! 让任务日志、状态等也留在仓库内，不污染用户真实数据。

use std::path::{Path, PathBuf};

use crate::cli::{CliArgs, DEV_PROFILE};
use crate::config::{APP_DIR_NAME, PORTABLE_CONFIG_FILE};

/// dev profile 的数据目录名（相对启动时的工作目录）。
pub const DEV_DATA_DIR: &str = ".dev-data";

/// dev profile 的数据目录。
pub fn dev_data_dir(cwd: &Path) -> PathBuf {
    cwd.join(DEV_DATA_DIR)
}

/// 传给配置层的数据目录覆盖项。
///
/// `--data-dir` 最优先；dev profile 下固定为 `.dev-data`（`[app].data_dir` 不再生效）；
/// 其余情况返回 `None`，由配置层按 `[app].data_dir` > `%LOCALAPPDATA%` 决定。
pub fn data_dir_override(args: &CliArgs, cwd: &Path) -> Option<PathBuf> {
    match (&args.data_dir, args.profile.as_deref()) {
        (Some(dir), _) => Some(dir.clone()),
        (None, Some(DEV_PROFILE)) => Some(dev_data_dir(cwd)),
        _ => None,
    }
}

/// dev profile 的默认配置文件：`<数据目录>/Rook.toml`。
///
/// 只在没有显式 `--config` 时使用；避免开发实例读写生产配置。
pub fn dev_config_path(args: &CliArgs, cwd: &Path) -> Option<PathBuf> {
    if !args.is_dev() {
        return None;
    }
    data_dir_override(args, cwd).map(|dir| dir.join(PORTABLE_CONFIG_FILE))
}

/// provisional 数据目录：日志与早期状态使用。
///
/// 返回的路径不保证存在，调用方自行 `create_dir_all`；失败只影响日志，不阻止启动。
pub fn provisional(
    args: &CliArgs,
    cwd: &Path,
    local_appdata: Option<&Path>,
) -> Result<PathBuf, String> {
    if let Some(dir) = data_dir_override(args, cwd) {
        return Ok(dir);
    }
    match local_appdata {
        Some(base) => Ok(base.join(APP_DIR_NAME)),
        None => Err("无法确定数据目录：LOCALAPPDATA 未设置，请用 --data-dir 指定".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    fn args(data_dir: Option<&str>, profile: Option<&str>) -> CliArgs {
        CliArgs {
            config: None,
            data_dir: data_dir.map(PathBuf::from),
            profile: profile.map(str::to_owned),
            autostart: false,
            import_plugins: Vec::new(),
            simulate_session_end: false,
            test_toast: false,
        }
    }

    fn cwd() -> PathBuf {
        PathBuf::from("D:/work/Rook")
    }

    fn local() -> PathBuf {
        PathBuf::from("C:/Users/tester/AppData/Local")
    }

    #[test]
    fn explicit_data_dir_wins_even_in_dev() {
        let parsed = args(Some("D:/tmp/explicit"), Some("dev"));
        assert_eq!(
            provisional(&parsed, &cwd(), Some(&local())),
            Ok(PathBuf::from("D:/tmp/explicit"))
        );
        assert_eq!(
            data_dir_override(&parsed, &cwd()),
            Some(PathBuf::from("D:/tmp/explicit"))
        );
    }

    #[test]
    fn dev_profile_uses_cwd_dev_data() {
        let parsed = args(None, Some("dev"));
        assert_eq!(
            provisional(&parsed, &cwd(), Some(&local())),
            Ok(PathBuf::from("D:/work/Rook/.dev-data"))
        );
        assert_eq!(
            data_dir_override(&parsed, &cwd()),
            Some(PathBuf::from("D:/work/Rook/.dev-data"))
        );
    }

    #[test]
    fn dev_profile_beats_local_appdata_and_configured_dir() {
        // 配置层的 [app].data_dir 不参与 provisional 解析：dev 下固定 .dev-data。
        let parsed = args(None, Some("dev"));
        assert_ne!(
            provisional(&parsed, &cwd(), Some(&local())),
            Ok(local().join(APP_DIR_NAME))
        );
    }

    #[test]
    fn default_falls_back_to_local_appdata() {
        let parsed = args(None, None);
        assert_eq!(
            provisional(&parsed, &cwd(), Some(&local())),
            Ok(local().join(APP_DIR_NAME))
        );
        assert_eq!(data_dir_override(&parsed, &cwd()), None);
    }

    #[test]
    fn dev_config_lives_in_dev_data_dir() {
        let parsed = args(None, Some("dev"));
        assert_eq!(
            dev_config_path(&parsed, &cwd()),
            Some(PathBuf::from("D:/work/Rook/.dev-data/Rook.toml"))
        );
    }

    #[test]
    fn dev_config_respects_explicit_data_dir() {
        let parsed = args(Some("D:/tmp/explicit"), Some("dev"));
        assert_eq!(
            dev_config_path(&parsed, &cwd()),
            Some(PathBuf::from("D:/tmp/explicit/Rook.toml"))
        );
    }

    #[test]
    fn non_dev_has_no_dev_config() {
        let parsed = args(None, None);
        assert_eq!(dev_config_path(&parsed, &cwd()), None);
    }

    #[test]
    fn missing_local_appdata_is_an_error() {
        let parsed = args(None, None);
        let error = provisional(&parsed, &cwd(), None).expect_err("应提示缺少 LOCALAPPDATA");
        assert!(error.contains("--data-dir"), "{error}");
    }

    #[test]
    fn unknown_profile_falls_back_to_local_appdata() {
        let parsed = args(None, Some("staging"));
        assert_eq!(
            provisional(&parsed, &cwd(), Some(&local())),
            Ok(local().join(APP_DIR_NAME))
        );
    }
}
