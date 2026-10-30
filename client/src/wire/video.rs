//! The video channel payload format (protocol.md §6). Binary, length-prefixed
//! like the control channel, but the payload is not JSON: a leading type byte
//! distinguishes the HEVC parameter sets from an access unit, and the lane the
//! message belongs to follows it (v2: every lane shares one connection).
//!
//! The mirror of the host's `VideoWire`. Pure and byte-oriented, so it is
//! unit-tested without a decoder or a socket.

/// One decoded video-channel message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoMessage {
    /// `0x01` — the lane's HEVC `hvcC` configuration record (VPS/SPS/PPS).
    /// Sent before the lane's first frame and again after a reconnect; an `hvc1`
    /// stream carries no inline parameter sets, so the decoder needs this first.
    Config { lane: u32, hvcc: Vec<u8> },
    /// `0x02` — one access unit of `lane`: monotonic `seq`, host-clock
    /// `pts_micros`, a keyframe flag, and the raw HEVC bytes.
    Frame {
        lane: u32,
        seq: u64,
        pts_micros: u64,
        keyframe: bool,
        data: Vec<u8>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoDecodeError {
    Empty,
    UnknownTag(u8),
    /// A payload was shorter than its fixed header.
    TruncatedFrame(usize),
}

impl std::fmt::Display for VideoDecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoDecodeError::Empty => write!(f, "empty video payload"),
            VideoDecodeError::UnknownTag(t) => write!(f, "unknown video tag 0x{t:02x}"),
            VideoDecodeError::TruncatedFrame(n) => {
                write!(f, "truncated video payload ({n} bytes)")
            }
        }
    }
}

impl std::error::Error for VideoDecodeError {}

/// The header every message starts with: tag(1) + lane(4).
const LANE_HEADER_LEN: usize = 1 + 4;
/// The fixed frame header: tag(1) + lane(4) + seq(8) + pts(8) + flags(1).
const FRAME_HEADER_LEN: usize = LANE_HEADER_LEN + 8 + 8 + 1;

impl VideoMessage {
    /// Decode one de-framed video-channel payload.
    pub fn decode(payload: &[u8]) -> Result<VideoMessage, VideoDecodeError> {
        let &tag = payload.first().ok_or(VideoDecodeError::Empty)?;
        if tag != 0x01 && tag != 0x02 {
            return Err(VideoDecodeError::UnknownTag(tag));
        }
        if payload.len() < LANE_HEADER_LEN {
            return Err(VideoDecodeError::TruncatedFrame(payload.len()));
        }
        let lane = u32::from_be_bytes([payload[1], payload[2], payload[3], payload[4]]);
        match tag {
            0x01 => Ok(VideoMessage::Config {
                lane,
                hvcc: payload[LANE_HEADER_LEN..].to_vec(),
            }),
            _ => {
                if payload.len() < FRAME_HEADER_LEN {
                    return Err(VideoDecodeError::TruncatedFrame(payload.len()));
                }
                let seq = be_u64(&payload[5..13]);
                let pts_micros = be_u64(&payload[13..21]);
                let keyframe = payload[21] & 0x01 != 0;
                Ok(VideoMessage::Frame {
                    lane,
                    seq,
                    pts_micros,
                    keyframe,
                    data: payload[FRAME_HEADER_LEN..].to_vec(),
                })
            }
        }
    }
}

fn be_u64(bytes: &[u8]) -> u64 {
    let mut arr = [0u8; 8];
    arr.copy_from_slice(&bytes[..8]);
    u64::from_be_bytes(arr)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduce the host's `VideoWire.encodeFrame` byte layout so the test pins
    /// the wire format, not just this module's own round-trip.
    fn host_encode_frame(lane: u32, seq: u64, pts: u64, keyframe: bool, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x02];
        out.extend_from_slice(&lane.to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&pts.to_be_bytes());
        out.push(if keyframe { 1 } else { 0 });
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn decodes_config() {
        let mut payload = vec![0x01, 0, 0, 0, 7];
        payload.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            VideoMessage::decode(&payload).unwrap(),
            VideoMessage::Config {
                lane: 7,
                hvcc: vec![0xDE, 0xAD, 0xBE, 0xEF]
            }
        );
    }

    #[test]
    fn decodes_keyframe() {
        let payload = host_encode_frame(0x0102_0304, 1, 16_000, true, &[1, 2, 3]);
        assert_eq!(
            VideoMessage::decode(&payload).unwrap(),
            VideoMessage::Frame {
                lane: 0x0102_0304,
                seq: 1,
                pts_micros: 16_000,
                keyframe: true,
                data: vec![1, 2, 3],
            }
        );
    }

    #[test]
    fn decodes_non_keyframe() {
        let payload = host_encode_frame(0, 9_000_000_000, 33_000, false, &[]);
        match VideoMessage::decode(&payload).unwrap() {
            VideoMessage::Frame {
                seq,
                pts_micros,
                keyframe,
                data,
                ..
            } => {
                assert_eq!(seq, 9_000_000_000);
                assert_eq!(pts_micros, 33_000);
                assert!(!keyframe);
                assert!(data.is_empty());
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn rejects_short_frame() {
        // Tag + lane only: well under the 22-byte frame header.
        assert_eq!(
            VideoMessage::decode(&[0x02, 0, 0, 0, 0]),
            Err(VideoDecodeError::TruncatedFrame(5))
        );
        assert_eq!(
            VideoMessage::decode(&[0x01, 0, 0]),
            Err(VideoDecodeError::TruncatedFrame(3))
        );
    }

    #[test]
    fn rejects_unknown_tag_and_empty() {
        assert_eq!(
            VideoMessage::decode(&[0x09]),
            Err(VideoDecodeError::UnknownTag(0x09))
        );
        assert_eq!(VideoMessage::decode(&[]), Err(VideoDecodeError::Empty));
    }
}
