// macOS capture + encode pipeline.
//
// Path: ScreenCaptureKit (SCStream) -> CVPixelBuffer -> VTCompressionSession (H.264) -> Annex B NALs.
// Capture and encode share the SCStream sample-handler dispatch queue, so encode is invoked
// synchronously on the capture callback — keeps end-to-end pipeline depth at one frame.

use anyhow::{Context, Result};
use bytes::Bytes;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::mpsc::UnboundedReceiver;
use tokio::task::JoinHandle;
use webrtc::api::media_engine::MIME_TYPE_H264;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::rtp_transceiver::{RTCPFeedback, TYPE_RTCP_FB_NACK};
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;

pub mod annex_b;
pub mod capture;
pub mod encode;

use capture::{Capturer, CapturerConfig};
use encode::{EncodedFrame, Encoder, EncoderConfig};

#[derive(Debug, Clone)]
pub struct VideoConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

pub struct VideoPipeline {
    capturer: Option<Capturer>,
    drain_task: JoinHandle<()>,
}

impl VideoPipeline {
    pub async fn start(
        peer_connection: Arc<RTCPeerConnection>,
        config: VideoConfig,
    ) -> Result<Self> {
        if config.fps == 0 {
            anyhow::bail!("macOS video fps must be non-zero");
        }

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<EncodedFrame>();
        let capturer = tokio::task::spawn_blocking(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let encoder = Arc::new(Encoder::new(
                    EncoderConfig {
                        width: config.width,
                        height: config.height,
                        bitrate_kbps: config.bitrate_kbps,
                    },
                    tx,
                )?);
                Capturer::start(
                    CapturerConfig {
                        width: config.width,
                        height: config.height,
                        fps: config.fps,
                    },
                    encoder,
                )
                .await
            })
        })
        .await??;

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

        if let Err(error) = peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add WebRTC H.264 video track")
        {
            stop_capturer(capturer).await?;
            return Err(error);
        }

        let drain_task = tokio::spawn(write_h264_samples(
            rx,
            track,
            Duration::from_secs_f64(1.0 / config.fps as f64),
        ));

        Ok(Self {
            capturer: Some(capturer),
            drain_task,
        })
    }

    pub async fn stop(mut self) -> Result<()> {
        if let Some(capturer) = self.capturer.take() {
            stop_capturer(capturer).await?;
        }
        self.drain_task.abort();
        Ok(())
    }
}

async fn stop_capturer(capturer: Capturer) -> Result<()> {
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(capturer.stop())
    })
    .await?
}

async fn write_h264_samples(
    mut encoded_rx: UnboundedReceiver<EncodedFrame>,
    track: Arc<TrackLocalStaticSample>,
    duration: Duration,
) {
    let mut frames_seen: u64 = 0;
    while let Some(frame) = encoded_rx.recv().await {
        frames_seen += 1;
        if frames_seen == 1 {
            tracing::info!("macOS ScreenCaptureKit/VideoToolbox pipeline produced H.264 video");
        }

        let pts_value = frame.pts.value;
        let pts_timescale = frame.pts.timescale;
        let sample = Sample {
            data: Bytes::from(frame.data),
            timestamp: SystemTime::now(),
            duration,
            packet_timestamp: 0,
            prev_dropped_packets: 0,
            prev_padding_packets: 0,
        };

        if let Err(error) = track.write_sample(&sample).await {
            tracing::warn!(
                %error,
                is_keyframe = frame.is_keyframe,
                pts_value,
                pts_timescale,
                "failed to write macOS H.264 video sample"
            );
            return;
        }

        tracing::trace!(
            is_keyframe = frame.is_keyframe,
            pts_value,
            pts_timescale,
            "wrote macOS H.264 video sample"
        );
    }
}
