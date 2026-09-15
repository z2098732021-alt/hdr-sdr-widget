//! cubic-bezier 采样表 + `SetWindowPos` 帧动画（展开 / 收缩 / 吸附）。
//!
//! 对应 ARCHITECTURE.md §2E / S3.3。两条曲线参数与前端 CSS token
//! （`--spring-pop` / `--spring-settle`）**逐字一致**（§8.4），保证窗口位移
//! 与内容缩放的观感统一。动画用 64 点采样表 + 16ms tick（rAF 等价）。
//!
//! 所有窗口移动都带 `SWP_NOACTIVATE`，全程不抢焦点。

use std::time::{Duration, Instant};

use once_cell::sync::Lazy;
use windows::Win32::Foundation::HWND;

use hdr_sdr_widget_lib::win32::geometry;

/// CSS `--spring-pop`：弹出 / 显隐 / 数值滚动末尾（轻微 overshoot）。
const SPRING_POP: (f64, f64, f64, f64) = (0.175, 0.885, 0.32, 1.275);
/// CSS `--spring-settle`：贴边收缩 / 吸附 / 磁吸（无 overshoot）。
const SPRING_SETTLE: (f64, f64, f64, f64) = (0.32, 0.72, 0.24, 1.0);

/// 采样表点数（§2E：64 点）。
const TABLE_SIZE: usize = 64;
/// 帧间隔（8ms ≈ 120fps，窗口位移更顺滑，消除 16ms 的卡顿感）。
const TICK_MS: u64 = 8;

/// `--spring-pop` 曲线（全局惰性构造一次）。
pub static POP: Lazy<Bezier> = Lazy::new(|| Bezier::sample(SPRING_POP));
/// `--spring-settle` 曲线。
pub static SETTLE: Lazy<Bezier> = Lazy::new(|| Bezier::sample(SPRING_SETTLE));

/// 一条 cubic-bezier 缓动曲线：64 点采样表 + 线性插值查询。
pub struct Bezier {
    table: [f64; TABLE_SIZE],
}

impl Bezier {
    /// 用控制点采样生成 64 点查找表。
    fn sample((x1, y1, x2, y2): (f64, f64, f64, f64)) -> Self {
        let mut table = [0.0; TABLE_SIZE];
        for (i, slot) in table.iter_mut().enumerate() {
            let x = i as f64 / (TABLE_SIZE - 1) as f64;
            *slot = y_at_x(x1, y1, x2, y2, x);
        }
        Self { table }
    }

    /// 查询时间进度 `t ∈ [0, 1]` 对应的缓动进度（相邻采样点线性插值）。
    #[must_use]
    pub fn ease(&self, t: f64) -> f64 {
        let t = t.clamp(0.0, 1.0);
        let pos = t * (TABLE_SIZE - 1) as f64;
        let idx = pos.floor() as usize;
        let frac = pos - idx as f64;
        let a = self.table[idx];
        let b = self.table[(idx + 1).min(TABLE_SIZE - 1)];
        a + (b - a) * frac
    }
}

/// 把窗口从当前位置帧动画移动到 `(target_x, target_y)`。
///
/// `cancel` 每帧检查一次，返回 `true` 立即中止（用户拖拽 / 状态被抢占时）。
/// 动画期间全程 `SWP_NOACTIVATE`。
pub fn animate_to(
    hwnd: HWND,
    target_x: i32,
    target_y: i32,
    curve: &Bezier,
    ms: u64,
    cancel: &dyn Fn() -> bool,
) {
    let start_rect = match geometry::window_rect(hwnd) {
        Some(r) => r,
        None => {
            // 拿不到当前位置：直接落位，跳过动画。
            geometry::set_window_pos(hwnd, target_x, target_y, true);
            return;
        }
    };
    let start_x = start_rect.left;
    let start_y = start_rect.top;
    let duration = Duration::from_millis(ms.max(1));
    let start = Instant::now();

    loop {
        if cancel() {
            return;
        }
        let t = start.elapsed().as_secs_f64() / duration.as_secs_f64();
        if t >= 1.0 {
            geometry::set_window_pos(hwnd, target_x, target_y, true);
            return;
        }
        let eased = curve.ease(t);
        let x = start_x + ((target_x - start_x) as f64 * eased).round() as i32;
        let y = start_y + ((target_y - start_y) as f64 * eased).round() as i32;
        geometry::set_window_pos(hwnd, x, y, true);
        std::thread::sleep(Duration::from_millis(TICK_MS));
    }
}

// ---------------------------------------------------------------------------
// cubic-bezier 数学（与 CSS 定义一致：P0=(0,0)、P3=(1,1)，P1/P2 由控制点给出）
// ---------------------------------------------------------------------------

/// Bezier 曲线 X(t)。
fn bezier_x(x1: f64, x2: f64, t: f64) -> f64 {
    let mt = 1.0 - t;
    let mt2 = mt * mt;
    let t2 = t * t;
    let t3 = t2 * t;
    3.0 * mt2 * t * x1 + 3.0 * mt * t2 * x2 + t3
}

/// Bezier 曲线 Y(t)。
fn bezier_y(y1: f64, y2: f64, t: f64) -> f64 {
    bezier_x(y1, y2, t)
}

/// 对目标 X 值二分求参数 t，再返回 Y(t)。
fn y_at_x(x1: f64, y1: f64, x2: f64, y2: f64, x: f64) -> f64 {
    if x <= 0.0 {
        return 0.0;
    }
    if x >= 1.0 {
        return 1.0;
    }
    let (mut lo, mut hi) = (0.0f64, 1.0f64);
    for _ in 0..24 {
        let mid = (lo + hi) * 0.5;
        if bezier_x(x1, x2, mid) < x {
            lo = mid;
        } else {
            hi = mid;
        }
    }
    bezier_y(y1, y2, (lo + hi) * 0.5)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 曲线端点正确() {
        for curve in [&*POP, &*SETTLE] {
            assert!((curve.ease(0.0) - 0.0).abs() < 1e-9);
            assert!((curve.ease(1.0) - 1.0).abs() < 1e-9);
        }
    }

    #[test]
    fn 曲线特征_pop过冲_settle缓出() {
        // pop 曲线中段应明显 > 1（overshoot：先冲过头再回落）。
        assert!(POP.ease(0.5) > 1.0, "pop 中段应过冲超过 1，实际 {}", POP.ease(0.5));
        // settle 是缓出曲线：前半段已推进大部分进度，但严格 < 1。
        let settle_mid = SETTLE.ease(0.5);
        assert!(settle_mid > 0.6 && settle_mid < 1.0, "settle 中段应在 (0.6, 1)，实际 {settle_mid}");
    }

    #[test]
    fn 采样表_settle单调_pop含过冲点() {
        let table = &SETTLE.table;
        for w in table.windows(2) {
            assert!(w[1] >= w[0] - 1e-12, "settle 采样表必须单调");
        }
        // pop 有过冲：采样表内存在 > 1 的采样点（先超出再回落，故整表非单调）。
        assert!(
            POP.table.iter().any(|&y| y > 1.0),
            "pop 采样表应包含过冲采样点"
        );
    }
}
