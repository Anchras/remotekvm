use anyhow::Result;

/// WASAPI loopback audio capture.
///
/// Captures system audio output (what the user hears) and encodes to Opus.
pub struct AudioCapture {
    // TODO: WASAPI IAudioClient, IAudioCaptureClient
}

pub struct AudioPacket {
    pub pcm_data: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("initializing WASAPI audio capture");
        // TODO: Activate audio client on default render endpoint with loopback
        anyhow::bail!("Audio capture not yet implemented")
    }

    pub async fn start(&self, _sender: tokio::sync::mpsc::Sender<AudioPacket>) -> Result<()> {
        // TODO: Initialize loopback capture, read PCM, send packets
        anyhow::bail!("Audio capture not yet implemented")
    }
}

/// Opus encoder for audio.
pub struct OpusEncoder {
    // TODO: opus::Encoder
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate_kbps: u32) -> Result<Self> {
        tracing::info!(
            sample_rate,
            channels,
            bitrate_kbps,
            "initializing Opus encoder"
        );
        // TODO: Create opus encoder
        anyhow::bail!("Opus encoder not yet implemented")
    }

    pub fn encode(&self, _packet: &AudioPacket) -> Result<Vec<u8>> {
        // TODO: Encode PCM to Opus
        anyhow::bail!("Opus encoder not yet implemented")
    }
}
