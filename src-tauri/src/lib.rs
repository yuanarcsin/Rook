pub mod brand;
pub mod cli;
// M5-1：开机自启（HKCU Run）。纯逻辑可测；注册表实现仅 Windows，
// 启动时由 Engine 按配置 reconcile（dev profile 跳过）。
pub mod autostart;
pub mod config;
pub mod data_dir;
// 2026-11-04：诊断包导出（配置/状态/日志/心跳复制到 diagnostics/）。
pub mod diagnostics;
// M3-1：三档健康探测与评估核心（process/heartbeat/http），
// 不依赖 Tauri；Engine 接线在后续里程碑完成。
pub mod health;
// A4：命令失败统一返回 `IpcError`（stage/message/code/taskId/currentRevision/line/col）。
// 放在 `cfg(not(test))` 之外，单测才能直接覆盖序列化形状。
pub mod ipc_error;
pub mod logging;
// 2026-11-15：一次性身份迁移（CampusSentry → Rook）。必须在日志/配置之前跑。
pub mod migrate_identity;
// 2026-11-04：无头插件注册（`--import-plugin`），不依赖 Tauri；
// 用于首次部署/真实测试前把脚本清单注册进实际配置。
pub mod plugin_cli;
// M3-5b：通知降级与节流（纯逻辑，不依赖 Tauri）；Engine 负责投递事件/日志。
pub mod notifications;
// 系统 Toast（Windows）：AUMID = 当前 exe 路径；只读取用户提供的开始菜单快捷方式，
// 不创建/修改快捷方式、不写 HKCU AppUserModelId。WinRT 只在专用线程内使用。
pub mod toast;
// M2-3：任务日志采集（管道原始字节 → current.log、轮转/保留/降级），
// 不依赖 Tauri；读线程基于 Windows 匿名管道。
#[cfg(windows)]
pub mod logs;
// M2-1：纯 Windows 进程启动器（CreateProcessW + Job Object + 管道），
// 不依赖 Tauri，可在 cargo test 中直接跑真实进程。
#[cfg(windows)]
pub mod process;
// M3-2：重启/退避/熔断决策状态机（纯逻辑，不依赖 Tauri）；
// Engine 在后续里程碑接线。
pub mod restart;
// M2-4a：单任务运行时（端口预检 → 启动 → 日志 → 退出监控 → 状态收敛），
// 组合 process/logs，不依赖 Tauri；Engine 接线在后续里程碑完成。
#[cfg(windows)]
pub mod runtime;
pub mod schedule;
// B1/B3：应用退出协调（预算/幂等/干净退出标记）。纯逻辑可在测试中编译。
pub mod shutdown;
// B3：Windows 会话结束（注销/关机）消息专用窗口；仅非测试构建。
#[cfg(all(windows, not(test)))]
pub mod session;
pub mod snapshot;
// M3-3：state.json 持久化（模型 + 原子读写，不依赖 Tauri）；
// Engine 在 M3 接线时负责恢复语义与写盘时机。
pub mod state_store;
// 托盘聚合与文案（纯逻辑，不依赖 Tauri）：`tray` 是 cfg(not(test))，
// 这些逻辑放这里才能被 `cargo test` 编译与覆盖。
pub mod tray_summary;
// 托盘图标聚合角标绘制（纯逻辑，不依赖 Tauri）：给定 RGBA 像素与聚合级别，
// 在右下角叠加实心圆点，返回新像素。
pub mod tray_icon;

// Tauri 外壳不参与单元测试构建：
// 1) 配置/快照模块的测试不依赖 Tauri/GUI；
// 2) Windows 的 GNU 测试二进制没有 comctl32 v6 清单，一旦链接 WebView2/托盘代码，
//    进程会以 STATUS_ENTRYPOINT_NOT_FOUND 直接退出（TaskDialogIndirect 找不到）。
// A1：`engine` 是 Tauri-free 的编排核心，测试构建也要编译（core 单测）。
pub mod engine;
// A1：Tauri 宿主层（命令/Scheduler/事件广播）只在非测试构建里存在。
#[cfg(not(test))]
pub(crate) mod commands;
#[cfg(not(test))]
pub(crate) mod engine_host;
#[cfg(not(test))]
mod tray;
#[cfg(not(test))]
mod ui;

#[cfg(not(test))]
use tauri::RunEvent;

#[cfg(not(test))]
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    // 日志必须先于一切初始化：配置错误、托盘/窗口失败都要落到核心日志里。
    // `logging::init` 只降级不失败，磁盘不可写也不会阻止启动。
    let raw_args: Vec<String> = std::env::args().skip(1).collect();
    let args = cli::parse_args(raw_args.iter().cloned());
    let simulate_session_end = args.simulate_session_end;
    // 身份迁移必须先于日志/配置：两者都按新目录解析，先创建空目录会让旧数据无法整体改名。
    let identity_migration = migrate_identity::run(args.is_dev());
    let logging = logging::init(&args);
    tracing::info!(
        event = "app.start",
        version = env!("CARGO_PKG_VERSION"),
        profile = args.profile.as_deref().unwrap_or("default"),
        "{} 启动",
        brand::DISPLAY_NAME
    );
    for line in &identity_migration.moved {
        tracing::info!(event = "identity.migrated", "{line}");
    }
    for line in &identity_migration.warnings {
        tracing::warn!(event = "identity.migrate_skipped", "{line}");
    }

    // 隐藏 QA 入口：只初始化通知身份并发一条测试 Toast，不创建 Tauri/托盘/窗口。
    if args.test_toast {
        let code = test_toast_headless();
        drop(logging);
        std::process::exit(code);
    }

    // 无头插件注册：在创建 Tauri/托盘之前完成，成功后直接退出。
    // 这样首次部署可以先注册三份清单，再双击 exe 看到完整主窗口。
    if !args.import_plugins.is_empty() {
        let code = import_plugins_headless(&args);
        // 显式 drop 日志 guard，确保退出前 flush；`process::exit` 不会跑析构。
        drop(logging);
        std::process::exit(code);
    }

    if args.autostart {
        // M5-1：开机自启只初始化托盘与核心；手动双击启动则显示主窗口。
        tracing::info!("以 --autostart 启动：不创建窗口、不弹通知");
    } else {
        tracing::info!("手动启动：显示主窗口（--autostart 才静默驻留托盘）");
    }

    // 系统通知身份必须在创建 Tauri Builder/窗口之前设置；快捷方式缺失只降级。
    let toast_status = toast::init();
    if toast_status.available {
        tracing::info!(
            event = "toast",
            aumid = toast_status.aumid.as_deref().unwrap_or("-"),
            shortcut = toast_status.shortcut.as_deref().unwrap_or("-"),
            "系统通知已启用（使用开始菜单快捷方式）"
        );
    } else {
        tracing::warn!(
            event = "toast_unavailable",
            reason = toast_status.reason.as_deref().unwrap_or("-"),
            "系统通知不可用，继续使用应用内 banner + 托盘摘要"
        );
    }

    let mut builder = tauri::Builder::default();

    // 单实例插件必须最先注册：第二实例在此直接退出，并把 argv 交给第一实例。
    #[cfg(desktop)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            ui::spawn_show(app);
        }));
    }

    builder
        .invoke_handler(tauri::generate_handler![
            commands::system::get_snapshot,
            commands::system::reload_config,
            commands::system::get_autostart_status,
            commands::system::get_toast_status,
            commands::system::test_toast,
            commands::task::start_task,
            commands::task::stop_task,
            commands::task::restart_task,
            commands::log::read_task_log,
            commands::log::list_task_logs,
            commands::config::read_config_text,
            commands::config::save_config_text,
            commands::config::restore_config_backup,
            commands::task::get_task_draft,
            commands::task::get_task_defaults,
            commands::task::save_task_draft,
            commands::task::delete_task,
            commands::task::reorder_tasks,
            commands::schedule::get_schedule,
            commands::schedule::save_schedule,
            commands::plugin::inspect_plugin,
            commands::plugin::import_plugin,
            commands::plugin::sync_plugin,
            commands::events::read_event_log,
            commands::events::export_diagnostics,
            commands::task::open_task_panel,
            commands::system::open_app_path,
            commands::system::quit_app,
            commands::system::acknowledge_tray_hint
        ])
        .setup(move |app| {
            // 配置错误只进入错误态，不影响托盘/窗口。
            engine_host::initialize(app.handle(), args, logging);

            tray::create(app.handle())?;

            // B3：安装会话结束消息窗口（注销/关机）。失败只降级为 Job 兜底。
            #[cfg(windows)]
            {
                if let Err(error) = session::install(app.handle()) {
                    tracing::warn!(
                        error = %error,
                        "安装会话结束消息窗口失败，注销/关机仅由 Job 兜底"
                    );
                }
                if simulate_session_end {
                    session::spawn_simulation(app.handle());
                }
            }

            // 手动双击/无参数启动显示窗口；`--autostart` 仅初始化托盘与核心；
            // 显式 `--show` 也显示（第二实例唤醒等）。
            if cli::wants_window(&raw_args) {
                ui::spawn_show(app.handle());
            }

            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // 用户关闭最后一个窗口时不退出，应用继续驻留托盘；程序化 exit 放行。
            if let RunEvent::ExitRequested { code, api, .. } = event
                && code.is_none()
            {
                api.prevent_exit();
            }
        });
}

/// `--import-plugin` 的无头实现：加载实际配置路径后逐个导入/同步，返回进程退出码。
#[cfg(not(test))]
fn import_plugins_headless(args: &cli::CliArgs) -> i32 {
    let overrides = config::Overrides {
        config: args.config.as_deref(),
        data_dir: args.data_dir.as_deref(),
    };
    let loaded = match config::load(&overrides) {
        Ok(loaded) => loaded,
        Err(error) => {
            tracing::error!(error = %error, "插件注册：加载配置失败");
            eprintln!("插件注册失败：{error}");
            return 1;
        }
    };

    match plugin_cli::import_plugins(
        &loaded.config_path,
        &args.import_plugins,
        Some(loaded.data_dir.as_path()),
    ) {
        Ok(reports) => {
            tracing::info!(
                config = %loaded.config_path.display(),
                count = reports.len(),
                "插件清单注册完成"
            );
            for report in reports {
                tracing::info!(
                    manifest = %report.manifest.display(),
                    task = %report.task_id,
                    changed = report.changed,
                    synced = report.synced,
                    "插件清单已注册"
                );
                println!(
                    "{}: {} ({})",
                    if report.synced { "同步" } else { "导入" },
                    report.task_id,
                    report.manifest.display()
                );
            }
            0
        }
        Err(error) => {
            tracing::error!(error = %error, "插件清单注册失败");
            eprintln!("插件注册失败：{error}");
            1
        }
    }
}

/// `--test-toast` 的无头实现：初始化身份 → 发一条测试 Toast → 打印状态并返回退出码。
///
/// 不创建 Tauri/托盘/窗口；快捷方式缺失或被系统禁用时如实打印原因，不伪造成功。
#[cfg(not(test))]
fn test_toast_headless() -> i32 {
    use std::io::Write;

    // 发布产物是 windows_subsystem="windows"：从没有控制台的终端直接启动时，
    // stdout 可能是一个已关闭的管道。println! 在写失败时会 panic；这里用
    // writeln! 并忽略错误，测试结果以退出码和核心日志为准。
    let mut out = std::io::stdout();
    let status = toast::init();
    if !status.available {
        let reason = status.reason.as_deref().unwrap_or("原因未知");
        let _ = writeln!(out, "系统通知不可用：{reason}");
        tracing::warn!(event = "toast_unavailable", reason, "测试系统通知：不可用");
        return 1;
    }

    let status = toast::test_toast(std::time::Duration::from_secs(5));
    match status.last_error.as_deref() {
        None if status.available => {
            let _ = writeln!(
                out,
                "系统通知已发送：aumid={} shortcut={}",
                status.aumid.as_deref().unwrap_or("-"),
                status.shortcut.as_deref().unwrap_or("-")
            );
            tracing::info!(
                event = "toast",
                aumid = status.aumid.as_deref().unwrap_or("-"),
                shortcut = status.shortcut.as_deref().unwrap_or("-"),
                "测试系统通知已发送"
            );
            0
        }
        Some(error) => {
            let _ = writeln!(out, "系统通知发送失败：{error}");
            tracing::warn!(event = "toast_error", error, "测试系统通知发送失败");
            1
        }
        None => {
            let _ = writeln!(
                out,
                "系统通知状态异常：available={} reason={}",
                status.available,
                status.reason.as_deref().unwrap_or("-")
            );
            tracing::warn!(
                event = "toast_error",
                available = status.available,
                reason = status.reason.as_deref().unwrap_or("-"),
                "测试系统通知：状态异常"
            );
            1
        }
    }
}
