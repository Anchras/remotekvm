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
use objc2::{define_class, msg_send, AllocAnyThread};
use objc2_core_media::{CMSampleBuffer, CMTime};
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::{
    SCContentFilter, SCDisplay, SCShareableContent, SCStream, SCStreamConfiguration,
    SCStreamOutput, SCStreamOutputType,
};
use std::sync::Arc;

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

impl Capturer {
    pub async fn start(config: CapturerConfig, encoder: Arc<Encoder>) -> Result<Self> {
        let content = get_shareable_content().await?;
        let displays: Retained<NSArray<SCDisplay>> = unsafe { content.displays() };
        let display: Retained<SCDisplay> = displays
            .first()
            .ok_or_else(|| {
                anyhow!("no displays available — is Screen Recording permission granted?")
            })?
            .retain();

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
                flags: objc2_core_media::CMTimeFlags::VALID,
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
        let mut tx_cell = Some(tx);
        let block = RcBlock::new(move |err: *mut NSError| {
            let retained = if err.is_null() {
                None
            } else {
                unsafe { Retained::retain(err) }
            };
            if let Some(tx) = tx_cell.take() {
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
        let mut tx_cell = Some(tx);
        let block = RcBlock::new(move |err: *mut NSError| {
            let retained = if err.is_null() {
                None
            } else {
                unsafe { Retained::retain(err) }
            };
            if let Some(tx) = tx_cell.take() {
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
    let mut tx_cell = Some(tx);
    let block = RcBlock::new(move |content: *mut SCShareableContent, err: *mut NSError| {
        let result = if !err.is_null() {
            Err(unsafe { Retained::retain(err) }.expect("non-null NSError must retain"))
        } else if !content.is_null() {
            Ok(unsafe { Retained::retain(content) }.expect("non-null content must retain"))
        } else {
            // Shouldn't happen per Apple's contract, but bail rather than panic.
            return;
        };
        if let Some(tx) = tx_cell.take() {
            let _ = tx.send(result);
        }
    });
    unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&block) };
    rx.await?
        .map_err(|e| anyhow!("SCShareableContent fetch failed: {:?}", e))
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
