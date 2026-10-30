//! The administrator step (firewall, wake, virtual-display modes) and autostart.
#[cfg(windows)]
use anyhow::Context;
use anyhow::Result;
use latch_core::config::data_dir;
use latch_core::CONTROL_PORT;
use std::path::{Path, PathBuf};

pub struct Plan<'a> {
    pub exe: &'a Path,
    pub adapter: &'a str,
    pub adapter_description: &'a str,
}

/// The setup transcript. On Windows the elevated step writes it beside
/// the copy of Latch it runs from, in `%ProgramFiles%\Latch\Setup`:
/// never under a folder the signed-in user (or whoever started the
/// elevated step) can redirect.
pub fn log_path() -> Option<PathBuf> {
    if cfg!(windows) {
        let pf = std::env::var_os("ProgramFiles").filter(|d| !d.is_empty())?;
        return Some(PathBuf::from(pf).join(SETUP_DIR).join("setup.log"));
    }
    data_dir().ok().map(|d| d.join("setup.log"))
}

/// Under `%ProgramFiles%`, which only administrators can change.
const SETUP_DIR: &str = r"Latch\Setup";

/// Windows' own PowerShell, by full path. A bare `powershell` is looked
/// for in the running executable's folder first, which for Latch is one
/// the user can write to.
#[cfg(any(windows, test))]
fn powershell_exe() -> PathBuf {
    let root = std::env::var("SystemRoot")
        .ok()
        .filter(|r| !r.is_empty())
        .unwrap_or_else(|| r"C:\Windows".into());
    PathBuf::from(format!(
        r"{}\System32\WindowsPowerShell\v1.0\powershell.exe",
        root.trim_end_matches('\\')
    ))
}

/// What the administrator step exits with when it never ran setup.
#[cfg_attr(not(windows), allow(dead_code))]
mod exit {
    /// Windows' "the operation was cancelled by the user": the UAC prompt
    /// was declined, or could not be shown.
    pub const DECLINED: i32 = 1223;
    /// Latch could not be copied into `%ProgramFiles%\Latch\Setup`.
    pub const COPY_FAILED: i32 = 3;
    /// The copy is not the file this window was started from.
    pub const CHANGED: i32 = 4;
}

/// What the window says when the administrator prompt was turned down.
pub const ADMIN_DECLINED: &str = "Setup needs an administrator's approval, and Windows' prompt was declined. Try again and choose Yes.";

/// Why the administrator step failed, from its exit code.
#[cfg_attr(not(windows), allow(dead_code))]
fn elevation_error(code: Option<i32>) -> String {
    match code {
        Some(exit::DECLINED) => ADMIN_DECLINED.into(),
        Some(exit::COPY_FAILED) => {
            "setup could not copy Latch into Program Files to run it as administrator".into()
        }
        Some(exit::CHANGED) => "latch-host.exe changed on disk while Windows asked for approval, so setup did not run it. Open Latch again and retry.".into(),
        _ => "setup stopped part way; the setup log shows the last step reached".into(),
    }
}

/// The elevated first step, run by Windows' own PowerShell after the UAC
/// prompt. It copies `program` into `%ProgramFiles%\Latch\Setup`, checks
/// the copy is the file the window hashed (`sha256`), and runs the copy as
/// `--setup-elevated`. The elevated Latch therefore loads, runs and writes
/// nothing in a folder a user can change: the install folder under
/// `%LOCALAPPDATA%` is the user's, and an unzipped copy may sit in one
/// every user can write to. `program` is only named in the firewall rules.
#[cfg(any(windows, test))]
fn stage_script(program: &str, sha256: &str) -> String {
    format!(
        r#"$ErrorActionPreference = 'Stop'
$program = '{program}'
try {{
    $dir = Join-Path ([Environment]::GetFolderPath('ProgramFiles')) '{dir}'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    $copy = Join-Path $dir 'latch-host.exe'
    Copy-Item -LiteralPath $program -Destination $copy -Force
}} catch {{ exit {copy_failed} }}
if ((Get-FileHash -LiteralPath $copy -Algorithm SHA256).Hash -ne '{sha256}') {{
    Remove-Item -LiteralPath $copy -Force -ErrorAction SilentlyContinue
    exit {changed}
}}
# Started directly rather than through the shell, which would ask again
# about a downloaded file this window already runs.
$start = New-Object System.Diagnostics.ProcessStartInfo $copy
$start.Arguments = '--setup-elevated --program "' + $program + '"'
$start.UseShellExecute = $false
$p = [System.Diagnostics.Process]::Start($start)
$p.WaitForExit()
Remove-Item -LiteralPath $copy -Force -ErrorAction SilentlyContinue
exit $p.ExitCode
"#,
        program = q(program),
        dir = SETUP_DIR,
        sha256 = sha256,
        copy_failed = exit::COPY_FAILED,
        changed = exit::CHANGED,
    )
}

/// The unelevated launcher: Windows' PowerShell, elevated through UAC, runs
/// [`stage_script`]. The script travels on the command line, which UAC
/// fixes when it is approved, rather than in a file the user could edit.
#[cfg(any(windows, test))]
fn launch_script(stage: &str) -> String {
    format!(
        "try {{ $p = Start-Process -FilePath '{ps}' -Verb RunAs -Wait -PassThru -WindowStyle Hidden -ArgumentList '-NoProfile -NonInteractive -EncodedCommand {encoded}' -ErrorAction Stop }} catch {{ exit {declined} }}; if ($null -eq $p) {{ exit {declined} }}; exit $p.ExitCode",
        ps = q(&powershell_exe().display().to_string()),
        encoded = encoded_command(stage),
        declined = exit::DECLINED,
    )
}

/// A script as `-EncodedCommand` takes it: base64 of its UTF-16LE text,
/// which no quoting on the way there can change.
#[cfg(any(windows, test))]
fn encoded_command(script: &str) -> String {
    let utf16: Vec<u8> = script.encode_utf16().flat_map(u16::to_le_bytes).collect();
    base64(&utf16)
}

/// Standard base64 with padding, for `-EncodedCommand`.
#[cfg(any(windows, test))]
fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |n, (i, b)| n | u32::from(*b) << (16 - 8 * i));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

pub fn script(p: &Plan<'_>) -> String {
    let q = |s: &str| s.replace('\'', "''");
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
    format!(
        r#"# Latch sharing setup. Generated by latch-host.
$ErrorActionPreference = 'Continue'
function Step($m) {{ Write-Output "[$(Get-Date -Format HH:mm:ss)] $m" }}
$setupFailed = $false
Step "Latch setup started"
{retire}
Step "Opening TCP and UDP {port} to the tailnet for Latch"
# The first time a new latch-host.exe listens, Windows asks whether to allow
# it; unanswered (nobody at the PC, an install over SSH) that becomes a pair
# of inbound Block rules for the program, and a Block rule wins over every
# Allow below. Only rules for this very executable are removed.
$latchExe = '{exe}'
Get-NetFirewallApplicationFilter -ErrorAction SilentlyContinue | Where-Object {{ $_.Program -ieq $latchExe }} | Get-NetFirewallRule -ErrorAction SilentlyContinue | Where-Object {{ $_.Action -eq 'Block' -and $_.Direction -eq 'Inbound' }} | Remove-NetFirewallRule -ErrorAction SilentlyContinue
foreach ($rule in 'Latch Host', 'Latch Media') {{ netsh advfirewall firewall delete rule name="$rule" | Out-Null }}
# Tailscale's addresses only: its IPv4 range and its IPv6 one.
netsh advfirewall firewall add rule name="Latch Host" dir=in action=allow protocol=TCP localport={port} remoteip={tailnet} program='{exe}' | Out-Null
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
netsh advfirewall firewall add rule name="Latch Media" dir=in action=allow protocol=UDP localport={port} remoteip={tailnet} program='{exe}' | Out-Null
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
Step "Opening UDP 9 to check the wake path while this PC is awake"
netsh advfirewall firewall delete rule name="Latch wake" | Out-Null
netsh advfirewall firewall add rule name="Latch wake" dir=in action=allow protocol=UDP localport=9 program='{exe}' | Out-Null
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
{virtual_display}
Step "Recording the power settings this PC had before Latch"
{record}
Step "Turning Fast Startup off so a shut-down PC can wake"
try {{ Set-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -Value 0 -Type DWord -ErrorAction Stop }} catch {{ Write-Output "  Fast Startup: $_"; $setupFailed = $true }}
Step "Keeping this PC awake when plugged in so Tailscale stays reachable"
powercfg /change standby-timeout-ac 0
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
powercfg /change hibernate-timeout-ac 0
if ($LASTEXITCODE -ne 0) {{ $setupFailed = $true }}
{adapter}
if ($setupFailed) {{ Step "Some setup steps failed. See the lines above."; exit 1 }}
Step "Latch setup finished"
exit 0
"#,
        retire = crate::legacy::retire_windows_ps(),
        port = CONTROL_PORT,
        tailnet = "100.64.0.0/10,fd7a:115c:a1e0::/48",
        exe = q(&p.exe.display().to_string()),
        virtual_display = crate::virtual_display::setup_ps(),
        record = record_power_ps(),
    )
}

/// Where setup keeps what it changed, for `--uninstall` to put back.
/// Administrators' only, like everything setup writes.
const SAVED_KEY: &str = r"HKLM:\SOFTWARE\Latch\Setup";

/// The AC sleep and hibernate timeouts and Fast Startup as they were before
/// Latch's first setup, under [`SAVED_KEY`]. A later setup records nothing:
/// by then the values are its own. powercfg's labels are translated, so the
/// AC value is read as the second-to-last hex number it prints (the DC
/// value is the last).
fn record_power_ps() -> String {
    format!(
        r#"$latchSaved = '{key}'
if (-not (Test-Path -LiteralPath $latchSaved)) {{
    New-Item -Path $latchSaved -Force | Out-Null
    $hb = (Get-ItemProperty 'HKLM:\SYSTEM\CurrentControlSet\Control\Session Manager\Power' -Name HiberbootEnabled -ErrorAction SilentlyContinue).HiberbootEnabled
    if ($null -ne $hb) {{ New-ItemProperty -LiteralPath $latchSaved -Name HiberbootEnabled -Value "$hb" -PropertyType String -Force | Out-Null }}
    foreach ($s in @(@('StandbyTimeoutAC', 'STANDBYIDLE'), @('HibernateTimeoutAC', 'HIBERNATEIDLE'))) {{
        $hex = [regex]::Matches((powercfg /query SCHEME_CURRENT SUB_SLEEP $s[1] | Out-String), '0x[0-9a-fA-F]{{8}}')
        if ($hex.Count -ge 2) {{
            New-ItemProperty -LiteralPath $latchSaved -Name $s[0] -Value ([string][Convert]::ToUInt32($hex[$hex.Count - 2].Value, 16)) -PropertyType String -Force | Out-Null
        }}
    }}
}} else {{ Write-Output "  kept from an earlier setup" }}"#,
        key = SAVED_KEY
    )
}

pub fn run(p: &Plan<'_>) -> Result<()> {
    #[cfg(not(windows))]
    {
        crate::unix_setup::run(p)
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        static WARNED: parking_lot::Mutex<Option<std::time::Instant>> =
            parking_lot::Mutex::new(None);
        let viewer = latch_core::http::get_json::<latch_core::api::Status>(
            ("127.0.0.1", CONTROL_PORT),
            "/v1/status",
            std::time::Duration::from_secs(2),
        )
        .is_ok_and(|s| s.streamer.viewer);
        {
            let now = std::time::Instant::now();
            let mut warned = WARNED.lock();
            if refuse_during_stream(viewer, *warned, now) {
                *warned = Some(now);
                anyhow::bail!(SETUP_DURING_STREAM);
            }
            *warned = None;
        }
        // Pinned now: the elevated step runs only a copy of these bytes.
        let sha256 = latch_core::update::sha256_hex(
            &std::fs::read(p.exe).with_context(|| format!("read {}", p.exe.display()))?,
        );
        let launch = launch_script(&stage_script(&p.exe.display().to_string(), &sha256));
        let status = std::process::Command::new(powershell_exe())
            .args(["-NoProfile", "-NonInteractive", "-Command", &launch])
            .creation_flags(0x0800_0000)
            .status()
            .context("launch elevated Latch Host")?;
        anyhow::ensure!(status.success(), elevation_error(status.code()));
        Ok(())
    }
}

/// UAC needs someone at the PC even though a stream is already open.
pub const SETUP_DURING_STREAM: &str = "A stream is running. Windows asks for an administrator on this PC's own screen. Click again within two minutes to run setup with someone at the PC, or wait until the stream ends.";
#[cfg_attr(not(windows), allow(dead_code))]
const SETUP_CONFIRM_WINDOW: std::time::Duration = std::time::Duration::from_secs(120);

/// Whether a setup click is turned down: a viewer is connected, and this is
/// not the second click within [`SETUP_CONFIRM_WINDOW`] of the warning.
#[cfg_attr(not(windows), allow(dead_code))]
fn refuse_during_stream(
    viewer: bool,
    warned: Option<std::time::Instant>,
    now: std::time::Instant,
) -> bool {
    viewer && !warned.is_some_and(|at| now.duration_since(at) < SETUP_CONFIRM_WINDOW)
}

/// Run a read-only PowerShell probe and return what it printed.
#[cfg(windows)]
pub(crate) fn powershell(script: &str) -> Result<String> {
    use std::os::windows::process::CommandExt;
    let out = std::process::Command::new(powershell_exe())
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

// Only the Windows setup path is compiled in a release build; tests use it everywhere.
#[cfg_attr(not(windows), allow(dead_code))]
/// A PowerShell single-quoted literal: only `'` needs escaping, nothing is
/// expanded.
fn q(s: &str) -> String {
    s.replace('\'', "''")
}

/// Generate and run the setup script. Called from `--setup-elevated`,
/// already elevated: with `program`, by the copy [`stage_script`] made in
/// `%ProgramFiles%\Latch\Setup`; without, by an administrator's own
/// session (SSH), which stages this file the same way first.
pub fn run_as_admin(program: Option<&Path>) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        let exe = std::env::current_exe().context("own path")?;
        let Some(program) = program else {
            let sha256 = latch_core::update::sha256_hex(&std::fs::read(&exe)?);
            let stage = stage_script(&exe.display().to_string(), &sha256);
            let status = std::process::Command::new(powershell_exe())
                .args(["-NoProfile", "-NonInteractive", "-EncodedCommand"])
                .arg(encoded_command(&stage))
                .creation_flags(0x0800_0000)
                .status()
                .context("run the setup copy")?;
            anyhow::ensure!(status.success(), elevation_error(status.code()));
            return Ok(());
        };
        // Everything below is written beside this copy, so it has to be
        // the one in Program Files and not a file a user put anywhere.
        let dir = exe.parent().context("own folder")?.to_path_buf();
        let protected = std::env::var_os("ProgramFiles")
            .map(|pf| PathBuf::from(pf).join(SETUP_DIR))
            .and_then(|d| d.canonicalize().ok());
        anyhow::ensure!(
            protected.is_some_and(|p| dir.canonicalize().is_ok_and(|d| same_path(&d, &p))),
            "--program is only for the copy setup makes in Program Files"
        );
        anyhow::ensure!(
            plausible_program(&program.display().to_string()) && program.is_file(),
            "{} is not an installed latch-host.exe",
            program.display()
        );
        let wake = crate::wake::probe();
        let plan = Plan {
            exe: program,
            adapter: &wake.adapter,
            adapter_description: &wake.description,
        };
        // PowerShell 5.1 reads a BOM-less file as the system ANSI code page, so
        // a Korean/Japanese username or adapter name ("이더넷") would be mangled
        // and the firewall rule would point at a path that does not exist.
        // We write to a file rather than piping to stdin or -EncodedCommand to preserve
        // reliable UTF-8 BOM detection and avoid Windows command-line length limits.
        // The folder is administrators' only; create_new with a random name
        // still never opens a file that is already there.
        use std::io::Write;
        let mut bytes = b"\xEF\xBB\xBF".to_vec();
        bytes.extend(script(&plan).as_bytes());
        let mut tmp = None;
        for _ in 0..5 {
            let candidate = dir.join(format!(
                "latch-setup-{}-{:032x}.ps1",
                std::process::id(),
                rand::random::<u128>()
            ));
            match std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)
            {
                Ok(mut file) => {
                    file.write_all(&bytes)?;
                    tmp = Some(candidate);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e).context("create the setup script"),
            }
        }
        let tmp = tmp.context("could not allocate unique setup script path")?;
        let log = dir.join("setup.log");
        let out = std::fs::File::create(&log)?;
        let err = out.try_clone()?;
        let status = std::process::Command::new(powershell_exe())
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
    {
        let _ = program;
        anyhow::bail!("setup runs on Windows only")
    }
}

/// Whether two canonical paths name the same folder: Windows compares
/// names without regard to case.
#[cfg(windows)]
fn same_path(a: &Path, b: &Path) -> bool {
    a.as_os_str()
        .to_string_lossy()
        .eq_ignore_ascii_case(&b.as_os_str().to_string_lossy())
}

/// What the staging step may pass as `--program`: a full local path to an
/// `.exe`. It is only ever named in firewall rules, never run or written.
#[cfg(any(windows, test))]
fn plausible_program(path: &str) -> bool {
    let b = path.as_bytes();
    b.len() > 7
        && b[0].is_ascii_alphabetic()
        && b[1] == b':'
        && b[2] == b'\\'
        && path.to_ascii_lowercase().ends_with(".exe")
        && !path.contains(['"', '\n', '\r'])
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
    fn setup_opens_both_control_transports_and_keeps_wake_and_display_steps() {
        let s = script(&Plan {
            exe: Path::new("C:\\Users\\O'Brien\\Latch\\latch-host.exe"),
            adapter: "Ethernet",
            adapter_description: "Realtek",
        });
        // Tailscale's IPv4 range and its IPv6 one (IN-10).
        assert!(
            s.contains("protocol=TCP localport=47850 remoteip=100.64.0.0/10,fd7a:115c:a1e0::/48")
        );
        assert!(
            s.contains("protocol=UDP localport=47850 remoteip=100.64.0.0/10,fd7a:115c:a1e0::/48")
        );
        assert!(s.contains("O''Brien"));
        assert!(s.contains("HiberbootEnabled"));
        assert!(s.contains("*WakeOnMagicPacket"));
        assert!(s.contains("vdd_settings.xml"));
        assert!(!s.contains("Invoke-WebRequest"));
        assert!(!s.contains("--creds"));
    }

    /// The power settings setup changes are recorded first, once, so that
    /// `--uninstall` can put them back (IN-11).
    #[test]
    fn setup_records_the_power_settings_before_changing_them() {
        let s = script(&Plan {
            exe: Path::new("C:\\Latch\\latch-host.exe"),
            adapter: "",
            adapter_description: "",
        });
        let record = s
            .find(r"$latchSaved = 'HKLM:\SOFTWARE\Latch\Setup'")
            .unwrap();
        let only_once = s
            .find("if (-not (Test-Path -LiteralPath $latchSaved))")
            .unwrap();
        for change in [
            "-Name HiberbootEnabled -Value 0",
            "standby-timeout-ac 0",
            "hibernate-timeout-ac 0",
        ] {
            assert!(record < s.find(change).unwrap(), "{change}");
        }
        assert!(record < only_once);
        for name in ["HiberbootEnabled", "StandbyTimeoutAC", "HibernateTimeoutAC"] {
            assert!(
                s.contains(&format!("-Name {name}")) || s.contains(&format!("'{name}'")),
                "{name}"
            );
        }
        assert!(s.contains("powercfg /query SCHEME_CURRENT SUB_SLEEP $s[1]"));
    }

    #[test]
    fn setup_during_a_stream_needs_a_second_click() {
        let now = std::time::Instant::now();
        assert!(refuse_during_stream(true, None, now));
        assert!(!refuse_during_stream(true, Some(now), now));
        assert!(refuse_during_stream(
            true,
            Some(now),
            now + SETUP_CONFIRM_WINDOW
        ));
        assert!(!refuse_during_stream(false, None, now));
    }

    #[test]
    fn the_run_key_quotes_the_path() {
        assert!(run_value(Path::new("C:/a b/latch-host.exe")).starts_with('\"'));
    }

    /// The UAC prompt runs Windows' own PowerShell, which copies Latch into
    /// Program Files, checks the copy is the pinned file and runs the copy:
    /// nothing elevated loads or runs from the user's folder, and no folder
    /// the caller names is written to (IN-01, IN-02).
    #[test]
    fn the_elevated_step_runs_a_checked_copy_from_program_files() {
        let program = r"C:\Users\O'Brien $x\AppData\Local\Latch\latch-host.exe";
        let stage = stage_script(program, &"ab".repeat(32));
        assert!(
            stage.contains(r"$program = 'C:\Users\O''Brien $x\AppData\Local\Latch\latch-host.exe'"),
            "{stage}"
        );
        assert!(stage.contains(r"[Environment]::GetFolderPath('ProgramFiles')) 'Latch\Setup'"));
        let copy = stage
            .find("Copy-Item -LiteralPath $program -Destination $copy")
            .unwrap();
        let check = stage
            .find("Get-FileHash -LiteralPath $copy -Algorithm SHA256")
            .unwrap();
        let run = stage.find("ProcessStartInfo $copy").unwrap();
        assert!(
            copy < check && check < run,
            "copy, then check, then run the copy"
        );
        assert!(stage.contains(&format!("-ne '{}'", "ab".repeat(32))));
        assert!(stage.contains("'--setup-elevated --program \"' + $program + '\"'"));
        assert!(!stage.contains("--local-app-data"));
        assert!(!stage.contains("ProcessStartInfo $program"));

        let launch = launch_script(&stage);
        assert!(
            launch.contains(
                r"-FilePath 'C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe' -Verb RunAs"
            ) || std::env::var_os("SystemRoot").is_some(),
            "{launch}"
        );
        assert!(
            !launch.contains("latch-host.exe"),
            "the user's file is never elevated: {launch}"
        );
        let encoded = launch
            .split("-EncodedCommand ")
            .nth(1)
            .and_then(|rest| rest.split('\'').next())
            .unwrap();
        let utf16 = stage
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>();
        assert_eq!(encoded, base64(&utf16));
        assert_eq!(encoded, encoded_command(&stage));
        assert!(launch.contains("catch { exit 1223 }"));
    }

    #[test]
    fn base64_matches_the_rfc_vectors() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(plain.as_bytes()), coded);
        }
        assert_eq!(base64(&[0xFF, 0xFE, 0xFD]), "//79");
    }

    /// A declined prompt says so plainly and points at no log: nothing ran,
    /// and the log on disk is an older run's (IN-09).
    #[test]
    fn a_declined_prompt_is_not_reported_as_a_failed_setup() {
        let declined = elevation_error(Some(1223));
        assert_eq!(declined, ADMIN_DECLINED);
        assert!(!declined.contains("setup.log") && !declined.contains("AppData"));
        assert!(elevation_error(Some(4)).contains("changed"));
        assert!(elevation_error(Some(1)).contains("setup log"));
        assert!(elevation_error(None).contains("setup log"));
    }

    #[test]
    fn only_a_full_path_to_an_exe_is_named_in_the_firewall_rules() {
        assert!(plausible_program(
            r"C:\Users\Ada Lovelace\AppData\Local\Latch\latch-host.exe"
        ));
        assert!(plausible_program(r"D:\Latch\LATCH-HOST.EXE"));
        for bad in [
            "latch-host.exe",
            r"..\latch-host.exe",
            r"\\server\share\latch-host.exe",
            r"C:\Latch\latch-host.exe\",
            r"C:\Latch\setup.ps1",
            "C:\\Latch\\a\"b.exe",
        ] {
            assert!(!plausible_program(bad), "{bad}");
        }
    }

    #[test]
    fn setup_lifts_windows_block_rules_for_this_executable_only() {
        let s = script(&Plan {
            exe: Path::new("C:\\Users\\O'Brien\\latch-host.exe"),
            adapter: "",
            adapter_description: "",
        });
        assert!(
            s.contains("$latchExe = 'C:\\Users\\O''Brien\\latch-host.exe'"),
            "{s}"
        );
        assert!(s.contains("$_.Program -ieq $latchExe"));
        assert!(s.contains("$_.Action -eq 'Block' -and $_.Direction -eq 'Inbound'"));
        let lift = s.find("Remove-NetFirewallRule").unwrap();
        let allow = s.find("add rule name=\"Latch Host\"").unwrap();
        assert!(
            lift < allow,
            "block rules go before the allow rules are added"
        );
    }

    #[test]
    fn the_generated_script_can_be_dumped_for_a_syntax_check() {
        let Ok(out) = std::env::var("LATCH_DUMP_SETUP_SCRIPT") else {
            return;
        };
        let exe =
            std::env::var("LATCH_DUMP_EXE").unwrap_or_else(|_| "C:\\Latch\\latch-host.exe".into());
        let adapter = std::env::var("LATCH_DUMP_ADAPTER").unwrap_or_default();
        let description = std::env::var("LATCH_DUMP_ADAPTER_DESC").unwrap_or_default();
        std::fs::write(
            out,
            script(&Plan {
                exe: Path::new(&exe),
                adapter: &adapter,
                adapter_description: &description,
            }),
        )
        .unwrap();
        // The elevated staging step and the one-line launcher around it.
        if let Ok(out) = std::env::var("LATCH_DUMP_STAGE_SCRIPT") {
            let stage = stage_script(&exe, &"0".repeat(64));
            std::fs::write(format!("{out}.launch.ps1"), launch_script(&stage)).unwrap();
            std::fs::write(out, stage).unwrap();
        }
    }
}
