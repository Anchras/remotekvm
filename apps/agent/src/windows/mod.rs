// Windows-specific agent implementation.
//
// Architecture:
//   - Service Controller: runs in Session 0, manages persistent server connection
//   - Session Agent: runs in active user session, handles capture + encode + WebRTC

pub mod audio;
pub mod capture;
pub mod encode;
pub mod input;
pub mod service;

use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc;
use webrtc::api::media_engine::MIME_TYPE_H264;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::{RTCPFeedback, TYPE_RTCP_FB_NACK};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

use crate::signaling::AgentControlState;
use capture::DxgiCapturer;
use encode::{EncodedFrame, VideoEncoder};

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

/// WebRTC H.264 video task bundle. Dropping it aborts background tasks.
pub struct VideoPipeline {
    capturer: Arc<DxgiCapturer>,
    encoder: Arc<VideoEncoder>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl VideoPipeline {
    pub async fn start(
        peer_connection: Arc<RTCPeerConnection>,
        config: VideoConfig,
    ) -> Result<Option<Self>> {
        let (encoded_tx, encoded_rx) = mpsc::channel::<EncodedFrame>(8);
        let encoder = Arc::new(VideoEncoder::new(
            config.width,
            config.height,
            config.fps,
            config.bitrate_kbps,
            encoded_tx,
        )?);
        let capturer = Arc::new(DxgiCapturer::new(config.width, config.height, config.fps)?);

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90_000,
                channels: 0,
                sdp_fmtp_line:
                    "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f"
                        .to_owned(),
                rtcp_feedback: vec![RTCPFeedback {
                    typ: TYPE_RTCP_FB_NACK.to_owned(),
                    parameter: "pli".to_owned(),
                }],
            },
            "video".to_owned(),
            "remotekvm".to_owned(),
        ));

        peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add WebRTC H.264 video track")?;

        let (frame_tx, mut frame_rx) = mpsc::channel(4);
        let capture_task = {
            let capturer = Arc::clone(&capturer);
            tokio::spawn(async move {
                if let Err(error) = capturer.start(frame_tx).await {
                    tracing::warn!(%error, "DXGI capture stopped");
                }
            })
        };

        let encode_task = {
            let encoder = Arc::clone(&encoder);
            tokio::spawn(async move {
                while let Some(frame) = frame_rx.recv().await {
                    if let Err(error) = encoder.encode_frame(&frame) {
                        tracing::warn!(%error, "failed to encode H.264 video frame");
                    }
                }
            })
        };

        let write_task = tokio::spawn(write_h264_samples(
            encoded_rx,
            track,
            Duration::from_secs_f64(1.0 / config.fps as f64),
        ));

        Ok(Some(Self {
            capturer,
            encoder,
            tasks: vec![capture_task, encode_task, write_task],
        }))
    }

    pub fn apply_control_state(&self, state: &AgentControlState) -> Result<()> {
        let target_bitrate = state.bitrate_kbps();
        if target_bitrate != self.encoder.bitrate_kbps() {
            self.encoder.set_bitrate_kbps(target_bitrate)?;
        }
        if state.take_keyframe_request() {
            self.encoder.request_keyframe();
        }
        Ok(())
    }

    pub async fn stop(self) {
        self.capturer.stop();
        for task in self.tasks {
            task.abort();
        }
    }
}

async fn write_h264_samples(
    mut encoded_rx: mpsc::Receiver<EncodedFrame>,
    track: Arc<TrackLocalStaticSample>,
    duration: Duration,
) {
    while let Some(encoded) = encoded_rx.recv().await {
        let sample = Sample {
            data: Bytes::from(encoded.data),
            timestamp: SystemTime::now(),
            duration,
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };
        if let Err(error) = track.write_sample(&sample).await {
            tracing::warn!(
                %error,
                backend = ?encoded.backend,
                is_keyframe = encoded.is_keyframe,
                timestamp = encoded.timestamp,
                "failed to write H.264 video sample"
            );
            return;
        }
    }
}
