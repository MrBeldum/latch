//! The session's TCP control messages, after the HTTP upgrade. See
//! `docs/PROTOCOL.md`, "Session start" and "Control messages (TCP)".

use serde::{Deserialize, Serialize};
use std::io::{Read, Write};

/// The `Upgrade` token.
pub const UPGRADE: &str = "latch-stream/1";
/// The route the viewer posts its [`StreamRequest`] to.
pub const PATH: &str = "/v1/stream";
/// Largest control message, type byte included.
pub const MAX_MESSAGE: usize = 65_536;

/// A video codec, as named on the wire.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Codec {
    #[serde(rename = "hevc")]
    Hevc,
    #[serde(rename = "h264-high")]
    H264High,
    #[serde(rename = "h264-baseline")]
    H264Baseline,
}

impl Codec {
    pub fn name(self) -> &'static str {
        match self {
            Codec::Hevc => "HEVC",
            Codec::H264High | Codec::H264Baseline => "H.264",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StreamRequest {
    pub protocol: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    /// What the viewer decodes, best first. Unknown names are skipped.
    #[serde(deserialize_with = "known_codecs")]
    pub codecs: Vec<Codec>,
    pub udp_port: u16,
    pub audio: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub cursor: bool,
}

fn known_codecs<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Vec<Codec>, D::Error> {
    let names = Vec::<serde_json::Value>::deserialize(d)?;
    Ok(names
        .into_iter()
        .filter_map(|v| serde_json::from_value(v).ok())
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioFormat {
    pub rate: u32,
    pub channels: u32,
    pub frame_samples: u32,
    pub codec: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Accept {
    pub session: u32,
    /// 32 hex digits.
    pub token: String,
    pub codec: Codec,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub encoder: String,
    pub audio: Option<AudioFormat>,
    #[serde(default)]
    pub audio_problem: String,
    #[serde(default)]
    pub cursor: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct End {
    pub code: u32,
    pub message: String,
}

pub mod end {
    pub const ENDED: u32 = 0;
    pub const TAKEN_OVER: u32 = 1;
    pub const CAPTURE_FAILED: u32 = 2;
    pub const ENCODER_FAILED: u32 = 3;
    pub const VIEWER_SILENT: u32 = 4;
    pub const HOST_STOPPING: u32 = 5;
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Notice {
    /// "audio" or "video".
    pub kind: String,
    /// Empty clears an earlier notice of this kind.
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Report {
    pub fps: f32,
    pub loss_pct: f32,
    pub decode_ms: f32,
    pub rtt_ms: f32,
    #[serde(default)]
    pub received_kbps: u32,
    #[serde(default)]
    pub frames_lost: u64,
    #[serde(default)]
    pub unrecovered_loss_pct: Option<f32>,
    #[serde(default)]
    pub queue_ms: f32,
    #[serde(default)]
    pub window_ms: u32,
    /// Host-to-viewer telemetry, on the existing Report message type.
    #[serde(default)]
    pub applied_bitrate_kbps: u32,
    #[serde(default)]
    pub requested_bitrate_kbps: u32,
    #[serde(default)]
    pub pacing_queue_ms: f32,
    #[serde(default)]
    pub send_errors: u64,
    #[serde(default)]
    pub send_retries: u64,
    #[serde(default)]
    pub socket_send_bytes: usize,
    #[serde(default)]
    pub socket_receive_bytes: usize,
}

/// A pointer shape sent by the host to the viewer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CursorShape {
    pub serial: u32,
    pub width: u32,
    pub height: u32,
    pub hot_x: u32,
    pub hot_y: u32,
    pub rgba: String,
}

impl CursorShape {
    /// Creates a shape, downscaling if larger than 96×96.
    pub fn new(serial: u32, width: u32, height: u32, hot_x: u32, hot_y: u32, rgba: &[u8]) -> Self {
        let (w, h, hx, hy, data) = crate::base64::downscale_rgba(width, height, hot_x, hot_y, rgba);
        Self {
            serial,
            width: w,
            height: h,
            hot_x: hx,
            hot_y: hy,
            rgba: crate::base64::encode(&data),
        }
    }

    /// Decodes the straight-RGBA pixel buffer.
    pub fn decode_rgba(&self) -> Result<Vec<u8>, &'static str> {
        crate::base64::decode(&self.rgba)
    }
}

/// Host → viewer pointer update. Sent when visibility or shape changes;
/// `shape: None` means the shape has not changed since the last update.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Cursor {
    pub visible: bool,
    #[serde(default)]
    pub shape: Option<CursorShape>,
}

/// Viewer → host command to include or omit the cursor from video frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct DrawCursor {
    pub draw: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Accept(Accept),
    End(End),
    Keepalive,
    Notice(Notice),
    Report(Report),
    Cursor(Cursor),
    DrawCursor(DrawCursor),
    /// A type this version does not know; skipped by the reader's caller.
    Unknown(u8),
}

/// Writes one message (`u32 length | u8 type | JSON`).
pub fn write_message(w: &mut impl Write, message: &Message) -> std::io::Result<()> {
    let (kind, payload) = match message {
        Message::Accept(v) => (1, serde_json::to_vec(v)),
        Message::End(v) => (2, serde_json::to_vec(v)),
        Message::Keepalive => (3, Ok(b"{}".to_vec())),
        Message::Notice(v) => (4, serde_json::to_vec(v)),
        Message::Report(v) => (5, serde_json::to_vec(v)),
        Message::Cursor(v) => (6, serde_json::to_vec(v)),
        Message::DrawCursor(v) => (7, serde_json::to_vec(v)),
        Message::Unknown(t) => (*t, Ok(b"{}".to_vec())),
    };
    let payload = payload.map_err(std::io::Error::other)?;
    if payload.len() + 1 > MAX_MESSAGE {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    w.write_all(&((payload.len() + 1) as u32).to_le_bytes())?;
    w.write_all(&[kind])?;
    w.write_all(&payload)
}

/// Reads one message. A length over [`MAX_MESSAGE`] or a payload that is
/// not the type's JSON is `InvalidData`.
pub fn read_message(r: &mut impl Read) -> std::io::Result<Message> {
    let mut len = [0; 4];
    r.read_exact(&mut len)?;
    let len = u32::from_le_bytes(len) as usize;
    if !(1..=MAX_MESSAGE).contains(&len) {
        return Err(std::io::ErrorKind::InvalidData.into());
    }
    let mut bytes = vec![0; len];
    r.read_exact(&mut bytes)?;
    let invalid = |e| std::io::Error::new(std::io::ErrorKind::InvalidData, e);
    Ok(match bytes[0] {
        1 => Message::Accept(serde_json::from_slice(&bytes[1..]).map_err(invalid)?),
        2 => Message::End(serde_json::from_slice(&bytes[1..]).map_err(invalid)?),
        3 => {
            let _: serde_json::Map<String, serde_json::Value> =
                serde_json::from_slice(&bytes[1..]).map_err(invalid)?;
            Message::Keepalive
        }
        4 => Message::Notice(serde_json::from_slice(&bytes[1..]).map_err(invalid)?),
        5 => Message::Report(serde_json::from_slice(&bytes[1..]).map_err(invalid)?),
        6 => Message::Cursor(serde_json::from_slice(&bytes[1..]).map_err(invalid)?),
        7 => Message::DrawCursor(serde_json::from_slice(&bytes[1..]).map_err(invalid)?),
        t => Message::Unknown(t),
    })
}

/// 16 random bytes as 32 lowercase hex digits, and back.
pub fn token_hex(token: &[u8; 16]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut s = String::with_capacity(32);
    for &b in token {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 15) as usize] as char);
    }
    s
}

pub fn token_from_hex(s: &str) -> Option<[u8; 16]> {
    if s.len() != 32 {
        return None;
    }
    let mut out = [0; 16];
    for (i, pair) in s.as_bytes().as_chunks::<2>().0.iter().enumerate() {
        let digit = |c: u8| match c {
            b'0'..=b'9' => Some(c - b'0'),
            b'a'..=b'f' => Some(c - b'a' + 10),
            b'A'..=b'F' => Some(c - b'A' + 10),
            _ => None,
        };
        out[i] = digit(pair[0])? * 16 + digit(pair[1])?;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_request_backward_compatible() {
        let json = r#"{
            "protocol": 1,
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate_kbps": 8000,
            "codecs": ["hevc"],
            "udp_port": 1234,
            "audio": true,
            "name": "old-viewer"
        }"#;
        let req: StreamRequest = serde_json::from_str(json).unwrap();
        assert_eq!(req.name, "old-viewer");
        assert!(!req.cursor);

        let json_with_cursor = r#"{
            "protocol": 1,
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate_kbps": 8000,
            "codecs": ["hevc"],
            "udp_port": 1234,
            "audio": true,
            "name": "new-viewer",
            "cursor": true
        }"#;
        let req_new: StreamRequest = serde_json::from_str(json_with_cursor).unwrap();
        assert!(req_new.cursor);
    }

    #[test]
    fn accept_backward_compatible() {
        let json = r#"{
            "session": 42,
            "token": "0123456789abcdef0123456789abcdef",
            "codec": "hevc",
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate_kbps": 8000,
            "encoder": "nvenc",
            "audio": null
        }"#;
        let acc: Accept = serde_json::from_str(json).unwrap();
        assert!(!acc.cursor);

        let json_with_cursor = r#"{
            "session": 42,
            "token": "0123456789abcdef0123456789abcdef",
            "codec": "hevc",
            "width": 1920,
            "height": 1080,
            "fps": 60,
            "bitrate_kbps": 8000,
            "encoder": "nvenc",
            "audio": null,
            "cursor": true
        }"#;
        let acc_new: Accept = serde_json::from_str(json_with_cursor).unwrap();
        assert!(acc_new.cursor);
    }

    #[test]
    fn cursor_messages_wire_roundtrip() {
        let shape = CursorShape::new(101, 2, 2, 0, 0, &[255; 16]);
        let msg = Message::Cursor(Cursor {
            visible: true,
            shape: Some(shape),
        });

        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();

        let mut read_slice = &buf[..];
        let decoded = read_message(&mut read_slice).unwrap();
        assert_eq!(msg, decoded);

        let msg_noshape = Message::Cursor(Cursor {
            visible: false,
            shape: None,
        });
        let mut buf2 = Vec::new();
        write_message(&mut buf2, &msg_noshape).unwrap();
        let mut read_slice2 = &buf2[..];
        let decoded2 = read_message(&mut read_slice2).unwrap();
        assert_eq!(msg_noshape, decoded2);
    }

    #[test]
    fn draw_cursor_message_wire_roundtrip() {
        let msg = Message::DrawCursor(DrawCursor { draw: false });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        let mut read_slice = &buf[..];
        let decoded = read_message(&mut read_slice).unwrap();
        assert_eq!(msg, decoded);
    }

    #[test]
    fn cursor_max_shape_fits_max_message() {
        let raw_pixels = vec![255u8; 96 * 96 * 4];
        let shape = CursorShape::new(1, 96, 96, 10, 10, &raw_pixels);
        let msg = Message::Cursor(Cursor {
            visible: true,
            shape: Some(shape),
        });
        let mut buf = Vec::new();
        write_message(&mut buf, &msg).unwrap();
        assert!(buf.len() <= MAX_MESSAGE + 4);
    }

    #[test]
    fn cursor_json_format_matches_spec() {
        let shape = CursorShape::new(1, 1, 1, 0, 0, &[1, 2, 3, 4]);
        let cursor = Cursor {
            visible: true,
            shape: Some(shape),
        };
        let s = serde_json::to_string(&cursor).unwrap();
        assert!(s.contains("\"visible\":true"));
        assert!(s.contains("\"shape\":{"));
        assert!(s.contains("\"serial\":1"));

        let cursor_null = Cursor {
            visible: false,
            shape: None,
        };
        let s_null = serde_json::to_string(&cursor_null).unwrap();
        assert_eq!(s_null, r#"{"visible":false,"shape":null}"#);
    }

    #[test]
    fn unknown_message_type_ignored() {
        let mut buf = Vec::new();
        let unknown = Message::Unknown(99);
        write_message(&mut buf, &unknown).unwrap();

        let mut read_slice = &buf[..];
        let decoded = read_message(&mut read_slice).unwrap();
        assert_eq!(decoded, Message::Unknown(99));
    }
}
