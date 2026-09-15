//! Desktop Duplication API 桌面捕获：抓取悬浮物窗口**后方**的真实桌面内容。
//!
//! 用途：Liquid Glass 背景采样。悬浮物窗口透明，`backdrop-filter` 只能采样到
//! WebView 内部合成内容，采不到 OS 桌面上其它应用；因此用 DDA 把「窗口矩形
//! 覆盖的桌面区域」抓成帧，**并在读回时把 B/R 换位成 RGBA**，前端可以直接
//! 当 `ImageData` 用，把它当作玻璃的 backdrop 输入。
//!
//! 防反馈回路：悬浮物窗口已调用 [`geometry::apply_exclude_from_capture`]
//! （`WDA_EXCLUDEFROMCAPTURE`），DDA 合成的桌面图像里不含悬浮物自身像素。
//!
//! # 两个必须遵守的 DDA 语义（v0.3 修复，此前踩坑的根因）
//!
//! 1. **`LastPresentTime == 0` 的帧内容未定义**。创建/重建复制会话后的第一次
//!    `AcquireNextFrame` 常常立即返回且 `LastPresentTime == 0`，此时桌面图像
//!    可能是全 0。旧实现无条件接受该帧，导致每次唤出都把一张空帧写进共享槽；
//!    若此刻桌面静止，后续不再有新帧到达，**整个可见期玻璃都采样到空帧**
//!    （表现为「完全没有光学效果」或「不透明黑块」）。
//!    现在必须循环到 `LastPresentTime != 0` 才算拿到真帧。
//! 2. **`DXGI_OUTDUPL_DESC.ModeDesc.Format` 是「源格式」，不是「交付格式」**。
//!    HDR 开启时它报 `R16G16B16A16_FLOAT`，但实际交付的纹理是
//!    `B8G8R8A8_UNORM`（DXGI 已代为转换）。**必须查询交付纹理自身的
//!    `ID3D11Texture2D::GetDesc().Format`**，不能信 `ModeDesc`。
//!
//! 另外 `CopySubresourceRegion` **不做格式转换**（源/目标必须同格式族），
//! 且 windows crate 的封装不返回 HRESULT —— 非法调用会被静默丢弃。
//! 因此本模块一律在拷贝前显式校验格式，并在拷贝后用统计计数确认产出非空。
//!
//! 性能：抓取区域约 119×399 物理像素（68×228 DIP @175%），静止时降频；
//! 帧内容不变时不更新共享槽。所有 `unsafe` 收敛于本文件。

use std::sync::atomic::{AtomicBool, AtomicI32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde::Serialize;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_MAP_WRITE, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
};

use super::geometry::Rect;

/// 单次 `AcquireNextFrame` 的超时（毫秒）。
///
/// 会话刚建立/重建、尚未持有有效帧时用较长的超时（见 [`FIRST_FRAME_TIMEOUT_MS`]），
/// 稳态下用短超时避免拖慢轮询。
const STEADY_TIMEOUT_MS: u32 = 8;
/// 首次取有效帧的超时（毫秒）：给 DXGI 时间产出创建会话后的第一张真实桌面图。
const FIRST_FRAME_TIMEOUT_MS: u32 = 120;
/// 首次取有效帧的最大尝试次数（跳过 `LastPresentTime == 0` 的空帧）。
const FIRST_FRAME_MAX_TRIES: u32 = 8;

/// 一帧捕获结果（**RGBA** 字节序，尺寸 = 抓取区域物理尺寸）。
///
/// 注意字节序：桌面纹理是 `B8G8R8A8_UNORM`，[`CaptureContext::capture_region`]
/// 在逐行读回时**已把 B/R 换位**，所以这里的四元组是 `R,G,B,A`。
/// 前端可以直接把它当 `ImageData` 用，不需要再换一次 —— 在两端各换一次
/// 等于没换，而且颜色错得很像"色调不对"，极易被误判成光学问题。
#[derive(Clone)]
pub struct CaptureFrame {
    /// 内容变化时自增（前端据此判断"是否新帧"）。
    pub seq: u64,
    /// 帧宽（物理像素）。
    pub w: u32,
    /// 帧高（物理像素）。
    pub h: u32,
    /// 像素数据，字节数恒为 `w * h * 4`，四元组顺序为 `R,G,B,A`。
    pub rgba: Vec<u8>,
}

impl CaptureFrame {
    /// 该帧是否可能含有真实内容（全 0 视为无效）。
    ///
    /// 兜底用：即使 `LastPresentTime != 0`，极端情况下仍可能拿到空图。
    #[must_use]
    pub fn is_blank(&self) -> bool {
        self.rgba.iter().all(|b| *b == 0)
    }
}

/// 抓取配置（由 edge 线程随状态更新）。
#[derive(Clone, Copy, Default)]
pub struct CaptureConfig {
    /// 抓取区域（虚拟屏物理像素）；`None` = 暂停抓取（**保留已持有的帧**）。
    pub region: Option<Rect>,
    /// 抓取间隔（毫秒）：静止 100 / 交互 33。
    pub interval_ms: u64,
}

/// 捕获链路运行统计。
///
/// 存在的理由：旧实现把所有错误 `Err(_) => {}` 吞掉，导致「玻璃没效果」
/// 这类问题在系统里**没有任何可观测信号**，只能靠盲猜（历史上连续 8 轮
/// 光学修复全部无效，根因就在这里）。这些计数经 `get_capture_stats`
/// 命令暴露给前端诊断面板。
#[derive(Default)]
pub struct CaptureStats {
    /// 成功创建复制会话次数。
    pub session_created: AtomicU64,
    /// 创建复制会话失败次数（常见：被录屏/采集程序占用、模式不支持）。
    pub session_failed: AtomicU64,
    /// 取到 `LastPresentTime != 0` 的真实帧次数。
    pub frame_acquired: AtomicU64,
    /// 取到 `LastPresentTime == 0` 的空帧次数（已跳过）。
    pub frame_empty: AtomicU64,
    /// `AcquireNextFrame` 超时次数（桌面无变化）。
    pub frame_timeout: AtomicU64,
    /// 复制会话失效次数（分辨率/模式切换、全屏独占切换等）。
    pub access_lost: AtomicU64,
    /// 成功拷贝进 staging 的帧数。
    pub copy_ok: AtomicU64,
    /// 拷贝失败/被跳过的帧数（含格式不符）。
    pub copy_error: AtomicU64,
    /// 交付帧格式与预期不符的次数。
    pub unsupported_format: AtomicU64,
    /// 最近一次交付帧的 `DXGI_FORMAT` 原始值。
    pub last_format: AtomicI32,
    /// 最近一次失败的 HRESULT（0 = 无）。
    pub last_error: AtomicI32,
    /// 当前抓取区域宽（物理像素，0 = 未抓取）。
    pub region_w: AtomicI32,
    /// 当前抓取区域高。
    pub region_h: AtomicI32,
    /// 共享槽当前是否持有帧。
    pub has_frame: AtomicBool,
}

/// 统计快照（IPC 契约，camelCase）。
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureStatsSnapshot {
    pub session_created: u64,
    pub session_failed: u64,
    pub frame_acquired: u64,
    pub frame_empty: u64,
    pub frame_timeout: u64,
    pub access_lost: u64,
    pub copy_ok: u64,
    pub copy_error: u64,
    pub unsupported_format: u64,
    pub last_format: i32,
    pub last_error: i32,
    pub region_w: i32,
    pub region_h: i32,
    pub has_frame: bool,
}

impl CaptureStats {
    /// 生成快照（Relaxed 读取；诊断用途无需严格一致）。
    #[must_use]
    pub fn snapshot(&self) -> CaptureStatsSnapshot {
        CaptureStatsSnapshot {
            session_created: self.session_created.load(Ordering::Relaxed),
            session_failed: self.session_failed.load(Ordering::Relaxed),
            frame_acquired: self.frame_acquired.load(Ordering::Relaxed),
            frame_empty: self.frame_empty.load(Ordering::Relaxed),
            frame_timeout: self.frame_timeout.load(Ordering::Relaxed),
            access_lost: self.access_lost.load(Ordering::Relaxed),
            copy_ok: self.copy_ok.load(Ordering::Relaxed),
            copy_error: self.copy_error.load(Ordering::Relaxed),
            unsupported_format: self.unsupported_format.load(Ordering::Relaxed),
            last_format: self.last_format.load(Ordering::Relaxed),
            last_error: self.last_error.load(Ordering::Relaxed),
            region_w: self.region_w.load(Ordering::Relaxed),
            region_h: self.region_h.load(Ordering::Relaxed),
            has_frame: self.has_frame.load(Ordering::Relaxed),
        }
    }
}

/// 捕获线程与调用方共享的状态。
pub struct CaptureShared {
    pub config: Mutex<CaptureConfig>,
    pub frame: Mutex<Option<CaptureFrame>>,
    pub stats: CaptureStats,
    pub stop: Arc<AtomicBool>,
    /// `glass://` 协议处理器被调用的次数。
    ///
    /// 诊断用：区分"前端压根没发请求"与"请求到了但被拒"。
    pub proto_hits: AtomicU64,
    /// 协议处理器返回 `204`（无内容）的次数。
    ///
    /// 与服务端 `has_frame` 对照即可定位 —— 若 `has_frame == true` 而
    /// `proto_204` 持续增长，说明**槽里有帧却没被发出去**，问题在路径匹配
    /// 或槽读取，而不在采集端。
    pub proto_204: AtomicU64,
    /// 协议处理器成功构造出 `200` 响应的次数。
    ///
    /// 与 `proto_hits` 对照的关键用途：`hits` 增加而 `200 + 204` 不增加，
    /// 说明某次请求**进得来出不去**（卡在处理器内部）；
    /// 若 `200` 有值而前端仍收不到帧，则问题在响应体传输（Tauri/WebView2 侧）。
    pub proto_200: AtomicU64,
    /// 最近一次请求的路径（诊断用，直接暴露路径解析结果）。
    pub proto_last_path: Mutex<String>,
    /// IPC 取帧命令被调用的次数。
    ///
    /// 与 `proto_hits` 并列，用来对照两条传输通道的可用性 —— `glass://` 协议
    /// 在真机上约十几次响应后会让 WebView2 渲染进程的 JS 整体停止
    /// （见 FIX-PLAN §4.0.2.3），故新增 IPC 通道并保留两边计数做对照。
    pub ipc_hits: AtomicU64,
    /// IPC 通道实际送出帧的次数（即"有新帧"的次数）。
    pub ipc_frames: AtomicU64,
}

impl CaptureShared {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            // 采集间隔 16ms ≈ 60fps。
            //
            // ⚠️ 这个值**就是玻璃跟随桌面的上限** —— 前端问得再勤也没用：后端不产出
            // 新 `seq`，前端拿到的永远是"没有新帧"。曾经是 150ms（6.7fps）→ 33ms → 现在 16ms，
            // 用户实测反馈"采样太低导致非常卡顿"，问题就在这一行，不在前端。
            //
            // 代价可控：单帧 119×399×4 ≈ 190KB，60fps 即约 11.4MB/s 的拷贝；
            // 且**只在内容变化时**才写共享槽（见下方 `rgba != last_buf`），
            // 静态桌面依然是零传输。
            config: Mutex::new(CaptureConfig { region: None, interval_ms: 16 }),
            frame: Mutex::new(None),
            stats: CaptureStats::default(),
            stop: Arc::new(AtomicBool::new(false)),
            proto_hits: AtomicU64::new(0),
            proto_204: AtomicU64::new(0),
            proto_200: AtomicU64::new(0),
            proto_last_path: Mutex::new(String::new()),
            ipc_hits: AtomicU64::new(0),
            ipc_frames: AtomicU64::new(0),
        })
    }
}

/// 捕获线程句柄。持有时线程运行；`Drop` 时请求退出并 join。
pub struct CaptureHandle {
    thread: Option<JoinHandle<()>>,
    /// 与捕获线程共享的停止标志。
    ///
    /// `Drop` 必须**先置位再 join**：线程主循环的条件是
    /// `while !stop.load(..)`，只 join 不置位就是 join 一个永不退出的线程 ——
    /// 进程退出时会卡死在状态析构上（Tauri `app.exit()` 走的就是这条路径）。
    stop: Arc<AtomicBool>,
}

impl Drop for CaptureHandle {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// 请求捕获线程退出（不阻塞）。
///
/// 供 `CaptureHandle` 之外的持有者（如诊断命令）主动停流时使用。
pub fn request_stop(shared: &CaptureShared) {
    shared.stop.store(true, Ordering::Relaxed);
}

/// 启动捕获线程。
pub fn spawn(shared: Arc<CaptureShared>) -> CaptureHandle {
    let stop = Arc::clone(&shared.stop);
    let thread = std::thread::Builder::new()
        .name("backdrop-capture".to_string())
        .spawn(move || run(shared))
        .expect("无法创建背景捕获线程");
    CaptureHandle { thread: Some(thread), stop }
}

/// 线程主循环。
fn run(shared: Arc<CaptureShared>) {
    let stats = &shared.stats;
    // 初始化失败（如无可用显示器、D3D 设备创建失败）时低频重试，不空转。
    let mut ctx = loop {
        match CaptureContext::init() {
            Ok(c) => break c,
            Err(e) => {
                stats.session_failed.fetch_add(1, Ordering::Relaxed);
                stats.last_error.store(e.hr(), Ordering::Relaxed);
                if shared.stop.load(Ordering::Relaxed) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1000));
            }
        }
    };
    ctx.attach_stats(stats);
    let mut last_buf: Vec<u8> = Vec::new();
    let mut seq: u64 = 0;

    while !shared.stop.load(Ordering::Relaxed) {
        let cfg = *shared.config.lock().unwrap();
        let Some(region) = cfg.region else {
            // 暂停抓取：**保留已持有的帧**。
            //
            // 旧实现在这里把帧清空，导致「隐藏 → 唤出」每次都从零开始捕获，
            // 而重建会话后的第一帧是未定义内容 → 每次唤出都重演一次空帧。
            // 保留上一张有效帧后，同一位置的唤出可以立即呈现正确背景。
            stats.region_w.store(0, Ordering::Relaxed);
            stats.region_h.store(0, Ordering::Relaxed);
            std::thread::sleep(Duration::from_millis(150));
            continue;
        };
        if region.is_empty() {
            std::thread::sleep(Duration::from_millis(cfg.interval_ms.max(1)));
            continue;
        }
        stats.region_w.store(region.width().max(0), Ordering::Relaxed);
        stats.region_h.store(region.height().max(0), Ordering::Relaxed);

        match ctx.capture_region(region) {
            Ok(Some(rgba)) => {
                if rgba != last_buf {
                    last_buf = rgba.clone();
                    seq += 1;
                    if let Ok(mut f) = shared.frame.lock() {
                        *f = Some(CaptureFrame {
                            seq,
                            w: region.width().max(0) as u32,
                            h: region.height().max(0) as u32,
                            rgba,
                        });
                    }
                    stats.has_frame.store(true, Ordering::Relaxed);
                }
            }
            Ok(None) => {}
            Err(CaptureError::AccessLost) => {
                stats.access_lost.fetch_add(1, Ordering::Relaxed);
                let _ = ctx.recreate();
            }
            Err(e) => {
                stats.last_error.store(e.hr(), Ordering::Relaxed);
            }
        }
        std::thread::sleep(Duration::from_millis(cfg.interval_ms.max(1)));
    }
}

// ---------------------------------------------------------------------------
// D3D11 / DXGI 上下文
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CaptureError {
    /// 携带 HRESULT 的失败（0 = 无具体错误码）。
    Hr(i32),
    AccessLost,
    /// 交付帧的像素格式本模块尚不支持。
    UnsupportedFormat(i32),
}

impl CaptureError {
    fn hr(&self) -> i32 {
        match self {
            Self::Hr(v) => *v,
            Self::AccessLost => DXGI_ERROR_ACCESS_LOST.0,
            Self::UnsupportedFormat(f) => *f,
        }
    }
}

struct CaptureContext {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    /// 全部输出（桌面矩形 + 输出句柄）。
    outputs: Vec<OutputInfo>,
    /// 当前复制会话。
    dup: Option<ActiveDup>,
    /// 抓取用 staging 纹理。
    staging: Option<ID3D11Texture2D>,
    staging_size: (u32, u32),
    /// 本次会话是否已拿到过有效帧。
    ///
    /// 未拿到时用更长的超时和多次重试跳过 `LastPresentTime == 0` 的空帧。
    have_valid: bool,
    /// 诊断统计（由 run 注入；缺失时静默跳过计数）。
    stats: Option<*const CaptureStats>,
}

struct OutputInfo {
    desktop: Rect,
    output: IDXGIOutput,
}

struct ActiveDup {
    desktop: Rect,
    dup: IDXGIOutputDuplication,
    w: u32,
    h: u32,
}

impl CaptureContext {
    fn attach_stats(&mut self, stats: &CaptureStats) {
        // SAFETY: `stats` 的生命周期长于本上下文（由 Arc<CaptureShared> 持有，
        // 且 run 的整个循环期间都存在）。此处仅作诊断计数的裸指针共享。
        self.stats = Some(stats as *const CaptureStats);
    }

    fn count(&self, f: impl FnOnce(&CaptureStats)) {
        if let Some(p) = self.stats {
            // SAFETY: 见 attach_stats 的说明。
            f(unsafe { &*p });
        }
    }

    fn init() -> Result<Self, CaptureError> {
        let factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }.map_err(|e| CaptureError::Hr(e.code().0))?;

        // 枚举所有适配器，找第一个「接了显示器（有输出）」的适配器，并在其上创建
        // D3D11 设备。多 GPU 机器（集显 + 独显）下显示器通常只接独显；若固定用
        // `EnumAdapters1(0)`（往往是集显、无输出）或让设备落到另一个适配器，
        // `DuplicateOutput(device)` 会因 device 与 output 不同适配器而失败。
        // 实测本机：适配器 0 = RTX 3070（有输出），1 = UHD 770（无输出）。
        let feature_levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut adapter_idx = 0u32;
        loop {
            let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_idx) } {
                Ok(a) => a,
                Err(_) => break, // 枚举完毕
            };
            adapter_idx += 1;

            let mut outputs = Vec::new();
            let mut i = 0u32;
            loop {
                // SAFETY: EnumOutputs 只读枚举。
                let out = match unsafe { adapter.EnumOutputs(i) } {
                    Ok(o) => o,
                    Err(_) => break,
                };
                // SAFETY: desc 为值返回。
                let desc: DXGI_OUTPUT_DESC = unsafe { out.GetDesc() }
                    .map_err(|e| CaptureError::Hr(e.code().0))?;
                outputs.push(OutputInfo {
                    desktop: dxgi_rect_to_rect(desc.DesktopCoordinates),
                    output: out,
                });
                i += 1;
            }
            if outputs.is_empty() {
                continue; // 无显示器输出（集显），试下一个适配器
            }

            // 显式指定 pAdapter 时 DriverType 必须为 D3D_DRIVER_TYPE_UNKNOWN，
            // 否则 D3D11CreateDevice 返回 E_INVALIDARG(0x80070057)。
            let adapter_base: IDXGIAdapter =
                adapter.cast().map_err(|e| CaptureError::Hr(e.code().0))?;
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            // SAFETY: 输出指针均指向栈上 Option，由 windows crate 写入。
            unsafe {
                D3D11CreateDevice(
                    Some(&adapter_base),
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&feature_levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            }
            .map_err(|e| CaptureError::Hr(e.code().0))?;

            let Some(device) = device else { continue };
            let Some(context) = context else { continue };

            return Ok(Self {
                device,
                context,
                outputs,
                dup: None,
                staging: None,
                staging_size: (0, 0),
                have_valid: false,
                stats: None,
            });
        }

        Err(CaptureError::Hr(0))
    }

    /// 丢弃当前复制会话（下次抓取时重建）。
    ///
    /// 同时重置 `have_valid`：重建后的第一帧内容未定义，必须重新走
    /// 「跳过空帧」的流程。
    fn recreate(&mut self) -> Result<(), CaptureError> {
        self.dup = None;
        self.have_valid = false;
        Ok(())
    }

    fn capture_region(&mut self, region: Rect) -> Result<Option<Vec<u8>>, CaptureError> {
        let cx = region.left + region.width() / 2;
        let cy = region.top + region.height() / 2;
        let Some(info) = self.outputs.iter().find(|o| o.desktop.contains(cx, cy)) else {
            return Ok(None);
        };
        let desktop = info.desktop;

        let need_dup = match &self.dup {
            Some(d) => d.desktop != desktop,
            None => true,
        };
        if need_dup {
            self.dup = None;
            self.have_valid = false;
            // SAFETY: output 与 device 同适配器。
            let out1: IDXGIOutput1 =
                info.output.cast().map_err(|e| CaptureError::Hr(e.code().0))?;
            let dup: IDXGIOutputDuplication = unsafe { out1.DuplicateOutput(&self.device) }
                .map_err(|e| {
                    self.count(|s| {
                        s.session_failed.fetch_add(1, Ordering::Relaxed);
                        s.last_error.store(e.code().0, Ordering::Relaxed);
                    });
                    CaptureError::Hr(e.code().0)
                })?;
            self.count(|s| {
                s.session_created.fetch_add(1, Ordering::Relaxed);
            });
            // SAFETY: desc 由 API 直接返回。
            let desc: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
            self.dup = Some(ActiveDup {
                desktop,
                dup,
                w: desc.ModeDesc.Width,
                h: desc.ModeDesc.Height,
            });
        }
        // 取出复制会话的本地副本（COM 接口可 Clone），结束对 self 的借用。
        let (dup, dw, dh) = {
            let active = self.dup.as_ref().unwrap();
            (active.dup.clone(), active.w, active.h)
        };

        // ---- 取帧：跳过 `LastPresentTime == 0` 的未定义帧 ----
        let (rw, rh) = (region.width().max(0) as u32, region.height().max(0) as u32);
        if rw == 0 || rh == 0 {
            return Ok(None);
        }

        let mut acquired: Option<ID3D11Texture2D> = None;
        if self.have_valid {
            // 稳态：只探一次，短超时；无新帧就沿用上一张。
            match acquire_once(&dup, STEADY_TIMEOUT_MS) {
                AcquireOutcome::Frame(tex) => acquired = Some(tex),
                AcquireOutcome::NotPresented => {
                    self.count(|s| {
                        s.frame_empty.fetch_add(1, Ordering::Relaxed);
                    });
                    return Ok(None);
                }
                AcquireOutcome::Timeout => {
                    self.count(|s| {
                        s.frame_timeout.fetch_add(1, Ordering::Relaxed);
                    });
                    return Ok(None);
                }
                AcquireOutcome::Lost => return Err(CaptureError::AccessLost),
                AcquireOutcome::Failed(hr) => return Err(CaptureError::Hr(hr)),
            }
        } else {
            // 会话刚建立/重建：反复尝试直到拿到真实呈现帧。
            for _ in 0..FIRST_FRAME_MAX_TRIES {
                match acquire_once(&dup, FIRST_FRAME_TIMEOUT_MS) {
                    AcquireOutcome::Frame(tex) => {
                        acquired = Some(tex);
                        break;
                    }
                    AcquireOutcome::NotPresented => {
                        self.count(|s| {
                            s.frame_empty.fetch_add(1, Ordering::Relaxed);
                        });
                    }
                    AcquireOutcome::Timeout => {
                        self.count(|s| {
                            s.frame_timeout.fetch_add(1, Ordering::Relaxed);
                        });
                    }
                    AcquireOutcome::Lost => return Err(CaptureError::AccessLost),
                    AcquireOutcome::Failed(hr) => {
                        self.count(|s| {
                            s.last_error.store(hr, Ordering::Relaxed);
                        });
                        break;
                    }
                }
            }
        }
        // 一张有效帧都没拿到：保留共享槽里的旧帧，不算错误。
        let Some(src) = acquired else {
            return Ok(None);
        };

        self.count(|s| {
            s.frame_acquired.fetch_add(1, Ordering::Relaxed);
        });

        // ---- 格式校验：必须查询交付纹理自身的格式 ----
        // `duplDesc.ModeDesc.Format` 是源格式（HDR 下为 R16G16B16A16_FLOAT），
        // 而实际交付通常是 B8G8R8A8_UNORM。只信 GetDesc。
        let mut td = D3D11_TEXTURE2D_DESC::default();
        unsafe { src.GetDesc(&mut td) };
        self.count(|s| {
            s.last_format.store(td.Format.0, Ordering::Relaxed);
        });
        if td.Format.0 != DXGI_FORMAT_B8G8R8A8_UNORM.0 {
            // CopySubresourceRegion 不做格式转换，硬拷会静默失败。宁可跳过并上报。
            let _ = unsafe { dup.ReleaseFrame() };
            self.count(|s| {
                s.unsupported_format.fetch_add(1, Ordering::Relaxed);
                s.copy_error.fetch_add(1, Ordering::Relaxed);
                s.last_error.store(td.Format.0, Ordering::Relaxed);
            });
            self.have_valid = false;
            return Err(CaptureError::UnsupportedFormat(td.Format.0));
        }

        let local_l = region.left - desktop.left;
        let local_t = region.top - desktop.top;
        let (dw, dh) = (dw as i32, dh as i32);
        let ol = local_l.clamp(0, dw);
        let ot = local_t.clamp(0, dh);
        let or = (local_l + rw as i32).clamp(0, dw);
        let ob = (local_t + rh as i32).clamp(0, dh);
        if or <= ol || ob <= ot {
            let _ = unsafe { dup.ReleaseFrame() };
            return Ok(None);
        }

        self.ensure_staging(rw, rh)?;

        let full_overlap =
            ol == local_l && ot == local_t && or == local_l + rw as i32 && ob == local_t + rh as i32;
        if !full_overlap {
            self.zero_staging(rw, rh)?;
        }
        let box_ = D3D11_BOX {
            left: ol as u32,
            top: ot as u32,
            front: 0,
            right: or as u32,
            bottom: ob as u32,
            back: 1,
        };
        let dst_x = (ol - local_l).max(0) as u32;
        let dst_y = (ot - local_t).max(0) as u32;
        let staging = self.staging.as_ref().unwrap();
        let staging_res: ID3D11Resource =
            staging.cast().map_err(|e| CaptureError::Hr(e.code().0))?;
        let src_res: ID3D11Resource = src.cast().map_err(|e| CaptureError::Hr(e.code().0))?;
        // SAFETY: 纹理有效，box 在 src 尺寸内，且格式已校验为一致。
        unsafe {
            self.context.CopySubresourceRegion(
                Some(&staging_res),
                0,
                dst_x,
                dst_y,
                0,
                Some(&src_res),
                0,
                Some(&box_),
            );
            let _ = dup.ReleaseFrame();
        }

        // 读回像素。staging 里是 BGRA（与桌面纹理同格式，拷贝不做转换），
        // 下面的循环顺手把 B/R 换位，交给上层的 `rgba` 就是真正的 RGBA。
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
        // SAFETY: staging 为 CPU_ACCESS_READ|WRITE 的 staging 纹理。
        unsafe {
            self.context
                .Map(Some(&staging_res), 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Hr(e.code().0))?;
        }
        let row_pitch = mapped.RowPitch as usize;
        let src_ptr = mapped.pData as *const u8;
        let mut rgba = Vec::with_capacity((rw * rh * 4) as usize);
        // SAFETY: 逐行读取 row_pitch 字节，拷贝 rw*4 有效字节（BGRA→RGBA）。
        unsafe {
            for y in 0..rh {
                let row = src_ptr.add((y as usize) * row_pitch);
                for x in 0..rw {
                    let off = (x as usize) * 4;
                    rgba.push(*row.add(off + 2)); // R
                    rgba.push(*row.add(off + 1)); // G
                    rgba.push(*row.add(off)); // B
                    rgba.push(*row.add(off + 3)); // A
                }
            }
            self.context.Unmap(Some(&staging_res), 0);
        }

        self.have_valid = true;
        self.count(|s| {
            s.copy_ok.fetch_add(1, Ordering::Relaxed);
        });
        Ok(Some(rgba))
    }

    fn ensure_staging(&mut self, w: u32, h: u32) -> Result<(), CaptureError> {
        if self.staging_size == (w, h) && self.staging.is_some() {
            return Ok(());
        }
        let desc = D3D11_TEXTURE2D_DESC {
            Width: w,
            Height: h,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
            MiscFlags: 0,
        };
        let mut staging: Option<ID3D11Texture2D> = None;
        // SAFETY: desc 有效；staging 输出指针由 windows crate 写入。
        unsafe {
            self.device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .map_err(|e| CaptureError::Hr(e.code().0))?;
        }
        self.staging = staging;
        self.staging_size = (w, h);
        Ok(())
    }

    /// 把 staging 纹理清零（区域越过桌面边缘时，未覆盖部分必须是黑色而不是
    /// 上一帧的残留）。
    ///
    /// 必须用 `D3D11_MAP_WRITE`：旧实现用 `D3D11_MAP_READ` 去写，属于对只读
    /// 映射区写入的未定义行为。
    fn zero_staging(&mut self, w: u32, h: u32) -> Result<(), CaptureError> {
        let staging = self.staging.as_ref().unwrap();
        let staging_res: ID3D11Resource =
            staging.cast().map_err(|e| CaptureError::Hr(e.code().0))?;
        let mut mapped: D3D11_MAPPED_SUBRESOURCE = unsafe { std::mem::zeroed() };
        // SAFETY: staging 为 CPU_ACCESS_WRITE 的 staging 纹理；MAP_WRITE 后可写。
        unsafe {
            self.context
                .Map(Some(&staging_res), 0, D3D11_MAP_WRITE, 0, Some(&mut mapped))
                .map_err(|e| CaptureError::Hr(e.code().0))?;
            let ptr = mapped.pData as *mut u8;
            let row_pitch = mapped.RowPitch as usize;
            for y in 0..h {
                let row = ptr.add((y as usize) * row_pitch);
                std::ptr::write_bytes(row, 0, (w as usize) * 4);
            }
            self.context.Unmap(Some(&staging_res), 0);
        }
        Ok(())
    }
}

/// 单次取帧结果。
enum AcquireOutcome {
    /// 拿到 `LastPresentTime != 0` 的真实帧。
    Frame(ID3D11Texture2D),
    /// 返回成功但 `LastPresentTime == 0`：内容未定义，调用方应重试。
    NotPresented,
    Timeout,
    Lost,
    Failed(i32),
}

/// 取一帧并判定其是否为「真实呈现帧」。
fn acquire_once(dup: &IDXGIOutputDuplication, timeout_ms: u32) -> AcquireOutcome {
    let mut frame_info: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
    let mut resource: Option<IDXGIResource> = None;
    // SAFETY: frame_info / resource 指向栈上可变数据。
    match unsafe { dup.AcquireNextFrame(timeout_ms, &mut frame_info, &mut resource) } {
        Ok(()) => {}
        Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => return AcquireOutcome::Timeout,
        Err(e) if e.code() == DXGI_ERROR_ACCESS_LOST => return AcquireOutcome::Lost,
        Err(e) => return AcquireOutcome::Failed(e.code().0),
    }

    // 关键判定：LastPresentTime == 0 表示自上次 Acquire 以来没有新的呈现，
    // 此时桌面图像内容未定义（实测为全 0），必须释放并重试。
    if frame_info.LastPresentTime == 0 {
        let _ = unsafe { dup.ReleaseFrame() };
        return AcquireOutcome::NotPresented;
    }

    let Some(resource) = resource else {
        let _ = unsafe { dup.ReleaseFrame() };
        return AcquireOutcome::NotPresented;
    };
    match resource.cast::<ID3D11Texture2D>() {
        Ok(tex) => AcquireOutcome::Frame(tex),
        Err(_) => {
            let _ = unsafe { dup.ReleaseFrame() };
            AcquireOutcome::Failed(0)
        }
    }
}

impl Rect {
    fn contains(&self, x: i32, y: i32) -> bool {
        x >= self.left && x < self.right && y >= self.top && y < self.bottom
    }
}

fn dxgi_rect_to_rect(r: RECT) -> Rect {
    Rect { left: r.left, top: r.top, right: r.right, bottom: r.bottom }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    /// 主显示器工作区（取不到时退回 (0,0) 所在显示器）。
    fn primary_region() -> Rect {
        crate::win32::geometry::primary_work_area()
            .or_else(|| crate::win32::geometry::monitor_rect(0, 0))
            .expect("需要至少一台活动显示器")
    }

    /// 轻微拨动光标，强制 DWM 产出一次新的呈现。
    ///
    /// DDA 的 `AcquireNextFrame` 只在桌面**有新的呈现**时才给出内容已定义的
    /// 帧；完全静止的桌面上理论上可以一直不呈现。真实使用中鼠标永远在动，
    /// 所以这里模拟一次最小扰动，把"过滤逻辑是否正确"与"桌面是否恰好静止"
    /// 这两件事解耦 —— 否则测试会在静止机器上偶发失败。
    fn nudge_cursor() {
        use windows::Win32::Foundation::POINT;
        use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SetCursorPos};
        let mut p = POINT::default();
        if unsafe { GetCursorPos(&mut p) }.is_ok() {
            unsafe {
                let _ = SetCursorPos(p.x + 1, p.y);
                let _ = SetCursorPos(p.x, p.y);
            }
        }
    }

    /// 真机闭环：捕获线程必须交出**内容已定义**的帧。
    ///
    /// 这条测试直接对应 v0.3 修复的 P0 缺陷：旧实现不检查 `LastPresentTime`，
    /// 于是常态化地把"未定义的全零首帧"当成有效帧交给前端，前端即渲染成黑板。
    /// 断言里三件事都必须成立才算过：
    /// 1. 帧尺寸/序号推进（说明确实拿到了帧，而不是一直在超时）；
    /// 2. 像素非全零（说明过滤掉了未定义帧）；
    /// 3. 交付纹理格式是 B8G8R8A8_UNORM 且拷贝成功（说明 staging 格式对上了）。
    #[test]
    #[serial_test::serial]
    fn 真机_静止桌面可捕获非空帧() {
        let shared = CaptureShared::new();
        {
            let mut cfg = shared.config.lock().expect("配置锁");
            cfg.region = Some(primary_region());
            cfg.interval_ms = 33;
        }
        let handle = spawn(Arc::clone(&shared));

        let start = Instant::now();
        let mut got: Option<(u32, u32, u64, bool)> = None;
        let mut nudged = false;
        while start.elapsed() < Duration::from_secs(8) {
            if let Ok(f) = shared.frame.lock() {
                if let Some(fr) = f.as_ref() {
                    let nonzero = fr.rgba.iter().any(|b| *b != 0);
                    got = Some((fr.w, fr.h, fr.seq, nonzero));
                    break;
                }
            }
            if !nudged && start.elapsed() > Duration::from_millis(1500) {
                nudged = true;
                nudge_cursor();
            }
            std::thread::sleep(Duration::from_millis(40));
        }

        let stats = shared.stats.snapshot();
        // 先释放会话再断言：失败时也必须放掉 DDA 会话（全系统上限 4 个），
        // 否则同一台机器上后续跑测试会一直撞 DXGI_ERROR_NOT_CURRENTLY_AVAILABLE。
        drop(handle);

        let (w, h, seq, nonzero) = got.unwrap_or_else(|| {
            panic!(
                "8 秒内没有拿到有效帧。sessionCreated={} sessionFailed={} \
                 frameAcquired={} frameEmpty={} frameTimeout={} accessLost={} \
                 copyOk={} copyError={} unsupportedFormat={} lastFormat={} lastError=0x{:08X}",
                stats.session_created,
                stats.session_failed,
                stats.frame_acquired,
                stats.frame_empty,
                stats.frame_timeout,
                stats.access_lost,
                stats.copy_ok,
                stats.copy_error,
                stats.unsupported_format,
                stats.last_format,
                stats.last_error as u32,
            )
        });

        assert!(w > 0 && h > 0, "帧尺寸非法：{w}×{h}");
        assert!(seq >= 1, "帧序号未推进，说明共享槽从未被写入");
        assert!(
            nonzero,
            "帧内容全零 —— LastPresentTime 过滤没生效，前端会渲染成黑板"
        );
        assert!(stats.copy_ok > 0, "没有任何一次成功拷贝");
        assert_eq!(
            stats.last_format,
            DXGI_FORMAT_B8G8R8A8_UNORM.0,
            "交付纹理格式不是 B8G8R8A8_UNORM，staging 会因格式不符而拷贝失败"
        );
    }
}
