//! The one administrator step, and the per-user autostart.
//!
//! Everything that needs elevation is done by a single PowerShell script
//! behind a single UAC prompt: install the bundled Sunshine if none is
//! present, give it the login Latch will use, open the control port to the
//! tailnet, list the Mac screen sizes on the virtual display, turn Fast
//! Startup off and arm the network card for Wake-on-LAN.
//! Each step logs and carries on, so one failure does not undo the others;
//! the service re-probes afterwards and the setup card says what is still
//! missing.

#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use latch_core::config::data_dir;
use latch_core::CONTROL_PORT;
use std::path::{Path, PathBuf};

pub struct Plan<'a> {
    pub exe: &'a Path,
    pub install_engine: bool,
    /// Copy old engine state, skip `--creds`, uninstall the MSI after prove.
    pub migrate: bool,
    /// Copy+verify only; never msiexec.
    pub dry_run: bool,
    pub sunshine_user: &'a str,
    pub sunshine_pass: &'a str,
    /// Adapter name and interface description from the wake probe; empty
    /// when unknown, which skips that step.
    pub adapter: &'a str,
    pub adapter_description: &'a str,
}

/// Log the elevated script writes; the setup card shows its tail.
pub fn log_path() -> Option<PathBuf> {
    data_dir().ok().map(|d| d.join("setup.log"))
}

/// The engine archive shipped beside `latch-host.exe`. The lite archive
/// is a plain zip: no Add/Remove Programs entry, no Start Menu shortcut and
/// no service or firewall rule of its own, so Latch names all three.
pub const ENGINE_ZIP: &str = "Sunshine-Windows-AMD64-lite.zip";

/// The upstream release the engine is pinned to, and the SHA-256 GitHub
/// publishes for `ENGINE_ZIP` on it. Both the bundled copy and a download
/// are checked against this digest before anything is unpacked.
pub const ENGINE_TAG: &str = "v2026.906.222525";
pub const ENGINE_SHA256: &str = "50f4123dd15a6817513589c912581260c34cc4a58e9a5f3071ebac3d92c94c15";

/// The single folder every path in `ENGINE_ZIP` sits under.
pub const ENGINE_ZIP_ROOT: &str = "Sunshine";

/// What a usable engine directory must contain, relative to its root. The
/// list is checked twice: once on the unpacked staging tree, and again on
/// the copy in `ENGINE_DIR`, because the copy is what the service points at.
pub const ENGINE_REQUIRED: [&str; 2] = ["sunshine.exe", r"tools\sunshinesvc.exe"];

/// The service Latch registers, and the name it shows under. The engine's
/// service wrapper is `SERVICE_WIN32_OWN_PROCESS`, for which Windows ignores
/// the name the process passes to `StartServiceCtrlDispatcher`, so the SCM
/// name is Latch's to pick. `UPSTREAM_SERVICE` is the name that wrapper
/// was compiled with, kept only as the name the script falls back *to*
/// after proving `SERVICE` will not start.
pub const SERVICE: &str = "LatchStream";
pub const SERVICE_DISPLAY: &str = "Latch Streaming";
pub const UPSTREAM_SERVICE: &str = "SunshineService";

pub const TCP_RULE: &str = "Latch Streaming TCP";
pub const UDP_RULE: &str = "Latch Streaming UDP";

/// Keys written into `config\sunshine.conf` before the engine first starts.
/// From Sunshine v2026.906.222525 (`cb72dff`): `system_tray` is the tray
/// *and* desktop toasts (there is no separate toast key); `origin_web_ui_allowed
/// = pc` is localhost-only Web UI. `bind_address` is not set: it binds every
/// socket, including GameStream.
pub const ENGINE_CONF: &[(&str, &str)] = &[
    ("system_tray", "disabled"),
    ("origin_web_ui_allowed", "pc"),
    ("dd_configuration_option", "ensure_active"),
    ("dd_resolution_option", "auto"),
    ("dd_refresh_rate_option", "auto"),
    ("dd_config_revert_on_disconnect", "enabled"),
    ("max_bitrate", "0"),
    ("minimum_fps_target", "60"),
    // Constant bitrate. Sunshine's AMD default is vbr_latency, which
    // swings the received rate with scene complexity even on a fast path.
    ("fec_percentage", "20"),
    ("packetsize", "1184"),
    ("amd_rc", "cbr"),
    ("amd_enforce_hrd", "enabled"),
    ("amd_quality", "speed"),
    ("amd_usage", "ultralowlatency"),
    ("amd_preanalysis", "disabled"),
    ("amd_vbaq", "disabled"),
    ("nvenc_twopass", "quarter_res"),
    ("nvenc_vbv_increase", "0"),
    ("vaapi_rc", "cbr"),
    ("vaapi_strict_rc_buffer", "enabled"),
    ("vk_rc_mode", "2"),
    // Software encode (VPS, no GPU): ultrafast + every core, or 1080p60
    // drops to ~30 fps and a still desktop sits at a few hundred kbps.
    ("sw_preset", "ultrafast"),
    ("sw_tune", "zerolatency"),
    ("min_threads", "4"),
    ("vt_realtime", "enabled"),
];

/// What Linux changes in [`ENGINE_CONF`]. Latch's Linux hosts are VPS
/// desktops that encode in software.
///
/// Sunshine's X11 capture hands the encoder a picture every frame interval
/// whether or not anything moved, so a frame-rate floor above the stream's
/// rate can only re-encode the last picture: whenever a frame is done in
/// under 16.7 ms (a still desktop on an idle core), a 60 fps floor encodes
/// the same picture again before the next one arrives, up to doubling the
/// work under a 30 fps stream. 0 is the engine's own default, half the
/// stream's rate, which periodic capture never falls below.
///
/// The viewer asks a small CPU for 30 fps (see `streamer::max_fps`): on a
/// two-core Ampere VPS Sunshine took 136% CPU for a still desktop at 60 fps
/// and 78% for a busier one at 30. That leaves room for superfast: on an Ampere core it took 17.1 ms a
/// 1080p frame of scrolling text against ultrafast's 14.4 ms, and at
/// 20 Mbps its keyframes came out at QP 21 instead of 38 (Y PSNR 35.1 dB
/// against 31.8): text stays legible after every keyframe the viewer asks
/// for, instead of smearing until the next frames repair it.
pub const LINUX_ENGINE_CONF: &[(&str, &str)] =
    &[("minimum_fps_target", "0"), ("sw_preset", "superfast")];

/// [`ENGINE_CONF`], with [`LINUX_ENGINE_CONF`] in place when `linux`.
pub fn engine_conf_for(linux: bool) -> Vec<(&'static str, &'static str)> {
    ENGINE_CONF
        .iter()
        .map(|&(k, v)| {
            let linux_value = LINUX_ENGINE_CONF
                .iter()
                .find(|(n, _)| linux && *n == k)
                .map(|&(_, v)| v);
            (k, linux_value.unwrap_or(v))
        })
        .collect()
}

/// The engine keys for this machine's platform.
pub fn engine_conf() -> Vec<(&'static str, &'static str)> {
    engine_conf_for(cfg!(target_os = "linux"))
}

/// The only streamable app Latch launches. Upstream ships extra entries
/// (Steam, a low-res desktop) that would show as a second product.
pub const DESKTOP_APPS_JSON: &str = r#"{
  "apps": [
    {
      "name": "Desktop",
      "image-path": "desktop.png"
    }
  ]
}
"#;

fn conf_key(line: &str) -> Option<&str> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let (k, _) = line.split_once('=')?;
    let k = k.trim();
    (!k.is_empty()).then_some(k)
}

/// Upsert this platform's [`engine_conf`] into an existing sunshine.conf.
/// Known keys are replaced in place (first occurrence wins, later copies of
/// those keys dropped); everything else is kept. Sunshine's parser uses
/// `emplace`, so a naive append would leave an old `system_tray = enabled`
/// in force.
pub fn conceal_conf(existing: &str) -> String {
    conceal_conf_with(existing, &engine_conf())
}

fn conceal_conf_with(existing: &str, conf: &[(&str, &str)]) -> String {
    let existing = existing.strip_prefix('\u{feff}').unwrap_or(existing);
    let mut seen = vec![false; conf.len()];
    let mut out = String::new();
    if !existing.trim().is_empty() {
        for line in existing.lines() {
            match conf_key(line).and_then(|k| conf.iter().position(|(n, _)| *n == k)) {
                Some(i) if seen[i] => continue,
                Some(i) => {
                    seen[i] = true;
                    let (n, v) = conf[i];
                    out.push_str(n);
                    out.push_str(" = ");
                    out.push_str(v);
                    out.push('\n');
                }
                None => {
                    out.push_str(line);
                    out.push('\n');
                }
            }
        }
    }
    for (i, (n, v)) in conf.iter().enumerate() {
        if !seen[i] {
            out.push_str(n);
            out.push_str(" = ");
            out.push_str(v);
            out.push('\n');
        }
    }
    out
}

fn conceal_ps() -> String {
    let pairs = ENGINE_CONF
        .iter()
        .map(|(k, v)| format!("        '{k}' = '{v}'"))
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        r#"function Write-EngineConf($dir) {{
    Step "Hiding the engine tray and restricting the web UI to this PC"
    $confDir = Join-Path $dir 'config'
    New-Item -ItemType Directory -Force -Path $confDir | Out-Null
    $conf = Join-Path $confDir 'sunshine.conf'
    $raw = ''
    if (Test-Path -LiteralPath $conf) {{ $raw = [IO.File]::ReadAllText($conf) }}
    if ($raw.Length -gt 0 -and [int][char]$raw[0] -eq 0xFEFF) {{ $raw = $raw.Substring(1) }}
    $want = [ordered]@{{
{pairs}
    }}
    $seen = @{{}}
    $out = @()
    $lines = @()
    if ($raw.Trim().Length -gt 0) {{ $lines = $raw -split '\r?\n' }}
    if ($lines.Count -gt 0 -and $lines[-1] -eq '') {{
        if ($lines.Count -eq 1) {{ $lines = @() }} else {{ $lines = $lines[0..($lines.Count-2)] }}
    }}
    foreach ($line in $lines) {{
        if ($line -match '^\s*#' -or $line -match '^\s*$') {{ $out += $line; continue }}
        $eq = $line.IndexOf('=')
        if ($eq -lt 1) {{ $out += $line; continue }}
        $k = $line.Substring(0, $eq).Trim()
        if ($want.Contains($k)) {{
            if ($seen.Contains($k)) {{ continue }}
            $seen[$k] = $true
            $out += "$k = $($want[$k])"
            continue
        }}
        $out += $line
    }}
    foreach ($k in @($want.Keys)) {{
        if (-not $seen.Contains($k)) {{ $out += "$k = $($want[$k])" }}
    }}
    $text = ($out -join "`n") + "`n"
    [IO.File]::WriteAllText($conf, $text, (New-Object System.Text.UTF8Encoding $false))
}}
"#
    )
}

pub fn bundled_engine(exe: &Path) -> bool {
    exe.parent().is_some_and(|d| d.join(ENGINE_ZIP).exists())
}

/// Directory the exe sits in, split on either slash so a Windows path
/// dumped from a Mac test still points at the bundled archive.
fn win_dir(path: &Path) -> String {
    let s = path.to_string_lossy();
    s.rsplit_once(['\\', '/'])
        .map(|(d, _)| d.to_string())
        .unwrap_or_else(|| s.into_owned())
}

pub fn script(p: &Plan<'_>) -> String {
    let q = |s: &str| s.replace('\'', "''");
    let exe_dir = win_dir(p.exe);
    // The folder an earlier version used is left out: the retire step has
    // moved it to ENGINE_DIR by now, and an engine that could not be moved
    // must not be mistaken for one to keep, with its pairing state missing.
    let dirs = crate::streamer::INSTALL_DIRS
        .iter()
        .filter(|(_, d)| *d != crate::legacy::WINDOWS_ENGINE_DIR)
        .map(|(_, d)| format!("'{d}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let required = ENGINE_REQUIRED
        .iter()
        .map(|f| format!("'{f}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let migrate_copy = if p.migrate {
        crate::migrate::copy_ps(crate::streamer::ENGINE_DIR)
    } else {
        String::new()
    };
    let stop_old = if p.install_engine {
        crate::migrate::stop_old_ps()
    } else {
        String::new()
    };
    let after_start = if p.migrate {
        crate::migrate::after_start_ps(p.dry_run)
    } else {
        String::new()
    };
    let install = if p.install_engine {
        format!(
            r#"$needFiles = (-not $dir) -or ($dir -ne '{engine}')
$svcUp = ((Get-Service -Name '{service}' -ErrorAction SilentlyContinue).Status -eq 'Running') -or ((Get-Service -Name '{upstream}' -ErrorAction SilentlyContinue).Status -eq 'Running')
if ($needFiles -or -not $svcUp) {{
    if ($needFiles) {{
    # Every mutating step below is critical: a half-installed engine that the
    # service points at is worse than no engine. 'Stop' turns the cmdlets that
    # report failure as a NON-terminating error - Expand-Archive, Copy-Item,
    # New-Item, New-Service - into throws the catch below can see. It is
    # restored in the finally so the best-effort steps after the engine (wake,
    # firewall) keep their own carry-on behaviour.
    $ErrorActionPreference = 'Stop'
    # Owned by this run alone. A fixed name under a TEMP that is shared
    # (LocalSystem's TEMP is C:\Windows\Temp) would let one run delete another
    # run's staging, and -Recurse -Force on a guessable shared path is a
    # footgun even when nothing else is running.
    $staging = Join-Path $env:TEMP ('latch-engine-' + [guid]::NewGuid().ToString('N'))
    New-Item -ItemType Directory -Force -Path $staging | Out-Null
    $zip = Join-Path '{exe_dir}' '{zip}'
    if (Test-Path $zip) {{
        Step "Installing the streaming engine that ships with Latch"
    }} else {{
        Step "Downloading the streaming engine {tag}"
        $json = curl.exe -sSL -A latch https://api.github.com/repos/{repo}/releases/tags/{tag}
        if ($LASTEXITCODE -ne 0) {{ throw "could not reach GitHub for engine release {tag} (curl exit $LASTEXITCODE)" }}
        $asset = ($json | ConvertFrom-Json).assets | Where-Object {{ $_.name -like '*lite.zip' }} | Select-Object -First 1
        if (-not $asset) {{ throw "no lite archive in engine release {tag}" }}
        $zip = Join-Path $staging 'engine.zip'
        curl.exe -sSL -A latch -o $zip $asset.browser_download_url
        if ($LASTEXITCODE -ne 0) {{ throw "downloading the engine archive failed (curl exit $LASTEXITCODE)" }}
    }}
    if (-not (Test-Path $zip)) {{ throw "the engine archive is missing: $zip" }}
    # An archive that is not byte-for-byte the pinned release is never
    # unpacked, whether it shipped beside the exe or was just downloaded.
    $got = (Get-FileHash -Algorithm SHA256 -LiteralPath $zip).Hash.ToLower()
    if ($got -ne '{sha}') {{ throw "engine archive is not {tag}: expected {sha}, got $got" }}
    Step "Unpacking the engine to {engine}"
    $unpack = Join-Path $staging 'unpack'
    Expand-Archive -LiteralPath $zip -DestinationPath $unpack -Force
    $root = Join-Path $unpack '{root}'
    foreach ($need in {required}) {{
        if (-not (Test-Path (Join-Path $root $need))) {{ throw "the engine archive did not unpack: {root}\$need is missing" }}
    }}
    New-Item -ItemType Directory -Force -Path '{engine}' | Out-Null
    # The archive has no config directory, so -Force overwrites program files
    # and leaves an existing engine config in place on a re-run.
    Copy-Item (Join-Path $root '*') '{engine}' -Recurse -Force
    # The DESTINATION is what the service will point at, so the destination is
    # what has to be checked. A copy that failed part way leaves a directory
    # that exists and a service binary that does not.
    foreach ($need in {required}) {{
        if (-not (Test-Path (Join-Path '{engine}' $need))) {{ throw "the engine did not copy to {engine}: $need is missing" }}
    }}
    {restore_state}$dir = '{engine}'
    }}
    if (-not $dir) {{ $dir = '{engine}' }}
    {migrate_copy}Write-EngineConf $dir
    {brand_new}{stop_old}Step "Registering the {display} service"
    # The engine's service wrapper is SERVICE_WIN32_OWN_PROCESS, so Windows
    # ignores the name it hands StartServiceCtrlDispatcher and '{service}'
    # should dispatch fine. That is proven here rather than assumed: if the
    # service will not reach Running it is removed and re-registered under
    # '{upstream}', the name the wrapper was built with, still displayed as
    # "{display}". An existing '{upstream}' belongs to an engine Latch did
    # not install and is never touched - migrating that one is a later step.
    $svcBin = '"' + (Join-Path $dir 'tools\sunshinesvc.exe') + '"'
    $svc = '{service}'
    if (Get-Service -Name $svc -ErrorAction SilentlyContinue) {{
        Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue
        sc.exe delete $svc | Out-Null
        if ($LASTEXITCODE -ne 0) {{ throw "could not remove the existing $svc service (sc exit $LASTEXITCODE)" }}
        $deadline = (Get-Date).AddSeconds(15)
        while ((Get-Service -Name $svc -ErrorAction SilentlyContinue) -and ((Get-Date) -lt $deadline)) {{ Start-Sleep -Milliseconds 400 }}
    }}
    New-Service -Name $svc -BinaryPathName $svcBin -DisplayName '{display}' -StartupType Automatic -Description 'Streams this PC to Latch.' | Out-Null
    Start-Service -Name $svc -ErrorAction SilentlyContinue
    if ((Get-Service -Name $svc -ErrorAction SilentlyContinue).Status -ne 'Running') {{
        Step "  '{service}' would not start; re-registering under the engine's own service name"
        Stop-Service -Name $svc -Force -ErrorAction SilentlyContinue
        sc.exe delete $svc | Out-Null
        $deadline = (Get-Date).AddSeconds(15)
        while ((Get-Service -Name $svc -ErrorAction SilentlyContinue) -and ((Get-Date) -lt $deadline)) {{ Start-Sleep -Milliseconds 400 }}
        if (Get-Service -Name '{upstream}' -ErrorAction SilentlyContinue) {{ throw "'{service}' would not start and '{upstream}' already exists; leaving that engine alone" }}
        $svc = '{upstream}'
        New-Service -Name $svc -BinaryPathName $svcBin -DisplayName '{display}' -StartupType Automatic -Description 'Streams this PC to Latch.' | Out-Null
        Start-Service -Name $svc -ErrorAction SilentlyContinue
    }}
    if ((Get-Service -Name $svc -ErrorAction SilentlyContinue).Status -ne 'Running') {{ throw "the {display} service did not start" }}
    {after_start}}}
"#,
            exe_dir = q(&exe_dir),
            zip = ENGINE_ZIP,
            tag = ENGINE_TAG,
            sha = ENGINE_SHA256,
            root = ENGINE_ZIP_ROOT,
            required = required,
            engine = crate::streamer::ENGINE_DIR,
            service = SERVICE,
            upstream = UPSTREAM_SERVICE,
            display = SERVICE_DISPLAY,
            repo = crate::streamer::REPO,
            restore_state = crate::legacy::restore_retired_state_ps(crate::streamer::ENGINE_DIR),
            migrate_copy = migrate_copy,
            brand_new = if p.dry_run {
                ""
            } else {
                "Brand-Engine $dir\n    "
            },
            stop_old = stop_old,
            after_start = after_start,
        )
    } else {
        String::new()
    };
    // Only the engine Latch installed is ever branded: an engine someone
    // else put in Program Files is theirs. A dry run changes no files.
    let brand_existing = if p.dry_run {
        String::new()
    } else {
        format!(
            "if ($dir -eq '{}') {{ Brand-Engine $dir }}\n        ",
            crate::streamer::ENGINE_DIR
        )
    };
    let adapter = if p.adapter.is_empty() {
        "Step \"Wake-on-LAN: adapter unknown, skipped\"\n".to_string()
    } else {
        format!(
            r#"# The names are single-quoted literals held in variables. Spliced into
# a double-quoted string, a '$' in a name would expand and a '"' end it.
$wakeAdapter = '{adapter}'
$wakeDevice = '{desc}'
Step "Enabling Wake-on-LAN on '$wakeAdapter'"
try {{
    Set-NetAdapterPowerManagement -Name $wakeAdapter -WakeOnMagicPacket Enabled -ErrorAction Stop
}} catch {{ Write-Output "  cmdlet failed ($_); the driver keywords below still apply" }}
# The NDIS keywords are what the driver reads: magic packet from sleep, from
# modern standby, and (Realtek's own keyword) from a full shutdown. ARP and
# NS offload keep the card answering for the PC's address while it sleeps,
# which is what lets a unicast wake packet reach it through a router.
$g = (Get-NetAdapter -Name $wakeAdapter -ErrorAction SilentlyContinue).InterfaceGuid
$k = Get-ChildItem 'HKLM:\SYSTEM\CurrentControlSet\Control\Class\{{4d36e972-e325-11ce-bfc1-08002be10318}}' -ErrorAction SilentlyContinue | Where-Object {{ (Get-ItemProperty $_.PSPath -Name NetCfgInstanceId -ErrorAction SilentlyContinue).NetCfgInstanceId -eq $g }} | Select-Object -First 1
if ($k) {{
    $changed = $false
    foreach ($kw in '*WakeOnMagicPacket', '*ModernStandbyWoLMagicPacket', 'S5WakeOnLan', '*PMARPOffload', '*PMNSOffload') {{
        if ((Get-ItemProperty $k.PSPath -ErrorAction SilentlyContinue).$kw -ne '1') {{
            Set-ItemProperty $k.PSPath -Name $kw -Value '1' -Type String
            $changed = $true
        }}
    }}
    if ($changed) {{ Restart-NetAdapter -Name $wakeAdapter -ErrorAction SilentlyContinue }}
}} else {{ Write-Output "  no class key for the adapter; keywords unchanged" }}
try {{ powercfg /deviceenablewake $wakeDevice | Out-Null }} catch {{ Write-Output "  powercfg: $_" }}
"#,
            adapter = q(p.adapter),
            desc = q(p.adapter_description)
        )
    };
    // A migrated PC keeps the web login it already has: rotating it would
    // orphan the credentials the Mac side stored, so the script never even
    // contains the `--creds` line (nor the password) in that case.
    let creds = if p.migrate {
        "        Step \"Keeping the migrated web login\"\n".to_string()
    } else {
        format!(
            r#"        Step "Setting the engine login Latch uses"
        Push-Location $dir
        & (Join-Path $dir 'sunshine.exe') --creds '{user}' '{pass}' 2>&1 | Out-Null
        Pop-Location
        Step "Restarting the streaming service"
        Get-Service | Where-Object {{ $_.Name -match '{service_match}' }} | Restart-Service -ErrorAction SilentlyContinue
"#,
            user = q(p.sunshine_user),
            pass = q(p.sunshine_pass),
            service_match = crate::migrate::SERVICE_MATCH,
        )
    };
    format!(
        r#"# Latch setup. Generated; re-run "Set up this PC" in Latch Host rather than editing.
$ErrorActionPreference = 'Continue'
function Step($m) {{ Write-Output "[$(Get-Date -Format HH:mm:ss)] $m" }}
function Brand-Engine($d) {{
    # Task Manager, the volume mixer and a firewall prompt show a program's
    # version block and icon. The engine's executables get Latch's, in
    # place, with their copyright and licence strings kept; the archive they
    # were unpacked from is untouched. The engine has to be stopped for the
    # rewrite, so only services whose binary is inside this directory are
    # paused, and they come back whatever happens. A failure here is
    # cosmetic: streaming works either way, so it is reported, not thrown.
    $exe = Join-Path $d 'sunshine.exe'
    if ((Get-Item -LiteralPath $exe -ErrorAction SilentlyContinue).VersionInfo.FileDescription -eq '{description}') {{ return }}
    Step "Giving the engine Latch's name and icon"
    $held = @()
    try {{
        $held = @(Get-CimInstance Win32_Service | Where-Object {{ $_.State -eq 'Running' -and $_.PathName -like ('*' + $d + '*') }} | ForEach-Object {{ $_.Name }})
        foreach ($s in $held) {{ Stop-Service -Name $s -Force -ErrorAction SilentlyContinue }}
        $errFile = Join-Path $env:TEMP ('latch-brand-' + [guid]::NewGuid().ToString('N') + '.txt')
        $p = Start-Process -FilePath '{host_exe}' -ArgumentList @('--brand-engine', ('"' + $d + '"')) -Wait -PassThru -WindowStyle Hidden -RedirectStandardError $errFile
        if ($p.ExitCode -ne 0) {{
            $why = Get-Content -LiteralPath $errFile -Raw -ErrorAction SilentlyContinue
            Write-Output "  the engine keeps its upstream name: exit $($p.ExitCode) $why"
        }}
        Remove-Item -LiteralPath $errFile -Force -ErrorAction SilentlyContinue
    }} catch {{
        Write-Output "  the engine keeps its upstream name: $_"
    }} finally {{
        foreach ($s in $held) {{ Start-Service -Name $s -ErrorAction SilentlyContinue }}
    }}
}}
{conceal}
Step "Latch setup started"
$migrate = {migrate_flag}
{retire}$dir = @({dirs}) | Where-Object {{ Test-Path (Join-Path $_ 'sunshine.exe') }} | Select-Object -First 1
# Set when the engine step fails. The wake and host-firewall steps below are
# independent and still run, but the script exits non-zero at the end so the
# caller does not report a successful setup over a broken engine.
$engineError = $null
$staging = $null
$keepEAP = $ErrorActionPreference
try {{
{install}
    if ($dir) {{
        {brand_existing}Write-EngineConf $dir
{creds}{take_over}    }} else {{
        Step "The streaming engine is not installed and was not requested"
    }}
}} catch {{
    $engineError = "$_"
    Write-Output "  engine: $_"
    # A failed install leaves no engine worth pointing a firewall rule at, and
    # $dir may name a directory that only half exists.
    $dir = $null
}} finally {{
    $ErrorActionPreference = $keepEAP
    if ($staging -and (Test-Path $staging)) {{ Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue }}
}}
Step "Opening TCP {port} to the tailnet for Latch Host"
netsh advfirewall firewall delete rule name="Latch Host" | Out-Null
netsh advfirewall firewall add rule name="Latch Host" dir=in action=allow protocol=TCP localport={port} remoteip=100.64.0.0/10 program='{exe}' | Out-Null
Step "Opening UDP 9 so a Mac can check its wake path while this PC is awake"
netsh advfirewall firewall delete rule name="Latch wake" | Out-Null
netsh advfirewall firewall add rule name="Latch wake" dir=in action=allow protocol=UDP localport=9 program='{exe}' | Out-Null
if ($dir) {{
    # Latch names and scopes these itself rather than running the engine's
    # own add-firewall-rule script, which opens every TCP and UDP port under
    # the name "Sunshine". The engine web UI port is deliberately absent: it
    # is reachable on loopback only.
    Step "Opening the streaming ports to the tailnet"
    foreach ($old in 'Latch Sunshine TCP', 'Latch Sunshine UDP', '{tcp_rule}', '{udp_rule}') {{
        netsh advfirewall firewall delete rule name="$old" | Out-Null
    }}
    $engineExe = Join-Path $dir 'sunshine.exe'
    netsh advfirewall firewall add rule name="{tcp_rule}" dir=in action=allow protocol=TCP localport=47984,47989,48010 remoteip=100.64.0.0/10 program="$engineExe" | Out-Null
    netsh advfirewall firewall add rule name="{udp_rule}" dir=in action=allow protocol=UDP localport=47998-48010 remoteip=100.64.0.0/10 program="$engineExe" | Out-Null
}}
{virtual_display}Step "Turning Fast Startup off: a PC shut down with it on cannot be woken"
Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value 0 -Type DWord
Step "Never idle-sleep when plugged in, so Tailscale stays up from anywhere"
powercfg /change standby-timeout-ac 0
powercfg /change hibernate-timeout-ac 0
{adapter}if ($engineError) {{
    Step "Latch setup FAILED: $engineError"
    exit 1
}}
Step "Latch setup finished"
exit 0
"#,
        dirs = dirs,
        retire = crate::legacy::retire_windows_ps(crate::streamer::ENGINE_DIR),
        conceal = conceal_ps(),
        description = crate::brand::DESCRIPTION,
        host_exe = q(&p.exe.display().to_string()),
        brand_existing = brand_existing,
        install = install,
        migrate_flag = if p.migrate { "$true" } else { "$false" },
        creds = creds,
        take_over = if p.dry_run {
            String::new()
        } else {
            // Base64 UTF-16 is the one form PowerShell takes verbatim from a
            // command line: no quoting layer between this script and the
            // helper, and nothing on disk a user-writable path could swap.
            format!(
                "        Step \"Running the streaming engine as the signed-in user\"\n        & powershell.exe -NoProfile -ExecutionPolicy Bypass -EncodedCommand {}\n",
                encoded_command(include_str!("../windows/take-over-engine.ps1"))
            )
        },
        port = CONTROL_PORT,
        exe = q(&p.exe.display().to_string()),
        tcp_rule = TCP_RULE,
        udp_rule = UDP_RULE,
        adapter = adapter,
        virtual_display = if p.dry_run {
            String::new()
        } else {
            crate::virtual_display::setup_ps()
        },
    )
}

/// Ask Windows for an administrator token, then run [`run_as_admin`] in a
/// new process. The script is generated only after elevation, so a user-
/// writable `setup.ps1` cannot be swapped during the UAC prompt. On macOS
/// and Linux setup needs no elevation and runs here.
pub fn run(p: &Plan<'_>) -> Result<()> {
    #[cfg(not(windows))]
    {
        crate::unix_setup::run(p)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // The logon helper lives in this user's profile and runs as this
        // user, so the unelevated panel installs it.
        let _ = crate::audio::install_helpers();
        let exe = q(&p.exe.display().to_string());
        let log = log_path().unwrap_or_else(|| std::env::temp_dir().join("latch-setup.log"));
        // UAC may run the helper as another administrator, whose profile
        // holds no host.toml: pass this user's folder so the engine login
        // and setup.log stay with the account that shares the machine.
        let args = elevated_args(
            std::env::var_os("LOCALAPPDATA")
                .filter(|dir| !dir.is_empty())
                .map(|dir| dir.to_string_lossy().into_owned())
                .as_deref(),
        );
        let launch = format!(
            "$p = Start-Process -FilePath '{exe}' -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList @({args}); if ($null -eq $p) {{ exit 1 }}; exit $p.ExitCode"
        );
        let status = std::process::Command::new("powershell")
            .args(["-NoProfile", "-NonInteractive", "-Command", &launch])
            .creation_flags(0x0800_0000)
            .status()
            .context("launch elevated Latch Host")?;
        anyhow::ensure!(
            status.success(),
            "the administrator prompt was declined or setup failed (see {})",
            log.display()
        );
        Ok(())
    }
}

/// Run a read-only PowerShell probe and return what it printed.
#[cfg(windows)]
pub(crate) fn powershell(script: &str) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", script])
        .creation_flags(0x0800_0000)
        .output()
        .context("run powershell")?;
    anyhow::ensure!(
        out.status.success(),
        "powershell exited with {}: {}",
        out.status,
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The `-ArgumentList` array for the elevated `--setup-elevated` run.
/// Start-Process joins the elements with spaces and quotes none of them,
/// so a folder with a space in it ("C:\Users\Ada Lovelace\...") has to
/// carry its own double quotes or it arrives as two arguments. A trailing
/// backslash would escape the closing quote, and a folder never needs one.
#[cfg(any(windows, test))]
fn elevated_args(local_app_data: Option<&str>) -> String {
    match local_app_data {
        Some(dir) => format!(
            "'--setup-elevated', '--local-app-data', '\"{}\"'",
            q(dir.trim_end_matches('\\'))
        ),
        None => "'--setup-elevated'".into(),
    }
}

// Only the Windows setup path is compiled in a release build; tests use it everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
/// A PowerShell single-quoted literal: only `'` needs escaping, nothing is
/// expanded.
fn q(s: &str) -> String {
    s.replace('\'', "''")
}

// Only the Windows setup path is compiled in a release build; tests use it everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
/// `powershell -EncodedCommand` takes the script as base64 of UTF-16LE.
fn encoded_command(script: &str) -> String {
    let bytes: Vec<u8> = script
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect();
    base64(&bytes)
}

// Only the Windows setup path is compiled in a release build; tests use it everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
            | u32::from(*chunk.get(2).unwrap_or(&0));
        out.push(TABLE[(n >> 18 & 63) as usize] as char);
        out.push(TABLE[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6 & 63) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 63) as usize] as char
        } else {
            '='
        });
    }
    out
}

/// Generate and run the setup script. Called from `--setup-elevated` after
/// UAC, so the file is written by the elevated process itself.
pub fn run_as_admin() -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe().context("own path")?;
        let cfg = crate::config::HostConfig::load();
        let wake = crate::wake::probe();
        let install = crate::streamer::find();
        let kind = install.as_ref().map(|i| i.kind).unwrap_or("");
        let migrate = crate::migrate::uses_old_engine(kind);
        let running = install.is_some() && crate::streamer::running();
        let old_folder = install.as_ref().is_some_and(crate::streamer::in_old_folder);
        let plan = Plan {
            exe: &exe,
            install_engine: install.is_none() || migrate || !running || old_folder,
            migrate,
            dry_run: false,
            sunshine_user: &cfg.sunshine_user,
            sunshine_pass: &cfg.sunshine_pass,
            adapter: &wake.adapter,
            adapter_description: &wake.description,
        };
        // %SystemRoot%\Temp is writable by administrators only, so the
        // script this elevated process writes cannot be swapped by a process
        // running as the plain user before PowerShell reads it.
        let tmp_dir = std::env::var_os("SystemRoot")
            .map(|root| PathBuf::from(root).join("Temp"))
            .filter(|dir| dir.is_dir())
            .unwrap_or_else(std::env::temp_dir);
        let tmp = tmp_dir.join(format!("latch-setup-{}.ps1", std::process::id()));
        // PowerShell 5.1 reads a BOM-less file as the system ANSI code page, so
        // a Korean/Japanese username or adapter name ("이더넷") would be mangled
        // and the firewall rule would point at a path that does not exist.
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(script(&plan).as_bytes());
        std::fs::write(&tmp, &bytes)?;
        let log = log_path().unwrap_or_else(|| std::env::temp_dir().join("latch-setup.log"));
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let status = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
                tmp.to_str().unwrap_or(""),
            ])
            .stdout(out)
            .stderr(err)
            .creation_flags(0x0800_0000)
            .status();
        let _ = std::fs::remove_file(&tmp);
        let status = status.context("run setup script")?;
        anyhow::ensure!(status.success(), "setup failed (see {})", log.display());
        Ok(())
    }
    #[cfg(not(windows))]
    anyhow::bail!("setup runs on Windows only")
}

/// Register or remove `latch-host.exe --background` under the current
/// user's Run key.
pub fn set_start_with_windows(enable: bool, exe: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const KEY: &str = r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run";
        let reg = |args: &[&str]| {
            std::process::Command::new("reg")
                .args(args)
                .creation_flags(0x0800_0000)
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
        };
        // An earlier version's value goes either way: on, it is replaced;
        // off, it would still start the host.
        let _ = reg(&["delete", KEY, "/v", crate::legacy::WINDOWS_RUN_VALUE, "/f"]);
        let status = if enable {
            reg(&[
                "add",
                KEY,
                "/v",
                "LatchHost",
                "/t",
                "REG_SZ",
                "/d",
                &run_value(exe),
                "/f",
            ])?
        } else {
            reg(&["delete", KEY, "/v", "LatchHost", "/f"])?
        };
        anyhow::ensure!(
            status.success() || (!enable && !starts_with_windows()),
            "could not update the Run key"
        );
        Ok(())
    }
    #[cfg(not(windows))]
    crate::unix_setup::set_autostart(enable, exe)
}

/// Keep the login registration current from the background service: the
/// Run value on Windows, the launch agent or user unit elsewhere. Written
/// only; nothing is started or stopped, because the caller is the service.
pub fn register_autostart(exe: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        set_start_with_windows(true, exe)
    }
    #[cfg(not(windows))]
    crate::unix_setup::register_autostart(exe)
}

pub fn starts_with_windows() -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        ["LatchHost", crate::legacy::WINDOWS_RUN_VALUE]
            .iter()
            .any(|value| {
                std::process::Command::new("reg")
                    .args([
                        "query",
                        r"HKCU\Software\Microsoft\Windows\CurrentVersion\Run",
                        "/v",
                        value,
                    ])
                    .creation_flags(0x0800_0000)
                    .output()
                    .map(|o| o.status.success())
                    .unwrap_or(false)
            })
    }
    #[cfg(not(windows))]
    crate::unix_setup::autostart_enabled()
}

#[cfg(any(windows, test))]
pub fn run_value(exe: &Path) -> String {
    format!("\"{}\" --background", exe.display())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc_4648_vectors() {
        for (input, want) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), want);
        }
    }

    #[test]
    fn encoded_command_is_base64_of_utf16le() {
        // "Hi" in UTF-16LE is 48 00 69 00.
        assert_eq!(encoded_command("Hi"), "SABpAA==");
        let helper = encoded_command(include_str!("../windows/take-over-engine.ps1"));
        assert!(helper.len() < 30_000, "must fit a Windows command line");
        assert!(helper
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='));
    }

    #[test]
    fn the_elevated_run_gets_the_whole_folder_as_one_argument() {
        assert_eq!(elevated_args(None), "'--setup-elevated'");
        assert_eq!(
            elevated_args(Some(r"C:\Users\Ada Lovelace\AppData\Local")),
            r#"'--setup-elevated', '--local-app-data', '"C:\Users\Ada Lovelace\AppData\Local"'"#
        );
        assert_eq!(
            elevated_args(Some(r"C:\Users\O'Brien\AppData\Local\")),
            r#"'--setup-elevated', '--local-app-data', '"C:\Users\O''Brien\AppData\Local"'"#,
            "quote escaped for PowerShell, no trailing backslash before the quote"
        );
    }

    #[test]
    fn run_value_quotes_the_path_and_asks_for_background() {
        let p = PathBuf::from(r"C:\Users\Ada\AppData\Local\Latch\latch-host.exe");
        assert_eq!(
            run_value(&p),
            r#""C:\Users\Ada\AppData\Local\Latch\latch-host.exe" --background"#
        );
    }

    /// Position of `needle` after the preamble's function definitions, so
    /// an ordering test reads the steps as they run, not the helpers.
    fn find_in_body(s: &str, needle: &str) -> Option<usize> {
        let at = s.find("Latch setup started")?;
        s[at..].find(needle).map(|i| i + at)
    }

    /// The script with its comments stripped: a "must not appear" check has
    /// to be about what the script does, not about what it explains.
    fn code(s: &str) -> String {
        s.lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn plan<'a>(exe: &'a Path, install_engine: bool) -> Plan<'a> {
        Plan {
            exe,
            install_engine,
            migrate: false,
            dry_run: false,
            sunshine_user: "u",
            sunshine_pass: "p",
            adapter: "",
            adapter_description: "",
        }
    }

    #[test]
    fn script_escapes_quotes_and_skips_what_is_not_wanted() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&Plan {
            sunshine_user: "latch",
            sunshine_pass: "p'w",
            adapter: "Ethernet",
            adapter_description: "Realtek PCIe GbE",
            ..plan(&exe, false)
        });
        assert!(s.contains("--creds 'latch' 'p''w'"), "{s}");
        assert!(!s.contains("Downloading the streaming engine"));
        assert!(s.contains("$wakeAdapter = 'Ethernet'"), "{s}");
        assert!(s.contains("Set-NetAdapterPowerManagement -Name $wakeAdapter"));
        assert!(s.contains("Restart-NetAdapter -Name $wakeAdapter"));
        assert!(s.contains("'S5WakeOnLan'"));
        assert!(s.contains("HiberbootEnabled -Value 0"));
        assert!(s.contains("powercfg /change standby-timeout-ac 0"));
        assert!(s.contains("powercfg /change hibernate-timeout-ac 0"));
        assert!(s.contains("protocol=UDP localport=9 program='C:\\x\\latch-host.exe'"));
        assert!(s.contains("$wakeDevice = 'Realtek PCIe GbE'"));
        assert!(s.contains("powercfg /deviceenablewake $wakeDevice"));
        // A name only ever appears as a single-quoted literal: in a
        // double-quoted string `$(...)` would run and a `"` end the string.
        let named = script(&Plan {
            adapter: r#"Eth "$(Stop-Computer)" 'x'"#,
            adapter_description: "NIC $env:TEMP",
            ..plan(&exe, false)
        });
        assert!(
            named.contains(r#"$wakeAdapter = 'Eth "$(Stop-Computer)" ''x'''"#),
            "{named}"
        );
        assert!(named.contains("$wakeDevice = 'NIC $env:TEMP'"), "{named}");
        assert_eq!(named.matches("Stop-Computer").count(), 1, "{named}");
        assert!(
            s.contains("localport=47850 remoteip=100.64.0.0/10 program='C:\\x\\latch-host.exe'")
        );
        let dollar = PathBuf::from(r"C:\Users\joe$lab\Latch\latch-host.exe");
        let s = script(&plan(&dollar, false));
        assert!(
            s.contains("program='C:\\Users\\joe$lab\\Latch\\latch-host.exe'"),
            "firewall path must be a single-quoted PowerShell literal:\n{s}"
        );
        assert!(
            !s.contains("program=\"C:\\Users\\joe$lab"),
            "double-quoted firewall path would expand $lab:\n{s}"
        );

        let s = script(&plan(&exe, true));
        assert!(s.contains("Downloading the streaming engine"));
        assert!(s.contains("adapter unknown, skipped"));
        assert!(!s.contains("Set-NetAdapterPowerManagement"));
    }

    #[test]
    fn a_fresh_install_unpacks_the_pinned_lite_archive_and_leaves_no_upstream_traces() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));

        assert!(s.contains(ENGINE_ZIP), "{s}");
        assert!(
            s.contains(r"Join-Path 'C:\x' 'Sunshine-Windows-AMD64-lite.zip'"),
            "the bundled archive is next to the exe even when this is dumped on a Mac:\n{s}"
        );
        assert!(s.contains(&format!("releases/tags/{ENGINE_TAG}")), "{s}");
        assert!(s.contains("$_.name -like '*lite.zip'"), "{s}");
        assert!(s.contains("Expand-Archive -LiteralPath $zip"), "{s}");
        assert!(s.contains(crate::streamer::ENGINE_DIR), "{s}");
        assert!(s.contains(r"tools\sunshinesvc.exe"), "{s}");

        let code = code(&s);
        for gone in [
            "Sunshine-Windows-AMD64-installer.msi",
            "msiexec",
            "releases/latest",
            ".bat",
            "Start Menu",
            "Uninstall",
        ] {
            assert!(!code.contains(gone), "{gone} should be gone:\n{s}");
        }
    }

    #[test]
    fn a_missing_or_wrong_archive_throws_before_anything_is_unpacked() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = code(&script(&plan(&exe, true)));

        let missing = s
            .find(r#"throw "the engine archive is missing"#)
            .expect("missing-archive guard");
        let digest = s
            .find("Get-FileHash -Algorithm SHA256")
            .expect("digest check");
        let mismatch = s
            .find(&format!(r#"throw "engine archive is not {ENGINE_TAG}"#))
            .expect("digest guard");
        let expand = s.find("Expand-Archive").expect("expand");

        assert!(missing < expand, "the missing-archive guard runs first");
        assert!(
            digest < expand,
            "the archive is hashed before it is unpacked"
        );
        assert!(
            mismatch < expand,
            "a digest mismatch throws before unpacking"
        );
        assert!(
            s.contains(ENGINE_SHA256),
            "the pinned digest is in the script"
        );
        assert_eq!(ENGINE_SHA256.len(), 64, "sha256 is 64 hex characters");
        assert!(ENGINE_SHA256
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
    }

    #[test]
    fn a_failed_extract_throws_before_the_service_is_registered() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = code(&script(&plan(&exe, true)));

        assert!(
            s.contains(&format!("$root = Join-Path $unpack '{ENGINE_ZIP_ROOT}'")),
            "the archive root folder is named, not guessed:\n{s}"
        );
        let guard = s
            .find(r#"throw "the engine archive did not unpack"#)
            .expect("unpack guard");
        let register = s.find("New-Service").expect("service registration");
        assert!(guard < register, "a bad unpack throws before New-Service");
        let listed = ENGINE_REQUIRED
            .iter()
            .map(|f| format!("'{f}'"))
            .collect::<Vec<_>>()
            .join(", ");
        assert!(
            s.contains(&format!("foreach ($need in {listed})")),
            "both checks use ENGINE_REQUIRED:\n{s}"
        );
        assert_eq!(
            s.matches(&format!("foreach ($need in {listed})")).count(),
            2,
            "unpack AND copy are both checked:\n{s}"
        );
    }

    #[test]
    fn the_latch_service_and_the_fallback_are_proven_not_assumed() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));

        assert_eq!(SERVICE, "LatchStream");
        assert_eq!(SERVICE_DISPLAY, "Latch Streaming");
        assert!(s.contains(&format!("$svc = '{SERVICE}'")), "{s}");
        assert_eq!(
            s.matches(&format!("-DisplayName '{SERVICE_DISPLAY}'"))
                .count(),
            2,
            "both registrations show the Latch name:\n{s}"
        );
        assert!(!code(&s).contains("Sunshine Service"), "{s}");

        let create = s.find("New-Service -Name $svc").expect("create");
        let probe = s.find("Start-Service -Name $svc").expect("start probe");
        let fallback = s
            .find(&format!("$svc = '{UPSTREAM_SERVICE}'"))
            .expect("fallback");
        assert!(create < probe, "the service is created before it is probed");
        assert!(
            probe < fallback,
            "the fallback is reached only after a real start attempt:\n{s}"
        );
        assert!(
            s.contains(&format!(
                r#"if (Get-Service -Name '{UPSTREAM_SERVICE}' -ErrorAction SilentlyContinue) {{ throw"#
            )),
            "an existing upstream service is refused, never reused:\n{s}"
        );
        assert!(
            s.contains(&format!(
                r#"throw "the {SERVICE_DISPLAY} service did not start""#
            )),
            "a service that never runs is an error, not a silent success:\n{s}"
        );
    }

    #[test]
    fn the_virtual_display_learns_every_size_a_mac_can_ask_for() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, false));
        let body = code(&s);
        let step = find_in_body(&body, "Listing the sizes a Mac can ask for").expect("step");
        let fast = find_in_body(&body, "Turning Fast Startup off").expect("fast startup");
        let ports = find_in_body(&body, "Opening the streaming ports").expect("ports");
        assert!(
            ports < step && step < fast,
            "after the engine, before the power steps"
        );
        assert!(s.contains("@(3024,1964)"), "{s}");
        assert!(s.contains("@(1280,720)"), "{s}");
        assert!(s.contains("pnputil /restart-device"), "{s}");
        let dry = script(&Plan {
            dry_run: true,
            ..plan(&exe, false)
        });
        assert!(
            !dry.contains("pnputil /restart-device") && !dry.contains("vdd_settings"),
            "a dry run changes no display:\n{dry}"
        );
    }

    #[test]
    fn firewall_rules_are_named_for_latch_and_leave_the_web_ui_on_loopback() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, false));

        assert!(s.contains(&format!(r#"name="{TCP_RULE}" dir=in action=allow protocol=TCP localport=47984,47989,48010 remoteip=100.64.0.0/10 program="$engineExe""#)), "{s}");
        assert!(s.contains(&format!(r#"name="{UDP_RULE}" dir=in action=allow protocol=UDP localport=47998-48010 remoteip=100.64.0.0/10 program="$engineExe""#)), "{s}");

        let code = code(&s);
        assert!(
            !code.contains("47990"),
            "the web UI port stays off the tailnet:\n{s}"
        );
        assert!(
            !code.contains("localport=47984-48010"),
            "the old range included the web UI port:\n{s}"
        );
        assert!(
            s.contains("'Latch Sunshine TCP', 'Latch Sunshine UDP'"),
            "the rules this replaces are cleaned up:\n{s}"
        );
    }

    #[test]
    fn setup_looks_for_the_latch_engine_before_any_other() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, false));
        let line = s
            .lines()
            .find(|l| l.starts_with("$dir = @("))
            .expect("install dir probe");
        assert_eq!(
            line,
            format!(
                r"$dir = @('{}', 'C:\Program Files\Sunshine', 'C:\Program Files\Apollo') | Where-Object {{ Test-Path (Join-Path $_ 'sunshine.exe') }} | Select-Object -First 1",
                crate::streamer::ENGINE_DIR
            )
        );
    }

    #[test]
    fn a_failed_copy_is_checked_on_the_destination_before_the_service_is_registered() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = code(&script(&plan(&exe, true)));
        let dest = s
            .find(r#"throw "the engine did not copy to"#)
            .expect("destination check");
        let register = s.find("New-Service").expect("service registration");
        assert!(
            dest < register,
            "a partial copy must not become a service:\n{s}"
        );
        assert!(
            s.contains(&format!(
                "if (-not (Test-Path (Join-Path '{}' $need)))",
                crate::streamer::ENGINE_DIR
            )),
            "the check is on ENGINE_DIR, not the staging tree:\n{s}"
        );
    }

    #[test]
    fn engine_failures_are_terminating_and_reported_as_nonzero_exit() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = code(&script(&plan(&exe, true)));

        let stop = s
            .find("$ErrorActionPreference = 'Stop'")
            .expect("engine scope is terminating");
        let restore = s
            .find("$ErrorActionPreference = $keepEAP")
            .expect("preference is restored");
        let expand = s.find("Expand-Archive").expect("expand");
        let copy = s.find("Copy-Item").expect("copy");
        let create = s.find("New-Service").expect("create");
        assert!(stop < expand && stop < copy && stop < create);
        assert!(restore > create, "restore is in finally, after the engine");

        assert!(s.contains("$engineError = $null"));
        assert!(s.contains(r#"$engineError = "$_""#));
        assert!(s.contains("$dir = $null"), "a failed install is not reused");

        let wake = s.find("HiberbootEnabled").expect("wake still runs");
        let fail = s
            .find("if ($engineError)")
            .expect("failure is reported at the end");
        let exit1 = s.rfind("exit 1").expect("nonzero exit");
        assert!(wake < fail, "unrelated safe steps still run");
        assert!(fail < exit1);
        assert!(s.contains("exit 0"), "success is an explicit zero");
    }

    #[test]
    fn staging_is_unique_per_run_and_cleaned_in_finally() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));
        assert!(s.contains("[guid]::NewGuid()"), "{s}");
        assert!(
            !code(&s).contains("latch-engine-unpack"),
            "fixed staging name is gone:\n{s}"
        );
        assert!(
            !code(&s).contains("latch-engine.zip"),
            "fixed download name is gone:\n{s}"
        );
        assert!(
            s.contains("Remove-Item $staging -Recurse -Force -ErrorAction SilentlyContinue"),
            "finally cleans the owned folder:\n{s}"
        );
    }

    #[test]
    fn native_command_failures_are_checked() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));
        assert!(s.contains("if ($LASTEXITCODE -ne 0) { throw \"could not reach GitHub"));
        assert!(
            s.contains("if ($LASTEXITCODE -ne 0) { throw \"downloading the engine archive failed")
        );
        assert!(s.contains(
            "if ($LASTEXITCODE -ne 0) { throw \"could not remove the existing $svc service"
        ));
    }

    fn key_hits(conf: &str, key: &str) -> usize {
        conf.lines().filter(|l| conf_key(l) == Some(key)).count()
    }

    #[test]
    fn conceal_conf_disables_the_tray_and_keeps_the_web_ui_on_this_pc() {
        let got = conceal_conf("");
        assert!(got.contains("system_tray = disabled"), "{got}");
        assert!(got.contains("origin_web_ui_allowed = pc"), "{got}");
        assert_eq!(key_hits(&got, "system_tray"), 1);
        assert_eq!(key_hits(&got, "origin_web_ui_allowed"), 1);
        assert!(!got.contains("bind_address"), "{got}");
        assert_eq!(conceal_conf(&got), got, "a second run must be a no-op");
    }

    #[test]
    fn conceal_conf_preserves_unrelated_settings_and_does_not_duplicate_keys() {
        let existing = "\
# user encoder
encoder = nvenc
system_tray = enabled
origin_web_ui_allowed = wan
resolution = 1920x1080
system_tray = enabled
";
        let got = conceal_conf(existing);
        assert!(got.contains("# user encoder"), "{got}");
        assert!(got.contains("encoder = nvenc"), "{got}");
        assert!(got.contains("resolution = 1920x1080"), "{got}");
        assert!(got.contains("system_tray = disabled"), "{got}");
        assert!(got.contains("origin_web_ui_allowed = pc"), "{got}");
        assert!(!got.contains("system_tray = enabled"), "{got}");
        assert!(!got.contains("origin_web_ui_allowed = wan"), "{got}");
        assert_eq!(key_hits(&got, "system_tray"), 1, "{got}");
        assert_eq!(key_hits(&got, "origin_web_ui_allowed"), 1, "{got}");
        assert_eq!(conceal_conf(&got), got);
    }

    #[test]
    fn linux_engines_get_no_frame_rate_floor_and_others_keep_theirs() {
        fn get(conf: &[(&'static str, &'static str)], key: &str) -> Option<&'static str> {
            conf.iter().find(|(k, _)| *k == key).map(|&(_, v)| v)
        }
        for (k, _) in LINUX_ENGINE_CONF {
            assert!(get(ENGINE_CONF, k).is_some(), "{k} overrides nothing");
        }
        let linux = engine_conf_for(true);
        let other = engine_conf_for(false);
        assert_eq!(linux.len(), ENGINE_CONF.len());
        assert_eq!(other, ENGINE_CONF);
        assert_eq!(get(&linux, "minimum_fps_target"), Some("0"));
        assert_eq!(get(&other, "minimum_fps_target"), Some("60"));
        assert_eq!(get(&linux, "sw_preset"), Some("superfast"));
        assert_eq!(get(&other, "sw_preset"), Some("ultrafast"));
        assert_eq!(get(&linux, "sw_tune"), Some("zerolatency"));
        let conf = conceal_conf_with("minimum_fps_target = 60\ncapture = x11\n", &linux);
        assert_eq!(key_hits(&conf, "minimum_fps_target"), 1, "{conf}");
        assert!(conf.contains("minimum_fps_target = 0"), "{conf}");
        assert!(conf.contains("capture = x11"), "{conf}");
    }

    #[test]
    fn the_engine_conf_is_written_before_the_service_starts() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));
        for (k, v) in ENGINE_CONF {
            assert!(
                s.contains(&format!("'{k}' = '{v}'")),
                "generated script must ship {k} = {v}:\n{s}"
            );
        }
        let write = s
            .find("Write-EngineConf $dir")
            .expect("conf write is called");
        let start = find_in_body(&s, "Start-Service").expect("service start");
        assert!(
            write < start,
            "conf must land before the first Start-Service or the tray flashes:\n{s}"
        );
        assert_eq!(
            s.matches("Write-EngineConf $dir").count(),
            2,
            "fresh install and a re-run on an existing dir both write:\n{s}"
        );
    }

    #[test]
    fn the_script_does_not_bind_the_engine_to_loopback() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = code(&script(&plan(&exe, true)));
        assert!(
            !s.contains("bind_address"),
            "bind_address would take GameStream down with the web UI:\n{s}"
        );
        assert!(!s.contains("127.0.0.1"), "{s}");
    }

    fn migrate_plan<'a>(exe: &'a Path, dry_run: bool) -> Plan<'a> {
        Plan {
            migrate: true,
            dry_run,
            ..plan(exe, true)
        }
    }

    #[test]
    fn old_engine_is_disabled_before_the_new_one_starts() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));
        let disable = s
            .find("StartupType Disabled")
            .expect("old service is disabled");
        let start = find_in_body(&s, "Start-Service").expect("new service starts");
        assert!(
            disable < start,
            "the old engine must be down before 47984 is taken:\n{s}"
        );
        assert!(
            s.contains("-match 'Sunshine|Apollo' } | ForEach-Object"),
            "disable matches only upstream engines:\n{s}"
        );
        let restart = s
            .lines()
            .find(|l| l.contains("Restart-Service"))
            .expect("creds restart");
        assert!(
            restart.contains(crate::migrate::SERVICE_MATCH),
            "creds restart must see LatchStream:\n{restart}"
        );
    }

    #[test]
    fn setup_starts_the_engine_as_the_signed_in_user_after_the_service() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = script(&plan(&exe, true));
        let restart = s
            .lines()
            .find(|l| l.contains("Restart-Service"))
            .expect("creds restart");
        let take = s
            .find("-EncodedCommand ")
            .expect("setup runs the audio take-over helper");
        let restart_at = s.find(restart).expect("restart in script");
        assert!(
            restart_at < take,
            "the service restart would put SYSTEM back in front of the user engine:\n{s}"
        );
        assert!(
            !code(&script(&plan(&exe, true))).contains("127.0.0.1"),
            "take-over must not bind the engine to loopback"
        );
        let dry = script(&migrate_plan(&exe, true));
        assert!(
            !dry.contains("-EncodedCommand "),
            "dry-run must not take over the live engine:\n{dry}"
        );
    }

    #[test]
    fn migrate_skips_creds_and_fresh_install_still_sets_them() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let fresh = script(&plan(&exe, true));
        assert!(fresh.contains("--creds 'u' 'p'"), "{fresh}");
        assert!(fresh.contains("$migrate = $false"), "{fresh}");

        let mig = script(&migrate_plan(&exe, false));
        assert!(mig.contains("$migrate = $true"), "{mig}");
        assert!(
            !code(&mig).contains("--creds"),
            "migrate must not rotate web creds:\n{mig}"
        );
        assert!(mig.contains("Keeping the migrated web login"), "{mig}");
        assert!(fresh.contains("--creds"), "{fresh}");
    }

    #[test]
    fn migrate_uninstalls_only_after_prove_and_dry_run_never_calls_msiexec() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let mig = script(&migrate_plan(&exe, false));
        let copy = mig.find("Copy-EngineState").expect("copy");
        let verify = mig.find("Assert-EngineState").expect("verify");
        let start = find_in_body(&mig, "Start-Service").expect("start");
        let prove = mig
            .find("the new engine did not start listening")
            .expect("prove");
        let uninstall = mig.find("msiexec").expect("uninstall");
        let cleanup = mig
            .find("Removing what the old installer left behind")
            .expect("cleanup");
        assert!(copy < verify && verify < start && start < prove && prove < uninstall);
        assert!(
            uninstall < cleanup,
            "the old folder goes only after the MSI:\n{mig}"
        );
        assert!(mig.contains(crate::migrate::PRODUCT_CODE), "{mig}");
        assert!(mig.contains("/x"), "{mig}");
        assert!(
            mig.contains("$needFiles = (-not $dir) -or ($dir -ne '"),
            "Sunshine already installed must still unpack Latch:\n{mig}"
        );
        assert!(
            mig.contains("Stop-Service -Name $svc -Force"),
            "a leftover service must be stopped before sc.exe delete:\n{mig}"
        );

        let dry = code(&script(&migrate_plan(&exe, true)));
        assert!(
            !dry.contains("msiexec"),
            "dry-run must never call msiexec:\n{dry}"
        );
        assert!(
            !dry.contains("Removing what the old installer left behind"),
            "dry-run must not delete anything:\n{dry}"
        );
        assert!(dry.contains("Copy-EngineState"), "{dry}");
        assert!(dry.contains("Assert-EngineState"), "{dry}");
    }

    #[test]
    fn branding_follows_the_copy_touches_only_the_latch_engine_and_skips_dry_runs() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let fresh = code(&script(&plan(&exe, true)));
        let copied = fresh
            .find("the engine did not copy to")
            .expect("copy check");
        let brand = fresh.find("Brand-Engine $dir").expect("brand call");
        let register = fresh.find("Registering the").expect("register");
        assert!(copied < brand && brand < register, "{fresh}");
        assert!(
            fresh.contains(r"if ($dir -eq 'C:\Program Files\Latch\engine') { Brand-Engine $dir }"),
            "{fresh}"
        );
        assert!(
            fresh.contains(".VersionInfo.FileDescription -eq 'Latch Streaming'"),
            "{fresh}"
        );
        assert!(
            fresh.contains(r"-FilePath 'C:\x\latch-host.exe'"),
            "{fresh}"
        );
        assert!(fresh.contains("'--brand-engine'"), "{fresh}");
        // Only services running out of the engine directory are paused.
        assert!(
            fresh.contains("$_.PathName -like ('*' + $d + '*')"),
            "{fresh}"
        );
        assert!(fresh.contains("finally"), "{fresh}");

        let existing = code(&script(&plan(&exe, false)));
        assert!(existing.contains("{ Brand-Engine $dir }"), "{existing}");
        assert!(!existing.contains("Registering the"), "{existing}");

        let dry = code(&script(&migrate_plan(&exe, true)));
        assert!(
            !dry.contains("Brand-Engine $dir"),
            "dry run must not brand:\n{dry}"
        );
        let wet = code(&script(&migrate_plan(&exe, false)));
        assert!(wet.contains("Brand-Engine $dir"), "{wet}");
    }

    #[test]
    fn a_fresh_engine_gets_the_pairing_state_an_unmoved_old_one_kept() {
        let exe = PathBuf::from(r"C:\x\latch-host.exe");
        let s = code(&script(&plan(&exe, true)));
        let retire = s
            .find("$retiredState = $null")
            .expect("reset by the retire step");
        let copied = s.find("the engine did not copy to").expect("copy check");
        let restore = s.find("if ($retiredState -and").expect("state copy");
        let conf = s[copied..].find("Write-EngineConf $dir").expect("conf") + copied;
        let register = s.find("New-Service").expect("register");
        assert!(
            retire < copied && copied < restore && restore < conf && conf < register,
            "{s}"
        );
    }

    /// The seam CI's PowerShell syntax check runs through (see
    /// `.github/workflows/ci.yml`): the generated script is the real
    /// artefact, so it has to be obtainable without running setup.
    /// `LATCH_DUMP_SETUP_SCRIPT=/tmp/setup.ps1` writes one; the other
    /// `LATCH_DUMP_*` variables pick the plan.
    #[test]
    fn the_generated_script_can_be_dumped_for_a_syntax_check() {
        let Ok(out) = std::env::var("LATCH_DUMP_SETUP_SCRIPT") else {
            return;
        };
        let var = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.into());
        let exe = PathBuf::from(var("LATCH_DUMP_EXE", r"C:\x\latch-host.exe"));
        let user = var("LATCH_DUMP_USER", "u");
        let pass = var("LATCH_DUMP_PASS", "p");
        let adapter = var("LATCH_DUMP_ADAPTER", "");
        let desc = var("LATCH_DUMP_ADAPTER_DESC", "");
        let p = Plan {
            migrate: var("LATCH_DUMP_MIGRATE", "0") == "1",
            dry_run: var("LATCH_DUMP_DRY_RUN", "0") == "1",
            sunshine_user: &user,
            sunshine_pass: &pass,
            adapter: &adapter,
            adapter_description: &desc,
            ..plan(&exe, true)
        };
        std::fs::write(out, script(&p)).expect("write the script");
    }
}
