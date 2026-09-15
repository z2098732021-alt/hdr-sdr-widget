//! 背景捕获装配：`glass` 自定义协议，供前端以二进制帧拉取悬浮物窗口后方的
//! 实时桌面内容。
//!
//! # ⚠️ 现状：应用内**已不再使用**这条通道（2026-09-13）
//!
//! 帧数据改走 IPC（`commands::pull_frame`）。真机实测本协议在**约 10～19 次响应
//! 之后**会让 WebView2 渲染进程的 JS 整体停止执行；换 IPC 后协议请求降为 0。
//! 完整证据与排除清单见 `docs/hdr-sdr-widget/FIX-PLAN.md` §4.0.2.3 / §4.0.3。
//!
//! 本模块**保留**的原因是它定义了帧的二进制格式（16 字节头 + RGBA），
//! `tools/glass_preview.py` 的浏览器预览 harness 仍在用同一套字节布局喂合成帧。
//!
//! **不要把它接回前端热路径** —— 它会让胶囊再次没有背景内容，而且症状与
//! "没渲染"极难区分（本模块为此类误判贡献过很多轮排查）。
//!
//! 数据流（历史）：`capture` 线程（`hdr_sdr_widget_lib::win32::capture`）抓取
//! 窗口矩形桌面 → 写入共享槽 → 前端 `fetch("http://glass.localhost/frame")` 拉取
//! → 解码 → Canvas 逐像素位移 → 上屏。
//!
//! # 响应体格式（自描述二进制帧）
//!
//! 响应 = **16 字节定长头** + **RGBA 像素字节**：
//!
//! | 偏移 | 类型 | 含义 |
//! | --- | --- | --- |
//! | 0..4 | u32 LE | 魔数 `0x48534452`（"HSDR"） |
//! | 4..8 | u32 LE | 帧序号 `seq` |
//! | 8..12 | u32 LE | 帧宽（物理像素） |
//! | 12..16 | u32 LE | 帧高（物理像素） |
//!
//! 像素是 **RGBA 而不是 BGRA**：桌面纹理是 `B8G8R8A8_UNORM`，但
//! `capture::CaptureContext::capture_region` 在读回时就换了位（见该函数注释）。
//! 这条容易记反 —— 前端若再换一次等于没换，症状是"颜色发青/发红"，
//! 极易被误判成光学或色调问题。
//!
//! 为什么把尺寸放进 body 而不是响应头：早期实现用自定义响应头传递尺寸，
//! 前端跨域 `fetch` 读不到（`Access-Control-Expose-Headers` 行为在不同
//! WebView2 版本上不可靠），于是退化成"用字节数反推缩放系数"，再做
//! `Math.round` 校验 —— 非整数缩放必然失配并**静默丢帧**。
//! 把元数据放进 body 后，前端不再依赖任何 CORS 头语义，也不存在舍入歧义。
//!
//! 走自定义协议而非 `emit` 事件：Tauri 事件走 JSON 序列化，二进制会被膨胀成
//! 数字数组 / base64；自定义协议返回原始字节，帧传输开销最小。

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tauri::http::header::{ACCESS_CONTROL_ALLOW_ORIGIN, CONTENT_LENGTH, CONTENT_TYPE};
use tauri::http::{HeaderValue, Request, Response, StatusCode};
use tauri::{Runtime, UriSchemeContext};

use hdr_sdr_widget_lib::win32::capture::CaptureShared;

/// 帧头魔数。`0x4853_4452` 按**大端**书写即 ASCII "HSDR"（字节序 `48 53 44 52`）；
/// 在线路上按小端 u32 序列化，所以抓包看到的是 `52 44 53 48`（"RDSH"）。
/// 别被字节序绕进去 —— 前端读的时候用 `getUint32(0, true)`，两边数值一致即可。
pub const FRAME_MAGIC: u32 = 0x4853_4452;
/// 帧头长度（字节）。
pub const FRAME_HEADER_LEN: usize = 16;

/// `HSDR_FRAME_BYTES=<n>` 诊断开关：把 `/frame` 响应体截断到 `n` 字节。
///
/// 见 [`serve`] 内对该开关用途的说明。默认（不设变量）返回 `None`，即不截断。
fn diag_body_limit() -> Option<usize> {
    std::env::var("HSDR_FRAME_BYTES")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
}

/// `glass` 协议处理器：返回最新桌面帧（16 字节头 + RGBA 字节）。
///
/// 路径 `/frame` → 返回帧；无帧 → `204`。
///
/// # ⚠️ 必须显式给 `Content-Length`
///
/// 真机实测：不设该头时**响应体传输会卡死** —— WebView2 侧拿不到 body 结束的
/// 信号，连接一直占着不释放。后果远不止"这一帧拿不到"：同一 host 的后续请求
/// （**包括 Tauri 自己的 IPC**）全部排队在它后面，于是渲染端的
/// `report_glass_status` 上报也停了，整个前端在诊断面上看起来像"死了"。
///
/// 症状组合很好认：`proto_hits` 在涨、`proto_200` 也出现了，但前端上报的
/// `polls` 定格在很小的一个数且再也不更新。修法就是补这一个头。
pub fn serve<R: Runtime>(
    shared: Arc<CaptureShared>,
    _ctx: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    // 诊断计数：把"请求到没到""路径长什么样""槽里有没有帧"全部记下来。
    // 这三个数放在一起才能定位"前端一直拿到 204"到底是哪一环的问题。
    let path = request.uri().path().to_string();
    shared.proto_hits.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut p) = shared.proto_last_path.lock() {
        *p = path.clone();
    }

    let frame = shared.frame.lock().ok().and_then(|g| g.clone());
    match (path.as_str(), frame) {
        ("/frame", Some(f)) => {
            let mut body = Vec::with_capacity(FRAME_HEADER_LEN + f.rgba.len());
            body.extend_from_slice(&FRAME_MAGIC.to_le_bytes());
            body.extend_from_slice(&(f.seq as u32).to_le_bytes());
            body.extend_from_slice(&f.w.to_le_bytes());
            body.extend_from_slice(&f.h.to_le_bytes());
            body.extend_from_slice(&f.rgba);

            // 诊断开关 `HSDR_FRAME_BYTES=<n>`：把响应体截断到 n 字节。
            //
            // 用途：判定"200 响应体传不完"到底与体积有关，还是协议通路本身不通。
            // 截断后前端会报 `bad-payload`（像素数不符）—— **那恰好是通路可用的
            // 证据**：小体积能完整送达，说明问题在体积/传输，而非请求发不出去。
            // 不设该变量则完全不生效。
            if let Some(n) = diag_body_limit() {
                body.truncate(n);
            }

            let len = body.len();
            let mut resp = Response::new(body);
            *resp.status_mut() = StatusCode::OK;
            shared.proto_200.fetch_add(1, Ordering::Relaxed);
            let _ = resp
                .headers_mut()
                .insert(CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
            // 关键：显式声明长度，否则响应体可能永远传不完（见函数文档）。
            if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
                let _ = resp.headers_mut().insert(CONTENT_LENGTH, v);
            }
            // 前端不再依赖自定义响应头，但保留 CORS 头以便将来诊断工具直读。
            let _ = resp
                .headers_mut()
                .insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            resp
        }
        _ => {
            shared.proto_204.fetch_add(1, Ordering::Relaxed);
            let mut resp = Response::new(Vec::new());
            *resp.status_mut() = StatusCode::NO_CONTENT;
            let _ = resp
                .headers_mut()
                .insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
            resp
        }
    }
}
