//! DDA 捕获诊断探针：逐步测试「适配器枚举 → D3D11 设备 → DuplicateOutput → 取帧」。
//!
//! 独立于 Tauri，秒级编译。运行后把输出发回，即可定位桌面捕获卡在哪一步。

use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_UNKNOWN, D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_11_1,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_BGRA_SUPPORT,
    D3D11_SDK_VERSION,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, IDXGIAdapter, IDXGIAdapter1, IDXGIFactory1, IDXGIOutput, IDXGIOutput1,
    IDXGIOutputDuplication, DXGI_OUTDUPL_DESC, DXGI_OUTDUPL_FRAME_INFO,
};

fn main() {
    println!("=== DDA capture diagnosis ===");

    let factory: IDXGIFactory1 = match unsafe { CreateDXGIFactory1() } {
        Ok(f) => f,
        Err(e) => {
            println!("CreateDXGIFactory1 FAILED: {:?}", e.code());
            return;
        }
    };

    let mut adapter_idx = 0u32;
    loop {
        let adapter: IDXGIAdapter1 = match unsafe { factory.EnumAdapters1(adapter_idx) } {
            Ok(a) => a,
            Err(_) => break,
        };

        let desc = unsafe { adapter.GetDesc1() }.unwrap_or_default();
        let name = String::from_utf16_lossy(&desc.Description)
            .trim_end_matches('\0')
            .to_string();
        println!(
            "\nAdapter {}: {}  (vendor={:#06x} device={:#06x} flags={:#x})",
            adapter_idx, name, desc.VendorId, desc.DeviceId, desc.Flags
        );

        // 枚举输出。
        let mut out_idx = 0u32;
        let mut has_output = false;
        loop {
            let out: IDXGIOutput = match unsafe { adapter.EnumOutputs(out_idx) } {
                Ok(o) => o,
                Err(_) => break,
            };
            has_output = true;
            match unsafe { out.GetDesc() } {
                Ok(od) => {
                    println!(
                        "  output {}: desktop=({},{},{},{}) attached={}",
                        out_idx,
                        od.DesktopCoordinates.left,
                        od.DesktopCoordinates.top,
                        od.DesktopCoordinates.right,
                        od.DesktopCoordinates.bottom,
                        od.AttachedToDesktop.as_bool()
                    );
                }
                Err(e) => println!("  output {} GetDesc FAILED {:?}", out_idx, e.code()),
            }
            out_idx += 1;
        }
        if !has_output {
            println!("  (no outputs)");
            adapter_idx += 1;
            continue;
        }

        // 在该适配器上创建 D3D11 设备。
        let adapter_base: IDXGIAdapter = match adapter.cast() {
            Ok(a) => a,
            Err(e) => {
                println!("  cast -> IDXGIAdapter FAILED {:?}", e.code());
                adapter_idx += 1;
                continue;
            }
        };
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;
        let levels = [D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0];
        let hr = unsafe {
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
        };
        match hr {
            Ok(()) => println!("  D3D11CreateDevice: OK"),
            Err(e) => {
                println!("  D3D11CreateDevice FAILED {:?}", e.code());
                adapter_idx += 1;
                continue;
            }
        }
        let Some(device) = device else {
            println!("  device is None");
            adapter_idx += 1;
            continue;
        };

        // 对第一个输出尝试 DuplicateOutput。
        let out: IDXGIOutput = match unsafe { adapter.EnumOutputs(0) } {
            Ok(o) => o,
            Err(_) => {
                adapter_idx += 1;
                continue;
            }
        };
        let out1: IDXGIOutput1 = match out.cast() {
            Ok(o) => o,
            Err(e) => {
                println!("  cast -> IDXGIOutput1 FAILED {:?}", e.code());
                adapter_idx += 1;
                continue;
            }
        };
        let dup: IDXGIOutputDuplication = match unsafe { out1.DuplicateOutput(&device) } {
            Ok(d) => d,
            Err(e) => {
                println!("  DuplicateOutput FAILED {:?}", e.code());
                adapter_idx += 1;
                continue;
            }
        };
        println!("  DuplicateOutput: OK");
        let ddesc: DXGI_OUTDUPL_DESC = unsafe { dup.GetDesc() };
        println!("  desktop image: {} x {}", ddesc.ModeDesc.Width, ddesc.ModeDesc.Height);

        // 尝试取一帧。
        let mut fi: DXGI_OUTDUPL_FRAME_INFO = unsafe { std::mem::zeroed() };
        let mut res = None;
        match unsafe { dup.AcquireNextFrame(1000, &mut fi, &mut res) } {
            Ok(()) => println!("  AcquireNextFrame: OK"),
            Err(e) => println!("  AcquireNextFrame FAILED {:?}", e.code()),
        }
        let _ = unsafe { dup.ReleaseFrame() };

        adapter_idx += 1;
    }

    println!("\n=== done ===");
    std::thread::sleep(std::time::Duration::from_secs(3));
}
