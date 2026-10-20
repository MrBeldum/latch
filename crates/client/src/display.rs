//! Asking a PC why its picture is black, and letting it be fixed from here.
//!
//! When a stream is connected and every frame is black, the one thing the
//! Mac cannot do is look at the PC to find out why. The PC answers instead,
//! over the same control API that can put it to sleep, and the answer is a
//! sentence a person can act on rather than a dump of display settings.

use anyhow::Result;
use latch_core::api::DisplayRequest;
use latch_core::{http, CONTROL_PORT};
use serde_json::Value;
use std::net::Ipv4Addr;
use std::time::Duration;

/// Turning HDR off answers within seconds.
const TIMEOUT: Duration = Duration::from_secs(25);
/// The report is slower: PowerShell with WMI and a screen sample, then two
/// calls to the engine's web API of up to 8 s each.
const REPORT_TIMEOUT: Duration = Duration::from_secs(60);

/// What the PC said, in a form the stream's notice can show.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Help {
    /// One sentence naming the cause, empty while nothing is known.
    pub message: String,
    /// The PC's advanced-colour desktop is what breaks the capture, and it
    /// can be turned off from here.
    pub hdr_is_on: bool,
    /// What that mode is called on the PC, for the button that turns it
    /// off: "HDR" or "wide colour".
    pub mode: String,
    /// A fix is on its way to the PC.
    pub busy: bool,
}

/// Whether a stream's video problem is the black picture, the one problem
/// the PC can be asked about. Video that never came, stopped coming or
/// cannot be decoded is the network's doing or this Mac's.
pub fn is_black_picture(problem: &str) -> bool {
    problem.contains("black picture")
}

/// Whether the PC can say why its picture is black: only a Windows host
/// reports its displays. An OS not known yet is asked anyway.
pub fn can_explain(os: &str) -> bool {
    os.is_empty() || os.eq_ignore_ascii_case("windows")
}

/// The answer when there is none to be had, so the notice stops saying it
/// is asking.
pub fn no_answer(os: &str) -> Help {
    Help {
        message: if can_explain(os) {
            "The PC couldn't say why. Restarting the stream, or the PC, is the next thing to try."
        } else {
            "Only a Windows PC can be asked why its picture is black. Restarting the stream is the next thing to try."
        }
        .into(),
        ..Default::default()
    }
}

pub fn ask(ip: Ipv4Addr) -> Result<Value> {
    http::get_json::<Value>((ip, CONTROL_PORT), "/v1/display", REPORT_TIMEOUT)
}

/// Turn the PC's HDR desktop off (or back on) and return its new state.
pub fn set_hdr(ip: Ipv4Addr, on: bool) -> Result<Value> {
    http::post_json(
        (ip, CONTROL_PORT),
        "/v1/display",
        &DisplayRequest { advanced_color: on },
        TIMEOUT,
    )
}

/// Whether any of the PC's displays is composing in advanced colour.
pub fn hdr_is_on(report: &Value) -> bool {
    displays(report).any(|d| d["enabled"] == Value::Bool(true))
}

/// Whether turning it off is a switch anyone can throw. Windows enforces
/// the mode on a display with no monitor behind it — neither supported nor
/// switched on by anyone — and then refuses every switch that would leave
/// it. Offering a button for that would be offering a button that fails.
fn can_be_turned_off(report: &Value) -> bool {
    displays(report).any(|d| {
        d["enabled"] == Value::Bool(true)
            && (d["supported"] == Value::Bool(true)
                || d["hdr_on"] == Value::Bool(true)
                || d["wide_color_on"] == Value::Bool(true))
    })
}

fn displays(report: &Value) -> impl Iterator<Item = &Value> {
    report["advanced_color"]["displays"]
        .as_array()
        .map(|ds| ds.iter())
        .unwrap_or_default()
}

/// What Windows calls the mode it is in. A PC that has lost its monitor
/// usually ends up in wide colour rather than HDR proper, and the two are
/// different switches in the PC's own settings.
fn colour_mode(report: &Value) -> String {
    // Windows 11 names the running mode outright; older ones only say
    // whether wide colour is being enforced.
    if let Some(mode) = displays(report).find_map(|d| d["mode"].as_str()) {
        return mode.to_string();
    }
    if displays(report)
        .any(|d| d["enabled"] == Value::Bool(true) && d["wide_color_enforced"] == Value::Bool(true))
    {
        "wide colour".into()
    } else {
        "HDR".into()
    }
}

/// True when Windows lists no monitor that is actually plugged in. The
/// devices it remembers stay listed with `Present: false`.
fn no_monitor(report: &Value) -> bool {
    match report["windows"]["monitors"].as_array() {
        Some(monitors) => !monitors.iter().any(|m| m["Present"] == Value::Bool(true)),
        None => false,
    }
}

/// The brightest pixel the PC sampled on its own desktop, if it looked.
fn desktop_brightness(report: &Value) -> Option<u64> {
    report["windows"]["desktop"]["max"].as_u64()
}

/// One sentence about why the picture is black, in the order that matters:
/// the causes that can be fixed from here come first.
pub fn verdict(report: &Value) -> Help {
    let hdr = hdr_is_on(report);
    let dark = desktop_brightness(report).is_some_and(|max| max <= 16);
    let headless = no_monitor(report);
    let mode = colour_mode(report);
    let switchable = can_be_turned_off(report);
    let message = if hdr && !switchable {
        // Windows will not leave this one while the PC has no display.
        return Help {
            message: format!(
                "Windows is composing the PC's desktop in {mode} on a \
                 placeholder display, because no monitor is attached. With \
                 no display to describe the brightness, the capture turns \
                 every frame black — and Windows will not leave that mode \
                 until the PC has a display. Attach a monitor or an \
                 HDMI/DisplayPort dummy plug, or install a virtual display \
                 driver on the PC."
            ),
            hdr_is_on: false,
            mode,
            busy: false,
        };
    } else if hdr && headless {
        return Help {
            message: format!(
                "The PC has no monitor attached, but Windows is still \
                 composing its desktop in {mode}. With no display left to \
                 describe the brightness, the capture converts every frame \
                 to black."
            ),
            hdr_is_on: true,
            mode,
            busy: false,
        };
    } else if hdr {
        return Help {
            message: format!(
                "The PC's desktop is composed in {mode}. The capture \
                 converts it to an ordinary picture, and on this PC that \
                 conversion is coming out black."
            ),
            hdr_is_on: true,
            mode,
            busy: false,
        };
    } else if dark && headless {
        "The PC has no monitor attached and its desktop is drawing nothing. \
         Attach a monitor or a dummy plug, or enable a virtual display."
    } else if dark {
        "The PC's own desktop is black: its display is off or asleep. Wake \
         it with a key press, or check the PC's power settings."
    } else if report["windows"]["locked"] == Value::Bool(true) {
        "The PC is showing its lock screen, which the streaming engine \
         cannot capture. Sign in on the PC, or let the engine run as a \
         service."
    } else if headless {
        "No monitor is attached to the PC, so Windows is drawing to a \
         placeholder display that captures as black. Attach a monitor or a \
         dummy plug, or enable a virtual display."
    } else if desktop_brightness(report).is_some() {
        "The PC's desktop has a picture, so the capture is what is failing. \
         Restarting the stream, or the PC, is the next thing to try."
    } else {
        return Help::default();
    };
    Help {
        message: message.into(),
        hdr_is_on: false,
        mode: String::new(),
        busy: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(hdr: bool, present: bool, max: u64, locked: bool) -> Value {
        serde_json::json!({
            "windows": {
                "locked": locked,
                "monitors": [{"FriendlyName": "Generic Monitor", "Present": present}],
                "desktop": {"max": max, "mean": 100, "lit": 998, "samples": 1000},
            },
            "advanced_color": {"supported": true, "displays": [
                {"display": "\\\\.\\DISPLAY1", "supported": true, "enabled": hdr,
                 "bits_per_color": if hdr { 10 } else { 8 }}
            ]},
        })
    }

    #[test]
    fn an_hdr_desktop_without_a_monitor_is_named_and_offered_a_fix() {
        let mut r = report(true, false, 255, false);
        r["advanced_color"]["displays"][0]["supported"] = Value::Bool(true);
        let h = verdict(&r);
        assert!(h.message.contains("no monitor"), "{}", h.message);
        assert!(h.message.contains("HDR"), "{}", h.message);
        assert!(h.hdr_is_on);
    }

    /// Exactly what a PC that has lost its monitor answers: Windows runs
    /// wide colour on a placeholder display that supports neither mode and
    /// that nobody switched on, and refuses every switch away from it.
    #[test]
    fn a_mode_windows_enforces_is_explained_rather_than_offered_a_switch() {
        let mut r = report(true, false, 255, false);
        r["advanced_color"]["displays"][0] = serde_json::json!({
            "display": "\\\\.\\DISPLAY1",
            "supported": false, "enabled": true, "wide_color_enforced": true,
            "force_disabled": false, "bits_per_color": 10, "color_encoding": 0,
            "sdr_white_level": 1000, "mode": "wide colour", "active": true,
            "hdr_supported": false, "hdr_on": false,
            "wide_color_supported": false, "wide_color_on": false,
            "limited_by_policy": false,
        });
        let h = verdict(&r);
        assert!(h.message.contains("wide colour"), "{}", h.message);
        assert!(
            h.message.contains("no monitor is attached"),
            "{}",
            h.message
        );
        assert!(h.message.contains("dummy plug"), "{}", h.message);
        assert_eq!(h.mode, "wide colour");
        assert!(
            !h.hdr_is_on,
            "no switch may be offered for a mode Windows refuses to leave"
        );
    }

    /// The same mode, but switched on by whoever owns the PC: a button can
    /// turn that one off.
    #[test]
    fn a_mode_someone_turned_on_keeps_its_switch() {
        let mut r = report(true, true, 255, false);
        r["advanced_color"]["displays"][0]["mode"] = "HDR".into();
        r["advanced_color"]["displays"][0]["hdr_on"] = Value::Bool(true);
        let h = verdict(&r);
        assert!(h.hdr_is_on, "{}", h.message);
        assert_eq!(h.mode, "HDR");
    }

    #[test]
    fn hdr_with_a_monitor_still_offers_the_switch() {
        let mut r = report(true, true, 255, false);
        r["advanced_color"]["displays"][0]["supported"] = Value::Bool(true);
        let h = verdict(&r);
        assert!(h.message.contains("HDR"), "{}", h.message);
        assert_eq!(h.mode, "HDR");
        assert!(h.hdr_is_on);
    }

    #[test]
    fn a_dark_desktop_and_a_lock_screen_are_told_apart() {
        let dark = verdict(&report(false, true, 3, false));
        assert!(dark.message.contains("off or asleep"), "{}", dark.message);
        assert!(!dark.hdr_is_on);
        let locked = verdict(&report(false, true, 255, true));
        assert!(locked.message.contains("lock screen"), "{}", locked.message);
    }

    #[test]
    fn a_desktop_with_a_picture_blames_the_capture() {
        let h = verdict(&report(false, true, 255, false));
        assert!(
            h.message.contains("capture is what is failing"),
            "{}",
            h.message
        );
        assert!(!h.hdr_is_on);
    }

    #[test]
    fn only_a_black_picture_is_asked_about_and_every_ask_ends_in_an_answer() {
        assert!(is_black_picture("The PC is sending a black picture: the connection and the video are healthy, but every frame is blank. Asking the PC why…"));
        for other in [
            "Video cannot be decoded: decode failed (-12909). Try restarting the stream or selecting H.264 in Settings.",
            "Video stopped arriving from the PC. Try restarting the stream or lowering Quality.",
            "Connected, but no video has arrived from the PC. Check its display, or restart the stream.",
        ] {
            assert!(!is_black_picture(other), "{other}");
        }
        assert!(can_explain("windows") && can_explain(""));
        assert!(!can_explain("linux") && !can_explain("macOS"));
        // What a Linux or Mac host answers: nothing to go on.
        let elsewhere = serde_json::json!({
            "windows": {"supported": false},
            "advanced_color": {"supported": false},
            "sunshine": {},
        });
        assert_eq!(verdict(&elsewhere).message, "");
        for os in ["windows", "linux"] {
            let h = no_answer(os);
            assert!(!h.message.is_empty() && !h.hdr_is_on, "{os}");
            assert!(!h.message.contains("Asking"), "{}", h.message);
        }
    }

    #[test]
    fn a_report_that_says_nothing_yields_no_verdict() {
        assert_eq!(verdict(&serde_json::json!({})), Help::default());
        // A PC that could not sample its desktop still names what it knows.
        let mut r = report(false, false, 0, false);
        r["windows"]["desktop"] = Value::Null;
        assert!(verdict(&r).message.contains("No monitor"));
    }
}
