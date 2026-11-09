//! Portable H.264 Annex-B and NV12 helpers shared by the Windows encoder.
//!
//! These stay outside the `#[cfg(windows)]` modules so the bitstream rules can
//! be tested on any host, including the NV12 plane split the D3D11 upload uses.

use anyhow::{bail, Result};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Nv12Planes<'a> {
    pub y: &'a [u8],
    pub uv: &'a [u8],
    /// Bytes per Y row. Tightly packed NV12 uses one byte per luma sample.
    pub y_row_pitch: u32,
    /// Bytes per interleaved UV row. Tightly packed NV12 uses `width` bytes.
    pub uv_row_pitch: u32,
    /// Width of the UV plane in R8G8 texels (`width / 2`).
    pub uv_width: u32,
    /// Height of the UV plane in R8G8 texels (`height / 2`).
    pub uv_height: u32,
}

/// Split a tightly packed NV12 buffer into its Y and interleaved UV planes.
///
/// D3D11 `DXGI_FORMAT_NV12` exposes those planes as subresource 0 and 1.
/// Uploading the whole buffer to subresource 0 writes only luma.
pub fn split_nv12(nv12: &[u8], width: u32, height: u32) -> Result<Nv12Planes<'_>> {
    if width == 0 || height == 0 {
        bail!("NV12 dimensions must be non-zero");
    }
    if width % 2 != 0 || height % 2 != 0 {
        bail!("NV12 dimensions must be even");
    }
    let y_len = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| anyhow::anyhow!("NV12 Y plane size overflow"))?;
    let uv_len = y_len / 2;
    let expected = y_len + uv_len;
    if nv12.len() != expected {
        bail!(
            "NV12 size mismatch: got {} bytes, expected {expected} for {width}x{height}",
            nv12.len()
        );
    }
    let (y, uv) = nv12.split_at(y_len);
    Ok(Nv12Planes {
        y,
        uv,
        y_row_pitch: width,
        uv_row_pitch: width,
        uv_width: width / 2,
        uv_height: height / 2,
    })
}

#[derive(Debug, Default, Clone)]
pub struct H264ParameterSetCache {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

/// Remember SPS/PPS and, when an IDR is missing them, prepend the cached sets.
///
/// WebRTC clients need parameter sets in front of every keyframe. The Media
/// Foundation H.264 MFT often emits SPS/PPS only on the first sample.
pub fn with_parameter_sets_before_idr(
    cache: &mut H264ParameterSetCache,
    annex_b: &[u8],
) -> Vec<u8> {
    let units = annex_b_units(annex_b);
    let mut saw_sps_before_idr = false;
    let mut idr_needs_sets = false;
    for unit in &units {
        match unit.nal_type {
            7 => {
                cache.sps = Some(annex_b[unit.start..unit.end].to_vec());
                if !idr_needs_sets {
                    saw_sps_before_idr = true;
                }
            }
            8 => {
                cache.pps = Some(annex_b[unit.start..unit.end].to_vec());
            }
            5 if !saw_sps_before_idr => idr_needs_sets = true,
            _ => {}
        }
    }

    if idr_needs_sets {
        if let (Some(sps), Some(pps)) = (&cache.sps, &cache.pps) {
            let mut out = Vec::with_capacity(sps.len() + pps.len() + annex_b.len());
            out.extend_from_slice(sps);
            out.extend_from_slice(pps);
            out.extend_from_slice(annex_b);
            return out;
        }
    }
    annex_b.to_vec()
}

pub fn ensure_annex_b(data: &[u8]) -> Vec<u8> {
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

pub fn annex_b_contains_idr(data: &[u8]) -> bool {
    annex_b_units(data)
        .into_iter()
        .any(|unit| unit.nal_type == 5 || unit.nal_type == 7)
}

fn has_annex_b_start_code(data: &[u8]) -> bool {
    data.windows(3).any(|window| window == [0, 0, 1])
        || data.windows(4).any(|window| window == [0, 0, 0, 1])
}

struct AnnexBNal {
    start: usize,
    end: usize,
    nal_type: u8,
}

fn annex_b_units(data: &[u8]) -> Vec<AnnexBNal> {
    let mut units = Vec::new();
    let mut index = 0;
    let mut current: Option<(usize, usize)> = None;
    while index + 3 <= data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        if let Some((start, header)) = current.take() {
            if header < data.len() {
                units.push(AnnexBNal {
                    start,
                    end: index,
                    nal_type: data[header] & 0x1f,
                });
            }
        }
        let header = index + start_len;
        current = Some((index, header));
        index = header;
    }
    if let Some((start, header)) = current {
        if header < data.len() {
            units.push(AnnexBNal {
                start,
                end: data.len(),
                nal_type: data[header] & 0x1f,
            });
        }
    }
    units
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_packed_nv12_into_luma_and_chroma() {
        let data: Vec<u8> = (0..12).collect();
        let planes = split_nv12(&data, 4, 2).unwrap();

        assert_eq!(planes.y, &data[..8]);
        assert_eq!(planes.uv, &data[8..]);
        assert_eq!(planes.y_row_pitch, 4);
        assert_eq!(planes.uv_row_pitch, 4);
        assert_eq!(planes.uv_width, 2);
        assert_eq!(planes.uv_height, 1);
    }

    #[test]
    fn rejects_odd_nv12_dimensions() {
        assert!(split_nv12(&[0, 0, 0], 1, 2).is_err());
    }

    #[test]
    fn converts_avcc_lengths_to_annex_b() {
        let avcc = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 2, 0x41, 0x99];
        let annex_b = ensure_annex_b(&avcc);
        assert_eq!(&annex_b[..4], &[0, 0, 0, 1]);
        assert!(annex_b_contains_idr(&annex_b));
    }

    #[test]
    fn prepends_cached_parameter_sets_to_a_bare_idr() {
        let mut cache = H264ParameterSetCache::default();
        let parameter_sets = [0, 0, 0, 1, 0x67, 0x42, 0x11, 0, 0, 0, 1, 0x68, 0xce];
        assert_eq!(
            with_parameter_sets_before_idr(&mut cache, &parameter_sets),
            parameter_sets
        );

        let idr = [0, 0, 0, 1, 0x65, 0x88];
        let with_sets = with_parameter_sets_before_idr(&mut cache, &idr);
        assert_eq!(
            with_sets,
            [0, 0, 0, 1, 0x67, 0x42, 0x11, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88]
        );
    }

    #[test]
    fn leaves_idr_that_already_carries_parameter_sets_unchanged() {
        let mut cache = H264ParameterSetCache::default();
        let sample = [
            0, 0, 0, 1, 0x67, 0x42, 0x11, 0, 0, 0, 1, 0x68, 0xce, 0, 0, 0, 1, 0x65, 0x88,
        ];
        assert_eq!(with_parameter_sets_before_idr(&mut cache, &sample), sample);
    }

    #[test]
    fn does_not_prepend_parameter_sets_to_delta_frames() {
        let mut cache = H264ParameterSetCache::default();
        let parameter_sets = [0, 0, 0, 1, 0x67, 0x42, 0x11, 0, 0, 0, 1, 0x68, 0xce];
        with_parameter_sets_before_idr(&mut cache, &parameter_sets);

        let p_frame = [0, 0, 0, 1, 0x41, 0x9a];
        assert_eq!(
            with_parameter_sets_before_idr(&mut cache, &p_frame),
            p_frame
        );
    }
}
