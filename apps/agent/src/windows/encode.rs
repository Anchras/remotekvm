use anyhow::Result;
use tokio::sync::mpsc;

/// Video encoder for Windows.
///
/// Priority:
///   1. NVIDIA NVENC (lowest latency, best quality)
///   2. Media Foundation (generic Windows fallback)
pub struct VideoEncoder {
    // TODO: NVENC session or IMFTransform
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub timestamp: u64,
}

impl VideoEncoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        _sender: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        tracing::info!(
            width,
            height,
            fps,
            bitrate_kbps,
            "initializing video encoder"
        );
        // TODO: Detect GPU, prefer NVENC, fallback to Media Foundation
        anyhow::bail!("Video encoder not yet implemented")
    }

    pub fn encode_frame(&self, _frame: &crate::windows::capture::CapturedFrame) -> Result<()> {
        // TODO: Submit frame to encoder, callback pushes EncodedFrame to channel
        anyhow::bail!("Video encoder not yet implemented")
    }
}
