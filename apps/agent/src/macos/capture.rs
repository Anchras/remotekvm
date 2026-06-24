// ScreenCaptureKit wrapper.
//
// Owns an SCStream + delegate (FrameSink). The delegate is an NSObject subclass defined
// via `define_class!`; on each captured CMSampleBuffer it pulls out the CVPixelBuffer
// and PTS and synchronously calls Encoder::encode. That keeps capture → encode at one
// hop on the same dispatch queue.

use anyhow::{anyhow, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchQueueAttr};
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{define_class, msg_send, AllocAnyThread, DefinedClass};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};
use std::sync::{Arc, Mutex};

use super::encode::Encoder;

// kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ('420v') — NV12, what VideoToolbox prefers.
const PIXEL_FORMAT_NV12_VIDEO_RANGE: u32 = 0x34323076;

pub struct CapturerConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

pub struct Capturer {
    _delegate: Retained<FrameSink>,
    stream: Retained<SCStream>,
}

// SCStream callbacks are delivered on the serial dispatch queue we provide, and
// session teardown is explicit via `stop`; keeping the guard in the async agent
// task matches the current single-process macOS lifecycle.
unsafe impl Send for Capturer {}

impl Capturer {
    pub async fn start(config: CapturerConfig, encoder: Arc<Encoder>) -> Result<Self> {
        let content = get_shareable_content().await?;
        let displays: Retained<NSArray<SCDisplay>> = unsafe { content.displays() };
        let display: Retained<SCDisplay> = displays.firstObject().ok_or_else(|| {
            anyhow!("no displays available — is Screen Recording permission granted?")
        })?;

        let filter: Retained<SCContentFilter> = unsafe {
            let alloc = SCContentFilter::alloc();
            let empty: Retained<NSArray<NSObject>> = NSArray::new();
            msg_send![alloc, initWithDisplay: &*display, excludingWindows: &*empty]
        };

        let stream_config: Retained<SCStreamConfiguration> =
            unsafe { SCStreamConfiguration::new() };
        unsafe {
            stream_config.setWidth(config.width as usize);
            stream_config.setHeight(config.height as usize);
            stream_config.setMinimumFrameInterval(CMTime {
                value: 1,
                timescale: config.fps as i32,
                flags: objc2_core_media::CMTimeFlags::Valid,
                epoch: 0,
            });
            stream_config.setShowsCursor(true);
            stream_config.setPixelFormat(PIXEL_FORMAT_NV12_VIDEO_RANGE);
        }

        let delegate: Retained<FrameSink> = {
            let alloc = FrameSink::alloc().set_ivars(FrameSinkIvars { encoder });
            unsafe { msg_send![super(alloc), init] }
        };

        let stream: Retained<SCStream> = unsafe {
            let alloc = SCStream::alloc();
            SCStream::initWithFilter_configuration_delegate(alloc, &filter, &stream_config, None)
        };

        let proto: &ProtocolObject<dyn SCStreamOutput> = ProtocolObject::from_ref(&*delegate);
        let queue = DispatchQueue::new("io.adant.remotekvm.capture", DispatchQueueAttr::SERIAL);

        unsafe {
            stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    proto,
                    SCStreamOutputType::Screen,
                    Some(&queue),
                )
                .map_err(|e| anyhow!("addStreamOutput failed: {:?}", e))?;
        }

        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Retained<NSError>>>();
        let tx_cell = Mutex::new(Some(tx));
        let block = RcBlock::new(move |err: *mut NSError| {
            let retained = if err.is_null() {
                None
            } else {
                unsafe { Retained::retain(err) }
            };
            if let Some(tx) = tx_cell.lock().unwrap().take() {
                let _ = tx.send(retained);
            }
        });
        unsafe { stream.startCaptureWithCompletionHandler(Some(&block)) };
        if let Some(err) = rx.await? {
            return Err(anyhow!("startCapture failed: {:?}", err));
        }

        Ok(Self {
            _delegate: delegate,
            stream,
        })
    }

    pub async fn stop(self) -> Result<()> {
        let (tx, rx) = tokio::sync::oneshot::channel::<Option<Retained<NSError>>>();
        let tx_cell = Mutex::new(Some(tx));
        let block = RcBlock::new(move |err: *mut NSError| {
            let retained = if err.is_null() {
                None
            } else {
                unsafe { Retained::retain(err) }
            };
            if let Some(tx) = tx_cell.lock().unwrap().take() {
                let _ = tx.send(retained);
            }
        });
        unsafe { self.stream.stopCaptureWithCompletionHandler(Some(&block)) };
        if let Some(err) = rx.await? {
            return Err(anyhow!("stopCapture failed: {:?}", err));
        }
        Ok(())
    }
}

async fn get_shareable_content() -> Result<Retained<SCShareableContent>> {
    let (tx, rx) =
        tokio::sync::oneshot::channel::<Result<Retained<SCShareableContent>, Retained<NSError>>>();
    let tx_cell = Mutex::new(Some(tx));
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let result = if !err.is_null() {
            Err(unsafe { Retained::retain(err) }.expect("non-null NSError must retain"))
        } else if !content.is_null() {
            Ok(unsafe { Retained::retain(content) }.expect("non-null content must retain"))
        } else {
            // Shouldn't happen per Apple's contract, but bail rather than panic.
            return;
        };
        if let Some(tx) = tx_cell.lock().unwrap().take() {
            let _ = tx.send(result);
        }
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.await?
        .map_err(|e| anyhow!("SCShareableContent fetch failed: {:?}", e))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::macos::encode::{Encoder, EncoderConfig};
    use std::time::Duration;
    use tokio::sync::mpsc;

    /// End-to-end on real hardware: ScreenCaptureKit captures the live display,
    /// VideoToolbox encodes H.264, and we confirm non-empty Annex-B frames arrive.
    ///
    /// Requires **Screen Recording permission** for the test runner. If no display
    /// is shareable (permission not granted / headless), the test skips with a
    /// message rather than failing — there is nothing to capture.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn captures_and_encodes_real_screen_to_h264() {
        // Probe permission first so a denial reads as a skip, not a hard failure.
        match get_shareable_content().await {
            Ok(content) => {
                let displays = unsafe { content.displays() };
                if displays.firstObject().is_none() {
                    eprintln!(
                        "SKIP: no shareable display (grant Screen Recording permission to verify capture)"
                    );
                    return;
                }
            }
            Err(e) => {
                eprintln!("SKIP: cannot list shareable content ({e}); Screen Recording permission likely not granted");
                return;
            }
        }

        let (tx, mut rx) = mpsc::unbounded_channel();
        let encoder = Arc::new(
            Encoder::new(
                EncoderConfig {
                    width: 640,
                    height: 480,
                    bitrate_kbps: 2_000,
                },
                tx,
            )
            .expect("create VideoToolbox H.264 encoder"),
        );

        let capturer = Capturer::start(
            CapturerConfig {
                width: 640,
                height: 480,
                fps: 15,
            },
            encoder,
        )
        .await
        .expect("start ScreenCaptureKit capture");

        // Collect a few encoded frames within a bounded window.
        let mut frames = 0u32;
        let mut total_bytes = 0usize;
        let mut keyframes = 0u32;
        let overall = tokio::time::Instant::now();
        while frames < 5 && overall.elapsed() < Duration::from_secs(8) {
            match tokio::time::timeout(Duration::from_secs(3), rx.recv()).await {
                Ok(Some(frame)) => {
                    frames += 1;
                    total_bytes += frame.data.len();
                    if frame.is_keyframe {
                        keyframes += 1;
                    }
                    // Annex-B access units must start with a 00 00 00 01 / 00 00 01 start code.
                    assert!(
                        frame.data.starts_with(&[0, 0, 0, 1]) || frame.data.starts_with(&[0, 0, 1]),
                        "encoded frame is not Annex-B (no start code): {:?}",
                        &frame.data[..frame.data.len().min(8)]
                    );
                }
                Ok(None) => break,
                Err(_) => break,
            }
        }

        capturer.stop().await.expect("stop capture");

        assert!(
            frames > 0 && total_bytes > 0,
            "no H.264 frames produced from live capture"
        );
        eprintln!(
            "VERIFIED: live capture produced {frames} H.264 frame(s), {total_bytes} bytes, {keyframes} keyframe(s)"
        );
    }
}

pub struct FrameSinkIvars {
    encoder: Arc<Encoder>,
}

define_class!(
    #[unsafe(super(NSObject))]
    #[ivars = FrameSinkIvars]
    struct FrameSink;

    unsafe impl NSObjectProtocol for FrameSink {}

    unsafe impl SCStreamOutput for FrameSink {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        unsafe fn stream_did_output_sample_buffer_of_type(
            &self,
            _stream: &SCStream,
            sample_buffer: &CMSampleBuffer,
            output_type: SCStreamOutputType,
        ) {
            if output_type != SCStreamOutputType::Screen {
                return;
            }
            let image = match sample_buffer.image_buffer() {
                Some(b) => b,
                None => return, // SCK occasionally delivers status-only samples with no image.
            };
            let pts = sample_buffer.presentation_time_stamp();
            if let Err(e) = self.ivars().encoder.encode(&image, pts) {
                tracing::warn!(error = %e, "encoder rejected frame");
            }
        }
    }
);
