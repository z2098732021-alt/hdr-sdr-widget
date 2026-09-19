//! 系统托盘：单色图标 + 左键切换面板 + 右键菜单（显示 / 预设 / 退出）。
//!
//! PRD P0-1：程序常驻托盘，关闭面板不退出进程。托盘图标用单色扁平太阳 PNG
//! （`icons/32x32.png`，与 UI 图标同一视觉语言）。
//!
//! v0.2.0：胶囊 UI 精简为纯玻璃 + Fill（无图标 / 无文字），三档亮度预设
//! 的入口迁移到托盘菜单，避免破坏胶囊的克制观感。

use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu, CheckMenuItem};
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
                if let Some(worker) = state.brightness.get() { worker.stop(); }
                let _ = crate::window::hide(app, &state);
                if let Ok(guard) = state.settings.lock() {
                    let _ = state.store.save(&guard);
                }
                app.exit(0);
            }
            "brightness:reprobe" => crate::commands::reprobe_brightness(app.state::<crate::AppState>()),
            "brightness:restore" => {
                if let Some(worker) = app.state::<crate::AppState>().brightness.get() { worker.restore_software(); }
            }
            id if id.starts_with("mode:") => {
                let mut parts = id.splitn(3, ':'); parts.next();
                let mode = match parts.next() { Some("hdr") => crate::brightness::Mode::Hdr, Some("ddc") => crate::brightness::Mode::Ddc,
                    Some("software") => crate::brightness::Mode::Software, _ => crate::brightness::Mode::Auto };
                if let Some(key) = parts.next() {
                    if let Err(e) = crate::commands::set_control_mode(key.into(), mode, app.state::<crate::AppState>()) {
                        eprintln!("[brightness mode] {e:?}");
                    }
                }
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

    let app = app.clone();
    std::thread::spawn(move || {
        let mut signature = String::new();
        loop {
            std::thread::sleep(std::time::Duration::from_millis(750));
            let state = app.state::<crate::AppState>();
            let Some(worker) = state.brightness.get() else { continue; };
            let readings = worker.readings();
            let next = readings.iter().map(|r| format!("{}:{:?}:{:?}:{:?}:{}",r.key,r.mode,r.backend,r.fallback_reason,r.can_control)).collect::<Vec<_>>().join("|");
            if next == signature { continue; } signature = next;
            if let Ok(menu) = control_menu(&app, &readings) {
                if let Some(tray) = app.tray_by_id("main-tray") { let _ = tray.set_menu(Some(menu)); }
            }
        }
    });
    Ok(())
}

fn control_menu(app: &AppHandle, readings: &[crate::brightness::Reading]) -> tauri::Result<Menu<tauri::Wry>> {
    use crate::brightness::Mode;
    let menu = Menu::new(app)?;
    menu.append(&MenuItem::with_id(app,"show","显示/隐藏滑条",true,None::<&str>)?)?;
    let controls = Submenu::new(app,"亮度控制方式",true)?;
    for r in readings {
        let display = Submenu::new(app,&r.name,true)?;
        display.append(&MenuItem::new(app,format!("当前：{}",r.backend.label()),false,None::<&str>)?)?;
        if let Some(reason) = &r.fallback_reason { display.append(&MenuItem::new(app,reason,false,None::<&str>)?)?; }
        for (id,label,mode) in [("auto","自动",Mode::Auto),("hdr","HDR SDR 内容亮度",Mode::Hdr),("ddc","DDC/CI 硬件亮度",Mode::Ddc),("software","软件压暗",Mode::Software)] {
            display.append(&CheckMenuItem::with_id(app,format!("mode:{id}:{}",r.key),label,true,r.mode==mode,None::<&str>)?)?;
        }
        controls.append(&display)?;
    }
    menu.append(&controls)?;
    menu.append(&MenuItem::with_id(app,"brightness:reprobe","重新检测显示器控制能力",true,None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app,"brightness:restore","恢复所有屏幕的软件亮度",true,None::<&str>)?)?;
    let presets = Submenu::new(app,"预设亮度",true)?;
    for (id,label) in [("day","白天"),("movie","观影"),("night","夜间")] { presets.append(&MenuItem::with_id(app,format!("preset:{id}"),label,true,None::<&str>)?)?; }
    menu.append(&presets)?;
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&MenuItem::with_id(app,"diag","诊断面板…",true,None::<&str>)?)?;
    menu.append(&MenuItem::with_id(app,"quit","退出",true,None::<&str>)?)?;
    Ok(menu)
}
