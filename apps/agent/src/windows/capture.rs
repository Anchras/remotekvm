use anyhow::{Context, Result};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::sync::mpsc;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_CPU_ACCESS_READ, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_MAPPED_SUBRESOURCE,
    D3D11_MAP_READ, D3D11_SDK_VERSION, D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC};
use windows::Win32::Graphics::Dxgi::{
    IDXGIAdapter, IDXGIDevice, IDXGIOutput1, IDXGIOutputDuplication, IDXGIResource,
    DXGI_ERROR_ACCESS_LOST, DXGI_ERROR_WAIT_TIMEOUT, DXGI_OUTDUPL_FRAME_INFO,
};

const DXGI_FRAME_TIMEOUT_MS: u32 = 100;

/// DXGI Desktop Duplication capture.
///
/// Captures the primary display via IDXGIOutputDuplication and returns CPU-readable
/// BGRA frames for the encoder. The D3D/DXGI objects are created inside the
/// blocking capture thread and stay on that thread for their lifetime.
pub struct DxgiCapturer {
    width: u32,
    height: u32,
    fps: u32,
    stop: Arc<AtomicBool>,
}

impl DxgiCapturer {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        if width == 0 || height == 0 {
            anyhow::bail!("capture dimensions must be non-zero");
        }
        if fps == 0 {
            anyhow::bail!("capture fps must be non-zero");
        }

        tracing::info!(width, height, fps, "initializing DXGI capture");
        Ok(Self {
            width,
            height,
            fps,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub async fn start(&self, sender: mpsc::Sender<CapturedFrame>) -> Result<()> {
        let config = CaptureConfig {
            width: self.width,
            height: self.height,
            fps: self.fps,
        };
        let stop = Arc::clone(&self.stop);
        tokio::task::spawn_blocking(move || capture_loop(config, sender, stop))
            .await
            .context("DXGI capture task panicked")?
    }
}

#[derive(Debug, Clone, Copy)]
struct CaptureConfig {
    width: u32,
    height: u32,
    fps: u32,
}

pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub timestamp: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Bgra8,
    Rgba8,
    Nv12,
}

struct DxgiSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    staging: ID3D11Texture2D,
    output_width: u32,
    output_height: u32,
}

fn capture_loop(
    config: CaptureConfig,
    sender: mpsc::Sender<CapturedFrame>,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let mut session = DxgiSession::new(config)?;
    let frame_interval = Duration::from_secs_f64(1.0 / config.fps as f64);
    let mut next_frame = Instant::now();

    while !stop.load(Ordering::Acquire) {
        match session.capture_frame() {
            Ok(Some(frame)) => {
                if sender.blocking_send(frame).is_err() {
                    break;
                }
            }
            Ok(None) => {}
            Err(error) if is_dxgi_access_lost(&error) => {
                tracing::warn!(%error, "DXGI duplication access lost; recreating capture session");
                session = DxgiSession::new(config)?;
            }
            Err(error) => return Err(error),
        }

        next_frame += frame_interval;
        let now = Instant::now();
        if next_frame > now {
            std::thread::sleep(next_frame - now);
        } else {
            next_frame = now;
        }
    }

    Ok(())
}

impl DxgiSession {
    fn new(config: CaptureConfig) -> Result<Self> {
        let mut device = None;
        let mut context = None;
        let feature_levels = [D3D_FEATURE_LEVEL_11_0];
        let mut selected_level = D3D_FEATURE_LEVEL::default();

        unsafe {
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                None,
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                Some(&feature_levels),
                D3D11_SDK_VERSION,
                Some(&mut device),
                Some(&mut selected_level),
                Some(&mut context),
            )
            .context("create D3D11 device for DXGI capture")?;
        }

        let device = device.context("D3D11CreateDevice returned no device")?;
        let context = context.context("D3D11CreateDevice returned no immediate context")?;
        let dxgi_device: IDXGIDevice = device.cast().context("cast D3D11 device to IDXGIDevice")?;
        let adapter: IDXGIAdapter =
            unsafe { dxgi_device.GetAdapter().context("get DXGI adapter")? };
        let output = unsafe {
            adapter
                .EnumOutputs(0)
                .context("enumerate primary DXGI output")?
        };
        let output1: IDXGIOutput1 = output
            .cast()
            .context("cast primary output to IDXGIOutput1")?;
        let duplication = unsafe {
            output1
                .DuplicateOutput(&device)
                .context("duplicate primary DXGI output")?
        };
        let dupl_desc = unsafe { duplication.GetDesc() };

        let output_width = dupl_desc.ModeDesc.Width;
        let output_height = dupl_desc.ModeDesc.Height;
        if output_width == 0 || output_height == 0 {
            anyhow::bail!("DXGI output returned zero-sized desktop");
        }

        if output_width != config.width || output_height != config.height {
            tracing::info!(
                requested_width = config.width,
                requested_height = config.height,
                output_width,
                output_height,
                "DXGI capture will use native output size"
            );
        }

        let desc = D3D11_TEXTURE2D_DESC {
            Width: output_width,
            Height: output_height,
            MipLevels: 1,
            ArraySize: 1,
            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Usage: D3D11_USAGE_STAGING,
            BindFlags: 0,
            CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
            MiscFlags: 0,
        };

        let mut staging = None;
        unsafe {
            device
                .CreateTexture2D(&desc, None, Some(&mut staging))
                .context("create CPU-readable DXGI staging texture")?;
        }
        let staging = staging.context("CreateTexture2D returned no staging texture")?;

        tracing::info!(
            width = output_width,
            height = output_height,
            ?selected_level,
            "DXGI capture session ready"
        );

        Ok(Self {
            device,
            context,
            duplication,
            staging,
            output_width,
            output_height,
        })
    }

    fn capture_frame(&mut self) -> Result<Option<CapturedFrame>> {
        let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
        let mut resource = None;
        let acquired = unsafe {
            self.duplication
                .AcquireNextFrame(DXGI_FRAME_TIMEOUT_MS, &mut frame_info, &mut resource)
        };

        match acquired {
            Ok(()) => {}
            Err(error) if error.code() == DXGI_ERROR_WAIT_TIMEOUT => return Ok(None),
            Err(error) => return Err(error).context("acquire next DXGI frame"),
        }

        let result = (|| {
            let resource: IDXGIResource = resource.context("DXGI returned no frame resource")?;
            let texture: ID3D11Texture2D =
                resource.cast().context("cast DXGI resource to texture")?;
            let src: ID3D11Resource = texture.cast().context("cast frame texture to resource")?;
            let dst: ID3D11Resource = self
                .staging
                .cast()
                .context("cast staging texture to resource")?;

            unsafe {
                self.context.CopyResource(&dst, &src);
            }

            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            unsafe {
                self.context
                    .Map(&dst, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .context("map DXGI staging texture")?;
            }

            let frame = copy_mapped_bgra(
                &mapped,
                self.output_width,
                self.output_height,
                frame_info.LastPresentTime as u64,
            );

            unsafe {
                self.context.Unmap(&dst, 0);
            }

            frame
        })();

        unsafe {
            self.duplication
                .ReleaseFrame()
                .context("release DXGI frame")?;
        }

        result.map(Some)
    }
}

fn copy_mapped_bgra(
    mapped: &D3D11_MAPPED_SUBRESOURCE,
    width: u32,
    height: u32,
    _present_time: u64,
) -> Result<CapturedFrame> {
    let row_bytes = width as usize * 4;
    let row_pitch = mapped.RowPitch as usize;
    if mapped.pData.is_null() {
        anyhow::bail!("DXGI mapped frame has null data pointer");
    }
    if row_pitch < row_bytes {
        anyhow::bail!("DXGI row pitch {row_pitch} is smaller than row bytes {row_bytes}");
    }

    let mut data = vec![0; row_bytes * height as usize];
    unsafe {
        let src = mapped.pData.cast::<u8>();
        for row in 0..height as usize {
            let src_row = src.add(row * row_pitch);
            let dst_row = &mut data[row * row_bytes..(row + 1) * row_bytes];
            std::ptr::copy_nonoverlapping(src_row, dst_row.as_mut_ptr(), row_bytes);
        }
    }

    Ok(CapturedFrame {
        data,
        width,
        height,
        format: PixelFormat::Bgra8,
        timestamp: Instant::now(),
    })
}

fn is_dxgi_access_lost(error: &anyhow::Error) -> bool {
    error
        .chain()
        .filter_map(|cause| cause.downcast_ref::<windows::core::Error>())
        .any(|error| error.code() == DXGI_ERROR_ACCESS_LOST)
}
