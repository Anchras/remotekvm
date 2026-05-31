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
