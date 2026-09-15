//! 系统托盘：单色图标 + 左键切换面板 + 右键菜单（显示 / 预设 / 退出）。
//!
//! PRD P0-1：程序常驻托盘，关闭面板不退出进程。托盘图标用单色扁平太阳 PNG
//! （`icons/32x32.png`，与 UI 图标同一视觉语言）。
//!
//! v0.2.0：胶囊 UI 精简为纯玻璃 + Fill（无图标 / 无文字），三档亮度预设
//! 的入口迁移到托盘菜单，避免破坏胶囊的克制观感。

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::{AppHandle, Manager};

/// 创建托盘图标并挂接事件。
///
/// 左键单击：切换悬浮物显示 / 隐藏；
/// 右键菜单：「显示/隐藏滑条」「预设亮度 ▸ 白天/观影/夜间」「退出」。
pub fn build_tray(app: &AppHandle) -> Result<(), String> {
    // 菜单项：id 供 on_menu_event 匹配。
    let show_item = MenuItem::with_id(app, "show", "显示/隐藏滑条", true, None::<&str>)
        .map_err(|e| format!("创建菜单失败：{e}"))?;
    let preset_day = MenuItem::with_id(app, "preset:day", "白天", true, None::<&str>)
        .map_err(|e| format!("创建菜单失败：{e}"))?;
    let preset_movie = MenuItem::with_id(app, "preset:movie", "观影", true, None::<&str>)
        .map_err(|e| format!("创建菜单失败：{e}"))?;
    let preset_night = MenuItem::with_id(app, "preset:night", "夜间", true, None::<&str>)
        .map_err(|e| format!("创建菜单失败：{e}"))?;
    let preset_menu = Submenu::with_items(app, "预设亮度", true, &[&preset_day, &preset_movie, &preset_night])
        .map_err(|e| format!("创建子菜单失败：{e}"))?;
    let quit_item = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)
        .map_err(|e| format!("创建菜单失败：{e}"))?;
    // v0.3：诊断面板入口。玻璃不亮时用户第一件事就是把它打开看统计。
    let diag_item = MenuItem::with_id(app, "diag", "诊断面板…", true, None::<&str>)
        .map_err(|e| format!("创建菜单失败：{e}"))?;
    let sep = PredefinedMenuItem::separator(app).map_err(|e| format!("创建分隔线失败：{e}"))?;
    let menu = Menu::with_items(app, &[&show_item, &preset_menu, &sep, &diag_item, &quit_item])
        .map_err(|e| format!("组装菜单失败：{e}"))?;

    // 单色扁平太阳图标：与 bundle.icon 同一份 PNG，缩到 16px 仍可辨认。
    // `include_image!` 基于 `$CARGO_MANIFEST_DIR`（= src-tauri）解析路径。
    let icon = tauri::include_image!("icons/32x32.png");

    TrayIconBuilder::with_id("main-tray")
        .icon(icon)
        .tooltip("HDR SDR 内容亮度")
        .menu(&menu)
        // 左键不弹菜单，交给 Click 事件做切换（右键仍弹菜单）。
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "show" => {
                let state = app.state::<crate::AppState>();
                let _ = crate::window::toggle(app, &state);
            }
            "preset:day" => {
                let app=app.clone();std::thread::spawn(move || {let _ = crate::commands::apply_preset("day".into(), app.state::<crate::AppState>());});
            }
            "preset:movie" => {
                let app=app.clone();std::thread::spawn(move || {let _ = crate::commands::apply_preset("movie".into(), app.state::<crate::AppState>());});
            }
            "preset:night" => {
                let app=app.clone();std::thread::spawn(move || {let _ = crate::commands::apply_preset("night".into(), app.state::<crate::AppState>());});
            }
            "diag" => {
                let _ = crate::commands::open_diag_window(app.clone());
            }
            "quit" => {
                crate::native::stop();
                let state = app.state::<crate::AppState>();
                let _ = crate::window::hide(app, &state);
                if let Ok(guard) = state.settings.lock() {
                    let _ = state.store.save(&guard);
                }
                app.exit(0);
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                let app = tray.app_handle();
                let state = app.state::<crate::AppState>();
                let _ = crate::window::toggle(app, &state);
            }
        })
        .build(app)
        .map_err(|e| format!("创建托盘图标失败：{e}"))?;

    Ok(())
}
