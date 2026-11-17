//! 托盘（M1-4 静态菜单 → M2-5 动态任务菜单）。
//!
//! 菜单结构：聚合摘要（禁用）/ 分隔线 / 打开面板 / 分隔线 / 每个任务一个子菜单
//! （状态 + 启动/停止/重启）/ 分隔线 / 退出。任务子菜单由当前快照生成，**不缓存**
//! 旧状态：每次 `snapshot:changed`（运行时事件、命令完成、配置重载）都会整体重建
//! 菜单与 tooltip。
//!
//! 图标：以 `app.default_window_icon()` 为底图，右下角叠加聚合角标（红 > 琥珀 >
//! 绿 > 灰），绘制逻辑在不依赖 Tauri 的 [`crate::tray_icon`] 里；底图缺失或尺寸异常
//! 时保留原图标，只记日志，绝不 panic。
//!
//! # 线程与锁
//!
//! - 菜单构建需要 GUI 线程亲和性，[`refresh`] 一律通过 `run_on_main_thread` 切回主线程；
//!   调用方（engine 的事件转发线程 / 命令工作线程）不会因此阻塞在 GUI 上。
//! - 构建菜单只读取传入的快照副本，不取 Engine 锁；菜单回调只把动作丢给
//!   `tauri::async_runtime`，事件回调立即返回。
//! - 只有“打开面板”与托盘图标左键点击会调用 [`crate::ui::spawn_show`]；任务动作
//!   绝不 show/focus 窗口，也不置顶。

use tauri::{
    AppHandle, Wry,
    image::Image,
    menu::{IsMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
    tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent},
};

use crate::engine_host;
use crate::snapshot::{Snapshot, TaskSnapshot};
// 聚合与文案是纯逻辑，放在不依赖 Tauri 的模块里以便 `cargo test` 覆盖。
use crate::tray_icon::draw_badge;
use crate::tray_summary::{
    AggregateLevel, aggregate, menu_summary_text, status_label, tooltip_text,
};

const TRAY_ID: &str = "main";

/// 创建托盘：初始菜单直接来自当前快照（不经过事件）。
pub fn create(app: &AppHandle) -> tauri::Result<()> {
    let snapshot = engine_host::current_snapshot(app);
    let menu = build_menu(app, &snapshot)?;

    let mut builder = TrayIconBuilder::with_id(TRAY_ID)
        .tooltip(tooltip_text(&snapshot))
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(handle_menu_event)
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                // 左键点击等价于“打开面板”；任务动作不走这里。
                crate::ui::spawn_show(tray.app_handle());
            }
        });

    // 优先用带角标的图标；底图不可用时回落到默认图标（再没有就交给 Tauri 的默认值）。
    builder = match badge_icon(app, aggregate(&snapshot).level()) {
        Some(image) => builder.icon(image),
        None => match app.default_window_icon() {
            Some(icon) => builder.icon(icon.clone()),
            None => builder,
        },
    };

    builder.build(app)?;
    Ok(())
}

/// 快照变化后重建菜单与 tooltip；可从任意线程调用。
pub fn refresh(app: &AppHandle, snapshot: &Snapshot) {
    let app = app.clone();
    let snapshot = snapshot.clone();
    // 主线程已被占用或应用正在退出时忽略：下一次快照变化会再试。
    if let Err(error) = app.clone().run_on_main_thread(move || {
        if let Err(error) = apply(&app, &snapshot) {
            tracing::warn!("重建托盘菜单失败：{error}");
        }
    }) {
        tracing::debug!(error = %error, "调度托盘菜单重建失败");
    }
}

fn apply(app: &AppHandle, snapshot: &Snapshot) -> tauri::Result<()> {
    let Some(tray) = app.tray_by_id(TRAY_ID) else {
        return Ok(());
    };
    let menu = build_menu(app, snapshot)?;
    tray.set_menu(Some(menu))?;
    tray.set_tooltip(Some(tooltip_text(snapshot)))?;
    // 图标失败不影响菜单/tooltip：保留上一帧图标，只记日志。
    match badge_icon(app, aggregate(snapshot).level()) {
        Some(image) => {
            if let Err(error) = tray.set_icon(Some(image)) {
                tracing::warn!("更新托盘图标失败：{error}");
            }
        }
        None => tracing::debug!("聚合角标不可用，保留托盘原图标"),
    }
    Ok(())
}

/// 以默认窗口图标为底图生成带聚合角标的图标；底图缺失或尺寸异常时返回 `None`。
///
/// 纯绘制逻辑在 [`draw_badge`]；这里只做 Tauri 侧的取图/封装，不 panic。
fn badge_icon(app: &AppHandle, level: AggregateLevel) -> Option<Image<'static>> {
    let Some(base) = app.default_window_icon() else {
        tracing::debug!("没有默认窗口图标，无法生成聚合角标");
        return None;
    };
    let (width, height) = (base.width(), base.height());
    match draw_badge(base.rgba(), width, height, level) {
        Some(pixels) => Some(Image::new_owned(pixels, width, height)),
        None => {
            tracing::warn!(width, height, "默认图标尺寸/像素异常，保留托盘原图标");
            None
        }
    }
}

fn build_menu(app: &AppHandle, snapshot: &Snapshot) -> tauri::Result<Menu<Wry>> {
    // 顶部摘要：禁用项，只读展示聚合计数，不参与动作分发。
    let summary = MenuItem::with_id(
        app,
        "summary",
        menu_summary_text(snapshot),
        false,
        None::<&str>,
    )?;
    let head = PredefinedMenuItem::separator(app)?;
    let open = MenuItem::with_id(app, "open", "打开面板", true, None::<&str>)?;
    let lead = PredefinedMenuItem::separator(app)?;
    let tail = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;

    if snapshot.tasks.is_empty() {
        let empty = MenuItem::with_id(app, "no-tasks", "（没有任务）", false, None::<&str>)?;
        return Menu::with_items(app, &[&summary, &head, &open, &lead, &empty, &tail, &quit]);
    }

    let mut submenus = Vec::with_capacity(snapshot.tasks.len());
    for task in &snapshot.tasks {
        submenus.push(task_submenu(app, task)?);
    }

    let mut items: Vec<&dyn IsMenuItem<Wry>> = vec![&summary, &head, &open, &lead];
    for submenu in &submenus {
        items.push(submenu);
    }
    items.push(&tail);
    items.push(&quit);
    Menu::with_items(app, &items)
}

/// 单个任务的子菜单：`名称 — 状态`，内含启动/停止/重启。
///
/// A3：状态文案与可用动作都来自快照（`displayStatus` / `allowedActions`），
/// 托盘不再自行推导规则。
fn task_submenu(app: &AppHandle, task: &TaskSnapshot) -> tauri::Result<Submenu<Wry>> {
    let label = format!("{} — {}", task.name, status_label(&task.display_status));

    let start = task_item(app, task, "start", "启动", task.allowed_actions.start)?;
    let stop = task_item(app, task, "stop", "停止", task.allowed_actions.stop)?;
    let restart = task_item(app, task, "restart", "重启", task.allowed_actions.restart)?;
    let children: Vec<&dyn IsMenuItem<Wry>> = vec![&start, &stop, &restart];

    Submenu::with_items(app, label, task.valid, &children)
}

fn task_item(
    app: &AppHandle,
    task: &TaskSnapshot,
    action: &str,
    text: &str,
    enabled: bool,
) -> tauri::Result<MenuItem<Wry>> {
    let id = format!("task:{}:{}", task.id, action);
    MenuItem::with_id(app, id, text, enabled, None::<&str>)
}

fn handle_menu_event(app: &AppHandle, event: MenuEvent) {
    match event.id.as_ref() {
        "open" => crate::ui::spawn_show(app),
        "quit" => crate::shutdown::request_quit(app, crate::shutdown::ExitReason::User, true),
        id => {
            if let Some((task_id, action)) = parse_task_id(id) {
                engine_host::spawn_task_action(app, task_id, action);
            }
        }
    }
}

/// 解析 `task:<id>:<action>`；id 只含小写字母/数字/`-`/`_`，用 `rsplit_once` 取动作。
fn parse_task_id(id: &str) -> Option<(&str, &str)> {
    let rest = id.strip_prefix("task:")?;
    let (task_id, action) = rest.rsplit_once(':')?;
    if task_id.is_empty() {
        return None;
    }
    Some((task_id, action))
}
