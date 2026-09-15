//! 悬浮物窗口的部分点击穿透：透明 halo 区域点击穿透到下层应用。
//!
//! 悬浮物窗口比胶囊大（四周透明 halo，供 hover 放大 / 投影 / 折射取样）。
//! 若不处理，halo 会吞掉胶囊周边约 14 DIP 范围内的点击。本模块替换窗口过程，
//! 在 `WM_NCHITTEST` 上：命中胶囊矩形 → `HTCLIENT`（正常交给 WebView），
//! 否则 → `HTTRANSPARENT`（系统视为窗口不存在，点击落到下层应用）。
//!
//! 单悬浮物窗口，故命中矩形与原过程用进程内全局变量保存，避免在窗口过程里加锁。

use std::sync::atomic::{AtomicI32, AtomicIsize, Ordering};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::ScreenToClient;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, SetWindowLongPtrW, GWLP_WNDPROC, HTCLIENT, HTTRANSPARENT,
    WM_NCHITTEST, WNDPROC,
};

static PREV_PROC: AtomicIsize = AtomicIsize::new(0);
static RECT_L: AtomicI32 = AtomicI32::new(0);
static RECT_T: AtomicI32 = AtomicI32::new(0);
static RECT_R: AtomicI32 = AtomicI32::new(0);
static RECT_B: AtomicI32 = AtomicI32::new(0);

/// 安装点击穿透子类化。失败（拿不到原过程）返回 `false`。
pub fn install(hwnd: HWND) -> bool {
    // SAFETY: hwnd 有效；GWLP_WNDPROC 读旧过程并写入新过程。
    unsafe {
        let prev = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, hit_test_proc as *const () as usize as isize);
        if prev == 0 {
            return false;
        }
        PREV_PROC.store(prev, Ordering::Relaxed);
    }
    true
}

/// 设置胶囊命中矩形（窗口客户坐标，物理像素，含少量缩放余量）。
/// `None` = 全窗口点击穿透（隐藏 / 抑制态）。
pub fn set_rect(rect: Option<[i32; 4]>) {
    match rect {
        Some([l, t, r, b]) => {
            RECT_L.store(l, Ordering::Relaxed);
            RECT_T.store(t, Ordering::Relaxed);
            RECT_R.store(r, Ordering::Relaxed);
            RECT_B.store(b, Ordering::Relaxed);
        }
        None => {
            RECT_R.store(0, Ordering::Relaxed);
            RECT_B.store(0, Ordering::Relaxed);
        }
    }
}

/// 替换后的窗口过程：只在 `WM_NCHITTEST` 做穿透判定，其余全部链回原过程。
unsafe extern "system" fn hit_test_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        let (rl, rt, rr, rb) = (
            RECT_L.load(Ordering::Relaxed),
            RECT_T.load(Ordering::Relaxed),
            RECT_R.load(Ordering::Relaxed),
            RECT_B.load(Ordering::Relaxed),
        );
        if rr > rl && rb > rt {
            // WM_NCHITTEST 的 lParam 是屏幕坐标（低 16 位 x，高 16 位 y，有符号）。
            let sx = (l.0 & 0xffff) as i16 as i32;
            let sy = ((l.0 >> 16) & 0xffff) as i16 as i32;
            let mut pt = POINT { x: sx, y: sy };
            let _ = ScreenToClient(hwnd, &mut pt);
            if pt.x >= rl && pt.x <= rr && pt.y >= rt && pt.y <= rb {
                return LRESULT(HTCLIENT as isize);
            }
        }
        return LRESULT(HTTRANSPARENT as isize);
    }

    let prev = PREV_PROC.load(Ordering::Relaxed);
    if prev != 0 {
        // SAFETY: prev 是安装时保存的原过程指针。
        let prev_fn: WNDPROC = std::mem::transmute(prev);
        return CallWindowProcW(prev_fn, hwnd, msg, w, l);
    }
    DefWindowProcW(hwnd, msg, w, l)
}
