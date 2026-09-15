//! `#[tauri::command]` IPC 接口层——UI 唯一依赖面。
//!
//! 与前端 `src/bridge.ts` 的契约一一对应。所有对外结构都显式序列化为
//! camelCase JSON，不直接把 `core::model` 的内部结构透出（避免 `Percent` 等
//! 包装类型泄漏成 `{value: 37}` 这种前端不友好的形状）。
//!
//! 错误统一返回 [`CommandError`]（`{code, message, detail}`），前端
//! `bridge.ts` 拿到后可直接用于 Toast，无需再映射一次。

use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, State};

// base64 的 encode/decode 是 trait 方法，必须把 Engine trait 带进作用域。
use base64::Engine as _;

use hdr_sdr_widget_lib::core::controller::{real_controller, RealController, WriteRoute};
use hdr_sdr_widget_lib::core::model::{DisplayState, DisplayTarget, Percent, WriteResult};
use hdr_sdr_widget_lib::error::AppError;
use hdr_sdr_widget_lib::win32::capture::{CaptureShared, CaptureStatsSnapshot};

use crate::store::settings::{AppSettings, SettingsStore};

/// 全局运行时状态，由 `main.rs` 通过 `.manage(AppState::new())` 注册。
///
/// # 加锁纪律（务必遵守）
///
/// `std::sync::Mutex` **不可重入**：同一线程对同一把锁二次 `lock()` 不是报错，
/// 而是**永久挂起** —— 整个应用卡死且没有任何日志，是最难查的一类故障。
///
/// 因此：
/// 1. 任何 `lock()` 的 guard 都要限制在最小作用域内，取完值立刻 drop；
/// 2. **绝不**在持有某把锁时调用会去锁同一把锁的函数。踩过的例子：
///    `apply_preset` 曾持有 `settings` guard 到函数末尾，同时调用
///    `remember_current_key`（它也要锁 `settings`）→ 首启后第一次点托盘预设
///    即死锁；
/// 3. 需要多把锁时统一顺序：先 `current_key`，再 `settings`，不要嵌套反向。
///
/// - `controller`：亮度读写编排（节流 / 去重 / 回读校验）。
/// - `settings`：内存中的配置副本，`set_settings` 落盘前以此为准。
/// - `current_key`：用户最近操作的显示器稳定键（跨会话持久化在 settings）。
/// - `store`：配置的磁盘读写器。
/// - `widget`：悬浮物贴边状态唯一真源（v2，`edge.rs` 轮询线程 / 命令共用）。
/// - `capture`：背景捕获共享槽（v0.3 新增，供诊断 HUD 读取捕获统计）。
///   用 `OnceLock` 是因为 `AppState` 在 `.manage()` 时构造，而捕获共享槽在
///   `setup()` 内才创建，`manage` 之后无法再拿到 `&mut AppState`。
/// - `glass`：前端渲染端自报的帧状态（v0.3）。与 `capture` 配对——一个是
///   "采集端做了什么"，一个是"渲染端看到了什么"，两边对不上就能把故障
///   锁定在中间的协议层。
pub struct AppState {
    pub controller: Mutex<RealController>,
    pub settings: Mutex<AppSettings>,
    pub current_key: Mutex<Option<String>>,
    pub store: SettingsStore,
    pub widget: Mutex<crate::edge::WidgetState>,
    pub capture: std::sync::OnceLock<Arc<CaptureShared>>,
    pub glass: Mutex<GlassStatusSnapshot>,
    /// Toast 世代号：每次弹新 Toast 自增，旧 Toast 的定时收起线程据此让位。
    pub toast_gen: Mutex<u64>,
    /// 最近一条 Toast 载荷（Toast 窗口挂载时拉取，补事件竞态）。
    pub last_toast: Mutex<Option<ToastPayload>>,
}

impl AppState {
    /// 用真实 API + 磁盘配置构造状态。APPDATA 无法定位时视为致命错误。
    pub fn new() -> Self {
        let store = SettingsStore::new().expect("无法定位应用数据目录（APPDATA）");
        let settings = store.load();
        // 恢复上次选中的显示器。以前这里恒为 `None`，于是 `resolve_target`
        // 每次都退化到"主屏 / 第一台"——副屏用户的选择在重启后就没了。
        let remembered = settings.last_monitor_key.clone();
        Self {
            controller: Mutex::new(real_controller()),
            settings: Mutex::new(settings),
            current_key: Mutex::new(if remembered.is_empty() { None } else { Some(remembered) }),
            store,
            widget: Mutex::new(crate::edge::WidgetState::default()),
            capture: std::sync::OnceLock::new(),
            glass: Mutex::new(GlassStatusSnapshot::default()),
            toast_gen: Mutex::new(0),
            last_toast: Mutex::new(None),
        }
    }
}

/// 记录"用户最近操作的显示器"：内存 + 落盘（`lastMonitorKey`）。
///
/// 两处都写是必须的。只写内存 → 重启丢失；只写磁盘 → 本次会话内
/// `resolve_target` 仍读内存，读到的是旧值。
fn remember_current_key(state: &AppState, key: &str) {
    if key.is_empty() {
        return;
    }
    if let Ok(mut cur) = state.current_key.lock() {
        *cur = Some(key.to_string());
    }
    if let Ok(mut settings) = state.settings.lock() {
        if settings.last_monitor_key != key {
            settings.last_monitor_key = key.to_string();
            let _ = state.store.save(&settings);
        }
    }
}

/// 前端玻璃渲染链路状态（`src/ui/liquidGlass.ts::GlassFrameInfo` 的镜像）。
///
/// 采集端统计（[`CaptureStatsSnapshot`]）说"后端拷了几帧、格式对不对"；
/// 本结构说"前端解码了几帧、有几帧是空的、上屏成功几次"。两个都对上，
/// 玻璃才是真的亮了。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlassStatusSnapshot {
    /// `idle` / `ok` / `blank` / `no-frame` / `bad-payload` / `error`。
    pub status: String,
    /// 后端帧序号（0 = 尚未收到有效帧）。
    pub seq: u32,
    /// 帧物理宽（0 = 未知）。
    pub frame_w: u32,
    /// 帧物理高。
    pub frame_h: u32,
    /// 帧物理宽 / 窗口 DIP 宽 = DPI 缩放系数。
    pub scale: f64,
    /// 最近一次异常的说明文本。
    pub detail: String,
    /// 已成功上屏的帧数。
    pub rendered: u64,
    /// 判定为空白并丢弃的帧数。
    pub blank: u64,
    /// 单帧渲染耗时滚动平均（ms，含模糊 + 裁剪 + 逐像素折射）。0 = 尚未渲染过。
    ///
    /// 用途：判断"采样率还能不能往上提" —— 想跑 30fps 就得知道一帧花多久。
    pub render_ms: f64,
    /// 前端已发起的取帧请求次数（含失败的）。
    ///
    /// 诊断价值：区分"帧循环压根没启动"（`polls == 0`，相位判定问题）与
    /// "循环在跑但取不到帧"（`polls` 大而 `rendered == 0`）。缺了它，
    /// 两种相反的病因在诊断面上都只是"没有画面"。
    pub polls: u64,
    /// 前端**自己看到的**相位（胶囊 DOM 的 `data-phase`）。
    ///
    /// 与 Rust 侧状态机相位对照：不一致 = `widget:state` 事件丢失、
    /// 前端卡在旧相位上（帧循环永久停摆的典型成因）。
    pub phase: String,
    /// 服务端受理上报的时刻（Unix 毫秒，0 = 从未上报）。
    /// 由 Rust 打戳，不信任前端时间——跨进程比对时钟没有意义。
    pub updated_at: u64,
}

impl Default for GlassStatusSnapshot {
    fn default() -> Self {
        Self {
            status: "idle".into(),
            seq: 0,
            frame_w: 0,
            frame_h: 0,
            scale: 0.0,
            detail: String::new(),
            rendered: 0,
            blank: 0,
            render_ms: 0.0,
            polls: 0,
            phase: String::new(),
            updated_at: 0,
        }
    }
}

/// 错误载荷：`code`（稳定英文标识）+ `message`（用户可读中文）+ 可选细节。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: String,
    pub message: String,
    pub detail: Option<serde_json::Value>,
}

impl From<AppError> for CommandError {
    fn from(err: AppError) -> Self {
        let detail = match &err {
            AppError::ApiFailed(code) => Some(serde_json::json!({ "win32": code })),
            AppError::InvalidArgument(s) | AppError::Inconsistent(s) | AppError::Io(s) => {
                Some(serde_json::json!({ "detail": s }))
            }
            _ => None,
        };
        Self {
            code: err.code().to_string(),
            message: err.user_message(),
            detail,
        }
    }
}

impl From<String> for CommandError {
    fn from(msg: String) -> Self {
        Self { code: "ERROR".to_string(), message: msg, detail: None }
    }
}

/// 对配置做一次快照读取（`hotkey.rs` / `window.rs` 等非命令模块复用）。
///
/// 锁竞争失败时退回默认配置，保证热键 / 失焦策略即使读不到配置也不会 panic。
pub fn with_settings<R>(state: &AppState, f: impl FnOnce(&AppSettings) -> R) -> R {
    match state.settings.lock() {
        Ok(guard) => f(&guard),
        Err(_) => f(&AppSettings::default()),
    }
}

// ---------------------------------------------------------------------------
// 对外 DTO（camelCase JSON）
// ---------------------------------------------------------------------------

/// 一台显示器在 UI 中展示所需的最小信息。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MonitorInfo {
    pub key: String,
    pub name: String,
    pub index: u32,
    pub is_primary: bool,
    pub hdr_supported: bool,
    pub hdr_enabled: bool,
    pub bits_per_color: u8,
    pub refresh_hz: f64,
}

impl From<&DisplayState> for MonitorInfo {
    fn from(s: &DisplayState) -> Self {
        Self {
            key: s.target.key.clone(),
            name: s.target.name.clone(),
            index: s.target.index,
            is_primary: s.target.is_primary,
            hdr_supported: s.hdr.supported,
            hdr_enabled: s.hdr.enabled,
            bits_per_color: s.hdr.bits_per_color,
            refresh_hz: s.target.refresh_hz,
        }
    }
}

/// 当前选中显示器的 SDR 亮度读数。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SdrReading {
    pub key: String,
    pub name: String,
    pub percent: u8,
    pub nits: f64,
    pub raw: u32,
    pub hdr_supported: bool,
    pub hdr_enabled: bool,
    pub readable: bool,
}

impl From<&DisplayState> for SdrReading {
    fn from(s: &DisplayState) -> Self {
        Self {
            key: s.target.key.clone(),
            name: s.target.name.clone(),
            percent: s.percent.value,
            nits: s.nits,
            raw: s.raw,
            hdr_supported: s.hdr.supported,
            hdr_enabled: s.hdr.enabled,
            readable: s.readable,
        }
    }
}

/// 诊断信息：API 路线 / 最近错误 / 显示器数量。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Diagnostics {
    pub route: &'static str,
    pub targets: usize,
    pub last_error: Option<String>,
    pub fallback_available: bool,
    /// 背景捕获链路统计（v0.3 新增）。
    pub capture: CaptureStatsSnapshot,
}

// ---------------------------------------------------------------------------
// 命令实现
// ---------------------------------------------------------------------------

/// 枚举全部活动显示器（附带 HDR 状态）。打开面板时调用。
#[tauri::command]
pub fn list_monitors(state: State<AppState>) -> Result<Vec<MonitorInfo>, CommandError> {
    let mut controller = state
        .controller
        .lock()
        .map_err(|_| CommandError::from("控制器锁不可用".to_string()))?;
    let states = controller.refresh().map_err(CommandError::from)?;
    Ok(states.iter().map(MonitorInfo::from).collect())
}

/// 重新枚举显示器并返回最新快照（非命令入口）。
///
/// 与 [`list_monitors`] 的区别：那个是 `#[tauri::command]`，参数由 Tauri 注入，
/// 只能从 webview 调用路径进入。本函数是普通 Rust 入口，供 `monitor_watch`
/// 的桥接线程在收到 `WM_DISPLAYCHANGE` 时直接调用。
///
/// 失败时返回空列表而不是 `Result`：这是后台自愈路径，没有调用方能处理错误，
/// 更不该因为一次枚举失败就把线程打死。
pub fn refresh_monitors(state: &AppState) -> Vec<MonitorInfo> {
    let Ok(mut controller) = state.controller.lock() else {
        return Vec::new();
    };
    match controller.refresh() {
        Ok(states) => states.iter().map(MonitorInfo::from).collect(),
        Err(_) => Vec::new(),
    }
}

/// 读取指定显示器（默认当前选中）的 SDR 内容亮度。
#[tauri::command]
pub fn read_sdr_level(
    key: Option<String>,
    state: State<AppState>,
) -> Result<SdrReading, CommandError> {
    let mut controller = state
        .controller
        .lock()
        .map_err(|_| CommandError::from("控制器锁不可用".to_string()))?;

    let target = resolve_target(&mut controller, &state, key.as_deref())?;
    let raw = hdr_sdr_widget_lib::win32::display::read_sdr_white(&target)
        .map_err(CommandError::from)?;
    let hdr = hdr_sdr_widget_lib::win32::display::read_advanced_color(&target)
        .unwrap_or_default();
    let percent = hdr_sdr_widget_lib::core::convert::raw_to_percent(raw);
    let nits = hdr_sdr_widget_lib::core::convert::raw_to_nits(raw);

    Ok(SdrReading {
        key: target.key.clone(),
        name: target.name.clone(),
        percent: percent.value,
        nits,
        raw,
        hdr_supported: hdr.supported,
        hdr_enabled: hdr.enabled,
        readable: true,
    })
}

/// 写入指定显示器（默认当前选中）的 SDR 内容亮度百分比。
#[tauri::command]
pub fn apply_percent(
    key: String,
    percent: u8,
    state: State<AppState>,
) -> Result<WriteResult, CommandError> {
    if percent > Percent::MAX {
        return Err(CommandError::from(AppError::InvalidArgument(format!(
            "百分比 {percent} 超出 0–100 范围"
        ))));
    }
    if crate::native::enabled() {
        if let Some(result)=crate::native::apply_confirmed(key.clone(),percent) {
            remember_current_key(&state,&key);return Ok(result);
        }
    }
    let mut controller = state
        .controller
        .lock()
        .map_err(|_| CommandError::from("控制器锁不可用".to_string()))?;
    let result = controller.set_percent(&key, Percent::new(percent)).map_err(CommandError::from)?;
    // 记录用户最近操作的显示器（内存 + 落盘）。
    drop(controller);
    remember_current_key(&state, &key);
    Ok(result)
}

/// 一键应用预设（day / movie / night）。作用于当前选中显示器。
#[tauri::command]
pub fn apply_preset(
    preset: String,
    state: State<AppState>,
) -> Result<WriteResult, CommandError> {
    // 取值后**立刻释放**配置锁。
    //
    // 千万别把 guard 留到这个函数末尾：下面 `remember_current_key` 还要再锁一次
    // `state.settings`，而 `std::sync::Mutex` 不可重入 —— 同一线程二次 `lock()`
    // 就是自锁死，整个应用卡住（不是报错，是静默挂起，最难查的那种）。
    // 触发路径很常见：首启后第一次点托盘里的预设。
    let percent = {
        let guard = state
            .settings
            .lock()
            .map_err(|_| CommandError::from("配置锁不可用".to_string()))?;
        guard
            .preset(&preset)
            .map(|p| p.percent)
            .ok_or_else(|| CommandError::from(format!("未知预设：{preset}")))?
    };

    if crate::native::enabled() {
        let key=state.current_key.lock().unwrap().clone().unwrap_or_default();
        if !key.is_empty() {return apply_percent(key,percent,state);}
    }

    let mut controller = state
        .controller
        .lock()
        .map_err(|_| CommandError::from("控制器锁不可用".to_string()))?;
    let key = {
        let cur = state
            .current_key
            .lock()
            .map_err(|_| CommandError::from("状态锁不可用".to_string()))?;
        cur.clone().unwrap_or_default()
    };

    if key.is_empty() {
        // 从未选过显示器：选主显示器 / 第一台。
        let states = controller.refresh().map_err(CommandError::from)?;
        let chosen = states
            .iter()
            .find(|s| s.target.is_primary)
            .or_else(|| states.first())
            .ok_or_else(|| CommandError::from("未枚举到任何活动显示器".to_string()))?
            .target
            .key
            .clone();
        let result = controller
            .set_percent(&chosen, Percent::new(percent))
            .map_err(CommandError::from)?;
        drop(controller);
        remember_current_key(&state, &chosen);
        return Ok(result);
    }

    let result = controller
        .set_percent(&key, Percent::new(percent))
        .map_err(CommandError::from)?;
    drop(controller);
    // 非空分支理论上 key 已经落过盘（它就是从 `current_key` 来的），这里再走一次
    // 是幂等的：`remember_current_key` 内部有相等判断，不会重复写盘。
    remember_current_key(&state, &key);
    Ok(result)
}

/// 读取配置。
#[tauri::command]
pub fn get_settings(state: State<AppState>) -> AppSettings {
    state
        .settings
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// 保存配置。落盘后同步自启状态与热键重注册。
#[tauri::command]
pub fn set_settings(
    app: AppHandle,
    next: AppSettings,
    state: State<AppState>,
) -> Result<(), CommandError> {
    let mut guard = state
        .settings
        .lock()
        .map_err(|_| CommandError::from("配置锁不可用".to_string()))?;
    *guard = next.clone();
    state.store.save(&guard).map_err(CommandError::from)?;
    drop(guard);

    // 自启状态按配置同步（幂等）。
    crate::autostart::sync(next.autostart).map_err(CommandError::from)?;
    // 热键按新配置重注册（空串 = 禁用，v2 默认关）。
    crate::hotkey::reload(&app);
    Ok(())
}

/// 切换面板显示 / 隐藏（托盘左键、热键、前端 Esc 都走这里）。
#[tauri::command]
pub fn toggle_window(app: AppHandle, state: State<AppState>) -> Result<(), CommandError> {
    crate::window::toggle(&app, &state)
        .map(|_| ())
        .map_err(CommandError::from)
}

/// 拖拽移动悬浮物（前端 pointermove 上报位移，rAF 节流 ~60Hz）。
///
/// 前端上报的是**逻辑像素**位移（CSS px），这里按窗口 `scale_factor()`
/// 换算成物理像素再移动——否则 175% 等高 DPI 下拖拽跟手度会按比例偏小。
#[tauri::command]
pub fn move_window(app: AppHandle, dx: i32, dy: i32) -> Result<(), CommandError> {
    let scale = crate::window::window_scale(&app);
    let dx = ((dx as f64) * scale).round() as i32;
    let dy = ((dy as f64) * scale).round() as i32;
    crate::window::move_window(&app, dx, dy).map_err(CommandError::from)
}

/// 松手结束拖拽：贴边吸附 / 越界拉回 / 自由落位 + 位置持久化。
#[tauri::command]
pub fn end_window_drag(app: AppHandle, state: State<AppState>) -> Result<(), CommandError> {
    crate::window::end_window_drag(&app, &state).map_err(CommandError::from)
}

/// 读悬浮物贴边状态（前端初始化时同步一次）。
#[tauri::command]
pub fn get_widget_state(state: State<AppState>) -> crate::edge::WidgetState {
    crate::window::get_widget_state(&state)
}

/// 前端上报指针交互相位（`hovered` / `pressed` / `dragging` / 其它=可见）。
///
/// 状态机唯一真源仍在 Rust：本命令只在「可见 / 交互」相位下被接受，避免
/// 覆盖 Rust 驱动的 `Hidden` / `Revealing` / `Hiding` / `Suppressed`。
#[tauri::command]
pub fn set_pointer_phase(
    app: AppHandle,
    phase: String,
    state: State<AppState>,
) -> Result<(), CommandError> {
    use crate::edge::Phase;
    let cur = crate::window::widget_state(&state);
    let allowed = matches!(
        cur.phase,
        Phase::Visible | Phase::Hovered | Phase::Pressed | Phase::Dragging
    );
    if !allowed {
        return Ok(());
    }
    let next = match phase.as_str() {
        "hovered" => Phase::Hovered,
        "pressed" => Phase::Pressed,
        "dragging" => Phase::Dragging,
        _ => Phase::Visible,
    };
    crate::window::set_widget_state(
        &state,
        crate::edge::WidgetState { phase: next, ..cur },
    );
    crate::window::emit_state(&app, &state);
    Ok(())
}

/// 退出进程（托盘右键菜单「退出」）。
#[tauri::command]
pub fn quit_app(app: AppHandle, state: State<AppState>) -> Result<(), CommandError> {
    // 退出前落盘配置与位置。
    let _ = crate::window::hide(&app, &state);
    if let Ok(guard) = state.settings.lock() {
        let _ = state.store.save(&guard);
    }
    crate::native::stop();
    app.exit(0);
    Ok(())
}

/// 打开系统设置 HDR 页面（PRD P0-9「前往系统设置开启 HDR」）。
#[tauri::command]
pub fn open_hdr_settings(app: AppHandle) -> Result<(), CommandError> {
    use tauri_plugin_opener::OpenerExt;
    let uri = "ms-settings:display-hdr";
    app.opener()
        .open_url(uri, None::<&str>)
        .map_err(|e| CommandError::from(format!("无法打开系统设置：{e}")))
}

/// 复制诊断信息（P0-10：写入失败后提供排障入口）。
#[tauri::command]
pub fn get_diagnostics(state: State<AppState>) -> Diagnostics {
    let controller_guard = state.controller.lock();
    let controller = controller_guard.as_ref().ok();
    let route = controller
        .map(|c| match c.route() {
            WriteRoute::Primary => "primary (SET_SDR_WHITE_LEVEL)",
            WriteRoute::Fallback => "fallback (DwmpSDRToHDRBoost)",
            WriteRoute::ReadOnly => "read-only",
        })
        .unwrap_or("unavailable");
    let targets = controller.map(|c| c.targets().len()).unwrap_or(0);
    let last_error = controller.and_then(|c| c.last_error().map(|e| e.user_message()));
    let fallback_available = hdr_sdr_widget_lib::win32::dwm_fallback::is_available();
    let capture = state
        .capture
        .get()
        .map(|c| c.stats.snapshot())
        .unwrap_or_default();
    Diagnostics { route, targets, last_error, fallback_available, capture }
}

/// 背景捕获链路统计（诊断 HUD 高频轮询用）。
///
/// 单独成一个轻量命令：`get_diagnostics` 会去抢控制器锁（可能触发显示器
/// 枚举），HUD 每 500ms 轮询不能用它。
#[tauri::command]
pub fn get_capture_stats(state: State<AppState>) -> CaptureStatsSnapshot {
    state
        .capture
        .get()
        .map(|c| c.stats.snapshot())
        .unwrap_or_default()
}

/// 拉取一帧（**二进制** IPC）：仅当存在比 `since_seq` 更新的帧时才返回数据。
///
/// 载荷格式（小端）：`seq: u64 | w: u32 | h: u32 | rgba...`，再整体 base64。
/// 无新帧 → 空串。
///
/// # 为什么最终是 base64，而不是 `tauri::ipc::Response`（二进制）
///
/// v0.3.2 改成 `tauri::ipc::Response::new(Vec<u8>)`，以为前端会拿到 `ArrayBuffer`
/// （依据是 tauri 的 `ipc-protocol.js` 里 octet-stream → `response.arrayBuffer()`）。
/// **真机自检（HSDR_STATUS_DUMP）证明不是**：本工程走的是 `ipc.js` 的
/// postMessage(brownfield) 路径（`tauri-2.11.5/src/manager/webview.rs:57`），
/// raw body 到前端变成 **JSON 数字数组** `Array(N)` ——
/// 98KB 帧膨胀成 ~34 万字符（比 base64 的 131KB 还差 2.6 倍），
/// 且 `new DataView(array)` 直接抛 TypeError → 前端每帧报错、`rendered` 恒为 0
/// （表现就是"完全没有背景内容 / 光学效果全没了"）。
/// "大 payload 走 fetch" 那条快路径在 `channel.rs` 里**只对 `tauri::ipc::Channel` 生效**，
/// 普通 command 拿不到。所以退回 base64 —— 本 IPC 配置下代价最低且稳定的选择。
///
/// # 为什么只在新帧时返回数据
///
/// 桌面内容未变化时返回空串，避免每次轮询都传一帧。平均带宽与**桌面变化频率**同阶。
#[tauri::command]
pub fn pull_frame(state: State<AppState>, since_seq: Option<u32>) -> Result<String, CommandError> {
    use std::sync::atomic::Ordering;
    let Some(shared) = state.capture.get() else {
        return Ok(String::new());
    };
    shared.ipc_hits.fetch_add(1, Ordering::Relaxed);

    let since = u64::from(since_seq.unwrap_or(0));
    // 只在**确实有新帧**时才 clone；否则只读一下 seq 就返回空串。
    let fresh = match shared.frame.lock() {
        Ok(g) => match g.as_ref() {
            Some(f) if f.seq > since => Some(f.clone()),
            _ => None,
        },
        Err(_) => None,
    };
    let Some(f) = fresh else {
        return Ok(String::new());
    };
    shared.ipc_frames.fetch_add(1, Ordering::Relaxed);

    // 16 字节帧头 + RGBA，整体 base64。帧头让前端无需对整帧做哈希、
    // 也无需从字节数反推缩放。
    let mut body = Vec::with_capacity(16 + f.rgba.len());
    body.extend_from_slice(&f.seq.to_le_bytes());
    body.extend_from_slice(&f.w.to_le_bytes());
    body.extend_from_slice(&f.h.to_le_bytes());
    body.extend_from_slice(&f.rgba);
    Ok(base64::engine::general_purpose::STANDARD.encode(&body))
}

/// 面板窗口上报玻璃渲染链路状态（v0.3）。
///
/// 由 `liquidGlass.ts` 在每次状态变化时调用（内部已按 ~4Hz 节流）。诊断窗口
/// 是**另一个 webview**，拿不到面板里 `GlassFrameInfo` 的内存对象，必须走
/// 这条 IPC 把渲染端视角搬到 Rust 中转。
///
/// `updated_at` 由本函数打戳，忽略前端传值。
#[tauri::command]
pub fn report_glass_status(
    state: State<AppState>,
    status: GlassStatusSnapshot,
) -> Result<(), CommandError> {
    let mut next = status;
    next.updated_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let mut guard = state
        .glass
        .lock()
        .map_err(|_| CommandError::from("玻璃状态锁失效".to_string()))?;
    *guard = next;
    Ok(())
}

/// 读取前端上报的玻璃渲染链路状态（诊断窗口轮询用）。
#[tauri::command]
pub fn get_glass_status(state: State<AppState>) -> GlassStatusSnapshot {
    state.glass.lock().map(|g| g.clone()).unwrap_or_default()
}

#[tauri::command]
pub fn get_native_diagnostics() -> Option<crate::native::Diagnostics> {
    crate::native::enabled().then(crate::native::diagnostics)
}

/// 生成**无头自检报告**（纯文本），供 `HSDR_STATUS_DUMP` 使用。
///
/// # 为什么需要它
///
/// 悬浮物面板的外观从外部**无法观察**，两条独立原因叠加：
/// ① 窗口带 `WDA_EXCLUDEFROMCAPTURE` → 对一切截屏 API 隐身；
/// ② 即便关掉，WebView2 走 DirectComposition 合成，没有 GDI 重定向表面
///    → 走 `BitBlt` 的截图库（mss 等）同样拍不到内容。
///
/// 于是"界面到底渲染到哪一步"只能靠程序自己报出来。这份报告把三路信号拼在
/// 一起：渲染端自报（面板 webview 经 `report_glass_status` 上报）+ 采集端统计
/// + 窗口几何与相位。三者对不上，就能把故障夹在中间那一段。
///
/// 这是本项目"先建可观测性，再改参数"纪律的一部分 —— 不依赖任何截图能力。
#[must_use]
pub fn build_status_report(app: &tauri::AppHandle) -> String {
    if crate::native::enabled() { return serde_json::to_string_pretty(&crate::native::diagnostics()).unwrap_or_default(); }
    use std::fmt::Write as _;
    use tauri::Manager;

    let state = app.state::<AppState>();
    let mut out = String::new();

    let now = unix_millis();
    let _ = writeln!(out, "HDR SDR 控制器 · 无头自检报告");
    let _ = writeln!(out, "生成时刻(Unix ms): {now}");

    // ---- 渲染端自报 ----
    let _ = writeln!(out, "\n[渲染端] 面板 webview 自报（report_glass_status）");
    match state.glass.lock() {
        Ok(g) => {
            let age = if g.updated_at == 0 {
                "从未上报".to_string()
            } else {
                format!("{} ms 前", now.saturating_sub(g.updated_at))
            };
            let _ = writeln!(out, "  status      = {}", g.status);
            let _ = writeln!(out, "  detail      = {}", g.detail);
            let _ = writeln!(
                out,
                "  单帧耗时    = {:.1} ms（滚动平均：模糊+裁剪+折射）",
                g.render_ms
            );
            let _ = writeln!(out, "  polls       = {} 次取帧请求", g.polls);
            let _ = writeln!(
                out,
                "  前端相位    = {:?}   ← 与下面 [窗口] 的 Rust 相位对照；不一致 = 事件丢失",
                g.phase
            );
            let _ = writeln!(out, "  seq         = {}", g.seq);
            let _ = writeln!(out, "  frame       = {}×{}  scale={:.3}", g.frame_w, g.frame_h, g.scale);
            let _ = writeln!(out, "  rendered    = {} 张已上屏", g.rendered);
            let _ = writeln!(out, "  blank       = {} 张被判空白", g.blank);
            let _ = writeln!(out, "  最后上报    = {age}");
        }
        Err(_) => {
            let _ = writeln!(out, "  <玻璃状态锁已中毒，无法读取>");
        }
    }

    // ---- 采集端 ----
    let _ = writeln!(out, "\n[采集端] DDA 统计（get_capture_stats）");
    match state.capture.get() {
        Some(c) => {
            let s = c.stats.snapshot();
            let _ = writeln!(out, "  会话 建立/失败 = {}/{}", s.session_created, s.session_failed);
            let _ = writeln!(out, "  帧  取得/空/超时 = {}/{}/{}", s.frame_acquired, s.frame_empty, s.frame_timeout);
            let _ = writeln!(out, "  ACCESS_LOST   = {}", s.access_lost);
            let _ = writeln!(out, "  拷贝 成功/失败 = {}/{}", s.copy_ok, s.copy_error);
            let _ = writeln!(out, "  不支持的格式  = {}", s.unsupported_format);
            let _ = writeln!(out, "  最近格式/错误 = {} / {:#010x}", s.last_format, s.last_error);
            let _ = writeln!(out, "  最近区域      = {}×{}", s.region_w, s.region_h);
            let _ = writeln!(out, "  持有帧        = {}", s.has_frame);
            // 协议通路计数：这三个数 + 上面的 has_frame 一起，才能判定
            // "前端一直拿到 204"到底是"请求没到"、"路径不对"还是"槽里没帧"。
            let _ = writeln!(
                out,
                "  协议请求/200/204 = {} / {} / {}",
                c.proto_hits.load(std::sync::atomic::Ordering::Relaxed),
                c.proto_200.load(std::sync::atomic::Ordering::Relaxed),
                c.proto_204.load(std::sync::atomic::Ordering::Relaxed),
            );
            let last_path = c
                .proto_last_path
                .lock()
                .map(|p| p.clone())
                .unwrap_or_else(|_| "<锁中毒>".to_string());
            let _ = writeln!(out, "  协议最近路径  = {last_path:?}（期望 \"/frame\"）");
            // IPC 通道计数：与上面的协议计数对照即可看出当前走的是哪条通道、
            // 以及它是否还在被调用（"停了"就说明前端又冻住了）。
            let _ = writeln!(
                out,
                "  IPC 取帧/送帧 = {} / {}",
                c.ipc_hits.load(std::sync::atomic::Ordering::Relaxed),
                c.ipc_frames.load(std::sync::atomic::Ordering::Relaxed),
            );
        }
        None => {
            let _ = writeln!(out, "  <尚未挂载（setup 还未把 CaptureShared 放进 AppState）>");
        }
    }

    // ---- 窗口与相位 ----
    let _ = writeln!(out, "\n[窗口] 面板");
    let ws = crate::window::widget_state(&state);
    let _ = writeln!(out, "  phase       = {:?}", ws.phase);
    let _ = writeln!(out, "  docked      = {:?}", ws.docked);
    let _ = writeln!(out, "  expanded    = {}", ws.expanded);
    match app.get_window(crate::window::PANEL_LABEL) {
        Some(w) => {
            let _ = writeln!(out, "  is_visible  = {:?}", w.is_visible().map_err(|e| e.to_string()));
            let _ = writeln!(
                out,
                "  position    = {:?}",
                w.outer_position().map(|p| (p.x, p.y)).map_err(|e| e.to_string())
            );
            let _ = writeln!(
                out,
                "  size        = {:?}",
                w.outer_size().map(|s| (s.width, s.height)).map_err(|e| e.to_string())
            );
            let _ = writeln!(out, "  scale_factor= {:?}", w.scale_factor().map_err(|e| e.to_string()));
            // 尺寸对账：`outer_size` 与"按 DIP×缩放算出来的期望物理尺寸"应当一致。
            // 不一致就说明窗口没按配置尺寸创建（或某处把 DIP 当物理像素传了），
            // 而前端用 `frame_w / 68` 反推缩放系数，尺寸错 → 系数错 → 裁剪区域整体偏移。
            let sf = w.scale_factor().unwrap_or(1.0);
            let g = hdr_sdr_widget_lib::win32::geometry::WINDOW_W as f64 * sf;
            let gh = hdr_sdr_widget_lib::win32::geometry::WINDOW_H as f64 * sf;
            let _ = writeln!(
                out,
                "  期望物理尺寸= {}×{}（= {}×{} DIP × {sf}）",
                g.round(),
                gh.round(),
                hdr_sdr_widget_lib::win32::geometry::WINDOW_W,
                hdr_sdr_widget_lib::win32::geometry::WINDOW_H,
            );
        }
        None => {
            let _ = writeln!(out, "  <取不到面板窗口>");
        }
    }

    out
}

/// 当前 Unix 毫秒。
fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// 诊断窗口的窗口标签，供 `main.rs` 与托盘共用。
pub const DIAG_LABEL: &str = "diag";

/// 打开（或聚焦）诊断窗口。
///
/// FIX-PLAN 阶段 0 的交付物：在能看见"帧到底有没有效"之前，任何光学调参都是
/// 猜。诊断窗口独立于悬浮物窗口，不参与贴边状态机，也不注册 `WDA_EXCLUDEFROMCAPTURE`
/// 之外的特殊处理——它本身是普通窗口，被捕获到不影响玻璃取样区（取样区在屏右缘）。
#[tauri::command]
pub fn open_diag_window(app: AppHandle) -> Result<(), CommandError> {
    use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

    if let Some(win) = app.get_webview_window(DIAG_LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }

    WebviewWindowBuilder::new(&app, DIAG_LABEL, WebviewUrl::App("diag.html".into()))
        // 必须与 panel 一致（见 window::BROWSER_ARGS 的说明）
        .additional_browser_args(crate::window::BROWSER_ARGS)
        .title("HDR SDR 诊断面板")
        .inner_size(600.0, 680.0)
        .min_inner_size(420.0, 360.0)
        .resizable(true)
        .decorations(true)
        .transparent(false)
        .always_on_top(false)
        .skip_taskbar(false)
        .build()
        .map_err(|e| CommandError::from(format!("无法打开诊断窗口：{e}")))?;
    Ok(())
}

/// 关闭诊断窗口（不存在时静默成功）。
#[tauri::command]
pub fn close_diag_window(app: AppHandle) -> Result<(), CommandError> {
    use tauri::Manager;
    if let Some(win) = app.get_webview_window(DIAG_LABEL) {
        let _ = win.close();
    }
    Ok(())
}

/// Toast 窗口的窗口标签。
pub const TOAST_LABEL: &str = "toast";

/// 一条 Toast 的载荷。
///
/// 同时经两条路发给前端：事件 `toast:item`，以及 [`get_toast_payload`] 命令。
/// 这是刻意的冗余 —— 原因见 [`show_toast`] 的说明。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToastPayload {
    /// 自增序号。前端据此去重，避免"事件 + 拉取"把同一条渲染两次。
    pub id: u64,
    pub message: String,
    /// `info` / `warn` / `error`。
    pub level: String,
    pub duration_ms: u64,
}

/// Toast 窗口的逻辑尺寸（宽 × 高），与 `toastWin.css` 保持一致。
const TOAST_W: f64 = 320.0;
const TOAST_H: f64 = 64.0;
/// Toast 与胶囊之间的间距（逻辑像素）。
const TOAST_GAP: f64 = 10.0;

/// 弹出一条 Toast（独立窗口）。
///
/// # 为什么不是窗口内提示
///
/// 悬浮物窗口只有 68×228 逻辑像素，且 `.capsule` 带 `overflow: hidden`。
/// 在里面塞 9px 字号、40px 宽的提示条，一行放不下六个汉字 —— 这个界面
/// "看起来实现了"，实际一个字都读不了。所以提示必须有自己的显示面。
///
/// 同一条 Toast 反复弹出时只更新内容不重建窗口（`duration_ms` 重新计时由
/// 前端负责）。
#[tauri::command]
pub fn show_toast(
    app: AppHandle,
    message: String,
    level: Option<String>,
    duration_ms: Option<u64>,
) -> Result<(), CommandError> {
    use tauri::{Emitter, Manager, PhysicalPosition, WebviewUrl, WebviewWindowBuilder};

    let level = level.unwrap_or_else(|| "info".to_string());
    let duration = duration_ms.unwrap_or(4200);

    let win = match app.get_webview_window(TOAST_LABEL) {
        Some(w) => w,
        None => WebviewWindowBuilder::new(
            &app,
            TOAST_LABEL,
            WebviewUrl::App("toast.html".into()),
        )
        // 必须与 panel 一致（见 window::BROWSER_ARGS 的说明）
        .additional_browser_args(crate::window::BROWSER_ARGS)
        .title("HDR SDR 提示")
        .inner_size(TOAST_W, TOAST_H)
        .resizable(false)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .visible(false)
        .build()
        .map_err(|e| CommandError::from(format!("无法创建 Toast 窗口：{e}")))?,
    };

    // 贴着胶囊放：优先放在胶囊上方，上方放不下就放下方；水平中心对齐胶囊。
    if let Ok(panel) = crate::window::panel(&app) {
        if let (Ok(pos), Ok(size), Ok(scale)) =
            (panel.outer_position(), panel.outer_size(), panel.scale_factor())
        {
            // 缩放系数取**胶囊所在显示器**的（即 `scale`），不要问 Toast 窗口自己。
            //
            // 原因：Toast 窗口此刻还没 show()，`win.scale_factor()` 返回的是它被创建
            // 时所在显示器（主屏）的值。跨 DPI 多屏下（例如主屏 100%、胶囊在 175% 屏）
            // 会拿错缩放，Toast 的尺寸与定位一起偏掉。而 Toast 本来就放在胶囊旁边，
            // 胶囊的缩放系数就是目标显示器的缩放系数。
            let sx = scale;
            let tw = (TOAST_W * sx).round() as i32;
            let th = (TOAST_H * sx).round() as i32;
            let gap = (TOAST_GAP * sx).round() as i32;
            let m = hdr_sdr_widget_lib::win32::geometry::monitor_work_area(pos.x, pos.y)
                .or_else(hdr_sdr_widget_lib::win32::geometry::primary_work_area)
                .unwrap_or_default();

            // 贴边隐藏态的窗口有一大半在屏幕外，直接用窗口中心会把 Toast 推到
            // 屏幕角落去。所以先把中心点钳进工作区，再以它为中心 —— 结果是
            // "贴着胶囊露出的那侧屏幕边缘"，符合直觉。
            let half = (tw / 2).min(m.width() / 2).max(0);
            let cap_cx = (pos.x + size.width as i32 / 2).clamp(m.left + half, m.right - half);
            let x = cap_cx - tw / 2;

            // 上方空间不足 → 落到胶囊下方；两者都放不下就钳进工作区。
            let want_y = pos.y - th - gap;
            let y = if want_y >= m.top {
                want_y
            } else {
                (pos.y + size.height as i32 + gap).min(m.bottom - th)
            };
            let _ = win.set_position(PhysicalPosition::new(x, y.max(m.top)));
        }
    }

    // 载荷入槽 + 自增世代号（供旧定时器让位）。
    let payload = {
        let state = app.state::<AppState>();
        let mut gen = state
            .toast_gen
            .lock()
            .map_err(|_| CommandError::from("Toast 状态锁失效".to_string()))?;
        *gen = gen.wrapping_add(1);
        let payload = ToastPayload {
            id: *gen,
            message,
            level,
            duration_ms: duration,
        };
        drop(gen);
        if let Ok(mut slot) = state.last_toast.lock() {
            *slot = Some(payload.clone());
        }
        payload
    };

    let _ = win.show();
    let _ = app.emit_to(TOAST_LABEL, "toast:item", payload.clone());

    if duration > 0 {
        let gen = payload.id;
        let handle = app.clone();
        std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(duration));
            // 期间又弹了新 Toast 就别关（世代号不匹配）。
            let state = handle.state::<AppState>();
            let current = state.toast_gen.lock().map(|g| *g).unwrap_or(gen);
            if current != gen {
                return;
            }
            if let Some(w) = handle.get_webview_window(TOAST_LABEL) {
                let _ = w.hide();
            }
            // 同 hide_toast_window：自动收起时也要清载荷槽，否则窗口的 webview
            // 之后被重挂/重载，挂载补拉会把这条已经收起的旧提示又显示一遍。
            clear_last_toast(&state);
        });
    }
    Ok(())
}

/// 读取当前 Toast 载荷。
///
/// Toast 窗口挂载时拉一次，补上"窗口首次创建时事件早于 listen 注册"的竞态。
#[tauri::command]
pub fn get_toast_payload(state: State<AppState>) -> Option<ToastPayload> {
    state.last_toast.lock().ok().and_then(|g| g.clone())
}

/// 清空 Toast 载荷槽（收起提示时必须调用）。
///
/// 抽成独立函数而不是在两处内联，是因为内联有个真实的借用坑：若把它写成闭包的
/// **尾表达式** —— `if let Ok(mut slot) = state.last_toast.lock() { .. }` ——
/// `if let` 的临时值 `Result<MutexGuard<..>>` 会活到闭包块结束，而局部
/// `state: State<AppState>` 先被析构，编译报 E0597「`state` dropped here while
/// still borrowed」。作为普通函数调用时临时值在函数体内即析构，天然规避。
fn clear_last_toast(state: &AppState) {
    if let Ok(mut slot) = state.last_toast.lock() {
        *slot = None;
    }
}

/// 立刻收起 Toast 窗口（用户点击 Toast 时调用）。
#[tauri::command]
pub fn hide_toast_window(app: AppHandle) -> Result<(), CommandError> {
    use tauri::Manager;
    if let Some(win) = app.get_webview_window(TOAST_LABEL) {
        let _ = win.hide();
    }
    // 清掉载荷槽。不清的话，Toast 窗口的 webview 之后一旦被重挂 / 重载，
    // 挂载时会调 `get_toast_payload` 补拉，把这条**早已收起**的旧提示又显示一遍
    // （表现为"莫名闪回一条旧提示"）。窗口本身不销毁，所以平时看不出来。
    let state = app.state::<AppState>();
    clear_last_toast(&state);
    Ok(())
}

// ---------------------------------------------------------------------------
// 内部辅助
// ---------------------------------------------------------------------------

/// 解析要操作的目标显示器：优先显式 key，其次当前选中，最后主/第一台。
fn resolve_target(
    controller: &mut RealController,
    state: &AppState,
    explicit_key: Option<&str>,
) -> Result<DisplayTarget, CommandError> {
    if let Some(key) = explicit_key {
        if !key.is_empty() {
            return controller.resolve(key).map_err(CommandError::from);
        }
    }
    let key = state
        .current_key
        .lock()
        .ok()
        .and_then(|g| g.clone())
        .unwrap_or_default();
    if !key.is_empty() {
        if let Ok(t) = controller.resolve(&key) {
            return Ok(t);
        }
    }
    // 兜底：主显示器 / 第一台。
    let states = controller.refresh().map_err(CommandError::from)?;
    let target = states
        .iter()
        .find(|s| s.target.is_primary)
        .or_else(|| states.first())
        .ok_or_else(|| CommandError::from("未枚举到任何活动显示器".to_string()))?;
    Ok(target.target.clone())
}
