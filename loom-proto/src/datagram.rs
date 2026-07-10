//! Video/audio datagram framing — PROTOCOL.md §4.
//!
//! The 12-byte, big-endian header:
//! ```text
//! 0  u8   magic     = 0x4C ('L')
//! 1  u8   flags     bit0 KEYFRAME, bit1 LAST_FRAGMENT, bits2-7 reserved
//! 2  u16  stream_id 0=video, 1=audio
//! 4  u32  frame_seq per-stream, +1 per frame, never wraps
//! 8  u16  frag_index
//! 10 u16  frag_count
//! ```

/// Header magic byte: ASCII `'L'`.
pub const MAGIC: u8 = 0x4C;
/// `flags` bit 0 — set on all fragments of a keyframe (video only).
pub const FLAG_KEYFRAME: u8 = 0x01;
/// `flags` bit 1 — set iff `frag_index == frag_count - 1`.
pub const FLAG_LAST_FRAGMENT: u8 = 0x02;
/// Fixed header length in bytes.
pub const HEADER_LEN: usize = 12;
/// Maximum total datagram size (header + payload), PROTOCOL.md §2.
pub const MAX_DATAGRAM_LEN: usize = 1350;

/// A decoded (or to-be-encoded) datagram header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DatagramHeader {
    /// KEYFRAME flag (video only).
    pub keyframe: bool,
    /// LAST_FRAGMENT flag; must equal `frag_index == frag_count - 1`.
    pub last_fragment: bool,
    /// 0 = video, 1 = audio.
    pub stream_id: u16,
    /// Per-stream monotonically increasing frame counter.
    pub frame_seq: u32,
    /// 0-based fragment index within the frame.
    pub frag_index: u16,
    /// Total fragment count for this frame (identical across its fragments).
    pub frag_count: u16,
}

impl DatagramHeader {
    /// Construct a header for a fragment, deriving `last_fragment` from the
    /// fragment position as §4 requires (`LAST_FRAGMENT` set iff last fragment).
    pub fn new(keyframe: bool, stream_id: u16, frame_seq: u32, frag_index: u16, frag_count: u16) -> Self {
        let last_fragment = frag_count > 0 && frag_index == frag_count - 1;
        Self {
            keyframe,
            last_fragment,
            stream_id,
            frame_seq,
            frag_index,
            frag_count,
        }
    }

    /// The `flags` byte for this header (reserved bits always zero, per §4).
    pub fn flags(&self) -> u8 {
        let mut f = 0u8;
        if self.keyframe {
            f |= FLAG_KEYFRAME;
        }
        if self.last_fragment {
            f |= FLAG_LAST_FRAGMENT;
        }
        f
    }

    /// Serialize the 12-byte big-endian header.
    pub fn to_bytes(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0] = MAGIC;
        b[1] = self.flags();
        b[2..4].copy_from_slice(&self.stream_id.to_be_bytes());
        b[4..8].copy_from_slice(&self.frame_seq.to_be_bytes());
        b[8..10].copy_from_slice(&self.frag_index.to_be_bytes());
        b[10..12].copy_from_slice(&self.frag_count.to_be_bytes());
        b
    }

    /// Serialize header + payload into one datagram.
    pub fn encode(&self, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(HEADER_LEN + payload.len());
        out.extend_from_slice(&self.to_bytes());
        out.extend_from_slice(payload);
        out
    }
}

/// Why a datagram was dropped on decode. All of these are *silent drops* in
/// production (§6.6); the strings exist only for the conformance vectors.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DropReason {
    /// Fewer than 12 bytes — no room for a header.
    TooShort,
    /// More than 1350 bytes total (§2).
    Oversize,
    /// First byte is not `0x4C`.
    BadMagic,
    /// `frag_count` is zero.
    FragCountZero,
    /// `frag_index >= frag_count`.
    FragIndexRange,
    /// `LAST_FRAGMENT` flag disagrees with `frag_index == frag_count - 1`.
    LastFragmentMismatch,
    /// `stream_id` is neither 0 (video) nor 1 (audio).
    UnknownStream,
}

impl DropReason {
    /// The stable reason string used by the conformance vectors.
    pub fn as_str(&self) -> &'static str {
        match self {
            DropReason::TooShort => "too_short",
            DropReason::Oversize => "oversize",
            DropReason::BadMagic => "bad_magic",
            DropReason::FragCountZero => "frag_count_zero",
            DropReason::FragIndexRange => "frag_index_range",
            DropReason::LastFragmentMismatch => "last_fragment_mismatch",
            DropReason::UnknownStream => "unknown_stream",
        }
    }
}

/// A successfully decoded datagram: its header and the payload length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DecodedDatagram {
    /// The parsed header.
    pub header: DatagramHeader,
    /// Payload length (`total - 12`).
    pub payload_len: usize,
}

/// Decode and validate a datagram header per §4. The validation order matches
/// the normative reference model exactly (length checks precede magic, etc.).
pub fn decode(bytes: &[u8]) -> core::result::Result<DecodedDatagram, DropReason> {
    if bytes.len() < HEADER_LEN {
        return Err(DropReason::TooShort);
    }
    if bytes.len() > MAX_DATAGRAM_LEN {
        return Err(DropReason::Oversize);
    }
    if bytes[0] != MAGIC {
        return Err(DropReason::BadMagic);
    }
    let flags = bytes[1];
    let stream_id = u16::from_be_bytes([bytes[2], bytes[3]]);
    let frame_seq = u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
    let frag_index = u16::from_be_bytes([bytes[8], bytes[9]]);
    let frag_count = u16::from_be_bytes([bytes[10], bytes[11]]);

    if frag_count < 1 {
        return Err(DropReason::FragCountZero);
    }
    if frag_index >= frag_count {
        return Err(DropReason::FragIndexRange);
    }
    let last = flags & FLAG_LAST_FRAGMENT != 0;
    if last != (frag_index == frag_count - 1) {
        return Err(DropReason::LastFragmentMismatch);
    }
    if stream_id != 0 && stream_id != 1 {
        return Err(DropReason::UnknownStream);
    }

    Ok(DecodedDatagram {
        header: DatagramHeader {
            keyframe: flags & FLAG_KEYFRAME != 0,
            last_fragment: last,
            stream_id,
            frame_seq,
            frag_index,
            frag_count,
        },
        payload_len: bytes.len() - HEADER_LEN,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_single_fragment_keyframe() {
        // frag 0 of 1 => last; keyframe => flags 0x03.
        let h = DatagramHeader::new(true, 0, 0, 0, 1);
        assert_eq!(h.flags(), 0x03);
        assert_eq!(
            hex::encode(h.encode(&[0xAA; 4])),
            "4c0300000000000000000001aaaaaaaa"
        );
    }

    #[test]
    fn encode_derives_last_flag() {
        // frag 0 of 3 => not last; keyframe only => 0x01.
        assert_eq!(DatagramHeader::new(true, 0, 0, 0, 3).flags(), 0x01);
        // frag 2 of 3 => last; keyframe => 0x03.
        assert_eq!(DatagramHeader::new(true, 0, 0, 2, 3).flags(), 0x03);
        // audio, not keyframe, single fragment => last only => 0x02.
        assert_eq!(DatagramHeader::new(false, 1, 7, 0, 1).flags(), 0x02);
    }

    #[test]
    fn decode_valid_roundtrip() {
        let bytes = hex::decode("4c03000000000005000000010102").unwrap();
        let d = decode(&bytes).unwrap();
        assert_eq!(
            d.header,
            DatagramHeader {
                keyframe: true,
                last_fragment: true,
                stream_id: 0,
                frame_seq: 5,
                frag_index: 0,
                frag_count: 1,
            }
        );
        assert_eq!(d.payload_len, 2);
    }

    #[test]
    fn decode_reasons() {
        let cases = [
            ("4c0300", DropReason::TooShort),
            ("4d0200000000000000000001", DropReason::BadMagic),
            ("4c0200000000000000000000", DropReason::FragCountZero),
            ("4c0200000000000000030003", DropReason::FragIndexRange),
            ("4c0000000000000000020003", DropReason::LastFragmentMismatch), // idx2/3 no LAST
            ("4c0200000000000000000003", DropReason::LastFragmentMismatch), // LAST on non-last
            ("4c0200090000000000000001", DropReason::UnknownStream),
        ];
        for (h, want) in cases {
            assert_eq!(decode(&hex::decode(h).unwrap()), Err(want), "case {h}");
        }
    }

    #[test]
    fn decode_oversize() {
        let bytes = DatagramHeader::new(false, 0, 0, 0, 1).encode(&vec![0u8; 1339]);
        assert_eq!(bytes.len(), 1351);
        assert_eq!(decode(&bytes), Err(DropReason::Oversize));
        // Exactly 1350 is allowed.
        let ok = DatagramHeader::new(false, 0, 0, 0, 1).encode(&vec![0u8; 1338]);
        assert_eq!(ok.len(), 1350);
        assert!(decode(&ok).is_ok());
    }

    #[test]
    fn decode_ignores_reserved_flag_bits() {
        // flags 0xff = KEYFRAME|LAST|all reserved bits; reserved bits ignored.
        let bytes = hex::decode("4cff00000000000100000001ee").unwrap();
        let d = decode(&bytes).unwrap();
        assert!(d.header.keyframe && d.header.last_fragment);
        assert_eq!(d.payload_len, 1);
    }
}
