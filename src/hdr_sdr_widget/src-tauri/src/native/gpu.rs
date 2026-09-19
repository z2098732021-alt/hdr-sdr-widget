use super::capture::Capture;
use windows::{
    core::{s, Interface, Result, PCSTR},
    Win32::{
        Foundation::{CloseHandle, HANDLE, HWND},
        Graphics::{
            Direct3D::{Fxc::*, *},
            Direct3D11::*,
            DirectComposition::*,
            Dxgi::{Common::*, *},
        },
        System::Threading::WaitForSingleObject,
    },
};

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct RenderSnapshot {
    pub viewport: [f32; 4],
    pub capsule: [f32; 4],
    pub desktop: [f32; 4],
    pub material: [f32; 4],
    pub feedback: [f32; 4],
    pub optics: [f32; 4],
    pub pointer: [f32; 4],
    pub timing: [f32; 4],
    pub hdr: [f32; 4],
    pub control: [f32; 4],
}
pub struct Gpu {
    pub monitor_key: String,
    pub device: ID3D11Device,
    pub context: ID3D11DeviceContext,
    pub output: IDXGIOutput,
    pub capture: Capture,
    swap: IDXGISwapChain1,
    wait: HANDLE,
    target: Option<ID3D11RenderTargetView>,
    _composition: IDCompositionDevice,
    _root: IDCompositionVisual,
    _window_target: IDCompositionTarget,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    constants: ID3D11Buffer,
    sampler: ID3D11SamplerState,
    pub size: (u32, u32),
    audit_frozen: bool,
    adaptation_ps: ID3D11PixelShader,
    adaptation: [(ID3D11RenderTargetView, ID3D11ShaderResourceView); 2],
    adaptation_index: usize,
    pub peak_nits: f32,
    pub peak_fallback: bool,
    pub white_nits: f32,
    pub highlight_nits: f32,
    capability_checked: std::time::Instant,
    peak_target: f32,
}
pub unsafe fn compile(entry: PCSTR, model: PCSTR) -> Result<Vec<u8>> {
    let source = include_bytes!("glass.hlsl");
    let mut code = None;
    let mut errors = None;
    let result = D3DCompile(
        source.as_ptr().cast(),
        source.len(),
        s!("glass.hlsl"),
        None,
        None::<&ID3DInclude>,
        entry,
        model,
        D3DCOMPILE_OPTIMIZATION_LEVEL3,
        0,
        &mut code,
        Some(&mut errors),
    );
    if let Err(e) = result {
        if let Some(errors) = errors {
            let text = std::slice::from_raw_parts(
                errors.GetBufferPointer() as *const u8,
                errors.GetBufferSize(),
            );
            return Err(windows::core::Error::new(
                e.code(),
                String::from_utf8_lossy(text).as_ref(),
            ));
        }
        return Err(e);
    }
    let blob = code.unwrap();
    Ok(
        std::slice::from_raw_parts(blob.GetBufferPointer() as *const u8, blob.GetBufferSize())
            .to_vec(),
    )
}
impl Gpu {
    pub unsafe fn new(hwnd: HWND, x: i32, y: i32, width: u32, height: u32) -> Result<Self> {
        let factory: IDXGIFactory2 = CreateDXGIFactory1()?;
        let mut selected = None;
        for ai in 0..32 {
            let Ok(adapter) = factory.EnumAdapters1(ai) else {
                break;
            };
            for oi in 0..32 {
                let Ok(output) = adapter.EnumOutputs(oi) else {
                    break;
                };
                let desc = output.GetDesc()?;
                if !desc.AttachedToDesktop.as_bool() {
                    continue;
                }
                let r = desc.DesktopCoordinates;
                let hit = x >= r.left && x < r.right && y >= r.top && y < r.bottom;
                if selected.is_none() || hit {
                    selected = Some((adapter.clone(), output));
                }
                if hit {
                    break;
                }
            }
            if let Some((_, out)) = &selected {
                let r = out.GetDesc()?.DesktopCoordinates;
                if x >= r.left && x < r.right && y >= r.top && y < r.bottom {
                    break;
                }
            }
        }
        let (adapter, output) = selected.ok_or_else(windows::core::Error::from_win32)?;
        let mut device = None;
        let mut context = None;
        D3D11CreateDevice(
            &adapter,
            D3D_DRIVER_TYPE_UNKNOWN,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&[D3D_FEATURE_LEVEL_11_1, D3D_FEATURE_LEVEL_11_0]),
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            Some(&mut context),
        )?;
        let device = device.unwrap();
        let context = context.unwrap();
        let multithread: ID3D11Multithread = context.cast()?;
        let _ = multithread.SetMultithreadProtected(true);
        let desc = DXGI_SWAP_CHAIN_DESC1 {
            Width: width,
            Height: height,
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
            BufferCount: 2,
            Scaling: DXGI_SCALING_STRETCH,
            SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
            AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
            Flags: DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT.0 as u32,
            ..Default::default()
        };
        let swap = factory.CreateSwapChainForComposition(&device, &desc, None::<&IDXGIOutput>)?;
        let swap2: IDXGISwapChain2 = swap.cast()?;
        swap2.SetMaximumFrameLatency(1)?;
        let wait = swap2.GetFrameLatencyWaitableObject();
        let swap3: IDXGISwapChain3 = swap.cast()?;
        swap3.SetColorSpace1(DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709)?;
        let dxgi: IDXGIDevice = device.cast()?;
        let composition: IDCompositionDevice = DCompositionCreateDevice(&dxgi)?;
        let window_target = composition.CreateTargetForHwnd(hwnd, true)?;
        let root = composition.CreateVisual()?;
        root.SetContent(&swap)?;
        window_target.SetRoot(&root)?;
        composition.Commit()?;
        let mut vs = None;
        let mut ps = None;
        device.CreateVertexShader(
            &compile(s!("vsMain"), s!("vs_5_0"))?,
            None::<&ID3D11ClassLinkage>,
            Some(&mut vs),
        )?;
        device.CreatePixelShader(
            &compile(s!("psMain"), s!("ps_5_0"))?,
            None::<&ID3D11ClassLinkage>,
            Some(&mut ps),
        )?;
        let mut constants = None;
        device.CreateBuffer(
            &D3D11_BUFFER_DESC {
                ByteWidth: std::mem::size_of::<RenderSnapshot>() as u32,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            },
            None,
            Some(&mut constants),
        )?;
        let mut sampler = None;
        device.CreateSamplerState(
            &D3D11_SAMPLER_DESC {
                Filter: D3D11_FILTER_MIN_MAG_MIP_LINEAR,
                AddressU: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressV: D3D11_TEXTURE_ADDRESS_CLAMP,
                AddressW: D3D11_TEXTURE_ADDRESS_CLAMP,
                MaxLOD: f32::MAX,
                ComparisonFunc: D3D11_COMPARISON_NEVER,
                ..Default::default()
            },
            Some(&mut sampler),
        )?;
        let capture = Capture::new(&device, output.clone())?;
        let mut adaptation_ps = None;
        device.CreatePixelShader(
            &compile(s!("psAdapt"), s!("ps_5_0"))?,
            None::<&ID3D11ClassLinkage>,
            Some(&mut adaptation_ps),
        )?;
        let make_adaptation = || -> Result<(ID3D11RenderTargetView, ID3D11ShaderResourceView)> {
            let mut texture = None;
            device.CreateTexture2D(
                &D3D11_TEXTURE2D_DESC {
                    Width: 1,
                    Height: 1,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                    SampleDesc: DXGI_SAMPLE_DESC {
                        Count: 1,
                        Quality: 0,
                    },
                    Usage: D3D11_USAGE_DEFAULT,
                    BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                    ..Default::default()
                },
                None,
                Some(&mut texture),
            )?;
            let texture = texture.unwrap();
            let mut rtv = None;
            let mut srv = None;
            device.CreateRenderTargetView(&texture, None, Some(&mut rtv))?;
            device.CreateShaderResourceView(&texture, None, Some(&mut srv))?;
            let rtv = rtv.unwrap();
            context.ClearRenderTargetView(&rtv, &[0.0; 4]);
            Ok((rtv, srv.unwrap()))
        };
        let adaptation = [make_adaptation()?, make_adaptation()?];
        let reported_peak = output
            .cast::<IDXGIOutput6>()
            .and_then(|o| o.GetDesc1())
            .map(|d| d.MaxLuminance)
            .unwrap_or(0.0);
        let peak_fallback = !reported_peak.is_finite() || reported_peak <= 0.0;
        let peak_nits = if peak_fallback { 400.0 } else { reported_peak };
        let mut gpu = Self {
            monitor_key: hdr_sdr_widget_lib::win32::display::key_at_point(x,y).unwrap_or_default(),
            device,
            context,
            output,
            capture,
            swap,
            wait,
            target: None,
            _composition: composition,
            _root: root,
            _window_target: window_target,
            vs: vs.unwrap(),
            ps: ps.unwrap(),
            constants: constants.unwrap(),
            sampler: sampler.unwrap(),
            size: (width, height),
            audit_frozen: false,
            adaptation_ps: adaptation_ps.unwrap(),
            adaptation,
            adaptation_index: 0,
            peak_nits,
            peak_fallback,
            white_nits: 80.0,
            highlight_nits: 80.0,
            capability_checked: std::time::Instant::now(),
            peak_target: peak_nits,
        };
        gpu.make_target()?;
        Ok(gpu)
    }
    unsafe fn make_target(&mut self) -> Result<()> {
        let buffer: ID3D11Texture2D = self.swap.GetBuffer(0)?;
        self.device
            .CreateRenderTargetView(&buffer, None, Some(&mut self.target))
    }
    pub unsafe fn resize(&mut self, w: u32, h: u32) -> Result<()> {
        if self.size == (w, h) {
            return Ok(());
        }
        self.context
            .OMSetRenderTargets(None, None::<&ID3D11DepthStencilView>);
        self.target = None;
        self.swap.ResizeBuffers(
            2,
            w,
            h,
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            DXGI_SWAP_CHAIN_FLAG_FRAME_LATENCY_WAITABLE_OBJECT,
        )?;
        self.size = (w, h);
        self.make_target()
    }
    pub unsafe fn wait(&self) {
        if !self.wait.is_invalid() {
            WaitForSingleObject(self.wait, 32);
        }
    }
    pub unsafe fn draw(&mut self, mut snapshot: RenderSnapshot) -> Result<bool> {
        let test_pattern = snapshot.desktop[3];
        self.context.PSSetShaderResources(0, Some(&[None]));
        let fresh = if self.audit_frozen {
            false
        } else {
            self.capture.update(&self.device, &self.context)?
        };
        if let Some(frame) = &self.capture.frame {
            snapshot.desktop = [
                frame.width as f32,
                frame.height as f32,
                if frame.format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                    1.0
                } else if frame.color_space == DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020.0 {
                    2.0
                } else {
                    0.0
                },
                1.0,
            ];
            self.context
                .PSSetShaderResources(0, Some(&[Some(frame.view.clone())]));
        }
        if test_pattern > 1.5 {
            snapshot.desktop[3] = test_pattern;
        }
        snapshot.viewport[2] -= self.capture.origin.0 as f32;
        snapshot.viewport[3] -= self.capture.origin.1 as f32;
        if self.capability_checked.elapsed().as_millis() >= 500 {
            self.capability_checked = std::time::Instant::now();
            let reported = self
                .output
                .cast::<IDXGIOutput6>()
                .and_then(|o| o.GetDesc1())
                .map(|d| d.MaxLuminance)
                .unwrap_or(0.0);
            self.peak_fallback = !reported.is_finite() || reported <= 0.0;
            self.peak_target = if self.peak_fallback { 400.0 } else { reported };
        }
        self.peak_nits += (self.peak_target - self.peak_nits)
            * (1.0 - (-snapshot.timing[0].min(0.1) / 0.15).exp());
        self.white_nits = snapshot.material[2] * 80.0;
        let peak = if snapshot.feedback[3] > 0.5 {
            self.peak_nits / 80.0
        } else {
            1.0
        };
        self.highlight_nits = peak * 0.95 * 80.0;
        snapshot.hdr = [peak * 0.95, peak * 0.65, peak, 0.0];
        self.context.UpdateSubresource(
            &self.constants,
            0,
            None,
            (&snapshot as *const RenderSnapshot).cast(),
            0,
            0,
        );
        self.context.OMSetRenderTargets(
            Some(&[self.target.clone()]),
            None::<&ID3D11DepthStencilView>,
        );
        self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
            Width: self.size.0 as f32,
            Height: self.size.1 as f32,
            MaxDepth: 1.0,
            ..Default::default()
        }]));
        self.context
            .IASetPrimitiveTopology(D3D_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
        self.context.VSSetShader(&self.vs, None);
        self.context.PSSetShader(&self.ps, None);
        self.context
            .PSSetConstantBuffers(0, Some(&[Some(self.constants.clone())]));
        self.context
            .PSSetSamplers(0, Some(&[Some(self.sampler.clone())]));
        // One-pixel temporal adaptation, entirely on GPU. Unbind before reuse as RTV.
        let previous = self.adaptation_index;
        let next = 1 - previous;
        self.context.PSSetShaderResources(1, Some(&[None]));
        self.context.OMSetRenderTargets(
            Some(&[Some(self.adaptation[next].0.clone())]),
            None::<&ID3D11DepthStencilView>,
        );
        self.context
            .PSSetShaderResources(1, Some(&[Some(self.adaptation[previous].1.clone())]));
        self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
            Width: 1.0,
            Height: 1.0,
            MaxDepth: 1.0,
            ..Default::default()
        }]));
        self.context.PSSetShader(&self.adaptation_ps, None);
        self.context.Draw(3, 0);
        self.context.PSSetShaderResources(1, Some(&[None]));
        self.context.OMSetRenderTargets(
            Some(&[self.target.clone()]),
            None::<&ID3D11DepthStencilView>,
        );
        self.context
            .PSSetShaderResources(1, Some(&[Some(self.adaptation[next].1.clone())]));
        self.context.RSSetViewports(Some(&[D3D11_VIEWPORT {
            Width: self.size.0 as f32,
            Height: self.size.1 as f32,
            MaxDepth: 1.0,
            ..Default::default()
        }]));
        self.context.PSSetShader(&self.ps, None);
        self.context.Draw(3, 0);
        self.adaptation_index = next;
        self.swap.Present(1, DXGI_PRESENT(0)).ok()?;
        Ok(fresh)
    }
    pub unsafe fn present_count(&self) -> Option<u32> {
        let mut stats = DXGI_FRAME_STATISTICS::default();
        self.swap
            .GetFrameStatistics(&mut stats)
            .ok()
            .map(|_| stats.PresentCount)
    }
    pub unsafe fn presentation(&self) -> Option<DXGI_FRAME_STATISTICS> {
        let mut s = DXGI_FRAME_STATISTICS::default();
        self.swap.GetFrameStatistics(&mut s).ok().map(|_| s)
    }
    pub unsafe fn prepare_visual_audit(&mut self, hwnd: HWND) {
        if !self.audit_frozen && self.capture.frame.is_some() {
            // Freeze one captured desktop before allowing the external UI screenshot.
            // This explicitly opt-in audit mode must never be used for frame-rate claims.
            self.audit_frozen = true;
            let _ = windows::Win32::UI::WindowsAndMessaging::SetWindowDisplayAffinity(
                hwnd,
                windows::Win32::UI::WindowsAndMessaging::WDA_NONE,
            );
        }
    }
    /// Explicit test-only readback. The production render loop never maps pixels to the CPU.
    pub unsafe fn dump(&self, path: &std::path::Path, white: f32) -> Result<()> {
        let src: ID3D11Texture2D = self.target.as_ref().unwrap().GetResource()?.cast()?;
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut desc);
        desc.Usage = D3D11_USAGE_STAGING;
        desc.BindFlags = 0;
        desc.CPUAccessFlags = D3D11_CPU_ACCESS_READ.0 as u32;
        desc.MiscFlags = 0;
        let mut staging = None;
        self.device
            .CreateTexture2D(&desc, None, Some(&mut staging))?;
        let staging = staging.unwrap();
        self.context.CopyResource(&staging, &src);
        let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
        self.context
            .Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;
        let mut floats = Vec::new();
        let mut bytes = format!("P6\n{} {}\n255\n", desc.Width, desc.Height).into_bytes();
        for y in 0..desc.Height {
            for x in 0..desc.Width {
                let p = (mapped.pData as *const u8).add((y * mapped.RowPitch + x * 8) as usize)
                    as *const u16;
                for c in 0..4 {
                    floats.extend_from_slice(&half_to_float(*p.add(c)).to_le_bytes());
                }
                for c in 0..3 {
                    let linear = (half_to_float(*p.add(c)) / white.max(1.0)).clamp(0.0, 1.0);
                    let srgb = if linear <= 0.0031308 {
                        12.92 * linear
                    } else {
                        1.055 * linear.powf(1.0 / 2.4) - 0.055
                    };
                    bytes.push((srgb * 255.0).round() as u8);
                }
            }
        }
        self.context.Unmap(&staging, 0);
        let meta = serde_json::json!({"width":desc.Width,"height":desc.Height,"format":"little-endian RGBA float32, premultiplied scRGB, top-down","sdrWhiteNits":white*80.0,"peakNits":self.peak_nits,"highlightTargetNits":self.highlight_nits});
        std::fs::write(path.with_extension("json"), meta.to_string()).map_err(|e| {
            windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), e.to_string())
        })?;
        std::fs::write(path.with_extension("rgba32f"), floats).map_err(|e| {
            windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), e.to_string())
        })?;
        std::fs::write(path, bytes).map_err(|e| {
            windows::core::Error::new(windows::core::HRESULT(0x80004005u32 as i32), e.to_string())
        })
    }
}
fn half_to_float(h: u16) -> f32 {
    let sign = if h & 0x8000 != 0 { -1.0 } else { 1.0 };
    let e = ((h >> 10) & 31) as i32;
    let m = (h & 1023) as f32;
    if e == 0 {
        sign * m * 2.0f32.powi(-24)
    } else if e == 31 {
        if m == 0.0 {
            sign * f32::INFINITY
        } else {
            f32::NAN
        }
    } else {
        sign * (1.0 + m / 1024.0) * 2.0f32.powi(e - 15)
    }
}
impl Drop for Gpu {
    fn drop(&mut self) {
        unsafe {
            self.context.ClearState();
            if !self.wait.is_invalid() {
                let _ = CloseHandle(self.wait);
            }
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shaders_compile() {
        unsafe {
            compile(s!("vsMain"), s!("vs_5_0")).unwrap();
            compile(s!("psMain"), s!("ps_5_0")).unwrap();
            compile(s!("psAdapt"), s!("ps_5_0")).unwrap();
        }
    }
}
