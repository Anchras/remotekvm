use anyhow::{Context, Result};
use bytes::Bytes;
use opus::{Application, Bitrate, Channels, Encoder as NativeOpusEncoder};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant, SystemTime};
use tokio::sync::mpsc;
use webrtc::api::media_engine::MIME_TYPE_OPUS;
use webrtc::media::Sample;
use webrtc::peer_connection::RTCPeerConnection;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;
use webrtc::track::track_local::TrackLocal;
use windows::core::GUID;
use windows::Win32::Foundation::{RPC_E_CHANGED_MODE, S_FALSE, S_OK};
use windows::Win32::Media::Audio::{
    eConsole, eRender, IAudioCaptureClient, IAudioClient, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    WAVEFORMATEX, WAVEFORMATEXTENSIBLE, WAVE_FORMAT_PCM,
};
use windows::Win32::Media::KernelStreaming::{KSDATAFORMAT_SUBTYPE_PCM, WAVE_FORMAT_EXTENSIBLE};
use windows::Win32::Media::Multimedia::WAVE_FORMAT_IEEE_FLOAT;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoTaskMemFree, CoUninitialize, CLSCTX_ALL,
    COINIT_MULTITHREADED,
};

const REFERENCE_TIME_PER_SECOND: i64 = 10_000_000;
const DEFAULT_BUFFER_DURATION: i64 = REFERENCE_TIME_PER_SECOND / 2;
const OPUS_SAMPLE_RATE: u32 = 48_000;
const OPUS_CHANNELS: u16 = 2;
const OPUS_FRAME_SAMPLES: usize = 960;
const OPUS_FRAME_PCM_SAMPLES: usize = OPUS_FRAME_SAMPLES * OPUS_CHANNELS as usize;
const OPUS_MAX_PACKET_SIZE: usize = 1_275;
const OPUS_FRAME_DURATION: Duration = Duration::from_millis(20);
const KSDATAFORMAT_SUBTYPE_IEEE_FLOAT: GUID =
    GUID::from_u128(0x00000003_0000_0010_8000_00aa00389b71);

/// WASAPI loopback audio capture.
///
/// Captures the default render endpoint (what the user hears) as signed 16-bit
/// PCM. The capture loop runs on a blocking thread because WASAPI/COM objects
/// are apartment-bound and should not be moved between Tokio worker threads.
pub struct AudioCapture {
    format: WasapiFormat,
    stop: Arc<AtomicBool>,
}

#[derive(Debug, Clone)]
pub struct AudioPacket {
    pub pcm_data: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
    pub frames: u32,
    pub timestamp: Instant,
}

impl AudioCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("initializing WASAPI audio capture");
        let format = probe_default_loopback_format()?;
        tracing::info!(
            sample_rate = format.sample_rate,
            channels = format.channels,
            bits_per_sample = format.bits_per_sample,
            ?format.sample_format,
            "default WASAPI loopback format"
        );

        Ok(Self {
            format,
            stop: Arc::new(AtomicBool::new(false)),
        })
    }

    pub fn sample_rate(&self) -> u32 {
        self.format.sample_rate
    }

    pub fn channels(&self) -> u16 {
        self.format.channels
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Release);
    }

    pub async fn start(&self, sender: mpsc::Sender<AudioPacket>) -> Result<()> {
        let stop = Arc::clone(&self.stop);
        tokio::task::spawn_blocking(move || capture_loop(sender, stop))
            .await
            .context("WASAPI capture task panicked")?
    }
}

/// Encoded audio packet ready for a media transport.
#[derive(Debug, Clone)]
pub struct EncodedAudioPacket {
    pub data: Vec<u8>,
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u16,
    pub duration: Duration,
    pub timestamp: Instant,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioCodec {
    Opus,
}

/// Opus encoder facade for WASAPI loopback audio.
///
/// WASAPI may return arbitrary packet sizes, sample rates, and channel counts.
/// This encoder normalizes packets to 48 kHz stereo 20 ms PCM frames before
/// handing them to libopus, which is the format advertised on the WebRTC track.
pub struct OpusEncoder {
    sample_rate: u32,
    channels: u16,
    bitrate_kbps: u32,
    normalizer: AudioNormalizer,
    encoder: NativeOpusEncoder,
}

impl OpusEncoder {
    pub fn new(sample_rate: u32, channels: u16, bitrate_kbps: u32) -> Result<Self> {
        if sample_rate == 0 {
            anyhow::bail!("audio sample rate must be non-zero");
        }
        if channels == 0 {
            anyhow::bail!("audio channel count must be non-zero");
        }
        if bitrate_kbps == 0 {
            anyhow::bail!("audio bitrate must be non-zero");
        }

        if channels > 8 {
            anyhow::bail!("unsupported WASAPI channel count: {channels}");
        }

        tracing::info!(
            sample_rate,
            channels,
            bitrate_kbps,
            opus_sample_rate = OPUS_SAMPLE_RATE,
            opus_channels = OPUS_CHANNELS,
            "initializing Opus encoder"
        );

        let mut encoder =
            NativeOpusEncoder::new(OPUS_SAMPLE_RATE, Channels::Stereo, Application::Audio)
                .map_err(|error| anyhow::anyhow!("create Opus encoder: {error:?}"))?;
        encoder
            .set_bitrate(Bitrate::Bits((bitrate_kbps * 1_000) as i32))
            .map_err(|error| anyhow::anyhow!("set Opus bitrate: {error:?}"))?;
        encoder
            .set_vbr(true)
            .map_err(|error| anyhow::anyhow!("enable Opus VBR: {error:?}"))?;
        encoder
            .set_inband_fec(true)
            .map_err(|error| anyhow::anyhow!("enable Opus in-band FEC: {error:?}"))?;

        Ok(Self {
            sample_rate,
            channels,
            bitrate_kbps,
            normalizer: AudioNormalizer::new(sample_rate, channels),
            encoder,
        })
    }

    pub fn encode(&mut self, packet: &AudioPacket) -> Result<Vec<EncodedAudioPacket>> {
        if packet.sample_rate != self.sample_rate {
            anyhow::bail!(
                "audio sample-rate mismatch: expected {}, got {}",
                self.sample_rate,
                packet.sample_rate
            );
        }
        if packet.channels != self.channels {
            anyhow::bail!(
                "audio channel mismatch: expected {}, got {}",
                self.channels,
                packet.channels
            );
        }

        let frames = self.normalizer.push_packet(packet)?;
        let mut encoded = Vec::with_capacity(frames.len());
        for frame in frames {
            let data = self
                .encoder
                .encode_vec(&frame, OPUS_MAX_PACKET_SIZE)
                .map_err(|error| anyhow::anyhow!("encode Opus frame: {error:?}"))?;
            encoded.push(EncodedAudioPacket {
                data,
                codec: AudioCodec::Opus,
                sample_rate: OPUS_SAMPLE_RATE,
                channels: OPUS_CHANNELS,
                duration: OPUS_FRAME_DURATION,
                timestamp: packet.timestamp,
            });
        }
        Ok(encoded)
    }

    pub fn codec(&self) -> AudioCodec {
        AudioCodec::Opus
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }
}

/// WebRTC audio task bundle. Dropping it aborts background capture/encode tasks.
pub struct AudioPipeline {
    capture: Arc<AudioCapture>,
    tasks: Vec<tokio::task::JoinHandle<()>>,
}

impl AudioPipeline {
    pub async fn start(
        peer_connection: Arc<RTCPeerConnection>,
        bitrate_kbps: u32,
    ) -> Result<Option<Self>> {
        let capture = Arc::new(AudioCapture::new().context("initialize WASAPI loopback")?);
        let encoder = OpusEncoder::new(capture.sample_rate(), capture.channels(), bitrate_kbps)?;

        let track = Arc::new(TrackLocalStaticSample::new(
            RTCRtpCodecCapability {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: OPUS_SAMPLE_RATE,
                channels: OPUS_CHANNELS,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            "audio".to_owned(),
            "remotekvm".to_owned(),
        ));

        peer_connection
            .add_track(Arc::clone(&track) as Arc<dyn TrackLocal + Send + Sync>)
            .await
            .context("add WebRTC Opus audio track")?;

        let (pcm_tx, mut pcm_rx) = mpsc::channel::<AudioPacket>(16);
        let capture_task = {
            let capture = Arc::clone(&capture);
            tokio::spawn(async move {
                if let Err(error) = capture.start(pcm_tx).await {
                    tracing::warn!(%error, "WASAPI loopback capture stopped");
                }
            })
        };

        let encode_task = tokio::spawn(async move {
            let mut encoder = encoder;
            while let Some(packet) = pcm_rx.recv().await {
                match encoder.encode(&packet) {
                    Ok(encoded_packets) => {
                        for encoded in encoded_packets {
                            let sample = Sample {
                                data: Bytes::from(encoded.data),
                                timestamp: SystemTime::now(),
                                duration: encoded.duration,
                                packet_timestamp: 0,
                                prev_dropped_packets: 0,
                                prev_padding_packets: 0,
                            };
                            if let Err(error) = track.write_sample(&sample).await {
                                tracing::warn!(%error, "failed to write Opus audio sample");
                                return;
                            }
                        }
                    }
                    Err(error) => tracing::warn!(%error, "failed to encode audio packet"),
                }
            }
        });

        Ok(Some(Self {
            capture,
            tasks: vec![capture_task, encode_task],
        }))
    }

    pub async fn stop(self) {
        self.capture.stop();
        for task in self.tasks {
            task.abort();
        }
    }
}

struct AudioNormalizer {
    input_sample_rate: u32,
    input_channels: u16,
    resample_remainder: u64,
    buffered_stereo: Vec<i16>,
}

impl AudioNormalizer {
    fn new(input_sample_rate: u32, input_channels: u16) -> Self {
        Self {
            input_sample_rate,
            input_channels,
            resample_remainder: 0,
            buffered_stereo: Vec::with_capacity(OPUS_FRAME_PCM_SAMPLES * 2),
        }
    }

    fn push_packet(&mut self, packet: &AudioPacket) -> Result<Vec<Vec<i16>>> {
        if packet.channels == 0 || packet.sample_rate == 0 {
            anyhow::bail!("invalid audio packet format");
        }
        if packet.channels != self.input_channels || packet.sample_rate != self.input_sample_rate {
            anyhow::bail!("audio packet format changed mid-stream");
        }
        let expected_samples = packet.frames as usize * packet.channels as usize;
        if packet.pcm_data.len() != expected_samples {
            anyhow::bail!(
                "audio packet sample count mismatch: expected {}, got {}",
                expected_samples,
                packet.pcm_data.len()
            );
        }

        let stereo = interleaved_to_stereo(&packet.pcm_data, packet.channels);
        let normalized = resample_stereo_linear_with_remainder(
            &stereo,
            packet.sample_rate,
            OPUS_SAMPLE_RATE,
            &mut self.resample_remainder,
        );
        self.buffered_stereo.extend_from_slice(&normalized);

        let complete_frames = self.buffered_stereo.len() / OPUS_FRAME_PCM_SAMPLES;
        if complete_frames == 0 {
            return Ok(Vec::new());
        }

        let mut frames = Vec::with_capacity(complete_frames);
        for chunk in self
            .buffered_stereo
            .chunks_exact(OPUS_FRAME_PCM_SAMPLES)
            .take(complete_frames)
        {
            frames.push(chunk.to_vec());
        }

        let used_samples = complete_frames * OPUS_FRAME_PCM_SAMPLES;
        self.buffered_stereo.drain(..used_samples);
        Ok(frames)
    }
}

fn interleaved_to_stereo(samples: &[i16], channels: u16) -> Vec<i16> {
    match channels {
        0 => Vec::new(),
        1 => samples
            .iter()
            .flat_map(|sample| [*sample, *sample])
            .collect(),
        2 => samples.to_vec(),
        _ => samples
            .chunks_exact(channels as usize)
            .flat_map(|frame| [frame[0], frame[1]])
            .collect(),
    }
}

fn resample_stereo_linear(stereo: &[i16], input_rate: u32, output_rate: u32) -> Vec<i16> {
    let mut remainder = 0;
    resample_stereo_linear_with_remainder(stereo, input_rate, output_rate, &mut remainder)
}

fn resample_stereo_linear_with_remainder(
    stereo: &[i16],
    input_rate: u32,
    output_rate: u32,
    remainder: &mut u64,
) -> Vec<i16> {
    if stereo.is_empty() || input_rate == 0 || output_rate == 0 {
        return Vec::new();
    }
    if input_rate == output_rate {
        return stereo.to_vec();
    }

    let input_frames = stereo.len() / OPUS_CHANNELS as usize;
    if input_frames <= 1 {
        return stereo.to_vec();
    }

    let scaled_frames = input_frames as u64 * output_rate as u64 + *remainder;
    let output_frames = (scaled_frames / input_rate as u64) as usize;
    *remainder = scaled_frames % input_rate as u64;
    if output_frames == 0 {
        return Vec::new();
    }

    let mut output = Vec::with_capacity(output_frames * OPUS_CHANNELS as usize);
    for out_frame in 0..output_frames {
        let position = out_frame as f64 * input_rate as f64 / output_rate as f64;
        let left = position.floor() as usize;
        let right = (left + 1).min(input_frames - 1);
        let fraction = position - left as f64;

        for channel in 0..OPUS_CHANNELS as usize {
            let a = stereo[left * OPUS_CHANNELS as usize + channel] as f64;
            let b = stereo[right * OPUS_CHANNELS as usize + channel] as f64;
            output.push((a + (b - a) * fraction).round() as i16);
        }
    }
    output
}

fn probe_default_loopback_format() -> Result<WasapiFormat> {
    let _com = ComApartment::init()?;
    let client = activate_default_loopback_client()?;
    let mix_format = MixFormat::get(&client)?;
    unsafe { WasapiFormat::from_wave_format(mix_format.as_ptr()) }
}

fn capture_loop(sender: mpsc::Sender<AudioPacket>, stop: Arc<AtomicBool>) -> Result<()> {
    let _com = ComApartment::init()?;
    let client = activate_default_loopback_client()?;
    let mix_format = MixFormat::get(&client)?;
    let format = unsafe { WasapiFormat::from_wave_format(mix_format.as_ptr())? };

    unsafe {
        client.Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_LOOPBACK,
            DEFAULT_BUFFER_DURATION,
            0,
            mix_format.as_ptr(),
            None,
        )?;
    }

    let capture_client: IAudioCaptureClient =
        unsafe { client.GetService() }.context("get IAudioCaptureClient")?;
    let mut default_period = 0;
    unsafe {
        client.GetDevicePeriod(Some(&mut default_period), None)?;
        client.Start().context("start WASAPI loopback capture")?;
    }
    let _client_stop = AudioClientStopGuard(&client);
    let sleep_duration = Duration::from_millis((default_period / 20_000).max(5) as u64);

    while !stop.load(Ordering::Acquire) && !sender.is_closed() {
        let mut packet_size = unsafe { capture_client.GetNextPacketSize()? };
        if packet_size == 0 {
            std::thread::sleep(sleep_duration);
            continue;
        }

        while packet_size > 0 {
            let packet = read_capture_packet(&capture_client, &format)?;
            if packet.frames > 0 && sender.blocking_send(packet).is_err() {
                return Ok(());
            }
            packet_size = unsafe { capture_client.GetNextPacketSize()? };
        }
    }

    Ok(())
}

fn activate_default_loopback_client() -> Result<IAudioClient> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL) }
            .context("create MMDeviceEnumerator")?;
    let endpoint = unsafe { enumerator.GetDefaultAudioEndpoint(eRender, eConsole) }
        .context("get default render endpoint")?;
    unsafe { endpoint.Activate(CLSCTX_ALL, None) }.context("activate IAudioClient")
}

fn read_capture_packet(
    capture_client: &IAudioCaptureClient,
    format: &WasapiFormat,
) -> Result<AudioPacket> {
    let mut data = std::ptr::null_mut();
    let mut frames = 0;
    let mut flags = 0;

    unsafe {
        capture_client.GetBuffer(&mut data, &mut frames, &mut flags, None, None)?;
    }

    let pcm_data = if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
        vec![0; frames as usize * format.channels as usize]
    } else {
        unsafe { format.copy_as_i16(data, frames)? }
    };

    unsafe {
        capture_client.ReleaseBuffer(frames)?;
    }

    Ok(AudioPacket {
        pcm_data,
        sample_rate: format.sample_rate,
        channels: format.channels,
        frames,
        timestamp: Instant::now(),
    })
}

#[derive(Debug, Clone, Copy)]
struct WasapiFormat {
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
    block_align: u16,
    sample_format: SampleFormat,
}

#[derive(Debug, Clone, Copy)]
enum SampleFormat {
    Float32,
    Pcm16,
    Pcm24,
    Pcm32,
}

impl WasapiFormat {
    unsafe fn from_wave_format(format: *const WAVEFORMATEX) -> Result<Self> {
        if format.is_null() {
            anyhow::bail!("WASAPI returned a null mix format");
        }

        let wave = *format;
        let tag = wave.wFormatTag as u32;
        let sample_format = match tag {
            WAVE_FORMAT_PCM => match wave.wBitsPerSample {
                16 => SampleFormat::Pcm16,
                24 => SampleFormat::Pcm24,
                32 => SampleFormat::Pcm32,
                bits => anyhow::bail!("unsupported PCM bit depth: {bits}"),
            },
            WAVE_FORMAT_IEEE_FLOAT => {
                if wave.wBitsPerSample != 32 {
                    anyhow::bail!("unsupported float bit depth: {}", wave.wBitsPerSample);
                }
                SampleFormat::Float32
            }
            WAVE_FORMAT_EXTENSIBLE => {
                let extensible = format as *const WAVEFORMATEXTENSIBLE;
                let sub_format = std::ptr::addr_of!((*extensible).SubFormat).read_unaligned();
                if sub_format == KSDATAFORMAT_SUBTYPE_PCM {
                    match wave.wBitsPerSample {
                        16 => SampleFormat::Pcm16,
                        24 => SampleFormat::Pcm24,
                        32 => SampleFormat::Pcm32,
                        bits => anyhow::bail!("unsupported extensible PCM bit depth: {bits}"),
                    }
                } else if sub_format == KSDATAFORMAT_SUBTYPE_IEEE_FLOAT {
                    if wave.wBitsPerSample != 32 {
                        anyhow::bail!(
                            "unsupported extensible float bit depth: {}",
                            wave.wBitsPerSample
                        );
                    }
                    SampleFormat::Float32
                } else {
                    anyhow::bail!("unsupported WASAPI extensible subformat: {sub_format:?}");
                }
            }
            other => anyhow::bail!("unsupported WASAPI format tag: {other}"),
        };

        if wave.nChannels == 0 || wave.nSamplesPerSec == 0 || wave.nBlockAlign == 0 {
            anyhow::bail!("invalid WASAPI mix format");
        }

        Ok(Self {
            sample_rate: wave.nSamplesPerSec,
            channels: wave.nChannels,
            bits_per_sample: wave.wBitsPerSample,
            block_align: wave.nBlockAlign,
            sample_format,
        })
    }

    unsafe fn copy_as_i16(&self, data: *const u8, frames: u32) -> Result<Vec<i16>> {
        let sample_count = frames as usize * self.channels as usize;
        let byte_count = frames as usize * self.block_align as usize;

        let pcm = match self.sample_format {
            SampleFormat::Float32 => {
                let samples = std::slice::from_raw_parts(data as *const f32, sample_count);
                samples
                    .iter()
                    .map(|sample| {
                        let scaled = sample.clamp(-1.0, 1.0) * i16::MAX as f32;
                        scaled.round() as i16
                    })
                    .collect()
            }
            SampleFormat::Pcm16 => {
                let samples = std::slice::from_raw_parts(data as *const i16, sample_count);
                samples.to_vec()
            }
            SampleFormat::Pcm24 => {
                let bytes = std::slice::from_raw_parts(data, byte_count);
                bytes
                    .chunks_exact(3)
                    .map(|chunk| {
                        let raw = i32::from_le_bytes([
                            chunk[0],
                            chunk[1],
                            chunk[2],
                            if chunk[2] & 0x80 != 0 { 0xFF } else { 0x00 },
                        ]);
                        (raw >> 8) as i16
                    })
                    .collect()
            }
            SampleFormat::Pcm32 => {
                let samples = std::slice::from_raw_parts(data as *const i32, sample_count);
                samples.iter().map(|sample| (sample >> 16) as i16).collect()
            }
        };

        Ok(pcm)
    }
}

struct MixFormat(*mut WAVEFORMATEX);

impl MixFormat {
    fn get(client: &IAudioClient) -> Result<Self> {
        let ptr = unsafe { client.GetMixFormat() }.context("get WASAPI mix format")?;
        Ok(Self(ptr))
    }

    fn as_ptr(&self) -> *const WAVEFORMATEX {
        self.0
    }
}

impl Drop for MixFormat {
    fn drop(&mut self) {
        unsafe {
            CoTaskMemFree(Some(self.0.cast()));
        }
    }
}

struct ComApartment {
    should_uninitialize: bool,
}

impl ComApartment {
    fn init() -> Result<Self> {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr == S_OK || hr == S_FALSE {
            Ok(Self {
                should_uninitialize: true,
            })
        } else if hr == RPC_E_CHANGED_MODE {
            Ok(Self {
                should_uninitialize: false,
            })
        } else {
            hr.ok().context("initialize COM for WASAPI")?;
            Ok(Self {
                should_uninitialize: true,
            })
        }
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.should_uninitialize {
            unsafe {
                CoUninitialize();
            }
        }
    }
}

struct AudioClientStopGuard<'a>(&'a IAudioClient);

impl Drop for AudioClientStopGuard<'_> {
    fn drop(&mut self) {
        if let Err(error) = unsafe { self.0.Stop() } {
            tracing::debug!(%error, "failed to stop WASAPI audio client");
        }
    }
}

fn duration_from_frames(frames: u32, sample_rate: u32) -> Duration {
    if frames == 0 || sample_rate == 0 {
        OPUS_FRAME_DURATION
    } else {
        Duration::from_secs_f64(frames as f64 / sample_rate as f64)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn computes_audio_duration_from_frames() {
        assert_eq!(duration_from_frames(960, 48_000), Duration::from_millis(20));
    }

    #[test]
    fn normalizer_buffers_until_twenty_ms_opus_frame() {
        let mut normalizer = AudioNormalizer::new(48_000, 2);
        let packet = AudioPacket {
            pcm_data: vec![42; 480 * 2],
            sample_rate: 48_000,
            channels: 2,
            frames: 480,
            timestamp: Instant::now(),
        };

        assert!(normalizer
            .push_packet(&packet)
            .expect("first half")
            .is_empty());
        let frames = normalizer.push_packet(&packet).expect("second half");
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].len(), OPUS_FRAME_PCM_SAMPLES);
        assert!(frames[0].iter().all(|sample| *sample == 42));
    }

    #[test]
    fn normalizer_duplicates_mono_and_resamples_to_opus_rate() {
        let mono = vec![10; 441];
        let stereo = interleaved_to_stereo(&mono, 1);
        assert_eq!(stereo.len(), 882);
        assert_eq!(&stereo[..4], &[10, 10, 10, 10]);

        let resampled = resample_stereo_linear(&stereo, 44_100, OPUS_SAMPLE_RATE);
        assert_eq!(resampled.len(), 960);
    }

    #[test]
    fn opus_encoder_emits_opus_packets_for_twenty_ms_frame() {
        let mut encoder = OpusEncoder::new(48_000, 2, 64).expect("create Opus encoder");
        let packet = AudioPacket {
            pcm_data: vec![0; OPUS_FRAME_PCM_SAMPLES],
            sample_rate: 48_000,
            channels: 2,
            frames: OPUS_FRAME_SAMPLES as u32,
            timestamp: Instant::now(),
        };

        let encoded = encoder.encode(&packet).expect("encode silence");
        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0].codec, AudioCodec::Opus);
        assert_eq!(encoded[0].sample_rate, OPUS_SAMPLE_RATE);
        assert_eq!(encoded[0].channels, OPUS_CHANNELS);
        assert_eq!(encoded[0].duration, OPUS_FRAME_DURATION);
        assert!(!encoded[0].data.is_empty());
    }
}
