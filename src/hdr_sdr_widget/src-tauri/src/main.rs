//! 主程序入口：Tauri 应用装配。
//!
//! `hdr_sdr_widget_lib` 刻意不依赖 Tauri（见 `lib.rs` 顶部说明），因此所有
//! Tauri 侧的模块（`commands` / `tray` / `hotkey` / `window` / `backdrop` /
//! `autostart` / `store`）都在本二进制 crate 中声明。
//!
//! 装配职责：
//! - 注册全部 `#[tauri::command]`（与 `src/bridge.ts` 的契约一一对应）；
//! - 单实例插件（二次启动唤起面板而非新开进程）；
//! - 系统托盘、全局热键、面板窗口事件；
//! - 背景捕获（DDA）+ `glass://` 协议 + 贴边状态机线程。

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

// ⚠️ 编译期护栏：release 构建不能落进 dev 模式（否则前端资源不嵌入 exe）。
//
// 为什么需要它：dev 模式下 `AssetResolver` 走 `from_dev_url` → 前端资源**完全不
// 嵌入** exe → 用户双击后本地没有 1420 端口，得到一块白屏。这个错误编译期无提示、
// 运行时只在用户机器上暴露，2026-09-11 实际发生过一次（已构建的 exe 因此报废）。
//
// 为什么判 `dev` 而**不是** `feature = "custom-protocol"`：
//   `dev` 是 tauri-build 依据 `DEP_TAURI_DEV` 发出的 cfg 别名
//   （见 tauri-build `cfg_alias("dev", is_dev())`），而 `DEP_TAURI_DEV` 来自
//   tauri 自己的构建脚本 `let dev = !has_feature("custom-protocol")`。
//   也就是说 `dev` 判的是 **tauri crate 实际编译成什么样**，与"特征从哪条路径
//   进来"无关。这一点至关重要：`tauri build` 注入的是**依赖特征**
//   `tauri/custom-protocol`，并不会点亮本 crate 的同名 app 特征 ——
//   若用 `feature = "custom-protocol"` 判断，`npm run tauri -- build`
//   会直接编译失败（2026-09-12 实测踩过）。
//
// 放在 main.rs 而不是 lib.rs：lib 刻意不依赖 tauri，且 `probe*` 验证二进制
// 不该被这条约束牵连 —— 它们本来就用 debug 构建。
#[cfg(all(not(debug_assertions), dev))]
compile_error!(
    "release 构建落进了 dev 模式：前端资源不会嵌入 exe，用户双击会白屏。\n\
     正确出包方式：`npm run tauri -- build`（会注入 tauri/custom-protocol）。\n\
     手工构建则用：`cargo build --release --features custom-protocol`。"
);

mod anim;
mod brightness;
mod brightness_audit;
mod dimmer;
mod native;
mod autostart;
mod backdrop;
mod commands;
mod edge;
mod hotkey;
mod store;
mod tray;
mod window;

use std::sync::Arc;

use tauri::{Emitter, Manager};

use hdr_sdr_widget_lib::core::monitor_watch::{self, DisplayPowerState, MonitorEvent, WatcherHandle};
use hdr_sdr_widget_lib::win32::capture::{self, CaptureShared};
use hdr_sdr_widget_lib::win32::geometry;
use hdr_sdr_widget_lib::win32::hit_test;

/// 全局状态（Tauri `manage` 注册）。
pub use commands::AppState;

thread_local! {
    /// 显示变化监听线程的句柄。
    ///
    /// 放进 `thread_local!` 而不是 Tauri 的 `manage`：`WatcherHandle` 内含
    /// `HWND`（裸指针），既非 `Send` 也非 `Sync`，过不了 `manage` 的 trait 约束。
    /// 监听线程本身是按进程生命周期常驻的，放在创建它的主线程上正好 —— 主线程
    /// 退出即进程退出，`Drop` 会投递退出消息，不需要提前手动 stop。
    static MONITOR_WATCH: std::cell::RefCell<Option<WatcherHandle>> =
        const { std::cell::RefCell::new(None) };
}

/// 启动显示变化监听，并把事件桥接到 Tauri 事件与窗口自愈。
///
/// 这是 ARCHITECTURE.md S2.8 的落地接线。此前 `monitor_watch` 模块实现完整、
/// 单测齐全，却从没有任何地方调用 `start()` —— 等于没做：改分辨率、拔插显示器、
/// 在系统设置里开关 HDR，程序全程无感，读数与窗口位置都停在旧状态。
fn start_monitor_watch(app: &tauri::AppHandle) {
    let (tx, rx) = std::sync::mpsc::channel::<MonitorEvent>();
    match monitor_watch::start(tx) {
        Ok(handle) => MONITOR_WATCH.with(|slot| *slot.borrow_mut() = Some(handle)),
        Err(e) => {
            // 监听起不来不该拖垮主程序：热键、托盘、亮度写入都不依赖它。
            eprintln!("[monitor-watch] 启动失败，显示变化将不会被感知：{e}");
            return;
        }
    }

    let app = app.clone();
    let spawned = std::thread::Builder::new()
        .name("monitor-bridge".to_string())
        .spawn(move || {
            // `recv()` 返回 Err 表示监听端已退出（进程收尾），自然结束即可。
            while let Ok(ev) = rx.recv() {
                let state = app.state::<AppState>();
                if let Some(worker) = state.brightness.get() {
                    match &ev {
                        MonitorEvent::SettingChange => worker.refresh(),
                        MonitorEvent::DisplayChange | MonitorEvent::PowerDisplay(DisplayPowerState::On) => worker.invalidate(),
                        _ => {}
                    }
                }
                match ev {
                    MonitorEvent::DisplayChange => {
                        native::invalidate_capture();
                        // 拓扑/分辨率变了：
                        // 1. 控制器缓存的显示器列表作废 → 重枚举；
                        // 2. 窗口原坐标可能已落在不存在的区域 → 重新钳回工作区。
                        let monitors = commands::refresh_monitors(&state);
                        let _ = app.emit("monitors:changed", monitors);
                        if let Err(e) = window::reclamp(&app, &state) {
                            eprintln!("[monitor-watch] 越界纠正失败：{e}");
                        }
                    }
                    MonitorEvent::SettingChange => {
                        // 系统设置变化，多数情况是用户刚开了 HDR：读数必须重读，
                        // 否则胶囊上的 HDR 提示点会一直停在旧状态。
                        let monitors = commands::refresh_monitors(&state);
                        let _ = app.emit("monitors:changed", monitors);
                    }
                    MonitorEvent::PowerDisplay(DisplayPowerState::On) => {
                        native::invalidate_capture();
                        // 屏幕唤醒：DDA 会话通常已失效，捕获线程会自行重连；
                        // 这里让前端重读一次，避免显示唤醒前的过期读数。
                        let _ = app.emit("widget:shown", ());
                    }
                    MonitorEvent::PowerDisplay(_) => {}
                }
            }
        });

    if let Err(e) = spawned {
        eprintln!("[monitor-watch] 桥接线程创建失败：{e}");
    }
}

/// Tauri 应用入口。
fn main() {
    // 背景捕获共享槽：协议处理器与贴边线程各持一份 Arc。
    let capture = CaptureShared::new();
    let capture_proto = Arc::clone(&capture);
    let capture_edge = Arc::clone(&capture);

    tauri::Builder::default()
        .register_uri_scheme_protocol("glass", move |ctx, req| {
            backdrop::serve(Arc::clone(&capture_proto), ctx, req)
        })
        .plugin(tauri_plugin_single_instance::init(|app, _args, _cwd| {
            let state = app.state::<AppState>();
            let _ = window::show(app, &state);
        }))
        .plugin(tauri_plugin_opener::init())
        .manage(AppState::new())
        .invoke_handler(tauri::generate_handler![
            commands::list_monitors,
            commands::read_sdr_level,
            commands::read_brightness,
            commands::set_control_mode,
            commands::reprobe_brightness,
            commands::apply_percent,
            commands::apply_preset,
            commands::get_settings,
            commands::set_settings,
            commands::toggle_window,
            commands::move_window,
            commands::end_window_drag,
            commands::get_widget_state,
            commands::set_pointer_phase,
            commands::quit_app,
            commands::open_hdr_settings,
            commands::get_diagnostics,
            commands::get_capture_stats,
            commands::pull_frame,
            commands::report_glass_status,
            commands::get_glass_status,
            commands::get_native_diagnostics,
            commands::open_diag_window,
            commands::close_diag_window,
            commands::show_toast,
            commands::get_toast_payload,
            commands::hide_toast_window,
        ])
        .setup(move |app| {
            let handle = app.handle();
            let config = &app.config().app.windows[0];
            if native::enabled() {
                tauri::window::WindowBuilder::from_config(handle, config)?.build()?;
            } else {
                tauri::WebviewWindowBuilder::from_config(handle, config)?.build()?;
            }

            // 托盘（常驻 + 左键切换 + 右键菜单）。
            tray::build_tray(handle).map_err(std::io::Error::other)?;
            // 全局热键。
            hotkey::reload(handle);

            // 显示变化监听（S2.8）：分辨率 / HDR 设置 / 屏幕电源 → 自愈与前端刷新。
            start_monitor_watch(handle);

            // 悬浮物窗口：不抢焦点 + 排除捕获 + 部分点击穿透。
            let mut hwnd: isize = 0;
            if let Ok(panel) = window::panel(handle) {
                if let Some(h) = window::widget_hwnd(&panel) {
                    geometry::apply_no_activate(h);

                    // 剥掉标题栏类样式（WS_CAPTION 等）。**必须做**：`decorations:false`
                    // 只是把非客户区画掉，样式位还在，而 Windows 对带标题按钮的窗口
                    // 强制最小宽度（100% 下约 136 DIP）→ 68 DIP 的窗口被撑到
                    // 175% 下的 236 物理像素，连锁导致帧尺寸翻倍、前端裁剪整体错位。
                    // 详见 geometry::apply_borderless 的注释。
                    geometry::apply_borderless(h);
                    // 创建时已被撑大，剥完样式要主动改回配置尺寸。
                    if let Ok(scale) = panel.scale_factor() {
                        geometry::set_window_size_dip(h, geometry::WINDOW_W, geometry::WINDOW_H, scale);
                    }

                    // 验收逃生门：默认必须排除捕获，否则 DDA 会把自己的画面抓进来 →
                    // 「玻璃套玻璃」的反馈回路。
                    //
                    // 但 `WDA_EXCLUDEFROMCAPTURE` 的副作用是：本窗口对**一切**截屏 API
                    // 隐身（Desktop Duplication / PrintWindow / mss / BitBlt / 截图工具
                    // 全部拍不到），于是 UI 外观就无法目视验收 —— 你会看到窗口明明在
                    // `windows` 列表里、位置也对，截图里却什么都没有。
                    //
                    // 因此留一个仅用于验收的开关：
                    //     set HSDR_NO_CAPTURE_EXCLUDE=1
                    // ⚠️ 开着它运行时画面会出现玻璃套玻璃，**不要日常使用**。
                    let exclude = !matches!(
                        std::env::var("HSDR_NO_CAPTURE_EXCLUDE").as_deref(),
                        Ok("1") | Ok("true")
                    );
                    if exclude {
                        geometry::apply_exclude_from_capture(h);
                    } else {
                        eprintln!(
                            "[验收模式] 已跳过 WDA_EXCLUDEFROMCAPTURE —— 本窗口可被截屏，\
                             但抓取画面会含自身（玻璃套玻璃）。"
                        );
                    }

                    if !native::enabled() { hit_test::install(h); }
                    hwnd = h.0 as isize;
                }
            }

            // 背景捕获线程 + 贴边状态机线程（捕获区域由状态机随状态刷新）。
            {
                let state = app.state::<AppState>();
                let settings = state.settings.lock().unwrap().clone();
                let worker = brightness::Worker::start(settings.last_monitor_key, settings.follow_mouse_monitor,
                    settings.control_modes, hwnd);
                let _ = state.brightness.set(worker);
                if let Some(path) = std::env::var_os("HSDR_BRIGHTNESS_AUDIT") {
                    if std::env::var_os("HSDR_CONFIG_DIR").is_some() {
                        brightness_audit::start(state.brightness.get().unwrap().clone(), path.into());
                    }
                }
            }
            if !native::enabled() { app.manage(capture::spawn(Arc::clone(&capture))); }
            if !native::enabled() { app.manage(edge::start(handle.clone(), capture_edge, hwnd)); }
            // 把捕获共享槽挂到 AppState，供 `get_capture_stats` 诊断命令读取。
            let _ = app.state::<AppState>().capture.set(Arc::clone(&capture));

            // 启动即显示悬浮物（首启贴右缘隐藏位、只露边缘提示量），否则窗口
            // 因 `visible:false` 一直不显示，用户找不到滑条。
            let _ = window::show(handle, app.state::<AppState>().inner());
            if native::enabled() { native::start(handle.clone())?; }

            // 验收逃生门（二）：程序化打开诊断窗口。
            //
            // 自动化 / 脚本化验收点不到托盘菜单（图标通常折叠在通知区溢出面板里），
            // 而诊断面是判断"玻璃链路到底走到哪一步"的唯一入口 —— 没有它，
            // 面板不显示时只能靠猜（本项目历史上就是这么空转 8 轮的）。
            //     set HSDR_OPEN_DIAG=1
            if matches!(std::env::var("HSDR_OPEN_DIAG").as_deref(), Ok("1") | Ok("true")) {
                if let Err(e) = commands::open_diag_window(handle.clone()) {
                    eprintln!("[验收模式] 打开诊断窗口失败：{e:?}");
                }
            }

            // 验收逃生门（三）：无头自检 —— 等一会儿把内部状态写成文件，然后退出。
            //
            // 面板外观从**外部**无从观察：窗口带 `WDA_EXCLUDEFROMCAPTURE` 时对一切
            // 截屏 API 隐身；即便关掉，WebView2 走 DirectComposition、没有 GDI 重定向
            // 表面，走 `BitBlt` 的截图库同样拍不到内容。所以"界面到底渲染到哪一步"
            // 必须由程序自己报出来。
            //
            //     set HSDR_STATUS_DUMP=<输出文件路径>
            //     set HSDR_STATUS_DELAY=6000      # 可选，等多久再写（默认 6000ms）
            //     set HSDR_STATUS_KEEP=1          # 可选，写完不退出（默认写完就退）
            if let Some(path) = std::env::var_os("HSDR_STATUS_DUMP") {
                let delay = std::env::var("HSDR_STATUS_DELAY")
                    .ok()
                    .and_then(|v| v.parse::<u64>().ok())
                    .unwrap_or(6000);
                let h = handle.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(std::time::Duration::from_millis(delay));
                    let text = commands::build_status_report(&h);
                    match std::fs::write(&path, &text) {
                        Ok(()) => eprintln!("[验收模式] 自检报告已写入 {}", path.to_string_lossy()),
                        Err(e) => eprintln!("[验收模式] 写自检报告失败：{e}"),
                    }
                    if !native::flag("HSDR_STATUS_KEEP") {
                        native::stop();
                        h.exit(0);
                    }
                });
            }

            // 验收逃生门（四）：程序化弹一条 Toast。
            //
            // Toast 的触发路径全是"热键冲突""写入失败"这类难以主动制造的场景，
            // 而它的实现被改动过（清载荷槽 / 跨 DPI 估宽）—— 没有这个开关就只能
            // 被动等它出现，等于没验证。与其它验收开关同族，日常不要开。
            //
            //     set HSDR_TEST_TOAST=<要显示的文字>
            if let Ok(msg) = std::env::var("HSDR_TEST_TOAST") {
                let h = handle.clone();
                std::thread::spawn(move || {
                    // 等悬浮物窗口就位（Toast 是贴着胶囊定位的）。
                    std::thread::sleep(std::time::Duration::from_millis(3500));
                    if let Err(e) = commands::show_toast(
                        h,
                        msg,
                        Some("warn".to_string()),
                        // 给足观察时间，否则验收还没截图就自己收了。
                        Some(20_000),
                    ) {
                        eprintln!("[验收模式] 弹 Toast 失败：{e:?}");
                    }
                });
            }

            Ok(())
        })
        .on_window_event(|win, event| {
            if win.label() == "panel" {
                window::on_window_event(win, event);
            }
        })
        .run(tauri::generate_context!())
        .expect("无法启动 HDR SDR 控制器");
}
