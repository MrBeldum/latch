//! Latch. One executable, two jobs:
//!
//! * `--background`: the control service other machines talk to.
//! * no flags: the window — a list of machines to connect to, and setup
//!   to share this one.
//!
//! `--replaces <pid>` is how an update hands over. `--brand-engine` is
//! Windows setup rewriting the streaming engine's name and icon.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use anyhow::Result;
use clap::Parser;
use latch_core::config::data_dir;
use latch_host::brand;
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
    /// Give the streaming engine in DIR Latch's name and icon. Run by
    /// setup, elevated, with the engine stopped; exits non-zero with the
    /// reason on stderr.
    #[arg(long, value_name = "DIR", conflicts_with = "background")]
    brand_engine: Option<std::path::PathBuf>,
    /// Install and start the streaming engine, then exit. Used by Mac/Linux
    /// setup and by the Docker node; on Windows this is the same path as
    /// Share this machine (UAC).
    #[arg(long, conflicts_with_all = ["background", "brand_engine"])]
    setup: bool,
    /// Run setup as administrator. The unelevated panel launches this so
    /// the script is generated after UAC, not from a user-writable file.
    #[arg(long, conflicts_with_all = ["background", "brand_engine"])]
    setup_elevated: bool,
    /// The launching user's %LOCALAPPDATA%, so an elevated setup approved
    /// with another administrator's password still uses that user's
    /// Latch folder (host.toml, setup.log).
    #[arg(long, value_name = "DIR", requires = "setup_elevated")]
    local_app_data: Option<std::path::PathBuf>,
}

fn main() -> Result<()> {
    // Before anything can replace it: which file this process is.
    let _ = service::started_from();
    let args = Args::parse();
    if let Some(dir) = &args.local_app_data {
        // Before any thread or data_dir() call; edition 2021, so set_var is safe.
        std::env::set_var("LOCALAPPDATA", dir);
    }
    // The elevated setup helper logs with the panel; `setup.log` is the
    // script's own output, which the setup card shows.
    let log = if args.background {
        "service.log"
    } else {
        "panel.log"
    };
    init_logging(log);
    install_panic_hook(data_dir().ok().map(|d| d.join(log)));
    // init_logging has just called data_dir(), which takes over an older
    // install's folder; the engine's config inside still names the old one.
    #[cfg(not(windows))]
    latch_host::unix_setup::repair_adopted_engine_config();
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
        return latch_host::setup::run_as_admin();
    }
    if let Some(dir) = &args.brand_engine {
        return brand::brand(dir).map_err(|e| {
            tracing::error!("brand engine at {}: {e:#}", dir.display());
            e
        });
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
    let mut cfg = latch_host::config::HostConfig::load();
    if !cfg.has_creds() {
        cfg.sunshine_user = "latch".into();
        cfg.sunshine_pass = latch_host::config::random_password();
        cfg.save()?;
    }
    let exe = std::env::current_exe()?;
    latch_host::setup::run(&latch_host::setup::Plan {
        exe: &exe,
        install_engine: true,
        migrate: false,
        dry_run: false,
        sunshine_user: &cfg.sunshine_user,
        sunshine_pass: &cfg.sunshine_pass,
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
const DEFAULT_LOG: &str = "info,wgpu_hal=error";

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
fn init_logging(file: &str) {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(DEFAULT_LOG));
    let sink: Box<dyn std::io::Write + Send> =
        match data_dir().and_then(|d| Ok(RotatingLog::open(d.join(file))?)) {
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
    use super::{panic_line, DEFAULT_LOG};
    use tracing_subscriber::EnvFilter;

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
