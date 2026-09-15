//! DDA 帧格式 + 拷贝可行性诊断探针（**只读**，不改动显示器任何设置）。
//!
//! 待验证的关键假设（`win32/capture.rs` 的隐含前提）：
//!   1. `IDXGIOutput1::DuplicateOutput` 在 HDR 开启时给出的帧是 `B8G8R8A8_UNORM`；
//!   2. 因此可以用固定 `B8G8R8A8_UNORM` 的 staging 纹理承接 `CopySubresourceRegion`。
//!
//! 本探针做无歧义的 A/B 对照：
//!   - 先枚举适配器/输出并打印 HDR 色彩状态；
//!   - 取帧时**严格等待 `LastPresentTime != 0` 的真实帧**（避免 DDA 首帧内容未定义导致的假阳性）；
//!   - 对**同一帧**分别用 `B8G8R8A8_UNORM` 与 `R16G16B16A16_FLOAT` 两种 staging 拷贝，
//!     统计读回的非零字节数 —— 谁出数据，谁就是正确的格式；
//!   - 顺带测试 `IDXGIOutput5::DuplicateOutput1` 指定 BGRA 是否真能得到 BGRA 帧。
//!
//! 运行：`cargo run --bin probe-dda`

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BOX, D3D11_CPU_ACCESS_READ, D3D11_CPU_ACCESS_WRITE, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_CREATE_DEVICE_DEBUG, D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_R16G16B16A16_FLOAT, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutput5, IDXGIOutput6, IDXGIOutputDuplication, IDXGIResource, DXGI_ERROR_WAIT_TIMEOUT,
    DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC, DXGI_OUTPUT_DESC1,
};

fn hex(code: i32) -> String {
    format!("0x{:08X}", code as u32)
}

fn fmt_name(f: i32) -> &'static str {
    match f {
        0 => "UNKNOWN",
        10 => "R16G16B16A16_FLOAT",
        24 => "R10G10B10A2_UNORM",
        28 => "R8G8B8A8_UNORM",
        87 => "B8G8R8A8_UNORM",
        _ => "(other)",
    }
}

/// 取一张**内容有效**的帧：跳过 `LastPresentTime == 0` 的空帧。
///
/// DDA 的已知语义：`DuplicateOutput` 之后第一次 `AcquireNextFrame` 常立即返回
/// 且 `LastPresentTime == 0`，此时桌面图像内容**未定义**（可能全 0）。必须循环
/// 到真正发生呈现的那一帧，否则会把「没数据」误判成「格式不对」。
///
/// 返回 `(资源, 是否确认为真实呈现帧)`。若始终没等到真实帧，则返回最后一次
/// 拿到的资源并标记 `false`（此时 A/B 对照结果只能作为参考，不能作为定论）。
fn acquire_real_frame(dup: &IDXGIOutputDuplication) -> Option<(IDXGIResource, bool)> {
    for attempt in 0..25 {
        let mut fi: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut res: Option<IDXGIResource> = None;
        match unsafe { dup.AcquireNextFrame(100, &mut fi, &mut res) } {
            Ok(()) => {
                if fi.LastPresentTime != 0 {
                    println!(
                        "  取帧成功（第 {} 次）：LastPresentTime={} AccumulatedFrames={}",
                        attempt + 1,
                        fi.LastPresentTime,
                        fi.AccumulatedFrames
                    );
                    return res.map(|r| (r, true));
                }
                // 空帧：必须释放后才能再次 Acquire。
                let _ = unsafe { dup.ReleaseFrame() };
            }
            Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => { /* 继续重试 */ }
            Err(e) => {
                println!("  AcquireNextFrame 失败 {}", hex(e.code().0));
                return None;
            }
        }
    }
    // 兜底：再取一次，无论是否空帧都接受（结论仅供参考）。
    let mut fi: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
    let mut res: Option<IDXGIResource> = None;
    match unsafe { dup.AcquireNextFrame(200, &mut fi, &mut res) } {
        Ok(()) => {
            println!("  警告：仅能取到 LastPresentTime==0 的空帧，兜底接受（结论仅供参考）");
            res.map(|r| (r, false))
        }
        Err(e) => {
            println!("  兜底取帧亦失败 {}", hex(e.code().0));
            None
        }
    }
}

/// 用指定格式的 staging 纹理拷贝 `src` 的 `(rw, rh)` 区域，返回非零字节数。
///
/// `None` 表示建纹理或 Map 失败。
fn probe_copy(
    device: &ID3D11Device,
    context: &ID3D11DeviceContext,
    src: &ID3D11Texture2D,
    format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT,
    bytes_per_pixel: u32,
    rw: u32,
    rh: u32,
) -> Option<usize> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: rw,
        Height: rh,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: D3D11_USAGE_STAGING,
        BindFlags: 0,
        CPUAccessFlags: (D3D11_CPU_ACCESS_READ.0 | D3D11_CPU_ACCESS_WRITE.0) as u32,
        MiscFlags: 0,
    };
    let mut staging: Option<ID3D11Texture2D> = None;
    unsafe { device.CreateTexture2D(&desc, None, Some(&mut staging)) }.ok()?;
    let staging = staging?;
    let dst: ID3D11Resource = staging.cast().ok()?;
    let sres: ID3D11Resource = src.cast().ok()?;
    let bx = D3D11_BOX { left: 0, top: 0, front: 0, right: rw, bottom: rh, back: 1 };
    unsafe {
        // windows 0.58 不返回 HRESULT；非法调用只在 debug layer 下被报告。
        context.CopySubresourceRegion(Some(&dst), 0, 0, 0, 0, Some(&sres), 0, Some(&bx));
    }
    let mut m = D3D11_MAPPED_SUBRESOURCE::default();
    unsafe { context.Map(Some(&dst), 0, D3D11_MAP_READ, 0, Some(&mut m)) }.ok()?;
    let mut nonzero = 0usize;
    let row_bytes = (rw * bytes_per_pixel) as usize;
    unsafe {
        let p = m.pData as *const u8;
        for y in 0..rh as usize {
            let row = p.add(y * m.RowPitch as usize);
            for x in 0..row_bytes {
                if *row.add(x) != 0 {
                    nonzero += 1;
                }
            }
        }
        context.Unmap(Some(&dst), 0);
    }
    Some(nonzero)
}

fn main() {
    println!("=== DDA frame-format & copy diagnosis (READ-ONLY) ===\n");

    // ---------- 0. 坐标空间基准 ----------
    {
        use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
        use windows::Win32::UI::WindowsAndMessaging::{
            GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CXSCREEN, SM_CYSCREEN, SM_CYVIRTUALSCREEN,
        };
        unsafe {
            println!(
                "坐标空间: SM_CXSCREEN={} SM_CYSCREEN={}  virtual={}x{}",
                GetSystemMetrics(SM_CXSCREEN),
                GetSystemMetrics(SM_CYSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN)
            );
            let mut dm =
                DEVMODEW { dmSize: std::mem::size_of::<DEVMODEW>() as u16, ..Default::default() };
            if EnumDisplaySettingsW(None, ENUM_CURRENT_SETTINGS, &mut dm).as_bool() {
                println!(
                    "当前显示模式: {}x{} @{}Hz {}bpp",
                    dm.dmPelsWidth, dm.dmPelsHeight, dm.dmDisplayFrequency, dm.dmBitsPerPel
                );
            }
        }
        println!(
            "geometry::monitor_rect(0,0)   = {:?}",
            hdr_sdr_widget_lib::win32::geometry::monitor_rect(0, 0)
        );
        println!(
            "geometry::primary_work_area() = {:?}\n",
            hdr_sdr_widget_lib::win32::geometry::primary_work_area()
        );
    }

    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(e) => {
            println!("CreateDXGIFactory1 失败 {}", hex(e.code().0));
            return;
        }
    };

    let mut idx = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(idx) } {
            Ok(a) => a,
            Err(_) => break,
        };
        let ad = unsafe { adapter.GetDesc1() }.unwrap_or_default();
        let name =
            String::from_utf16_lossy(&ad.Description).trim_end_matches('\0').to_string();
        println!("[适配器 {idx}] {name}  vendor=0x{:04X} device=0x{:04X}", ad.VendorId, ad.DeviceId);

        let mut outputs: Vec<IDXGIOutput> = Vec::new();
        let mut o = 0u32;
        while let Ok(out) = unsafe { adapter.EnumOutputs(o) } {
            outputs.push(out);
            o += 1;
        }
        if outputs.is_empty() {
            println!("  (无输出)\n");
            idx += 1;
            continue;
        }

        for (i, out) in outputs.iter().enumerate() {
            if let Ok(d) = unsafe { out.GetDesc() } {
                let d: DXGI_OUTPUT_DESC = d;
                println!(
                    "  [输出 {i}] desktop=({},{},{},{}) attached={}",
                    d.DesktopCoordinates.left,
                    d.DesktopCoordinates.top,
                    d.DesktopCoordinates.right,
                    d.DesktopCoordinates.bottom,
                    d.AttachedToDesktop.as_bool()
                );
            }
            if let Ok(out6) = out.cast::<IDXGIOutput6>() {
                match unsafe { out6.GetDesc1() } {
                    Ok(d1) => {
                        let d1: DXGI_OUTPUT_DESC1 = d1;
                        println!(
                            "    HDR/色彩: bitsPerColor={} colorSpace={} maxLum={} minLum={}",
                            d1.BitsPerColor, d1.ColorSpace.0, d1.MaxLuminance, d1.MinLuminance
                        );
                    }
                    Err(e) => println!("    GetDesc1 失败 {}", hex(e.code().0)),
                }
            }
        }

        // ---------- 建 D3D11 设备（优先带 debug layer，失败则降级）----------
        let adapter_base: IDXGIAdapter = match adapter.cast() {
            Ok(a) => a,
            Err(_) => {
                idx += 1;
                continue;
            }
        };
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let debug_on = unsafe {
            D3D11CreateDevice(
                Some(&adapter_base),
                D3D_DRIVER_TYPE_UNKNOWN,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_DEBUG,
                Some(&levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        }
        .is_ok();
        if !debug_on {
            device = None;
            context = None;
            if let Err(e) = unsafe {
                D3D11CreateDevice(
                    Some(&adapter_base),
                    D3D_DRIVER_TYPE_UNKNOWN,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    Some(&levels),
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                )
            } {
                println!("  D3D11CreateDevice 失败 {}\n", hex(e.code().0));
                idx += 1;
                continue;
            }
        }
        let (Some(device), Some(context)) = (device, context) else {
            idx += 1;
            continue;
        };
        println!(
            "  D3D11 设备: 就绪（debug layer {}）",
            if debug_on { "已启用（下方会报告非法调用）" } else { "不可用，非法调用将被静默丢弃" }
        );

        let out1: IDXGIOutput1 = match outputs[0].cast() {
            Ok(x) => x,
            Err(e) => {
                println!("  cast IDXGIOutput1 失败 {}\n", hex(e.code().0));
                idx += 1;
                continue;
            }
        };

        // ---------- A. 现状路径：IDXGIOutput1::DuplicateOutput ----------
        println!("\n  === A. 现状路径 DuplicateOutput ===");
        let dup: IDXGIOutputDuplication = match unsafe { out1.DuplicateOutput(&device) } {
            Ok(d) => d,
            Err(e) => {
                println!(
                    "  DuplicateOutput 失败 {}  → 捕获完全不可用\n   （0x887A0022 = 并发上限/被占用；0x887A0004 = 模式不支持）",
                    hex(e.code().0)
                );
                idx += 1;
                continue;
            }
        };
        let dd: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
        println!(
            "  duplDesc.ModeDesc = {}x{} format={} ({})",
            dd.ModeDesc.Width,
            dd.ModeDesc.Height,
            dd.ModeDesc.Format.0,
            fmt_name(dd.ModeDesc.Format.0)
        );
        let dd_w = dd.ModeDesc.Width;
        let dd_h = dd.ModeDesc.Height;

        match acquire_real_frame(&dup) {
            Some((res, real)) => {
                if !real {
                    println!("  （注意：以下对照基于可能为空的兜底帧）");
                }
                if let Ok(tex) = res.cast::<ID3D11Texture2D>() {
                    let mut td = D3D11_TEXTURE2D_DESC::default();
                    unsafe { tex.GetDesc(&mut td) };
                    println!(
                        "  >>> 真实帧纹理: {}x{} format={} ({})",
                        td.Width,
                        td.Height,
                        td.Format.0,
                        fmt_name(td.Format.0)
                    );
                    if td.Format.0 != DXGI_FORMAT_B8G8R8A8_UNORM.0 {
                        println!(
                            "  >>> 与 capture.rs 硬编码的 B8G8R8A8 staging 不一致（该代码从不查询真实格式）"
                        );
                    }

                    // 同一帧，两种 staging 格式对照。
                    println!("  --- A/B 拷贝对照（同一帧、全屏区域）---");
                    let bgra = probe_copy(
                        &device,
                        &context,
                        &tex,
                        DXGI_FORMAT_B8G8R8A8_UNORM,
                        4,
                        dd_w,
                        dd_h,
                    );
                    match bgra {
                        Some(n) => println!(
                            "  [B8G8R8A8 staging]            非零字节 = {n}  → {}",
                            if n > 0 { "可用" } else { "全 0，拷贝无效" }
                        ),
                        None => println!("  [B8G8R8A8 staging]            建纹理/Map 失败"),
                    }
                    let flt = probe_copy(
                        &device,
                        &context,
                        &tex,
                        DXGI_FORMAT_R16G16B16A16_FLOAT,
                        8,
                        dd_w,
                        dd_h,
                    );
                    match flt {
                        Some(n) => println!(
                            "  [R16G16B16A16_FLOAT staging]  非零字节 = {n}  → {}",
                            if n > 0 { "可用（此即正确格式）" } else { "全 0" }
                        ),
                        None => println!("  [R16G16B16A16_FLOAT staging]  建纹理/Map 失败"),
                    }
                }
                let _ = unsafe { dup.ReleaseFrame() };
            }
            None => println!("  未取到有效帧（25 次重试均无 LastPresentTime != 0）"),
        }
        drop(dup);

        // ---------- B. 官方推荐：IDXGIOutput5::DuplicateOutput1 指定 BGRA ----------
        println!("\n  === B. DuplicateOutput1(supportedFormats=[B8G8R8A8_UNORM]) ===");
        match outputs[0].cast::<IDXGIOutput5>() {
            Ok(out5) => {
                let fmts = [DXGI_FORMAT_B8G8R8A8_UNORM];
                match unsafe { out5.DuplicateOutput1(&device, 0, &fmts) } {
                    Ok(d2) => {
                        let d2d: DXGI_OUTDUPL_DESC = unsafe { d2.GetDesc() };
                        println!(
                            "  DuplicateOutput1 OK  duplDesc.Format={} ({})",
                            d2d.ModeDesc.Format.0,
                            fmt_name(d2d.ModeDesc.Format.0)
                        );
                        if let Some((res2, _)) = acquire_real_frame(&d2) {
                            if let Ok(t2) = res2.cast::<ID3D11Texture2D>() {
                                let mut td2 = D3D11_TEXTURE2D_DESC::default();
                                unsafe { t2.GetDesc(&mut td2) };
                                println!(
                                    "  >>> 请求 BGRA，实得: format={} ({})  → {}",
                                    td2.Format.0,
                                    fmt_name(td2.Format.0),
                                    if td2.Format.0 == DXGI_FORMAT_B8G8R8A8_UNORM.0 {
                                        "DXGI 已代为转换，可直接用 BGRA"
                                    } else {
                                        "DXGI 未转换，仍需自行处理 HDR 格式"
                                    }
                                );
                            }
                            let _ = unsafe { d2.ReleaseFrame() };
                        } else {
                            println!("  未取到有效帧");
                        }
                    }
                    Err(e) => println!("  DuplicateOutput1 失败 {}", hex(e.code().0)),
                }
            }
            Err(e) => println!("  cast IDXGIOutput5 失败 {}", hex(e.code().0)),
        }

        idx += 1;
    }

    println!("\n=== done ===");
}
