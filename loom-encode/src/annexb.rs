//! AVCC (length-prefixed) → Annex-B (start-code prefixed) NAL conversion.
//!
//! VideoToolbox emits `hvcC`-style access units: each NAL unit is preceded by a
//! big-endian length field (the size of which the format description reports),
//! and the parameter sets live *out of band* in the format description rather
//! than in the stream. §4.1 and §5.2 want the opposite on the wire — Annex-B with
//! VPS/SPS/PPS in every IDR — so [`crate::videotoolbox`] does both conversions.
//!
//! Pure byte work, so it is unit-tested directly: this is where an off-by-one in
//! the length field would otherwise become a mystery decoder failure.

use crate::EncodeError;

/// The 4-byte Annex-B start code prefixed to every emitted NAL.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// HEVC NAL types for IDR access units (§5.2), from the H.265 spec table 7-1.
const NAL_IDR_W_RADL: u8 = 19;
const NAL_IDR_N_LP: u8 = 20;

/// Append `src`'s length-prefixed NALs to `out` as Annex-B, returning whether any
/// of them was an IDR slice. `nal_length_size` is the prefix width the format
/// description reported (VideoToolbox uses 4, but it is read, not assumed).
pub fn append(src: &[u8], nal_length_size: usize, out: &mut Vec<u8>) -> Result<bool, EncodeError> {
    let mut keyframe = false;
    let mut offset = 0;

    while offset < src.len() {
        if offset + nal_length_size > src.len() {
            return Err(EncodeError::Bitstream("truncated NAL length prefix"));
        }

        // The prefix is big-endian and 1..=4 bytes wide.
        let mut length = 0usize;
        for &byte in &src[offset..offset + nal_length_size] {
            length = (length << 8) | byte as usize;
        }
        offset += nal_length_size;

        let end = offset
            .checked_add(length)
            .filter(|&end| end <= src.len())
            .ok_or(EncodeError::Bitstream("NAL length exceeds the access unit"))?;
        if length == 0 {
            return Err(EncodeError::Bitstream("zero-length NAL"));
        }

        // HEVC NAL header: forbidden_zero(1) | type(6) | layer(6) | tid(3).
        let nal_type = (src[offset] >> 1) & 0x3f;
        keyframe |= nal_type == NAL_IDR_W_RADL || nal_type == NAL_IDR_N_LP;

        out.extend_from_slice(&START_CODE);
        out.extend_from_slice(&src[offset..end]);
        offset = end;
    }

    Ok(keyframe)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One NAL, 4-byte prefix: a P slice (type 1) with a 3-byte payload.
    #[test]
    fn converts_length_prefix_to_start_code() {
        let src = [0, 0, 0, 4, 0x02, 0x01, 0xAA, 0xBB];

        let mut out = Vec::new();
        let keyframe = append(&src, 4, &mut out).expect("convert");

        assert!(!keyframe, "NAL type 1 is not an IDR");
        assert_eq!(out, [0, 0, 0, 1, 0x02, 0x01, 0xAA, 0xBB]);
    }

    /// Two NALs in one AU, the second an IDR_W_RADL (type 19 → header byte 0x26).
    #[test]
    fn walks_every_nal_and_flags_idr() {
        let src = [
            0, 0, 0, 2, 0x4E, 0x01, // SEI (type 39)
            0, 0, 0, 2, 0x26, 0x01, // IDR_W_RADL (type 19)
        ];

        let mut out = Vec::new();
        let keyframe = append(&src, 4, &mut out).expect("convert");

        assert!(keyframe, "an IDR NAL must be reported");
        assert_eq!(
            out,
            [0, 0, 0, 1, 0x4E, 0x01, 0, 0, 0, 1, 0x26, 0x01],
            "both NALs get start codes"
        );
    }

    #[test]
    fn honors_a_non_four_byte_length_size() {
        let src = [0x02, 0x26, 0x01]; // 2-byte NAL, 1-byte prefix

        let mut out = Vec::new();
        let keyframe = append(&src, 1, &mut out).expect("convert");

        assert!(keyframe);
        assert_eq!(out, [0, 0, 0, 1, 0x26, 0x01]);
    }

    #[test]
    fn rejects_a_length_running_past_the_buffer() {
        let src = [0, 0, 0, 9, 0x26, 0x01]; // claims 9 bytes, has 2

        let mut out = Vec::new();
        assert!(append(&src, 4, &mut out).is_err());
    }
}
