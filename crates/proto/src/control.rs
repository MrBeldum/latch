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
}

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Accept(Accept),
    End(End),
    Keepalive,
    Notice(Notice),
    Report(Report),
    /// A type this version does not know; skipped by the reader's caller.
    Unknown(u8),
}

/// Writes one message (`u32 length | u8 type | JSON`).
pub fn write_message(w: &mut impl Write, message: &Message) -> std::io::Result<()> {
    let _ = (w, message);
    todo!()
}

/// Reads one message. A length over [`MAX_MESSAGE`] or a payload that is
/// not the type's JSON is `InvalidData`.
pub fn read_message(r: &mut impl Read) -> std::io::Result<Message> {
    let _ = r;
    todo!()
}

/// 16 random bytes as 32 lowercase hex digits, and back.
pub fn token_hex(token: &[u8; 16]) -> String {
    let _ = token;
    todo!()
}

pub fn token_from_hex(s: &str) -> Option<[u8; 16]> {
    let _ = s;
    todo!()
}
