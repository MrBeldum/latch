//! Settings and what has been learned about each PC, in
//! `~/Library/Application Support/com.bardbro.Latch/client.toml`.

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
    /// This screen's own pixel size.
    Native,
}

impl Resolution {
    /// Every choice, in the order the list shows them.
    pub const ALL: [Resolution; 7] = [
        Resolution::Native,
        Resolution::P540,
        Resolution::P720,
        Resolution::P900,
        Resolution::P1080,
        Resolution::P1440,
        Resolution::P2160,
    ];

    /// This choice applied to a screen of `native` pixels: the standard
    /// 16:9 sizes are exactly that, and "Match screen" is the screen
    /// itself. See `latch_core::screens`.
    pub fn pixels(self, native: (u32, u32)) -> (u32, u32) {
        let [p540, p720, p900, p1080, p1440, p2160] = latch_core::screens::STANDARD;
        match self {
            Resolution::P540 => p540,
            Resolution::P720 => p720,
            Resolution::P900 => p900,
            Resolution::P1080 => p1080,
            Resolution::P1440 => p1440,
            Resolution::P2160 => p2160,
            Resolution::Native => latch_core::screens::even(native),
        }
    }

    /// "1080p · 1920 × 1080", or "Match screen · 3024 × 1964".
    pub fn describe(self, native: (u32, u32)) -> String {
        let (w, h) = self.pixels(native);
        format!("{} · {w} × {h}", self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Codec {
    /// HEVC when the PC and this machine both decode it, else H.264.
    Auto,
    Hevc,
    H264,
}

/// Who decides resolution, frame rate and bitrate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum Quality {
    /// Recommended quality for this screen. Route latency is not bandwidth.
    #[default]
    Auto,
    /// The values in [`StreamSettings`], as set.
    Custom,
}

/// Three named points on the quality scale, for the toolbar menu.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Preset {
    Smooth,
    Balanced,
    Sharp,
}

impl Preset {
    pub const ALL: [Preset; 3] = [Preset::Smooth, Preset::Balanced, Preset::Sharp];

    pub fn label(self) -> &'static str {
        match self {
            Preset::Smooth => "Smooth",
            Preset::Balanced => "Balanced",
            Preset::Sharp => "Sharp",
        }
    }

    /// Resolution, frames per second, kilobits per second.
    pub fn values(self) -> (Resolution, u32, u32) {
        match self {
            Preset::Smooth => (Resolution::P1080, 60, 20_000),
            Preset::Balanced => (Resolution::Native, 60, 50_000),
            Preset::Sharp => (Resolution::Native, 60, 100_000),
        }
    }

    /// "1080p · 60 fps · 20 Mbps"
    pub fn describe(self) -> String {
        let (r, fps, kbps) = self.values();
        format!("{} · {fps} fps · {} Mbps", r.label(), kbps / 1000)
    }
}

impl Resolution {
    pub fn label(self) -> &'static str {
        match self {
            Resolution::P540 => "540p",
            Resolution::P720 => "720p",
            Resolution::P900 => "900p",
            Resolution::P1080 => "1080p",
            Resolution::P1440 => "1440p",
            Resolution::P2160 => "4K",
            Resolution::Native => "Match screen",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StreamSettings {
    pub quality: Quality,
    pub resolution: Resolution,
    pub fps: u32,
    pub bitrate_kbps: u32,
    pub codec: Codec,
    /// The Sunshine app to start; "Desktop" is the whole PC.
    pub app: String,
    pub fullscreen: bool,
}

impl Default for StreamSettings {
    fn default() -> Self {
        Self {
            quality: Quality::Auto,
            resolution: Resolution::Native,
            fps: 60,
            bitrate_kbps: 50_000,
            codec: Codec::Auto,
            app: "Desktop".into(),
            fullscreen: true,
        }
    }
}

impl StreamSettings {
    /// Switch to Custom with a preset's values.
    pub fn apply_preset(&mut self, p: Preset) {
        let (r, fps, kbps) = p.values();
        self.quality = Quality::Custom;
        self.resolution = r;
        self.fps = fps;
        self.bitrate_kbps = kbps;
    }

    /// The preset these values are, if they are one.
    pub fn preset(&self) -> Option<Preset> {
        Preset::ALL
            .into_iter()
            .find(|p| p.values() == (self.resolution, self.fps, self.bitrate_kbps))
    }

    /// "1080p · 60 fps · 20 Mbps"
    pub fn describe(&self) -> String {
        format!(
            "{} · {} fps · {} Mbps",
            self.resolution.label(),
            self.fps,
            self.bitrate_kbps / 1000
        )
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
    /// Sunshine's certificate (hex DER) from pairing; absent until paired.
    pub server_cert: Option<String>,
    /// Tailscale OS string, so a remembered machine still shows Windows /
    /// macOS / Linux when Tailscale is down.
    #[serde(default)]
    pub os: String,
    /// The PC's Tailscale address, so it can still be listed and reached
    /// when this Mac's Tailscale cannot say.
    pub tailscale_ip: Option<String>,
    /// When the PC was last seen online (Unix seconds).
    pub last_seen_unix: Option<u64>,
    /// The size its Desktop was last launched at from this machine, so the
    /// first connect after Latch reopens can rejoin it at that size.
    pub desktop_size: Option<[u32; 2]>,
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
        if c.stream.quality == Quality::Auto {
            c.stream.resolution = Resolution::Native;
        }
        c.normalise();
        c
    }

    /// Keep a hand-edited or older file within what the UI offers: the
    /// frame rates it lists and the bitrate slider's 2 to 150 Mbps.
    fn normalise(&mut self) {
        self.stream.fps = self.stream.fps.clamp(30, 240);
        self.stream.bitrate_kbps = self.stream.bitrate_kbps.clamp(2_000, 150_000);
        if self.stream.app.trim().is_empty() {
            self.stream.app = "Desktop".into();
        }
    }

    /// The window owns preferences; discovery and pairing own `pcs`. The
    /// window's copy of `pcs` is refreshed from the file at the same time, so
    /// a PC paired or seen during the run is known to the window too.
    pub fn save(&mut self) -> anyhow::Result<()> {
        let mut pcs = None;
        Self::update(|current| {
            current.apply_preferences(self);
            pcs = Some(current.pcs.clone());
        })?;
        if let Some(pcs) = pcs {
            self.pcs = pcs;
        }
        Ok(())
    }

    fn apply_preferences(&mut self, settings: &Self) {
        let pcs = std::mem::take(&mut self.pcs);
        *self = settings.clone();
        self.pcs = pcs;
    }

    /// One read-modify-write of the file, under the process-wide config lock.
    pub fn update(edit: impl FnOnce(&mut Self)) -> anyhow::Result<()> {
        latch_core::config::update(FILE, edit)
    }

    /// Forget a machine: its addresses, how to wake it, and its pairing.
    /// One still on the tailnet is listed again, unpaired and unwakeable
    /// until it has been seen online.
    pub fn forget(&mut self, node_id: &str) -> bool {
        self.pcs.remove(node_id).is_some()
    }

    pub fn forget_pin_on_mismatch(&mut self, node_id: &str, err: &anyhow::Error) -> bool {
        if !latch_stream::nvhttp::is_pin_mismatch(err) {
            return false;
        }
        if let Some(pc) = self.pcs.get_mut(node_id) {
            pc.server_cert = None;
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stale_window_settings_preserve_new_pairing_and_wake_details() {
        let mut window = ClientConfig::default();
        let mut disk = window.clone();
        disk.pcs.insert(
            "pc".into(),
            KnownPc {
                server_cert: Some("new-cert".into()),
                mac: Some("02:00:00:00:00:01".into()),
                ..Default::default()
            },
        );
        window.stream.fullscreen = false;
        disk.apply_preferences(&window);
        assert!(!disk.stream.fullscreen);
        assert_eq!(disk.pcs["pc"].server_cert.as_deref(), Some("new-cert"));
        assert_eq!(disk.pcs["pc"].mac.as_deref(), Some("02:00:00:00:00:01"));
        window = disk.clone();
        disk.pcs.get_mut("pc").unwrap().server_cert = None;
        disk.apply_preferences(&window);
        assert_eq!(
            disk.pcs["pc"].server_cert, None,
            "never revive a revoked pin"
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
        assert_eq!(st.preset(), Some(Preset::Balanced));
        st.apply_preset(Preset::Smooth);
        assert_eq!(st.quality, Quality::Custom);
        assert_eq!(st.preset(), Some(Preset::Smooth));
        assert_eq!(st.describe(), "1080p · 60 fps · 20 Mbps");
        assert_eq!(Preset::Sharp.describe(), "Match screen · 60 fps · 100 Mbps");
        let mut c = ClientConfig::default();
        c.pcs.insert(
            "n".into(),
            KnownPc {
                name: "Gaming-PC".into(),
                mac: Some("02:00:00:00:00:01".into()),
                lan_ip: Some("192.168.1.10".into()),
                public_ip: None,
                server_cert: Some("3082".into()),
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
    /// machines, their certificates and wake addresses all stay.
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
        assert_eq!(c.pcs["n1"].server_cert.as_deref(), Some("3082"));
        assert_eq!(c.pcs["n1"].mac.as_deref(), Some("02:00:00:00:00:01"));
        assert_eq!(c.pcs["n1"].public_ip.as_deref(), Some("116.125.113.36"));
        assert_eq!(c.pcs["n2"].name, "stuzha");
        assert_eq!(c.pcs["n2"].last_seen_unix, None);
        // Text that is not TOML at all is still an error (and set aside).
        assert!(latch_core::config::parse_lenient::<ClientConfig>("[stream").is_err());
    }

    #[test]
    fn every_preset_survives_a_reload() {
        for p in Preset::ALL {
            let mut c = ClientConfig::default();
            c.stream.apply_preset(p);
            if c.stream.quality == Quality::Auto {
                c.stream.resolution = Resolution::Native;
            }
            c.normalise();
            assert_eq!(c.stream.preset(), Some(p), "{p:?} was clamped away");
        }
        let mut c = ClientConfig::default();
        c.stream.bitrate_kbps = 500;
        c.stream.fps = 5;
        c.stream.app = "  ".into();
        if c.stream.quality == Quality::Auto {
            c.stream.resolution = Resolution::Native;
        }
        c.normalise();
        assert_eq!(c.stream.bitrate_kbps, 2_000);
        assert_eq!(c.stream.fps, 30);
        assert_eq!(c.stream.app, "Desktop");
    }

    #[test]
    fn pin_mismatch_clears_the_stored_server_cert() {
        let mut c = ClientConfig::default();
        c.pcs.insert(
            "n".into(),
            KnownPc {
                name: "Gaming-PC".into(),
                server_cert: Some("3082".into()),
                ..Default::default()
            },
        );
        let mismatch = anyhow::Error::from(latch_stream::nvhttp::PinMismatch);
        assert!(c.forget_pin_on_mismatch("n", &mismatch));
        assert_eq!(c.pcs["n"].server_cert, None);

        c.pcs.get_mut("n").unwrap().server_cert = Some("3082".into());
        let generic = anyhow::anyhow!("invalid peer certificate: expired");
        assert!(!c.forget_pin_on_mismatch("n", &generic));
        assert_eq!(c.pcs["n"].server_cert.as_deref(), Some("3082"));
    }

    #[test]
    fn a_forgotten_machine_takes_its_pairing_and_nothing_else() {
        let mut c = ClientConfig::default();
        for id in ["artemis", "stuzha"] {
            c.pcs.insert(
                id.into(),
                KnownPc {
                    name: id.into(),
                    server_cert: Some("3082".into()),
                    desktop_size: Some([1280, 720]),
                    ..Default::default()
                },
            );
        }
        assert!(c.forget("artemis"));
        assert!(!c.forget("artemis"), "gone already");
        assert_eq!(c.pcs.keys().collect::<Vec<_>>(), ["stuzha"]);
        assert_eq!(c.pcs["stuzha"].server_cert.as_deref(), Some("3082"));
        // A file without the newer fields still reads.
        let back: ClientConfig =
            toml::from_str("[pcs.n]\nname = \"Gaming-PC\"\nserver_cert = \"3082\"\n").unwrap();
        assert_eq!(back.pcs["n"].desktop_size, None);
        assert_eq!(back.pcs["n"].tailnet, None);
    }
}
