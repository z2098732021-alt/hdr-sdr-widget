//! 悬浮物几何：光标位置 / 显示器工作区 / 显示器矩形 / 窗口移动 / 捕获排除。
//!
//! 对应 ARCHITECTURE.md §4.1（常量表与函数签名）。全部 `unsafe` 收敛于
//! 本文件，对外只暴露纯值函数。函数返回的坐标均为**物理像素**（§8.1 统一基准），
//! 以主显示器左上角为虚拟屏原点，多屏坐标可为负。
//!
//! 常量是**逻辑像素**（CSS px / DIP）。与 Win32 物理坐标运算前必须经 [`phys`]
//! 按窗口 `scale_factor()` 换算，否则在 DPI 缩放 ≠ 100% 时（如 175%）会错位。
//!
//! v0.2.0 形态变化（相对 v1）：
//! - 窗口不再等于胶囊，而是「胶囊 + 四周透明 halo」的更大透明窗口（`WINDOW_W/H`），
//!   为 hover 放大 / 投影 / 折射取样预留空间；胶囊居中于窗口内（`CAPSULE_MARGIN`）。
//! - 贴边隐藏时只露 `EDGE_REVEAL`（0–4 DIP）玻璃边缘；唤出依赖独立的
//!   `EDGE_ZONE_HIT` 热区（视觉热区 `EDGE_ZONE_VISUAL` 可更窄甚至不可见）。

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HMONITOR, MONITORINFO,
    MONITOR_DEFAULTTOPRIMARY, MONITOR_DEFAULTTONEAREST, MONITOR_FROM_FLAGS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, GetWindowLongPtrW, GetWindowRect, SetWindowDisplayAffinity, SetWindowLongPtrW,
    SetWindowPos, GWL_EXSTYLE, GWL_STYLE, SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_NOACTIVATE,
    SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WINDOW_DISPLAY_AFFINITY, WS_CAPTION, WS_EX_NOACTIVATE,
    WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_SYSMENU, WS_THICKFRAME,
};

// ---------------------------------------------------------------------------
// 尺寸常量（逻辑像素 / DIP）
// ---------------------------------------------------------------------------

/// 胶囊宽度（逻辑像素）。v0.2.0 实测 52 DIP 偏大，收窄到 40 DIP。
pub const WIDGET_W: i32 = 40;
/// 胶囊高度（逻辑像素）。
pub const WIDGET_H: i32 = 200;
/// 胶囊圆角半径（逻辑像素，≈ width/2，完整圆角胶囊）。
pub const WIDGET_RADIUS: i32 = 20;
/// 胶囊四周透明 halo 宽度（逻辑像素）：为 hover 放大 / 投影 / 折射取样留白。
pub const CAPSULE_MARGIN: i32 = 14;
/// 悬浮物**窗口**宽度 = 胶囊 + 两侧 halo。
pub const WINDOW_W: i32 = WIDGET_W + CAPSULE_MARGIN * 2; // 68
/// 悬浮物**窗口**高度。
pub const WINDOW_H: i32 = WIDGET_H + CAPSULE_MARGIN * 2; // 228

/// 贴边隐藏时露出的窄边宽度（逻辑像素；0–4 DIP 的「玻璃边缘提示量」）。
pub const EDGE_REVEAL: i32 = 2;
/// 展开时胶囊距屏边（工作区）的内边距（逻辑像素）。
pub const EDGE_INSET: i32 = 8;
/// 唤出热区宽度（逻辑像素，6–10 DIP，用于 175% 高 DPI 下易触发）。
pub const EDGE_ZONE_HIT: i32 = 8;
/// 视觉热区宽度（逻辑像素，2–6 DIP，可完全不可见）。
pub const EDGE_ZONE_VISUAL: i32 = 3;
/// 松手吸附带宽度：胶囊内缘距屏边 ≤ 该值即贴边（逻辑像素，24–40 DIP）。
pub const SNAP_BAND: i32 = 28;
/// 唤出热区在 Y 方向超出胶囊的上下额外量（逻辑像素，40–80 DIP）。
pub const CORRIDOR_Y: i32 = 48;
/// 唤出意图判定停留时长（毫秒，60–120ms）。
pub const INTENT_DWELL_MS: u64 = 90;
/// 移出有效区后自动缩回的防抖时长（毫秒，800–1500ms，首版 1200）。
pub const RETRACT_DELAY_MS: u64 = 1200;
/// Reveal / Hide 窗口位移动画时长（毫秒，220–320ms）。
pub const DOCK_ANIM_MS: u64 = 260;
/// 光标轮询间隔（毫秒）。
pub const CURSOR_POLL_MS: u64 = 30;
/// 全屏检测轮询间隔（毫秒）。
pub const FULLSCREEN_POLL_MS: u64 = 400;

/// 逻辑像素 → 物理像素：按窗口 DPI 缩放系数 `scale` 四舍五入换算。
///
/// `scale` 取自 `WebviewWindow::scale_factor()`（如 175% 缩放 → `1.75`）。
#[must_use]
pub fn phys(value: i32, scale: f64) -> i32 {
    if !(scale > 0.0) {
        return value;
    }
    ((value as f64) * scale).round() as i32
}

/// 物理像素矩形。字段布局与 `windows::Win32::Foundation::RECT` 相同，独立类型
/// 避免把 Win32 类型泄漏到业务层。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct Rect {
    pub left: i32,
    pub top: i32,
    pub right: i32,
    pub bottom: i32,
}

impl Rect {
    /// 矩形宽度。
    #[must_use]
    pub const fn width(&self) -> i32 {
        self.right - self.left
    }

    /// 矩形高度。
    #[must_use]
    pub const fn height(&self) -> i32 {
        self.bottom - self.top
    }

    /// 是否为空 / 无效（右 ≤ 左 或 下 ≤ 上）。
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.right <= self.left || self.bottom <= self.top
    }
}

/// 当前光标位置（虚拟屏坐标，物理像素）。取不到时返回 `None`。
#[must_use]
pub fn cursor_pos() -> Option<(i32, i32)> {
    let mut pt = POINT::default();
    // SAFETY: pt 指向栈上已初始化的 POINT，API 只会写入。
    unsafe {
        GetCursorPos(&mut pt).ok()?;
    }
    Some((pt.x, pt.y))
}

/// 包含或最接近点 `(x, y)` 的显示器**工作区**（`rcWork`，物理像素）。
#[must_use]
pub fn monitor_work_area(x: i32, y: i32) -> Option<Rect> {
    work_area_of(monitor_at(x, y, MONITOR_DEFAULTTONEAREST))
}

/// 包含或最接近点 `(x, y)` 的显示器**完整矩形**（`rcMonitor`，物理像素，
/// 含任务栏区域）。用于全屏判定与 DPI 归属判断。
#[must_use]
pub fn monitor_rect(x: i32, y: i32) -> Option<Rect> {
    rect_of(monitor_at(x, y, MONITOR_DEFAULTTONEAREST), MonitorField::Monitor)
}

/// 主显示器的工作区（`rcWork`）。
#[must_use]
pub fn primary_work_area() -> Option<Rect> {
    work_area_of(monitor_at(0, 0, MONITOR_DEFAULTTOPRIMARY))
}

/// 窗口当前矩形（虚拟屏坐标，物理像素）。取不到时返回 `None`。
#[must_use]
pub fn window_rect(hwnd: HWND) -> Option<Rect> {
    let mut r = RECT::default();
    // SAFETY: r 指向栈上已初始化的 RECT，API 只会写入。
    unsafe {
        GetWindowRect(hwnd, &mut r).ok()?;
    }
    Some(Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom })
}

/// 窗口所在显示器的完整矩形（`rcMonitor`）。用于全屏判定。
#[must_use]
pub fn window_monitor_rect(hwnd: HWND) -> Option<Rect> {
    // SAFETY: hwnd 由调用方保证有效；返回 HMONITOR 或空句柄，由 rect_of 判空。
    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    rect_of(hmon, MonitorField::Monitor)
}

/// 移动窗口到 `(x, y)`，保持尺寸与 Z 序不变。
///
/// `no_activate` 为真时带 `SWP_NOACTIVATE`：贴边 / 拖拽全程不抢焦点
/// （配合 `WS_EX_NOACTIVATE`，见 [`apply_no_activate`]）。
pub fn set_window_pos(hwnd: HWND, x: i32, y: i32, no_activate: bool) {
    let mut flags: SET_WINDOW_POS_FLAGS = SWP_NOSIZE | SWP_NOZORDER;
    if no_activate {
        flags |= SWP_NOACTIVATE;
    }
    // SAFETY: hwnd 由调用方保证有效；uflags 不含 SWP_SHOWWINDOW 等副作用位。
    unsafe {
        let _ = SetWindowPos(hwnd, None::<&HWND>, x, y, 0, 0, flags);
    }
}

/// 剥掉窗口的「标题栏类」样式，让 Windows 不再对它强制最小宽度。
///
/// # 为什么必须做
///
/// `tauri.conf.json` 里 `decorations: false` 只是把非客户区**画**掉
/// （窗口过程处理 `WM_NCCALCSIZE` 把非客户区收成 0），**样式位并没有清除** ——
/// 实测面板窗口的 `GWL_STYLE` 是 `0x14CB0000`，含
/// `WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX | WS_MAXIMIZEBOX`。
///
/// 而 Windows **对带标题按钮的窗口强制一个最小宽度**（要容下图标 + 三个按钮，
/// 100% 缩放下约 136 DIP）。于是请求的 68 DIP 宽被系统撑到 ≈136 DIP：
/// 175% 缩放下实测 236 物理像素，而按配置应当是 119。
///
/// 连锁后果（真机实测）：
/// - 帧尺寸从 98KB 涨到 376KB（宽了一倍）；
/// - 前端用 `frame_w / 68` 反推缩放系数，得 3.47 而非 1.75 →
///   裁剪原点从 24.5px 变成 48.6px、取样区 70×350 变成 139×694
///   → 玻璃显示的是**错位且大一倍**的背景。
///
/// 清掉这些样式位后最小宽度限制消失，窗口才能真的做成 68 DIP 宽。
/// 调用后需要 `SWP_FRAMECHANGED` 让样式变更立即生效。
pub fn apply_borderless(hwnd: HWND) {
    // SAFETY: GWL_STYLE 读写同一窗口，无数据竞争；SetWindowPos 仅触发布局重算。
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let strip = WS_CAPTION.0
            | WS_THICKFRAME.0
            | WS_MINIMIZEBOX.0
            | WS_MAXIMIZEBOX.0
            | WS_SYSMENU.0;
        let new_style = style & !(strip as isize);
        let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
        let _ = SetWindowPos(
            hwnd,
            None::<&HWND>,
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 按 DIP 尺寸重设窗口大小（物理尺寸 = DIP × 缩放）。
///
/// 用途：`apply_borderless` 之后把窗口恢复成配置尺寸 —— 此前它在创建时就被
/// Windows 撑到了最小标题栏宽度，不主动改回去就一直是错的。
///
/// `scale` 由调用方传入（Tauri 的 `scale_factor()`），避免本模块再引入
/// `Win32_UI_HiDpi` 特征与一次额外查询。
pub fn set_window_size_dip(hwnd: HWND, w_dip: i32, h_dip: i32, scale: f64) {
    let w = phys(w_dip, scale);
    let h = phys(h_dip, scale);
    // SAFETY: 只改尺寸，不动位置与 Z 序。
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None::<&HWND>,
            0,
            0,
            w,
            h,
            SWP_NOMOVE | SWP_NOZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 给窗口叠加 `WS_EX_NOACTIVATE`（点击不激活、不抢焦点）。
///
/// `skipTaskbar` 已隐含 `WS_EX_TOOLWINDOW`；此处**刻意不设 `WS_EX_APPWINDOW`**。
pub fn apply_no_activate(hwnd: HWND) {
    // SAFETY: hwnd 由调用方保证有效；GWL_EXSTYLE 读取/写回同属一窗，无数据竞争。
    unsafe {
        let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let new_style = style | (WS_EX_NOACTIVATE.0 as isize);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, new_style);
    }
}

/// `WDA_EXCLUDEFROMCAPTURE`（Windows 10 2004+，值 0x11）。
const WDA_EXCLUDEFROMCAPTURE: WINDOW_DISPLAY_AFFINITY = WINDOW_DISPLAY_AFFINITY(0x11);

/// 把窗口排除在桌面捕获之外（Desktop Duplication / Windows Graphics Capture /
/// PrintWindow 等都会跳过它）。
///
/// 关键用途：悬浮物窗口位于透明 halo 内，抓取窗口后方桌面时若把自身窗口也
/// 抓进来会产生「玻璃套玻璃」的反馈回路。调用本函数后，捕获结果中该窗口区域
/// 显示的是其**下方**的桌面内容，从而得到干净的背景采样。
///
/// # ⚠️ 副作用：本窗口将对一切截屏手段隐身
///
/// `WDA_EXCLUDEFROMCAPTURE` 不是"只对 DDA 隐藏"，而是让该窗口从**所有**捕获
/// 路径中消失 —— Desktop Duplication、Windows Graphics Capture、`PrintWindow`、
/// GDI `BitBlt`（mss / 各种截图库走的就是这条）、乃至系统截图工具，全部拍不到它。
///
/// 后果：**UI 外观无法通过截图验收**。症状很有迷惑性 —— `EnumWindows` /
/// `tasklist` 都能看到窗口，位置尺寸也正确，但截出来的图里那块区域是纯桌面，
/// 看起来像"没渲染"。排查时不要往渲染方向找，先想起这条。
///
/// 验收时需要目视外观的话，用 `HSDR_NO_CAPTURE_EXCLUDE=1` 启动（见 `main.rs`）。
pub fn apply_exclude_from_capture(hwnd: HWND) -> bool {
    // SAFETY: hwnd 由调用方保证有效且为顶层窗口；API 失败返回 Err。
    unsafe { SetWindowDisplayAffinity(hwnd, WDA_EXCLUDEFROMCAPTURE).is_ok() }
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

/// `GetMonitorInfoW` 要读取的字段。
enum MonitorField {
    Work,
    Monitor,
}

/// 取包含或最接近指定点的显示器句柄。
fn monitor_at(x: i32, y: i32, flags: MONITOR_FROM_FLAGS) -> HMONITOR {
    let pt = POINT { x, y };
    // SAFETY: pt 为值参数；返回 HMONITOR 或空句柄，由读取函数判空。
    unsafe { MonitorFromPoint(pt, flags) }
}

/// 读指定显示器句柄的 `rcWork`。句柄为空或调用失败时返回 `None`。
fn work_area_of(hmon: HMONITOR) -> Option<Rect> {
    rect_of(hmon, MonitorField::Work)
}

/// 读指定显示器句柄的 `rcWork` 或 `rcMonitor`。
fn rect_of(hmon: HMONITOR, field: MonitorField) -> Option<Rect> {
    if hmon.0.is_null() {
        return None;
    }
    let mut info = MONITORINFO::default();
    // `cbSize` 必须等于结构体尺寸，否则 API 返回失败。
    info.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
    // SAFETY: info 指向栈上已初始化的 MONITORINFO 且 cbSize 正确，API 只会写入。
    let ok = unsafe { GetMonitorInfoW(hmon, &mut info) };
    if !ok.as_bool() {
        return None;
    }
    let r = match field {
        MonitorField::Work => info.rcWork,
        MonitorField::Monitor => info.rcMonitor,
    };
    Some(Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 常量符合02版规格() {
        assert_eq!(WIDGET_W, 40);
        assert_eq!(WIDGET_H, 200);
        assert_eq!(WIDGET_RADIUS, 20);
        assert_eq!(CAPSULE_MARGIN, 14);
        assert_eq!(WINDOW_W, 68);
        assert_eq!(WINDOW_H, 228);
        assert_eq!(EDGE_REVEAL, 2);
        assert_eq!(EDGE_INSET, 8);
        assert_eq!(EDGE_ZONE_HIT, 8);
        assert_eq!(EDGE_ZONE_VISUAL, 3);
        assert_eq!(SNAP_BAND, 28);
        assert_eq!(CORRIDOR_Y, 48);
        assert_eq!(INTENT_DWELL_MS, 90);
        assert_eq!(RETRACT_DELAY_MS, 1200);
        assert_eq!(DOCK_ANIM_MS, 260);
    }

    #[test]
    fn 逻辑像素按175缩放换算() {
        let scale = 1.75;
        assert_eq!(phys(EDGE_INSET, scale), 14); // 8 × 1.75
        assert_eq!(phys(EDGE_REVEAL, scale), 4); // 2 × 1.75 = 3.5 → 4
        assert_eq!(phys(EDGE_ZONE_HIT, scale), 14); // 8 × 1.75
        assert_eq!(phys(SNAP_BAND, scale), 49); // 28 × 1.75
        assert_eq!(phys(WIDGET_W, scale), 70); // 40 × 1.75
        assert_eq!(phys(WIDGET_H, scale), 350); // 200 × 1.75
        assert_eq!(phys(WINDOW_W, scale), 119); // 68 × 1.75
        assert_eq!(phys(WINDOW_H, scale), 399); // 228 × 1.75
        // 缩放 1.0 不变化。
        assert_eq!(phys(EDGE_INSET, 1.0), 8);
        assert_eq!(phys(WIDGET_W, 1.0), 40);
        // 非法缩放回退原值。
        assert_eq!(phys(8, 0.0), 8);
        assert_eq!(phys(8, f64::NAN), 8);
    }

    #[test]
    fn 矩形宽高与空判定() {
        let r = Rect { left: 10, top: 20, right: 60, bottom: 80 };
        assert_eq!(r.width(), 50);
        assert_eq!(r.height(), 60);
        assert!(!r.is_empty());
        assert!(Rect { left: 5, top: 5, right: 5, bottom: 9 }.is_empty());
    }
}
