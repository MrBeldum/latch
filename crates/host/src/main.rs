//! Latch. One executable, two jobs:
//!
//! * `--background`: the control service other machines talk to.
//! * no flags: the window — a list of machines to connect to, and setup
//!   to share this one.
//!
//! `--replaces <pid>` is how an update hands over.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::{Context, Result};
use clap::Parser;
use latch_core::config::data_dir;
use latch_host::logfile::RotatingLog;
use latch_host::product::NodeApp;
use latch_host::service::{self, Service};

#[derive(Parser, Debug)]
#[command(
    name = "latch-host",
    version,
    about = "Latch: your machines, on any of your machines"
)]
struct Args {
    /// Run the control service with no window.
    #[arg(long)]
    background: bool,
    /// Started by an update: wait for this process to give up the port.
    #[arg(long, requires = "background")]
    replaces: Option<u32>,
    /// Started by an update from the executable it retired: watch the new
    /// service (this process id) and put the retired one back if it never
    /// answers. See `update::probation`.
    #[arg(
        long,
        value_name = "PID",
        hide = true,
        requires_all = ["background", "probation_version", "probation_exe"]
    )]
    probation: Option<u32>,
    #[arg(long, hide = true)]
    probation_version: Option<String>,
    #[arg(long, hide = true)]
    probation_exe: Option<std::path::PathBuf>,
    /// Set up sharing, then exit. Used by Mac/Linux
    /// setup and by the Docker node; on Windows this is the same path as
    /// Share this machine (UAC).
    #[arg(long, conflicts_with_all = ["background"])]
    setup: bool,
    /// Undo setup and the installer: stop the service and remove its login
    /// item, firewall rules, tasks and the power settings setup changed
    /// (Windows asks for an administrator once). The program and its
    /// settings stay.
    #[arg(long, conflicts_with_all = ["background", "setup", "setup_elevated"])]
    uninstall: bool,
    /// Run setup as administrator. The unelevated panel launches this so
    /// the script is generated after UAC, not from a user-writable file.
    #[arg(long, conflicts_with_all = ["background"])]
    setup_elevated: bool,
    /// Accepted from 5.0.0's panel and docs, and ignored: the elevated step
    /// writes nothing under a folder its caller names.
    #[arg(long, value_name = "DIR", requires = "setup_elevated", hide = true)]
    local_app_data: Option<std::path::PathBuf>,
    /// The installed latch-host.exe the firewall rules name, passed by the
    /// staging step to the copy it runs from Program Files.
    #[arg(long, value_name = "EXE", requires = "setup_elevated", hide = true)]
    program: Option<std::path::PathBuf>,
}

/// The log file under the data directory, if this run keeps one. The
/// elevated setup step keeps none: the data directory is the signed-in
/// user's, and an administrator's process must not write where that user
/// can point a link (setup.log is written beside its protected copy).
fn log_file(args: &Args) -> Option<&'static str> {
    if args.setup_elevated {
        None
    } else if args.background {
        Some("service.log")
    } else {
        Some("panel.log")
    }
}

fn main() -> Result<()> {
    // Before anything can replace it: which file this process is.
    let _ = service::started_from();
    let args = Args::parse();
    let log = log_file(&args);
    init_logging(log);
    install_panic_hook(log.and_then(|log| data_dir().ok().map(|d| d.join(log))));
    #[cfg(target_os = "macos")]
    if let Err(e) = latch_host::unix_setup::adopt_legacy_bundle(args.background) {
        // Only returns when there was nothing to move or the move failed;
        // either way this copy runs from where it is.
        tracing::warn!("could not move into Latch.app: {e:#}");
    }
    if let (Some(pid), Some(version), Some(exe)) =
        (args.probation, &args.probation_version, &args.probation_exe)
    {
        return latch_host::update::probation(exe, pid, version).inspect_err(|e| {
            tracing::error!("the update watcher failed: {e:#}");
        });
    }
    if args.background {
        // Started by a window or a supervisor, nobody sees stderr: the
        // reason it could not start belongs in its log.
        return Service::new()
            .run_arc(args.replaces.is_some())
            .inspect_err(|e| tracing::error!("the service could not start: {e:#}"));
    }
    if args.setup {
        return run_setup();
    }
    if args.setup_elevated {
        return latch_host::setup::run_as_admin(args.program.as_deref());
    }
    if args.uninstall {
        let result = latch_host::setup::uninstall();
        let text = match &result {
            Ok(done) => done.join("\n"),
            Err(e) => format!("{e:#}"),
        };
        latch_host::setup::tell("Uninstall Latch", &text, result.is_err());
        return result.map(drop);
    }
    if service::starts_with_window(&latch_host::config::HostConfig::load()) {
        service::ensure_service_running();
    }
    eframe::run_native(
        "Latch",
        latch_client::native_options(),
        Box::new(|cc| Ok(Box::new(NodeApp::new(cc)))),
    )
    .map_err(|e| anyhow::anyhow!("{e}"))
}

fn run_setup() -> Result<()> {
    let exe = std::env::current_exe()?;
    latch_host::setup::run(&latch_host::setup::Plan {
        exe: &exe,
        adapter: "",
        adapter_description: "",
    })
}

/// What is logged when RUST_LOG says nothing.
///
/// wgpu's Vulkan backend warns once for every presented frame whose
/// swapchain reports suboptimal (`wgpu-hal`, `vulkan/mod.rs`). On Hermes
/// that is every frame the window draws, and it was **every line** of a
/// 32 MB `panel.log` — the condition is normal and wgpu recreates the
/// swapchain itself, so there is nothing to act on. Latch stays at info
/// and that one target is heard from only when it is an error.
///
/// The PulseAudio client (Linux desktop audio) logs "Reactor error: Client
/// disconnected" at error level whenever Latch closes its capture stream at
/// the end of a session, which is every session; a capture that really
/// fails is reported by the engine itself ("sound stopped: …").
const DEFAULT_LOG: &str = "info,wgpu_hal=error,pulseaudio::client::reactor=off";

/// Release builds abort on a panic, in any thread. Without this the reason
/// went to stderr, which nothing keeps (a launchd agent, a Windows Run-key
/// process), and the log simply stopped. The line is appended to `log`
/// directly rather than through tracing, whose writer may be the very lock
/// the panic happened under; the default hook still runs after it.
fn install_panic_hook(log: Option<std::path::PathBuf>) {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let line = panic_line(
            latch_core::dates::now_unix(),
            std::thread::current().name(),
            &info.to_string(),
        );
        if let Some(path) = &log {
            use std::io::Write;
            if let Ok(mut f) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
            {
                let _ = f.write_all(line.as_bytes());
            }
        }
        previous(info);
    }));
}

/// `2026-10-17T16:46:33Z ERROR panic in thread 'main': <message and place>`,
/// in the log's own format.
fn panic_line(unix: u64, thread: Option<&str>, info: &str) -> String {
    format!(
        "{} ERROR panic in thread '{}': {}\n",
        latch_core::dates::utc_timestamp(unix),
        thread.unwrap_or("unnamed"),
        info.replace('\n', " ")
    )
}

/// Logs go to a file in the data directory: neither mode has a console.
fn init_logging(file: Option<&str>) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
    let sink: Box<dyn std::io::Write + Send> = match file
        .context("no log file")
        .and_then(|file| Ok(RotatingLog::open(data_dir()?.join(file))?))
    {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(std::io::stderr()),
    };
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_ansi(false)
        .with_writer(std::sync::Mutex::new(sink))
        .init();
}

#[cfg(test)]
mod tests {
    use super::{log_file, panic_line, Args, DEFAULT_LOG};
    use clap::Parser;
    use tracing_subscriber::EnvFilter;

    /// The elevated step opens no file under the data directory, and a
    /// caller-supplied folder changes nothing (IN-02).
    #[test]
    fn the_elevated_step_logs_into_no_user_folder() {
        let before = std::env::var_os("LOCALAPPDATA");
        let args = Args::try_parse_from([
            "latch-host",
            "--setup-elevated",
            "--local-app-data",
            r"C:\Users\someone\junction",
        ])
        .unwrap();
        assert_eq!(log_file(&args), None);
        assert_eq!(std::env::var_os("LOCALAPPDATA"), before);
        let staged = Args::try_parse_from([
            "latch-host",
            "--setup-elevated",
            "--program",
            r"C:\Users\Ada\AppData\Local\Latch\latch-host.exe",
        ])
        .unwrap();
        assert_eq!(log_file(&staged), None);
        assert!(staged.program.is_some());
        assert!(Args::try_parse_from(["latch-host", "--program", "x.exe"]).is_err());
        assert!(Args::try_parse_from(["latch-host", "--uninstall", "--background"]).is_err());
        let uninstall = Args::try_parse_from(["latch-host", "--uninstall"]).unwrap();
        assert_eq!(log_file(&uninstall), Some("panel.log"));
        let service = Args::try_parse_from(["latch-host", "--background"]).unwrap();
        assert_eq!(log_file(&service), Some("service.log"));
        let window = Args::try_parse_from(["latch-host"]).unwrap();
        assert_eq!(log_file(&window), Some("panel.log"));
    }

    #[test]
    fn a_panic_leaves_a_line_in_the_log() {
        let line = panic_line(
            1_792_258_000,
            Some("refresh"),
            "panicked at crates/host/src/service.rs:212:9:\nindex out of bounds",
        );
        assert!(line.starts_with("2026-10-17T"), "{line}");
        assert!(
            line.contains("panic in thread 'refresh': panicked at crates/host/src/service.rs:212:9: index out of bounds"),
            "{line}"
        );
        assert!(line.ends_with('\n') && line.matches('\n').count() == 1);
        assert!(panic_line(0, None, "x").contains("thread 'unnamed'"));
    }

    /// A typo here would be silent: `EnvFilter` drops directives it cannot
    /// parse and carries on, so a broken default would quietly restore the
    /// per-frame flood rather than fail the build.
    #[test]
    fn the_default_filter_parses_and_keeps_both_directives() {
        let parsed = EnvFilter::builder()
            .parse(DEFAULT_LOG)
            .expect("DEFAULT_LOG is a valid filter")
            .to_string();
        assert!(parsed.contains("wgpu_hal=error"), "{parsed}");
        assert!(parsed.contains("info"), "{parsed}");
    }
}
