//! The JSON the host serves and the client reads. Plain HTTP over Tailscale:
//! WireGuard already encrypts it, and `tailscale whois` already says who is
//! asking, so Latch adds neither a cipher nor a password of its own.
//!
//! Routes (all JSON, all on [`crate::CONTROL_PORT`]):
//!
//! | Method | Path        | Body            | Reply        |
//! |--------|-------------|-----------------|--------------|
//! | GET    | /v1/status  |                 | [`Status`]   |
//! | POST   | /v1/stream  | stream request | TCP upgrade |
//! | POST   | /v1/power   | [`PowerRequest`]| [`Ack`]      |
//! | POST   | /v1/quit    |                 | [`Ack`] (loopback only) |
//! | POST   | /v1/update  | `latch-host.exe` bytes | [`Ack`]     |
//! | GET    | /v1/clipboard |               | [`Clipboard`] |
//! | POST   | /v1/clipboard | [`Clipboard`] | [`Ack`]      |
//! | GET    | /v1/display |                 | display report (JSON) |
//! | POST   | /v1/display | [`DisplayRequest`] | display report (JSON) |
//!
//! Only machines signed in to the same Tailscale account get an answer
//! (a tagged server answers its tailnet's members); see the host's
//! `service` module.
//!
//! `/v1/update` carries the new executable itself, with its version in
//! [`UPDATE_VERSION_HEADER`] and its SHA-256 in [`UPDATE_SHA256_HEADER`]. A
//! tailnet peer that may sleep the PC may also update it; the host verifies
//! the digest, swaps the file in and restarts. See [`crate::update`].
//!
//! `/v1/clipboard` is the PC's clipboard as text (3.1+). The Mac reads it
//! while streaming so ⌘C on the PC lands in the Mac's clipboard, and writes
//! it before a ⌘V so the Mac's text is what the PC pastes.

use serde::{Deserialize, Serialize};

pub const UPDATE_PATH: &str = "/v1/update";
pub const UPDATE_VERSION_HEADER: &str = "x-latch-version";
pub const UPDATE_SHA256_HEADER: &str = "x-latch-sha256";
/// The largest host executable accepted; the real one is a tenth of this.
pub const UPDATE_MAX_BYTES: usize = 128 * 1024 * 1024;
/// Oldest Latch Host that serves [`UPDATE_PATH`]. Older hosts cap the
/// body at 64 KiB and close; a Mac that POSTs the executable anyway sees
/// a broken pipe (and on macOS, a socket timeout as EAGAIN).
pub const FIRST_UPDATE_VERSION: &str = "3.1.0";

pub const CLIPBOARD_PATH: &str = "/v1/clipboard";
/// Clipboard text longer than this is cut: nobody pastes a novel through a
/// stream.
pub const CLIPBOARD_MAX_BYTES: usize = 32 * 1024;
/// ... and so is text that would be longer than this once JSON-escaped, so
/// it fits the 64 KiB body either side accepts with room for the other
/// fields. A quote or a newline takes two bytes there and other control
/// characters six, so 32 KiB of them alone would not fit.
pub const CLIPBOARD_MAX_JSON: usize = 60 * 1024;

/// The PC's clipboard, as far as it is text.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct Clipboard {
    pub text: String,
    /// Windows' clipboard sequence number: changes on every copy, so a
    /// reader can tell "new" from "same" without comparing text.
    pub seq: u64,
    /// The text was cut to [`CLIPBOARD_MAX_BYTES`].
    pub truncated: bool,
    /// A write also sets the X selection (PRIMARY), so Shift+Insert pastes
    /// it in a terminal. Linux hosts newer than 4.1.1; false from any other.
    pub selection: bool,
}

impl Clipboard {
    /// `text` cut on a character boundary to [`CLIPBOARD_MAX_BYTES`], and
    /// to [`CLIPBOARD_MAX_JSON`] once escaped.
    pub fn fit(text: &str, seq: u64) -> Self {
        let (mut raw, mut escaped, mut end) = (0, 0, text.len());
        for (i, c) in text.char_indices() {
            let (r, e) = (c.len_utf8(), json_len(c));
            if raw + r > CLIPBOARD_MAX_BYTES || escaped + e > CLIPBOARD_MAX_JSON {
                end = i;
                break;
            }
            raw += r;
            escaped += e;
        }
        Self {
            text: text[..end].to_string(),
            seq,
            truncated: end < text.len(),
            selection: false,
        }
    }
}

/// How many bytes `c` takes inside a JSON string as serde_json writes it.
fn json_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\u{08}' | '\u{0C}' | '\n' | '\r' | '\t' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// What `tailscale netcheck` says about one machine's network, in the
/// terms that decide whether two machines can connect directly. Both apps
/// report their own; the Mac combines the two to explain a relayed path.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct NatReport {
    /// UDP gets out at all. Without it every packet goes through a relay.
    pub udp: bool,
    pub ipv4: bool,
    pub ipv6: bool,
    /// A "hard" NAT maps each destination to a different port; two hard NATs
    /// cannot reach each other without a port mapping. `None` when unknown.
    pub hard: Option<bool>,
    /// The router will map a port on request (UPnP, NAT-PMP or PCP), which
    /// makes a hard NAT reachable.
    pub portmap: bool,
    /// Nearest DERP region code ("tok"); see `tailscale::derp_city`.
    pub derp: String,
}

/// Everything a peer needs to know about this machine, and everything the
/// host's own control panel shows.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Status {
    pub app: String,
    pub version: String,
    /// This machine's name as the OS knows it.
    pub name: String,
    /// Tailscale's OS string: "windows", "macOS", "linux". Empty on hosts
    /// older than 4.0.
    pub os: String,
    pub tailscale_ip: Option<String>,
    /// Who the PC is signed in to Tailscale as. Only this account may talk
    /// to the host from the tailnet.
    pub tailscale_login: Option<String>,
    /// LAN address and MAC of the adapter a wake packet must reach.
    pub lan_ip: Option<String>,
    pub mac: Option<String>,
    /// `Some(true)` once the adapter wakes on a magic packet and Windows lets
    /// it wake the PC. `None` while unknown.
    pub wake_ready: Option<bool>,
    /// The adapter's name and driver description, which the setup step
    /// needs to arm it.
    pub wake_adapter: String,
    pub wake_adapter_description: String,
    /// Seconds since a magic packet for this PC's MAC last arrived, so a Mac
    /// can check its wake path while the PC is awake.
    pub wake_packet_age_secs: Option<u64>,
    /// Windows Fast Startup turns shutdown into a hibernate the network card
    /// cannot wake from. `None` while unknown.
    pub fast_startup: Option<bool>,
    pub streamer: Streamer,
    /// Whether the owner lets a Mac sleep, restart, or shut this PC down.
    pub power_allowed: bool,
    /// This PC's side of the NAT story (3.1+); absent from older hosts.
    pub nat: Option<NatReport>,
    /// What the host thinks still needs doing, for the setup card.
    pub setup: Vec<String>,
    /// Recent log lines, newest last.
    pub log: Vec<String>,
}

/// The streaming engine Latch runs on this machine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct Streamer {
    /// "Latch" when sharing is set up; empty when no engine is installed.
    pub kind: String,
    pub installed: bool,
    /// The media server is bound.
    pub running: bool,
    /// The built-in streaming control API is ready.
    pub api_ok: bool,
    /// The running or last session's encoder, or the platform's expected encoder.
    pub encoder: String,
    /// Why the running or last session had no audio; empty when capture works.
    pub audio_problem: String,
    /// The desktop the engine captures, `[width, height]`, when its size is
    /// fixed whatever a viewer asks for: a Linux X screen. Absent when the
    /// host matches the viewer's size itself (Windows' virtual display), and
    /// from 4.1.1 and older hosts. A viewer asks for no more than this size:
    /// a larger one is the same picture scaled up, blurrier and dearer to
    /// encode; a smaller one is scaled down and cheaper.
    pub capture_size: Option<[u32; 2]>,
    /// The highest frame rate the engine keeps up with on this machine at
    /// `capture_size`, or at 1920 × 1080 when that is absent; 0 when
    /// nothing limits it. Set for software encoding on a small CPU, where
    /// asking for more only drops frames. A viewer asking for fewer pixels
    /// raises it in proportion.
    pub max_fps: u32,
    /// A viewer is connected right now. A Mac
    /// holds a host update back while it is true, instead of sending the
    /// whole executable to be turned down. False from older hosts.
    pub viewer: bool,
    /// The Latch stream protocol this host speaks (`docs/PROTOCOL.md`); 0
    /// from a Latch 4 host; that host needs an update.
    #[serde(default)]
    pub protocol: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PowerAction {
    Sleep,
    Restart,
    Shutdown,
}

impl PowerAction {
    pub fn label(self) -> &'static str {
        match self {
            PowerAction::Sleep => "Sleep",
            PowerAction::Restart => "Restart",
            PowerAction::Shutdown => "Shut down",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowerRequest {
    pub action: PowerAction,
}

/// Asking a PC to turn its HDR desktop off, or back on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DisplayRequest {
    pub advanced_color: bool,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Ack {
    pub ok: bool,
    pub error: Option<String>,
}

impl Ack {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
        }
    }
    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(msg.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_round_trips_and_tolerates_missing_fields() {
        let s: Status = serde_json::from_str(r#"{"name":"PC","mac":"aa:bb:cc:dd:ee:ff"}"#).unwrap();
        assert_eq!(s.name, "PC");
        assert_eq!(s.mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert!(!s.power_allowed);
        let back: Status = serde_json::from_str(&serde_json::to_string(&s).unwrap()).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn an_older_host_reports_no_capture_size_or_frame_rate_limit() {
        let s: Status =
            serde_json::from_str(r#"{"streamer":{"running":true,"encoder":"software"}}"#).unwrap();
        assert_eq!(s.streamer.capture_size, None);
        assert_eq!(s.streamer.max_fps, 0);
        let mut s = s;
        s.streamer.capture_size = Some([1920, 1080]);
        s.streamer.max_fps = 30;
        let json = serde_json::to_string(&s).unwrap();
        assert!(json.contains(r#""capture_size":[1920,1080]"#), "{json}");
        let back: Status = serde_json::from_str(&json).unwrap();
        assert_eq!(back, s);
    }

    #[test]
    fn power_actions_are_lowercase_on_the_wire() {
        let r = PowerRequest {
            action: PowerAction::Shutdown,
        };
        assert_eq!(
            serde_json::to_string(&r).unwrap(),
            r#"{"action":"shutdown"}"#
        );
        let r: PowerRequest = serde_json::from_str(r#"{"action":"sleep"}"#).unwrap();
        assert_eq!(r.action, PowerAction::Sleep);
        assert!(serde_json::from_str::<PowerRequest>(r#"{"action":"Sleep"}"#).is_err());
    }

    #[test]
    fn an_older_hosts_clipboard_does_not_claim_the_selection() {
        let c: Clipboard = serde_json::from_str(r#"{"text":"x","seq":1}"#).unwrap();
        assert!(!c.selection);
        assert!(!Clipboard::fit("x", 1).selection);
    }

    #[test]
    fn clipboard_is_cut_on_a_character_boundary() {
        let c = Clipboard::fit("héllo", 3);
        assert_eq!(c.text, "héllo");
        assert!(!c.truncated);
        assert_eq!(c.seq, 3);
        let long = "é".repeat(CLIPBOARD_MAX_BYTES); // 2 bytes each
        let c = Clipboard::fit(&long, 1);
        assert!(c.truncated);
        assert!(c.text.len() <= CLIPBOARD_MAX_BYTES);
        assert!(c.text.chars().all(|ch| ch == 'é'));
        // Fits a request body once serialised.
        assert!(serde_json::to_string(&c).unwrap().len() < 64 * 1024);
        // Old hosts have neither `nat` nor `encoder`.
        let s: Status = serde_json::from_str(r#"{"streamer":{"kind":"Latch"}}"#).unwrap();
        assert_eq!(s.nat, None);
        assert_eq!(s.streamer.encoder, "");
        assert_eq!(s.streamer.audio_problem, "");
    }

    #[test]
    fn clipboard_text_fits_a_request_body_however_it_escapes() {
        let body = |c: &Clipboard| serde_json::to_string(c).unwrap().len();
        for s in [
            "\u{1b}".repeat(CLIPBOARD_MAX_BYTES),
            "\"".repeat(CLIPBOARD_MAX_BYTES),
            "\n".repeat(CLIPBOARD_MAX_BYTES),
            "a\u{1}\"é\\".repeat(20_000),
        ] {
            let c = Clipboard::fit(&s, u64::MAX);
            assert!(c.truncated);
            assert!(s.starts_with(&c.text));
            assert!(body(&c) <= 64 * 1024, "{} bytes", body(&c));
        }
        // Plain text still runs to the byte limit.
        let plain = "x".repeat(CLIPBOARD_MAX_BYTES + 10);
        let c = Clipboard::fit(&plain, 1);
        assert_eq!(c.text.len(), CLIPBOARD_MAX_BYTES);
        assert!(c.truncated);
        let exact = "x".repeat(CLIPBOARD_MAX_BYTES);
        assert!(!Clipboard::fit(&exact, 1).truncated);
    }

    #[test]
    fn the_escaped_length_matches_serde_json() {
        let mut chars: Vec<char> = (0u8..128).map(char::from).collect();
        chars.extend(['é', '€', '한', '😀', '\u{7f}', '\u{2028}']);
        for c in chars {
            let s = c.to_string();
            assert_eq!(
                json_len(c),
                serde_json::to_string(&s).unwrap().len() - 2,
                "{c:?}"
            );
        }
    }

    #[test]
    fn first_update_version_is_3_1() {
        assert_eq!(FIRST_UPDATE_VERSION, "3.1.0");
        assert!(semver::Version::parse(FIRST_UPDATE_VERSION).is_ok());
    }
}
