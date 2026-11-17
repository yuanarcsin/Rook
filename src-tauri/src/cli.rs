//! 命令行参数（M1-5）：`--config` / `--data-dir` / `--profile`；M5-1 增加 `--autostart`。
//! 2026-11-04 增加 `--import-plugin`（无头注册插件清单）与手动启动默认开窗语义。
//!
//! 刻意不依赖 Tauri：`engine` 在启动时用它，测试也直接验证解析结果。
//! 窗口开关由 [`wants_window`] 判断（`lib.rs` 调用）。

use std::path::PathBuf;

/// 开发 profile 名；只有它会把数据目录切到 `.dev-data`。
pub const DEV_PROFILE: &str = "dev";

/// 自启动标记：不打开窗口、不弹通知。
pub const AUTOSTART_ARG: &str = "--autostart";

/// 无头导入插件清单：导入后直接退出，不创建托盘/窗口。
pub const IMPORT_PLUGIN_ARG: &str = "--import-plugin";

/// 隐藏测试入口：向本进程的会话消息窗口投递 WM_QUERYENDSESSION/WM_ENDSESSION。
///
/// 正常启动不使用；仅用于自动化/手工验证注销/关机路径。
pub const SIMULATE_SESSION_END_ARG: &str = "--simulate-session-end";

/// 隐藏测试入口：初始化系统通知身份后发一条测试 Toast，打印结果并退出。
///
/// 不创建 Tauri/托盘/窗口；用于 QA 真机验收（配合开始菜单快捷方式）。
pub const TEST_TOAST_ARG: &str = "--test-toast";

/// 与配置/数据目录相关的命令行参数。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct CliArgs {
    /// `--config <path>`：配置文件路径。
    pub config: Option<PathBuf>,
    /// `--data-dir <path>`：数据目录，优先级最高。
    pub data_dir: Option<PathBuf>,
    /// `--profile <name>`：运行 profile，目前只认 [`DEV_PROFILE`]。
    pub profile: Option<String>,
    /// `--autostart`：由 HKCU Run 开机拉起；不创建窗口、不弹通知。
    pub autostart: bool,
    /// `--import-plugin <manifest>`：无头导入清单；可重复。
    pub import_plugins: Vec<PathBuf>,
    /// 隐藏测试入口：模拟会话结束（投递 WM_QUERYENDSESSION/WM_ENDSESSION）。
    pub simulate_session_end: bool,
    /// 隐藏测试入口：发送一条测试系统通知后退出（不创建 Tauri/托盘）。
    pub test_toast: bool,
}

impl CliArgs {
    /// 是否运行在 dev profile 下。
    pub fn is_dev(&self) -> bool {
        self.profile.as_deref() == Some(DEV_PROFILE)
    }
}

/// 是否请求显示窗口。
///
/// 语义（2026-11-04 用户要求）：
/// - 手动双击/无参数启动：显示主窗口；
/// - `--autostart`：只驻留托盘，不显示窗口、不弹通知；
/// - `--import-plugin`：无头导入后退出，不显示窗口；
/// - 显式 `--show`：即使与 `--autostart` 同时出现也显示（单实例唤醒等场景）。
pub fn wants_window(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == "--show") {
        return true;
    }
    if args.iter().any(|arg| arg == AUTOSTART_ARG) {
        return false;
    }
    if args
        .iter()
        .any(|arg| arg == IMPORT_PLUGIN_ARG || arg.starts_with("--import-plugin="))
    {
        return false;
    }
    if args.iter().any(|arg| arg == TEST_TOAST_ARG) {
        return false;
    }
    true
}

/// 解析 `--config` / `--data-dir` / `--profile` / `--autostart` / `--import-plugin`，
/// 空格与 `=` 两种写法都接受。
///
/// 未知参数忽略；缺少取值的开关保持 `None`。`--import-plugin` 可重复。
#[allow(clippy::while_let_on_iterator)] // 需要同时消费下一个参数，for 循环不适用
pub fn parse_args<I>(args: I) -> CliArgs
where
    I: IntoIterator<Item = String>,
{
    let mut parsed = CliArgs::default();
    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => parsed.config = iter.next().map(PathBuf::from),
            "--data-dir" => parsed.data_dir = iter.next().map(PathBuf::from),
            "--profile" => parsed.profile = iter.next(),
            AUTOSTART_ARG => parsed.autostart = true,
            SIMULATE_SESSION_END_ARG => parsed.simulate_session_end = true,
            TEST_TOAST_ARG => parsed.test_toast = true,
            IMPORT_PLUGIN_ARG => {
                if let Some(path) = iter.next() {
                    parsed.import_plugins.push(PathBuf::from(path));
                }
            }
            _ => {
                if let Some(value) = arg.strip_prefix("--config=") {
                    parsed.config = Some(PathBuf::from(value));
                } else if let Some(value) = arg.strip_prefix("--data-dir=") {
                    parsed.data_dir = Some(PathBuf::from(value));
                } else if let Some(value) = arg.strip_prefix("--profile=") {
                    parsed.profile = Some(value.to_owned());
                } else if let Some(value) = arg.strip_prefix("--import-plugin=")
                    && !value.is_empty()
                {
                    parsed.import_plugins.push(PathBuf::from(value));
                }
            }
        }
    }
    parsed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> CliArgs {
        parse_args(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn profile_space_form_is_dev() {
        let args = parse(&["--show", "--profile", "dev"]);
        assert_eq!(args.profile.as_deref(), Some(DEV_PROFILE));
        assert!(args.is_dev());
    }

    #[test]
    fn profile_equals_form_is_dev() {
        let args = parse(&["--profile=dev"]);
        assert_eq!(args.profile.as_deref(), Some(DEV_PROFILE));
        assert!(args.is_dev());
    }

    #[test]
    fn no_profile_is_not_dev() {
        let args = parse(&["--show"]);
        assert_eq!(args.profile, None);
        assert!(!args.is_dev());
    }

    #[test]
    fn unknown_profile_is_kept_but_not_dev() {
        let args = parse(&["--profile", "staging"]);
        assert_eq!(args.profile.as_deref(), Some("staging"));
        assert!(!args.is_dev());
    }

    #[test]
    fn config_and_data_dir_support_both_forms() {
        let args = parse(&[
            "--config",
            "a.toml",
            "--data-dir=D:/tmp/data",
            "--profile",
            "dev",
        ]);
        assert_eq!(args.config, Some(PathBuf::from("a.toml")));
        assert_eq!(args.data_dir, Some(PathBuf::from("D:/tmp/data")));
        assert!(args.is_dev());

        let args = parse(&["--config=b.toml", "--data-dir", "D:/tmp/other"]);
        assert_eq!(args.config, Some(PathBuf::from("b.toml")));
        assert_eq!(args.data_dir, Some(PathBuf::from("D:/tmp/other")));
    }

    #[test]
    fn switch_without_value_keeps_none() {
        let args = parse(&["--profile"]);
        assert_eq!(args.profile, None);
    }

    #[test]
    fn autostart_flag_is_parsed() {
        let args = parse(&[AUTOSTART_ARG]);
        assert!(args.autostart);
        assert!(!args.is_dev());

        assert!(!parse(&[]).autostart);
    }

    #[test]
    fn autostart_does_not_request_window() {
        // 开机自启只驻留托盘：不创建窗口、不弹通知。
        let raw = vec![AUTOSTART_ARG.to_owned()];
        assert!(!wants_window(&raw));

        let raw = vec![
            AUTOSTART_ARG.to_owned(),
            "--profile".to_owned(),
            "dev".to_owned(),
        ];
        assert!(!wants_window(&raw));
    }

    #[test]
    fn show_requests_window() {
        assert!(wants_window(&["--show".to_owned()]));
        assert!(wants_window(&[
            AUTOSTART_ARG.to_owned(),
            "--show".to_owned()
        ]));
    }

    #[test]
    fn manual_launch_requests_window() {
        // 双击 exe（无参数）必须弹窗；--autostart 才静默。
        assert!(wants_window(&[]));
        assert!(wants_window(&["--profile".to_owned(), "dev".to_owned()]));
    }

    #[test]
    fn simulate_session_end_flag_is_parsed() {
        assert!(parse(&[SIMULATE_SESSION_END_ARG]).simulate_session_end);
        assert!(!parse(&[]).simulate_session_end);
        // 隐藏入口不影响窗口语义（仍按普通启动处理）。
        assert!(wants_window(&[SIMULATE_SESSION_END_ARG.to_owned()]));
    }

    #[test]
    fn test_toast_flag_is_parsed_and_headless() {
        assert!(parse(&[TEST_TOAST_ARG]).test_toast);
        assert!(!parse(&[]).test_toast);
        assert!(!wants_window(&[TEST_TOAST_ARG.to_owned()]));
    }

    #[test]
    fn import_plugin_does_not_request_window() {
        assert!(!wants_window(&[
            IMPORT_PLUGIN_ARG.to_owned(),
            "a.toml".to_owned()
        ]));
        assert!(!wants_window(&["--import-plugin=a.toml".to_owned()]));
    }

    #[test]
    fn import_plugin_is_repeatable_and_supports_equals() {
        let args = parse(&[
            "--import-plugin",
            "a.toml",
            "--import-plugin=b.toml",
            "--import-plugin",
            "c.toml",
        ]);
        assert_eq!(
            args.import_plugins,
            vec![
                PathBuf::from("a.toml"),
                PathBuf::from("b.toml"),
                PathBuf::from("c.toml")
            ]
        );
    }
}
