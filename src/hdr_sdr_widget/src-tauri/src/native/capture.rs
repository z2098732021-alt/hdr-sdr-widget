//! GPU-only desktop capture. Cached desktop and widget geometry have independent lifetimes.
use std::time::Instant;
use windows::{
    core::{Interface, Result},
    Graphics::{
        Capture::{
            Direct3D11CaptureFramePool, GraphicsCaptureAccess, GraphicsCaptureAccessKind,
            GraphicsCaptureItem, GraphicsCaptureSession,
        },
        DirectX::{Direct3D11::IDirect3DDevice, DirectXPixelFormat},
        SizeInt32,
    },
    Win32::{
        Graphics::{
            Direct3D11::*,
            Dxgi::{Common::*, *},
            Gdi::HMONITOR,
        },
        System::WinRT::{
            Direct3D11::{CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess},
            Graphics::Capture::IGraphicsCaptureItemInterop,
        },
    },
};
fn qpc_ms() -> f64 {
    let mut count = 0;
    let mut frequency = 0;
    unsafe {
        let _ = windows::Win32::System::Performance::QueryPerformanceCounter(&mut count);
        let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(&mut frequency);
    }
    count as f64 * 1000.0 / frequency.max(1) as f64
}

pub struct CaptureFrame {
    pub texture: ID3D11Texture2D,
    pub view: ID3D11ShaderResourceView,
    pub width: u32,
    pub height: u32,
    pub format: DXGI_FORMAT,
    pub sequence: u64,
    pub acquired: Instant,
    pub source_age_ms: f64,
    pub color_space: i32,
}
enum Source {
    Wgc {
        pool: Direct3D11CaptureFramePool,
        session: GraphicsCaptureSession,
        device: IDirect3DDevice,
        size: SizeInt32,
    },
    Dda(IDXGIOutputDuplication),
}
impl Drop for Source {
    fn drop(&mut self) {
        if let Self::Wgc { pool, session, .. } = self {
            let _ = session.Close();
            let _ = pool.Close();
        }
    }
}
pub struct Capture {
    source: Source,
    pub frame: Option<CaptureFrame>,
    pub backend: String,
    pub fallback_reason: String,
    pub origin: (i32, i32),
    output: IDXGIOutput,
    started: Instant,
}
impl Capture {
    pub unsafe fn new(device: &ID3D11Device, output: IDXGIOutput) -> Result<Self> {
        let desc = output.GetDesc()?;
        let (source, backend, fallback_reason) = match Self::wgc(device, desc.Monitor) {
            Ok(s) => (s, "WGC FP16".into(), String::new()),
            Err(e) => (
                Self::dda(device, &output)?,
                "DDA".into(),
                format!("WGC unavailable: {e}"),
            ),
        };
        Ok(Self {
            source,
            frame: None,
            backend,
            fallback_reason,
            origin: (desc.DesktopCoordinates.left, desc.DesktopCoordinates.top),
            output,
            started: Instant::now(),
        })
    }
    unsafe fn wgc(device: &ID3D11Device, monitor: HMONITOR) -> Result<Source> {
        if super::flag("HSDR_FORCE_DDA") {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(
                0x80004001u32 as i32,
            )));
        }
        if !GraphicsCaptureSession::IsSupported()? {
            return Err(windows::core::Error::from_win32());
        }
        // Unpackaged desktop systems may deny borderless access; DDA is the explicit fallback.
        let access =
            GraphicsCaptureAccess::RequestAccessAsync(GraphicsCaptureAccessKind::Borderless)?
                .get()?;
        if access != windows::Security::Authorization::AppCapabilityAccess::AppCapabilityAccessStatus::Allowed {
            return Err(windows::core::Error::from_hresult(windows::core::HRESULT(0x80070005u32 as i32)));
        }
        let interop: IGraphicsCaptureItemInterop =
            windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()?;
        let item: GraphicsCaptureItem = interop.CreateForMonitor(monitor)?;
        let dxgi: IDXGIDevice = device.cast()?;
        let rt: IDirect3DDevice = CreateDirect3D11DeviceFromDXGIDevice(&dxgi)?.cast()?;
        let size = item.Size()?;
        let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
            &rt,
            DirectXPixelFormat::R16G16B16A16Float,
            2,
            size,
        )?;
        let session = pool.CreateCaptureSession(&item)?;
        session.SetIsCursorCaptureEnabled(false)?;
        // Available on recent Windows; older systems retain their supported default.
        let _ = session.SetMinUpdateInterval(windows::Foundation::TimeSpan { Duration: 0 });
        session.SetIsBorderRequired(false)?;
        if session.IsBorderRequired()? {
            return Err(windows::core::Error::from_win32());
        }
        session.StartCapture()?;
        Ok(Source::Wgc {
            pool,
            session,
            device: rt,
            size,
        })
    }
    unsafe fn dda(device: &ID3D11Device, output: &IDXGIOutput) -> Result<Source> {
        let output5: IDXGIOutput5 = output.cast()?;
        Ok(Source::Dda(output5.DuplicateOutput1(
            device,
            0,
            &[
                DXGI_FORMAT_R16G16B16A16_FLOAT,
                DXGI_FORMAT_R10G10B10A2_UNORM,
                DXGI_FORMAT_B8G8R8A8_UNORM,
            ],
        )?))
    }
    pub unsafe fn update(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
    ) -> Result<bool> {
        let mut source_texture = None;
        match &mut self.source {
            Source::Wgc {
                pool,
                device: rt,
                size,
                ..
            } => {
                // Consume all queued frames, Copy only the freshest one, and release every frame.
                let mut newest: Option<windows::Graphics::Capture::Direct3D11CaptureFrame> = None;
                while let Ok(frame) = pool.TryGetNextFrame() {
                    if let Some(old) = newest.replace(frame) {
                        let _ = old.Close();
                    }
                }
                if let Some(frame) = newest {
                    let source_ms = frame.SystemRelativeTime()?.Duration as f64 / 10_000.0;
                    let next_size = frame.ContentSize()?;
                    if next_size != *size {
                        frame.Close()?;
                        *size = next_size;
                        pool.Recreate(&*rt, DirectXPixelFormat::R16G16B16A16Float, 2, next_size)?;
                        self.frame = None;
                        return Ok(false);
                    }
                    let surface: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
                    let texture: ID3D11Texture2D = surface.GetInterface()?;
                    // Copy while the system frame is still checked out.
                    let result = self.copy_frame(device, context, &texture);
                    frame.Close()?;
                    result?;
                    if let Some(f) = &mut self.frame {
                        f.source_age_ms = (qpc_ms() - source_ms).max(0.0);
                    }
                    return Ok(true);
                }
            }
            Source::Dda(dup) => {
                let mut info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource = None;
                match dup.AcquireNextFrame(0, &mut info, &mut resource) {
                    Ok(()) => {
                        if info.LastPresentTime != 0 {
                            source_texture =
                                resource.and_then(|r| r.cast::<ID3D11Texture2D>().ok());
                        }
                        if let Some(texture) = source_texture.as_ref() {
                            // Clone the COM interface so the mutable capture borrow can end.
                            let release = dup.clone();
                            let result = self.copy_frame(device, context, texture);
                            let _ = release.ReleaseFrame();
                            result?;
                            let mut frequency = 0;
                            let _ = windows::Win32::System::Performance::QueryPerformanceFrequency(
                                &mut frequency,
                            );
                            if let Some(f) = &mut self.frame {
                                f.source_age_ms = (qpc_ms()
                                    - info.LastPresentTime as f64 * 1000.0
                                        / frequency.max(1) as f64)
                                    .max(0.0);
                            }
                            return Ok(true);
                        }
                        dup.ReleaseFrame()?;
                    }
                    Err(e) if e.code() == DXGI_ERROR_WAIT_TIMEOUT => {}
                    Err(e) => return Err(e),
                }
            }
        }
        if self.frame.is_none()
            && self.started.elapsed().as_secs() >= 2
            && matches!(self.source, Source::Wgc { .. })
        {
            self.source = Self::dda(device, &self.output)?;
            self.backend = "DDA".into();
            self.fallback_reason = "WGC produced no frame within 2s".into();
        }
        Ok(false)
    }
    unsafe fn copy_frame(
        &mut self,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        src: &ID3D11Texture2D,
    ) -> Result<()> {
        let mut desc = D3D11_TEXTURE2D_DESC::default();
        src.GetDesc(&mut desc);
        if ![
            DXGI_FORMAT_R16G16B16A16_FLOAT,
            DXGI_FORMAT_R10G10B10A2_UNORM,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            DXGI_FORMAT_R8G8B8A8_UNORM,
        ]
        .contains(&desc.Format)
        {
            return Err(windows::core::Error::new(
                windows::core::HRESULT(0x80004001u32 as i32),
                "Unsupported desktop texture format",
            ));
        }
        let rebuild = self.frame.as_ref().is_none_or(|f| {
            (f.width, f.height, f.format) != (desc.Width, desc.Height, desc.Format)
        });
        if rebuild {
            desc.Usage = D3D11_USAGE_DEFAULT;
            desc.BindFlags = D3D11_BIND_SHADER_RESOURCE.0 as u32;
            desc.CPUAccessFlags = 0;
            desc.MiscFlags = 0;
            let mut texture = None;
            device.CreateTexture2D(&desc, None, Some(&mut texture))?;
            let texture = texture.unwrap();
            let mut view = None;
            device.CreateShaderResourceView(&texture, None, Some(&mut view))?;
            self.frame = Some(CaptureFrame {
                texture,
                view: view.unwrap(),
                width: desc.Width,
                height: desc.Height,
                format: desc.Format,
                sequence: 0,
                acquired: Instant::now(),
                source_age_ms: 0.0,
                color_space: if desc.Format == DXGI_FORMAT_R16G16B16A16_FLOAT {
                    DXGI_COLOR_SPACE_RGB_FULL_G10_NONE_P709.0
                } else if desc.Format == DXGI_FORMAT_R10G10B10A2_UNORM {
                    self.output
                        .cast::<IDXGIOutput6>()
                        .and_then(|o| o.GetDesc1())
                        .map(|d| d.ColorSpace.0)
                        .unwrap_or(DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709.0)
                } else {
                    DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P709.0
                },
            });
        }
        let frame = self.frame.as_mut().unwrap();
        context.CopyResource(&frame.texture, src);
        frame.sequence += 1;
        frame.acquired = Instant::now();
        Ok(())
    }
}
