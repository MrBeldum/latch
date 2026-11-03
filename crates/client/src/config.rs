//! Settings and what has been learned about each PC, in
//! `~/Library/Application Support/io.github.MrBeldum.Latch/client.toml`.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub const FILE: &str = "client.toml";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    P540,
    P720,
    P900,
    P1080,
    P1440,
    P2160,
    P2880,
    P4320,
    /// This screen's own pixel size.
    Native,
    /// This screen's own size, scaled down in proportion to fit 4K: what
    /// Cinematic asks for. Not a choice of its own in the list.
    NativeUpTo4K,
}

impl Resolution {
    /// Every choice, in the order the list shows them.
    pub const ALL: [Resolution; 9] = [
        Resolution::Native,
        Resolution::P540,
        Resolution::P720,
        Resolution::P900,
        Resolution::P1080,
        Resolution::P1440,
        Resolution::P2160,
        Resolution::P2880,
        Resolution::P4320,
    ];

    /// This choice applied to a screen of `native` pixels: the standard
    /// 16:9 sizes are exactly that, and "Match screen" is the screen
    /// itself. See `latch_core::screens`.
    pub fn pixels(self, native: (u32, u32)) -> (u32, u32) {
        let [p540, p720, p900, p1080, p1440, p2160, p2880, p4320] = latch_core::screens::STANDARD;
        match self {
            Resolution::P540 => p540,
            Resolution::P720 => p720,
            Resolution::P900 => p900,
            Resolution::P1080 => p1080,
            Resolution::P1440 => p1440,
            Resolution::P2160 => p2160,
            Resolution::P2880 => p2880,
            Resolution::P4320 => p4320,
            Resolution::Native => latch_core::screens::even(native),
            Resolution::NativeUpTo4K => latch_core::screens::within(native, p2160),
        }
    }

    /// "1080p · 1920 × 1080", or "Match screen · 3024 × 1964".
    pub fn describe(self, native: (u32, u32)) -> String {
        let (w, h) = self.pixels(native);
        format!("{} · {w} × {h}", self.label())
    }

    pub fn label(self) -> &'static str {
        match self {
            Resolution::P540 => "540p",
            Resolution::P720 => "720p",
            Resolution::P900 => "900p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::P2160 => "4K",
            Resolution::P2880 => "5K",
            Resolution::P4320 => "8K",
            Resolution::Native => "Match screen",
            Resolution::NativeUpTo4K => "Match screen up to 4K",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// The best codec both ends have: AV1, then HEVC, then H.264.
    Auto,
    Av1,
    Hevc,
    H264,
}

impl Codec {
    /// The codecs better than H.264 a stream may ask for: (HEVC, AV1).
    /// H.264 is always offered too, as the one every host encodes.
    pub fn allows(self) -> (bool, bool) {
        match self {
            Codec::Auto => (true, true),
            Codec::Av1 => (false, true),
            Codec::Hevc => (true, false),
            Codec::H264 => (false, false),
        }
    }
}

/// Who decides resolution, frame rate and bitrate. Auto and the two named
/// profiles follow this display (its size and refresh rate), capped only
/// by what the PC's encoder takes; Custom is the values as set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Quality {
    /// Balanced: this display's own size at its own refresh rate.
    #[default]
    Auto,
    /// 1080p at the display's refresh rate, up to 360: frames and latency
    /// first.
    Competitive,
    /// This display's size up to 4K, up to 120 fps, at half again the
    /// bitrate.
    Cinematic,
    /// The values in [`StreamSettings`], as set.
    Custom,
}

impl Quality {
    /// The choices, in the order the list shows them.
    pub const ALL: [Quality; 4] = [
        Quality::Competitive,
        Quality::Auto,
        Quality::Cinematic,
        Quality::Custom,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Competitive => "Competitive",
            Quality::Auto => "Balanced",
            Quality::Cinematic => "Cinematic",
            Quality::Custom => "Custom",
        }
    }

    /// Resolution and frame rate on a display refreshing at `hz`; None
    /// for Custom.
    pub fn values(self, hz: u32) -> Option<(Resolution, u32)> {
        let hz = hz.clamp(MIN_FPS, latch_proto::control::MAX_FPS);
        match self {
            Quality::Auto => Some((Resolution::Native, hz)),
            Quality::Competitive => Some((Resolution::P1080, hz.min(360))),
            Quality::Cinematic => Some((Resolution::NativeUpTo4K, hz.min(120))),
            Quality::Custom => None,
        }
    }

    /// The bitrate ceiling for `width` × `height` at `fps`: 40 Mbps for
    /// 1080p at 60, growing as pixels to the ¾ and frame rate to the ½
    /// (a larger or faster picture differs less from one frame to the
    /// next, so needs fewer bits a pixel), half again for Cinematic. Never
    /// under what 5.1 asked for (50 Mbps; Cinematic 100, 5.1's Sharp).
    ///
    /// GeForce NOW streams about 15 Mbps at 720p60, 25–35 at 1080p60,
    /// 45–50 at 1440p120 and 75–100 at 4K. Balanced asks for 50, 50, 87
    /// and 113–160: one to two times that, because Latch's encoders run
    /// with a one-frame buffer, no B-frames and no look-ahead (the lowest
    /// latency, not the fewest bits), may be H.264 where GeForce NOW uses
    /// AV1 or HEVC, and mostly cross a LAN or a tailnet, not the internet.
    ///
    /// A ceiling, not a target: the encoder spends less on a still
    /// picture, and the host's rate control comes down from it as soon as
    /// the network does not carry it. That needs the host's lowest rate
    /// to be fixed, not a share of this ceiling (a tenth of 160 Mbps is
    /// more than many paths carry). None for Custom.
    pub fn bitrate_kbps(self, width: u32, height: u32, fps: u32) -> Option<u32> {
        let (scale, floor) = match self {
            Quality::Auto | Quality::Competitive => (1.0, AUTO_FLOOR_KBPS),
            Quality::Cinematic => (1.5, CINEMATIC_FLOOR_KBPS),
            Quality::Custom => return None,
        };
        let pixels = f64::from(width) * f64::from(height) / (1920.0 * 1080.0);
        let rate = f64::from(fps) / 60.0;
        let kbps = f64::from(BASE_KBPS) * scale * pixels.powf(0.75) * rate.sqrt();
        Some((kbps as u32).clamp(floor, latch_proto::control::MAX_BITRATE_KBPS))
    }
}

/// Balanced's ceiling for 1080p at 60 fps.
const BASE_KBPS: u32 = 40_000;
/// What 5.1's Sharp asked for, and the least Cinematic asks for now.
const CINEMATIC_FLOOR_KBPS: u32 = 100_000;

/// The fewest frames a second a stream asks for.
pub const MIN_FPS: u32 = 30;
/// The bitrate slider's ends, in kilobits a second.
pub const MIN_BITRATE_KBPS: u32 = 2_000;
/// What Auto asked for before 5.2, and the least it asks for now.
const AUTO_FLOOR_KBPS: u32 = 50_000;
/// [`StreamSettings::lighter`]'s values.
const LIGHTER: (Resolution, u32, u32) = (Resolution::P1080, 60, 20_000);
/// `StreamSettings::version` from 5.2: the 5.1 profiles have been moved
/// to their successors.
const SETTINGS_VERSION: u32 = 2;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub quality: Quality,
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    pub fullscreen: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_resolution: Option<Resolution>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_fps: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub custom_bitrate_kbps: Option<u32>,
    /// 0 in a file from 5.1 or older, whose Custom values may be one of
    /// that release's profiles; see [`StreamSettings::migrate`].
    #[serde(default)]
    pub version: u32,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            quality: Quality::Auto,
            resolution: Resolution::Native,
            fps: 60,
            bitrate_kbps: AUTO_FLOOR_KBPS,
            codec: Codec::Auto,
            fullscreen: true,
            custom_resolution: None,
            custom_fps: None,
            custom_bitrate_kbps: None,
            version: SETTINGS_VERSION,
        }
    }
}

impl StreamSettings {
    /// Switch to a profile. Values dialled in by hand are kept for when
    /// Custom is chosen again.
    pub fn choose(&mut self, q: Quality) {
        if self.quality == Quality::Custom {
            self.custom_resolution = Some(self.resolution);
            self.custom_fps = Some(self.fps);
            self.custom_bitrate_kbps = Some(self.bitrate_kbps);
        }
        if q == Quality::Custom && self.quality != Quality::Custom {
            self.resolution = self.custom_resolution.unwrap_or(Resolution::Native);
            self.fps = self.custom_fps.unwrap_or(self.fps);
            self.bitrate_kbps = self.custom_bitrate_kbps.unwrap_or(self.bitrate_kbps);
        }
        if let Some((r, _)) = q.values(self.fps) {
            self.resolution = r;
        }
        self.quality = q;
    }

    /// What to try when the network ended the last stream: Custom at
    /// 1080p, 60 fps and 20 Mbps.
    pub fn lighter(&mut self) {
        self.choose(Quality::Custom);
        (self.resolution, self.fps, self.bitrate_kbps) = LIGHTER;
    }

    pub fn is_lighter(&self) -> bool {
        self.quality == Quality::Custom && (self.resolution, self.fps, self.bitrate_kbps) == LIGHTER
    }

    /// 5.1's three profiles were Custom values: Smooth (1080p, 60 fps,
    /// 20 Mbps), Balanced (this screen, 60, 50) and Sharp (this screen, 60,
    /// 100). Each becomes the profile that took its place, which follows
    /// the display: Competitive, Balanced and Cinematic. Values set by hand
    /// stay as they are.
    fn migrate(&mut self) {
        if self.version >= SETTINGS_VERSION {
            return;
        }
        self.version = SETTINGS_VERSION;
        if self.quality != Quality::Custom {
            return;
        }
        let profile = match (self.resolution, self.fps, self.bitrate_kbps) {
            (Resolution::P1080, 60, 20_000) => Quality::Competitive,
            (Resolution::Native, 60, 50_000) => Quality::Auto,
            (Resolution::Native, 60, 100_000) => Quality::Cinematic,
            _ => return,
        };
        self.choose(profile);
    }

    /// "1080p · 60 fps · 20 Mbps", or the profile's name when the values
    /// follow the display.
    pub fn describe(&self) -> String {
        match self.quality {
            Quality::Custom => format!(
                "{} · {} fps · {} Mbps",
                self.resolution.label(),
                self.fps,
                self.bitrate_kbps / 1000
            ),
            q => q.label().to_string(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct KnownPc {
    pub name: String,
    pub mac: Option<String>,
    pub lan_ip: Option<String>,
    /// The PC's public address as Tailscale last saw it, for a router that
    /// forwards wake packets.
    pub public_ip: Option<String>,
    /// Tailscale OS string, so a remembered machine still shows Windows /
    /// macOS / Linux when Tailscale is down.
    #[serde(default)]
    pub os: String,
    /// The PC's Tailscale address, so it can still be listed and reached
    /// when this Mac's Tailscale cannot say.
    pub tailscale_ip: Option<String>,
    /// When the PC was last seen online (Unix seconds).
    pub last_seen_unix: Option<u64>,
    /// The tailnet it was seen on, so a machine is only ever forgotten for
    /// leaving the tailnet this machine is on now.
    pub tailnet: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ClientConfig {
    pub stream: StreamSettings,
    /// Offer to put the PC to sleep when a session ends. Off by default:
    /// asleep, Tailscale is asleep too, and the Mac can only wake the PC
    /// from that PC's own network.
    pub sleep_prompt: bool,
    /// The Mac's Command key acts as Ctrl on the PC (else as the Windows key).
    pub cmd_is_ctrl: bool,
    /// A click on the picture captures the mouse: the cursor is hidden and
    /// raw movement goes to the PC, which is what games read. Off, the Mac
    /// cursor's position is sent instead and nothing is captured.
    pub capture_mouse: bool,
    /// Install new releases of this app and send them to the PCs.
    pub auto_update: bool,
    /// A GitHub token for the release downloads, when git has none stored.
    pub github_token: Option<String>,
    /// Keyed by Tailscale node id.
    pub pcs: BTreeMap<String, KnownPc>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            stream: StreamSettings::default(),
            sleep_prompt: false,
            cmd_is_ctrl: true,
            capture_mouse: true,
            auto_update: true,
            github_token: None,
            pcs: BTreeMap::new(),
        }
    }
}

impl ClientConfig {
    pub fn load() -> Self {
        let mut c: Self = latch_core::config::load(FILE);
        c.normalise();
        c
    }

    /// Bring an older or hand-edited file to what the UI offers: 5.1's
    /// profiles to theirs, and values within the frame rates and the
    /// bitrate slider's range (30 to 500 fps, 2 to 1000 Mbps).
    fn normalise(&mut self) {
        self.stream.migrate();
        if let Some((r, _)) = self.stream.quality.values(self.stream.fps) {
            self.stream.resolution = r;
        }
        self.stream.fps = self
            .stream
            .fps
            .clamp(MIN_FPS, latch_proto::control::MAX_FPS);
        self.stream.bitrate_kbps = self
            .stream
            .bitrate_kbps
            .clamp(MIN_BITRATE_KBPS, latch_proto::control::MAX_BITRATE_KBPS);
    }

    /// The window owns preferences; discovery owns `pcs`. The
    /// window's copy of `pcs` is refreshed from the file at the same time, so
    /// a PC seen during the run is known to the window too. Non-UI fields on
    /// disk (such as `github_token`) are also refreshed.
    pub fn save(&mut self) -> anyhow::Result<()> {
        let mut pcs = None;
        let mut github_token = None;
        Self::update(|current| {
            current.apply_preferences(self);
            pcs = Some(current.pcs.clone());
            github_token = current.github_token.clone();
        })?;
        if let Some(pcs) = pcs {
            self.pcs = pcs;
        }
        self.github_token = github_token;
        Ok(())
    }

    pub fn save_changes(&self, before: &Self) -> anyhow::Result<()> {
        Self::update(|disk| disk.apply_changes(before, self))
    }

    fn apply_changes(&mut self, before: &Self, after: &Self) {
        macro_rules! changed {
            ($($field:ident).+) => {
                if after.$($field).+ != before.$($field).+ {
                    self.$($field).+ = after.$($field).+;
                }
            };
        }
        changed!(stream.quality);
        changed!(stream.resolution);
        changed!(stream.fps);
        changed!(stream.bitrate_kbps);
        changed!(stream.codec);
        changed!(stream.fullscreen);
        changed!(stream.custom_resolution);
        changed!(stream.custom_fps);
        changed!(stream.custom_bitrate_kbps);
        changed!(stream.version);
        changed!(sleep_prompt);
        changed!(cmd_is_ctrl);
        changed!(capture_mouse);
        changed!(auto_update);
    }

    fn apply_preferences(&mut self, settings: &Self) {
        self.stream = settings.stream.clone();
        self.sleep_prompt = settings.sleep_prompt;
        self.cmd_is_ctrl = settings.cmd_is_ctrl;
        self.capture_mouse = settings.capture_mouse;
        self.auto_update = settings.auto_update;
    }

    /// One read-modify-write of the file, under the process-wide config lock.
    pub fn update(edit: impl FnOnce(&mut Self)) -> anyhow::Result<()> {
        latch_core::config::update(FILE, edit)
    }

    /// Forget a machine: its addresses and how to wake it.
    /// One still on the tailnet is listed again, unwakeable
    /// until it has been seen online.
    pub fn forget(&mut self, node_id: &str) -> bool {
        self.pcs.remove(node_id).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preference_delta_preserves_concurrent_changes() {
        let before = ClientConfig::default();
        let mut after = before.clone();
        after.stream.bitrate_kbps = 20_000;
        let mut disk = before.clone();
        disk.stream.fps = 120;
        disk.auto_update = false;
        disk.github_token = Some("new".into());
        disk.apply_changes(&before, &after);
        assert_eq!(disk.stream.bitrate_kbps, 20_000);
        assert_eq!(disk.stream.fps, 120);
        assert!(!disk.auto_update);
        assert_eq!(disk.github_token.as_deref(), Some("new"));
    }

    #[test]
    fn stale_window_settings_preserve_new_wake_details() {
        let mut window = ClientConfig::default();
        let mut disk = window.clone();
        disk.pcs.insert(
            "pc".into(),
            KnownPc {
                mac: Some("02:00:00:00:00:01".into()),
                ..Default::default()
            },
        );
        window.stream.fullscreen = false;
        disk.apply_preferences(&window);
        assert!(!disk.stream.fullscreen);
        assert_eq!(disk.pcs["pc"].mac.as_deref(), Some("02:00:00:00:00:01"));
    }

    #[test]
    fn non_ui_fields_like_github_token_are_preserved_across_preference_updates() {
        let window = ClientConfig {
            github_token: Some("stale".into()),
            ..Default::default()
        };
        let mut disk = window.clone();
        disk.github_token = Some("ghp_secret".into());
        disk.apply_preferences(&window);
        assert_eq!(disk.github_token.as_deref(), Some("ghp_secret"));
    }

    #[test]
    fn old_stream_and_machine_fields_are_ignored() {
        let c: ClientConfig = toml::from_str(
            r#"
            [stream]
            app = "Desktop"
            bitrate_kbps = 42000
            [pcs.n]
            name = "Gaming-PC"
            server_cert = "3082"
            desktop_size = [1280, 720]
            mac = "02:00:00:00:00:01"
        "#,
        )
        .unwrap();
        assert_eq!(c.stream.bitrate_kbps, 42000);
        assert_eq!(c.pcs["n"].name, "Gaming-PC");
        assert_eq!(c.pcs["n"].mac.as_deref(), Some("02:00:00:00:00:01"));
        let saved = toml::to_string(&c).unwrap();
        assert!(
            !saved.contains("server_cert")
                && !saved.contains("desktop_size")
                && !saved.contains("app =")
        );
    }

    #[test]
    fn defaults_and_round_trip() {
        let s = StreamSettings::default();
        assert_eq!(s.resolution.pixels((3024, 1964)), (3024, 1964));
        assert_eq!(Resolution::Native.pixels((3024, 1964)), (3024, 1964));
        assert_eq!(Resolution::P1080.pixels((3024, 1964)), (1920, 1080));
        assert_eq!(Resolution::P1440.pixels((3024, 1964)), (2560, 1440));
        assert_eq!(Resolution::P2160.pixels((1920, 1080)), (3840, 2160));
        assert_eq!(Resolution::P900.pixels((3024, 1964)), (1600, 900));
        assert_eq!(Resolution::P720.pixels((3024, 1964)), (1280, 720));
        assert_eq!(Resolution::P540.pixels((3024, 1964)), (960, 540));
        assert_eq!(
            Resolution::Native.describe((3024, 1964)),
            "Match screen · 3024 × 1964"
        );
        assert_eq!(
            Resolution::P1080.describe((3024, 1964)),
            "1080p · 1920 × 1080"
        );
        assert_eq!(Resolution::P720.describe((3024, 1964)), "720p · 1280 × 720");
        let modes = latch_core::screens::stream_modes();
        for r in Resolution::ALL {
            for &screen in latch_core::screens::SCREENS {
                assert!(
                    modes.contains(&r.pixels(screen)),
                    "{r:?} on {screen:?} is a listed mode"
                );
            }
        }
        let c: ClientConfig = toml::from_str("").unwrap();
        assert_eq!(c.stream, s);
        assert_eq!(c.stream.quality, Quality::Auto, "auto unless someone chose");
        assert!(!c.sleep_prompt && c.cmd_is_ctrl && c.auto_update);
        assert!(c.capture_mouse, "games need raw movement, so capture is on");
        // A client.toml from 3.0 has no quality key; it gets Auto too.
        let c: ClientConfig = toml::from_str("[stream]\nfps = 90\n").unwrap();
        assert_eq!(c.stream.quality, Quality::Auto);
        assert_eq!(c.stream.fps, 90);
        let c: ClientConfig = toml::from_str("[stream]\nresolution = \"P720\"\n").unwrap();
        assert_eq!(c.stream.resolution, Resolution::P720);
        let mut st = StreamSettings::default();
        assert_eq!(st.describe(), "Balanced");
        st.choose(Quality::Competitive);
        assert_eq!(
            (st.quality, st.resolution),
            (Quality::Competitive, Resolution::P1080)
        );
        st.choose(Quality::Custom);
        st.resolution = Resolution::P1440;
        st.fps = 240;
        st.bitrate_kbps = 300_000;
        assert_eq!(st.describe(), "1440p · 240 fps · 300 Mbps");
        st.choose(Quality::Cinematic);
        assert_eq!(st.resolution, Resolution::NativeUpTo4K);
        st.choose(Quality::Custom);
        assert_eq!((st.resolution, st.fps), (Resolution::P1440, 240), "kept");
        let mut c = ClientConfig::default();
        c.pcs.insert(
            "n".into(),
            KnownPc {
                name: "Gaming-PC".into(),
                mac: Some("02:00:00:00:00:01".into()),
                lan_ip: Some("192.168.1.10".into()),
                public_ip: None,
                tailscale_ip: Some("100.64.0.10".into()),
                last_seen_unix: Some(1_788_739_200),
                ..Default::default()
            },
        );
        let back: ClientConfig = toml::from_str(&toml::to_string(&c).unwrap()).unwrap();
        assert_eq!(back, c);
    }

    /// A build without 720p (4.1.1, or a branch cut before it) reading a
    /// file this one wrote: the size goes back to its default, and the
    /// machines and wake addresses all stay.
    #[test]
    fn a_size_this_build_does_not_know_keeps_the_machines() {
        let text = "github_token = \"t\"\n\
                    [stream]\nquality = \"Custom\"\nresolution = \"P9999\"\nfps = 90\ncodec = \"Hevc\"\n\
                    [pcs.n1]\nname = \"Hermes\"\nmac = \"02:00:00:00:00:01\"\n\
                    public_ip = \"116.125.113.36\"\nserver_cert = \"3082\"\n\
                    [pcs.n2]\nname = \"stuzha\"\nlast_seen_unix = \"bad\"\n";
        let (c, dropped): (ClientConfig, _) = latch_core::config::parse_lenient(text).unwrap();
        assert_eq!(
            dropped,
            ["pcs.n2.last_seen_unix", "stream.resolution"],
            "only what cannot be read"
        );
        assert_eq!(c.stream.resolution, Resolution::Native);
        assert_eq!(c.stream.quality, Quality::Custom);
        assert_eq!(c.stream.fps, 90);
        assert_eq!(c.stream.codec, Codec::Hevc);
        assert_eq!(c.github_token.as_deref(), Some("t"));
        assert_eq!(c.pcs["n1"].mac.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(c.pcs["n1"].public_ip.as_deref(), Some("116.125.113.36"));
        assert_eq!(c.pcs["n2"].name, "stuzha");
        assert_eq!(c.pcs["n2"].last_seen_unix, None);
        // Text that is not TOML at all is still an error (and set aside).
        assert!(latch_core::config::parse_lenient::<ClientConfig>("[stream").is_err());
    }

    #[test]
    fn every_profile_survives_a_reload_and_nothing_under_the_hardware_is_clamped() {
        for q in Quality::ALL {
            let mut c = ClientConfig::default();
            c.stream.choose(q);
            let before = c.stream.clone();
            c.normalise();
            assert_eq!(c.stream, before, "{q:?} was changed by a reload");
        }
        let mut c = ClientConfig::default();
        c.stream.choose(Quality::Custom);
        c.stream.fps = 500;
        c.stream.bitrate_kbps = 1_000_000;
        c.normalise();
        assert_eq!((c.stream.fps, c.stream.bitrate_kbps), (500, 1_000_000));
        c.stream.bitrate_kbps = 500;
        c.stream.fps = 5;
        c.normalise();
        assert_eq!(c.stream.bitrate_kbps, 2_000);
        assert_eq!(c.stream.fps, 30);
        c.stream.fps = 100_000;
        c.stream.bitrate_kbps = u32::MAX;
        c.normalise();
        assert_eq!((c.stream.fps, c.stream.bitrate_kbps), (500, 1_000_000));
    }

    /// 5.1's profiles were Custom values; a file from it moves to the
    /// profiles that replaced them, and values set by hand stay.
    #[test]
    fn a_5_1_profile_becomes_its_successor() {
        let load = |text: &str| {
            let mut c: ClientConfig = toml::from_str(text).unwrap();
            c.normalise();
            c.stream
        };
        let old = |r: &str, fps: u32, kbps: u32| {
            format!("[stream]\nquality = \"Custom\"\nresolution = \"{r}\"\nfps = {fps}\nbitrate_kbps = {kbps}\n")
        };
        assert_eq!(
            load(&old("P1080", 60, 20_000)).quality,
            Quality::Competitive
        );
        assert_eq!(load(&old("Native", 60, 50_000)).quality, Quality::Auto);
        let sharp = load(&old("Native", 60, 100_000));
        assert_eq!(
            (sharp.quality, sharp.resolution),
            (Quality::Cinematic, Resolution::NativeUpTo4K)
        );
        let own = load(&old("P1440", 120, 80_000));
        assert_eq!(
            (own.quality, own.resolution, own.fps, own.bitrate_kbps),
            (Quality::Custom, Resolution::P1440, 120, 80_000)
        );
        // Once moved, the same values chosen by hand are left alone.
        assert_eq!(sharp.version, SETTINGS_VERSION);
        let mut again = StreamSettings {
            quality: Quality::Custom,
            resolution: Resolution::Native,
            fps: 60,
            bitrate_kbps: 50_000,
            ..sharp
        };
        again.migrate();
        assert_eq!(again.quality, Quality::Custom);
        let saved = toml::to_string(&ClientConfig::default()).unwrap();
        assert!(saved.contains("version = 2"), "{saved}");
    }

    #[test]
    fn a_forgotten_machine_leaves_others_alone() {
        let mut c = ClientConfig::default();
        for id in ["artemis", "stuzha"] {
            c.pcs.insert(
                id.into(),
                KnownPc {
                    name: id.into(),
                    ..Default::default()
                },
            );
        }
        assert!(c.forget("artemis"));
        assert!(!c.forget("artemis"), "gone already");
        assert_eq!(c.pcs.keys().collect::<Vec<_>>(), ["stuzha"]);
        // A file without the newer fields still reads.
        let back: ClientConfig =
            toml::from_str("[pcs.n]\nname = \"Gaming-PC\"\nserver_cert = \"3082\"\n").unwrap();
        assert_eq!(back.pcs["n"].tailnet, None);
    }
}
