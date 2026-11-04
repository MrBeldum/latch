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
pub(crate) const REG_KEY: &str = r"HKLM:\SOFTWARE\MikeTheTech\VirtualDisplayDriver";
/// Where setup records the driver's `VDDPATH` from before it pointed it at
/// Latch's protected copy, so uninstall can put it back.
pub(crate) const RECORD_KEY: &str = r"HKLM:\SOFTWARE\Latch\Display";
/// Common display timings; custom timings in the driver's file are preserved.
pub const REFRESH_RATES: [u32; 7] = [60, 90, 120, 144, 165, 240, 360];

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
$vddOldPath = (Get-ItemProperty '{reg}' -ErrorAction SilentlyContinue).VDDPATH
$vddDir = $vddOldPath
if (-not $vddDir) {{ $vddDir = '{dir}' }}
$vddFile = Join-Path $vddDir '{settings}'
$vddWanted = @({modes})
$vddListed = @{{}}
$vddXml = $null
$vddParseFailed = $false
if (Test-Path -LiteralPath $vddFile) {{
    try {{
        [xml]$vddXml = Get-Content -LiteralPath $vddFile -Raw
        foreach ($r in $vddXml.SelectNodes('/vdd_settings/resolutions/resolution')) {{ $vddListed["$($r.width)x$($r.height)"] = $r }}
    }} catch {{ $vddXml = $null; $vddParseFailed = $true }}
}}
$vddRates = @({rates})
function Vdd-Rates($w, $h) {{
    if (($w -eq 1920 -and $h -eq 1080) -or ($w -eq 2560 -and $h -eq 1440)) {{ return $vddRates }}
    return @(60, 120)
}}
$vddMissing = @()
foreach ($m in $vddWanted) {{
    $r = $vddListed["$($m[0])x$($m[1])"]
    if (-not $r -or @((Vdd-Rates $m[0] $m[1]) | Where-Object {{ $_ -notin @($r.refresh_rate) -and $_ -notin @($vddXml.vdd_settings.global.g_refresh_rate) }}).Count -gt 0) {{ $vddMissing += ,$m }}
}}

"#,
        device = DEVICE,
        reg = REG_KEY,
        dir = DEFAULT_DIR,
        settings = SETTINGS,
        modes = modes_ps(),
        rates = REFRESH_RATES
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    )
}

/// The setup step. Adds the missing sizes to the driver's settings (a
/// fresh, minimal file when there is none), then restarts the display so
/// the driver reads them; a file that will not parse is left alone and
/// said so. Nothing happens on a PC without the driver.
pub fn setup_ps() -> String {
    format!(
        r#"Step "Listing virtual display resolutions and refresh rates"
$vddModesUnchanged = $false
$vddRelocated = $false
$vddWriteStarted = $false
$vddPreviousBytes = $null
$vddErrorPreference = $ErrorActionPreference
function Restart-Vdd {{
    foreach ($d in $vddDevices) {{
        pnputil /restart-device $d.InstanceId | Out-Null
        if ($LASTEXITCODE -ne 0) {{ throw "Virtual display restart exited with $LASTEXITCODE" }}
    }}
}}
function Wait-VddMonitor {{
    foreach ($attempt in 1..60) {{
        Start-Sleep -Milliseconds 250
        if (@(Get-PnpDevice -Class Monitor -ErrorAction SilentlyContinue | Where-Object {{ $_.InstanceId -like 'DISPLAY\MTT1337\*' -and $_.Status -eq 'OK' }}).Count -gt 0) {{ return $true }}
    }}
    return $false
}}
try {{
$ErrorActionPreference = 'Stop'
{locate}if ($vddDevices.Count -eq 0) {{
    Write-Output "  no {device} on this PC; skipped"
}} elseif ($vddParseFailed) {{
    throw "$vddFile could not be read and is left alone"
}} elseif ($vddMissing.Count -eq 0) {{
    Write-Output "  every resolution and refresh rate is listed already"
}} else {{
    if (-not $vddXml) {{
        [xml]$vddXml = '<?xml version="1.0" encoding="utf-8"?><vdd_settings><monitors><count>1</count></monitors><gpu><friendlyname>default</friendlyname></gpu><resolutions></resolutions></vdd_settings>'
    }}
    $vddRoot = $vddXml.DocumentElement
    $vddList = $vddRoot.SelectSingleNode('resolutions')
    if (-not $vddList) {{ $vddList = $vddRoot.AppendChild($vddXml.CreateElement('resolutions')) }}
    foreach ($m in $vddMissing) {{
        $r = $vddListed["$($m[0])x$($m[1])"]
        if (-not $r) {{
            $r = $vddXml.CreateElement('resolution')
            foreach ($pair in @(@('width', $m[0]), @('height', $m[1]))) {{
                $e = $vddXml.CreateElement($pair[0])
                $e.InnerText = "$($pair[1])"
                [void]$r.AppendChild($e)
            }}
            [void]$vddList.AppendChild($r)
        }}
        foreach ($hz in (Vdd-Rates $m[0] $m[1])) {{
            if ($hz -notin @($r.refresh_rate) -and $hz -notin @($vddXml.vdd_settings.global.g_refresh_rate)) {{
                $e = $vddXml.CreateElement('refresh_rate')
                $e.InnerText = "$hz"
                [void]$r.AppendChild($e)
            }}
        }}
    }}
    # Never let an elevated XML write follow a user's junction or symlink.
    try {{
        Assert-ProtectedTree $vddDir
        if (Test-Path -LiteralPath $vddFile) {{ Assert-Protected $vddFile }}
    }} catch {{
        # The driver's installer often leaves C:\VirtualDisplayDriver writable.
        # Keep that file untouched; publish settings in a protected directory.
        Write-Output "  using protected settings instead of $vddFile ($_)"
        $vddDir = $env:ProgramFiles
        foreach ($part in @('Latch', 'Display')) {{
            Assert-ProtectedTree $vddDir
            $vddDir = Join-Path $vddDir $part
            if (-not (Test-Path -LiteralPath $vddDir)) {{ New-Item -ItemType Directory -Path $vddDir | Out-Null }}
        }}
        Assert-ProtectedTree $vddDir
        $vddFile = Join-Path $vddDir '{settings}'
        if (Test-Path -LiteralPath $vddFile) {{ Assert-Protected $vddFile }}
        $vddRelocated = $true
    }}
    $vddPreviousBytes = $null
    if (Test-Path -LiteralPath $vddFile) {{ $vddPreviousBytes = [IO.File]::ReadAllBytes($vddFile) }}
    $vddHadMonitor = @(Get-PnpDevice -Class Monitor -ErrorAction SilentlyContinue | Where-Object {{ $_.InstanceId -like 'DISPLAY\MTT1337\*' -and $_.Status -eq 'OK' }}).Count -gt 0
    $vddWriterSettings = New-Object System.Xml.XmlWriterSettings
    $vddWriterSettings.Indent = $true
    $vddWriterSettings.Encoding = New-Object System.Text.UTF8Encoding($false)
    $vddWriteStarted = $true
    $vddWriter = [System.Xml.XmlWriter]::Create($vddFile, $vddWriterSettings)
    try {{ $vddXml.Save($vddWriter) }} finally {{ $vddWriter.Close() }}
    if ($vddRelocated) {{
        if (-not (Test-Path -LiteralPath '{reg}')) {{ New-Item -Path '{reg}' -Force | Out-Null }}
        New-ItemProperty -Path '{reg}' -Name 'VDDPATH' -Value $vddDir -PropertyType String -Force | Out-Null
    }}
    Write-Output "  updated $($vddMissing.Count) resolutions to $vddFile; restarting the virtual display"
    Restart-Vdd
    if (-not (Wait-VddMonitor)) {{
        throw 'Virtual display monitor did not return after the settings change'
    }}
    # Record only a successful relocation. A failed attempt must leave no
    # uninstall record pointing at a copy which has already been removed.
    if ($vddRelocated -and -not (Test-Path -LiteralPath '{record}')) {{
        New-Item -Path '{record}' -Force | Out-Null
        $vddPrevious = 'absent'
        if ($vddOldPath) {{ $vddPrevious = $vddOldPath }}
        New-ItemProperty -Path '{record}' -Name 'PreviousVddPath' -Value $vddPrevious -PropertyType String -Force | Out-Null
    }}
}}
}} catch {{
    $vddModesUnchanged = $true
    Write-Output "  WARNING: virtual display modes unchanged: $_"
    if ($vddWriteStarted) {{
        try {{
            if ($null -ne $vddPreviousBytes) {{ [IO.File]::WriteAllBytes($vddFile, $vddPreviousBytes) }}
            else {{ Remove-Item -LiteralPath $vddFile -Force -ErrorAction SilentlyContinue }}
            if ($vddRelocated) {{
                if ($vddOldPath) {{ Set-ItemProperty -Path '{reg}' -Name VDDPATH -Value $vddOldPath }}
                else {{ Remove-ItemProperty -Path '{reg}' -Name VDDPATH -ErrorAction SilentlyContinue }}
            }}
            Restart-Vdd
            if ($vddHadMonitor -and -not (Wait-VddMonitor)) {{ throw 'The previous virtual display monitor did not return' }}
            Write-Output '  previous virtual display settings restored'
        }} catch {{ Write-Output "  WARNING: virtual display restore: $_"; $setupFailed = $true }}
    }}
    if ($vddRelocated -and (Get-ItemProperty '{reg}' -ErrorAction SilentlyContinue).VDDPATH -ne $vddDir) {{
        try {{
            if ($null -eq $vddPreviousBytes) {{ Assert-ProtectedTree $vddDir; Remove-Item -LiteralPath $vddDir -Recurse -Force }}
            if (Test-Path -LiteralPath '{record}') {{ Remove-Item -LiteralPath '{record}' -Force }}
        }} catch {{ Write-Output "  WARNING: virtual display cleanup: $_" }}
    }}
}} finally {{ $ErrorActionPreference = $vddErrorPreference }}

"#,
        locate = locate_ps(),
        device = DEVICE,
        settings = SETTINGS,
        reg = REG_KEY,
        record = RECORD_KEY,
    )
}

/// Uninstall's part: if setup pointed the driver at Latch's protected copy,
/// point it back where it was, restart it so it rereads its settings, and
/// remove the copy. Expects `$latchDir`, `$setupFailed` and
/// `Assert-ProtectedTree` from the uninstall script.
#[cfg(any(windows, test))]
pub(crate) fn uninstall_ps() -> String {
    format!(
        r#"Step "Pointing the virtual display back at its own settings"
$vddOurs = Join-Path $latchDir 'Display'
$vddNow = (Get-ItemProperty '{reg}' -ErrorAction SilentlyContinue).VDDPATH
if ($vddNow -and ($vddNow.TrimEnd('') -ieq $vddOurs)) {{
    $vddPrevious = (Get-ItemProperty '{record}' -ErrorAction SilentlyContinue).PreviousVddPath
    if ($vddPrevious -and $vddPrevious -ne 'absent') {{ Set-ItemProperty -Path '{reg}' -Name VDDPATH -Value $vddPrevious }}
    else {{ Remove-ItemProperty -Path '{reg}' -Name VDDPATH -ErrorAction SilentlyContinue }}
    foreach ($d in @(Get-PnpDevice -Class Display -ErrorAction SilentlyContinue | Where-Object {{ $_.FriendlyName -eq '{device}' -and $_.Status -eq 'OK' }})) {{
        pnputil /restart-device $d.InstanceId | Out-Null
    }}
}}
if (Test-Path -LiteralPath $vddOurs) {{
    try {{ Assert-ProtectedTree $vddOurs; Remove-Item -LiteralPath $vddOurs -Recurse -Force -ErrorAction Stop }} catch {{ Write-Output "  display settings folder: $_"; $setupFailed = $true }}
}}
if (Test-Path -LiteralPath '{record}') {{ Remove-Item -LiteralPath '{record}' -Force }}
"#,
        reg = REG_KEY,
        record = RECORD_KEY,
        device = DEVICE,
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
        if let Ok(path) = std::env::var("LATCH_DUMP_VDD_SCRIPT") {
            std::fs::write(
                path,
                format!(
                    "{}\nfunction Step($label) {{ Write-Output $label }}\n{}",
                    include_str!("../../../scripts/setup-protection.ps1"),
                    s
                ),
            )
            .unwrap();
        }
        for (w, h) in stream_modes() {
            assert!(s.contains(&format!("@({w},{h})")), "{w}x{h} is listed");
        }
        assert!(
            s.contains("@(3024,1964)"),
            "a MacBook Pro 14\" is listed:\n{s}"
        );
        for hz in [90, 144, 165, 240, 360] {
            assert!(REFRESH_RATES.contains(&hz));
        }
        let modes = modes_ps();
        for (w, h) in stream_modes() {
            assert_eq!(modes.matches(&format!("@({w},{h})")).count(), 1);
        }
        assert!(s.contains("@(7680,4320)"));
        assert!(s.contains("$hz -notin @($r.refresh_rate)"));
        assert!(s.contains(&format!("FriendlyName -eq '{DEVICE}'")));
        assert!(s.contains("VDDPATH"), "a relocated settings file is found");
        assert!(s.contains(DEFAULT_DIR));
        assert!(
            s.contains("$vddMissing += ,$m"),
            "pairs stay pairs when collected:\n{s}"
        );
        let restart = s.find("    Restart-Vdd\n").expect("restart");
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
