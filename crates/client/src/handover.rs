//! Getting a new Latch Host onto a PC whose host is too old to take
//! `/v1/update` (before 3.1, so one still named BroLink), with nobody at
//! the PC: the stream itself is the way in.
//!
//! The Mac serves the new `latch-host.exe` and a small PowerShell script
//! on its own Tailscale address, to that one PC only, for a few minutes.
//! Then it presses Win+R on the PC through the stream, types one line
//! (`powershell … irm http://<mac>:47851/u.ps1 | iex`) and presses Enter.
//! The script on the PC fetches the executable from the Mac, checks its
//! SHA-256 against the one baked into the script, asks the old service to
//! stop, swaps the file in place and starts the new one. From then on the
//! ordinary `/v1/update` route works. Nothing secret is typed: the Mac's
//! server is what has the file.

use crate::input::{self, press_chord};
use anyhow::{anyhow, Context, Result};
use latch_core::http;
use latch_core::update::{self, Release};
use latch_core::{legacy, HANDOVER_PORT};
use latch_stream::Input;
use parking_lot::Mutex;
use semver::Version;
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long the Mac keeps serving after typing the command.
const SERVE_FOR: Duration = Duration::from_secs(10 * 60);
/// After the PC has fetched the executable there is nothing more to serve;
/// a little grace in case the script retries.
const AFTER_FETCH: Duration = Duration::from_secs(60);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Phase {
    /// Fetching the release from GitHub.
    Preparing,
    /// The command has been typed; waiting for the PC to fetch.
    Serving,
    /// The PC has downloaded the executable; it is installing.
    Fetched,
    Failed(String),
    /// Stopped or timed out without the PC fetching.
    Ended,
}

#[derive(Debug, Clone)]
pub struct Progress {
    pub phase: Phase,
    pub version: Option<Version>,
    pub since: Instant,
}

pub struct Handover {
    progress: Arc<Mutex<Progress>>,
    stop: Arc<AtomicBool>,
}

impl Handover {
    /// Serve `release`'s host (fetching it first) and type the install
    /// command on the PC at `pc_ip` through `input`. `mac_ip` is this Mac's
    /// Tailscale address, the only one the PC can reach. `configured_token`
    /// is the one in Settings; without it, git's is asked for here, off
    /// the window's thread.
    pub fn start(
        release: Option<Release>,
        configured_token: Option<String>,
        mac_ip: Ipv4Addr,
        pc_ip: Ipv4Addr,
        input: Input,
        ctx: egui::Context,
    ) -> Self {
        let progress = Arc::new(Mutex::new(Progress {
            phase: Phase::Preparing,
            version: None,
            since: Instant::now(),
        }));
        let stop = Arc::new(AtomicBool::new(false));
        {
            let progress = progress.clone();
            let stop = stop.clone();
            std::thread::Builder::new()
                .name("handover".into())
                .spawn(move || {
                    let token = update::token(configured_token.as_deref());
                    let outcome = run(
                        release,
                        token.as_deref(),
                        mac_ip,
                        pc_ip,
                        &input,
                        &progress,
                        &stop,
                        &ctx,
                    );
                    let mut p = progress.lock();
                    match outcome {
                        Ok(()) if p.phase == Phase::Fetched => {}
                        Ok(()) => p.phase = Phase::Ended,
                        Err(e) => p.phase = Phase::Failed(format!("{e:#}")),
                    }
                    p.since = Instant::now();
                    ctx.request_repaint();
                })
                .expect("spawn handover thread");
        }
        Self { progress, stop }
    }

    pub fn progress(&self) -> Progress {
        self.progress.lock().clone()
    }

    /// One line for the toolbar.
    pub fn status(&self, pc: &str) -> String {
        let p = self.progress.lock();
        let v = p
            .version
            .as_ref()
            .map(|v| v.to_string())
            .unwrap_or_else(|| "the new Latch Host".into());
        match &p.phase {
            Phase::Preparing => format!("Fetching {v} from GitHub…"),
            Phase::Serving => format!(
                "Typed the install command on {pc}; waiting for it to fetch {v} ({}s)",
                p.since.elapsed().as_secs()
            ),
            Phase::Fetched => format!("{pc} has fetched {v} and is installing it…"),
            Phase::Failed(e) => format!("Could not install on {pc}: {e}"),
            Phase::Ended => format!("{pc} never fetched {v}. Is its desktop unlocked?"),
        }
    }

    pub fn active(&self) -> bool {
        matches!(
            self.progress.lock().phase,
            Phase::Preparing | Phase::Serving | Phase::Fetched
        )
    }

    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for Handover {
    fn drop(&mut self) {
        self.stop();
    }
}

/// The one line typed into the Run box on the PC.
pub fn command(mac_ip: Ipv4Addr) -> String {
    format!("powershell -ep bypass -c \"irm http://{mac_ip}:{HANDOVER_PORT}/u.ps1|iex\"")
}

/// The script the PC runs. It finds the running host by its process, under
/// either name, or the file where an install put it, and replaces that file
/// where it is: a host this old is a BroLink one, and its firewall rules
/// name that path. A fresh folder is made only when there is none.
pub fn script(mac_ip: Ipv4Addr, version: &Version, sha256_hex: &str) -> String {
    format!(
        r#"# Latch Host {version}: installed from your Mac through the stream.
$ErrorActionPreference = 'Stop'
$src = 'http://{mac_ip}:{port}'
$host.UI.RawUI.WindowTitle = 'Latch Host update'
Write-Host "Installing Latch Host {version} from your Mac..."
$p = (Get-Process latch-host, {old_process} -ErrorAction SilentlyContinue | Where-Object Path | Select-Object -First 1).Path
if (-not $p) {{ foreach ($c in (Join-Path $env:LOCALAPPDATA 'Latch\latch-host.exe'), (Join-Path $env:LOCALAPPDATA '{old_exe}')) {{ if (Test-Path $c) {{ $p = $c; break }} }} }}
if (-not $p) {{ $p = Join-Path $env:LOCALAPPDATA 'Latch\latch-host.exe' }}
$dir = Split-Path -Parent $p
New-Item -ItemType Directory -Force -Path $dir | Out-Null
Write-Host "Downloading to $p.new"
Invoke-WebRequest -UseBasicParsing "$src/latch-host.exe" -OutFile "$p.new"
$h = (Get-FileHash "$p.new" -Algorithm SHA256).Hash
if ($h -ne '{sha}') {{ Remove-Item "$p.new" -Force; throw "the download did not match its SHA-256" }}
Write-Host "Stopping the old service"
try {{ Invoke-RestMethod -Method Post -Uri http://127.0.0.1:47850/v1/quit -ContentType 'application/json' -Body '{{}}' -TimeoutSec 3 | Out-Null }} catch {{}}
Start-Sleep -Milliseconds 800
Get-Process latch-host, {old_process} -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Milliseconds 400
if (Test-Path $p) {{ Move-Item $p "$p.old" -Force }}
Move-Item "$p.new" $p -Force
Remove-Item "$p.old" -Force -ErrorAction SilentlyContinue
New-ItemProperty -Path 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run' -Name LatchHost -Value "`"$p`" --background" -PropertyType String -Force | Out-Null
Start-Process -FilePath $p -ArgumentList '--background' -WorkingDirectory $dir
Write-Host "Done: Latch Host {version} is running. This window closes in 5 seconds."
Start-Sleep 5
"#,
        version = version,
        mac_ip = mac_ip,
        port = HANDOVER_PORT,
        sha = sha256_hex.to_uppercase(),
        old_process = legacy::WINDOWS_HOST_PROCESS,
        old_exe = legacy::WINDOWS_HOST_EXE,
    )
}

/// The release worth installing this way: 3.1 or newer, or there is no
/// point (an older host has no update route either).
pub fn usable(rel: &Release, running: &Version) -> Result<()> {
    anyhow::ensure!(
        update::host_can_receive_update(&rel.version),
        "the newest release on GitHub is {}, which has no update route either; publish {} or newer first",
        rel.version,
        update::first_update()
    );
    anyhow::ensure!(
        rel.version > *running,
        "the newest release on GitHub is {}, and the PC already runs {running}",
        rel.version
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn run(
    release: Option<Release>,
    token: Option<&str>,
    mac_ip: Ipv4Addr,
    pc_ip: Ipv4Addr,
    input: &Input,
    progress: &Mutex<Progress>,
    stop: &AtomicBool,
    ctx: &egui::Context,
) -> Result<()> {
    let rel = match release {
        Some(r) => r,
        None => update::latest(token).context("look up the latest release")?,
    };
    progress.lock().version = Some(rel.version.clone());
    ctx.request_repaint();
    let exe = crate::update::host_exe(&rel, token).context("fetch latch-host.exe")?;
    let sha = update::sha256_hex(&exe);
    let script = script(mac_ip, &rel.version, &sha);
    let listener = TcpListener::bind(SocketAddr::from((mac_ip, HANDOVER_PORT)))
        .with_context(|| format!("listen on {mac_ip}:{HANDOVER_PORT}"))?;
    listener.set_nonblocking(true)?;
    anyhow::ensure!(input.connected(), "the stream ended");

    // Win+R, the command, Enter. The Run box takes a moment to appear.
    press_chord(input, &[input::VK_LWIN, input::VK_R], 0);
    std::thread::sleep(Duration::from_millis(1200));
    input.text(&command(mac_ip));
    std::thread::sleep(Duration::from_millis(600));
    press_chord(input, &[input::VK_RETURN], 0);
    {
        let mut p = progress.lock();
        p.phase = Phase::Serving;
        p.since = Instant::now();
    }
    ctx.request_repaint();

    let started = Instant::now();
    let mut fetched_at: Option<Instant> = None;
    loop {
        if stop.load(Ordering::Relaxed) {
            return Ok(());
        }
        if started.elapsed() > SERVE_FOR {
            return Ok(());
        }
        if fetched_at.is_some_and(|t| t.elapsed() > AFTER_FETCH) {
            return Ok(());
        }
        match accept(&listener) {
            Ok(Some((mut stream, peer))) => {
                let _ = stream.set_read_timeout(Some(Duration::from_secs(10)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(180)));
                if peer.ip() != pc_ip {
                    tracing::warn!("handover: refused {peer}");
                    let _ = http::write_bytes(&mut stream, 403, "text/plain", b"no");
                    continue;
                }
                let req = match http::read_request(&mut stream) {
                    Ok(Some(r)) => r,
                    _ => continue,
                };
                match (req.method.as_str(), req.path.as_str()) {
                    ("GET", "/u.ps1") => {
                        tracing::info!("handover: {peer} fetched the script");
                        let _ = http::write_bytes(
                            &mut stream,
                            200,
                            "text/plain; charset=utf-8",
                            script.as_bytes(),
                        );
                    }
                    ("GET", "/latch-host.exe") => {
                        tracing::info!("handover: {peer} fetching the executable");
                        let r =
                            http::write_bytes(&mut stream, 200, "application/octet-stream", &exe);
                        if r.is_ok() {
                            fetched_at = Some(Instant::now());
                            let mut p = progress.lock();
                            p.phase = Phase::Fetched;
                            p.since = Instant::now();
                            ctx.request_repaint();
                        } else {
                            tracing::warn!("handover: sending the executable: {r:?}");
                        }
                    }
                    _ => {
                        let _ = http::write_bytes(&mut stream, 404, "text/plain", b"no");
                    }
                }
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(150)),
            Err(e) => return Err(anyhow!("accept: {e}")),
        }
    }
}

/// The next connection, if one is waiting. The listener is non-blocking so
/// the loop can notice a stop, and on macOS a socket accepted from it
/// inherits that: the first read found nothing yet, and writing the 20 MB
/// executable failed the moment the send buffer filled, so the PC got a
/// truncated file. The connection itself blocks, within its timeouts.
fn accept(listener: &TcpListener) -> std::io::Result<Option<(TcpStream, SocketAddr)>> {
    match listener.accept() {
        Ok((stream, peer)) => {
            stream.set_nonblocking(false)?;
            Ok(Some((stream, peer)))
        }
        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => Ok(None),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_typed_line_is_short_and_the_script_checks_the_hash() {
        let ip: Ipv4Addr = "100.64.0.20".parse().unwrap();
        let cmd = command(ip);
        assert!(cmd.len() < 100, "{cmd}");
        assert!(cmd.contains("http://100.64.0.20:47851/u.ps1"), "{cmd}");
        assert!(cmd.starts_with("powershell -ep bypass"), "{cmd}");
        let s = script(ip, &Version::new(3, 1, 0), "abcdef");
        assert!(s.contains("$src = 'http://100.64.0.20:47851'"), "{s}");
        assert!(s.contains("-ne 'ABCDEF'"), "the hash is compared uppercase");
        // A host this old runs as BroLink: found under either name, and
        // replaced where it is, so its firewall rules still apply.
        assert!(s.contains("Get-Process latch-host, brolink-host"), "{s}");
        assert!(
            s.contains(
                "Get-Process latch-host, brolink-host -ErrorAction SilentlyContinue | Stop-Process"
            ),
            "both are stopped: {s}"
        );
        assert!(s.contains("'BroLink\\brolink-host.exe'"), "{s}");
        assert!(s.contains("Latch\\latch-host.exe"), "{s}");
        let found = s.find("Test-Path $c").unwrap();
        let fresh = s.rfind("if (-not $p) { $p = Join-Path").unwrap();
        assert!(
            found < fresh,
            "a new folder only when no install was found: {s}"
        );
        assert!(s.contains("/v1/quit"), "{s}");
        assert!(s.contains("--background"), "{s}");
        assert!(!s.contains("ghp_"), "no token in the script");
        assert!(!s.contains("{{"), "no leftover braces: {s}");
    }

    #[test]
    fn a_large_reply_goes_out_whole_from_the_non_blocking_listener() {
        use std::io::{Read, Write};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        assert!(accept(&listener).unwrap().is_none(), "nothing waiting yet");
        let addr = listener.local_addr().unwrap();
        let body = vec![7u8; 16 * 1024 * 1024];
        let reader = std::thread::spawn(move || {
            let mut c = TcpStream::connect(addr).unwrap();
            // Let the sender fill the socket buffer before anything is read.
            std::thread::sleep(Duration::from_millis(200));
            let mut got = Vec::new();
            c.read_to_end(&mut got).unwrap();
            got.len()
        });
        let (mut stream, _) = loop {
            if let Some(conn) = accept(&listener).unwrap() {
                break conn;
            }
            std::thread::sleep(Duration::from_millis(10));
        };
        stream
            .set_write_timeout(Some(Duration::from_secs(10)))
            .unwrap();
        stream.write_all(&body).unwrap();
        drop(stream);
        assert_eq!(reader.join().unwrap(), body.len());
    }

    #[test]
    fn only_a_release_with_an_update_route_is_worth_typing() {
        let rel = |v: &str| Release {
            version: Version::parse(v).unwrap(),
            tag: format!("v{v}"),
            prerelease: false,
            assets: vec![],
        };
        let running = Version::new(3, 0, 0);
        let e = usable(&rel("3.0.1"), &running).unwrap_err().to_string();
        assert!(e.contains("3.0.1") && e.contains("publish 3.1.0"), "{e}");
        assert!(usable(&rel("3.1.0"), &running).is_ok());
        assert!(usable(&rel("3.2.0"), &running).is_ok());
        let e = usable(&rel("3.1.0"), &Version::new(3, 1, 0))
            .unwrap_err()
            .to_string();
        assert!(e.contains("already runs"), "{e}");
    }
}
