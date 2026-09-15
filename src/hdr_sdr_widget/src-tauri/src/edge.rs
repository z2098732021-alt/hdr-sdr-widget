//! 悬浮物状态机：光标轮询 + 边缘唤出 / 意图判定 / 自动收回 / 全屏抑制。
//!
//! 对应 spec §19–§31 / §40–§41。状态机**唯一真源**在本模块持有的
//! `AppState.widget`（`Mutex<WidgetState>`），前端 `data-*` 仅作渲染镜像。
//!
//! 状态（按优先级从高到低）：
//! `Suppressed`（真全屏）> `Dragging` > `Pressed` > `Hovered` > `Visible`
//! > `Hiding` > `Hidden`；`Revealing` 是 `Hidden→Visible` 的过渡态。
//!
//! 采用 `GetCursorPos` 轮询（30ms）+ `GetForegroundWindow` 全屏轮询（400ms），
//! 而非低级别钩子：稳定、无杀软误报，且与现有 `WS_EX_NOACTIVATE` 不抢焦点策略一致。
//! 轮询期间窗口位移动画由 [`crate::anim::animate_to`] 同步驱动（阻塞约 260ms）。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::HWND;

use hdr_sdr_widget_lib::win32::capture::CaptureShared;
use hdr_sdr_widget_lib::win32::fullscreen;
use hdr_sdr_widget_lib::win32::geometry::{self, Rect};

use crate::commands::AppState;
use crate::window::{self, WidgetMetrics};

/// 贴边方向。`None`（自由悬浮）由 [`WidgetState::docked`] 表达。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Edge {
    Left,
    Right,
}

/// 悬浮物运行状态（spec §40 状态机；camelCase JSON）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Hidden,
    Revealing,
    Visible,
    Hovered,
    Pressed,
    Dragging,
    Hiding,
    Suppressed,
}

/// 悬浮物运行时状态（命令 / 事件 / 前端契约共用）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WidgetState {
    /// 贴边方向；`null` = 自由悬浮（完整显示）。
    pub docked: Option<Edge>,
    /// 窗口是否处于展开（屏内）位置。
    pub expanded: bool,
    /// 状态机相位。
    pub phase: Phase,
}

impl Default for WidgetState {
    fn default() -> Self {
        Self { docked: None, expanded: true, phase: Phase::Visible }
    }
}

/// 拖拽过程中的边缘接近提示（驱动前端吸附引导线）。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DragHint {
    pub near_edge: Option<Edge>,
}

/// 贴边轮询线程句柄。
pub struct EdgeHandle {
    thread: Option<JoinHandle<()>>,
    stop: Arc<AtomicBool>,
}

impl EdgeHandle {
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for EdgeHandle {
    fn drop(&mut self) {
        self.stop();
    }
}

/// 单次轮询维护的计时器。
#[derive(Default)]
struct Timers {
    /// 移出有效区后自动收回的计时起点。
    retract_start: Option<Instant>,
    /// 边缘唤出的意图停留计时起点。
    intent_start: Option<Instant>,
    /// 上次全屏判定时间。
    last_fullscreen: Option<Instant>,
}

/// 启动状态机轮询线程。`own_hwnd` 为悬浮物窗口句柄的原始值（`isize`，Send 安全）。
pub fn start(app: AppHandle, capture: Arc<CaptureShared>, own_hwnd: isize) -> EdgeHandle {
    let stop = Arc::new(AtomicBool::new(false));
    let stop_flag = Arc::clone(&stop);
    let thread = std::thread::Builder::new()
        .name("edge-watch".to_string())
        .spawn(move || {
            let own = if own_hwnd != 0 { Some(HWND(own_hwnd as *mut _)) } else { None };
            let mut timers = Timers::default();
            while !stop_flag.load(Ordering::Relaxed) {
                timers = poll_once(&app, &capture, own, timers);
                std::thread::sleep(Duration::from_millis(geometry::CURSOR_POLL_MS));
            }
        })
        .expect("无法创建贴边轮询线程");
    EdgeHandle { thread: Some(thread), stop }
}

/// 单次轮询：全屏判定 + 状态机迁移 + 捕获配置刷新。返回更新后的计时器。
fn poll_once(
    app: &AppHandle,
    capture: &Arc<CaptureShared>,
    own_hwnd: Option<HWND>,
    mut t: Timers,
) -> Timers {
    let state = app.state::<AppState>();
    let current = window::widget_state(&state);
    let Ok(window) = window::panel(app) else {
        return t;
    };
    let m = WidgetMetrics::from_window(&window);

    // ---- 全屏判定（400ms 节流）----
    let should_check = t
        .last_fullscreen
        .map(|i| i.elapsed() >= Duration::from_millis(geometry::FULLSCREEN_POLL_MS))
        .unwrap_or(true);
    if should_check {
        t.last_fullscreen = Some(Instant::now());
        let fs = fullscreen::foreground_fullscreen(own_hwnd);
        if fs && current.phase != Phase::Suppressed {
            enter_suppressed(app, &state, &window, current);
            update_capture(capture, &state, &window);
            return t;
        }
        if !fs && current.phase == Phase::Suppressed {
            // 全屏退出 → 保持 Hidden，等待下次边缘唤出（spec §31）。
            window::set_widget_state(
                &state,
                WidgetState { phase: Phase::Hidden, ..current },
            );
            window::emit_state(app, &state);
        }
    }
    let current = window::widget_state(&state);
    if current.phase == Phase::Suppressed {
        return t;
    }

    // 自由悬浮：不监听边缘唤出 / 自动隐藏。
    let Some(edge) = current.docked else {
        update_capture(capture, &state, &window);
        return t;
    };

    let Some((cx, cy)) = geometry::cursor_pos() else {
        return t;
    };
    let Ok(pos) = window.outer_position() else {
        return t;
    };
    // 当前显示器工作区（按窗口中心定位）。
    let area = geometry::monitor_work_area(pos.x + m.win_w / 2, pos.y + m.win_h / 2)
        .or_else(geometry::primary_work_area)
        .unwrap_or_default();

    match current.phase {
        Phase::Hidden => {
            // 边缘唤出：命中热区（Y 走廊内）+ 意图停留判定（spec §22/§23/§28）。
            if in_edge_zone(cx, cy, &area, edge, pos.y, &m) {
                match t.intent_start {
                    None => t.intent_start = Some(Instant::now()),
                    Some(s) if s.elapsed() >= Duration::from_millis(geometry::INTENT_DWELL_MS) => {
                        t.intent_start = None;
                        reveal(app, &state, &window, edge, &area, pos.y, &m);
                    }
                    Some(_) => {}
                }
            } else {
                t.intent_start = None;
            }
        }
        Phase::Revealing | Phase::Hiding => {
            // 过渡态由 animate_to 同步完成；此处不动作。
        }
        Phase::Visible | Phase::Hovered | Phase::Pressed | Phase::Dragging => {
            // Hover 不阻断自动收回（光标在胶囊上时 in_valid_zone 已覆盖）；
            // 只有 Pressed/Dragging（指针捕获中光标可能离开有效区）才需强制保持。
            let interacting = matches!(current.phase, Phase::Pressed | Phase::Dragging);
            if interacting || in_valid_zone(cx, cy, &area, edge, pos.y, &m) {
                t.retract_start = None;
            } else {
                match t.retract_start {
                    None => t.retract_start = Some(Instant::now()),
                    Some(s)
                        if s.elapsed() >= Duration::from_millis(geometry::RETRACT_DELAY_MS) =>
                    {
                        t.retract_start = None;
                        collapse(app, &state, &window, edge, &area, pos.y, &m);
                    }
                    Some(_) => {}
                }
            }
        }
        Phase::Suppressed => {}
    }

    update_capture(capture, &state, &window);
    t
}

/// 进入抑制态：状态 → 滑出屏幕 → 命中矩形全穿透。
fn enter_suppressed(
    app: &AppHandle,
    state: &AppState,
    window: &tauri::Window,
    current: WidgetState,
) {
    window::set_widget_state(&state, WidgetState { phase: Phase::Suppressed, ..current });
    window::emit_state(app, state);
    window::apply_hit_rect(None);
    // 已展开则滑出屏幕（抑制态不抢焦点，直接用最短路径落位，无动画避免拖尾）。
    if let Some(edge) = current.docked {
        if current.expanded {
            if let Some(hwnd) = window::widget_hwnd(window) {
                let m = WidgetMetrics::from_window(window);
                if let Ok(pos) = window.outer_position() {
                    let area = geometry::monitor_work_area(pos.x, pos.y)
                        .or_else(geometry::primary_work_area)
                        .unwrap_or_default();
                    let (tx, ty) = window::dock_hidden_pos(&area, edge, pos.y, &m);
                    geometry::set_window_pos(hwnd, tx, ty, true);
                }
            }
        }
    }
    window::set_widget_state(
        state,
        WidgetState { phase: Phase::Suppressed, expanded: false, ..current },
    );
}

/// Reveal：状态 → 动画滑入 → Visible。
fn reveal(
    app: &AppHandle,
    state: &AppState,
    window: &tauri::Window,
    edge: Edge,
    area: &Rect,
    y: i32,
    m: &WidgetMetrics,
) {
    let target = WidgetState { docked: Some(edge), phase: Phase::Revealing, expanded: false };
    window::set_widget_state(state, target);
    window::emit_state(app, state);
    window::apply_hit_rect(Some(window::hit_rect(m)));
    let Some(hwnd) = window::widget_hwnd(window) else { return };
    let (tx, ty) = window::dock_expanded_pos(area, edge, y, m);
    let cancel = || {
        let cur = window::widget_state(state);
        cur.phase == Phase::Suppressed || cur.phase == Phase::Hidden
    };
    crate::anim::animate_to(hwnd, tx, ty, &crate::anim::POP, geometry::DOCK_ANIM_MS, &cancel);
    if window::widget_state(state).phase != Phase::Revealing {
        return; // 动画被抢占。
    }
    window::set_widget_state(
        state,
        WidgetState { phase: Phase::Visible, expanded: true, ..target },
    );
    window::emit_state(app, state);
    let _ = app.emit("widget:shown", ());
}

/// Collapse：状态 → 动画滑出 → Hidden。
fn collapse(
    app: &AppHandle,
    state: &AppState,
    window: &tauri::Window,
    edge: Edge,
    area: &Rect,
    y: i32,
    m: &WidgetMetrics,
) {
    let target = WidgetState { docked: Some(edge), phase: Phase::Hiding, expanded: true };
    window::set_widget_state(state, target);
    window::emit_state(app, state);
    let Some(hwnd) = window::widget_hwnd(window) else { return };
    let (tx, ty) = window::dock_hidden_pos(area, edge, y, m);
    let cancel = || {
        let cur = window::widget_state(state);
        cur.phase == Phase::Suppressed || cur.phase == Phase::Revealing
    };
    crate::anim::animate_to(hwnd, tx, ty, &crate::anim::SETTLE, geometry::DOCK_ANIM_MS, &cancel);
    if window::widget_state(state).phase != Phase::Hiding {
        return;
    }
    window::set_widget_state(
        state,
        WidgetState { phase: Phase::Hidden, expanded: false, ..target },
    );
    // 隐藏后命中矩形全穿透：露出的 2 DIP 提示条不吞点击（唤出走光标热区，不依赖点击）。
    window::apply_hit_rect(None);
    window::emit_state(app, state);
}

/// 刷新背景捕获配置：展开且非抑制 → 抓取**胶囊区域**；否则暂停。
///
/// 只抓胶囊区域而不是整个窗口：折射采样点全部落在胶囊内（向中心偏移），
/// 窗口四周的 halo 永远用不到。窗口 119×399 里真正需要的只有胶囊 70×350，
/// 单帧数据量因此砍掉约一半（190KB → 98KB），这是降延迟的第一刀。
fn update_capture(capture: &Arc<CaptureShared>, state: &AppState, window: &tauri::Window) {
    let current = window::widget_state(state);
    let Ok(rect) = window_rect_phys(window) else {
        return;
    };
    let m = WidgetMetrics::from_window(window);
    // 胶囊区域（虚拟屏物理坐标）= 窗口原点 + halo 边距。
    let cap = Rect {
        left: rect.left + m.margin,
        top: rect.top + m.margin,
        right: rect.left + m.margin + m.cap_w,
        bottom: rect.top + m.margin + m.cap_h,
    };
    let region = if (current.expanded || current.phase == Phase::Revealing)
        && current.phase != Phase::Suppressed
    {
        Some(cap)
    } else {
        None
    };
    let interacting = matches!(current.phase, Phase::Hovered | Phase::Pressed | Phase::Dragging);
    if let Ok(mut cfg) = capture.config.lock() {
        *cfg = hdr_sdr_widget_lib::win32::capture::CaptureConfig {
            region,
            interval_ms: if interacting { 16 } else { 100 },
        };
    }
}

/// 窗口物理矩形（虚拟屏坐标）。
fn window_rect_phys(window: &tauri::Window) -> Result<Rect, ()> {
    let pos = window.outer_position().map_err(|_| ())?;
    let size = window.outer_size().map_err(|_| ())?;
    Ok(Rect {
        left: pos.x,
        top: pos.y,
        right: pos.x + size.width as i32,
        bottom: pos.y + size.height as i32,
    })
}

/// 光标是否位于边缘唤出热区（沿屏边 + 胶囊 Y 走廊，spec §22/§23）。
///
/// `win_top` 为窗口 top（物理像素）；胶囊 top = `win_top + margin`。
fn in_edge_zone(
    cx: i32,
    cy: i32,
    area: &Rect,
    edge: Edge,
    win_top: i32,
    m: &WidgetMetrics,
) -> bool {
    let band = m.zone_hit();
    let hit_x = match edge {
        Edge::Right => cx >= area.right - band && cx <= area.right,
        Edge::Left => cx >= area.left && cx <= area.left + band,
    };
    if !hit_x {
        return false;
    }
    let cap_top = win_top + m.margin;
    let corridor = m.corridor_y();
    cy >= cap_top - corridor && cy <= cap_top + m.cap_h + corridor
}

/// 光标是否位于「展开胶囊矩形 ∪ 边缘走廊」（展开后不缩回的有效区）。
fn in_valid_zone(cx: i32, cy: i32, area: &Rect, edge: Edge, y: i32, m: &WidgetMetrics) -> bool {
    if in_edge_zone(cx, cy, area, edge, y, m) {
        return true;
    }
    // 胶囊矩形（物理）。y = 窗口 top，胶囊 top = y + margin。
    let cap_top = y + m.margin;
    let cap_left = match edge {
        Edge::Right => area.right - m.inset() - m.cap_w,
        Edge::Left => area.left + m.inset(),
    };
    cx >= cap_left && cx <= cap_left + m.cap_w && cy >= cap_top && cy <= cap_top + m.cap_h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m100() -> WidgetMetrics {
        WidgetMetrics {
            cap_w: 52,
            cap_h: 260,
            margin: 14,
            win_w: 80,
            win_h: 288,
            scale: 1.0,
        }
    }

    #[test]
    fn 状态序列化契约() {
        let s = WidgetState { docked: Some(Edge::Right), expanded: false, phase: Phase::Hidden };
        let json = serde_json::to_string(&s).unwrap();
        assert_eq!(json, r#"{"docked":"right","expanded":false,"phase":"hidden"}"#);
    }

    #[test]
    fn 边缘热区右缘判定() {
        let area = Rect { left: 0, top: 0, right: 3840, bottom: 2160 };
        // 窗口 top=900 → 胶囊 top=914，走廊 [-46 相对胶囊顶 → 334] 即屏幕 [854, 1234]。
        assert!(in_edge_zone(3835, 1000, &area, Edge::Right, 900, &m100()));
        assert!(in_edge_zone(3840, 1000, &area, Edge::Right, 900, &m100()));
        assert!(!in_edge_zone(3831, 1000, &area, Edge::Right, 900, &m100())); // 8px 外
        assert!(!in_edge_zone(3835, 500, &area, Edge::Right, 900, &m100())); // 走廊外
        assert!(!in_edge_zone(3835, 2000, &area, Edge::Right, 900, &m100())); // 走廊外
    }

    #[test]
    fn 边缘热区按175缩放加宽() {
        let area = Rect { left: 0, top: 0, right: 3840, bottom: 2160 };
        // zone_hit 8→14 物理像素；corridor 60→105。
        let m175 = WidgetMetrics {
            cap_w: 91, cap_h: 455, margin: 25, win_w: 140, win_h: 504, scale: 1.75,
        };
        assert!(in_edge_zone(3840 - 14, 1000, &area, Edge::Right, 900, &m175));
        assert!(!in_edge_zone(3840 - 15, 1000, &area, Edge::Right, 900, &m175));
    }
}
