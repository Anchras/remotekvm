// Convert VideoToolbox-emitted CMSampleBuffers (AVCC, length-prefixed) into annex-B
// (start-code-prefixed) NAL units suitable for HEVC files and RTP packetization.
//
// On keyframes the VPS/SPS/PPS parameter sets must be prepended; they live in the
// CMVideoFormatDescription, not the block buffer.

use anyhow::{anyhow, Result};
use objc2::rc::Retained;
use objc2_core_media::{CMBlockBuffer, CMFormatDescription, CMSampleBuffer};
use std::ffi::c_void;
use std::ptr::NonNull;

const ANNEX_B_START_CODE: [u8; 4] = [0, 0, 0, 1];

pub fn sample_to_annex_b(sample: &CMSampleBuffer, is_keyframe: bool) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(64 * 1024);

    if is_keyframe {
        let fmt = unsafe { sample.format_description() }
            .ok_or_else(|| anyhow!("sample has no format description"))?;
        append_hevc_parameter_sets(&fmt, &mut out)?;
    }

    let block = unsafe { sample.data_buffer() }
        .ok_or_else(|| anyhow!("sample has no data buffer"))?;
    append_avcc_as_annex_b(&block, &mut out)?;

    Ok(out)
}

fn append_hevc_parameter_sets(fmt: &CMFormatDescription, out: &mut Vec<u8>) -> Result<()> {
    let mut count: usize = 0;
    let mut nal_header_len: i32 = 0;
    // First call: ask for the parameter set count and NAL header length (should be 4 for AVCC).
    let status = unsafe {
        CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
            fmt,
            0,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            &mut count,
            &mut nal_header_len,
        )
    };
    if status != 0 {
        return Err(anyhow!("GetHEVCParameterSetAtIndex(count) status={status}"));
    }
    for i in 0..count {
        let mut data_ptr: *const u8 = std::ptr::null();
        let mut size: usize = 0;
        let status = unsafe {
            CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
                fmt,
                i,
                &mut data_ptr,
                &mut size,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if status != 0 || data_ptr.is_null() {
            return Err(anyhow!("GetHEVCParameterSetAtIndex({i}) status={status}"));
        }
        out.extend_from_slice(&ANNEX_B_START_CODE);
        out.extend_from_slice(unsafe { std::slice::from_raw_parts(data_ptr, size) });
    }
    Ok(())
}

fn append_avcc_as_annex_b(block: &CMBlockBuffer, out: &mut Vec<u8>) -> Result<()> {
    let total = unsafe { block.data_length() };
    let mut buf = vec![0u8; total];
    let status = unsafe {
        block.copy_data_bytes(0, total, NonNull::new(buf.as_mut_ptr() as *mut c_void).unwrap())
    };
    if status != 0 {
        return Err(anyhow!("CMBlockBufferCopyDataBytes status={status}"));
    }
    let mut pos = 0;
    while pos + 4 <= buf.len() {
        let nal_len = u32::from_be_bytes([buf[pos], buf[pos + 1], buf[pos + 2], buf[pos + 3]]) as usize;
        pos += 4;
        if pos + nal_len > buf.len() {
            return Err(anyhow!("truncated NAL: declared {nal_len} bytes, only {} remaining", buf.len() - pos));
        }
        out.extend_from_slice(&ANNEX_B_START_CODE);
        out.extend_from_slice(&buf[pos..pos + nal_len]);
        pos += nal_len;
    }
    Ok(())
}

extern "C-unwind" {
    // Not yet exposed in objc2-core-media's safe surface (as of 0.3), so bind directly.
    fn CMVideoFormatDescriptionGetHEVCParameterSetAtIndex(
        videoDesc: &CMFormatDescription,
        parameterSetIndex: usize,
        parameterSetPointerOut: *mut *const u8,
        parameterSetSizeOut: *mut usize,
        parameterSetCountOut: *mut usize,
        NALUnitHeaderLengthOut: *mut i32,
    ) -> i32;
}
