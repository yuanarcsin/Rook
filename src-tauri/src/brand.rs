//! 产品身份（单一来源）：显示名 + 磁盘名。
//!
//! 2026-11-15 定名 **Rook**；同日晚第三轮（ADR-037）把磁盘/安装身份也改成 Rook：
//! exe、identifier、配置文件名、数据目录、HKCU Run 值、插件清单名、核心日志名。
//!
//! 旧名 `CampusSentry` 只保留在三处兼容代码里：
//! - [`crate::migrate`] 启动时把 `%APPDATA%` / `%LOCALAPPDATA%` 下的旧目录改名；
//! - 插件清单同时接受旧文件名 `campussentry.plugin.toml`；
//! - 便携配置在 `<exe_dir>/Rook.toml` 缺失时回退读 `CampusSentry.toml`。
//!
//! **脚本环境变量前缀仍是 `CAMPUS_SENTRY_*`**：那是脚本契约的一部分
//! （见 docs/scenarios/script-contract.md），改名会破坏已有脚本，本轮不动。
//!
//! 改显示名时同步：
//! - 前端 `src/lib/brand.ts`
//! - `src/app.html` 与 `src-tauri/tauri.conf.json` 的窗口标题
//! - `config/template.rs` 的模板首行（raw string 无法引用本常量）
//! - 文档标题

/// 显示名（窗口标题、托盘、Toast、退出确认、诊断包）。
pub const DISPLAY_NAME: &str = "Rook";

/// `%APPDATA%` / `%LOCALAPPDATA%` 下的目录名。
pub const APP_DIR_NAME: &str = "Rook";

/// 便携版配置文件名（与 exe 同目录）。
pub const PORTABLE_CONFIG_FILE: &str = "Rook.toml";

/// 脚本插件清单文件名（放在脚本目录旁）。
pub const PLUGIN_MANIFEST: &str = "rook.plugin.toml";

/// 核心日志文件名（`<data>/core/` 下）。
pub const CORE_LOG_FILE: &str = "rook.log";

/// HKCU Run 值名。
pub const AUTOSTART_VALUE: &str = "Rook";

/// 脚本环境变量前缀：脚本契约的一部分，改名会破坏已有脚本。
pub const ENV_PREFIX: &str = "CAMPUS_SENTRY";

/// HTTP 健康探测的 User-Agent；保持 ASCII，避免影响服务端日志。
pub const USER_AGENT: &str = "Rook-health/0.1";

/// 旧磁盘名（仅用于一次性迁移与兼容读取）。
pub const LEGACY_NAME: &str = "CampusSentry";

/// 旧便携配置文件名。
pub const LEGACY_PORTABLE_CONFIG_FILE: &str = "CampusSentry.toml";

/// 旧插件清单文件名。
pub const LEGACY_PLUGIN_MANIFEST: &str = "campussentry.plugin.toml";

/// 旧核心日志文件名（迁移前留下的归档由用户自行清理）。
pub const LEGACY_CORE_LOG_FILE: &str = "campus-sentry.log";

/// 旧 HKCU Run 值名。
pub const LEGACY_AUTOSTART_VALUE: &str = "CampusSentry";

/// 旧 identifier 对应的 WebView2 数据目录名（`%LOCALAPPDATA%` 下）。
pub const LEGACY_WEBVIEW_DIR: &str = "com.campussentry.app";
