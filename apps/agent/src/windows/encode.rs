use anyhow::{Context, Result};
use std::ffi::{c_void, CStr};
use std::mem::ManuallyDrop;
use std::ptr::NonNull;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;
use tokio::sync::mpsc;
use windows::core::{s, Interface, GUID, PCSTR};
use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::{
    D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL, D3D_FEATURE_LEVEL_11_0,
};
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Resource, ID3D11Texture2D,
    D3D11_BIND_VIDEO_ENCODER, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION,
    D3D11_TEXTURE2D_DESC, D3D11_USAGE_DEFAULT,
};
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_NV12, DXGI_SAMPLE_DESC};
use windows::Win32::Media::MediaFoundation::{
    IMFActivate, IMFMediaBuffer, IMFSample, IMFTransform, IWMVideoForceKeyFrame, MFCreateMediaType,
    MFCreateMemoryBuffer, MFCreateSample, MFMediaType_Video, MFStartup, MFTEnumEx,
    MFVideoFormat_H264, MFVideoFormat_NV12, MFVideoInterlace_Progressive, MFSTARTUP_FULL,
    MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG_ALL, MFT_MESSAGE_COMMAND_FLUSH,
    MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, MFT_MESSAGE_NOTIFY_END_OF_STREAM,
    MFT_MESSAGE_NOTIFY_START_OF_STREAM, MFT_OUTPUT_DATA_BUFFER,
    MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES, MFT_OUTPUT_STREAM_PROVIDES_SAMPLES,
    MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS, MFT_REGISTER_TYPE_INFO, MF_E_TRANSFORM_NEED_MORE_INPUT,
    MF_MT_AVG_BITRATE, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_SUBTYPE, MF_VERSION,
};
use windows::Win32::System::Com::CoTaskMemFree;
use windows::Win32::System::LibraryLoader::{GetProcAddress, LoadLibraryA};

const HNS_PER_SECOND: i64 = 10_000_000;
const NVENCAPI_MAJOR_VERSION: u32 = 13;
const NVENCAPI_MINOR_VERSION: u32 = 0;
const NVENCAPI_VERSION: u32 = NVENCAPI_MAJOR_VERSION | (NVENCAPI_MINOR_VERSION << 24);
const NVENCAPI_COMPACT_VERSION: u32 = (NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION;
const NVENCAPI_STRUCT_VERSION_TAG: u32 = 0x7 << 28;
const NVENCAPI_STRUCT_VERSION_FLAG: u32 = 1 << 31;
const NV_ENC_SUCCESS: NvencStatus = 0;
const NV_ENC_ERR_NEED_MORE_INPUT: NvencStatus = 16;
const NV_ENC_CODEC_H264_GUID: GUID = GUID::from_values(
    0x6bc82762,
    0x4e63,
    0x4ca4,
    [0xaa, 0x85, 0x1e, 0x50, 0xf3, 0x21, 0xf6, 0xbf],
);
const NV_ENC_H264_PROFILE_BASELINE_GUID: GUID = GUID::from_values(
    0x0727bcaa,
    0x78c4,
    0x4c83,
    [0x8c, 0x2f, 0xef, 0x3d, 0xff, 0x26, 0x7c, 0x6a],
);
const NV_ENC_PRESET_P1_GUID: GUID = GUID::from_values(
    0xfc0a8d3e,
    0x45f8,
    0x4cf8,
    [0x80, 0xc7, 0x29, 0x88, 0x71, 0x59, 0x0e, 0xbf],
);
const NV_ENC_BUFFER_FORMAT_NV12: u32 = 0x0000_0001;
const NV_ENC_DEVICE_TYPE_DIRECTX: u32 = 0;
const NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX: u32 = 0;
const NV_ENC_INPUT_IMAGE: u32 = 0;
const NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME: u32 = 0x01;
const NV_ENC_PARAMS_RC_CBR: u32 = 0x02;
const NV_ENC_MULTI_PASS_DISABLED: u32 = 0;
const NV_ENC_PIC_STRUCT_FRAME: u32 = 0x01;
const NV_ENC_PIC_TYPE_UNKNOWN: u32 = 0xff;
const NV_ENC_TUNING_INFO_LOW_LATENCY: u32 = 2;
const NV_ENC_LEVEL_AUTOSELECT: u32 = 0;
const NV_ENC_MV_PRECISION_QUARTER_PEL: u32 = 0x03;
const NV_ENC_PIC_FLAG_FORCEIDR: u32 = 0x02;
const NV_ENC_PIC_FLAG_OUTPUT_SPSPPS: u32 = 0x04;
const NVENC_INFINITE_GOPLENGTH: u32 = 0xffff_ffff;
const NV_ENC_CONFIG_H264_REPEAT_SPSPPS: u32 = 1 << 12;
const NV_ENC_CONFIG_H264_OUTPUT_AUD: u32 = 1 << 6;
const NV_ENC_RC_PARAMS_ZERO_REORDER_DELAY: u32 = 1 << 9;

macro_rules! nvenc_call {
    ($function:expr $(, $arg:expr)* $(,)?) => {{
        let function = $function.context("NVENC function pointer was not populated")?;
        function($($arg),*)
    }};
}

/// Video encoder for Windows.
///
/// Runtime selection never falls back to raw frames: successful construction
/// means encoded packets are H.264 Annex-B samples suitable for webrtc-rs'
/// H264 payloader.
pub struct VideoEncoder {
    config: EncoderConfig,
    bitrate_kbps: AtomicU32,
    force_keyframe: AtomicBool,
    sender: mpsc::Sender<EncodedFrame>,
    backend: EncoderBackend,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackendKind {
    Nvenc,
    MediaFoundation,
}

pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub is_keyframe: bool,
    pub timestamp: u64,
    pub backend: EncoderBackendKind,
}

impl VideoEncoder {
    pub fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate_kbps: u32,
        sender: mpsc::Sender<EncodedFrame>,
    ) -> Result<Self> {
        Self::with_config(
            EncoderConfig {
                width,
                height,
                fps,
                bitrate_kbps,
            },
            sender,
        )
    }

    pub fn with_config(config: EncoderConfig, sender: mpsc::Sender<EncodedFrame>) -> Result<Self> {
        validate_config(&config)?;
        tracing::info!(
            width = config.width,
            height = config.height,
            fps = config.fps,
            bitrate_kbps = config.bitrate_kbps,
            "initializing Windows H.264 video encoder"
        );

        let backend = EncoderBackend::select(&config)?;
        tracing::info!(backend = ?backend.kind(), "selected Windows H.264 encoder backend");

        Ok(Self {
            bitrate_kbps: AtomicU32::new(config.bitrate_kbps),
            force_keyframe: AtomicBool::new(true),
            config,
            sender,
            backend,
        })
    }

    pub fn encode_frame(&self, frame: &crate::windows::capture::CapturedFrame) -> Result<()> {
        let requested_keyframe = self.force_keyframe.swap(false, Ordering::AcqRel);
        let packet = self.backend.encode(
            frame,
            EncodeParams {
                bitrate_kbps: self.bitrate_kbps.load(Ordering::Acquire),
                force_keyframe: requested_keyframe,
            },
        )?;

        self.sender
            .try_send(packet)
            .context("queue encoded Windows H.264 frame")?;
        Ok(())
    }

    pub fn set_bitrate_kbps(&self, bitrate_kbps: u32) -> Result<()> {
        if bitrate_kbps == 0 {
            anyhow::bail!("bitrate must be greater than zero");
        }
        self.bitrate_kbps.store(bitrate_kbps, Ordering::Release);
        self.backend.set_bitrate_kbps(bitrate_kbps)?;
        tracing::info!(bitrate_kbps, "updated Windows encoder bitrate");
        Ok(())
    }

    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::Release);
        self.backend.request_keyframe();
        tracing::debug!("queued Windows encoder keyframe request");
    }

    pub fn backend_kind(&self) -> EncoderBackendKind {
        self.backend.kind()
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps.load(Ordering::Acquire)
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }
}

fn validate_config(config: &EncoderConfig) -> Result<()> {
    if config.width == 0 || config.height == 0 {
        anyhow::bail!("encoder dimensions must be non-zero");
    }
    if config.fps == 0 {
        anyhow::bail!("encoder fps must be non-zero");
    }
    if config.bitrate_kbps == 0 {
        anyhow::bail!("encoder bitrate must be non-zero");
    }
    if config.width % 2 != 0 || config.height % 2 != 0 {
        anyhow::bail!("H.264 NV12 encoding requires even dimensions");
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
struct EncodeParams {
    bitrate_kbps: u32,
    force_keyframe: bool,
}

enum EncoderBackend {
    Nvenc(NvencEncoder),
    MediaFoundation(MediaFoundationEncoder),
}

impl EncoderBackend {
    fn select(config: &EncoderConfig) -> Result<Self> {
        match NvencEncoder::new(config) {
            Ok(encoder) => return Ok(Self::Nvenc(encoder)),
            Err(error) => {
                tracing::info!(%error, "NVENC backend unavailable; trying Media Foundation");
            }
        }

        match MediaFoundationEncoder::new(config) {
            Ok(encoder) => Ok(Self::MediaFoundation(encoder)),
            Err(error) => {
                anyhow::bail!(
                    "no Windows H.264 encoder backend available: NVENC unavailable and Media Foundation failed: {error}"
                );
            }
        }
    }

    fn kind(&self) -> EncoderBackendKind {
        match self {
            Self::Nvenc(_) => EncoderBackendKind::Nvenc,
            Self::MediaFoundation(_) => EncoderBackendKind::MediaFoundation,
        }
    }

    fn encode(
        &self,
        frame: &crate::windows::capture::CapturedFrame,
        params: EncodeParams,
    ) -> Result<EncodedFrame> {
        match self {
            Self::Nvenc(encoder) => encoder.encode(frame, params),
            Self::MediaFoundation(encoder) => encoder.encode(frame, params),
        }
    }

    fn set_bitrate_kbps(&self, bitrate_kbps: u32) -> Result<()> {
        match self {
            Self::Nvenc(encoder) => encoder.set_bitrate_kbps(bitrate_kbps),
            Self::MediaFoundation(encoder) => encoder.set_bitrate_kbps(bitrate_kbps),
        }
    }

    fn request_keyframe(&self) {
        match self {
            Self::Nvenc(encoder) => encoder.request_keyframe(),
            Self::MediaFoundation(encoder) => encoder.request_keyframe(),
        }
    }
}

struct NvencEncoder {
    state: Mutex<NvencState>,
}

struct NvencState {
    api: NvencApi,
    session: *mut c_void,
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    input_texture: ID3D11Texture2D,
    registered_input: *mut c_void,
    bitstream_buffer: *mut c_void,
    config: EncoderConfig,
    frame_index: u64,
    requested_bitrate_kbps: u32,
    pending_keyframe: bool,
}

// NVENC, D3D11, and driver resources are used behind NvencEncoder's mutex.
unsafe impl Send for NvencState {}

impl NvencEncoder {
    fn new(config: &EncoderConfig) -> Result<Self> {
        let state = NvencState::new(config)?;
        Ok(Self {
            state: Mutex::new(state),
        })
    }

    fn encode(
        &self,
        frame: &crate::windows::capture::CapturedFrame,
        params: EncodeParams,
    ) -> Result<EncodedFrame> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("NVENC encoder lock poisoned"))?;
        state.encode(frame, params)
    }

    fn set_bitrate_kbps(&self, bitrate_kbps: u32) -> Result<()> {
        if bitrate_kbps == 0 {
            anyhow::bail!("bitrate must be greater than zero");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("NVENC encoder lock poisoned"))?;
        state.reconfigure_bitrate(bitrate_kbps)
    }

    fn request_keyframe(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending_keyframe = true;
        }
    }
}

impl NvencState {
    fn new(config: &EncoderConfig) -> Result<Self> {
        let api = NvencApi::load().context("load NVENC API")?;
        let max_version = api.max_supported_version()?;
        tracing::info!(
            max_supported_version = max_version,
            required_version = NVENCAPI_COMPACT_VERSION,
            "detected NVIDIA NVENC API"
        );
        if max_version < NVENCAPI_COMPACT_VERSION {
            anyhow::bail!(
                "NVIDIA driver supports NVENC API {max_version:#x}, but this build requires {NVENCAPI_COMPACT_VERSION:#x}"
            );
        }

        let (device, context) = create_nvenc_d3d11_device()?;
        let mut session = std::ptr::null_mut();
        let mut open_params = zeroed::<NvEncOpenEncodeSessionExParams>();
        open_params.version = nvenc_struct_version(1);
        open_params.device_type = NV_ENC_DEVICE_TYPE_DIRECTX;
        open_params.device = device.as_raw();
        open_params.api_version = NVENCAPI_VERSION;
        api.check(
            session,
            unsafe {
                nvenc_call!(
                    api.functions.nv_enc_open_encode_session_ex,
                    &mut open_params,
                    &mut session
                )
            },
            "open NVENC D3D11 encode session",
        )?;
        if session.is_null() {
            anyhow::bail!("NvEncOpenEncodeSessionEx returned a null session");
        }

        let input_texture = create_nv12_texture(&device, config)?;
        let mut state = Self {
            api,
            session,
            _device: device,
            context,
            input_texture,
            registered_input: std::ptr::null_mut(),
            bitstream_buffer: std::ptr::null_mut(),
            config: config.clone(),
            frame_index: 0,
            requested_bitrate_kbps: config.bitrate_kbps,
            pending_keyframe: true,
        };

        state.ensure_nv12_supported()?;
        state.initialize_encoder(config)?;
        state.register_input_texture()?;
        state.create_bitstream_buffer()?;

        tracing::info!(
            width = config.width,
            height = config.height,
            fps = config.fps,
            bitrate_kbps = config.bitrate_kbps,
            "NVENC D3D11 H.264 encode session ready"
        );
        Ok(state)
    }

    fn ensure_nv12_supported(&self) -> Result<()> {
        let mut count = 0;
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_get_input_format_count,
                    self.session,
                    NV_ENC_CODEC_H264_GUID,
                    &mut count,
                )
            },
            "query NVENC input format count",
        )?;
        if count == 0 {
            anyhow::bail!("NVENC reported no H.264 input formats");
        }

        let mut formats = vec![0u32; count as usize];
        let mut written = 0;
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_get_input_formats,
                    self.session,
                    NV_ENC_CODEC_H264_GUID,
                    formats.as_mut_ptr(),
                    formats.len() as u32,
                    &mut written,
                )
            },
            "query NVENC input formats",
        )?;
        if !formats[..written as usize].contains(&NV_ENC_BUFFER_FORMAT_NV12) {
            anyhow::bail!("NVENC H.264 encoder does not advertise NV12 input support");
        }
        Ok(())
    }

    fn initialize_encoder(&mut self, config: &EncoderConfig) -> Result<()> {
        let mut encode_config = nvenc_h264_config(config);
        let mut init = nvenc_initialize_params(config, &mut encode_config);
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_initialize_encoder,
                    self.session,
                    &mut init
                )
            },
            "initialize NVENC H.264 encoder",
        )
    }

    fn register_input_texture(&mut self) -> Result<()> {
        let mut register = zeroed::<NvEncRegisterResource>();
        register.version = nvenc_struct_version(5);
        register.resource_type = NV_ENC_INPUT_RESOURCE_TYPE_DIRECTX;
        register.width = self.config.width;
        register.height = self.config.height;
        register.resource_to_register = self.input_texture.as_raw();
        register.buffer_format = NV_ENC_BUFFER_FORMAT_NV12;
        register.buffer_usage = NV_ENC_INPUT_IMAGE;

        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_register_resource,
                    self.session,
                    &mut register
                )
            },
            "register D3D11 NV12 texture with NVENC",
        )?;
        if register.registered_resource.is_null() {
            anyhow::bail!("NvEncRegisterResource returned a null registered resource");
        }
        self.registered_input = register.registered_resource;
        Ok(())
    }

    fn create_bitstream_buffer(&mut self) -> Result<()> {
        let mut create = zeroed::<NvEncCreateBitstreamBuffer>();
        create.version = nvenc_struct_version(1);
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_create_bitstream_buffer,
                    self.session,
                    &mut create,
                )
            },
            "create NVENC output bitstream buffer",
        )?;
        if create.bitstream_buffer.is_null() {
            anyhow::bail!("NvEncCreateBitstreamBuffer returned a null buffer");
        }
        self.bitstream_buffer = create.bitstream_buffer;
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &crate::windows::capture::CapturedFrame,
        params: EncodeParams,
    ) -> Result<EncodedFrame> {
        validate_frame(frame)?;
        if frame.width != self.config.width || frame.height != self.config.height {
            anyhow::bail!(
                "NVENC frame size mismatch: got {}x{}, encoder is {}x{}",
                frame.width,
                frame.height,
                self.config.width,
                self.config.height
            );
        }

        if params.bitrate_kbps != self.requested_bitrate_kbps {
            self.reconfigure_bitrate(params.bitrate_kbps)?;
        }

        let nv12 = bgra_to_nv12(frame)?;
        self.upload_nv12(&nv12)?;
        let mapped = self.map_input()?;
        let duration_hns = HNS_PER_SECOND / self.config.fps as i64;
        let timestamp = (self.frame_index as i64 * duration_hns / 10) as u64;
        let force_keyframe = params.force_keyframe || self.pending_keyframe;

        let encode_result =
            self.encode_mapped_input(mapped, timestamp, duration_hns as u64, force_keyframe);
        let output = match encode_result {
            Ok(output) => output,
            Err(error) => {
                let _ = self.unmap_input(mapped);
                return Err(error);
            }
        };
        self.unmap_input(mapped)?;
        self.frame_index += 1;
        self.pending_keyframe = false;

        if output.is_empty() {
            anyhow::bail!("NVENC accepted input but produced no H.264 output");
        }

        let output = ensure_annex_b(&output);
        Ok(EncodedFrame {
            is_keyframe: annex_b_contains_idr(&output),
            data: output,
            timestamp,
            backend: EncoderBackendKind::Nvenc,
        })
    }

    fn upload_nv12(&self, nv12: &[u8]) -> Result<()> {
        let expected_len =
            self.config.width as usize * self.config.height as usize * 3usize / 2usize;
        if nv12.len() != expected_len {
            anyhow::bail!(
                "NV12 upload size mismatch: got {} bytes, expected {expected_len}",
                nv12.len()
            );
        }
        let resource: ID3D11Resource = self
            .input_texture
            .cast()
            .context("cast NVENC input texture to D3D11 resource")?;
        unsafe {
            self.context.UpdateSubresource(
                &resource,
                0,
                None,
                nv12.as_ptr().cast(),
                self.config.width,
                nv12.len() as u32,
            );
        }
        Ok(())
    }

    fn map_input(&self) -> Result<*mut c_void> {
        let mut map = zeroed::<NvEncMapInputResource>();
        map.version = nvenc_struct_version(4);
        map.registered_resource = self.registered_input;
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_map_input_resource,
                    self.session,
                    &mut map
                )
            },
            "map NVENC input resource",
        )?;
        if map.mapped_resource.is_null() {
            anyhow::bail!("NvEncMapInputResource returned a null mapped resource");
        }
        Ok(map.mapped_resource)
    }

    fn unmap_input(&self, mapped: *mut c_void) -> Result<()> {
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_unmap_input_resource,
                    self.session,
                    mapped
                )
            },
            "unmap NVENC input resource",
        )
    }

    fn encode_mapped_input(
        &self,
        mapped: *mut c_void,
        timestamp: u64,
        duration: u64,
        force_keyframe: bool,
    ) -> Result<Vec<u8>> {
        let mut pic = zeroed::<NvEncPicParams>();
        pic.version = nvenc_struct_version_with_flag(7);
        pic.input_width = self.config.width;
        pic.input_height = self.config.height;
        pic.input_pitch = self.config.width;
        pic.frame_idx = self.frame_index as u32;
        pic.input_time_stamp = timestamp;
        pic.input_duration = duration;
        pic.input_buffer = mapped;
        pic.output_bitstream = self.bitstream_buffer;
        pic.buffer_fmt = NV_ENC_BUFFER_FORMAT_NV12;
        pic.picture_struct = NV_ENC_PIC_STRUCT_FRAME;
        pic.picture_type = NV_ENC_PIC_TYPE_UNKNOWN;
        if force_keyframe {
            pic.encode_pic_flags = NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS;
        }

        let status = unsafe {
            nvenc_call!(
                self.api.functions.nv_enc_encode_picture,
                self.session,
                &mut pic
            )
        };
        if status == NV_ENC_ERR_NEED_MORE_INPUT {
            return Ok(Vec::new());
        }
        self.api
            .check(self.session, status, "encode NVENC H.264 picture")?;

        self.lock_bitstream()
    }

    fn lock_bitstream(&self) -> Result<Vec<u8>> {
        let mut lock = zeroed::<NvEncLockBitstream>();
        lock.version = nvenc_struct_version_with_flag(2);
        lock.output_bitstream = self.bitstream_buffer;
        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_lock_bitstream,
                    self.session,
                    &mut lock
                )
            },
            "lock NVENC bitstream",
        )?;

        let result = (|| {
            if lock.bitstream_buffer_ptr.is_null() {
                anyhow::bail!("NvEncLockBitstream returned a null bitstream pointer");
            }
            let bytes = unsafe {
                std::slice::from_raw_parts(
                    lock.bitstream_buffer_ptr.cast::<u8>(),
                    lock.bitstream_size_in_bytes as usize,
                )
                .to_vec()
            };
            Ok(bytes)
        })();

        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_unlock_bitstream,
                    self.session,
                    self.bitstream_buffer
                )
            },
            "unlock NVENC bitstream",
        )?;
        result
    }

    fn reconfigure_bitrate(&mut self, bitrate_kbps: u32) -> Result<()> {
        if bitrate_kbps == self.config.bitrate_kbps {
            self.requested_bitrate_kbps = bitrate_kbps;
            return Ok(());
        }

        let mut updated = self.config.clone();
        updated.bitrate_kbps = bitrate_kbps;
        let mut encode_config = nvenc_h264_config(&updated);
        let init = nvenc_initialize_params(&updated, &mut encode_config);
        let mut reconfigure = zeroed::<NvEncReconfigureParams>();
        reconfigure.version = nvenc_struct_version_with_flag(2);
        reconfigure.re_init_encode_params = init;
        reconfigure.flags = 1 << 1;

        self.api.check(
            self.session,
            unsafe {
                nvenc_call!(
                    self.api.functions.nv_enc_reconfigure_encoder,
                    self.session,
                    &mut reconfigure
                )
            },
            "reconfigure NVENC bitrate",
        )?;
        self.config.bitrate_kbps = bitrate_kbps;
        self.requested_bitrate_kbps = bitrate_kbps;
        self.pending_keyframe = true;
        Ok(())
    }
}

impl Drop for NvencState {
    fn drop(&mut self) {
        if !self.bitstream_buffer.is_null() {
            if let Some(destroy) = self.api.functions.nv_enc_destroy_bitstream_buffer {
                let _ = unsafe { destroy(self.session, self.bitstream_buffer) };
            }
        }
        if !self.registered_input.is_null() {
            if let Some(unregister) = self.api.functions.nv_enc_unregister_resource {
                let _ = unsafe { unregister(self.session, self.registered_input) };
            }
        }
        if !self.session.is_null() {
            if let Some(destroy) = self.api.functions.nv_enc_destroy_encoder {
                let _ = unsafe { destroy(self.session) };
            }
        }
    }
}

struct NvencApi {
    library: HMODULE,
    get_max_supported_version: NvEncodeApiGetMaxSupportedVersion,
    functions: NvEncodeApiFunctionList,
}

type NvencStatus = i32;
type NvEncodeApiGetMaxSupportedVersion = unsafe extern "system" fn(*mut u32) -> NvencStatus;
type NvEncodeApiCreateInstance =
    unsafe extern "system" fn(*mut NvEncodeApiFunctionList) -> NvencStatus;
type NvEncOpenEncodeSessionEx =
    unsafe extern "system" fn(*mut NvEncOpenEncodeSessionExParams, *mut *mut c_void) -> NvencStatus;
type NvEncGetInputFormatCount =
    unsafe extern "system" fn(*mut c_void, GUID, *mut u32) -> NvencStatus;
type NvEncGetInputFormats =
    unsafe extern "system" fn(*mut c_void, GUID, *mut u32, u32, *mut u32) -> NvencStatus;
type NvEncInitializeEncoder =
    unsafe extern "system" fn(*mut c_void, *mut NvEncInitializeParams) -> NvencStatus;
type NvEncCreateBitstreamBufferFn =
    unsafe extern "system" fn(*mut c_void, *mut NvEncCreateBitstreamBuffer) -> NvencStatus;
type NvEncDestroyBitstreamBufferFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void) -> NvencStatus;
type NvEncEncodePicture =
    unsafe extern "system" fn(*mut c_void, *mut NvEncPicParams) -> NvencStatus;
type NvEncLockBitstreamFn =
    unsafe extern "system" fn(*mut c_void, *mut NvEncLockBitstream) -> NvencStatus;
type NvEncUnlockBitstreamFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> NvencStatus;
type NvEncMapInputResourceFn =
    unsafe extern "system" fn(*mut c_void, *mut NvEncMapInputResource) -> NvencStatus;
type NvEncUnmapInputResourceFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> NvencStatus;
type NvEncDestroyEncoder = unsafe extern "system" fn(*mut c_void) -> NvencStatus;
type NvEncRegisterResourceFn =
    unsafe extern "system" fn(*mut c_void, *mut NvEncRegisterResource) -> NvencStatus;
type NvEncUnregisterResourceFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> NvencStatus;
type NvEncReconfigureEncoder =
    unsafe extern "system" fn(*mut c_void, *mut NvEncReconfigureParams) -> NvencStatus;
type NvEncGetLastErrorString = unsafe extern "system" fn(*mut c_void) -> *const i8;

#[repr(C)]
struct NvEncodeApiFunctionList {
    version: u32,
    reserved: u32,
    nv_enc_open_encode_session: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_guid_count: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_profile_guid_count: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_profile_guids: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_guids: Option<unsafe extern "system" fn()>,
    nv_enc_get_input_format_count: Option<NvEncGetInputFormatCount>,
    nv_enc_get_input_formats: Option<NvEncGetInputFormats>,
    nv_enc_get_encode_caps: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_preset_count: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_preset_guids: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_preset_config: Option<unsafe extern "system" fn()>,
    nv_enc_initialize_encoder: Option<NvEncInitializeEncoder>,
    nv_enc_create_input_buffer: Option<unsafe extern "system" fn()>,
    nv_enc_destroy_input_buffer: Option<unsafe extern "system" fn()>,
    nv_enc_create_bitstream_buffer: Option<NvEncCreateBitstreamBufferFn>,
    nv_enc_destroy_bitstream_buffer: Option<NvEncDestroyBitstreamBufferFn>,
    nv_enc_encode_picture: Option<NvEncEncodePicture>,
    nv_enc_lock_bitstream: Option<NvEncLockBitstreamFn>,
    nv_enc_unlock_bitstream: Option<NvEncUnlockBitstreamFn>,
    nv_enc_lock_input_buffer: Option<unsafe extern "system" fn()>,
    nv_enc_unlock_input_buffer: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_stats: Option<unsafe extern "system" fn()>,
    nv_enc_get_sequence_params: Option<unsafe extern "system" fn()>,
    nv_enc_register_async_event: Option<unsafe extern "system" fn()>,
    nv_enc_unregister_async_event: Option<unsafe extern "system" fn()>,
    nv_enc_map_input_resource: Option<NvEncMapInputResourceFn>,
    nv_enc_unmap_input_resource: Option<NvEncUnmapInputResourceFn>,
    nv_enc_destroy_encoder: Option<NvEncDestroyEncoder>,
    nv_enc_invalidate_ref_frames: Option<unsafe extern "system" fn()>,
    nv_enc_open_encode_session_ex: Option<NvEncOpenEncodeSessionEx>,
    nv_enc_register_resource: Option<NvEncRegisterResourceFn>,
    nv_enc_unregister_resource: Option<NvEncUnregisterResourceFn>,
    nv_enc_reconfigure_encoder: Option<NvEncReconfigureEncoder>,
    reserved1: *mut c_void,
    nv_enc_create_mv_buffer: Option<unsafe extern "system" fn()>,
    nv_enc_destroy_mv_buffer: Option<unsafe extern "system" fn()>,
    nv_enc_run_motion_estimation_only: Option<unsafe extern "system" fn()>,
    nv_enc_get_last_error_string: Option<NvEncGetLastErrorString>,
    nv_enc_set_io_cuda_streams: Option<unsafe extern "system" fn()>,
    nv_enc_get_encode_preset_config_ex: Option<unsafe extern "system" fn()>,
    nv_enc_get_sequence_param_ex: Option<unsafe extern "system" fn()>,
    nv_enc_restore_encoder_state: Option<unsafe extern "system" fn()>,
    nv_enc_lookahead_picture: Option<unsafe extern "system" fn()>,
    reserved2: [*mut c_void; 275],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncOpenEncodeSessionExParams {
    version: u32,
    device_type: u32,
    device: *mut c_void,
    reserved: *mut c_void,
    api_version: u32,
    reserved1: [u32; 253],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncCreateBitstreamBuffer {
    version: u32,
    size: u32,
    memory_heap: u32,
    reserved: u32,
    bitstream_buffer: *mut c_void,
    bitstream_buffer_ptr: *mut c_void,
    reserved1: [u32; 58],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncQp {
    qp_inter_p: u32,
    qp_inter_b: u32,
    qp_intra: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncRcParams {
    version: u32,
    rate_control_mode: u32,
    const_qp: NvEncQp,
    average_bit_rate: u32,
    max_bit_rate: u32,
    vbv_buffer_size: u32,
    vbv_initial_delay: u32,
    flags: u32,
    min_qp: NvEncQp,
    max_qp: NvEncQp,
    initial_rc_qp: NvEncQp,
    temporal_layer_idx_mask: u32,
    temporal_layer_qp: [u8; 8],
    target_quality: u8,
    target_quality_lsb: u8,
    lookahead_depth: u16,
    low_delay_key_frame_scale: u8,
    y_dc_qp_index_offset: i8,
    u_dc_qp_index_offset: i8,
    v_dc_qp_index_offset: i8,
    qp_map_mode: u32,
    multi_pass: u32,
    alpha_layer_bitrate_ratio: u32,
    cb_qp_index_offset: i8,
    cr_qp_index_offset: i8,
    reserved2: u16,
    lookahead_level: u32,
    view_bitrate_ratios: [u8; 7],
    reserved3: u8,
    reserved1: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncConfigH264VuiParameters {
    overscan_info_present_flag: u32,
    overscan_info: u32,
    video_signal_type_present_flag: u32,
    video_format: u32,
    video_full_range_flag: u32,
    colour_description_present_flag: u32,
    colour_primaries: u32,
    transfer_characteristics: u32,
    colour_matrix: u32,
    chroma_sample_location_flag: u32,
    chroma_sample_location_top: u32,
    chroma_sample_location_bot: u32,
    bitstream_restriction_flag: u32,
    timing_info_present_flag: u32,
    num_unit_in_ticks: u32,
    time_scale: u32,
    reserved: [u32; 12],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncConfigH264 {
    flags: u32,
    level: u32,
    idr_period: u32,
    separate_colour_plane_flag: u32,
    disable_deblocking_filter_idc: u32,
    num_temporal_layers: u32,
    sps_id: u32,
    pps_id: u32,
    adaptive_transform_mode: u32,
    fmo_mode: u32,
    bdirect_mode: u32,
    entropy_coding_mode: u32,
    stereo_mode: u32,
    intra_refresh_period: u32,
    intra_refresh_cnt: u32,
    max_num_ref_frames: u32,
    slice_mode: u32,
    slice_mode_data: u32,
    h264_vui_parameters: NvEncConfigH264VuiParameters,
    ltr_num_frames: u32,
    ltr_trust_mode: u32,
    chroma_format_idc: u32,
    max_temporal_layers: u32,
    use_b_frames_as_ref: u32,
    num_ref_l0: u32,
    num_ref_l1: u32,
    output_bit_depth: u32,
    input_bit_depth: u32,
    tf_level: u32,
    reserved1: [u32; 264],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
union NvEncCodecConfig {
    h264_config: NvEncConfigH264,
}

#[repr(C)]
struct NvEncConfig {
    version: u32,
    profile_guid: GUID,
    gop_length: u32,
    frame_interval_p: i32,
    monochrome_encoding: u32,
    frame_field_mode: u32,
    mv_precision: u32,
    rc_params: NvEncRcParams,
    encode_codec_config: NvEncCodecConfig,
    reserved: [u32; 278],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvencExternalMeHintCountsPerBlockType {
    flags: u32,
    reserved1: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncInitializeParams {
    version: u32,
    encode_guid: GUID,
    preset_guid: GUID,
    encode_width: u32,
    encode_height: u32,
    dar_width: u32,
    dar_height: u32,
    frame_rate_num: u32,
    frame_rate_den: u32,
    enable_encode_async: u32,
    enable_ptd: u32,
    flags: u32,
    priv_data_size: u32,
    reserved: u32,
    priv_data: *mut c_void,
    encode_config: *mut NvEncConfig,
    max_encode_width: u32,
    max_encode_height: u32,
    max_me_hint_counts_per_block: [NvencExternalMeHintCountsPerBlockType; 2],
    tuning_info: u32,
    buffer_format: u32,
    num_state_buffers: u32,
    output_stats_level: u32,
    reserved1: [u32; 284],
    reserved2: [*mut c_void; 64],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncReconfigureParams {
    version: u32,
    reserved: u32,
    re_init_encode_params: NvEncInitializeParams,
    flags: u32,
    reserved2: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncPicParamsH264Ext {
    reserved1: [u32; 32],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncSeiPayload {
    payload_size: u32,
    payload_type: u32,
    payload: *mut u8,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncClockTimestampSet {
    flags: u32,
    time_offset: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncTimeCode {
    display_pic_struct: u32,
    clock_timestamp: [NvEncClockTimestampSet; 3],
    skip_clock_timestamp_insertion: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncPicParamsH264 {
    display_poc_syntax: u32,
    reserved3: u32,
    ref_pic_flag: u32,
    colour_plane_id: u32,
    force_intra_refresh_with_frame_cnt: u32,
    flags: u32,
    slice_type_data: *mut u8,
    slice_type_array_cnt: u32,
    sei_payload_array_cnt: u32,
    sei_payload_array: *mut NvEncSeiPayload,
    slice_mode: u32,
    slice_mode_data: u32,
    ltr_mark_frame_idx: u32,
    ltr_use_frame_bitmap: u32,
    ltr_usage_mode: u32,
    force_intra_slice_count: u32,
    force_intra_slice_idx: *mut u32,
    h264_ext_pic_params: NvEncPicParamsH264Ext,
    time_code: NvEncTimeCode,
    reserved: [u32; 202],
    reserved2: [*mut c_void; 61],
}

#[repr(C)]
union NvEncCodecPicParams {
    h264_pic_params: NvEncPicParamsH264,
    reserved: [u64; 193],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvencExternalMeHint {
    flags: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvencExternalMeSbHint {
    flags: [u16; 3],
}

#[repr(C)]
struct NvEncPicParams {
    version: u32,
    input_width: u32,
    input_height: u32,
    input_pitch: u32,
    encode_pic_flags: u32,
    frame_idx: u32,
    input_time_stamp: u64,
    input_duration: u64,
    input_buffer: *mut c_void,
    output_bitstream: *mut c_void,
    completion_event: *mut c_void,
    buffer_fmt: u32,
    picture_struct: u32,
    picture_type: u32,
    codec_pic_params: NvEncCodecPicParams,
    me_hint_counts_per_block: [NvencExternalMeHintCountsPerBlockType; 2],
    me_external_hints: *mut NvencExternalMeHint,
    reserved2: [u32; 7],
    reserved5: [*mut c_void; 2],
    qp_delta_map: *mut i8,
    qp_delta_map_size: u32,
    reserved_bit_fields: u32,
    me_hint_ref_pic_dist: [u16; 2],
    reserved4: u32,
    alpha_buffer: *mut c_void,
    me_external_sb_hints: *mut NvencExternalMeSbHint,
    me_sb_hints_count: u32,
    state_buffer_idx: u32,
    output_recon_buffer: *mut c_void,
    reserved3: [u32; 284],
    reserved6: [*mut c_void; 57],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncLockBitstream {
    version: u32,
    flags: u32,
    output_bitstream: *mut c_void,
    slice_offsets: *mut u32,
    frame_idx: u32,
    hw_encode_status: u32,
    num_slices: u32,
    bitstream_size_in_bytes: u32,
    output_time_stamp: u64,
    output_duration: u64,
    bitstream_buffer_ptr: *mut c_void,
    picture_type: u32,
    picture_struct: u32,
    frame_avg_qp: u32,
    frame_satd: u32,
    ltr_frame_idx: u32,
    ltr_frame_bitmap: u32,
    temporal_id: u32,
    intra_mb_count: u32,
    inter_mb_count: u32,
    average_mvx: i32,
    average_mvy: i32,
    alpha_layer_size_in_bytes: u32,
    output_stats_ptr_size: u32,
    reserved: u32,
    output_stats_ptr: *mut c_void,
    frame_idx_display: u32,
    reserved1: [u32; 219],
    reserved2: [*mut c_void; 63],
    reserved_internal: [u32; 8],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncMapInputResource {
    version: u32,
    sub_resource_index: u32,
    input_resource: *mut c_void,
    registered_resource: *mut c_void,
    mapped_resource: *mut c_void,
    mapped_buffer_fmt: u32,
    reserved1: [u32; 251],
    reserved2: [*mut c_void; 63],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct NvEncRegisterResource {
    version: u32,
    resource_type: u32,
    width: u32,
    height: u32,
    pitch: u32,
    sub_resource_index: u32,
    resource_to_register: *mut c_void,
    registered_resource: *mut c_void,
    buffer_format: u32,
    buffer_usage: u32,
    input_fence_point: *mut c_void,
    chroma_offset: [u32; 2],
    chroma_offset_in: [u32; 2],
    reserved1: [u32; 244],
    reserved2: [*mut c_void; 61],
}

fn create_nvenc_d3d11_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let mut device = None;
    let mut context = None;
    let feature_levels = [D3D_FEATURE_LEVEL_11_0];
    let mut selected_level = D3D_FEATURE_LEVEL::default();

    unsafe {
        D3D11CreateDevice(
            None,
            D3D_DRIVER_TYPE_HARDWARE,
            None,
            D3D11_CREATE_DEVICE_BGRA_SUPPORT,
            Some(&feature_levels),
            D3D11_SDK_VERSION,
            Some(&mut device),
            Some(&mut selected_level),
            Some(&mut context),
        )
        .context("create D3D11 device for NVENC")?;
    }

    let device = device.context("D3D11CreateDevice returned no NVENC device")?;
    let context = context.context("D3D11CreateDevice returned no NVENC immediate context")?;
    tracing::debug!(?selected_level, "created D3D11 device for NVENC");
    Ok((device, context))
}

fn create_nv12_texture(device: &ID3D11Device, config: &EncoderConfig) -> Result<ID3D11Texture2D> {
    let desc = D3D11_TEXTURE2D_DESC {
        Width: config.width,
        Height: config.height,
        MipLevels: 1,
        ArraySize: 1,
        Format: DXGI_FORMAT_NV12,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Usage: D3D11_USAGE_DEFAULT,
        BindFlags: D3D11_BIND_VIDEO_ENCODER.0 as u32,
        CPUAccessFlags: 0,
        MiscFlags: 0,
    };

    let mut texture = None;
    unsafe {
        device
            .CreateTexture2D(&desc, None, Some(&mut texture))
            .context("create D3D11 NV12 texture for NVENC input")?;
    }
    texture.context("CreateTexture2D returned no NVENC input texture")
}

fn nvenc_h264_config(config: &EncoderConfig) -> NvEncConfig {
    let mut encode_config = zeroed::<NvEncConfig>();
    encode_config.version = nvenc_struct_version_with_flag(9);
    encode_config.profile_guid = NV_ENC_H264_PROFILE_BASELINE_GUID;
    encode_config.gop_length = NVENC_INFINITE_GOPLENGTH;
    encode_config.frame_interval_p = 1;
    encode_config.frame_field_mode = NV_ENC_PARAMS_FRAME_FIELD_MODE_FRAME;
    encode_config.mv_precision = NV_ENC_MV_PRECISION_QUARTER_PEL;
    encode_config.rc_params.version = nvenc_struct_version(1);
    encode_config.rc_params.rate_control_mode = NV_ENC_PARAMS_RC_CBR;
    encode_config.rc_params.average_bit_rate = config.bitrate_kbps * 1_000;
    encode_config.rc_params.max_bit_rate = config.bitrate_kbps * 1_000;
    encode_config.rc_params.vbv_buffer_size =
        (config.bitrate_kbps * 1_000 / config.fps.max(1)).max(1);
    encode_config.rc_params.vbv_initial_delay = encode_config.rc_params.vbv_buffer_size;
    encode_config.rc_params.flags = NV_ENC_RC_PARAMS_ZERO_REORDER_DELAY;
    encode_config.rc_params.multi_pass = NV_ENC_MULTI_PASS_DISABLED;

    encode_config.encode_codec_config.h264_config.flags =
        NV_ENC_CONFIG_H264_REPEAT_SPSPPS | NV_ENC_CONFIG_H264_OUTPUT_AUD;
    encode_config.encode_codec_config.h264_config.level = NV_ENC_LEVEL_AUTOSELECT;
    encode_config.encode_codec_config.h264_config.idr_period = NVENC_INFINITE_GOPLENGTH;
    encode_config
        .encode_codec_config
        .h264_config
        .chroma_format_idc = 1;
    encode_config.encode_codec_config.h264_config.slice_mode = 0;
    encode_config
        .encode_codec_config
        .h264_config
        .slice_mode_data = 0;
    encode_config
        .encode_codec_config
        .h264_config
        .h264_vui_parameters
        .timing_info_present_flag = 1;
    encode_config
        .encode_codec_config
        .h264_config
        .h264_vui_parameters
        .num_unit_in_ticks = 1;
    encode_config
        .encode_codec_config
        .h264_config
        .h264_vui_parameters
        .time_scale = config.fps * 2;

    encode_config
}

fn nvenc_initialize_params(
    config: &EncoderConfig,
    encode_config: &mut NvEncConfig,
) -> NvEncInitializeParams {
    let mut init = zeroed::<NvEncInitializeParams>();
    init.version = nvenc_struct_version_with_flag(7);
    init.encode_guid = NV_ENC_CODEC_H264_GUID;
    init.preset_guid = NV_ENC_PRESET_P1_GUID;
    init.encode_width = config.width;
    init.encode_height = config.height;
    init.dar_width = config.width;
    init.dar_height = config.height;
    init.frame_rate_num = config.fps;
    init.frame_rate_den = 1;
    init.enable_encode_async = 0;
    init.enable_ptd = 1;
    init.encode_config = encode_config;
    init.max_encode_width = config.width;
    init.max_encode_height = config.height;
    init.tuning_info = NV_ENC_TUNING_INFO_LOW_LATENCY;
    init.buffer_format = NV_ENC_BUFFER_FORMAT_NV12;
    init
}

const fn nvenc_struct_version(version: u32) -> u32 {
    NVENCAPI_VERSION | (version << 16) | NVENCAPI_STRUCT_VERSION_TAG
}

const fn nvenc_struct_version_with_flag(version: u32) -> u32 {
    nvenc_struct_version(version) | NVENCAPI_STRUCT_VERSION_FLAG
}

fn zeroed<T>() -> T {
    unsafe { std::mem::zeroed() }
}

impl NvencApi {
    fn load() -> Result<Self> {
        let library = unsafe { LoadLibraryA(s!("nvEncodeAPI64.dll")) }
            .context("nvEncodeAPI64.dll is not available")?;

        let get_max_supported_version = unsafe {
            load_symbol::<NvEncodeApiGetMaxSupportedVersion>(
                library,
                s!("NvEncodeAPIGetMaxSupportedVersion"),
            )
        }
        .context("load NvEncodeAPIGetMaxSupportedVersion")?;
        let create_instance = unsafe {
            load_symbol::<NvEncodeApiCreateInstance>(library, s!("NvEncodeAPICreateInstance"))
        }
        .context("load NvEncodeAPICreateInstance")?;

        let mut functions = zeroed::<NvEncodeApiFunctionList>();
        functions.version = nvenc_struct_version(2);
        let status = unsafe { create_instance(&mut functions) };
        if status != NV_ENC_SUCCESS {
            anyhow::bail!("NvEncodeAPICreateInstance failed with status {status}");
        }

        Ok(Self {
            library,
            get_max_supported_version,
            functions,
        })
    }

    fn max_supported_version(&self) -> Result<u32> {
        let mut version = 0;
        let status = unsafe { (self.get_max_supported_version)(&mut version) };
        if status != 0 {
            anyhow::bail!("NvEncodeAPIGetMaxSupportedVersion failed with status {status}");
        }
        Ok(version)
    }

    fn check(&self, session: *mut c_void, status: NvencStatus, action: &str) -> Result<()> {
        if status == NV_ENC_SUCCESS {
            return Ok(());
        }

        if let Some(get_last_error) = self.functions.nv_enc_get_last_error_string {
            let message = unsafe { get_last_error(session) };
            if !message.is_null() {
                let message = unsafe { CStr::from_ptr(message) }.to_string_lossy();
                anyhow::bail!("{action} failed with NVENC status {status}: {message}");
            }
        }

        anyhow::bail!("{action} failed with NVENC status {status}");
    }
}

impl Drop for NvencApi {
    fn drop(&mut self) {
        unsafe {
            let _ = free_library(self.library);
        }
    }
}

#[link(name = "kernel32")]
unsafe extern "system" {
    fn FreeLibrary(hlibmodule: HMODULE) -> windows::Win32::Foundation::BOOL;
}

unsafe fn free_library(module: HMODULE) -> windows::Win32::Foundation::BOOL {
    FreeLibrary(module)
}

unsafe fn load_symbol<T>(library: HMODULE, name: PCSTR) -> Result<T>
where
    T: Copy,
{
    let symbol = GetProcAddress(library, name);
    let Some(symbol) = symbol else {
        anyhow::bail!("symbol not found");
    };
    Ok(std::mem::transmute_copy(&symbol))
}

struct MediaFoundationEncoder {
    state: Mutex<MediaFoundationState>,
}

struct MediaFoundationState {
    config: EncoderConfig,
    transform: IMFTransform,
    output_provides_samples: bool,
    output_buffer_size: u32,
    frame_index: u64,
    requested_bitrate_kbps: u32,
    pending_keyframe: bool,
}

impl MediaFoundationEncoder {
    fn new(config: &EncoderConfig) -> Result<Self> {
        unsafe {
            MFStartup(MF_VERSION, MFSTARTUP_FULL).context("start Media Foundation")?;
        }
        let state = MediaFoundationState::new(config)?;
        Ok(Self {
            state: Mutex::new(state),
        })
    }

    fn encode(
        &self,
        frame: &crate::windows::capture::CapturedFrame,
        params: EncodeParams,
    ) -> Result<EncodedFrame> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Media Foundation encoder lock poisoned"))?;
        state.encode(frame, params)
    }

    fn set_bitrate_kbps(&self, bitrate_kbps: u32) -> Result<()> {
        if bitrate_kbps == 0 {
            anyhow::bail!("bitrate must be greater than zero");
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Media Foundation encoder lock poisoned"))?;
        state.requested_bitrate_kbps = bitrate_kbps;
        Ok(())
    }

    fn request_keyframe(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.pending_keyframe = true;
        }
    }
}

impl MediaFoundationState {
    fn new(config: &EncoderConfig) -> Result<Self> {
        let transform = activate_h264_encoder().context("activate Media Foundation H.264 MFT")?;
        configure_h264_transform(&transform, config)
            .context("configure Media Foundation H.264 MFT")?;
        let output_info = unsafe {
            transform
                .GetOutputStreamInfo(0)
                .context("get Media Foundation encoder output stream info")?
        };
        let output_provides_samples = output_info.dwFlags
            & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32
                | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
            != 0;

        unsafe {
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)
                .context("begin Media Foundation H.264 streaming")?;
            transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)
                .context("start Media Foundation H.264 stream")?;
        }

        Ok(Self {
            config: config.clone(),
            transform,
            output_provides_samples,
            output_buffer_size: output_info.cbSize.max(encoded_buffer_size(config)),
            frame_index: 0,
            requested_bitrate_kbps: config.bitrate_kbps,
            pending_keyframe: true,
        })
    }

    fn recreate_if_needed(&mut self) -> Result<()> {
        if self.requested_bitrate_kbps == self.config.bitrate_kbps {
            return Ok(());
        }

        unsafe {
            let _ = self
                .transform
                .ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_COMMAND_FLUSH, 0);
        }

        self.config.bitrate_kbps = self.requested_bitrate_kbps;
        let next = Self::new(&self.config).context("recreate Media Foundation encoder")?;
        self.transform = next.transform;
        self.output_provides_samples = next.output_provides_samples;
        self.output_buffer_size = next.output_buffer_size;
        self.pending_keyframe = true;
        Ok(())
    }

    fn encode(
        &mut self,
        frame: &crate::windows::capture::CapturedFrame,
        params: EncodeParams,
    ) -> Result<EncodedFrame> {
        validate_frame(frame)?;
        self.requested_bitrate_kbps = params.bitrate_kbps;
        self.recreate_if_needed()?;

        if params.force_keyframe || self.pending_keyframe {
            if let Ok(force_keyframe) = self.transform.cast::<IWMVideoForceKeyFrame>() {
                unsafe {
                    let _ = force_keyframe.SetKeyFrame();
                }
            }
            self.pending_keyframe = false;
        }

        let nv12 = bgra_to_nv12(frame)?;
        let duration_hns = HNS_PER_SECOND / self.config.fps as i64;
        let sample_time = self.frame_index as i64 * duration_hns;
        let input = sample_from_bytes(&nv12, sample_time, duration_hns)?;

        unsafe {
            self.transform
                .ProcessInput(0, &input, 0)
                .context("feed frame to Media Foundation H.264 MFT")?;
        }

        let mut output = self.drain_output()?;
        self.frame_index += 1;

        if output.is_empty() {
            anyhow::bail!("Media Foundation H.264 MFT accepted input but produced no output");
        }

        output = ensure_annex_b(&output);
        let is_keyframe = annex_b_contains_idr(&output);

        Ok(EncodedFrame {
            data: output,
            is_keyframe,
            timestamp: (sample_time / 10) as u64,
            backend: EncoderBackendKind::MediaFoundation,
        })
    }

    fn drain_output(&self) -> Result<Vec<u8>> {
        let mut all = Vec::new();

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
                self.transform
                    .ProcessOutput(0, std::slice::from_mut(&mut output), &mut status)
            };

            let sample = unsafe { ManuallyDrop::take(&mut output.pSample) };
            let events = unsafe { ManuallyDrop::take(&mut output.pEvents) };
            drop(events);

            match result {
                Ok(()) => {
                    if status & MFT_PROCESS_OUTPUT_STATUS_NEW_STREAMS.0 as u32 != 0 {
                        anyhow::bail!("Media Foundation H.264 MFT requested unsupported dynamic stream change");
                    }
                    if let Some(sample) = sample {
                        all.extend(read_sample_bytes(&sample)?);
                    }
                }
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => break,
                Err(error) => return Err(error).context("drain Media Foundation H.264 output"),
            }
        }

        Ok(all)
    }
}

fn activate_h264_encoder() -> Result<IMFTransform> {
    let input = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_NV12,
    };
    let output = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };
    let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
    let mut count = 0;

    unsafe {
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            MFT_ENUM_FLAG_ALL,
            Some(&input),
            Some(&output),
            &mut activates,
            &mut count,
        )
        .context("enumerate Media Foundation H.264 encoders")?;
    }

    let activates = MftActivates::new(activates, count);
    let activate = activates
        .first()
        .cloned()
        .context("no Media Foundation H.264 encoder MFT registered")?;

    unsafe {
        activate
            .ActivateObject::<IMFTransform>()
            .context("activate Media Foundation H.264 encoder")
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

fn configure_h264_transform(transform: &IMFTransform, config: &EncoderConfig) -> Result<()> {
    let output_type = unsafe { MFCreateMediaType().context("create H.264 output media type")? };
    unsafe {
        output_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        output_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
        output_type.SetUINT32(&MF_MT_AVG_BITRATE, config.bitrate_kbps * 1_000)?;
        output_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        output_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            pack_u32_pair(config.width, config.height),
        )?;
        output_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(config.fps, 1))?;
        transform
            .SetOutputType(0, &output_type, 0)
            .context("set Media Foundation H.264 output type")?;
    }

    let input_type = unsafe { MFCreateMediaType().context("create NV12 input media type")? };
    unsafe {
        input_type.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
        input_type.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
        input_type.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)?;
        input_type.SetUINT64(
            &MF_MT_FRAME_SIZE,
            pack_u32_pair(config.width, config.height),
        )?;
        input_type.SetUINT64(&MF_MT_FRAME_RATE, pack_u32_pair(config.fps, 1))?;
        transform
            .SetInputType(0, &input_type, 0)
            .context("set Media Foundation NV12 input type")?;
    }

    Ok(())
}

fn pack_u32_pair(high: u32, low: u32) -> u64 {
    ((high as u64) << 32) | low as u64
}

fn validate_frame(frame: &crate::windows::capture::CapturedFrame) -> Result<()> {
    if frame.width == 0 || frame.height == 0 {
        anyhow::bail!("captured frame dimensions must be non-zero");
    }
    if frame.width % 2 != 0 || frame.height % 2 != 0 {
        anyhow::bail!("captured frame dimensions must be even for NV12 conversion");
    }
    if frame.format != crate::windows::capture::PixelFormat::Bgra8 {
        anyhow::bail!("Windows H.264 encoder currently expects BGRA capture frames");
    }
    let expected_len = frame.width as usize * frame.height as usize * 4;
    if frame.data.len() != expected_len {
        anyhow::bail!(
            "captured frame size mismatch: got {} bytes, expected {expected_len}",
            frame.data.len()
        );
    }
    Ok(())
}

fn bgra_to_nv12(frame: &crate::windows::capture::CapturedFrame) -> Result<Vec<u8>> {
    validate_frame(frame)?;
    let width = frame.width as usize;
    let height = frame.height as usize;
    let y_len = width * height;
    let mut out = vec![0u8; y_len + y_len / 2];

    for y in 0..height {
        for x in 0..width {
            let idx = (y * width + x) * 4;
            let b = frame.data[idx] as f32;
            let g = frame.data[idx + 1] as f32;
            let r = frame.data[idx + 2] as f32;
            out[y * width + x] = clamp_u8(0.257 * r + 0.504 * g + 0.098 * b + 16.0);
        }
    }

    for y in (0..height).step_by(2) {
        for x in (0..width).step_by(2) {
            let mut u = 0.0;
            let mut v = 0.0;
            for dy in 0..2 {
                for dx in 0..2 {
                    let idx = ((y + dy) * width + (x + dx)) * 4;
                    let b = frame.data[idx] as f32;
                    let g = frame.data[idx + 1] as f32;
                    let r = frame.data[idx + 2] as f32;
                    u += -0.148 * r - 0.291 * g + 0.439 * b + 128.0;
                    v += 0.439 * r - 0.368 * g - 0.071 * b + 128.0;
                }
            }
            let uv_idx = y_len + (y / 2) * width + x;
            out[uv_idx] = clamp_u8(u / 4.0);
            out[uv_idx + 1] = clamp_u8(v / 4.0);
        }
    }

    Ok(out)
}

fn clamp_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

fn sample_from_bytes(data: &[u8], sample_time: i64, duration: i64) -> Result<IMFSample> {
    let sample = unsafe { MFCreateSample().context("create Media Foundation sample")? };
    let buffer = unsafe {
        MFCreateMemoryBuffer(data.len() as u32).context("create Media Foundation input buffer")?
    };
    write_buffer(&buffer, data)?;
    unsafe {
        sample.AddBuffer(&buffer).context("attach input buffer")?;
        sample.SetSampleTime(sample_time)?;
        sample.SetSampleDuration(duration)?;
    }
    Ok(sample)
}

fn empty_sample(size: u32) -> Result<IMFSample> {
    let sample = unsafe { MFCreateSample().context("create Media Foundation output sample")? };
    let buffer =
        unsafe { MFCreateMemoryBuffer(size).context("create Media Foundation output buffer")? };
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
            .context("lock Media Foundation buffer")?;
        std::ptr::copy_nonoverlapping(data.as_ptr(), ptr, data.len());
        buffer.Unlock().context("unlock Media Foundation buffer")?;
        buffer
            .SetCurrentLength(data.len() as u32)
            .context("set Media Foundation buffer length")?;
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

fn encoded_buffer_size(config: &EncoderConfig) -> u32 {
    (config.width as usize * config.height as usize * 4).min(u32::MAX as usize) as u32
}

fn ensure_annex_b(data: &[u8]) -> Vec<u8> {
    if has_annex_b_start_code(data) {
        return data.to_vec();
    }

    let mut offset = 0;
    let mut out = Vec::with_capacity(data.len() + 16);
    while offset + 4 <= data.len() {
        let nalu_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        if nalu_len == 0 || offset + nalu_len > data.len() {
            return data.to_vec();
        }
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(&data[offset..offset + nalu_len]);
        offset += nalu_len;
    }

    if offset == data.len() && !out.is_empty() {
        out
    } else {
        data.to_vec()
    }
}

fn has_annex_b_start_code(data: &[u8]) -> bool {
    data.windows(3).any(|w| w == [0, 0, 1]) || data.windows(4).any(|w| w == [0, 0, 0, 1])
}

fn annex_b_contains_idr(data: &[u8]) -> bool {
    annex_b_nalu_types(data).any(|nalu_type| nalu_type == 5 || nalu_type == 7)
}

fn annex_b_nalu_types(data: &[u8]) -> impl Iterator<Item = u8> + '_ {
    let mut offset = 0;
    std::iter::from_fn(move || {
        while offset + 4 <= data.len() {
            let start_len = if data[offset..].starts_with(&[0, 0, 0, 1]) {
                4
            } else if data[offset..].starts_with(&[0, 0, 1]) {
                3
            } else {
                offset += 1;
                continue;
            };
            offset += start_len;
            if offset < data.len() {
                let nalu_type = data[offset] & 0x1f;
                return Some(nalu_type);
            }
        }
        None
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::windows::capture::{CapturedFrame, PixelFormat};
    use std::time::Instant;

    #[test]
    fn converts_bgra_to_nv12() {
        let frame = CapturedFrame {
            data: vec![
                0, 0, 0, 255, 255, 255, 255, 255, 0, 0, 255, 255, 255, 0, 0, 255,
            ],
            width: 2,
            height: 2,
            format: PixelFormat::Bgra8,
            timestamp: Instant::now(),
        };

        let nv12 = bgra_to_nv12(&frame).unwrap();
        assert_eq!(nv12.len(), 6);
    }

    #[test]
    fn converts_avcc_lengths_to_annex_b() {
        let avcc = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 2, 0x41, 0x99];
        let annex_b = ensure_annex_b(&avcc);
        assert_eq!(&annex_b[..4], &[0, 0, 0, 1]);
        assert!(annex_b_contains_idr(&annex_b));
    }
}
