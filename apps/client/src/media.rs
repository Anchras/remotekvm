//! Client-side media receive plumbing.
//!
//! The current agent-side media path is still settling, but the client already
//! negotiates recv-only audio/video sections and runs inbound RTP through
//! explicit decoder/sink boundaries. Platform decoders can advance behind these
//! traits without changing the WebRTC signaling path.

#![allow(dead_code)]

use std::collections::VecDeque;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::mpsc as std_mpsc;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::Bytes;
use remotekvm_transport::PeerSession;
use rtp::header::Header as RtpHeader;
use rtp::packet::Packet as RtpPacket;
use webrtc::rtp_transceiver::rtp_codec::RTPCodecType;
use webrtc::rtp_transceiver::rtp_transceiver_direction::RTCRtpTransceiverDirection;
use webrtc::rtp_transceiver::RTCRtpTransceiverInit;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoPixelFormat {
    Rgba8,
    Bgra8,
    Nv12,
    I420,
}

#[derive(Debug, Clone)]
pub struct DecodedVideoFrame {
    pub width: u32,
    pub height: u32,
    pub format: VideoPixelFormat,
    pub pts: Option<Duration>,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioSampleFormat {
    F32Interleaved,
    I16Interleaved,
}

#[derive(Debug, Clone)]
pub struct DecodedAudioFrame {
    pub sample_rate_hz: u32,
    pub channels: u16,
    pub format: AudioSampleFormat,
    pub pts: Option<Duration>,
    pub data: Bytes,
}

#[derive(Debug, Clone)]
pub struct EncodedMediaPacket {
    pub payload: Bytes,
    pub payload_type: u8,
    pub sequence_number: u16,
    pub timestamp: u32,
    pub marker: bool,
}

pub trait VideoFrameSink: Send + Sync + 'static {
    fn on_video_frame(&self, frame: DecodedVideoFrame);
}

pub trait AudioFrameSink: Send + Sync + 'static {
    fn on_audio_frame(&self, frame: DecodedAudioFrame);
}

pub trait VideoDecoder: Send + Sync + 'static {
    fn decode(&self, packet: EncodedMediaPacket) -> Result<Option<DecodedVideoFrame>>;
}

pub trait AudioDecoder: Send + Sync + 'static {
    fn decode(&self, packet: EncodedMediaPacket) -> Result<Option<DecodedAudioFrame>>;
}

#[derive(Debug, Default)]
pub struct LatestVideoFrameSink {
    latest: Mutex<Option<DecodedVideoFrame>>,
}

impl LatestVideoFrameSink {
    pub fn latest(&self) -> Option<DecodedVideoFrame> {
        self.latest.lock().ok().and_then(|frame| frame.clone())
    }
}

impl VideoFrameSink for LatestVideoFrameSink {
    fn on_video_frame(&self, frame: DecodedVideoFrame) {
        if let Ok(mut latest) = self.latest.lock() {
            *latest = Some(frame);
        }
    }
}

#[derive(Debug, Default)]
pub struct BufferedAudioSink {
    frames: Mutex<Vec<DecodedAudioFrame>>,
}

impl BufferedAudioSink {
    pub fn drain(&self) -> Vec<DecodedAudioFrame> {
        self.frames
            .lock()
            .map(|mut frames| frames.drain(..).collect())
            .unwrap_or_default()
    }
}

impl AudioFrameSink for BufferedAudioSink {
    fn on_audio_frame(&self, frame: DecodedAudioFrame) {
        if let Ok(mut frames) = self.frames.lock() {
            frames.push(frame);
        }
    }
}

#[derive(Debug, Default)]
pub struct NoopVideoDecoder;

impl VideoDecoder for NoopVideoDecoder {
    fn decode(&self, _packet: EncodedMediaPacket) -> Result<Option<DecodedVideoFrame>> {
        Ok(None)
    }
}

#[derive(Debug, Default)]
pub struct NoopAudioDecoder;

impl AudioDecoder for NoopAudioDecoder {
    fn decode(&self, _packet: EncodedMediaPacket) -> Result<Option<DecodedAudioFrame>> {
        Ok(None)
    }
}

const OPUS_SAMPLE_RATE_HZ: u32 = 48_000;
const OPUS_CHANNELS: u16 = 2;
const OPUS_MAX_FRAME_SAMPLES_PER_CHANNEL: usize = 5_760;
const AUDIO_QUEUE_SECONDS: usize = 2;

type OpusDecoderCreate =
    unsafe extern "C" fn(sample_rate_hz: c_int, channels: c_int, error: *mut c_int) -> *mut c_void;
type OpusDecoderDestroy = unsafe extern "C" fn(decoder: *mut c_void);
type OpusDecodeFloat = unsafe extern "C" fn(
    decoder: *mut c_void,
    data: *const u8,
    len: c_int,
    pcm: *mut f32,
    frame_size: c_int,
    decode_fec: c_int,
) -> c_int;
type OpusStrError = unsafe extern "C" fn(error: c_int) -> *const c_char;

struct OpusLibrary {
    _library: libloading::Library,
    decoder_create: OpusDecoderCreate,
    decoder_destroy: OpusDecoderDestroy,
    decode_float: OpusDecodeFloat,
    str_error: OpusStrError,
}

unsafe impl Send for OpusLibrary {}
unsafe impl Sync for OpusLibrary {}

struct DynamicOpusDecoderState {
    library: Arc<OpusLibrary>,
    decoder: *mut c_void,
}

unsafe impl Send for DynamicOpusDecoderState {}

impl Drop for DynamicOpusDecoderState {
    fn drop(&mut self) {
        unsafe {
            (self.library.decoder_destroy)(self.decoder);
        }
    }
}

pub struct OpusAudioDecoder {
    state: Mutex<DynamicOpusDecoderState>,
}

impl OpusAudioDecoder {
    pub fn new() -> Result<Self> {
        let library = Arc::new(load_opus_library()?);
        let mut error = 0;
        let decoder = unsafe {
            (library.decoder_create)(
                OPUS_SAMPLE_RATE_HZ as c_int,
                OPUS_CHANNELS as c_int,
                &mut error,
            )
        };
        if decoder.is_null() || error != 0 {
            anyhow::bail!("create Opus decoder: {}", opus_error(&library, error));
        }
        Ok(Self {
            state: Mutex::new(DynamicOpusDecoderState { library, decoder }),
        })
    }
}

impl AudioDecoder for OpusAudioDecoder {
    fn decode(&self, packet: EncodedMediaPacket) -> Result<Option<DecodedAudioFrame>> {
        let mut pcm = vec![0.0f32; OPUS_MAX_FRAME_SAMPLES_PER_CHANNEL * OPUS_CHANNELS as usize];
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Opus decoder mutex poisoned"))?;
        let frames = unsafe {
            (state.library.decode_float)(
                state.decoder,
                packet.payload.as_ptr(),
                packet.payload.len() as c_int,
                pcm.as_mut_ptr(),
                OPUS_MAX_FRAME_SAMPLES_PER_CHANNEL as c_int,
                0,
            )
        };
        if frames < 0 {
            anyhow::bail!(
                "decode Opus RTP payload: {}",
                opus_error(&state.library, frames)
            );
        }
        let frames = frames as usize;
        let samples = frames * OPUS_CHANNELS as usize;
        pcm.truncate(samples);

        let mut data = Vec::with_capacity(samples * std::mem::size_of::<f32>());
        for sample in pcm {
            data.extend_from_slice(&sample.to_ne_bytes());
        }

        Ok(Some(DecodedAudioFrame {
            sample_rate_hz: OPUS_SAMPLE_RATE_HZ,
            channels: OPUS_CHANNELS,
            format: AudioSampleFormat::F32Interleaved,
            pts: Some(Duration::from_secs_f64(
                packet.timestamp as f64 / OPUS_SAMPLE_RATE_HZ as f64,
            )),
            data: Bytes::from(data),
        }))
    }
}

fn load_opus_library() -> Result<OpusLibrary> {
    let mut errors = Vec::new();
    for candidate in opus_library_candidates() {
        match unsafe { libloading::Library::new(candidate) } {
            Ok(library) => return unsafe { opus_library_from(library) },
            Err(error) => errors.push(format!("{candidate}: {error}")),
        }
    }
    anyhow::bail!("libopus not available ({})", errors.join("; "))
}

fn opus_library_candidates() -> &'static [&'static str] {
    #[cfg(target_os = "windows")]
    {
        &["opus.dll", "libopus-0.dll"]
    }
    #[cfg(target_os = "macos")]
    {
        &[
            "libopus.dylib",
            "/opt/homebrew/lib/libopus.dylib",
            "/usr/local/lib/libopus.dylib",
        ]
    }
    #[cfg(all(unix, not(target_os = "macos")))]
    {
        &["libopus.so.0", "libopus.so"]
    }
    #[cfg(not(any(windows, unix)))]
    {
        &[]
    }
}

unsafe fn opus_library_from(library: libloading::Library) -> Result<OpusLibrary> {
    let decoder_create = *library
        .get::<OpusDecoderCreate>(b"opus_decoder_create\0")
        .context("load opus_decoder_create")?;
    let decoder_destroy = *library
        .get::<OpusDecoderDestroy>(b"opus_decoder_destroy\0")
        .context("load opus_decoder_destroy")?;
    let decode_float = *library
        .get::<OpusDecodeFloat>(b"opus_decode_float\0")
        .context("load opus_decode_float")?;
    let str_error = *library
        .get::<OpusStrError>(b"opus_strerror\0")
        .context("load opus_strerror")?;

    Ok(OpusLibrary {
        _library: library,
        decoder_create,
        decoder_destroy,
        decode_float,
        str_error,
    })
}

fn opus_error(library: &OpusLibrary, error: c_int) -> String {
    let ptr = unsafe { (library.str_error)(error) };
    if ptr.is_null() {
        return format!("libopus error {error}");
    }
    unsafe { CStr::from_ptr(ptr) }
        .to_string_lossy()
        .into_owned()
}

#[derive(Debug, Clone)]
struct AudioPlaybackConfig {
    sample_rate_hz: u32,
    channels: u16,
    max_samples: usize,
}

#[derive(Debug)]
struct AudioPlaybackBuffer {
    config: AudioPlaybackConfig,
    samples: VecDeque<f32>,
}

pub struct CpalAudioSink {
    sender: Option<std_mpsc::Sender<DecodedAudioFrame>>,
}

impl CpalAudioSink {
    pub fn new() -> Self {
        match Self::try_new() {
            Ok(sink) => sink,
            Err(error) => {
                tracing::warn!(%error, "audio playback disabled");
                Self { sender: None }
            }
        }
    }

    fn try_new() -> Result<Self> {
        let (frame_tx, frame_rx) = std_mpsc::channel();
        let (init_tx, init_rx) = std_mpsc::sync_channel(1);
        std::thread::Builder::new()
            .name("remotekvm-audio-output".to_string())
            .spawn(move || run_cpal_audio_thread(frame_rx, init_tx))
            .context("spawn audio output thread")?;

        init_rx
            .recv()
            .context("audio output thread exited during initialization")??;
        Ok(Self {
            sender: Some(frame_tx),
        })
    }
}

impl Default for CpalAudioSink {
    fn default() -> Self {
        Self::new()
    }
}

impl AudioFrameSink for CpalAudioSink {
    fn on_audio_frame(&self, frame: DecodedAudioFrame) {
        if let Some(sender) = &self.sender {
            let _ = sender.send(frame);
        }
    }
}

fn run_cpal_audio_thread(
    frame_rx: std_mpsc::Receiver<DecodedAudioFrame>,
    init_tx: std_mpsc::SyncSender<Result<()>>,
) {
    if let Err(error) = cpal_audio_thread(frame_rx, init_tx.clone()) {
        let _ = init_tx.send(Err(error));
    }
}

fn cpal_audio_thread(
    frame_rx: std_mpsc::Receiver<DecodedAudioFrame>,
    init_tx: std_mpsc::SyncSender<Result<()>>,
) -> Result<()> {
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .context("no default audio output device")?;
    let supported_config = device
        .default_output_config()
        .context("default audio output device has no supported config")?;
    let sample_format = supported_config.sample_format();
    let stream_config: cpal::StreamConfig = supported_config.into();
    let channels = stream_config.channels.max(1);
    let sample_rate_hz = stream_config.sample_rate.0.max(1);
    let max_samples = sample_rate_hz as usize * channels as usize * AUDIO_QUEUE_SECONDS;
    let buffer = Arc::new(Mutex::new(AudioPlaybackBuffer {
        config: AudioPlaybackConfig {
            sample_rate_hz,
            channels,
            max_samples,
        },
        samples: VecDeque::with_capacity(max_samples.min(48_000)),
    }));

    let err_fn = |error| tracing::warn!(%error, "audio output stream error");
    let stream = match sample_format {
        cpal::SampleFormat::F32 => {
            let buffer = Arc::clone(&buffer);
            device.build_output_stream(
                &stream_config,
                move |output: &mut [f32], _| fill_output_f32(output, &buffer),
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::I16 => {
            let buffer = Arc::clone(&buffer);
            device.build_output_stream(
                &stream_config,
                move |output: &mut [i16], _| fill_output_i16(output, &buffer),
                err_fn,
                None,
            )?
        }
        cpal::SampleFormat::U16 => {
            let buffer = Arc::clone(&buffer);
            device.build_output_stream(
                &stream_config,
                move |output: &mut [u16], _| fill_output_u16(output, &buffer),
                err_fn,
                None,
            )?
        }
        other => anyhow::bail!("unsupported audio output sample format: {other:?}"),
    };
    stream.play().context("start audio output stream")?;
    tracing::info!(
        sample_rate_hz,
        channels,
        ?sample_format,
        "audio playback stream started"
    );
    let _ = init_tx.send(Ok(()));

    while let Ok(frame) = frame_rx.recv() {
        let Ok(input) = audio_frame_to_f32(&frame) else {
            tracing::warn!("dropping unsupported decoded audio frame");
            continue;
        };
        if input.is_empty() {
            continue;
        }
        if let Ok(mut guard) = buffer.lock() {
            let converted = resample_interleaved(
                &input,
                frame.sample_rate_hz,
                frame.channels,
                guard.config.sample_rate_hz,
                guard.config.channels,
            );
            guard.samples.extend(converted);
            let overflow = guard.samples.len().saturating_sub(guard.config.max_samples);
            if overflow > 0 {
                guard.samples.drain(..overflow);
            }
        }
    }

    drop(stream);
    Ok(())
}

fn audio_frame_to_f32(frame: &DecodedAudioFrame) -> Result<Vec<f32>> {
    match frame.format {
        AudioSampleFormat::F32Interleaved => {
            if frame.data.len() % std::mem::size_of::<f32>() != 0 {
                anyhow::bail!("f32 audio payload is not sample aligned");
            }
            Ok(frame
                .data
                .chunks_exact(4)
                .map(|bytes| f32::from_ne_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
                .collect())
        }
        AudioSampleFormat::I16Interleaved => {
            if frame.data.len() % std::mem::size_of::<i16>() != 0 {
                anyhow::bail!("i16 audio payload is not sample aligned");
            }
            Ok(frame
                .data
                .chunks_exact(2)
                .map(|bytes| i16::from_ne_bytes([bytes[0], bytes[1]]) as f32 / i16::MAX as f32)
                .collect())
        }
    }
}

fn resample_interleaved(
    input: &[f32],
    input_rate: u32,
    input_channels: u16,
    output_rate: u32,
    output_channels: u16,
) -> Vec<f32> {
    if input.is_empty() || input_rate == 0 || input_channels == 0 || output_rate == 0 {
        return Vec::new();
    }

    let input_channels = input_channels as usize;
    let output_channels = output_channels.max(1) as usize;
    let input_frames = input.len() / input_channels;
    if input_frames == 0 {
        return Vec::new();
    }
    let output_frames =
        ((input_frames as u64 * output_rate as u64) / input_rate as u64).max(1) as usize;
    let mut output = Vec::with_capacity(output_frames * output_channels);

    for frame in 0..output_frames {
        let source_frame = ((frame as u64 * input_rate as u64) / output_rate as u64)
            .min(input_frames as u64 - 1) as usize;
        let source = &input[source_frame * input_channels..(source_frame + 1) * input_channels];
        for channel in 0..output_channels {
            let sample = if input_channels == 1 {
                source[0]
            } else if output_channels == 1 {
                source.iter().copied().sum::<f32>() / input_channels as f32
            } else {
                source[channel.min(input_channels - 1)]
            };
            output.push(sample);
        }
    }

    output
}

fn fill_output_f32(output: &mut [f32], buffer: &Arc<Mutex<AudioPlaybackBuffer>>) {
    if let Ok(mut guard) = buffer.lock() {
        for sample in output {
            *sample = guard.samples.pop_front().unwrap_or(0.0);
        }
    } else {
        output.fill(0.0);
    }
}

fn fill_output_i16(output: &mut [i16], buffer: &Arc<Mutex<AudioPlaybackBuffer>>) {
    if let Ok(mut guard) = buffer.lock() {
        for sample in output {
            let value = guard.samples.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0);
            *sample = (value * i16::MAX as f32) as i16;
        }
    } else {
        output.fill(0);
    }
}

fn fill_output_u16(output: &mut [u16], buffer: &Arc<Mutex<AudioPlaybackBuffer>>) {
    if let Ok(mut guard) = buffer.lock() {
        for sample in output {
            let value = guard.samples.pop_front().unwrap_or(0.0).clamp(-1.0, 1.0);
            *sample = ((value * 0.5 + 0.5) * u16::MAX as f32) as u16;
        }
    } else {
        output.fill(u16::MAX / 2);
    }
}

#[derive(Clone)]
pub struct MediaPipeline {
    video_decoder: Arc<dyn VideoDecoder>,
    audio_decoder: Arc<dyn AudioDecoder>,
    video_sink: Arc<LatestVideoFrameSink>,
    audio_sink: Arc<dyn AudioFrameSink>,
}

impl MediaPipeline {
    pub fn new() -> Self {
        Self {
            video_decoder: platform::video_decoder(),
            audio_decoder: platform::audio_decoder(),
            video_sink: Arc::new(LatestVideoFrameSink::default()),
            audio_sink: platform::audio_sink(),
        }
    }

    pub fn noop() -> Self {
        Self::new()
    }

    pub fn latest_video_frame(&self) -> Option<DecodedVideoFrame> {
        self.video_sink.latest()
    }

    /// Add recv-only media transceivers and install an inbound track reader.
    ///
    /// This must run before `create_offer` so the offer advertises that the
    /// client is prepared to receive desktop video and audio.
    pub async fn attach_to_session(&self, session: &PeerSession) -> Result<()> {
        let pc = session.peer_connection();
        let recv_only = |direction| RTCRtpTransceiverInit {
            direction,
            send_encodings: Vec::new(),
        };

        pc.add_transceiver_from_kind(
            RTPCodecType::Video,
            Some(recv_only(RTCRtpTransceiverDirection::Recvonly)),
        )
        .await?;
        pc.add_transceiver_from_kind(
            RTPCodecType::Audio,
            Some(recv_only(RTCRtpTransceiverDirection::Recvonly)),
        )
        .await?;

        let pipeline = self.clone();
        pc.on_track(Box::new(move |track, _receiver, _transceiver| {
            let pipeline = pipeline.clone();
            Box::pin(async move {
                let kind = track.kind();
                tracing::info!(
                    kind = %kind,
                    id = %track.id(),
                    stream_id = %track.stream_id(),
                    "remote media track attached"
                );
                tokio::spawn(async move {
                    while let Ok((packet, _)) = track.read_rtp().await {
                        let encoded = EncodedMediaPacket {
                            payload: packet.payload,
                            payload_type: packet.header.payload_type,
                            sequence_number: packet.header.sequence_number,
                            timestamp: packet.header.timestamp,
                            marker: packet.header.marker,
                        };
                        if let Err(e) = pipeline.handle_packet(kind, encoded) {
                            tracing::warn!(error = %e, "media packet handling failed");
                        }
                    }
                });
            })
        }));

        Ok(())
    }

    fn handle_packet(&self, kind: RTPCodecType, packet: EncodedMediaPacket) -> Result<()> {
        match kind {
            RTPCodecType::Video => {
                if let Some(frame) = self.video_decoder.decode(packet)? {
                    self.video_sink.on_video_frame(frame);
                }
            }
            RTPCodecType::Audio => {
                if let Some(frame) = self.audio_decoder.decode(packet)? {
                    self.audio_sink.on_audio_frame(frame);
                }
            }
            RTPCodecType::Unspecified => {
                tracing::debug!("dropping media packet with unspecified RTP codec type");
            }
        }
        Ok(())
    }
}

mod platform {
    use super::*;

    pub fn video_decoder() -> Arc<dyn VideoDecoder> {
        #[cfg(target_os = "macos")]
        {
            tracing::debug!("using macOS VideoToolbox H.264 decoder");
            Arc::new(MacVideoToolboxH264Decoder::default())
        }
        #[cfg(target_os = "windows")]
        {
            tracing::debug!("using Windows Media Foundation H.264 decoder");
            Arc::new(WindowsMediaFoundationH264Decoder::default())
        }
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        {
            tracing::debug!("using no-op video decoder for this platform");
            Arc::new(NoopVideoDecoder)
        }
    }

    pub fn audio_decoder() -> Arc<dyn AudioDecoder> {
        match OpusAudioDecoder::new() {
            Ok(decoder) => {
                tracing::debug!("using Opus audio decoder");
                Arc::new(decoder)
            }
            Err(error) => {
                tracing::warn!(%error, "using no-op audio decoder");
                Arc::new(NoopAudioDecoder)
            }
        }
    }

    pub fn audio_sink() -> Arc<dyn AudioFrameSink> {
        Arc::new(CpalAudioSink::new())
    }

    #[cfg(target_os = "macos")]
    #[derive(Debug, Default)]
    struct MacVideoToolboxH264Decoder {
        state: Mutex<MacH264DecoderState>,
    }

    #[cfg(target_os = "macos")]
    impl VideoDecoder for MacVideoToolboxH264Decoder {
        fn decode(&self, packet: EncodedMediaPacket) -> Result<Option<DecodedVideoFrame>> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("macOS video decoder mutex poisoned"))?;
            state.decode(packet)
        }
    }

    #[cfg(target_os = "macos")]
    #[derive(Debug, Default)]
    struct MacH264DecoderState {
        rtp: H264DecodeState,
        videotoolbox: Option<VideoToolboxH264Decoder>,
    }

    #[cfg(target_os = "macos")]
    impl MacH264DecoderState {
        fn decode(&mut self, packet: EncodedMediaPacket) -> Result<Option<DecodedVideoFrame>> {
            let Some(access_unit) = self.rtp.push_rtp(packet)? else {
                return Ok(None);
            };

            self.rtp.observe_annex_b_sample(&access_unit.annex_b);
            self.ensure_videotoolbox_decoder()?;

            let Some(avcc) = annex_b_to_avcc_sample(&access_unit.annex_b)? else {
                return Ok(None);
            };
            let Some(decoder) = &self.videotoolbox else {
                tracing::debug!("waiting for H.264 SPS/PPS before creating VideoToolbox decoder");
                return Ok(None);
            };

            decoder.decode(&avcc, access_unit.presentation_time())
        }

        fn ensure_videotoolbox_decoder(&mut self) -> Result<()> {
            let (Some(sps), Some(pps)) = (&self.rtp.sps, &self.rtp.pps) else {
                return Ok(());
            };
            let needs_rebuild = self
                .videotoolbox
                .as_ref()
                .map(|decoder| !decoder.matches_parameter_sets(sps, pps))
                .unwrap_or(true);
            if needs_rebuild {
                self.videotoolbox = Some(VideoToolboxH264Decoder::new(sps.clone(), pps.clone())?);
                tracing::info!("created macOS VideoToolbox H.264 decoder session");
            }
            Ok(())
        }
    }

    #[cfg(target_os = "macos")]
    mod videotoolbox {
        use std::ffi::c_void;
        use std::ptr::NonNull;
        use std::time::Duration;

        use anyhow::{anyhow, Context, Result};
        use bytes::Bytes;
        use objc2::rc::Retained;
        use objc2_core_foundation::{
            kCFTypeDictionaryKeyCallBacks, kCFTypeDictionaryValueCallBacks, CFDictionary,
            CFMutableDictionary, CFNumber, CFNumberType, CFRetained,
        };
        use objc2_core_media::{
            kCMTimeInvalid, CMBlockBuffer, CMFormatDescription, CMSampleBuffer, CMSampleTimingInfo,
            CMTime, CMVideoFormatDescriptionCreateFromH264ParameterSets,
        };
        use objc2_core_video::{
            kCVPixelBufferPixelFormatTypeKey, kCVPixelFormatType_32BGRA, kCVPixelFormatType_32RGBA,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange, CVImageBuffer, CVPixelBuffer,
            CVPixelBufferGetBaseAddress, CVPixelBufferGetBaseAddressOfPlane,
            CVPixelBufferGetBytesPerRow, CVPixelBufferGetBytesPerRowOfPlane,
            CVPixelBufferGetHeight, CVPixelBufferGetHeightOfPlane, CVPixelBufferGetPixelFormatType,
            CVPixelBufferGetPlaneCount, CVPixelBufferGetWidth, CVPixelBufferGetWidthOfPlane,
            CVPixelBufferLockBaseAddress, CVPixelBufferLockFlags, CVPixelBufferUnlockBaseAddress,
        };
        use objc2_video_toolbox::{
            VTDecodeFrameFlags, VTDecodeInfoFlags, VTDecompressionOutputCallbackRecord,
            VTDecompressionSession,
        };

        use super::*;

        #[derive(Debug)]
        pub(super) struct VideoToolboxH264Decoder {
            session: Retained<VTDecompressionSession>,
            format_description: CFRetained<CMFormatDescription>,
            sps: Bytes,
            pps: Bytes,
        }

        unsafe impl Send for VideoToolboxH264Decoder {}
        unsafe impl Sync for VideoToolboxH264Decoder {}

        impl VideoToolboxH264Decoder {
            pub(super) fn new(sps: Bytes, pps: Bytes) -> Result<Self> {
                let format_description = create_h264_format_description(&sps, &pps)?;
                let destination_attrs = bgra_destination_attributes()?;
                let callback = VTDecompressionOutputCallbackRecord {
                    decompressionOutputCallback: Some(decompression_callback),
                    decompressionOutputRefCon: std::ptr::null_mut(),
                };
                let mut session_out: *mut VTDecompressionSession = std::ptr::null_mut();
                let status = unsafe {
                    VTDecompressionSession::create(
                        None,
                        &format_description,
                        None,
                        Some(destination_attrs.as_ref()),
                        &callback,
                        NonNull::new(&mut session_out).unwrap(),
                    )
                };
                if status != 0 || session_out.is_null() {
                    return Err(anyhow!("VTDecompressionSessionCreate failed: {status}"));
                }
                let session = unsafe { Retained::from_raw(session_out) }
                    .ok_or_else(|| anyhow!("VTDecompressionSessionCreate returned null"))?;

                Ok(Self {
                    session,
                    format_description,
                    sps,
                    pps,
                })
            }

            pub(super) fn matches_parameter_sets(&self, sps: &Bytes, pps: &Bytes) -> bool {
                &self.sps == sps && &self.pps == pps
            }

            pub(super) fn decode(
                &self,
                avcc: &Bytes,
                pts: Duration,
            ) -> Result<Option<DecodedVideoFrame>> {
                let sample_buffer =
                    create_sample_buffer(avcc, &self.format_description, duration_to_cm_time(pts))?;
                let mut callback_state = DecodeCallbackState { frame: None };
                let mut info_flags = VTDecodeInfoFlags::empty();
                let status = unsafe {
                    self.session.decode_frame(
                        &sample_buffer,
                        VTDecodeFrameFlags::empty(),
                        &mut callback_state as *mut DecodeCallbackState as *mut c_void,
                        &mut info_flags,
                    )
                };
                if status != 0 {
                    return Err(anyhow!(
                        "VTDecompressionSessionDecodeFrame failed: {status}"
                    ));
                }
                if info_flags.contains(VTDecodeInfoFlags::FrameDropped) {
                    return Ok(None);
                }
                callback_state.frame.transpose()
            }
        }

        impl Drop for VideoToolboxH264Decoder {
            fn drop(&mut self) {
                unsafe {
                    let _ = self.session.wait_for_asynchronous_frames();
                    self.session.invalidate();
                }
            }
        }

        struct DecodeCallbackState {
            frame: Option<Result<DecodedVideoFrame>>,
        }

        unsafe extern "C-unwind" fn decompression_callback(
            _decompression_output_ref_con: *mut c_void,
            source_frame_ref_con: *mut c_void,
            status: i32,
            _info_flags: VTDecodeInfoFlags,
            image_buffer: *mut CVImageBuffer,
            presentation_time_stamp: CMTime,
            _presentation_duration: CMTime,
        ) {
            if source_frame_ref_con.is_null() {
                return;
            }
            let state = unsafe { &mut *(source_frame_ref_con as *mut DecodeCallbackState) };
            if status != 0 {
                state.frame = Some(Err(anyhow!(
                    "VideoToolbox decode callback failed: {status}"
                )));
                return;
            }
            if image_buffer.is_null() {
                state.frame = Some(Err(anyhow!("VideoToolbox returned no image")));
                return;
            }
            let pixel_buffer = unsafe { &*(image_buffer as *const CVPixelBuffer) };
            state.frame = Some(copy_pixel_buffer(pixel_buffer, presentation_time_stamp));
        }

        fn create_h264_format_description(
            sps: &Bytes,
            pps: &Bytes,
        ) -> Result<CFRetained<CMFormatDescription>> {
            let mut parameter_sets = [
                NonNull::new(sps.as_ptr() as *mut u8).context("empty H.264 SPS")?,
                NonNull::new(pps.as_ptr() as *mut u8).context("empty H.264 PPS")?,
            ];
            let mut sizes = [sps.len(), pps.len()];
            let mut format_out: *const CMFormatDescription = std::ptr::null();
            let status = unsafe {
                CMVideoFormatDescriptionCreateFromH264ParameterSets(
                    None,
                    parameter_sets.len(),
                    NonNull::new(parameter_sets.as_mut_ptr()).unwrap(),
                    NonNull::new(sizes.as_mut_ptr()).unwrap(),
                    4,
                    NonNull::new(&mut format_out).unwrap(),
                )
            };
            if status != 0 || format_out.is_null() {
                return Err(anyhow!(
                    "CMVideoFormatDescriptionCreateFromH264ParameterSets failed: {status}"
                ));
            }
            Ok(unsafe {
                CFRetained::from_raw(NonNull::new(format_out as *mut CMFormatDescription).unwrap())
            })
        }

        fn bgra_destination_attributes() -> Result<CFRetained<CFDictionary>> {
            let attrs = unsafe {
                CFMutableDictionary::new(
                    None,
                    1,
                    &kCFTypeDictionaryKeyCallBacks,
                    &kCFTypeDictionaryValueCallBacks,
                )
            }
            .context("failed to allocate CVPixelBuffer attributes dictionary")?;
            let attrs_ref: &CFMutableDictionary = &attrs;
            let pixel_format = kCVPixelFormatType_32BGRA as i32;
            let pixel_format_number = unsafe {
                CFNumber::new(
                    None,
                    CFNumberType::SInt32Type,
                    &pixel_format as *const i32 as *const c_void,
                )
            }
            .context("failed to allocate pixel format number")?;
            unsafe {
                CFMutableDictionary::set_value(
                    Some(attrs_ref),
                    kCVPixelBufferPixelFormatTypeKey as *const _ as *const c_void,
                    pixel_format_number.as_ref() as *const CFNumber as *const c_void,
                );
            }
            CFDictionary::new_copy(None, Some(attrs_ref.as_opaque()))
                .context("failed to freeze CVPixelBuffer attributes dictionary")
        }

        fn create_sample_buffer(
            avcc: &Bytes,
            format_description: &CMFormatDescription,
            pts: CMTime,
        ) -> Result<CFRetained<CMSampleBuffer>> {
            let block_buffer = create_block_buffer(avcc)?;
            let timing = CMSampleTimingInfo {
                duration: invalid_cm_time(),
                presentationTimeStamp: pts,
                decodeTimeStamp: invalid_cm_time(),
            };
            let sample_size = avcc.len();
            let mut sample_out: *mut CMSampleBuffer = std::ptr::null_mut();
            let status = unsafe {
                CMSampleBuffer::create_ready(
                    None,
                    Some(block_buffer.as_ref()),
                    Some(format_description),
                    1,
                    1,
                    &timing,
                    1,
                    &sample_size,
                    NonNull::new(&mut sample_out).unwrap(),
                )
            };
            if status != 0 || sample_out.is_null() {
                return Err(anyhow!("CMSampleBufferCreateReady failed: {status}"));
            }
            Ok(unsafe { CFRetained::from_raw(NonNull::new(sample_out).unwrap()) })
        }

        fn create_block_buffer(avcc: &Bytes) -> Result<CFRetained<CMBlockBuffer>> {
            let mut block_out: *mut CMBlockBuffer = std::ptr::null_mut();
            let status = unsafe {
                CMBlockBuffer::create_empty(None, 1, 0, NonNull::new(&mut block_out).unwrap())
            };
            if status != 0 || block_out.is_null() {
                return Err(anyhow!("CMBlockBufferCreateEmpty failed: {status}"));
            }
            let block_buffer = unsafe { CFRetained::from_raw(NonNull::new(block_out).unwrap()) };
            let status = unsafe {
                block_buffer.append_memory_block(
                    std::ptr::null_mut(),
                    avcc.len(),
                    None,
                    std::ptr::null(),
                    0,
                    avcc.len(),
                    objc2_core_media::kCMBlockBufferAssureMemoryNowFlag,
                )
            };
            if status != 0 {
                return Err(anyhow!("CMBlockBufferAppendMemoryBlock failed: {status}"));
            }
            let status = unsafe {
                CMBlockBuffer::replace_data_bytes(
                    NonNull::new(avcc.as_ptr() as *mut c_void).unwrap(),
                    &block_buffer,
                    0,
                    avcc.len(),
                )
            };
            if status != 0 {
                return Err(anyhow!("CMBlockBufferReplaceDataBytes failed: {status}"));
            }
            Ok(block_buffer)
        }

        fn copy_pixel_buffer(
            pixel_buffer: &CVPixelBuffer,
            pts: CMTime,
        ) -> Result<DecodedVideoFrame> {
            let lock_flags = CVPixelBufferLockFlags::ReadOnly;
            let status = unsafe { CVPixelBufferLockBaseAddress(pixel_buffer, lock_flags) };
            if status != 0 {
                return Err(anyhow!("CVPixelBufferLockBaseAddress failed: {status}"));
            }
            let result = copy_locked_pixel_buffer(pixel_buffer, pts);
            let unlock_status = unsafe { CVPixelBufferUnlockBaseAddress(pixel_buffer, lock_flags) };
            if unlock_status != 0 {
                return Err(anyhow!(
                    "CVPixelBufferUnlockBaseAddress failed after copy: {unlock_status}"
                ));
            }
            result
        }

        fn copy_locked_pixel_buffer(
            pixel_buffer: &CVPixelBuffer,
            pts: CMTime,
        ) -> Result<DecodedVideoFrame> {
            let width = CVPixelBufferGetWidth(pixel_buffer) as u32;
            let height = CVPixelBufferGetHeight(pixel_buffer) as u32;
            let pixel_format = CVPixelBufferGetPixelFormatType(pixel_buffer);
            if pixel_format == kCVPixelFormatType_32BGRA {
                copy_packed_pixel_buffer(pixel_buffer, width, height, VideoPixelFormat::Bgra8, pts)
            } else if pixel_format == kCVPixelFormatType_32RGBA {
                copy_packed_pixel_buffer(pixel_buffer, width, height, VideoPixelFormat::Rgba8, pts)
            } else if pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange
                || pixel_format == kCVPixelFormatType_420YpCbCr8BiPlanarFullRange
            {
                copy_nv12_pixel_buffer(pixel_buffer, width, height, pts)
            } else {
                Err(anyhow!(
                    "unsupported VideoToolbox pixel format: {pixel_format:#x}"
                ))
            }
        }

        fn copy_packed_pixel_buffer(
            pixel_buffer: &CVPixelBuffer,
            width: u32,
            height: u32,
            format: VideoPixelFormat,
            pts: CMTime,
        ) -> Result<DecodedVideoFrame> {
            let base = CVPixelBufferGetBaseAddress(pixel_buffer) as *const u8;
            if base.is_null() {
                return Err(anyhow!("packed CVPixelBuffer has no base address"));
            }
            let bytes_per_row = CVPixelBufferGetBytesPerRow(pixel_buffer);
            let row_len = width as usize * 4;
            let mut out = Vec::with_capacity(row_len * height as usize);
            for row in 0..height as usize {
                let row_start = unsafe { base.add(row * bytes_per_row) };
                out.extend_from_slice(unsafe { std::slice::from_raw_parts(row_start, row_len) });
            }
            Ok(DecodedVideoFrame {
                width,
                height,
                format,
                pts: cm_time_to_duration(pts),
                data: Bytes::from(out),
            })
        }

        fn copy_nv12_pixel_buffer(
            pixel_buffer: &CVPixelBuffer,
            width: u32,
            height: u32,
            pts: CMTime,
        ) -> Result<DecodedVideoFrame> {
            if CVPixelBufferGetPlaneCount(pixel_buffer) < 2 {
                return Err(anyhow!("NV12 CVPixelBuffer has fewer than two planes"));
            }
            let mut out = Vec::with_capacity(width as usize * height as usize * 3 / 2);
            for plane in 0..2 {
                let base = CVPixelBufferGetBaseAddressOfPlane(pixel_buffer, plane) as *const u8;
                if base.is_null() {
                    return Err(anyhow!(
                        "NV12 CVPixelBuffer plane {plane} has no base address"
                    ));
                }
                let plane_width = CVPixelBufferGetWidthOfPlane(pixel_buffer, plane);
                let plane_height = CVPixelBufferGetHeightOfPlane(pixel_buffer, plane);
                let bytes_per_row = CVPixelBufferGetBytesPerRowOfPlane(pixel_buffer, plane);
                for row in 0..plane_height {
                    let row_start = unsafe { base.add(row * bytes_per_row) };
                    out.extend_from_slice(unsafe {
                        std::slice::from_raw_parts(row_start, plane_width)
                    });
                }
            }
            Ok(DecodedVideoFrame {
                width,
                height,
                format: VideoPixelFormat::Nv12,
                pts: cm_time_to_duration(pts),
                data: Bytes::from(out),
            })
        }

        fn duration_to_cm_time(duration: Duration) -> CMTime {
            unsafe { CMTime::new(duration.as_micros().min(i64::MAX as u128) as i64, 1_000_000) }
        }

        fn cm_time_to_duration(time: CMTime) -> Option<Duration> {
            if !time.flags.contains(objc2_core_media::CMTimeFlags::Valid) || time.timescale <= 0 {
                return None;
            }
            let seconds = unsafe { time.seconds() };
            if seconds.is_finite() && seconds >= 0.0 {
                Some(Duration::from_secs_f64(seconds))
            } else {
                None
            }
        }

        fn invalid_cm_time() -> CMTime {
            unsafe { kCMTimeInvalid }
        }
    }

    #[cfg(target_os = "macos")]
    use videotoolbox::VideoToolboxH264Decoder;

    #[cfg(target_os = "windows")]
    #[derive(Debug, Default)]
    struct WindowsMediaFoundationH264Decoder {
        state: Mutex<WindowsH264DecoderState>,
    }

    #[cfg(target_os = "windows")]
    impl VideoDecoder for WindowsMediaFoundationH264Decoder {
        fn decode(&self, packet: EncodedMediaPacket) -> Result<Option<DecodedVideoFrame>> {
            let mut state = self
                .state
                .lock()
                .map_err(|_| anyhow::anyhow!("Windows video decoder mutex poisoned"))?;
            state.decode(packet)
        }
    }

    #[cfg(target_os = "windows")]
    #[derive(Debug, Default)]
    struct WindowsH264DecoderState {
        rtp: H264AccessUnitAssembler,
        media_foundation: Option<MediaFoundationH264Decoder>,
        pending_frames: VecDeque<DecodedVideoFrame>,
        sps: Option<Bytes>,
        pps: Option<Bytes>,
    }

    #[cfg(target_os = "windows")]
    impl WindowsH264DecoderState {
        fn decode(&mut self, packet: EncodedMediaPacket) -> Result<Option<DecodedVideoFrame>> {
            if let Some(frame) = self.pending_frames.pop_front() {
                return Ok(Some(frame));
            }

            let Some(access_unit) = self.rtp.push_rtp(packet)? else {
                return Ok(None);
            };
            self.observe_annex_b_sample(&access_unit.annex_b);
            if self.media_foundation.is_none() {
                if self.sps.is_none() || self.pps.is_none() {
                    tracing::debug!(
                        "waiting for H.264 SPS/PPS before creating Media Foundation decoder"
                    );
                    return Ok(None);
                }
                self.media_foundation = Some(MediaFoundationH264Decoder::new()?);
                tracing::info!("created Windows Media Foundation H.264 decoder");
            }

            let decoder = self
                .media_foundation
                .as_mut()
                .expect("decoder was just initialized");
            self.pending_frames
                .extend(decoder.decode(&access_unit.annex_b, access_unit.presentation_time())?);
            Ok(self.pending_frames.pop_front())
        }

        fn observe_annex_b_sample(&mut self, sample: &Bytes) {
            for nalu in annex_b_nalus(sample) {
                if let Some((&header, _)) = nalu.split_first() {
                    match header & 0x1f {
                        7 => self.sps = Some(Bytes::copy_from_slice(nalu)),
                        8 => self.pps = Some(Bytes::copy_from_slice(nalu)),
                        _ => {}
                    }
                }
            }
        }
    }

    #[cfg(target_os = "windows")]
    mod media_foundation {
        use std::mem::ManuallyDrop;
        use std::ptr::NonNull;
        use std::time::Duration;

        use anyhow::{anyhow, Context, Result};
        use bytes::Bytes;
        use windows::core::{Interface, GUID};
        use windows::Win32::Media::MediaFoundation::{
            IMFActivate, IMFMediaBuffer, IMFMediaType, IMFSample, IMFTransform, MFCreateMediaType,
            MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup, MFTEnumEx,
            MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoFormat_RGB32,
            MFVideoInterlace_Progressive, MFSTARTUP_FULL, MFT_CATEGORY_VIDEO_DECODER,
            MFT_ENUM_FLAG, MFT_ENUM_FLAG_ALL, MFT_ENUM_FLAG_HARDWARE, MFT_MESSAGE_COMMAND_FLUSH,
            MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
            MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
            MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE, MFT_OUTPUT_DATA_BUFFER_INCOMPLETE,
            MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
            MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS, MFT_REGISTER_TYPE_INFO, MF_E_NOTACCEPTING,
            MF_E_TRANSFORM_NEED_MORE_INPUT, MF_E_TRANSFORM_STREAM_CHANGE,
            MF_E_TRANSFORM_TYPE_NOT_SET, MF_MT_DEFAULT_STRIDE, MF_MT_FRAME_SIZE,
            MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_VERSION,
        };
        use windows::Win32::System::Com::CoTaskMemFree;

        use super::*;

        const HNS_PER_SECOND: i64 = 10_000_000;

        #[derive(Debug)]
        pub(super) struct MediaFoundationH264Decoder {
            transform: IMFTransform,
            output_provides_samples: bool,
            output_buffer_size: u32,
            output_type: Option<DecoderOutputType>,
        }

        unsafe impl Send for MediaFoundationH264Decoder {}
        unsafe impl Sync for MediaFoundationH264Decoder {}

        impl MediaFoundationH264Decoder {
            pub(super) fn new() -> Result<Self> {
                unsafe {
                    MFStartup(MF_VERSION, MFSTARTUP_FULL).context("start Media Foundation")?;
                }
                let transform =
                    activate_h264_decoder().context("activate Media Foundation H.264 decoder")?;
                configure_h264_input(&transform).context("configure H.264 decoder input")?;
                let output_info = unsafe {
                    transform
                        .GetOutputStreamInfo(0)
                        .context("get Media Foundation decoder output stream info")?
                };
                let output_provides_samples = output_info.dwFlags
                    & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                        | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                    != 0;
                let mut decoder = Self {
                    transform,
                    output_provides_samples,
                    output_buffer_size: output_info.cbSize.max(1),
                    output_type: None,
                };
                decoder.try_select_output_type()?;
                unsafe {
                    decoder
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                        .context("begin Media Foundation H.264 decoder streaming")?;
                    decoder
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                        .context("start Media Foundation H.264 decoder stream")?;
                }
                Ok(decoder)
            }

            pub(super) fn decode(
                &mut self,
                annex_b: &Bytes,
                pts: Duration,
            ) -> Result<Vec<DecodedVideoFrame>> {
                let sample = sample_from_bytes(annex_b, duration_to_hns(pts), 0)?;
                match unsafe { self.transform.ProcessInput(0, &sample, 0) } {
                    Ok(()) => {}
                    Err(error) if error.code() == MF_E_NOTACCEPTING => {
                        let _ = self.drain_output()?;
                        unsafe {
                            self.transform
                                .ProcessInput(0, &sample, 0)
                                .context("feed H.264 access unit to Media Foundation decoder")?;
                        }
                    }
                    Err(error) => {
                        return Err(error)
                            .context("feed H.264 access unit to Media Foundation decoder");
                    }
                }

                self.drain_output()
            }

            fn drain_output(&mut self) -> Result<Vec<DecodedVideoFrame>> {
                let mut frames = Vec::new();

                loop {
                    let output_sample = if self.output_provides_samples {
                        None
                    } else {
                        Some(empty_sample(self.output_buffer_size)?)
                    };
                    let mut output = MFT_OUTPUT_DATA_BUFFER {
                        dwStreamID: 0,
                        pSample: ManuallyDrop::new(output_sample),
                        dwStatus: 0,
                        pEvents: ManuallyDrop::new(None),
                    };
                    let mut status = 0;
                    let result = unsafe {
                        self.transform.ProcessOutput(
                            0,
                            std::slice::from_mut(&mut output),
                            &mut status,
                        )
                    };

                    let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
                    let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
                    drop(events);

                    match result {
                        Ok(()) => {
                            if status & MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS.0 as u32 != 0
                                || output.dwStatus & MFT_OUTPUT_DATA_BUFFER_FORMAT_CHANGE.0 as u32
                                    != 0
                            {
                                self.select_output_type()?;
                            }
                            if let Some(sample) = sample {
                                frames.push(self.copy_output_sample(&sample)?);
                            }
                            if output.dwStatus & MFT_OUTPUT_DATA_BUFFER_INCOMPLETE.0 as u32 == 0 {
                                continue;
                            }
                        }
                        Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                        Err(error)
                            if error.code() == MF_E_TRANSFORM_TYPE_NOT_SET
                                || error.code() == MF_E_TRANSFORM_STREAM_CHANGE =>
                        {
                            self.select_output_type()?;
                        }
                        Err(error) => {
                            return Err(error).context("drain Media Foundation H.264 decoder");
                        }
                    }
                }

                Ok(frames)
            }

            fn try_select_output_type(&mut self) -> Result<()> {
                match self.select_output_type() {
                    Ok(()) => Ok(()),
                    Err(error) => {
                        tracing::debug!(%error, "Media Foundation decoder output type not available yet");
                        Ok(())
                    }
                }
            }

            fn select_output_type(&mut self) -> Result<()> {
                let (media_type, output_type) = select_decoder_output_type(&self.transform)
                    .context("select H.264 decoder output type")?;
                unsafe {
                    self.transform
                        .SetOutputType(0, &media_type, 0)
                        .context("set Media Foundation decoder output type")?;
                }
                self.output_buffer_size = self.output_buffer_size.max(output_type.buffer_len());
                self.output_type = Some(output_type);
                Ok(())
            }

            fn copy_output_sample(&self, sample: &IMFSample) -> Result<DecodedVideoFrame> {
                let output_type = self
                    .output_type
                    .as_ref()
                    .context("Media Foundation decoder produced output before output type")?;
                let data = read_sample_bytes(sample)?;
                let compact = copy_strided_video_image(
                    &data,
                    output_type.width,
                    output_type.height,
                    output_type.format,
                    output_type.stride,
                    output_type.top_down,
                )?;
                let pts = unsafe { sample.GetSampleTime().ok().and_then(hns_to_duration) };
                Ok(DecodedVideoFrame {
                    width: output_type.width,
                    height: output_type.height,
                    format: output_type.format,
                    pts,
                    data: Bytes::from(compact),
                })
            }
        }

        impl Drop for MediaFoundationH264Decoder {
            fn drop(&mut self) {
                unsafe {
                    let _ = self
                        .transform
                        .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
                    let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
                }
            }
        }

        #[derive(Debug, Clone)]
        struct DecoderOutputType {
            width: u32,
            height: u32,
            format: VideoPixelFormat,
            stride: usize,
            top_down: bool,
        }

        impl DecoderOutputType {
            fn buffer_len(&self) -> u32 {
                let len = match self.format {
                    VideoPixelFormat::Nv12 => self.stride * self.height as usize * 3 / 2,
                    VideoPixelFormat::Bgra8 => self.stride * self.height as usize,
                    _ => 0,
                };
                len.min(u32::MAX as usize) as u32
            }
        }

        fn activate_h264_decoder() -> Result<IMFTransform> {
            if let Ok(transform) = activate_h264_decoder_with_flags(MFT_ENUM_FLAG_HARDWARE) {
                tracing::debug!("selected hardware Media Foundation H.264 decoder MFT");
                return Ok(transform);
            }
            activate_h264_decoder_with_flags(MFT_ENUM_FLAG_ALL)
        }

        fn activate_h264_decoder_with_flags(flags: MFT_ENUM_FLAG) -> Result<IMFTransform> {
            let input = MFT_REGISTER_TYPE_INFO {
                guidMajorType: MFMediaType_Video,
                guidSubtype: MFVideoFormat_H264,
            };
            let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
            let mut count = 0;

            unsafe {
                MFTEnumEx(
                    MFT_CATEGORY_VIDEO_DECODER,
                    flags,
                    Some(&input),
                    None,
                    &mut activates,
                    &mut count,
                )
                .context("enumerate Media Foundation H.264 decoders")?;
            }

            let activates = MftActivates::new(activates, count);
            let activate = activates
                .first()
                .cloned()
                .context("no Media Foundation H.264 decoder MFT registered")?;

            unsafe {
                activate
                    .ActivateObject::<IMFTransform>()
                    .context("activate Media Foundation H.264 decoder")
            }
        }

        struct MftActivates {
            ptr: NonNull<Option<IMFActivate>>,
            count: usize,
        }

        impl MftActivates {
            fn new(ptr: *mut Option<IMFActivate>, count: u32) -> Self {
                Self {
                    ptr: NonNull::new(ptr).unwrap_or_else(NonNull::dangling),
                    count: count as usize,
                }
            }

            fn first(&self) -> Option<&IMFActivate> {
                if self.count == 0 {
                    return None;
                }
                unsafe { self.ptr.as_ptr().as_ref()?.as_ref() }
            }
        }

        impl Drop for MftActivates {
            fn drop(&mut self) {
                if self.count == 0 {
                    return;
                }
                unsafe {
                    let slice = std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.count);
                    for item in slice {
                        let _ = item.take();
                    }
                    CoTaskMemFree(Some(self.ptr.as_ptr().cast()));
                }
            }
        }

        fn configure_h264_input(transform: &IMFTransform) -> Result<()> {
            let input_type =
                unsafe { MFCreateMediaType().context("create H.264 input media type")? };
            unsafe {
                input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
                input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
                input_type
                    .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
                transform
                    .SetInputType(0, &input_type, 0)
                    .context("set Media Foundation H.264 input type")?;
            }
            Ok(())
        }

        fn select_decoder_output_type(
            transform: &IMFTransform,
        ) -> Result<(IMFMediaType, DecoderOutputType)> {
            let preferred = [MFVideoFormat_NV12, MFVideoFormat_RGB32];
            let mut first_supported_error = None;
            for subtype in preferred {
                let mut index = 0;
                loop {
                    let media_type = match unsafe { transform.GetOutputAvailableType(0, index) } {
                        Ok(media_type) => media_type,
                        Err(error) => {
                            if index == 0 && first_supported_error.is_none() {
                                first_supported_error = Some(error);
                            }
                            break;
                        }
                    };
                    index += 1;
                    if media_type_subtype(&media_type).ok().as_ref() != Some(&subtype) {
                        continue;
                    }
                    return Ok((media_type.clone(), decoder_output_type(&media_type)?));
                }
            }
            if let Some(error) = first_supported_error {
                Err(error).context("enumerate decoder output media types")
            } else {
                Err(anyhow!("decoder does not expose NV12 or RGB32 output"))
            }
        }

        fn decoder_output_type(media_type: &IMFMediaType) -> Result<DecoderOutputType> {
            let subtype = media_type_subtype(media_type)?;
            let format = if subtype == MFVideoFormat_NV12 {
                VideoPixelFormat::Nv12
            } else if subtype == MFVideoFormat_RGB32 {
                VideoPixelFormat::Bgra8
            } else {
                return Err(anyhow!("unsupported decoder output subtype: {subtype:?}"));
            };
            let (width, height) = unsafe {
                unpack_u32_pair(
                    media_type
                        .GetUINT64(&MF_MT_FRAME_SIZE)
                        .context("decoder output type missing frame size")?,
                )
            };
            if width == 0 || height == 0 {
                return Err(anyhow!("decoder output type has zero dimensions"));
            }
            let default_stride = unsafe { media_type.GetUINT32(&MF_MT_DEFAULT_STRIDE).ok() }
                .map(|value| value as i32);
            let compact_stride = match format {
                VideoPixelFormat::Nv12 => width as usize,
                VideoPixelFormat::Bgra8 => width as usize * 4,
                _ => unreachable!(),
            };
            let stride = default_stride
                .map(|value| value.unsigned_abs() as usize)
                .unwrap_or(compact_stride)
                .max(compact_stride);
            let top_down = default_stride.map(|value| value >= 0).unwrap_or(true);
            Ok(DecoderOutputType {
                width,
                height,
                format,
                stride,
                top_down,
            })
        }

        fn media_type_subtype(media_type: &IMFMediaType) -> Result<GUID> {
            unsafe {
                media_type
                    .GetGUID(&MF_MT_SUBTYPE)
                    .context("media type missing subtype")
            }
        }

        fn sample_from_bytes(data: &[u8], sample_time: i64, duration: i64) -> Result<IMFSample> {
            let sample = unsafe { MFCreateSample().context("create Media Foundation sample")? };
            let buffer = unsafe {
                MFCreateMemoryBuffer(data.len() as u32)
                    .context("create Media Foundation input buffer")?
            };
            write_buffer(&buffer, data)?;
            unsafe {
                sample.AddBuffer(&buffer).context("attach input buffer")?;
                sample.SetSampleTime(sample_time)?;
                if duration > 0 {
                    sample.SetSampleDuration(duration)?;
                }
            }
            Ok(sample)
        }

        fn empty_sample(size: u32) -> Result<IMFSample> {
            let sample =
                unsafe { MFCreateSample().context("create Media Foundation output sample")? };
            let buffer = unsafe {
                MFCreateMemoryBuffer(size).context("create Media Foundation output buffer")?
            };
            unsafe {
                sample.AddBuffer(&buffer).context("attach output buffer")?;
            }
            Ok(sample)
        }

        fn write_buffer(buffer: &IMFMediaBuffer, data: &[u8]) -> Result<()> {
            let mut ptr = std::ptr::null_mut();
            unsafe {
                buffer
                    .Lock(&mut ptr, None, None)
                    .context("lock Media Foundation input buffer")?;
                std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
                buffer
                    .Unlock()
                    .context("unlock Media Foundation input buffer")?;
                buffer
                    .SetCurrentLength(data.len() as u32)
                    .context("set Media Foundation input buffer length")?;
            }
            Ok(())
        }

        fn read_sample_bytes(sample: &IMFSample) -> Result<Vec<u8>> {
            let buffer = unsafe {
                sample
                    .ConvertToContiguousBuffer()
                    .context("get contiguous Media Foundation output buffer")?
            };
            let mut ptr = std::ptr::null_mut();
            let mut current_len = 0;
            unsafe {
                buffer
                    .Lock(&mut ptr, None, Some(&mut current_len))
                    .context("lock Media Foundation output buffer")?;
                let bytes = std::slice::from_raw_parts(ptr, current_len as usize).to_vec();
                buffer
                    .Unlock()
                    .context("unlock Media Foundation output buffer")?;
                Ok(bytes)
            }
        }

        fn duration_to_hns(duration: Duration) -> i64 {
            duration
                .as_nanos()
                .saturating_mul(HNS_PER_SECOND as u128)
                .checked_div(1_000_000_000)
                .unwrap_or(0)
                .min(i64::MAX as u128) as i64
        }

        fn hns_to_duration(hns: i64) -> Option<Duration> {
            if hns < 0 {
                return None;
            }
            Some(Duration::from_nanos(
                (hns as u64).saturating_mul(1_000_000_000) / HNS_PER_SECOND as u64,
            ))
        }

        fn unpack_u32_pair(value: u64) -> (u32, u32) {
            ((value >> 32) as u32, value as u32)
        }
    }

    #[cfg(target_os = "windows")]
    use media_foundation::MediaFoundationH264Decoder;
}

#[derive(Debug, Default)]
struct H264DecodeState {
    depacketizer: rtp::codecs::h264::H264Packet,
    sps: Option<Bytes>,
    pps: Option<Bytes>,
}

impl H264DecodeState {
    fn push_rtp(&mut self, packet: EncodedMediaPacket) -> Result<Option<H264AccessUnit>> {
        use rtp::packetizer::Depacketizer;

        let timestamp = packet.timestamp;
        let rtp = RtpPacket {
            header: RtpHeader {
                version: 2,
                marker: packet.marker,
                payload_type: packet.payload_type,
                sequence_number: packet.sequence_number,
                timestamp: packet.timestamp,
                ..Default::default()
            },
            payload: packet.payload,
        };
        let annex_b = self
            .depacketizer
            .depacketize(&rtp.payload)
            .context("failed to depacketize H.264 RTP payload")?;
        if annex_b.is_empty() {
            Ok(None)
        } else {
            Ok(Some(H264AccessUnit { annex_b, timestamp }))
        }
    }

    fn observe_annex_b_sample(&mut self, sample: &Bytes) {
        for nalu in annex_b_nalus(sample) {
            if let Some((&header, payload)) = nalu.split_first() {
                match header & 0x1f {
                    7 => self.sps = Some(Bytes::copy_from_slice(nalu)),
                    8 => self.pps = Some(Bytes::copy_from_slice(nalu)),
                    5 => tracing::debug!(bytes = payload.len(), "received H.264 IDR NAL"),
                    _ => {}
                }
            }
        }
    }

    fn has_parameter_sets(&self) -> bool {
        self.sps.is_some() && self.pps.is_some()
    }
}

#[derive(Debug, Default)]
struct H264AccessUnitAssembler {
    depacketizer: rtp::codecs::h264::H264Packet,
    pending_annex_b: Vec<u8>,
    pending_timestamp: Option<u32>,
}

impl H264AccessUnitAssembler {
    fn push_rtp(&mut self, packet: EncodedMediaPacket) -> Result<Option<H264AccessUnit>> {
        use rtp::packetizer::Depacketizer;

        let annex_b = self
            .depacketizer
            .depacketize(&packet.payload)
            .context("failed to depacketize H.264 RTP payload")?;
        if annex_b.is_empty() {
            return Ok(None);
        }

        if self.pending_annex_b.is_empty() {
            self.pending_timestamp = Some(packet.timestamp);
        } else if self.pending_timestamp != Some(packet.timestamp) {
            let timestamp = self.pending_timestamp.unwrap_or(packet.timestamp);
            let finished = std::mem::take(&mut self.pending_annex_b);
            self.pending_timestamp = Some(packet.timestamp);
            self.pending_annex_b.extend_from_slice(&annex_b);
            tracing::warn!(
                previous_timestamp = timestamp,
                next_timestamp = packet.timestamp,
                "flushing H.264 access unit after RTP timestamp changed before marker"
            );
            return Ok(Some(H264AccessUnit {
                annex_b: Bytes::from(finished),
                timestamp,
            }));
        }

        self.pending_annex_b.extend_from_slice(&annex_b);
        if packet.marker {
            let timestamp = self.pending_timestamp.take().unwrap_or(packet.timestamp);
            let annex_b = Bytes::from(std::mem::take(&mut self.pending_annex_b));
            Ok(Some(H264AccessUnit { annex_b, timestamp }))
        } else {
            Ok(None)
        }
    }
}

#[derive(Debug, Clone)]
struct H264AccessUnit {
    annex_b: Bytes,
    timestamp: u32,
}

impl H264AccessUnit {
    fn presentation_time(&self) -> Duration {
        Duration::from_secs_f64(self.timestamp as f64 / H264_RTP_CLOCK_HZ as f64)
    }
}

const H264_RTP_CLOCK_HZ: u32 = 90_000;

fn annex_b_nalus(sample: &[u8]) -> Vec<&[u8]> {
    let mut nalus = Vec::new();
    let mut cursor = 0;
    while let Some(start) = find_start_code(sample, cursor) {
        let nalu_start = start + start_code_len(&sample[start..]);
        let next = find_start_code(sample, nalu_start).unwrap_or(sample.len());
        if nalu_start < next {
            nalus.push(&sample[nalu_start..next]);
        }
        cursor = next;
    }
    nalus
}

fn annex_b_to_avcc_sample(sample: &[u8]) -> Result<Option<Bytes>> {
    let mut out = Vec::with_capacity(sample.len());
    for nalu in annex_b_nalus(sample) {
        if nalu.is_empty() {
            continue;
        }
        match nalu[0] & 0x1f {
            7 | 8 => continue,
            _ => {}
        }
        let nalu_len = u32::try_from(nalu.len()).context("H.264 NAL unit exceeds AVCC length")?;
        out.extend_from_slice(&nalu_len.to_be_bytes());
        out.extend_from_slice(nalu);
    }
    if out.is_empty() {
        Ok(None)
    } else {
        Ok(Some(Bytes::from(out)))
    }
}

fn find_start_code(bytes: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= bytes.len() {
        if bytes[i..].starts_with(&[0, 0, 1]) || bytes[i..].starts_with(&[0, 0, 0, 1]) {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn start_code_len(bytes: &[u8]) -> usize {
    if bytes.starts_with(&[0, 0, 0, 1]) {
        4
    } else {
        3
    }
}

fn copy_strided_video_image(
    data: &[u8],
    width: u32,
    height: u32,
    format: VideoPixelFormat,
    stride: usize,
    top_down: bool,
) -> Result<Vec<u8>> {
    match format {
        VideoPixelFormat::Bgra8 => copy_strided_bgra(data, width, height, stride, top_down),
        VideoPixelFormat::Nv12 => copy_strided_nv12(data, width, height, stride, top_down),
        other => anyhow::bail!("unsupported strided video format: {other:?}"),
    }
}

fn copy_strided_bgra(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    top_down: bool,
) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    let row_len = width
        .checked_mul(4)
        .context("BGRA row byte length overflow")?;
    let stride = stride.max(row_len);
    copy_strided_rows(data, height, row_len, stride, 0, top_down)
}

fn copy_strided_nv12(
    data: &[u8],
    width: u32,
    height: u32,
    stride: usize,
    top_down: bool,
) -> Result<Vec<u8>> {
    if width % 2 != 0 || height % 2 != 0 {
        anyhow::bail!("NV12 frame dimensions must be even");
    }
    let width = width as usize;
    let height = height as usize;
    let stride = stride.max(width);
    let y_len = width.checked_mul(height).context("NV12 Y plane overflow")?;
    let uv_height = height / 2;
    let uv_offset = stride
        .checked_mul(height)
        .context("NV12 UV plane offset overflow")?;
    let mut out = copy_strided_rows(data, height, width, stride, 0, top_down)?;
    out.reserve(y_len / 2);
    let uv = copy_strided_rows(data, uv_height, width, stride, uv_offset, top_down)?;
    out.extend_from_slice(&uv);
    Ok(out)
}

fn copy_strided_rows(
    data: &[u8],
    rows: usize,
    row_len: usize,
    stride: usize,
    offset: usize,
    top_down: bool,
) -> Result<Vec<u8>> {
    if rows == 0 || row_len == 0 {
        return Ok(Vec::new());
    }
    let required = offset
        .checked_add(
            stride
                .checked_mul(rows - 1)
                .and_then(|value| value.checked_add(row_len))
                .context("strided image byte length overflow")?,
        )
        .context("strided image byte length overflow")?;
    if data.len() < required {
        anyhow::bail!(
            "strided image buffer too short: got {} bytes, need {required}",
            data.len()
        );
    }

    let mut out = Vec::with_capacity(rows * row_len);
    for row in 0..rows {
        let source_row = if top_down { row } else { rows - 1 - row };
        let start = offset + source_row * stride;
        out.extend_from_slice(&data[start..start + row_len]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_op_decoders_accept_encoded_packets_without_panic() {
        let packet = EncodedMediaPacket {
            payload: Bytes::from_static(b"encoded"),
            payload_type: 96,
            sequence_number: 42,
            timestamp: 123,
            marker: true,
        };

        assert!(NoopVideoDecoder.decode(packet.clone()).unwrap().is_none());
        assert!(NoopAudioDecoder.decode(packet).unwrap().is_none());
    }

    #[test]
    fn latest_video_sink_keeps_newest_frame() {
        let sink = LatestVideoFrameSink::default();
        sink.on_video_frame(DecodedVideoFrame {
            width: 2,
            height: 2,
            format: VideoPixelFormat::Rgba8,
            pts: None,
            data: Bytes::from_static(&[1, 2, 3, 4]),
        });

        assert_eq!(sink.latest().unwrap().width, 2);
    }

    #[test]
    fn buffered_audio_sink_drains_frames() {
        let sink = BufferedAudioSink::default();
        sink.on_audio_frame(DecodedAudioFrame {
            sample_rate_hz: 48_000,
            channels: 2,
            format: AudioSampleFormat::F32Interleaved,
            pts: None,
            data: Bytes::from_static(&[0; 8]),
        });

        assert_eq!(sink.drain().len(), 1);
        assert!(sink.drain().is_empty());
    }

    #[test]
    fn annex_b_parser_finds_sps_and_pps() {
        let sample = Bytes::from_static(&[
            0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 1, 0x68, 4, 5, 0, 0, 0, 1, 0x65, 9,
        ]);
        let mut state = H264DecodeState::default();
        state.observe_annex_b_sample(&sample);

        assert!(state.has_parameter_sets());
        assert_eq!(state.sps.unwrap(), Bytes::from_static(&[0x67, 1, 2, 3]));
        assert_eq!(state.pps.unwrap(), Bytes::from_static(&[0x68, 4, 5]));
    }

    #[test]
    fn annex_b_to_avcc_strips_parameter_sets_and_prefixes_lengths() {
        let sample = Bytes::from_static(&[
            0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 1, 0x68, 4, 5, 0, 0, 0, 1, 0x65, 9,
        ]);

        let avcc = annex_b_to_avcc_sample(&sample).unwrap().unwrap();

        assert_eq!(avcc.as_ref(), &[0, 0, 0, 2, 0x65, 9]);
    }

    #[test]
    fn annex_b_to_avcc_ignores_parameter_set_only_samples() {
        let sample = Bytes::from_static(&[0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 1, 0x68, 4, 5]);

        assert!(annex_b_to_avcc_sample(&sample).unwrap().is_none());
    }

    #[test]
    fn h264_access_unit_assembler_waits_for_marker() {
        let mut assembler = H264AccessUnitAssembler::default();
        let first = EncodedMediaPacket {
            payload: Bytes::from_static(&[0x41, 1, 2]),
            payload_type: 96,
            sequence_number: 1,
            timestamp: 90_000,
            marker: false,
        };
        let second = EncodedMediaPacket {
            payload: Bytes::from_static(&[0x41, 3, 4]),
            payload_type: 96,
            sequence_number: 2,
            timestamp: 90_000,
            marker: true,
        };

        assert!(assembler.push_rtp(first).unwrap().is_none());
        let access_unit = assembler.push_rtp(second).unwrap().unwrap();

        assert_eq!(access_unit.timestamp, 90_000);
        assert_eq!(
            access_unit.annex_b.as_ref(),
            &[0, 0, 0, 1, 0x41, 1, 2, 0, 0, 0, 1, 0x41, 3, 4]
        );
    }

    #[test]
    fn h264_access_unit_assembler_flushes_on_timestamp_change() {
        let mut assembler = H264AccessUnitAssembler::default();
        let first = EncodedMediaPacket {
            payload: Bytes::from_static(&[0x41, 1, 2]),
            payload_type: 96,
            sequence_number: 1,
            timestamp: 90_000,
            marker: false,
        };
        let second = EncodedMediaPacket {
            payload: Bytes::from_static(&[0x41, 3, 4]),
            payload_type: 96,
            sequence_number: 2,
            timestamp: 180_000,
            marker: false,
        };

        assert!(assembler.push_rtp(first).unwrap().is_none());
        let flushed = assembler.push_rtp(second).unwrap().unwrap();

        assert_eq!(flushed.timestamp, 90_000);
        assert_eq!(flushed.annex_b.as_ref(), &[0, 0, 0, 1, 0x41, 1, 2]);
    }

    #[test]
    fn h264_rtp_timestamp_maps_to_video_pts() {
        let access_unit = H264AccessUnit {
            annex_b: Bytes::new(),
            timestamp: 180_000,
        };

        assert_eq!(access_unit.presentation_time(), Duration::from_secs(2));
    }

    #[test]
    fn copy_strided_bgra_removes_row_padding() {
        let data = [
            1, 2, 3, 4, 5, 6, 7, 8, 99, 99, 9, 10, 11, 12, 13, 14, 15, 16, 88, 88,
        ];

        let compact =
            copy_strided_video_image(&data, 2, 2, VideoPixelFormat::Bgra8, 10, true).unwrap();

        assert_eq!(
            compact,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
    }

    #[test]
    fn copy_strided_nv12_removes_plane_padding() {
        let data = [1, 2, 3, 99, 4, 5, 6, 88, 7, 8, 9, 77];

        let compact = copy_strided_video_image(&data, 3, 2, VideoPixelFormat::Nv12, 4, true);

        assert!(compact.is_err());

        let data = [1, 2, 99, 99, 3, 4, 88, 88, 5, 6, 77, 77];
        let compact =
            copy_strided_video_image(&data, 2, 2, VideoPixelFormat::Nv12, 4, true).unwrap();

        assert_eq!(compact, vec![1, 2, 3, 4, 5, 6]);
    }

    #[test]
    fn copy_strided_bgra_flips_bottom_up_rows() {
        let data = [9, 10, 11, 12, 99, 1, 2, 3, 4, 88];

        let compact =
            copy_strided_video_image(&data, 1, 2, VideoPixelFormat::Bgra8, 5, false).unwrap();

        assert_eq!(compact, vec![1, 2, 3, 4, 9, 10, 11, 12]);
    }
}
