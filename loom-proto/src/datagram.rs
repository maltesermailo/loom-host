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
    pub fn new(
        keyframe: bool,
        stream_id: u16,
        frame_seq: u32,
        frag_index: u16,
        frag_count: u16,
    ) -> Self {
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

/// Maximum payload bytes per datagram (§2/§4): `1350 − 12` header.
pub const MAX_PAYLOAD: usize = MAX_DATAGRAM_LEN - HEADER_LEN;

/// Fragment a complete frame **body** into ordered datagrams (§4) — the
/// send-side inverse of [`crate::reassembly`]. `body` is the logical frame body:
/// for video that is `capture_ts (u64 BE) ‖ Annex-B access unit` (§4.1); the
/// timestamp is part of the body and therefore lands only in fragment 0. Each
/// returned datagram is a full header+payload buffer ≤ [`MAX_DATAGRAM_LEN`].
///
/// The body is split into `ceil(len / MAX_PAYLOAD)` fragments (at least one, so
/// an empty body still yields a single well-formed datagram). `frag_count` is
/// identical across the frame's fragments and `LAST_FRAGMENT` is set on the
/// last, exactly as the receiver validates.
pub fn fragment(stream_id: u16, frame_seq: u32, keyframe: bool, body: &[u8]) -> Vec<Vec<u8>> {
    let frag_count = body.len().div_ceil(MAX_PAYLOAD).max(1);
    let mut out = Vec::with_capacity(frag_count);
    // `chunks` yields nothing for an empty slice, so handle that as one empty frag.
    let mut chunks = body.chunks(MAX_PAYLOAD).peekable();
    for idx in 0..frag_count {
        let chunk = chunks.next().unwrap_or(&[]);
        let header = DatagramHeader::new(
            keyframe,
            stream_id,
            frame_seq,
            idx as u16,
            frag_count as u16,
        );
        out.push(header.encode(chunk));
    }
    out
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
    /// `stream_id` is neither 0 (video) nor 1 (audio) nor a negotiated video
    /// stream (§3.4 multi-display; see [`decode_with_streams`]).
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

/// Decode and validate a datagram header per §4, accepting only the two
/// always-present streams: 0 (primary-display video) and 1 (audio). Additional
/// negotiated video streams (`stream_id ≥ 2`, §3.4 multi-display) are rejected as
/// [`DropReason::UnknownStream`]; use [`decode_with_streams`] once they have been
/// negotiated. The validation order matches the normative reference model exactly
/// (length checks precede magic, etc.).
pub fn decode(bytes: &[u8]) -> core::result::Result<DecodedDatagram, DropReason> {
    decode_with_streams(bytes, &[])
}

/// Decode and validate a datagram header per §4, additionally accepting the
/// `extra_video_streams` — the video `stream_id`s ≥ 2 negotiated for this session
/// via CONFIG key 6 (§3.4 multi-display). A `stream_id` that is neither 0, 1, nor
/// one of these is dropped as [`DropReason::UnknownStream`], exactly as §4
/// requires for an un-negotiated stream. All other validation is identical to
/// [`decode`].
pub fn decode_with_streams(
    bytes: &[u8],
    extra_video_streams: &[u16],
) -> core::result::Result<DecodedDatagram, DropReason> {
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
    if stream_id != 0 && stream_id != 1 && !extra_video_streams.contains(&stream_id) {
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
    fn fragment_round_trips_through_decode_and_reassembly() {
        use crate::reassembly::{Event, Fragment, Reassembler};

        // A body spanning several fragments (2.5× the payload limit).
        let body: Vec<u8> = (0..MAX_PAYLOAD * 2 + 5).map(|i| i as u8).collect();
        let frames = fragment(0, 9, true, &body);
        assert_eq!(frames.len(), 3);

        let mut reassembled = vec![0u8; body.len()];
        let mut r = Reassembler::new();
        for (i, dg) in frames.iter().enumerate() {
            let d = decode(dg).expect("valid datagram");
            assert_eq!(d.header.frame_seq, 9);
            assert_eq!(d.header.frag_count, 3);
            assert!(d.header.keyframe);
            // Copy this fragment's payload back into its slot to prove ordering.
            let off = i * MAX_PAYLOAD;
            let payload = &dg[HEADER_LEN..];
            reassembled[off..off + payload.len()].copy_from_slice(payload);
            r.push(
                0,
                Fragment {
                    frame_seq: d.header.frame_seq,
                    frag_index: d.header.frag_index,
                    frag_count: d.header.frag_count,
                    keyframe: d.header.keyframe,
                },
            );
        }
        assert_eq!(reassembled, body);
        // The metadata state machine delivers exactly the one keyframe.
        assert!(matches!(
            r.events(),
            [Event::Deliver {
                frame_seq: 9,
                keyframe: true,
                ..
            }]
        ));
    }

    #[test]
    fn fragment_empty_and_small_bodies() {
        assert_eq!(fragment(1, 0, false, &[]).len(), 1); // one well-formed empty frag
        let one = fragment(0, 1, false, &[1, 2, 3]);
        assert_eq!(one.len(), 1);
        assert!(decode(&one[0]).unwrap().header.last_fragment);
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
    fn decode_with_streams_gates_negotiated_video() {
        // stream_id 2 is a valid video display only once negotiated (§3.4).
        let dg = DatagramHeader::new(true, 2, 3, 0, 1).encode(&[0xDD; 16]);
        // Default decode() keeps the v1 {0,1} set → dropped.
        assert_eq!(decode(&dg), Err(DropReason::UnknownStream));
        // Un-negotiated even with a different extra stream → still dropped.
        assert_eq!(
            decode_with_streams(&dg, &[3]),
            Err(DropReason::UnknownStream)
        );
        // Negotiated → decodes, carrying stream_id 2.
        let d = decode_with_streams(&dg, &[2, 3]).expect("stream 2 negotiated");
        assert_eq!(d.header.stream_id, 2);
        assert!(d.header.keyframe && d.header.last_fragment);
        // Streams 0 and 1 stay valid regardless of the negotiated set.
        let audio = DatagramHeader::new(false, 1, 0, 0, 1).encode(&[]);
        assert!(decode_with_streams(&audio, &[2]).is_ok());
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
