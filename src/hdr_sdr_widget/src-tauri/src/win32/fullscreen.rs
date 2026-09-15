//! 前台窗口全屏判定：区分「真全屏 / Borderless 全屏」与「普通最大化」。
//!
//! 硬性规则（spec §29/§30/§31）：只有真正覆盖整个显示器、符合 Fullscreen 判断
//! 逻辑的窗口才需要抑制悬浮物；普通最大化窗口（Chrome / Explorer）**不是**
//! 全屏，Edge Reveal 仍应工作。
//!
//! 判定策略（纯 Win32，无 DWM 依赖）：
//! 1. `GetForegroundWindow` 为空 → 桌面/安全桌面，非全屏。
//! 2. 前台窗口是自己的悬浮物窗口（传入 `own` 句柄）→ 非全屏。
//! 3. 窗口类名为桌面 / 任务栏 / 菜单等系统类 → 非全屏。
//! 4. 窗口带 `WS_MAXIMIZE` → 非全屏（最大化 ≠ 全屏，即使覆盖整个显示器）。
//! 5. 窗口矩形覆盖其所在显示器的 `rcMonitor`（容差 2 物理像素）→ 真全屏。

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{
    GetClassNameW, GetForegroundWindow, GetWindowLongPtrW, IsIconic, IsWindowVisible, GWL_STYLE,
    WS_MAXIMIZE, WS_MINIMIZE,
};

use super::geometry::{self, Rect};

/// 前台窗口是否处于真全屏（应抑制悬浮物）。
///
/// `own` 为悬浮物窗口句柄，用于排除自己（可选）。
pub fn foreground_fullscreen(own: Option<HWND>) -> bool {
    // SAFETY: GetForegroundWindow 无参，返回 HWND 或空句柄。
    let fg = unsafe { GetForegroundWindow() };
    if fg.0.is_null() {
        return false;
    }
    if let Some(own) = own {
        if own == fg {
            return false;
        }
    }

    // 验收逃生门：`HSDR_IGNORE_FULLSCREEN=1` 时一律不判全屏。
    //
    // 用途：抑制态下悬浮物既不显示、采集区域也是 0，于是"玻璃渲染链路到底通不通"
    // 在**用户正开着全屏应用**时无法验证（系统不允许别的进程抢前台，确实抢不了）。
    // 有了这个开关，可以在不动用户游戏的前提下把链路验到底。
    // 日常使用不要开。
    if std::env::var_os("HSDR_IGNORE_FULLSCREEN").is_some() {
        return false;
    }

    // 系统类窗口（桌面 / 任务栏 / 菜单 / 输入法）永不视为全屏应用。
    if is_system_class(fg) {
        return false;
    }
    // 最小化 / 不可见窗口不是全屏前台。
    // SAFETY: IsIconic / IsWindowVisible 只读，hwnd 有效。
    if unsafe { IsIconic(fg).as_bool() || !IsWindowVisible(fg).as_bool() } {
        return false;
    }
    // 普通最大化窗口不是全屏。
    // SAFETY: GWL_STYLE 只读。
    let style = unsafe { GetWindowLongPtrW(fg, GWL_STYLE) } as u32;
    if style & (WS_MAXIMIZE.0 | WS_MINIMIZE.0) != 0 {
        return false;
    }

    // 窗口矩形覆盖所在显示器完整矩形（rcMonitor，含任务栏区）。
    let Some(win_rect) = geometry::window_rect(fg) else {
        return false;
    };
    let Some(mon) = geometry::window_monitor_rect(fg) else {
        return false;
    };
    covers(win_rect, mon)
}

/// 矩形 `r` 是否以 ≤2 物理像素容差覆盖 `target`。
fn covers(r: Rect, target: Rect) -> bool {
    const TOL: i32 = 2;
    r.left <= target.left + TOL
        && r.top <= target.top + TOL
        && r.right >= target.right - TOL
        && r.bottom >= target.bottom - TOL
}

/// 窗口类名是否为系统桌面 / 任务栏 / 菜单类（这些窗口覆盖整个显示器但不是应用）。
fn is_system_class(hwnd: HWND) -> bool {
    let mut buf = [0u16; 64];
    // SAFETY: buf 指向栈上 64×u16 数组，API 写入至多 63 字符 + NUL。
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len == 0 {
        return true; // 拿不到类名视为非应用窗口，保守不抑制。
    }
    let name: String = String::from_utf16_lossy(&buf[..len as usize]);
    matches!(
        name.as_str(),
        "Progman" | "WorkerW" | "Shell_TrayWnd" | "Shell_SecondaryTrayWnd"
            | "Windows.UI.Core.CoreWindow" | "Windows.UI.Composition.Device" | "ApplicationFrameWindow"
    )
}

/// 供测试：直接暴露 `covers` 与系统类判定无需 Win32 句柄。
#[cfg(test)]
mod tests {
    use super::*;

    fn r(l: i32, t: i32, rt: i32, b: i32) -> Rect {
        Rect { left: l, top: t, right: rt, bottom: b }
    }

    #[test]
    fn 覆盖判定容差() {
        // 精确覆盖 → true。
        assert!(covers(r(0, 0, 3840, 2160), r(0, 0, 3840, 2160)));
        // 1px 容差内 → true。
        assert!(covers(r(1, 1, 3839, 2159), r(0, 0, 3840, 2160)));
        // 明显未覆盖（最大化只覆盖工作区，缺任务栏 40px）→ false。
        assert!(!covers(r(0, 0, 3840, 2120), r(0, 0, 3840, 2160)));
        // 非全屏普通窗口 → false。
        assert!(!covers(r(100, 100, 1800, 1000), r(0, 0, 3840, 2160)));
    }
}
