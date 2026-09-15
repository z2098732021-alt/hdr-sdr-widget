//! 悬浮物窗口：几何编排、贴边 / 拖拽落位、位置记忆与越界纠正、点击穿透命中矩形。
//!
//! v0.2.0 形态：窗口 = 「胶囊 + 四周透明 halo」（`WINDOW_W/H`），胶囊居中
//! （`CAPSULE_MARGIN`）。halo 为 hover 放大 / 投影 / 折射取样留白，且通过
//! [`hit_test`] 把 halo 区域设为点击穿透，只有胶囊本身接收鼠标。
//!
//! 所有位移走 `SWP_NOACTIVATE`（配合 `WS_EX_NOACTIVATE`，全程不抢焦点）。
//! 状态唯一真源在 `crate::edge::WidgetState`（存于 `AppState.widget`）；
//! 本模块只做「读状态 → 算坐标 → 移窗口 → 持久化」。

use tauri::{AppHandle, Emitter, Manager, PhysicalPosition};
use windows::Win32::Foundation::HWND;

use hdr_sdr_widget_lib::error::AppError;
use hdr_sdr_widget_lib::win32::geometry::{self, Rect};
use hdr_sdr_widget_lib::win32::hit_test;

use crate::edge::{Edge, Phase, WidgetState};
use crate::AppState;

/// 悬浮物窗口的**物理**几何（运行时取值，随 DPI 缩放变化）。
#[derive(Clone, Copy, Debug)]
pub(crate) struct WidgetMetrics {
    /// 胶囊物理宽（=`phys(WIDGET_W, scale)`）。
    pub cap_w: i32,
    /// 胶囊物理高。
    pub cap_h: i32,
    /// halo 物理边距（胶囊四周透明留白）。
    pub margin: i32,
    /// 窗口物理宽（=`outer_size().width`）。
    pub win_w: i32,
    /// 窗口物理高。
    pub win_h: i32,
    /// 窗口 DPI 缩放系数（如 175% → `1.75`）。
    pub scale: f64,
}

impl WidgetMetrics {
    /// 从悬浮物窗口取实际物理几何；取不到时回退逻辑尺寸常量 + 缩放 1.0。
    pub(crate) fn from_window(window: &tauri::Window) -> Self {
        let size = window.outer_size().unwrap_or_default();
        let scale = window.scale_factor().unwrap_or(1.0);
        Self {
            cap_w: geometry::phys(geometry::WIDGET_W, scale),
            cap_h: geometry::phys(geometry::WIDGET_H, scale),
            margin: geometry::phys(geometry::CAPSULE_MARGIN, scale),
            win_w: if size.width > 0 { size.width as i32 } else { geometry::phys(geometry::WINDOW_W, scale) },
            win_h: if size.height > 0 { size.height as i32 } else { geometry::phys(geometry::WINDOW_H, scale) },
            scale,
        }
    }

    /// 展开时胶囊距屏边（工作区）的内边距（物理像素）。
    pub(crate) fn inset(&self) -> i32 {
        geometry::phys(geometry::EDGE_INSET, self.scale)
    }

    /// 贴边时露出的窄边宽度（物理像素）。
    pub(crate) fn reveal(&self) -> i32 {
        geometry::phys(geometry::EDGE_REVEAL, self.scale)
    }

    /// 边缘唤出热区宽度（物理像素）。
    pub(crate) fn zone_hit(&self) -> i32 {
        geometry::phys(geometry::EDGE_ZONE_HIT, self.scale)
    }

    /// 唤出走廊在 Y 方向超出胶囊的上下额外量（物理像素）。
    pub(crate) fn corridor_y(&self) -> i32 {
        geometry::phys(geometry::CORRIDOR_Y, self.scale)
    }

    /// 松手吸附带宽度（物理像素）。
    pub(crate) fn snap(&self) -> i32 {
        geometry::phys(geometry::SNAP_BAND, self.scale)
    }
}

/// 窗口 label（见 tauri.conf.json）。
pub(crate) const PANEL_LABEL: &str = "panel";

/// WebView2 额外浏览器参数（**唯一真源**）。
///
/// `tauri.conf.json` 里 panel 窗口有一份等价副本（配置创建的窗口无法用代码设参），
/// 由测试 `浏览器参数与配置不漂移` 守着两处不漂移。
///
/// # 为什么必须 `--in-process-gpu`
///
/// 真机实测：WebView2 的**独立 GPU 进程在本机崩溃循环** —— Chromium 日志稳定给出
/// `GPU process exited unexpectedly: exit_code=1`（复现 9 次以上），
/// 并伴随用户数据目录里 `DawnGraphiteCache` 的缓存文件争用报错。
///
/// 后果远不止"GPU 不可用"：GPU 进程反复死亡会让**渲染进程的 JS 在页面加载后
/// 约 0.5 秒整体停止执行** —— 定时器、IPC、fetch 全部静默，而进程仍然活着。
/// 外部表现为"取帧循环跑十几次就再也不动、`rendered` 永远为 0、胶囊没有背景内容"，
/// 极难与"没渲染"区分。本项目为此空转了很多轮。
///
/// 换成进程内 GPU 后：GPU 崩溃 **0 次**，取帧循环可连续运行。
///
/// ⚠️ 一旦设置本参数，wry 的默认值就不再自动附加，必须自己带上
/// `--disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection`。
///
/// ⚠️ 所有 webview 必须用**同一个值**（WebView2 环境按首个 webview 的参数创建；
/// 参数不同的 webview 还会被要求使用不同的数据目录）。
pub(crate) const BROWSER_ARGS: &str =
    "--in-process-gpu --disable-features=msWebOOUI,msPdfOOUI,msSmartScreenProtection";

#[cfg(test)]
mod browser_args_tests {
    /// 配置里那份副本必须与 [`super::BROWSER_ARGS`] 完全一致。
    ///
    /// 为什么值得单独写个测试：这类"同一字符串两处各写一份"的漂移正是本项目
    /// 反复吃亏的地方（光学令牌也栽过一次）。改了代码忘改配置 = 修复静默失效。
    /// 窗口尺寸在**三处各写一份**：`geometry.rs` 常量、前端 `liquidGlass.ts`、
    /// `tauri.conf.json`。三者必须一致。
    ///
    /// 为什么值得单独写测试：前端用 `frame_w / WINDOW_W` 反推 DPI 缩放系数，
    /// 数字漂移 → 系数错 → 裁剪整体偏移。本项目就栽过 —— 窗口被 Windows 强制
    /// 撑宽一倍（119 → 236）时，三处数字"各自看都对"但对不上，排查耗了很久。
    /// **数字类的一致性必须交给机器守，不能靠人记。**
    #[test]
    fn 窗口尺寸三处一致() {
        let conf: serde_json::Value = serde_json::from_str(include_str!("../tauri.conf.json"))
            .expect("tauri.conf.json 解析失败");
        let win = &conf["app"]["windows"][0];
        assert_eq!(
            win["width"].as_i64(),
            Some(i64::from(super::geometry::WINDOW_W)),
            "tauri.conf.json 的 width 与 geometry::WINDOW_W 不一致"
        );
        assert_eq!(
            win["height"].as_i64(),
            Some(i64::from(super::geometry::WINDOW_H)),
            "tauri.conf.json 的 height 与 geometry::WINDOW_H 不一致"
        );

        // v0.3.2：前端改吃**胶囊区域**帧（见 `edge.rs::update_capture`），靠胶囊
        // 尺寸反推缩放，所以这里守护的是**胶囊**尺寸（WIDGET_W/H）三处一致。
        let ts = include_str!("../../src/ui/liquidGlass.ts");
        for (name, want) in [
            ("CAPSULE_W", super::geometry::WIDGET_W),
            ("CAPSULE_H", super::geometry::WIDGET_H),
        ] {
            let needle = format!("const {name} = {want};");
            assert!(
                ts.contains(&needle),
                "liquidGlass.ts 里找不到 `{needle}` —— 胶囊尺寸必须与 Rust 常量一致"
            );
        }
    }

    #[test]
    fn 浏览器参数与配置不漂移() {
        let conf = include_str!("../tauri.conf.json");
        assert!(
            conf.contains(super::BROWSER_ARGS),
            "tauri.conf.json 里的 additionalBrowserArgs 与 window::BROWSER_ARGS 不一致。\n\
             期望配置里出现：{}\n\
             两处必须同步修改（配置创建的 panel 窗口无法用代码设参）。",
            super::BROWSER_ARGS
        );
    }
}

/// 悬浮物窗口的 DPI 缩放系数（命令层把前端逻辑位移换算成物理位移用）。
pub(crate) fn window_scale(app: &AppHandle) -> f64 {
    panel(app)
        .ok()
        .and_then(|w| w.scale_factor().ok())
        .unwrap_or(1.0)
}

/// 取悬浮物窗口（label = "panel"）。
pub(crate) fn panel(app: &AppHandle) -> Result<tauri::Window, AppError> {
    app.get_window(PANEL_LABEL)
        .ok_or_else(|| AppError::Inconsistent("找不到悬浮物窗口".to_string()))
}

/// 取悬浮物窗口的 Win32 句柄。
pub(crate) fn widget_hwnd(window: &tauri::Window) -> Option<HWND> {
    let h = window.hwnd().ok()?;
    if h.is_invalid() {
        None
    } else {
        Some(HWND(h.0))
    }
}

/// 读当前状态。
pub(crate) fn widget_state(state: &AppState) -> WidgetState {
    state.widget.lock().map(|g| *g).unwrap_or_default()
}

/// 写入状态（唯一真源）。
pub(crate) fn set_widget_state(state: &AppState, target: WidgetState) {
    if let Ok(mut guard) = state.widget.lock() {
        *guard = target;
    }
}

/// 广播状态（`widget:state`）。
pub(crate) fn emit_state(app: &AppHandle, state: &AppState) {
    let ws = widget_state(state);
    let _ = app.emit("widget:state", ws);
}

// ---------------------------------------------------------------------------
// 坐标计算（物理像素，基于胶囊位置）
// ---------------------------------------------------------------------------

/// 展开时窗口 X（胶囊内缘距屏边 `inset`）。
pub(crate) fn dock_expanded_x(area: &Rect, edge: Edge, m: &WidgetMetrics) -> i32 {
    match edge {
        Edge::Right => area.right - m.inset() - m.cap_w - m.margin,
        Edge::Left => area.left + m.inset() - m.margin,
    }
}

/// 贴边隐藏时窗口 X（胶囊只露 `reveal` 窄边）。
pub(crate) fn dock_hidden_x(area: &Rect, edge: Edge, m: &WidgetMetrics) -> i32 {
    match edge {
        Edge::Right => area.right - m.reveal() - m.margin,
        Edge::Left => area.left + m.reveal() - m.cap_w - m.margin,
    }
}

/// 垂直位置钳位（胶囊 top 钳到工作区，再减 margin 得窗口 top）。
pub(crate) fn clamp_window_y(area: &Rect, cap_y: i32, m: &WidgetMetrics) -> i32 {
    let cap_top = cap_y.clamp(area.top + m.inset(), area.bottom - m.inset() - m.cap_h);
    cap_top - m.margin
}

/// 展开落位（胶囊位置 → 窗口位置）。
pub(crate) fn dock_expanded_pos(area: &Rect, edge: Edge, cap_y: i32, m: &WidgetMetrics) -> (i32, i32) {
    (dock_expanded_x(area, edge, m), clamp_window_y(area, cap_y, m))
}

/// 贴边隐藏落位（胶囊位置 → 窗口位置）。
pub(crate) fn dock_hidden_pos(area: &Rect, edge: Edge, cap_y: i32, m: &WidgetMetrics) -> (i32, i32) {
    (dock_hidden_x(area, edge, m), clamp_window_y(area, cap_y, m))
}

/// 胶囊命中矩形（窗口客户坐标，物理像素，含 2 DIP 缩放余量）。
pub(crate) fn hit_rect(m: &WidgetMetrics) -> [i32; 4] {
    let pad = geometry::phys(2, m.scale);
    [m.margin - pad, m.margin - pad, m.margin + m.cap_w + pad, m.margin + m.cap_h + pad]
}

/// 应用命中矩形（`None` = 全窗口点击穿透，用于隐藏 / 抑制态）。
pub(crate) fn apply_hit_rect(rect: Option<[i32; 4]>) {
    hit_test::set_rect(rect);
}

// ---------------------------------------------------------------------------
// 拖拽（命令 move_window / end_window_drag）
// ---------------------------------------------------------------------------

/// 拖拽移动：当前位置 + 位移 → 钳位到全部显示器工作区并集 → `SetWindowPos`。
pub(crate) fn move_window(app: &AppHandle, dx: i32, dy: i32) -> Result<(), AppError> {
    let window = panel(app)?;
    let m = WidgetMetrics::from_window(&window);
    let pos = window.outer_position().map_err(|e| AppError::Io(e.to_string()))?;
    let mut x = pos.x + dx;
    let mut y = pos.y + dy;
    if let Some(union) = all_work_area_union(app) {
        x = x.clamp(union.left - m.margin, union.right - m.win_w + m.margin);
        y = y.clamp(union.top - m.margin, union.bottom - m.win_h + m.margin);
    }
    if let Some(hwnd) = widget_hwnd(&window) {
        geometry::set_window_pos(hwnd, x, y, true);
    }
    let near = near_edge(&window);
    let _ = app.emit("widget:drag_hint", crate::edge::DragHint { near_edge: near });
    Ok(())
}

/// 松手落位：贴边吸附（记录 Monitor/Side/Y）/ 自由落位，并持久化。
pub(crate) fn end_window_drag(app: &AppHandle, state: &AppState) -> Result<(), AppError> {
    let window = panel(app)?;
    let m = WidgetMetrics::from_window(&window);
    let pos = window.outer_position().map_err(|e| AppError::Io(e.to_string()))?;
    // 胶囊 top-left（窗口 top-left + margin）。
    let cap_x = pos.x + m.margin;
    let cap_y = pos.y + m.margin;
    let area = current_work_area(cap_x, cap_y);

    let left_dist = cap_x - area.left;
    let right_dist = area.right - (cap_x + m.cap_w);

    // 贴边 → 吸附并**滑出隐藏**（spec §19/§20：拖到边缘即收进屏幕边缘）。
    let target = if right_dist <= m.snap() {
        WidgetState { docked: Some(Edge::Right), expanded: false, phase: Phase::Hidden }
    } else if left_dist <= m.snap() {
        WidgetState { docked: Some(Edge::Left), expanded: false, phase: Phase::Hidden }
    } else {
        WidgetState { docked: None, expanded: true, phase: Phase::Visible }
    };

    if let Some(edge) = target.docked {
        let (tx, ty) = dock_hidden_pos(&area, edge, cap_y, &m);
        if let Some(hwnd) = widget_hwnd(&window) {
            crate::anim::animate_to(hwnd, tx, ty, &crate::anim::SETTLE, geometry::DOCK_ANIM_MS, &|| false);
        }
    } else {
        let x = cap_x.clamp(area.left + m.inset(), area.right - m.cap_w - m.inset());
        let y = cap_y.clamp(area.top + m.inset(), area.bottom - m.cap_h - m.inset());
        let (wx, wy) = (x - m.margin, y - m.margin);
        if (wx, wy) != (pos.x, pos.y) {
            if let Some(hwnd) = widget_hwnd(&window) {
                geometry::set_window_pos(hwnd, wx, wy, true);
            }
        }
    }

    set_widget_state(state, target);
    emit_state(app, state);
    // 贴边隐藏 → 命中矩形全穿透；自由/展开 → 只命中胶囊。
    if target.expanded {
        apply_hit_rect(Some(hit_rect(&m)));
    } else {
        apply_hit_rect(None);
    }

    // 持久化：贴边记 `dock_side + widget_y`（胶囊 Y）；自由态记 `last_window_pos`。
    // `last_window_pos` 无论哪种状态都写 —— 它是下次启动判断"该在哪块屏落位"的
    // 锚点，缺了它副屏贴边重启后会跑回主屏。
    // 先读显示器键，再进配置锁。
    // 反过来写（持 settings 去锁 current_key）今天不会死锁，但它建立了一个
    // settings → current_key 的加锁顺序，一旦将来有人写出反向路径就是死锁。
    // 锁顺序要么统一、要么根本别嵌套。
    let monitor_key = state
        .current_key
        .lock()
        .ok()
        .and_then(|k| k.clone())
        .unwrap_or_default();
    let mut settings = state
        .settings
        .lock()
        .map_err(|_| AppError::Inconsistent("配置锁不可用".to_string()))?;
    match target.docked {
        Some(edge) => {
            settings.dock_side = Some(edge);
            settings.widget_y = cap_y;
            settings.last_window_pos.x = cap_x;
            settings.last_window_pos.y = cap_y;
        }
        None => {
            settings.dock_side = None;
            settings.last_window_pos.x = cap_x;
            settings.last_window_pos.y = cap_y;
        }
    }
    settings.last_window_pos.monitor_key = monitor_key;
    let _ = state.store.save(&settings);
    Ok(())
}

// ---------------------------------------------------------------------------
// 显隐切换（托盘 / 热键 / 单实例唤起）
// ---------------------------------------------------------------------------

/// 切换悬浮物显隐。
pub(crate) fn toggle(app: &AppHandle, state: &AppState) -> Result<bool, AppError> {
    let window = panel(app)?;
    if window.is_visible().unwrap_or(false) {
        hide(app, state)?;
        Ok(false)
    } else {
        show(app, state)?;
        Ok(true)
    }
}

/// 显示悬浮物：按当前状态定位（首启默认贴右缘隐藏）。
pub(crate) fn show(app: &AppHandle, state: &AppState) -> Result<(), AppError> {
    let window = panel(app)?;
    let m = WidgetMetrics::from_window(&window);

    let first_run = state.settings.lock().map(|g| g.first_run).unwrap_or(false);
    if first_run {
        let mut settings = state
            .settings
            .lock()
            .map_err(|_| AppError::Inconsistent("配置锁不可用".to_string()))?;
        settings.first_run = false;
        settings.dock_side = Some(Edge::Right);
        let area = current_work_area(0, 0);
        // 首启默认贴右缘，垂直居中（spec §2「垂直方向接近中部」）。
        let cap_y = area.top + (area.height() - m.cap_h) / 2;
        settings.widget_y = cap_y;
        let _ = state.store.save(&settings);
        drop(settings);
        // 首启贴右缘隐藏位（只露 2 DIP 边缘提示量，spec §21/§43）。
        set_widget_state(state, WidgetState { docked: Some(Edge::Right), expanded: false, phase: Phase::Hidden });
        let (x, y) = dock_hidden_pos(&area, Edge::Right, cap_y, &m);
        let _ = window.set_position(PhysicalPosition::new(x, y));
    } else {
        // 把持久化的贴边方向**读回运行时状态**。
        //
        // ⚠️ 这是个真 bug 的修复：`settings.dock_side` 此前**只写不读**
        // （写入在拖拽结束处 `persist_*`），于是重启后运行时状态的 `docked`
        // 恒为默认值 `None`。而状态机在 `docked == None` 时直接 return
        // （见 `edge.rs::poll_once`："自由悬浮：不监听边缘唤出 / 自动隐藏"）——
        // 结果是**贴过边的用户每次重启都会丢贴边状态**：自动隐藏与靠边唤出
        // 全部失效，必须重新把胶囊拖到边缘才能恢复。下面这个 `Some(edge)`
        // 分支因此从来没被执行过。
        let remembered = state.settings.lock().map(|g| g.dock_side).unwrap_or(None);
        let ws = WidgetState { docked: remembered, ..widget_state(state) };
        set_widget_state(
            state,
            match remembered {
                // 贴边：与首启一致，落在隐藏位并置 `Hidden`，等待边缘唤出。
                Some(_) => WidgetState { phase: Phase::Hidden, expanded: false, ..ws },
                // 自由悬浮：保持完整显示。
                None => ws,
            },
        );

        let ws = widget_state(state);
        // 用**记忆位置所在屏**的工作区，而不是 `(0, 0)`。
        // `current_work_area(0, 0)` 永远解析成主屏（或恰好覆盖 0,0 的那块），
        // 于是副屏上贴边的悬浮物重启后会跑到主屏右缘去 —— 位置记了，屏幕没记。
        let seed = state
            .settings
            .lock()
            .map(|g| (g.last_window_pos.x, g.last_window_pos.y))
            .unwrap_or((0, 0));
        let area = current_work_area(seed.0, seed.1);
        let (x, y) = match ws.docked {
            Some(edge) => {
                let cap_y = state.settings.lock().map(|g| g.widget_y).unwrap_or(220);
                let (x, y) = dock_hidden_pos(&area, edge, cap_y, &m);
                (x, y)
            }
            None => {
                let (px, py) = seed;
                (px - m.margin, py - m.margin)
            }
        };
        let _ = window.set_position(PhysicalPosition::new(x, y));
    }

    window.show().map_err(|e| AppError::Io(e.to_string()))?;
    emit_state(app, state);
    // 命中矩形：贴边隐藏态全穿透；展开 / 自由态只命中胶囊。
    let ws = widget_state(state);
    if ws.expanded {
        apply_hit_rect(Some(hit_rect(&m)));
    } else {
        apply_hit_rect(None);
    }
    let _ = app.emit("widget:shown", ());
    Ok(())
}

/// 隐藏悬浮物。
pub(crate) fn hide(app: &AppHandle, state: &AppState) -> Result<(), AppError> {
    remember_position(app, state);
    let window = panel(app)?;
    window.hide().map_err(|e| AppError::Io(e.to_string()))
}

/// 读当前状态（命令 `get_widget_state`）。
pub(crate) fn get_widget_state(state: &AppState) -> WidgetState {
    widget_state(state)
}

/// 悬浮物窗口事件（`main.rs` 在 label == "panel" 时调用）。
pub(crate) fn on_window_event(window: &tauri::Window, event: &tauri::WindowEvent) {
    match event {
        tauri::WindowEvent::CloseRequested { api, .. } => {
            api.prevent_close();
            let _ = window.hide();
        }
        tauri::WindowEvent::ScaleFactorChanged { scale_factor, .. } => {
            // DPI 变化：WebView 会自动 resize，这里重算命中矩形保证 Hit Test 不偏移。
            //
            // v0.3 修复两处：
            // 1. 用**事件携带的** `scale_factor`，而不是 `window.scale_factor()`。
            //    后者在事件派发时可能仍是旧值（Tauri 在变更生效前广播），会让命中
            //    矩形按旧 DPI 计算 —— 175% ↔ 100% 之间切换时肉眼可见地错位。
            // 2. 隐藏 / 抑制态下必须保持"全穿透"。旧代码无条件写入胶囊命中矩形，
            //    结果：在隐藏态发生 DPI 变化，那根只露 2 DIP 的提示条会突然开始
            //    吃点击，而状态机并不知道自己已不穿透。
            let m = WidgetMetrics::from_window_scale(*scale_factor);
            let visible = matches!(
                widget_state(&window.app_handle().state::<AppState>()).phase,
                Phase::Revealing | Phase::Visible | Phase::Hovered | Phase::Pressed | Phase::Dragging
            );
            apply_hit_rect(if visible { Some(hit_rect(&m)) } else { None });
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

impl WidgetMetrics {
    /// 按显式 scale 构造度量（用于 DPI 变化时无需完整窗口）。
    pub(crate) fn from_window_scale(scale: f64) -> Self {
        WidgetMetrics {
            cap_w: geometry::phys(geometry::WIDGET_W, scale),
            cap_h: geometry::phys(geometry::WIDGET_H, scale),
            margin: geometry::phys(geometry::CAPSULE_MARGIN, scale),
            win_w: geometry::phys(geometry::WINDOW_W, scale),
            win_h: geometry::phys(geometry::WINDOW_H, scale),
            scale,
        }
    }
}

/// 窗口所在显示器的**工作区**（按点定位；失败退回主屏）。
fn current_work_area(x: i32, y: i32) -> Rect {
    geometry::monitor_work_area(x, y)
        .or_else(geometry::primary_work_area)
        .unwrap_or_default()
}

/// 全部显示器工作区的并集（拖拽钳位边界）。
fn all_work_area_union(app: &AppHandle) -> Option<Rect> {
    let monitors = app.available_monitors().ok()?;
    let mut union: Option<Rect> = None;
    for m in monitors {
        let r = m.work_area();
        let rect = Rect {
            left: r.position.x,
            top: r.position.y,
            right: r.position.x + r.size.width as i32,
            bottom: r.position.y + r.size.height as i32,
        };
        union = Some(match union {
            Some(u) => Rect {
                left: u.left.min(rect.left),
                top: u.top.min(rect.top),
                right: u.right.max(rect.right),
                bottom: u.bottom.max(rect.bottom),
            },
            None => rect,
        });
    }
    union
}

/// 胶囊内缘距最近屏边是否进入吸附带。
fn near_edge(window: &tauri::Window) -> Option<Edge> {
    let Ok(pos) = window.outer_position() else { return None };
    let m = WidgetMetrics::from_window(window);
    let cap_x = pos.x + m.margin;
    let cap_y = pos.y + m.margin;
    let area = current_work_area(cap_x + m.cap_w / 2, cap_y + m.cap_h / 2);
    let left_dist = cap_x - area.left;
    let right_dist = area.right - (cap_x + m.cap_w);
    if right_dist <= m.snap() {
        Some(Edge::Right)
    } else if left_dist <= m.snap() {
        Some(Edge::Left)
    } else {
        None
    }
}

/// 记忆位置到配置并落盘（胶囊坐标）。
///
/// **任何状态都记**，不只自由态。原因：`show()` 需要用一个坐标点来判断
/// "该在哪块屏上落位"，而贴边态的唯一可用点就是当前位置。以前贴边时直接
/// return，导致 `last_window_pos` 永远停在"上一次自由拖动过的地方"，副屏
/// 贴边重启后锚点就错了。
fn remember_position(app: &AppHandle, state: &AppState) {
    let window = match panel(app) {
        Ok(w) => w,
        Err(_) => return,
    };
    let m = WidgetMetrics::from_window(&window);
    let Ok(mut pos) = window.outer_position() else { return };
    if crate::native::enabled() {
        if let Some((x,y)) = crate::native::position() { pos.x=x; pos.y=y; }
    }
    // 同 `end_window_drag`：先读 current_key，再进 settings 锁，不做嵌套。
    let monitor_key = state
        .current_key
        .lock()
        .ok()
        .and_then(|k| k.clone())
        .unwrap_or_default();
    if let Ok(mut settings) = state.settings.lock() {
        settings.last_window_pos.x = pos.x + m.margin;
        settings.last_window_pos.y = pos.y + m.margin;
        // 记录锚点屏的稳定键，供跨会话恢复（显示器重排后仍能对上）。
        settings.last_window_pos.monitor_key = monitor_key;
        let _ = state.store.save(&settings);
    }
}

/// 把窗口重新钳回当前所在屏的工作区。
///
/// 用途：`WM_DISPLAYCHANGE` / 分辨率变化后，窗口原来的坐标可能落在已不存在的
/// 区域（拔掉副屏时尤其明显），表现为悬浮物"消失在屏幕外，热键唤也唤不出来"。
pub(crate) fn reclamp(app: &AppHandle, state: &AppState) -> Result<(), AppError> {
    let window = panel(app)?;
    let m = WidgetMetrics::from_window(&window);
    let pos = window.outer_position().map_err(|e| AppError::Io(e.to_string()))?;
    let cap_x = pos.x + m.margin;
    let cap_y = pos.y + m.margin;
    let area = current_work_area(cap_x, cap_y);

    let ws = widget_state(state);
    let (x, y) = match ws.docked {
        Some(edge) => {
            let cap_y = cap_y.clamp(area.top + m.inset(), area.bottom - m.inset() - m.cap_h);
            let (x, _) = dock_hidden_pos(&area, edge, cap_y, &m);
            (x, clamp_window_y(&area, cap_y, &m))
        }
        None => {
            let cx = cap_x.clamp(area.left + m.inset(), area.right - m.cap_w - m.inset());
            let cy = cap_y.clamp(area.top + m.inset(), area.bottom - m.cap_h - m.inset());
            (cx - m.margin, cy - m.margin)
        }
    };
    if (x, y) != (pos.x, pos.y) {
        if let Some(hwnd) = widget_hwnd(&window) {
            geometry::set_window_pos(hwnd, x, y, true);
        }
    }
    // 钳位后位置变了，记忆也得跟着更新，否则下次启动又回到屏外。
    remember_position(app, state);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn area() -> Rect {
        Rect { left: 0, top: 0, right: 3840, bottom: 2160 }
    }

    fn m100() -> WidgetMetrics {
        WidgetMetrics { cap_w: 40, cap_h: 200, margin: 14, win_w: 68, win_h: 228, scale: 1.0 }
    }

    fn m175() -> WidgetMetrics {
        WidgetMetrics { cap_w: 70, cap_h: 350, margin: 25, win_w: 119, win_h: 399, scale: 1.75 }
    }

    #[test]
    fn 展开坐标右缘() {
        // 胶囊右缘距屏边 inset(8)，窗口 = 胶囊 - margin。
        assert_eq!(dock_expanded_x(&area(), Edge::Right, &m100()), 3840 - 8 - 40 - 14);
        assert_eq!(dock_expanded_x(&area(), Edge::Left, &m100()), 8 - 14);
        assert_eq!(dock_expanded_x(&area(), Edge::Right, &m175()), 3840 - 14 - 70 - 25);
    }

    #[test]
    fn 隐藏坐标露2dip() {
        assert_eq!(dock_hidden_x(&area(), Edge::Right, &m100()), 3840 - 2 - 14);
        assert_eq!(dock_hidden_x(&area(), Edge::Left, &m100()), 0 + 2 - 40 - 14);
        assert_eq!(dock_hidden_x(&area(), Edge::Right, &m175()), 3840 - 4 - 25);
    }

    #[test]
    fn 垂直钳位到工作区() {
        // 胶囊 top 钳到 [inset, bottom - inset - cap_h]，窗口 top 再减 margin。
        assert_eq!(clamp_window_y(&area(), -999, &m100()), 8 - 14);
        assert_eq!(clamp_window_y(&area(), 500, &m100()), 500 - 14);
        assert_eq!(clamp_window_y(&area(), 99999, &m100()), 2160 - 8 - 200 - 14);
    }

    #[test]
    fn 命中矩形含缩放余量() {
        // pad = phys(2, scale)：100% → 2，175% → 4。
        assert_eq!(hit_rect(&m100()), [12, 12, 56, 216]);
        assert_eq!(hit_rect(&m175()), [21, 21, 99, 379]);
    }
}
