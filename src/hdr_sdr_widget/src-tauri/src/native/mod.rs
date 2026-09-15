//! Native capsule: one render clock, screen-space optics, velocity-continuous input.
mod brightness;
mod capture;
mod gpu;
pub mod motion;
mod tuning;

use crate::{
    edge::{Edge, Phase, WidgetState},
    window::{self, WidgetMetrics},
    AppState,
};
use gpu::{Gpu, RenderSnapshot};
use hdr_sdr_widget_lib::win32::{
    fullscreen,
    geometry::{self, Rect},
};
use motion::{rubber, Spring};
use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicIsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};
use tauri::{Emitter, Manager};
use windows::{
    core::Result,
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        Graphics::Gdi::{CreateRoundRectRgn, DeleteObject, SetWindowRgn},
        System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED},
        UI::{
            HiDpi::GetDpiForWindow,
            Input::KeyboardAndMouse::{ReleaseCapture, SetCapture},
            WindowsAndMessaging::*,
        },
    },
};

#[derive(Clone, Copy, Debug)]
enum Input {
    Move(i32, i32),
    Down(i32, i32),
    Up(i32, i32),
    Wheel(i16, bool),
    Cancel,
}
#[derive(Default, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub backend: String,
    pub format: i32,
    pub fallback_reason: String,
    pub error: String,
    pub submitted: u64,
    pub captured: u64,
    pub present_count: Option<u32>,
    pub submit_interval_p95_ms: f64,
    pub input_to_submit_p95_ms: f64,
    pub capture_age_ms: f64,
    pub cpu_frame_p95_ms: f64,
    pub actual_presentation_verified: bool,
    pub presented_fps: f64,
    pub presented_interval_p95_ms: f64,
    pub display_refresh_hz: f64,
    pub presentation_samples: u64,
    pub updated_at: u64,
    pub phase: String,
    pub visual_audit: bool,
    pub presentation_observation_gaps: u64,
    pub capture_to_submit_p95_ms: f64,
    pub input_samples: usize,
    pub startup_input_to_submit_ms: f64,
    pub brightness: brightness::Reading,
    pub capture_color_space: i32,
    pub capture_update_gap_max_ms: f64,
    pub display_peak_nits: f32,
    pub sdr_white_nits: f32,
    pub highlight_target_nits: f32,
    pub color_capability_fallback: String,
    pub reveal_scale: f64,
}
struct Shared {
    inputs: Mutex<VecDeque<(Instant, Input)>>,
    stop: AtomicBool,
    reset_capture: AtomicBool,
    diagnostics: Mutex<Diagnostics>,
    position: Mutex<(i32, i32)>,
    brightness: Arc<brightness::Worker>,
}
static SHARED: OnceLock<Arc<Shared>> = OnceLock::new();
static PREV_PROC: AtomicIsize = AtomicIsize::new(0);
pub fn enabled() -> bool {
    std::env::var("HSDR_RENDERER").as_deref() != Ok("legacy")
}
pub(super) fn flag(name: &str) -> bool {
    matches!(std::env::var(name).as_deref(), Ok("1") | Ok("true"))
}
pub fn diagnostics() -> Diagnostics {
    SHARED
        .get()
        .map(|s| s.diagnostics.lock().unwrap().clone())
        .unwrap_or_default()
}
pub fn position() -> Option<(i32, i32)> {
    SHARED.get().map(|s| *s.position.lock().unwrap())
}
pub fn apply_confirmed(
    key: String,
    percent: u8,
) -> Option<hdr_sdr_widget_lib::core::model::WriteResult> {
    SHARED
        .get()
        .map(|s| s.brightness.submit_confirmed(key, percent))
}
pub fn stop() {
    if let Some(s) = SHARED.get() {
        s.stop.store(true, Ordering::Release);
        s.brightness.stop();
    }
}
pub fn invalidate_capture() {
    if let Some(s) = SHARED.get() {
        s.reset_capture.store(true, Ordering::Release);
    }
}

fn push(input: Input) {
    if let Some(s) = SHARED.get() {
        let mut q = s.inputs.lock().unwrap();
        if matches!(input, Input::Move(..))
            && q.back().is_some_and(|(_, i)| matches!(i, Input::Move(..)))
        {
            q.pop_back();
        }
        if q.len() > 1024 {
            q.clear();
            q.push_back((Instant::now(), Input::Cancel));
        }
        q.push_back((Instant::now(), input));
    }
}
unsafe extern "system" fn input_proc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    let point = || {
        let mut p = windows::Win32::Foundation::POINT {
            x: (l.0 as u16 as i16) as i32,
            y: ((l.0 >> 16) as u16 as i16) as i32,
        };
        let _ = windows::Win32::Graphics::Gdi::ClientToScreen(hwnd, &mut p);
        (p.x, p.y)
    };
    match msg {
        WM_MOUSEACTIVATE => return LRESULT(MA_NOACTIVATE as isize),
        WM_LBUTTONDOWN => {
            let (x, y) = point();
            SetCapture(hwnd);
            push(Input::Down(x, y));
            return LRESULT(0);
        }
        WM_LBUTTONUP => {
            let (x, y) = point();
            push(Input::Up(x, y));
            let _ = ReleaseCapture();
            return LRESULT(0);
        }
        WM_MOUSEMOVE => {
            let (x, y) = point();
            push(Input::Move(x, y));
        }
        WM_MOUSEWHEEL => {
            push(Input::Wheel((w.0 >> 16) as u16 as i16, w.0 & 4 != 0));
            return LRESULT(0);
        }
        WM_CAPTURECHANGED | WM_CANCELMODE => push(Input::Cancel),
        WM_ERASEBKGND => return LRESULT(1),
        _ => {}
    }
    let old = PREV_PROC.load(Ordering::Relaxed);
    if old != 0 {
        CallWindowProcW(std::mem::transmute::<isize, WNDPROC>(old), hwnd, msg, w, l)
    } else {
        DefWindowProcW(hwnd, msg, w, l)
    }
}

pub fn start(app: tauri::AppHandle) -> Result<()> {
    let window = window::panel(&app).map_err(|e| {
        windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), e.to_string())
    })?;
    let hwnd = window::widget_hwnd(&window).unwrap();
    let key = app
        .state::<AppState>()
        .current_key
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    let pos = window.outer_position().unwrap_or_default();
    let follow_mouse = app
        .state::<AppState>()
        .settings
        .lock()
        .unwrap()
        .follow_mouse_monitor;
    let shared = Arc::new(Shared {
        inputs: Mutex::new(VecDeque::new()),
        stop: AtomicBool::new(false),
        reset_capture: AtomicBool::new(false),
        diagnostics: Mutex::new(Diagnostics::default()),
        position: Mutex::new((pos.x, pos.y)),
        brightness: brightness::Worker::start(key, follow_mouse),
    });
    let _ = SHARED.set(shared.clone());
    unsafe {
        // The compositor owns transparency; no layered/GDI bitmap is stretched by DWM.
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (ex & !(WS_EX_LAYERED.0 as isize))
                | WS_EX_NOREDIRECTIONBITMAP.0 as isize
                | WS_EX_NOACTIVATE.0 as isize,
        );
        PREV_PROC.store(
            SetWindowLongPtrW(hwnd, GWLP_WNDPROC, input_proc as *const () as isize),
            Ordering::Release,
        );
    }
    let raw = hwnd.0 as isize;
    std::thread::Builder::new()
        .name("native-glass".into())
        .spawn(move || {
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            if let Err(e) = run(app.clone(), HWND(raw as *mut _), shared.clone()) {
                shared.diagnostics.lock().unwrap().error = e.to_string();
                eprintln!("[native-glass] {e}");
                let _ = app.emit("native:error", e.to_string());
            }
        })
        .map_err(|e| {
            windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), e.to_string())
        })?;
    Ok(())
}

#[derive(PartialEq, Eq)]
enum Gesture {
    None,
    Pending,
    Value,
    Window,
}
struct Interaction {
    mode: Gesture,
    start: (i32, i32),
    last: (i32, i32),
    last_at: Instant,
    velocity: (f64, f64),
    request: u64,
    error_id: u64,
}
impl Interaction {
    fn new() -> Self {
        Self {
            mode: Gesture::None,
            start: (0, 0),
            last: (0, 0),
            last_at: Instant::now(),
            velocity: (0.0, 0.0),
            request: 0,
            error_id: 0,
        }
    }
}
fn phase(app: &tauri::AppHandle, next: Phase, expanded: bool) {
    let state = app.state::<AppState>();
    let old = window::widget_state(&state);
    if old.phase != next || old.expanded != expanded {
        window::set_widget_state(
            &state,
            WidgetState {
                phase: next,
                expanded,
                ..old
            },
        );
        window::emit_state(app, &state);
    }
}
fn area(x: i32, y: i32) -> Rect {
    geometry::monitor_work_area(x, y)
        .or_else(geometry::primary_work_area)
        .unwrap_or_default()
}
fn percentile(values: &VecDeque<f64>) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted: Vec<_> = values.iter().copied().collect();
    sorted.sort_by(f64::total_cmp);
    sorted[((sorted.len() - 1) as f64 * 0.95).ceil() as usize]
}
fn record(values: &mut VecDeque<f64>, value: f64) {
    if values.len() == 2048 {
        values.pop_front();
    }
    values.push_back(value);
}

fn run(app: tauri::AppHandle, hwnd: HWND, shared: Arc<Shared>) -> Result<()> {
    let window = window::panel(&app).unwrap();
    let mut scale = unsafe { GetDpiForWindow(hwnd) } as f64 / 96.0;
    if scale <= 0.0 {
        scale = 1.0;
    }
    let mut m = WidgetMetrics::from_window_scale(scale);
    let initial = window.outer_position().unwrap_or_default();
    let mut x = Spring::new(initial.x as f64);
    let mut y = Spring::new(initial.y as f64);
    let mut sx = Spring::new(1.0);
    let mut sy = Spring::new(1.0);
    let initial_state = window::widget_state(&app.state::<AppState>());
    let mut reveal = Spring::new(
        if initial_state.docked.is_some() && !initial_state.expanded {
            0.65
        } else {
            1.0
        },
    );
    let mut anchor = 0.0;
    let mut fill = Spring::new(0.5);
    let mut opacity = Spring::new(1.0);
    let mut hover = Spring::new(0.0);
    let mut pressure = Spring::new(0.0);
    let mut overscroll = Spring::new(0.0);
    let mut input = Interaction::new();
    let mut intent = None;
    let mut retract = None;
    let mut fs_at = Instant::now();
    let mut last = Instant::now();
    let mut last_report = last;
    let mut last_present = last;
    let mut intervals = VecDeque::new();
    let mut inputs = VecDeque::new();
    let mut costs = VecDeque::new();
    let mut capture_latencies = VecDeque::new();
    let mut g: Option<Gpu> = None;
    let mut recreate_at = Instant::now();
    let mut last_region = (0, 0, 0, 0, 0);
    let mut last_host = (i32::MIN, i32::MIN);
    let mut previous_reading = (u64::MAX, u8::MAX);
    let mut was_visible = true;
    let mut known_key = app
        .state::<AppState>()
        .current_key
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_default();
    let mut last_stats: Option<windows::Win32::Graphics::Dxgi::DXGI_FRAME_STATISTICS> = None;
    let mut presentation_intervals = VecDeque::new();
    let mut presentation_origin: Option<(u32, i64)> = None;
    let mut qpc_frequency = 0i64;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut qpc_frequency);
    }
    let visual_audit = flag("HSDR_VISUAL_AUDIT");
    let mut reduced_motion = windows::Win32::Foundation::BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some((&mut reduced_motion as *mut windows::Win32::Foundation::BOOL).cast()),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    let benchmark = flag("HSDR_BENCHMARK");
    let test_dir = std::env::var_os("HSDR_NATIVE_TEST")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    let mut test_started = Instant::now();
    let edge_test = flag("HSDR_EDGE_TEST");
    let edge_left = flag("HSDR_EDGE_LEFT");
    let material_test = flag("HSDR_MATERIAL_TEST");
    let record_dir = std::env::var_os("HSDR_RECORD")
        .filter(|v| !v.is_empty())
        .map(std::path::PathBuf::from);
    if let Some(dir) = &record_dir {
        let _ = std::fs::create_dir_all(dir);
    }
    let mut record_count = 0u64;
    let mut material_dumped = u64::MAX;
    let mut edge_test_stage = u32::MAX;
    let mut ready_at = None;
    let mut last_capture_at: Option<Instant> = None;
    let mut dumped = 0u64;
    if let Some(dir) = &test_dir {
        let _ = std::fs::create_dir_all(dir);
    }
    while !shared.stop.load(Ordering::Acquire) {
        if shared.reset_capture.swap(false, Ordering::AcqRel) {
            g = None;
            last_stats = None;
            recreate_at = Instant::now();
        }
        if let Some(gpu) = &g {
            unsafe {
                gpu.wait();
            }
        }
        let now = Instant::now();
        let dt = now.duration_since(last).as_secs_f64();
        last = now;
        let frame_start = now;
        let visible = window.is_visible().unwrap_or(false);
        if !visible {
            was_visible = false;
            g = None;
            last_stats = None;
            std::thread::sleep(Duration::from_millis(30));
            continue;
        }
        let mut state = window::widget_state(&app.state::<AppState>());
        if edge_test {
            let t = test_started.elapsed().as_millis() % 2400;
            let stage = if t < 1000 {
                0
            } else if t < 1200 {
                1
            } else if t < 1400 {
                2
            } else {
                3
            };
            if stage != edge_test_stage {
                edge_test_stage = stage;
                let s = app.state::<AppState>();
                window::set_widget_state(
                    &s,
                    WidgetState {
                        docked: Some(if edge_left { Edge::Left } else { Edge::Right }),
                        expanded: true,
                        phase: if stage % 2 == 0 {
                            Phase::Revealing
                        } else {
                            Phase::Hiding
                        },
                    },
                );
                state = window::widget_state(&s);
            }
        }
        let actual = geometry::window_rect(hwnd).unwrap_or(Rect {
            left: initial.x,
            top: initial.y,
            right: initial.x + m.win_w,
            bottom: initial.y + m.win_h,
        });
        if !was_visible
            || (last_host != (i32::MIN, i32::MIN)
                && (actual.left, actual.top) != last_host
                && input.mode == Gesture::None)
        {
            x.snap(actual.left as f64);
            y.snap(actual.top as f64);
        }
        was_visible = true;
        let new_scale = unsafe { GetDpiForWindow(hwnd) } as f64 / 96.0;
        if new_scale > 0.0 && (new_scale - scale).abs() > 0.001 {
            scale = new_scale;
            m = WidgetMetrics::from_window_scale(scale);
            last_region = (0, 0, 0, 0, 0);
        }
        let region = area(
            (x.position + m.margin as f64 + m.cap_w as f64 / 2.0) as i32,
            (y.position + m.margin as f64 + m.cap_h as f64 / 2.0) as i32,
        );
        if now.duration_since(fs_at) >= Duration::from_millis(200) {
            fs_at = now;
            let full = fullscreen::foreground_fullscreen(Some(hwnd));
            if full && state.phase != Phase::Suppressed {
                phase(&app, Phase::Suppressed, false);
                input.mode = Gesture::None;
                pressure.target = 0.0;
            } else if !full && state.phase == Phase::Suppressed {
                phase(
                    &app,
                    if state.docked.is_some() {
                        Phase::Hidden
                    } else {
                        Phase::Visible
                    },
                    state.docked.is_none(),
                );
            }
            state = window::widget_state(&app.state::<AppState>());
            if let Some(gpu) = &g {
                let d = unsafe { gpu.output.GetDesc()? }.DesktopCoordinates;
                let center = (
                    (x.position + m.margin as f64 + m.cap_w as f64 / 2.0)
                        .clamp(region.left as f64 + 1.0, region.right as f64 - 1.0),
                    y.position + m.margin as f64 + m.cap_h as f64 / 2.0,
                );
                if center.0 < d.left as f64
                    || center.0 >= d.right as f64
                    || center.1 < d.top as f64
                    || center.1 >= d.bottom as f64
                {
                    g = None;
                }
            }
        }
        let requested_key = app
            .state::<AppState>()
            .current_key
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_default();
        if requested_key != known_key && !requested_key.is_empty() {
            shared.brightness.select(&requested_key);
            known_key = requested_key;
            input.request = 0;
            previous_reading = (u64::MAX, u8::MAX);
        }
        let reading = shared.brightness.reading();
        if reading.key.is_empty() {
            let remembered = app.state::<AppState>().current_key.lock().unwrap().clone();
            if let Some(k) = remembered {
                shared.brightness.select(&k);
            }
        } else {
            // Persist the resolved target once; no per-frame disk writes.
            let s = app.state::<AppState>();
            if s.current_key.lock().unwrap().as_deref() != Some(reading.key.as_str()) {
                *s.current_key.lock().unwrap() = Some(reading.key.clone());
                known_key = reading.key.clone();
            }
        }
        if input.mode == Gesture::None
            && reading.id >= input.request
            && previous_reading != (reading.id, reading.percent)
        {
            fill.target = reading.percent as f64 / 100.0;
            previous_reading = (reading.id, reading.percent);
        }
        if reading.id >= input.request && reading.error.is_some() && reading.id != input.error_id {
            input.error_id = reading.id;
            fill.target = reading.percent as f64 / 100.0;
            let h = app.clone();
            let message = reading.error.clone().unwrap();
            std::thread::spawn(move || {
                let _ = crate::commands::show_toast(h, message, Some("error".into()), Some(4000));
            });
        }
        let events: Vec<_> = shared.inputs.lock().unwrap().drain(..).collect();
        let mut input_at = None;
        for (at, event) in events {
            input_at = Some(at);
            if state.phase == Phase::Suppressed {
                continue;
            }
            match event {
                Input::Down(px, py) => {
                    if input.mode != Gesture::None {
                        continue;
                    }
                    input.mode = Gesture::Pending;
                    input.start = (px, py);
                    input.last = (px, py);
                    input.last_at = at;
                    input.velocity = (0.0, 0.0);
                    // Take over at the current trajectory without jumping to an endpoint.
                    x.target = x.position;
                    y.target = y.position;
                    pressure.target = 1.0;
                    phase(&app, Phase::Pressed, true);
                }
                Input::Move(px, py) => {
                    if input.mode == Gesture::Pending {
                        let dx = px - input.start.0;
                        let dy = py - input.start.1;
                        if (dx.abs() + dy.abs()) as f64 >= 6.0 * scale {
                            input.mode = if dx.abs() > dy.abs() {
                                Gesture::Window
                            } else {
                                Gesture::Value
                            };
                            phase(&app, Phase::Dragging, true);
                        }
                    }
                    if input.mode == Gesture::Window {
                        let dx = px - input.last.0;
                        let dy = py - input.last.1;
                        let elapsed = at.duration_since(input.last_at).as_secs_f64().max(0.001);
                        input.velocity = (
                            (dx as f64 / elapsed).clamp(-2500.0 * scale, 2500.0 * scale),
                            (dy as f64 / elapsed).clamp(-2500.0 * scale, 2500.0 * scale),
                        );
                        if state.docked.is_some() {
                            x.position += anchor;
                            x.target += anchor;
                            anchor = 0.0;
                            state.docked = None;
                        }
                        x.snap(x.position + dx as f64);
                        y.snap(y.position + dy as f64);
                        let s = app.state::<AppState>();
                        window::set_widget_state(
                            &s,
                            WidgetState {
                                docked: None,
                                expanded: true,
                                phase: Phase::Dragging,
                            },
                        );
                    } else if input.mode == Gesture::Value && reading.hdr {
                        let elapsed = at.duration_since(input.last_at).as_secs_f64().max(0.001);
                        input.velocity.1 = ((py - input.last.1) as f64 / elapsed)
                            .clamp(-2500.0 * scale, 2500.0 * scale);
                        let p = 1.0
                            - (py as f64
                                - y.position
                                - m.margin as f64
                                - m.cap_h as f64 * (1.0 - sy.position * reveal.position) / 2.0)
                                / (m.cap_h as f64 * sy.position * reveal.position);
                        fill.snap(p.clamp(0.0, 1.0));
                        overscroll.snap(rubber((p - p.clamp(0.0, 1.0)) * 200.0, 12.0));
                        input.request = shared.brightness.submit(
                            reading.key.clone(),
                            (p.clamp(0.0, 1.0) * 100.0).round() as u8,
                            false,
                        );
                    }
                    input.last = (px, py);
                    input.last_at = at;
                }
                Input::Up(px, py) => {
                    if input.mode == Gesture::None {
                        continue;
                    }
                    if input.mode == Gesture::Window {
                        x.snap(x.position + (px - input.last.0) as f64);
                        y.snap(y.position + (py - input.last.1) as f64);
                        let a = area(px, py);
                        let capx = x.position + m.margin as f64;
                        let projected = capx + input.velocity.0 * 0.035;
                        let dock = if projected + m.cap_w as f64 >= a.right as f64 - 28.0 * scale {
                            Some(Edge::Right)
                        } else if projected <= a.left as f64 + 28.0 * scale {
                            Some(Edge::Left)
                        } else {
                            None
                        };
                        let s = app.state::<AppState>();
                        window::set_widget_state(
                            &s,
                            WidgetState {
                                docked: dock,
                                expanded: true,
                                phase: if dock.is_some() {
                                    Phase::Hiding
                                } else {
                                    Phase::Visible
                                },
                            },
                        );
                        if dock.is_some() {
                            x.velocity = input.velocity.0;
                        } else {
                            x.target = x.position.clamp(
                                a.left as f64 + 8.0 * scale - m.margin as f64,
                                a.right as f64 - 8.0 * scale - m.cap_w as f64 - m.margin as f64,
                            );
                        }
                        y.target = y.position.clamp(
                            a.top as f64 + 8.0 * scale - m.margin as f64,
                            a.bottom as f64 - 8.0 * scale - m.cap_h as f64 - m.margin as f64,
                        );
                        persist(
                            &app,
                            dock,
                            x.target + m.margin as f64,
                            y.target + m.margin as f64,
                        );
                        window::emit_state(&app, &s);
                    } else if reading.hdr {
                        let p = (1.0
                            - (py as f64
                                - y.position
                                - m.margin as f64
                                - m.cap_h as f64 * (1.0 - sy.position * reveal.position) / 2.0)
                                / (m.cap_h as f64 * sy.position * reveal.position))
                            .clamp(0.0, 1.0);
                        if input.mode == Gesture::Value {
                            fill.snap(p);
                            if at.duration_since(input.last_at) < Duration::from_millis(80) {
                                fill.velocity =
                                    (-input.velocity.1 / m.cap_h as f64).clamp(-1.5, 1.5);
                                if overscroll.position.abs() > 0.01 {
                                    overscroll.velocity =
                                        (-input.velocity.1 / scale).clamp(-120.0, 120.0);
                                }
                            }
                        } else {
                            fill.target = p;
                        }
                        input.request = shared.brightness.submit(
                            reading.key.clone(),
                            (p * 100.0).round() as u8,
                            true,
                        );
                    } else if input.mode == Gesture::Pending {
                        let h = app.clone();
                        std::thread::spawn(move || {
                            let _ = crate::commands::open_hdr_settings(h);
                        });
                    }
                    if input.mode != Gesture::Window {
                        phase(&app, Phase::Visible, true);
                    }
                    input.mode = Gesture::None;
                    pressure.target = 0.0;
                    overscroll.target = 0.0;
                }
                Input::Cancel => {
                    if input.mode != Gesture::None {
                        input.mode = Gesture::None;
                        pressure.target = 0.0;
                        overscroll.target = 0.0;
                        phase(&app, Phase::Visible, true);
                    }
                }
                Input::Wheel(delta, shift) => {
                    if reading.hdr && input.mode == Gesture::None {
                        let s = app.state::<AppState>();
                        let settings = s.settings.lock().unwrap();
                        let step = if shift {
                            settings.step_shift
                        } else {
                            settings.step
                        };
                        let p = (fill.target * 100.0 + delta as f64 / 120.0 * step as f64)
                            .round()
                            .clamp(0.0, 100.0);
                        fill.target = p / 100.0;
                        input.request =
                            shared.brightness.submit(reading.key.clone(), p as u8, true);
                    }
                }
            }
        }
        state = window::widget_state(&app.state::<AppState>());
        let cursor = geometry::cursor_pos().unwrap_or((-99999, -99999));
        let inside = capsule_contains(
            cursor.0 as f64 - x.position - anchor - m.margin as f64 - m.cap_w as f64 / 2.0,
            cursor.1 as f64 - y.position - m.margin as f64 - m.cap_h as f64 / 2.0,
            m.cap_w as f64 / 2.0 * sx.position * reveal.position,
            m.cap_h as f64 / 2.0 * sy.position * reveal.position,
        );
        hover.target = if inside { 1.0 } else { 0.0 };
        if let Some(edge) = state.docked {
            let edge_hit = match edge {
                Edge::Left => cursor.0 <= region.left + (8.0 * scale) as i32,
                Edge::Right => cursor.0 >= region.right - (8.0 * scale) as i32,
            };
            let corridor = cursor.1 as f64 >= y.position + m.margin as f64 - 48.0 * scale
                && cursor.1 as f64 <= y.position + (m.margin + m.cap_h) as f64 + 48.0 * scale;
            let near = edge_hit && corridor;
            // Scripted QA owns edge intent; physical cursor timers must not override it.
            if !edge_test {
                if state.phase == Phase::Hidden {
                    if near {
                        let t = intent.get_or_insert(now);
                        if now.duration_since(*t) >= Duration::from_millis(60) {
                            phase(&app, Phase::Revealing, true);
                            intent = None;
                        }
                    } else {
                        intent = None;
                    }
                } else if state.phase == Phase::Hiding && (inside || near) {
                    phase(&app, Phase::Revealing, true);
                    retract = None;
                } else if matches!(
                    state.phase,
                    Phase::Visible | Phase::Hovered | Phase::Revealing
                ) && input.mode == Gesture::None
                {
                    if inside || near {
                        retract = None;
                    } else {
                        let t = retract.get_or_insert(now);
                        if now.duration_since(*t) >= Duration::from_millis(1200) {
                            phase(&app, Phase::Hiding, true);
                            retract = None;
                        }
                    }
                }
            }
            state = window::widget_state(&app.state::<AppState>());
            let hidden = matches!(
                state.phase,
                Phase::Hidden | Phase::Hiding | Phase::Suppressed
            );
            let target = if hidden {
                window::dock_hidden_pos(&region, edge, (y.position + m.margin as f64) as i32, &m)
            } else {
                window::dock_expanded_pos(&region, edge, (y.position + m.margin as f64) as i32, &m)
            };
            if input.mode == Gesture::None {
                let correction = if hidden {
                    m.cap_w as f64 * (1.0 - 0.65) * if edge == Edge::Right { -1.0 } else { 1.0 }
                } else {
                    0.0
                };
                x.target = target.0 as f64 + correction;
            }
        }
        // A moving lens stretches across its travel and contracts vertically.
        // Targets derive from momentum, never a phase timer; interrupted motion
        // retains both the position spring and the existing shape spring velocity.
        reveal.target =
            if state.docked.is_some() && matches!(state.phase, Phase::Hidden | Phase::Hiding) {
                0.65
            } else {
                1.0
            };
        reveal.step(dt, tuning::REVEAL.0, tuning::REVEAL.1);
        let stretch = motion::travel_stretch(x.velocity / scale);
        sx.target = stretch
            + if input.mode == Gesture::Window {
                1.05
            } else {
                1.0 + tuning::HOVER_WIDTH * hover.target + tuning::PRESS_WIDTH * pressure.target
            };
        sy.target = 1.0
            + tuning::HOVER_HEIGHT * hover.target
            + tuning::PRESS_HEIGHT * pressure.target
            + overscroll.position.abs() * 0.001
            - stretch * 0.32;
        opacity.target = if state.phase == Phase::Suppressed {
            0.0
        } else if matches!(state.phase, Phase::Hidden | Phase::Hiding) {
            0.75
        } else {
            1.0
        };
        x.step(dt, tuning::POSITION.0, tuning::POSITION.1);
        y.step(dt, 4.5, 0.85);
        let shape = if pressure.target > 0.0 {
            tuning::PRESS
        } else {
            tuning::SHAPE
        };
        sx.step(dt, shape.0, shape.1);
        sy.step(dt, shape.0, shape.1);
        hover.step(dt, 6.0, 1.0);
        pressure.step(dt, 9.0, 1.0);
        fill.step(dt, tuning::LIQUID.0, tuning::LIQUID.1);
        opacity.step(dt, 5.0, 1.0);
        overscroll.step(dt, 5.0, 0.75);
        if !reduced_motion.as_bool() {
            reveal.snap(reveal.target);
            x.snap(x.target);
            y.snap(y.target);
            sx.snap(sx.target);
            sy.snap(sy.target);
            fill.snap(fill.target);
            opacity.snap(opacity.target);
        }
        if !x.moving() && !reveal.moving() && state.phase == Phase::Revealing {
            phase(&app, Phase::Visible, true);
        }
        if !x.moving() && !reveal.moving() && state.phase == Phase::Hiding {
            phase(&app, Phase::Hidden, false);
        }
        // Docking moves the shader geometry inside a stable host, not the HWND each frame.
        let host_x = if let Some(edge) = state.docked {
            window::dock_expanded_pos(&region, edge, (y.position + m.margin as f64) as i32, &m).0
        } else {
            x.position.round() as i32
        };
        let host_y = y.position.round() as i32;
        let host_w = (128.0 * scale).round() as u32;
        let host_h = m.win_h as u32;
        if last_host != (host_x, host_y)
            || actual.width() != host_w as i32
            || actual.height() != host_h as i32
        {
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    host_x,
                    host_y,
                    host_w as i32,
                    host_h as i32,
                    SWP_NOACTIVATE | SWP_NOZORDER,
                );
            }
            last_host = (host_x, host_y);
        }
        *shared.position.lock().unwrap() = (x.position.round() as i32, y.position.round() as i32);
        anchor = match state.docked {
            Some(Edge::Right) => m.cap_w as f64 * (1.0 - reveal.position) / 2.0,
            Some(Edge::Left) => -m.cap_w as f64 * (1.0 - reveal.position) / 2.0,
            None => 0.0,
        };
        let cx = m.margin as f64 + m.cap_w as f64 / 2.0 + x.position + anchor - host_x as f64;
        let cy = m.margin as f64 + m.cap_h as f64 / 2.0;
        let hw = m.cap_w as f64 / 2.0 * sx.position * reveal.position;
        let hh = m.cap_h as f64 / 2.0 * sy.position * reveal.position;
        let clip = if matches!(state.phase, Phase::Hidden | Phase::Suppressed) {
            (0, 0, 0, 0, 0)
        } else {
            (
                (cx - hw).floor() as i32,
                (cy - hh).floor() as i32,
                (cx + hw).ceil() as i32 + 1,
                (cy + hh).ceil() as i32 + 1,
                (2.0 * hw).round() as i32,
            )
        };
        // A shaped HWND routes outside clicks across process boundaries; HTTRANSPARENT alone cannot.
        // Hidden still needs a 2-DIP visual sliver: use the visual shape and pass mouse activation via edge logic.
        let visual_clip = (
            (cx - hw).floor() as i32,
            (cy - hh).floor() as i32,
            (cx + hw).ceil() as i32 + 1,
            (cy + hh).ceil() as i32 + 1,
            (2.0 * hw).round() as i32,
        );
        let clip = if state.phase == Phase::Suppressed {
            clip
        } else {
            visual_clip
        };
        if clip != last_region {
            unsafe {
                let r = CreateRoundRectRgn(clip.0, clip.1, clip.2, clip.3, clip.4, clip.4);
                if SetWindowRgn(hwnd, r, true) == 0 {
                    let _ = DeleteObject(r);
                }
            }
            last_region = clip;
        }
        if g.is_none() && now >= recreate_at {
            match unsafe {
                Gpu::new(
                    hwnd,
                    (x.position + m.margin as f64 + m.cap_w as f64 / 2.0) as i32,
                    (y.position + m.margin as f64 + m.cap_h as f64 / 2.0) as i32,
                    host_w,
                    host_h,
                )
            } {
                Ok(gpu) => {
                    ready_at = None;
                    last_capture_at = None;
                    g = Some(gpu);
                    last_stats = None;
                    presentation_intervals.clear();
                    presentation_origin = None;
                    let mut d = shared.diagnostics.lock().unwrap();
                    d.error.clear();
                    d.presentation_samples = 0;
                    d.actual_presentation_verified = false;
                    d.presentation_observation_gaps = 0;
                }
                Err(e) => {
                    shared.diagnostics.lock().unwrap().error = e.to_string();
                    recreate_at = now + Duration::from_millis(1000);
                }
            }
        }
        if let Some(gpu) = &mut g {
            let mut snapshot = RenderSnapshot {
                viewport: [host_w as f32, host_h as f32, host_x as f32, host_y as f32],
                capsule: [cx as f32, cy as f32, hw as f32, hh as f32],
                material: [
                    scale as f32,
                    fill.position as f32,
                    if reading.hdr {
                        (reading.raw as f32 / 1000.0).max(1.0)
                    } else {
                        1.0
                    },
                    opacity.position as f32,
                ],
                feedback: [
                    hover.position as f32,
                    pressure.position as f32,
                    overscroll.position as f32,
                    if reading.hdr { 1.0 } else { 0.0 },
                ],
                optics: tuning::OPTICS,
                timing: [dt as f32, reveal.position as f32, 0.0, 0.0],
                pointer: [
                    (input.last.0 - host_x) as f32,
                    (input.last.1 - host_y) as f32,
                    0.0,
                    0.0,
                ],
                ..Default::default()
            };
            let stage = test_started.elapsed().as_secs();
            if benchmark {
                let t = test_started.elapsed().as_secs_f32();
                snapshot.capsule[0] += (t * 4.0).sin() * 6.0 * scale as f32;
                snapshot.capsule[2] *= 1.0 + 0.04 * (t * 5.0).sin();
                snapshot.capsule[3] *= 1.0 + 0.012 * (t * 5.0).sin();
            }
            if test_dir.is_some() {
                let shape = match stage {
                    0..=2 => (1.0, 1.0),
                    3..=4 => (1.04, 1.012),
                    5..=6 => (1.055, 0.987),
                    _ => (1.05, 1.02),
                };
                snapshot.capsule = [
                    (m.margin + m.cap_w / 2) as f32,
                    cy as f32,
                    m.cap_w as f32 / 2.0 * shape.0,
                    m.cap_h as f32 / 2.0 * shape.1,
                ];
                snapshot.desktop[3] = 2.0;
                snapshot.material[1] = 0.4;
                snapshot.material[3] = 1.0;
            }
            if material_test {
                snapshot.desktop[3] = match stage / 5 {
                    0 => 3.0,
                    1 => 4.0,
                    2 => 5.0,
                    _ => 2.0,
                };
                snapshot.material[1] = (stage % 5) as f32 / 4.0;
                snapshot.material[3] = 1.0;
            }
            let result = unsafe { gpu.resize(host_w, host_h).and_then(|_| gpu.draw(snapshot)) };
            if visual_audit {
                unsafe {
                    gpu.prepare_visual_audit(hwnd);
                }
            }
            if let Some(dir) = &test_dir {
                if [2, 4, 6, 8].contains(&stage) && dumped != stage {
                    let _ = unsafe {
                        gpu.dump(
                            &dir.join(format!("shape-{stage}.ppm")),
                            snapshot.material[2],
                        )
                    };
                    dumped = stage;
                }
            }
            if result.is_ok() {
                if ready_at.is_some()
                    && material_test
                    && stage < 20
                    && test_started.elapsed().subsec_millis() >= 800
                    && material_dumped != stage
                {
                    if let Some(dir) = &test_dir {
                        unsafe {
                            gpu.dump(
                                &dir.join(format!("material-{stage:02}.ppm")),
                                snapshot.material[2],
                            )?;
                        }
                        material_dumped = stage;
                    }
                }
                if let Some(dir) = &record_dir {
                    if ready_at.is_some()
                        && test_started.elapsed().as_millis() >= record_count as u128 * 33
                        && record_count < 180
                    {
                        unsafe {
                            gpu.dump(
                                &dir.join(format!("frame-{record_count:04}.ppm")),
                                snapshot.material[2],
                            )?;
                        }
                        let entry = format!(
                            "{},{:.6},{:.6},{:.6},{:.6},{:.6}
",
                            record_count,
                            test_started.elapsed().as_secs_f64(),
                            x.position,
                            x.velocity,
                            reveal.position,
                            reveal.velocity
                        );
                        use std::io::Write;
                        if let Ok(mut f) = std::fs::OpenOptions::new()
                            .create(true)
                            .append(true)
                            .open(dir.join("motion.csv"))
                        {
                            let _ = f.write_all(entry.as_bytes());
                        }
                        record_count += 1;
                    }
                }
            }
            match result {
                Ok(fresh) => {
                    let submitted = Instant::now();
                    record(
                        &mut intervals,
                        submitted.duration_since(last_present).as_secs_f64() * 1000.0,
                    );
                    last_present = submitted;
                    record(&mut costs, frame_start.elapsed().as_secs_f64() * 1000.0);
                    if let Some(t) = input_at {
                        let latency = submitted.duration_since(t).as_secs_f64() * 1000.0;
                        if ready_at.is_some_and(|ready| t >= ready) {
                            record(&mut inputs, latency);
                        } else {
                            shared
                                .diagnostics
                                .lock()
                                .unwrap()
                                .startup_input_to_submit_ms = latency;
                        }
                    }
                    let mut d = shared.diagnostics.lock().unwrap();
                    if fresh {
                        if ready_at.is_none() && (test_dir.is_some() || record_dir.is_some()) {
                            test_started = Instant::now();
                        }
                        ready_at.get_or_insert(submitted);
                        if let Some(previous) = last_capture_at.replace(submitted) {
                            d.capture_update_gap_max_ms = d
                                .capture_update_gap_max_ms
                                .max(submitted.duration_since(previous).as_secs_f64() * 1000.0);
                        }
                    }
                    d.submitted += 1;
                    if fresh {
                        d.captured += 1;
                    }
                    d.display_peak_nits = gpu.peak_nits;
                    d.sdr_white_nits = gpu.white_nits;
                    d.highlight_target_nits = gpu.highlight_nits;
                    d.color_capability_fallback = if gpu.peak_fallback {
                        "Invalid or unavailable DXGI peak luminance; using 400 nit".into()
                    } else {
                        String::new()
                    };
                    d.reveal_scale = reveal.position;
                    d.brightness = reading.clone();
                    d.input_samples = inputs.len();
                    if fresh {
                        if let Some(f) = &gpu.capture.frame {
                            record(
                                &mut capture_latencies,
                                f.source_age_ms + f.acquired.elapsed().as_secs_f64() * 1000.0,
                            );
                        }
                    }
                    d.capture_to_submit_p95_ms = percentile(&capture_latencies);
                    d.updated_at = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as u64;
                    d.phase = format!("{:?}", state.phase).to_lowercase();
                    d.visual_audit = visual_audit;
                    if let Some(stats) = unsafe { gpu.presentation() } {
                        if stats.PresentRefreshCount > 0
                            && stats.SyncQPCTime > 0
                            && stats.PresentCount > 0
                        {
                            let origin = presentation_origin
                                .get_or_insert((stats.PresentCount, stats.SyncQPCTime));
                            let elapsed =
                                (stats.SyncQPCTime - origin.1) as f64 / qpc_frequency.max(1) as f64;
                            if elapsed > 1.0 {
                                d.presented_fps =
                                    stats.PresentCount.saturating_sub(origin.0) as f64 / elapsed;
                            }
                        }
                        if let Some(prev) = &last_stats {
                            let frames = stats.PresentCount.saturating_sub(prev.PresentCount);
                            let refreshes =
                                stats.SyncRefreshCount.saturating_sub(prev.SyncRefreshCount);
                            let ticks = stats.SyncQPCTime - prev.SyncQPCTime;
                            if frames > 0
                                && refreshes > 0
                                && ticks > 0
                                && qpc_frequency > 0
                                && prev.PresentRefreshCount > 0
                            {
                                let period =
                                    ticks as f64 / qpc_frequency as f64 * 1000.0 / refreshes as f64;
                                d.display_refresh_hz = 1000.0 / period;
                                let displayed_refreshes = stats
                                    .PresentRefreshCount
                                    .saturating_sub(prev.PresentRefreshCount);
                                if frames == 1 && displayed_refreshes > 0 {
                                    record(
                                        &mut presentation_intervals,
                                        displayed_refreshes as f64 * period,
                                    );
                                    d.presentation_samples += 1;
                                } else {
                                    d.presentation_observation_gaps += 1;
                                }
                                d.presented_interval_p95_ms = percentile(&presentation_intervals);
                                d.actual_presentation_verified = d.presentation_samples >= 120
                                    && d.presentation_observation_gaps == 0
                                    && !visual_audit;
                            }
                        }
                        if last_stats
                            .as_ref()
                            .is_none_or(|p| p.PresentCount != stats.PresentCount)
                        {
                            last_stats = Some(stats);
                        }
                    }
                    d.backend = gpu.capture.backend.clone();
                    d.fallback_reason = gpu.capture.fallback_reason.clone();
                    if let Some(f) = &gpu.capture.frame {
                        d.format = f.format.0;
                        d.capture_color_space = f.color_space;
                        d.capture_age_ms = f.acquired.elapsed().as_secs_f64() * 1000.0;
                    }
                    if now.duration_since(last_report) >= Duration::from_millis(500) {
                        d.submit_interval_p95_ms = percentile(&intervals);
                        d.input_to_submit_p95_ms = percentile(&inputs);
                        d.cpu_frame_p95_ms = percentile(&costs);
                        d.present_count = unsafe { gpu.present_count() };
                        last_report = now;
                        let _ = app.emit("native:diagnostics", &*d);
                    }
                }
                Err(e) => {
                    shared.diagnostics.lock().unwrap().error = e.to_string();
                    g = None;
                    recreate_at = now + Duration::from_millis(250);
                }
            }
        } else {
            std::thread::sleep(Duration::from_millis(16));
        }
    }
    Ok(())
}
fn capsule_contains(dx: f64, dy: f64, half_width: f64, half_height: f64) -> bool {
    dx.hypot((dy.abs() - (half_height - half_width).max(0.0)).max(0.0)) <= half_width
}
fn persist(app: &tauri::AppHandle, dock: Option<Edge>, x: f64, y: f64) {
    let app = app.clone();
    std::thread::spawn(move || {
        let s = app.state::<AppState>();
        let key = s.current_key.lock().unwrap().clone().unwrap_or_default();
        if let Ok(mut settings) = s.settings.lock() {
            settings.dock_side = dock;
            settings.widget_y = y.round() as i32;
            settings.last_window_pos.x = x.round() as i32;
            settings.last_window_pos.y = y.round() as i32;
            settings.last_window_pos.monitor_key = key.clone();
            settings.last_monitor_key = key;
            let _ = s.store.save(&settings);
        };
    });
}
