//! The network between this Mac and a PC: direct or through a relay, and
//! how long a round trip takes. From that, what to ask of the stream when
//! the quality setting is Auto, and, when the path is relayed, why and what
//! would fix it.
//!
//! Tailscale connects two machines directly when it can punch through both
//! NATs; when it cannot, every packet goes through one of its DERP relays.
//! Path latency and capacity are independent. Show the route as diagnostics;
//! never turn a high RTT or the presence of a relay into a quality cap.

use crate::config::{Quality, Resolution, StreamSettings};
use latch_core::api::NatReport;
use latch_core::tailscale::derp_city;
use latch_ui::Tone;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Path {
    /// `Some(true)` when packets go straight to the PC, `Some(false)` when
    /// they go through a relay, `None` before any traffic has flowed.
    pub direct: Option<bool>,
    /// The relay's region code ("tok") when relayed.
    pub relay: String,
    /// Tailscale's `PeerRelay` for this peer: non-empty when the path goes
    /// through a relay node on the tailnet rather than a DERP.
    pub peer_relay: String,
    /// One round trip to the PC, from a TCP connect.
    pub rtt_ms: Option<u32>,
}

/// Which of Tailscale's three paths carries the stream, in its own order of
/// preference: straight to the PC, through a relay node on the tailnet, or
/// through one of Tailscale's shared DERP relays.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PathKind {
    Direct,
    PeerRelay,
    Derp,
}

/// The path in use; `None` before any traffic has flowed. A direct path
/// wins over a peer relay even when Tailscale still reports one, and a
/// peer relay wins over the home DERP region, which is reported whether or
/// not packets go through it.
pub fn path_kind(path: &Path) -> Option<PathKind> {
    match path.direct {
        Some(true) => Some(PathKind::Direct),
        _ if !path.peer_relay.is_empty() => Some(PathKind::PeerRelay),
        Some(false) => Some(PathKind::Derp),
        None => None,
    }
}

impl Path {
    pub fn relayed(&self) -> bool {
        matches!(path_kind(self), Some(PathKind::PeerRelay | PathKind::Derp))
    }

    /// "Direct · 38 ms", "Via your relay · 60 ms", "Relayed via Tokyo ·
    /// 210 ms", "Path unknown".
    pub fn label(&self) -> String {
        let rtt = self
            .rtt_ms
            .map(|ms| format!(" · {ms} ms"))
            .unwrap_or_default();
        match path_kind(self) {
            Some(PathKind::Direct) => format!("Direct{rtt}"),
            Some(PathKind::PeerRelay) => format!("Via your relay{rtt}"),
            Some(PathKind::Derp) if self.relay.is_empty() => format!("Relayed{rtt}"),
            Some(PathKind::Derp) => format!("Relayed via {}{rtt}", derp_city(&self.relay)),
            None => format!("Path unknown{rtt}"),
        }
    }

    /// A DERP detour is the one thing worth a warning: it works, but it is
    /// shared and often fixable (see [`explain`]). A relay of your own is a
    /// detour chosen on purpose, and a long direct path is just distance,
    /// so both are plain; only a short direct path is green. Nothing about
    /// a path is red: red means broken.
    pub fn tone(&self) -> Tone {
        match (path_kind(self), self.rtt_ms) {
            (Some(PathKind::Derp), _) => Tone::Warning,
            (Some(PathKind::PeerRelay), _) => Tone::Neutral,
            (Some(PathKind::Direct), Some(ms)) if ms >= 80 => Tone::Neutral,
            (Some(PathKind::Direct), _) => Tone::Success,
            (None, _) => Tone::Neutral,
        }
    }
}

/// Recommended starting quality. RTT measures delay, not throughput: neither
/// a VPS relay nor a distant direct connection implies a bandwidth cap.
pub fn effective(settings: &StreamSettings) -> StreamSettings {
    effective_for(settings, settings.fps)
}

/// Recommended starting quality taking into account the local display's refresh rate (Hz).
pub fn effective_for(settings: &StreamSettings, display_hz: u32) -> StreamSettings {
    let mut s = settings.clone();
    if s.quality == Quality::Auto {
        s.resolution = Resolution::Native;
        s.bitrate_kbps = 50_000;
        let hz = if display_hz > 0 { display_hz } else { 60 };
        s.fps = hz.clamp(30, 240);
    }
    s
}

/// Why the PC is reached through a relay and what would give a direct
/// path, from the two machines' own network reports. `None` when the path
/// is direct or not known yet.
pub fn explain(
    pc: &str,
    path: &Path,
    pc_nat: Option<&NatReport>,
    mac_nat: Option<&NatReport>,
) -> Option<String> {
    let mut out = match path_kind(path) {
        Some(PathKind::PeerRelay) => format!(
            "{pc} is reached through your relay, not directly. Every packet takes that detour, which adds delay but caps neither the bitrate nor the frame rate. "
        ),
        Some(PathKind::Derp) if path.relay.is_empty() => format!(
            "{pc} is reached through a Tailscale relay, not directly. Every packet takes that detour; the relay adds delay and its own throughput is shared. "
        ),
        Some(PathKind::Derp) => format!(
            "{pc} is reached through Tailscale's {} relay, not directly. Every packet takes that detour; the relay adds delay and its own throughput is shared. ",
            derp_city(&path.relay)
        ),
        Some(PathKind::Direct) | None => return None,
    };
    let cause = match (pc_nat, mac_nat) {
        (Some(p), _) if !p.udp => format!(
            "{pc}'s network blocks UDP, so no direct path is possible from anywhere. A different network for {pc}, or a Tailscale subnet router beside it, is the only fix."
        ),
        (_, Some(m)) if !m.udp => "This Mac's network blocks UDP, so nothing can reach it directly; another network (a phone's hotspot will do) fixes it.".to_string(),
        (Some(p), Some(m)) => {
            let pc_hard = p.hard == Some(true) && !p.portmap;
            let mac_hard = m.hard == Some(true) && !m.portmap;
            if pc_hard {
                let mut t = format!(
                    "{pc}'s router is a hard NAT with no UPnP: it hides {pc} from anyone who has not been spoken to first. Turn UPnP or NAT-PMP on in that router (on {pc}, its page is usually at the gateway address), or forward a UDP port to {pc} and tell Tailscale; Tailscale then connects directly."
                );
                if mac_hard {
                    t.push_str(" This Mac's network is a hard NAT too, which makes the relay certain until one side changes.");
                }
                if p.ipv6 && !m.ipv6 {
                    t.push_str(&format!(
                        " {pc} has IPv6; a network with IPv6 on this machine's side would also connect directly."
                    ));
                }
                t
            } else if mac_hard {
                format!(
                    "{pc}'s network is fine; this machine's is a hard NAT with no UPnP. From another network, or with UPnP on this router, the connection is direct."
                )
            } else {
                "Both networks look easy to traverse, so Tailscale should switch to a direct path within a minute of traffic. If it never does, the routers may be blocking UDP between them.".to_string()
            }
        }
        (None, Some(m)) => {
            let mut t = format!(
                "Latch Host on {pc} would report that side of the network and say which router is in the way."
            );
            if m.hard == Some(true) && !m.portmap {
                t.push_str(" This Mac's network is a hard NAT with no UPnP, which is often enough on its own; another network here may connect directly.");
            }
            t
        }
        (Some(p), None) => {
            if p.hard == Some(true) && !p.portmap {
                format!(
                    "{pc}'s router is a hard NAT with no UPnP. Turn UPnP or NAT-PMP on in that router, or forward a UDP port to {pc}; Tailscale then connects directly."
                )
            } else {
                format!("{pc}'s network looks easy to traverse; this machine's side has not been checked yet.")
            }
        }
        (None, None) => {
            "A direct path needs UPnP or NAT-PMP on at least one router, or IPv6 on both networks.".to_string()
        }
    };
    out.push_str(&cause);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nat(udp: bool, hard: Option<bool>, portmap: bool, ipv6: bool) -> NatReport {
        NatReport {
            udp,
            ipv4: true,
            ipv6,
            hard,
            portmap,
            derp: "tok".into(),
        }
    }

    #[test]
    fn labels_say_direct_relayed_or_unknown() {
        let p = Path {
            direct: Some(false),
            relay: "tok".into(),
            rtt_ms: Some(210),
            ..Default::default()
        };
        assert_eq!(p.label(), "Relayed via Tokyo · 210 ms");
        assert_eq!(
            p.tone(),
            Tone::Warning,
            "a detour worth fixing, not a failure"
        );
        let p = Path {
            direct: Some(true),
            relay: "tok".into(),
            rtt_ms: Some(38),
            ..Default::default()
        };
        assert_eq!(p.label(), "Direct · 38 ms");
        assert_eq!(p.tone(), Tone::Success);
        let p = Path {
            direct: Some(true),
            rtt_ms: Some(120),
            ..Default::default()
        };
        assert_eq!(
            p.tone(),
            Tone::Neutral,
            "a long round trip is distance, not a fault"
        );
        assert_eq!(Path::default().label(), "Path unknown");
        assert_eq!(Path::default().tone(), Tone::Neutral);
        let p = Path {
            direct: Some(false),
            rtt_ms: Some(150),
            ..Default::default()
        };
        assert_eq!(p.label(), "Relayed · 150 ms");
    }

    #[test]
    fn a_peer_relay_is_named_as_yours_not_as_a_derp_detour() {
        // Given: no direct path, home DERP "tok" still reported, but a relay
        // node on the tailnet carries the packets.
        let p = Path {
            direct: Some(false),
            relay: "tok".into(),
            peer_relay: "100.64.0.40:40000:vni:17".into(),
            rtt_ms: Some(60),
        };
        assert_eq!(p.label(), "Via your relay · 60 ms");
        assert_eq!(p.tone(), Tone::Neutral, "a detour chosen on purpose");
        assert!(!p.label().contains("Tokyo"));

        // Given: the relay is named before any traffic has flowed.
        let early = Path {
            peer_relay: "100.64.0.40:40000:vni:17".into(),
            ..Default::default()
        };
        assert_eq!(early.label(), "Via your relay");
        assert_eq!(early.tone(), Tone::Neutral);

        // Given: a direct path holds while Tailscale still names the relay.
        let direct = Path {
            direct: Some(true),
            peer_relay: "100.64.0.40:40000:vni:17".into(),
            rtt_ms: Some(12),
            ..Default::default()
        };
        assert_eq!(direct.label(), "Direct · 12 ms");
        assert_eq!(direct.tone(), Tone::Success);

        let mac_easy = nat(true, Some(false), false, false);
        let pc_hard = nat(true, Some(true), false, true);
        let t = explain("Gaming-PC", &p, Some(&pc_hard), Some(&mac_easy)).unwrap();
        assert!(
            t.starts_with("Gaming-PC is reached through your relay, not directly."),
            "{t}"
        );
        assert!(!t.contains("Tokyo"), "{t}");
        assert!(!t.contains("few megabits"), "{t}");
        assert!(
            t.contains("Gaming-PC's router is a hard NAT with no UPnP"),
            "the way to a direct path is still worth saying: {t}"
        );
        let t = explain("Gaming-PC", &early, None, None).unwrap();
        assert!(
            t.starts_with("Gaming-PC is reached through your relay"),
            "{t}"
        );
    }

    /// The path is not an input at all: latency and relays never lower
    /// what Auto asks for.
    #[test]
    fn auto_is_the_recommended_quality_and_custom_is_left_alone() {
        let auto = StreamSettings {
            fps: 120,
            ..Default::default()
        };
        let s = effective(&auto);
        assert_eq!(
            (s.resolution, s.fps, s.bitrate_kbps),
            (Resolution::Native, 120, 50_000)
        );
        let custom = StreamSettings {
            quality: Quality::Custom,
            bitrate_kbps: 150_000,
            ..auto
        };
        assert_eq!(effective(&custom), custom);
    }

    #[test]
    fn effective_quality_uses_display_refresh_rate() {
        let default_settings = StreamSettings::default();
        // 120 Hz display capability (e.g. ProMotion)
        let s120 = effective_for(&default_settings, 120);
        assert_eq!(s120.fps, 120);
        assert_eq!(s120.resolution, Resolution::Native);

        // Clamping to max 240 fps and min 30 fps
        assert_eq!(effective_for(&default_settings, 360).fps, 240);
        assert_eq!(effective_for(&default_settings, 15).fps, 30);
    }

    #[test]
    fn a_direct_path_outranks_a_peer_relay_which_outranks_derp() {
        let direct = Path {
            direct: Some(true),
            relay: "tok".into(),
            ..Default::default()
        };
        assert_eq!(path_kind(&direct), Some(PathKind::Direct));
        assert!(!direct.relayed());

        // Given: Tailscale still names a peer relay while a direct path holds.
        let direct_over_relay = Path {
            peer_relay: "100.64.0.40:40000:vni:17".into(),
            ..direct.clone()
        };
        assert_eq!(path_kind(&direct_over_relay), Some(PathKind::Direct));
        assert!(!direct_over_relay.relayed());

        // Given: no direct path, but a relay node on the tailnet carries it.
        // Relay ("tok") is the home DERP region and must not win.
        let peer_relayed = Path {
            direct: Some(false),
            relay: "tok".into(),
            peer_relay: "100.64.0.40:40000:vni:17".into(),
            rtt_ms: Some(60),
        };
        assert_eq!(path_kind(&peer_relayed), Some(PathKind::PeerRelay));
        assert!(peer_relayed.relayed());

        // Given: a peer relay named before any traffic has flowed.
        let early = Path {
            peer_relay: "100.64.0.40:40000:vni:17".into(),
            ..Default::default()
        };
        assert_eq!(path_kind(&early), Some(PathKind::PeerRelay));

        let derp = Path {
            direct: Some(false),
            relay: "tok".into(),
            ..Default::default()
        };
        assert_eq!(path_kind(&derp), Some(PathKind::Derp));
        assert!(derp.relayed());

        assert_eq!(path_kind(&Path::default()), None);
        assert!(!Path::default().relayed());
    }

    #[test]
    fn explanations_name_the_router_in_the_way() {
        let relayed = Path {
            direct: Some(false),
            relay: "tok".into(),
            rtt_ms: Some(200),
            ..Default::default()
        };
        let direct = Path {
            direct: Some(true),
            ..Default::default()
        };
        assert_eq!(explain("Gaming-PC", &direct, None, None), None);
        assert_eq!(explain("Gaming-PC", &Path::default(), None, None), None);

        let pc_hard = nat(true, Some(true), false, true);
        let mac_easy = nat(true, Some(false), false, false);
        let t = explain("Gaming-PC", &relayed, Some(&pc_hard), Some(&mac_easy)).unwrap();
        assert!(
            t.starts_with("Gaming-PC is reached through Tailscale's Tokyo relay"),
            "{t}"
        );
        assert!(
            t.contains("Gaming-PC's router is a hard NAT with no UPnP"),
            "{t}"
        );
        assert!(t.contains("IPv6"), "{t}");
        assert!(!t.contains("hard NAT too"), "{t}");

        let mac_hard = nat(true, Some(true), false, false);
        let t = explain("Gaming-PC", &relayed, Some(&pc_hard), Some(&mac_hard)).unwrap();
        assert!(t.contains("hard NAT too"), "{t}");

        let pc_easy = nat(true, Some(false), true, false);
        let t = explain("Gaming-PC", &relayed, Some(&pc_easy), Some(&mac_hard)).unwrap();
        assert!(t.contains("this machine's is a hard NAT"), "{t}");

        let t = explain("Gaming-PC", &relayed, Some(&pc_easy), Some(&mac_easy)).unwrap();
        assert!(t.contains("within a minute"), "{t}");

        let no_udp = nat(false, None, false, false);
        let t = explain("Gaming-PC", &relayed, Some(&no_udp), Some(&mac_easy)).unwrap();
        assert!(t.contains("blocks UDP"), "{t}");

        // An old host reports nothing: say so, and what the Mac knows.
        let t = explain("Gaming-PC", &relayed, None, Some(&mac_hard)).unwrap();
        assert!(t.contains("Latch Host on Gaming-PC"), "{t}");
        assert!(!t.contains("3.1"), "{t}");
        assert!(t.contains("often enough on its own"), "{t}");
        let t = explain("Gaming-PC", &relayed, None, None).unwrap();
        assert!(t.contains("UPnP"), "{t}");
        // Nothing here is a wall of text.
        assert!(t.len() < 600, "{}", t.len());
    }
}
