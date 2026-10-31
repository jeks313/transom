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
    Config {
        stream: u64,
        width: u32,
        height: u32,
        hvcc: Vec<u8>,
    },
    /// `0x02` — one access unit of `lane`: monotonic `seq`, host-clock
    /// `pts_micros` (when it was composited), a keyframe flag, when it passed
    /// each host stage (v3), and the raw HEVC bytes.
    Frame {
        stream: u64,
        seq: u64,
        pts_micros: u64,
        keyframe: bool,
        timing: FrameTiming,
        /// The window's part of the picture, from its top left (v5); the rest
        /// pads it to the stream's `Config` size.
        width: u32,
        height: u32,
        data: Vec<u8>,
    },
}

/// When a frame passed each host stage, in host-clock microseconds.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FrameTiming {
    /// The capture handed it to the encoder.
    pub captured_micros: u64,
    /// The encoder returned it.
    pub encoded_micros: u64,
    /// The host began writing it to the socket.
    pub sent_micros: u64,
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

/// The header every message starts with: tag(1) + stream(8), the window id.
const STREAM_HEADER_LEN: usize = 1 + 8;
/// The config header: stream header + width(4) + height(4).
const CONFIG_HEADER_LEN: usize = STREAM_HEADER_LEN + 4 + 4;
/// The fixed frame header: stream header + seq(8) + pts(8) + flags(1) +
/// captured(8) + encoded(8) + sent(8) + width(4) + height(4).
const FRAME_HEADER_LEN: usize = STREAM_HEADER_LEN + 8 + 8 + 1 + 24 + 8;

impl VideoMessage {
    /// Decode one de-framed video-channel payload.
    pub fn decode(payload: &[u8]) -> Result<VideoMessage, VideoDecodeError> {
        let &tag = payload.first().ok_or(VideoDecodeError::Empty)?;
        if tag != 0x01 && tag != 0x02 {
            return Err(VideoDecodeError::UnknownTag(tag));
        }
        if payload.len() < STREAM_HEADER_LEN {
            return Err(VideoDecodeError::TruncatedFrame(payload.len()));
        }
        let stream = be_u64(&payload[1..9]);
        match tag {
            0x01 => {
                if payload.len() < CONFIG_HEADER_LEN {
                    return Err(VideoDecodeError::TruncatedFrame(payload.len()));
                }
                Ok(VideoMessage::Config {
                    stream,
                    width: be_u32(&payload[9..13]),
                    height: be_u32(&payload[13..17]),
                    hvcc: payload[CONFIG_HEADER_LEN..].to_vec(),
                })
            }
            _ => {
                if payload.len() < FRAME_HEADER_LEN {
                    return Err(VideoDecodeError::TruncatedFrame(payload.len()));
                }
                let timing = FrameTiming {
                    captured_micros: be_u64(&payload[26..34]),
                    encoded_micros: be_u64(&payload[34..42]),
                    sent_micros: be_u64(&payload[42..50]),
                };
                Ok(VideoMessage::Frame {
                    stream,
                    seq: be_u64(&payload[9..17]),
                    pts_micros: be_u64(&payload[17..25]),
                    keyframe: payload[25] & 0x01 != 0,
                    timing,
                    width: be_u32(&payload[50..54]),
                    height: be_u32(&payload[54..58]),
                    data: payload[FRAME_HEADER_LEN..].to_vec(),
                })
            }
        }
    }
}

fn be_u32(bytes: &[u8]) -> u32 {
    u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
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
    fn host_encode_frame(stream: u64, seq: u64, pts: u64, keyframe: bool, data: &[u8]) -> Vec<u8> {
        let mut out = vec![0x02];
        out.extend_from_slice(&stream.to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&pts.to_be_bytes());
        out.push(if keyframe { 1 } else { 0 });
        for stamp in [pts + 100, pts + 4_000, pts + 4_050] {
            out.extend_from_slice(&stamp.to_be_bytes());
        }
        out.extend_from_slice(&1568u32.to_be_bytes());
        out.extend_from_slice(&1420u32.to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn decodes_config() {
        let mut payload = vec![0x01];
        payload.extend_from_slice(&7u64.to_be_bytes());
        payload.extend_from_slice(&1680u32.to_be_bytes());
        payload.extend_from_slice(&1420u32.to_be_bytes());
        payload.extend_from_slice(&[0xDE, 0xAD, 0xBE, 0xEF]);
        assert_eq!(
            VideoMessage::decode(&payload).unwrap(),
            VideoMessage::Config {
                stream: 7,
                width: 1680,
                height: 1420,
                hvcc: vec![0xDE, 0xAD, 0xBE, 0xEF]
            }
        );
    }

    #[test]
    fn decodes_keyframe() {
        let payload = host_encode_frame(0x0102_0304_0506_0708, 1, 16_000, true, &[1, 2, 3]);
        assert_eq!(
            VideoMessage::decode(&payload).unwrap(),
            VideoMessage::Frame {
                stream: 0x0102_0304_0506_0708,
                seq: 1,
                pts_micros: 16_000,
                keyframe: true,
                timing: FrameTiming {
                    captured_micros: 16_100,
                    encoded_micros: 20_000,
                    sent_micros: 20_050,
                },
                width: 1568,
                height: 1420,
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
    fn rejects_short_messages() {
        // Tag + stream only: well under the 58-byte frame header.
        assert_eq!(
            VideoMessage::decode(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0]),
            Err(VideoDecodeError::TruncatedFrame(9))
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
