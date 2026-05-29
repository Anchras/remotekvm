use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;

/// DXGI Desktop Duplication capture.
///
/// Captures the primary display via IDXGIOutputDuplication.
pub struct DxgiCapturer {
    // TODO: DXGI resources (device, output duplication)
}

impl DxgiCapturer {
    pub fn new(width: u32, height: u32, fps: u32) -> Result<Self> {
        tracing::info!(width, height, fps, "initializing DXGI capture");
        // TODO: Create D3D11 device, get adapter output, create duplication
        anyhow::bail!("DXGI capture not yet implemented")
    }

    pub async fn start(&self, _sender: mpsc::Sender<CapturedFrame>) -> Result<()> {
        // TODO: Run capture loop in a thread, send frames via channel
        anyhow::bail!("DXGI capture not yet implemented")
    }
}

pub struct CapturedFrame {
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub format: PixelFormat,
    pub timestamp: std::time::Instant,
}

pub enum PixelFormat {
    Bgra8,
    Rgba8,
    Nv12,
}
