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
