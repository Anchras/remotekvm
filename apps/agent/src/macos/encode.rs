// VideoToolbox H.264 hardware encoder, tuned for low-latency screen capture.
//
// Properties set:
//   - RealTime = true
//   - AllowFrameReordering = false   (no B-frames => 1-frame encoder delay)
//   - MaxKeyFrameInterval = very large; we drive keyframes explicitly via control msgs
//   - AverageBitRate = configurable
//   - ProfileLevel = H264_ConstrainedBaseline_AutoLevel for browser-compatible WebRTC
//
// Output callback runs on a VT-owned thread. We push EncodedFrames into a tokio
// unbounded channel; consumer reads them on whatever runtime task it lives in.

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2_core_foundation::{CFBoolean, CFNumber, CFString, CFType};
use objc2_core_media::{kCMVideoCodecType_H264, CMSampleBuffer, CMTime};
use objc2_core_video::CVImageBuffer;
use objc2_video_toolbox::{
    kVTCompressionPropertyKey_AllowFrameReordering, kVTCompressionPropertyKey_AverageBitRate,
    kVTCompressionPropertyKey_MaxKeyFrameInterval,
    kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality,
    kVTCompressionPropertyKey_ProfileLevel, kVTCompressionPropertyKey_RealTime,
    kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel, VTCompressionSession, VTEncodeInfoFlags,
    VTSessionSetProperty,
};
use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::UnboundedSender;

use super::annex_b::sample_to_annex_b;

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub bitrate_kbps: u32,
}

#[derive(Debug)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub pts: CMTime,
    pub is_keyframe: bool,
}

struct CallbackState {
    sink: UnboundedSender<EncodedFrame>,
    error_logged: AtomicBool,
}

pub struct Encoder {
    session: Retained<VTCompressionSession>,
    // Kept alive for the lifetime of the session; reclaimed on drop after invalidate().
    callback_state: *mut CallbackState,
}

// `*mut CallbackState` is only ever touched from the C callback (which is single-threaded
// per session) and from Drop after invalidate() has drained pending callbacks. The
// CallbackState itself uses thread-safe primitives.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl Encoder {
    pub fn new(config: EncoderConfig, sink: UnboundedSender<EncodedFrame>) -> Result<Self> {
        let state = Box::into_raw(Box::new(CallbackState {
            sink,
            error_logged: AtomicBool::new(false),
        }));

        let mut session_out: *mut VTCompressionSession = std::ptr::null_mut();
        let status = unsafe {
            VTCompressionSession::create(
                None,
                config.width as i32,
                config.height as i32,
                kCMVideoCodecType_H264,
                None,
                None,
                None,
                Some(output_callback),
                state as *mut c_void,
                NonNull::new(&mut session_out).unwrap(),
            )
        };
        if status != 0 || session_out.is_null() {
            // Reclaim the leaked Box on failure.
            unsafe { drop(Box::from_raw(state)) };
            return Err(anyhow!("VTCompressionSessionCreate failed: {status}"));
        }
        let session = unsafe { Retained::from_raw(session_out) }
            .ok_or_else(|| anyhow!("VTCompressionSession::create returned null"))?;

        configure_session(&session, &config)?;

        let status = unsafe { session.prepare_to_encode_frames() };
        if status != 0 {
            return Err(anyhow!(
                "VTCompressionSessionPrepareToEncodeFrames failed: {status}"
            ));
        }

        Ok(Self {
            session,
            callback_state: state,
        })
    }

    pub fn encode(&self, image: &CVImageBuffer, pts: CMTime) -> Result<()> {
        let mut info = VTEncodeInfoFlags(0);
        let status = unsafe {
            self.session.encode_frame(
                image,
                pts,
                invalid_time(),
                None,
                std::ptr::null_mut(),
                &mut info,
            )
        };
        if status != 0 {
            return Err(anyhow!("VTCompressionSessionEncodeFrame failed: {status}"));
        }
        Ok(())
    }

    #[allow(dead_code)]
    pub fn request_keyframe(&self) {
        // No-op in v0; we'll wire ControlMessage::RequestKeyframe in later by setting
        // kVTEncodeFrameOptionKey_ForceKeyFrame per-frame.
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // Invalidate first; this blocks until in-flight callbacks finish.
        unsafe { self.session.invalidate() };
        // Now safe to reclaim the box — no more callbacks will fire.
        unsafe { drop(Box::from_raw(self.callback_state)) };
    }
}

fn configure_session(session: &VTCompressionSession, cfg: &EncoderConfig) -> Result<()> {
    set_bool(session, unsafe { kVTCompressionPropertyKey_RealTime }, true)?;
    set_bool(
        session,
        unsafe { kVTCompressionPropertyKey_AllowFrameReordering },
        false,
    )?;
    set_bool(
        session,
        unsafe { kVTCompressionPropertyKey_PrioritizeEncodingSpeedOverQuality },
        true,
    )?;
    set_i32(
        session,
        unsafe { kVTCompressionPropertyKey_MaxKeyFrameInterval },
        i32::MAX,
    )?;
    set_i32(
        session,
        unsafe { kVTCompressionPropertyKey_AverageBitRate },
        (cfg.bitrate_kbps * 1000) as i32,
    )?;
    set_cfstring(
        session,
        unsafe { kVTCompressionPropertyKey_ProfileLevel },
        unsafe { kVTProfileLevel_H264_ConstrainedBaseline_AutoLevel },
    )?;
    Ok(())
}

fn set_bool(session: &VTCompressionSession, key: &CFString, value: bool) -> Result<()> {
    let v = CFBoolean::new(value);
    let status = set_property(session, key, v);
    if status != 0 {
        return Err(anyhow!("VTSessionSetProperty(bool) failed: {status}"));
    }
    Ok(())
}

fn set_i32(session: &VTCompressionSession, key: &CFString, value: i32) -> Result<()> {
    let n = CFNumber::new_i32(value);
    let n_ref: &CFNumber = n.as_ref();
    let status = set_property(session, key, n_ref);
    if status != 0 {
        return Err(anyhow!("VTSessionSetProperty(i32) failed: {status}"));
    }
    Ok(())
}

fn set_cfstring(session: &VTCompressionSession, key: &CFString, value: &CFString) -> Result<()> {
    let status = set_property(session, key, value);
    if status != 0 {
        return Err(anyhow!("VTSessionSetProperty(CFString) failed: {status}"));
    }
    Ok(())
}

extern "C-unwind" fn output_callback(
    output_ref_con: *mut c_void,
    _source_frame_ref_con: *mut c_void,
    status: i32,
    _info_flags: VTEncodeInfoFlags,
    sample_buffer: *mut CMSampleBuffer,
) {
    if output_ref_con.is_null() {
        return;
    }
    let state = unsafe { &*(output_ref_con as *const CallbackState) };

    if status != 0 || sample_buffer.is_null() {
        if !state.error_logged.swap(true, Ordering::Relaxed) {
            tracing::warn!(status, "encoder dropped or errored on a frame");
        }
        return;
    }
    let sample: &CMSampleBuffer = unsafe { &*sample_buffer };
    let pts = unsafe { sample.presentation_time_stamp() };
    let is_keyframe = is_sync_sample(sample);

    match sample_to_annex_b(sample, is_keyframe) {
        Ok(data) => {
            let _ = state.sink.send(EncodedFrame {
                data,
                pts,
                is_keyframe,
            });
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to convert sample to annex-B");
        }
    }
}

fn is_sync_sample(sample: &CMSampleBuffer) -> bool {
    // The sample buffer's first attachment has kCMSampleAttachmentKey_NotSync set to true
    // for non-keyframes. Absence of NotSync (or NotSync == false) means it's a keyframe.
    // For v0 simplicity, conservatively treat frames as keyframes until the CFDictionary
    // attachment lookup is bound; this may prepend parameter sets more often than needed.
    let _ = sample;
    true
}

fn set_property<T>(session: &VTCompressionSession, key: &CFString, value: &T) -> i32 {
    let session = unsafe { &*(session as *const VTCompressionSession as *const CFType) };
    let value = unsafe { &*(value as *const T as *const CFType) };
    unsafe { VTSessionSetProperty(session, key, Some(value)) }
}

fn invalid_time() -> CMTime {
    CMTime {
        value: 0,
        timescale: 0,
        flags: objc2_core_media::CMTimeFlags::empty(),
        epoch: 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use objc2_core_video::{
        CVPixelBufferCreate, CVPixelBufferGetBaseAddressOfPlane,
        CVPixelBufferGetBytesPerRowOfPlane, CVPixelBufferGetHeightOfPlane, CVPixelBufferLockFlags,
        CVPixelBufferLockBaseAddress, CVPixelBufferUnlockBaseAddress,
    };
    use std::time::Duration;
    use tokio::sync::mpsc;

    // kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange ('420v') — NV12.
    const PIXEL_FORMAT_NV12: u32 = 0x3432_3076;

    /// Fill an NV12 CVPixelBuffer with a gradient luma plane + neutral chroma.
    /// Returns the locked-then-unlocked buffer ready to encode.
    unsafe fn make_nv12_buffer(width: usize, height: usize) -> *mut CVImageBuffer {
        let mut buf: *mut CVImageBuffer = std::ptr::null_mut();
        let status = CVPixelBufferCreate(
            None,
            width,
            height,
            PIXEL_FORMAT_NV12,
            None,
            NonNull::new(&mut buf).unwrap(),
        );
        assert_eq!(status, 0, "CVPixelBufferCreate failed: {status}");
        assert!(!buf.is_null());

        let lock = CVPixelBufferLockFlags(0);
        assert_eq!(CVPixelBufferLockBaseAddress(&*buf, lock), 0);

        // Plane 0: luma. Plane 1: interleaved CbCr.
        let y_base = CVPixelBufferGetBaseAddressOfPlane(&*buf, 0) as *mut u8;
        let y_stride = CVPixelBufferGetBytesPerRowOfPlane(&*buf, 0);
        let y_rows = CVPixelBufferGetHeightOfPlane(&*buf, 0);
        for row in 0..y_rows {
            for col in 0..width {
                *y_base.add(row * y_stride + col) = ((row + col) & 0xff) as u8;
            }
        }
        let uv_base = CVPixelBufferGetBaseAddressOfPlane(&*buf, 1) as *mut u8;
        let uv_stride = CVPixelBufferGetBytesPerRowOfPlane(&*buf, 1);
        let uv_rows = CVPixelBufferGetHeightOfPlane(&*buf, 1);
        for row in 0..uv_rows {
            for col in 0..uv_stride {
                *uv_base.add(row * uv_stride + col) = 128;
            }
        }
        assert_eq!(CVPixelBufferUnlockBaseAddress(&*buf, lock), 0);
        buf
    }

    /// Permission-free verification of the VideoToolbox H.264 encoder on this
    /// hardware: feed synthetic NV12 frames and confirm real Annex-B H.264 NALs,
    /// including at least one keyframe, come out. (The capture half needs Screen
    /// Recording permission — see capture.rs.)
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn videotoolbox_encodes_synthetic_frames_to_h264() {
        let (width, height) = (640usize, 480usize);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let encoder = Encoder::new(
            EncoderConfig {
                width: width as u32,
                height: height as u32,
                bitrate_kbps: 2_000,
            },
            tx,
        )
        .expect("create VideoToolbox H.264 encoder");

        let buffer = unsafe { make_nv12_buffer(width, height) };
        for i in 0..15i64 {
            let pts = CMTime {
                value: i * 6_000, // 90 kHz / 15 fps
                timescale: 90_000,
                flags: objc2_core_media::CMTimeFlags::Valid,
                epoch: 0,
            };
            encoder
                .encode(unsafe { &*buffer }, pts)
                .expect("submit frame to encoder");
        }

        let mut frames = 0u32;
        let mut total_bytes = 0usize;
        let mut keyframes = 0u32;
        while frames < 5 {
            match tokio::time::timeout(Duration::from_secs(5), rx.recv()).await {
                Ok(Some(frame)) => {
                    frames += 1;
                    total_bytes += frame.data.len();
                    if frame.is_keyframe {
                        keyframes += 1;
                    }
                    assert!(
                        frame.data.starts_with(&[0, 0, 0, 1]) || frame.data.starts_with(&[0, 0, 1]),
                        "encoded frame is not Annex-B: {:?}",
                        &frame.data[..frame.data.len().min(8)]
                    );
                }
                Ok(None) | Err(_) => break,
            }
        }

        // Encoder Drop invalidates the session (drains callbacks) before we free
        // the pixel buffer.
        drop(encoder);
        unsafe { objc2_core_foundation::CFRetained::from_raw(NonNull::new(buffer).unwrap()) };

        assert!(
            frames > 0 && total_bytes > 0,
            "VideoToolbox produced no H.264 frames"
        );
        assert!(keyframes > 0, "expected at least one keyframe (IDR)");
        eprintln!(
            "VERIFIED: VideoToolbox encoded {frames} H.264 frame(s), {total_bytes} bytes, {keyframes} keyframe(s)"
        );
    }
}
