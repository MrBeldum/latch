//! The PC's virtual display, when it has one.
//!
//! A PC with no monitor shows its desktop on the Virtual Display Driver
//! (github.com/VirtualDrivers), and that driver only switches to the sizes
//! listed in its `vdd_settings.xml`. The engine asks it for the Mac's own
//! screen size at every connect, so the file has to list every size a Mac
//! can ask for; a size it lacks leaves the desktop at whatever it was, and
//! the Mac gets that picture scaled up. Setup writes the sizes in, keeping
//! everything else the file says, and the service reports when any are
//! missing so the setup card can say so.

use latch_core::screens::stream_modes;

/// The driver's name in Device Manager.
pub const DEVICE: &str = "Virtual Display Driver";
/// Where the driver reads its settings unless the registry points elsewhere.
pub const DEFAULT_DIR: &str = r"C:\VirtualDisplayDriver";
pub const SETTINGS: &str = "vdd_settings.xml";
const REG_KEY: &str = r"HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver";
/// Listed with every size: the default frame rate and ProMotion's.
pub const REFRESH_RATES: [u32; 2] = [60, 120];

/// Sunshine's stable ID for the Virtual Display Driver, including an
/// inactive display. Use the latest device list, so a removed display
/// from an older list is never selected.
// ponytail: Sunshine logs its display IDs; use an enumeration API if it gains one.
pub fn output_in(log: &str) -> Option<String> {
    let (_, list) = log.rsplit_once("Currently available ")?;
    let json = list
        .strip_prefix("display devices:")
        .or_else(|| list.strip_prefix("devices:"))?;
    let devices = serde_json::Deserializer::from_str(json.trim_start())
        .into_iter::<Vec<serde_json::Value>>()
        .next()?
        .ok()?;
    devices.iter().find_map(|d| {
        let vdd = d["friendly_name"].as_str() == Some("VDD by MTT")
            || (d["edid"]["manufacturer_id"].as_str() == Some("MTT")
                && d["edid"]["product_code"].as_str() == Some("1337"));
        d["device_id"]
            .as_str()
            .filter(|id| vdd && !id.is_empty())
            .map(String::from)
    })
}

/// The engine key that picks the display a stream captures.
const OUTPUT: &str = "output_name";
/// The engine key for what happens to the other displays during a stream.
const DD_OPTION: &str = "dd_configuration_option";
/// The streamed display is the only one on while a stream runs.
const ONLY_DISPLAY: &str = "ensure_only_display";
/// Where the service keeps the display ID it put in [`OUTPUT`], so that ID
/// still counts as automatic when the driver comes back with another one.
const PICKED: &str = "capture-display";

/// Which display the engine captures, as far as Latch manages it on Windows.
#[derive(Debug, PartialEq)]
pub struct Capture {
    /// Engine settings to change, with their new values; empty when none need to.
    pub conf: Vec<(&'static str, String)>,
    /// The display ID Latch has set once `conf` is in force.
    pub picked: Option<String>,
}

impl Capture {
    /// `conf` as the engine-key pairs [`crate::streamer`] writes.
    pub fn pairs(&self) -> Vec<(&str, &str)> {
        self.conf.iter().map(|(k, v)| (*k, v.as_str())).collect()
    }
}

/// With capture on automatic, Latch prefers the Virtual Display Driver even
/// while a physical monitor is connected. Automatic is no display chosen
/// in the engine (`output_name` empty), the driver's own ID `vdd`, or the ID
/// Latch set before (`picked`): a reinstalled driver can come back with a
/// new ID, and capture follows it. Any other display is the owner's choice
/// and is kept; the engine settings Latch changed for its own pick go back.
pub fn capture(current: &serde_json::Value, vdd: Option<&str>, picked: Option<&str>) -> Capture {
    let selected = current[OUTPUT].as_str().unwrap_or_default();
    let automatic = selected.is_empty() || Some(selected) == vdd || Some(selected) == picked;
    let mut conf = Vec::new();
    let picked = match (automatic, vdd) {
        (true, Some(id)) => {
            conf.push((OUTPUT, id.to_string()));
            // Games already on the physical monitor must move onto the
            // streamed display too. The engine restores the physical
            // displays when the stream disconnects.
            conf.push((DD_OPTION, ONLY_DISPLAY.to_string()));
            Some(id.to_string())
        }
        // No device list in the log tail (a long stream pushes it out) or
        // no driver right now: what is set stays.
        (true, None) => picked.map(String::from),
        (false, _) => {
            if picked.is_some() && current[DD_OPTION] == ONLY_DISPLAY {
                let default = crate::setup::ENGINE_CONF
                    .iter()
                    .find(|(k, _)| *k == DD_OPTION)
                    .map_or("ensure_active", |(_, v)| *v);
                conf.push((DD_OPTION, default.to_string()));
            }
            None
        }
    };
    conf.retain(|(k, v)| current[*k].as_str() != Some(v.as_str()));
    Capture { conf, picked }
}

/// Keep the engine capturing the virtual display while capture is on
/// automatic (see [`capture`]); the service calls this every 30 seconds on
/// Windows, since the driver can be installed, or get a new ID, at any
/// time. Nothing is asked of the engine on a PC that has never had the
/// driver. Returns a line for the service log when settings changed.
pub fn follow(
    api: &crate::streamer::Api,
    install: &crate::streamer::Install,
    data_dir: &std::path::Path,
) -> anyhow::Result<Option<String>> {
    let vdd = crate::streamer::log_text(install).and_then(|log| output_in(&log));
    let file = data_dir.join(PICKED);
    let picked = std::fs::read_to_string(&file)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    if vdd.is_none() && picked.is_none() {
        return Ok(None);
    }
    let Some(capture) =
        api.follow_virtual_display(Some(install), vdd.as_deref(), picked.as_deref())?
    else {
        return Ok(None); // Someone may be watching: the display stays until they are done.
    };
    if capture.picked != picked {
        let saved = match &capture.picked {
            Some(id) => std::fs::write(&file, id),
            None => std::fs::remove_file(&file),
        };
        if let Err(e) = saved {
            tracing::warn!("could not save the capture display: {e}");
        }
    }
    Ok((!capture.conf.is_empty()).then(|| {
        let keys: Vec<String> = capture
            .conf
            .iter()
            .map(|(k, v)| format!("{k} = {v}"))
            .collect();
        format!(
            "capture display: {}; the streaming engine restarted",
            keys.join(", ")
        )
    }))
}

/// Every size as a PowerShell array of `@(width,height)` pairs.
pub fn modes_ps() -> String {
    stream_modes()
        .iter()
        .map(|(w, h)| format!("@({w},{h})"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// PowerShell that sets `$vddDevices` (the driver's working devices; none
/// when it is not installed), `$vddFile`, `$vddXml` (the parsed settings,
/// or null), `$vddParseFailed` and `$vddMissing` (the wanted sizes the
/// file does not list, as `@(width,height)` pairs).
fn locate_ps() -> String {
    format!(
        r#"$vddDevices = @(Get-PnpDevice -Class Display -ErrorAction SilentlyContinue | Where-Object {{ $_.FriendlyName -eq '{device}' -and $_.Status -eq 'OK' }})
$vddDir = (Get-ItemProperty '{reg}' -ErrorAction SilentlyContinue).VDDPATH
if (-not $vddDir) {{ $vddDir = '{dir}' }}
$vddFile = Join-Path $vddDir '{settings}'
$vddWanted = @({modes})
$vddListed = @{{}}
$vddXml = $null
$vddParseFailed = $false
if (Test-Path -LiteralPath $vddFile) {{
    try {{
        [xml]$vddXml = Get-Content -LiteralPath $vddFile -Raw
        foreach ($r in $vddXml.SelectNodes('/vdd_settings/resolutions/resolution')) {{ $vddListed["$($r.width)x$($r.height)"] = $true }}
    }} catch {{ $vddXml = $null; $vddParseFailed = $true }}
}}
$vddMissing = @()
foreach ($m in $vddWanted) {{ if (-not $vddListed["$($m[0])x$($m[1])"]) {{ $vddMissing += ,$m }} }}
"#,
        device = DEVICE,
        reg = REG_KEY,
        dir = DEFAULT_DIR,
        settings = SETTINGS,
        modes = modes_ps(),
    )
}

/// The setup step. Adds the missing sizes to the driver's settings (a
/// fresh, minimal file when there is none), then restarts the display so
/// the driver reads them; a file that will not parse is left alone and
/// said so. Nothing happens on a PC without the driver.
pub fn setup_ps() -> String {
    let rates = REFRESH_RATES
        .iter()
        .map(|r| format!(", @('refresh_rate', {r})"))
        .collect::<String>();
    format!(
        r#"Step "Listing the sizes a Mac can ask for on the virtual display"
{locate}if ($vddDevices.Count -eq 0) {{
    Write-Output "  no {device} on this PC; skipped"
}} elseif ($vddParseFailed) {{
    Write-Output "  $vddFile could not be read and is left alone"
}} elseif ($vddMissing.Count -eq 0) {{
    Write-Output "  every size is listed already"
}} else {{
    if (-not $vddXml) {{
        [xml]$vddXml = '<?xml version="1.0" encoding="utf-8"?><vdd_settings><monitors><count>1</count></monitors><gpu><friendlyname>default</friendlyname></gpu><resolutions></resolutions></vdd_settings>'
    }}
    $vddRoot = $vddXml.DocumentElement
    $vddList = $vddRoot.SelectSingleNode('resolutions')
    if (-not $vddList) {{ $vddList = $vddRoot.AppendChild($vddXml.CreateElement('resolutions')) }}
    foreach ($m in $vddMissing) {{
        $r = $vddXml.CreateElement('resolution')
        foreach ($pair in @(@('width', $m[0]), @('height', $m[1]){rates})) {{
            $e = $vddXml.CreateElement($pair[0])
            $e.InnerText = "$($pair[1])"
            [void]$r.AppendChild($e)
        }}
        [void]$vddList.AppendChild($r)
    }}
    New-Item -ItemType Directory -Force -Path $vddDir | Out-Null
    $vddWriterSettings = New-Object System.Xml.XmlWriterSettings
    $vddWriterSettings.Indent = $true
    $vddWriterSettings.Encoding = New-Object System.Text.UTF8Encoding($false)
    $vddWriter = [System.Xml.XmlWriter]::Create($vddFile, $vddWriterSettings)
    try {{ $vddXml.Save($vddWriter) }} finally {{ $vddWriter.Close() }}
    Write-Output "  added $($vddMissing.Count) sizes to $vddFile; restarting the virtual display"
    foreach ($d in $vddDevices) {{ pnputil /restart-device $d.InstanceId | Out-Null }}
}}
"#,
        locate = locate_ps(),
        device = DEVICE,
        rates = rates,
    )
}

/// Whether the driver lists every size: `None` on a PC without the driver
/// (or when the probe itself failed), `Some(false)` when setup has sizes
/// to add.
pub fn probe() -> Option<bool> {
    #[cfg(windows)]
    {
        let script = format!(
            "{}if ($vddDevices.Count -eq 0) {{ 'none' }} elseif ($vddMissing.Count -eq 0) {{ 'ok' }} else {{ 'missing' }}",
            locate_ps()
        );
        match crate::setup::powershell(&script) {
            Ok(o) => parse_probe(&o),
            Err(e) => {
                tracing::debug!("virtual display probe: {e:#}");
                None
            }
        }
    }
    #[cfg(not(windows))]
    None
}

#[cfg(any(windows, test))]
fn parse_probe(out: &str) -> Option<bool> {
    match out.trim() {
        "ok" => Some(true),
        "missing" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_uses_the_virtual_display_from_the_latest_device_list() {
        let list = r#"[
            {"device_id":"physical", "friendly_name":"27E1QA", "info":{"primary":true}},
            {"device_id":"virtual", "friendly_name":"VDD by MTT", "info":null}
        ]"#;
        let log = format!("Info: Currently available display devices:\n{list}\nInfo: Ready");
        assert_eq!(output_in(&log).as_deref(), Some("virtual"));
        let log = format!("{log}\nInfo: Currently available devices:\n[]\nInfo: Ready");
        assert_eq!(output_in(&log), None, "the old virtual display is gone");
        assert_eq!(output_in("Currently available devices:\n["), None);
        assert_eq!(output_in("no devices reported"), None);
    }

    #[test]
    fn automatic_capture_selects_the_virtual_display_and_keeps_the_owners_choice() {
        let fresh =
            serde_json::json!({"encoder":"amdvce", "dd_configuration_option":"ensure_active"});
        let first = capture(&fresh, Some("vdd-a"), None);
        assert_eq!(
            first.conf,
            [
                ("output_name", "vdd-a".to_string()),
                ("dd_configuration_option", "ensure_only_display".to_string())
            ]
        );
        assert_eq!(first.picked.as_deref(), Some("vdd-a"));

        // In force: nothing to change, with or without the device list.
        let set = serde_json::json!({"output_name":"vdd-a", "dd_configuration_option":"ensure_only_display"});
        let same = capture(&set, Some("vdd-a"), Some("vdd-a"));
        assert!(same.conf.is_empty(), "{same:?}");
        assert_eq!(same.picked.as_deref(), Some("vdd-a"));
        let unlisted = capture(&set, None, Some("vdd-a"));
        assert!(unlisted.conf.is_empty(), "{unlisted:?}");
        assert_eq!(unlisted.picked.as_deref(), Some("vdd-a"));

        // Set by an earlier build that kept no record: still Latch's.
        let adopted = capture(&set, Some("vdd-a"), None);
        assert!(adopted.conf.is_empty());
        assert_eq!(adopted.picked.as_deref(), Some("vdd-a"));

        // A reinstalled driver with a new ID: capture follows it.
        let moved = capture(&set, Some("vdd-b"), Some("vdd-a"));
        assert_eq!(moved.conf, [("output_name", "vdd-b".to_string())]);
        assert_eq!(moved.picked.as_deref(), Some("vdd-b"));

        // No driver and no pick: nothing is touched.
        let none = capture(&fresh, None, None);
        assert_eq!(
            none,
            Capture {
                conf: vec![],
                picked: None
            }
        );
    }

    #[test]
    fn an_explicit_display_is_kept_and_latchs_own_change_goes_back() {
        let chosen = serde_json::json!({"output_name":"monitor", "dd_configuration_option":"ensure_only_display"});
        // The owner replaced Latch's pick: only the option Latch set goes back.
        let replaced = capture(&chosen, Some("vdd-a"), Some("vdd-a"));
        assert_eq!(
            replaced.conf,
            [("dd_configuration_option", "ensure_active".to_string())]
        );
        assert_eq!(replaced.picked, None);
        // Latch never picked a display here: the owner's settings stay.
        let theirs = capture(&chosen, Some("vdd-a"), None);
        assert!(theirs.conf.is_empty(), "{theirs:?}");
        assert_eq!(theirs.picked, None);
        let primary = serde_json::json!({"output_name":"monitor", "dd_configuration_option":"ensure_primary"});
        assert!(capture(&primary, Some("vdd-a"), Some("vdd-a"))
            .conf
            .is_empty());
    }

    #[test]
    fn the_step_lists_every_mode_keeps_the_file_and_restarts_only_on_change() {
        let s = setup_ps();
        for (w, h) in stream_modes() {
            assert!(s.contains(&format!("@({w},{h})")), "{w}x{h} is listed");
        }
        assert!(
            s.contains("@(3024,1964)"),
            "a MacBook Pro 14\" is listed:\n{s}"
        );
        assert!(s.contains("@('refresh_rate', 60), @('refresh_rate', 120)"));
        assert!(s.contains(&format!("FriendlyName -eq '{DEVICE}'")));
        assert!(s.contains("VDDPATH"), "a relocated settings file is found");
        assert!(s.contains(DEFAULT_DIR));
        assert!(
            s.contains("$vddMissing += ,$m"),
            "pairs stay pairs when collected:\n{s}"
        );
        let restart = s.find("pnputil /restart-device").expect("restart");
        let save = s.find("$vddXml.Save(").expect("save");
        assert!(
            save < restart,
            "the file is written before the display restarts"
        );
        assert!(
            s.contains("is left alone"),
            "a file that will not parse is never overwritten"
        );
        assert!(
            s.contains("UTF8Encoding($false)"),
            "no byte-order mark for the driver's parser"
        );
    }

    #[test]
    fn the_probe_answer_is_three_words() {
        assert_eq!(parse_probe("ok\r\n"), Some(true));
        assert_eq!(parse_probe(" missing "), Some(false));
        assert_eq!(parse_probe("none"), None);
        assert_eq!(parse_probe(""), None);
        assert_eq!(parse_probe("Get-PnpDevice : not recognized"), None);
    }
}
