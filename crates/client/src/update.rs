//! Keeping this app and every Latch Host it can see on the newest release.
//!
//! The Mac does the fetching for everyone (with a GitHub login if the
//! repository is private; see `latch_core::update`). Every few hours it asks
//! GitHub for the latest release. A newer app is downloaded, verified against
//! GitHub's digest and its own code signature, and swapped into place once no
//! stream is running; the app then relaunches itself. A newer host is
//! downloaded once, and `latch-host.exe` is sent to each PC whose host
//! already speaks `/v1/update` (3.1+) and reports an older version, over
//! the same Tailscale-authenticated control API that can put the PC to
//! sleep. A 3.0 host cannot take that (it caps request bodies at 64 KiB
//! and closes, which showed as a broken pipe); for it, the stream's PC
//! menu offers to install the new host through the stream instead (see
//! `handover.rs`). A PC that is asleep gets it the next time it is seen.

use crate::config::ClientConfig;
use crate::session::{Discovery, Live, Progress};
use anyhow::{anyhow, bail, Context, Result};
use latch_core::api::{Ack, Status, UPDATE_PATH, UPDATE_SHA256_HEADER, UPDATE_VERSION_HEADER};
use latch_core::update::{self, Release, HOST_EXE, MAC_ASSET, WINDOWS_ASSET};
use latch_core::{http, legacy, CONTROL_PORT};
use latch_ui::Tone;
use parking_lot::Mutex;
use semver::Version;
use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

const FIRST_CHECK: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const RETRY_AFTER: Duration = Duration::from_secs(15 * 60);
/// After a host got its update, leave it alone this long: it restarts and
/// then reports the new version by itself.
const PUSH_GRACE: Duration = Duration::from_secs(180);
const TICK: Duration = Duration::from_secs(5);

/// What the updater is doing, for the settings card and the lobby.
#[derive(Debug, Clone, Default)]
pub struct State {
    /// One line: "Up to date", "Could not check…", "Sent 3.1.0 to Gaming-PC".
    pub message: String,
    pub checked: Option<Instant>,
    pub latest: Option<Version>,
    /// The latest release as fetched, for whoever needs its assets.
    pub release: Option<Release>,
    /// Set by the UI to check right away.
    pub check_now: bool,
    /// A new app is downloaded and verified; it goes in once no stream runs.
    pub ready: Option<Version>,
    /// Something the lobby should show while it is true of the moment.
    pub notice: Option<(Tone, String)>,
    /// Hosts sent an update recently: node id to (version, when).
    pub pushed: BTreeMap<String, (Version, Instant)>,
    /// Hosts that took the file for this version: node id to version.
    pub delivered: BTreeMap<String, Version>,
    /// Hosts that took a version and came back on the old one, which means
    /// the new executable died on start and the host put the old one back.
    /// Sending the same file again would only repeat that.
    pub rolled_back: BTreeMap<String, Version>,
    /// Hosts too old to receive `/v1/update`; told the user once.
    pub told_old: BTreeSet<String>,
}

pub fn spawn(
    state: Arc<Mutex<State>>,
    discovery: Arc<Mutex<Discovery>>,
    live: Arc<Mutex<Option<Live>>>,
    progress: Arc<Mutex<Progress>>,
    ctx: egui::Context,
) {
    if !cfg!(target_os = "macos") {
        // Everything below assumes a Mac: it replaces an app bundle and
        // unpacks the Windows zip with /usr/bin/unzip. A Windows PC gets
        // its new host pushed from the Mac, and a Linux node is rebuilt, so
        // there is nothing here for either to fetch or send.
        let _ = (discovery, live, progress, ctx);
        state.lock().message = if cfg!(windows) {
            "New versions arrive from the Mac on your Tailscale account.".into()
        } else {
            "Latch updates itself on a Mac; install new releases here by hand.".into()
        };
        return;
    }
    std::thread::spawn(move || {
        let mut next = Instant::now() + FIRST_CHECK;
        let mut release: Option<Release> = None;
        let mut token = None;
        // The last self-update that failed. It is not tried again on every
        // tick, and one that can only fail the same way not until the
        // release changes; the PCs get theirs meanwhile either way.
        let mut self_failed: Option<SelfFailure> = None;
        loop {
            std::thread::sleep(TICK);
            let cfg = ClientConfig::load();
            if !cfg.auto_update {
                let mut st = state.lock();
                st.message = "Off. This app and the PCs stay on their current versions.".into();
                if st.notice.take().is_some() {
                    ctx.request_repaint();
                }
                continue;
            }
            let due = state.lock().check_now || Instant::now() >= next;
            if due {
                token = update::token(cfg.github_token.as_deref());
                state.lock().check_now = false;
                match update::latest(token.as_deref()) {
                    Ok(r) => {
                        next = Instant::now() + CHECK_EVERY;
                        let mut st = state.lock();
                        st.checked = Some(Instant::now());
                        st.latest = Some(r.version.clone());
                        st.release = Some(r.clone());
                        st.message = if r.is_newer_than(&update::current()) {
                            format!("Latch {} is available.", r.version)
                        } else {
                            format!("Up to date (v{}).", update::current())
                        };
                        release = Some(r);
                    }
                    Err(e) => {
                        next = Instant::now() + RETRY_AFTER;
                        let mut st = state.lock();
                        st.checked = Some(Instant::now());
                        st.message = format!("Could not check for updates: {e}.");
                        tracing::warn!("update check: {e:#}");
                    }
                }
                ctx.request_repaint();
            }
            let Some(rel) = release.clone() else { continue };
            let rel = &rel;

            // This app first.
            if rel.is_newer_than(&update::current())
                && state.lock().ready.is_none()
                && self_update_due(self_failed.as_ref(), rel, Instant::now())
            {
                match prepare_self(rel, token.as_deref()) {
                    Ok(Some(v)) => {
                        self_failed = None;
                        let mut st = state.lock();
                        st.ready = Some(v.clone());
                        st.notice = Some((
                            Tone::Info,
                            format!(
                                "Latch {v} is downloaded and installs when no stream is running."
                            ),
                        ));
                    }
                    Ok(None) => {
                        state.lock().message = format!(
                            "Latch {} is available. This copy is not in an app bundle, so it is not replaced.",
                            rel.version
                        );
                    }
                    Err(e) => {
                        state.lock().message =
                            format!("Could not update this Mac to Latch {}: {e:#}.", rel.version);
                        tracing::warn!("self-update: {e:#}");
                        self_failed = Some(SelfFailure::new(rel, &e, Instant::now()));
                    }
                }
                ctx.request_repaint();
            }
            let ready = state.lock().ready.clone();
            if let Some(v) = ready {
                let idle = {
                    // The window's order (live, then progress); the reverse
                    // could deadlock against poll_live.
                    let live = live.lock();
                    let mut p = progress.lock();
                    if live.is_none() && !p.active() {
                        p.updating = true;
                        ctx.request_repaint();
                        true
                    } else {
                        false
                    }
                };
                if idle {
                    match install_self(rel, token.as_deref()) {
                        Ok(()) => relaunch(),
                        Err(e) => {
                            let mut st = state.lock();
                            st.ready = None;
                            st.notice = None;
                            st.message = format!("Could not install Latch {v}: {e:#}.");
                            tracing::error!("self-update: {e:#}");
                            progress.lock().updating = false;
                            self_failed = Some(SelfFailure::new(rel, &e, Instant::now()));
                        }
                    }
                    ctx.request_repaint();
                }
            }

            // Then the PCs whose host is older.
            let disc = discovery.lock().clone();
            let streaming_to = live.lock().as_ref().map(|l| l.ip);
            for pc in disc.pcs.iter().filter(|p| !p.remembered) {
                let (Some(ip), Some(h)) = (pc.ip, pc.host.as_ref()) else {
                    continue;
                };
                let Ok(v) = Version::parse(&h.version) else {
                    continue;
                };
                if !update::takes_pushed_host(&h.os) {
                    // A Mac updates with its own app; a Linux host or a
                    // container is updated by hand, and the lobby says so
                    // (`manual_update_message`).
                    continue;
                }
                if !update::host_can_receive_update(&v) {
                    // The lobby shows this PC what to do; here, just note it.
                    let mut st = state.lock();
                    if st.told_old.insert(pc.node_id.clone()) {
                        st.message = old_host_message(&pc.name, &v);
                        ctx.request_repaint();
                    }
                    continue;
                }
                if !rel.is_newer_than(&v) {
                    // Updated after all (through the stream, say): the
                    // rollback warning is no longer true.
                    let mut st = state.lock();
                    if st.rolled_back.remove(&pc.node_id).is_some()
                        && matches!(st.notice, Some((Tone::Warning, _)))
                    {
                        st.notice = None;
                        ctx.request_repaint();
                    }
                }
                if !should_push(&v, rel) || streaming_to == Some(ip) {
                    continue;
                }
                {
                    let mut st = state.lock();
                    let recently = st
                        .pushed
                        .get(&pc.node_id)
                        .is_some_and(|(pv, at)| *pv == rel.version && at.elapsed() < PUSH_GRACE);
                    if recently || st.rolled_back.get(&pc.node_id) == Some(&rel.version) {
                        continue;
                    }
                    if st.delivered.get(&pc.node_id) == Some(&rel.version) {
                        // It took the file, the grace is over, and it still
                        // runs the old version.
                        st.rolled_back
                            .insert(pc.node_id.clone(), rel.version.clone());
                        st.message = format!(
                            "Latch Host {} did not start on {}, so it kept {v}. It is not sent again until Latch restarts; its log says why.",
                            rel.version, pc.name
                        );
                        st.notice = Some((Tone::Warning, st.message.clone()));
                        tracing::warn!("{} rolled back host {}", pc.name, rel.version);
                        ctx.request_repaint();
                        continue;
                    }
                }
                let outcome = push_host(rel, token.as_deref(), ip);
                let mut st = state.lock();
                st.pushed
                    .insert(pc.node_id.clone(), (rel.version.clone(), Instant::now()));
                match outcome {
                    Ok(()) => {
                        st.delivered.insert(pc.node_id.clone(), rel.version.clone());
                        st.message = format!(
                            "Sent Latch Host {} to {}; it restarts by itself.",
                            rel.version, pc.name
                        );
                        st.notice = Some((Tone::Info, st.message.clone()));
                        tracing::info!("sent host {} to {} ({ip})", rel.version, pc.name);
                    }
                    Err(e) => {
                        st.message = format!("Could not update {}: {e}.", pc.name);
                        st.notice = Some((Tone::Danger, st.message.clone()));
                        tracing::warn!("host update for {}: {e:#}", pc.name);
                    }
                }
                ctx.request_repaint();
            }
            // Notices about hosts fade once the grace period has passed,
            // except a rollback, which stays until someone reads it.
            let mut st = state.lock();
            let fades = !matches!(st.notice, None | Some((Tone::Warning, _)));
            if fades
                && st.ready.is_none()
                && st.pushed.values().all(|(_, at)| at.elapsed() > PUSH_GRACE)
            {
                st.notice = None;
                ctx.request_repaint();
            }
        }
    });
}

/// Where downloads for `rel` live: `<data dir>/updates/<tag>/`.
fn updates_dir(rel: &Release) -> Result<PathBuf> {
    let dir = latch_core::config::data_dir()?
        .join("updates")
        .join(&rel.tag);
    std::fs::create_dir_all(&dir)?;
    // Only prune version-named cache directories, never arbitrary user files.
    // Retain this release and the immediately previous version for rollback.
    prune_updates(dir.parent().expect("updates parent"), &rel.tag);
    Ok(dir)
}

fn prune_updates(root: &std::path::Path, keep: &str) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    let mut old: Vec<_> = entries
        .flatten()
        .filter_map(|e| {
            if !e.file_type().ok()?.is_dir() {
                return None;
            }
            let name = e.file_name().to_str()?.to_string();
            if name == keep {
                return None;
            }
            let version = Version::parse(name.trim_start_matches('v')).ok()?;
            Some((version, e.path()))
        })
        .collect();
    old.sort_by(|a, b| b.0.cmp(&a.0));
    for (_, path) in old.into_iter().skip(1) {
        if let Err(e) = std::fs::remove_dir_all(&path) {
            tracing::warn!("could not prune update cache {}: {e}", path.display());
        }
    }
}

/// Recheck cached assets before using them: an interrupted download or a
/// modified cache must never become an executable update.
fn fetch_asset(rel: &Release, name: &str, token: Option<&str>) -> Result<PathBuf> {
    let asset = rel
        .asset(name)
        .ok_or_else(|| anyhow!("release {} has no {name}", rel.tag))?;
    update::require_digest(asset)?;
    let dest = updates_dir(rel)?.join(name);
    if update::verify_asset(asset, &dest).is_err() {
        update::download(asset, token, &dest)?;
    }
    Ok(dest)
}

/// The `.app` this executable runs from, when it does.
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    // .../Latch.app/Contents/MacOS/Latch
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension().is_some_and(|e| e == "app")).then(|| app.to_path_buf())
}

fn run(cmd: &str, args: &[&str]) -> Result<String> {
    let out = std::process::Command::new(cmd)
        .args(args)
        .output()
        .with_context(|| cmd.to_string())?;
    if !out.status.success() {
        bail!(
            "{cmd} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// A download that arrived whole and still cannot update this Mac. The
/// same asset fails the same way every time, so it is not fetched again.
#[derive(Debug)]
struct Unusable;

impl std::fmt::Display for Unusable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("this release cannot update this Mac")
    }
}

/// A self-update that failed: for which release and asset, and when it may
/// be tried again (`None`: not until the release or its asset changes).
#[derive(Debug, Clone, PartialEq)]
struct SelfFailure {
    tag: String,
    digest: Option<String>,
    retry_at: Option<Instant>,
}

impl SelfFailure {
    fn new(rel: &Release, e: &anyhow::Error, now: Instant) -> Self {
        Self {
            tag: rel.tag.clone(),
            digest: mac_digest(rel),
            retry_at: e
                .downcast_ref::<Unusable>()
                .is_none()
                .then_some(now + RETRY_AFTER),
        }
    }
}

fn mac_digest(rel: &Release) -> Option<String> {
    rel.asset(MAC_ASSET).and_then(|a| a.sha256.clone())
}

/// Whether to try updating this Mac to `rel` now: always after nothing has
/// failed, or for another release or a re-published asset; for the one
/// that failed, once its wait is over, and never if it cannot work.
fn self_update_due(failed: Option<&SelfFailure>, rel: &Release, now: Instant) -> bool {
    match failed {
        None => true,
        Some(f) if f.tag != rel.tag || f.digest != mac_digest(rel) => true,
        Some(f) => f.retry_at.is_some_and(|at| now >= at),
    }
}

/// Download and verify the new app. `Ok(None)` when this copy is not in a
/// bundle (a development build), which is left alone. A download that
/// fails its checks is [`Unusable`]; a failed download is not.
fn prepare_self(rel: &Release, token: Option<&str>) -> Result<Option<Version>> {
    if bundle_path().is_none() {
        return Ok(None);
    }
    if rel.asset(MAC_ASSET).is_none() {
        return Err(anyhow!("release {} has no {MAC_ASSET}", rel.tag).context(Unusable));
    }
    let tarball = fetch_asset(rel, MAC_ASSET, token)?;
    match prepare_in(&updates_dir(rel)?.join("unpacked"), &tarball, &rel.version) {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            // It matched GitHub's digest, so fetching it again would bring
            // the same bytes.
            let _ = std::fs::remove_file(&tarball);
            Err(e.context(Unusable))
        }
    }
}

/// Unpack `tarball` into an emptied `unpacked` and check the app in it,
/// then empty it again: an unpacked copy is never kept to be trusted later
/// (the install unpacks the tarball afresh).
fn prepare_in(unpacked: &Path, tarball: &Path, want: &Version) -> Result<Version> {
    let _ = std::fs::remove_dir_all(unpacked);
    std::fs::create_dir_all(unpacked)?;
    let checked = unpack_app(tarball, unpacked).and_then(|app| verify_app(&app, want));
    let _ = std::fs::remove_dir_all(unpacked);
    checked
}

/// Extract `Latch.app`, and nothing else in the archive, into `dir`.
fn unpack_app(tarball: &Path, dir: &Path) -> Result<PathBuf> {
    run(
        "/usr/bin/tar",
        &[
            "xzf",
            &tarball.display().to_string(),
            "-C",
            &dir.display().to_string(),
            "Latch.app",
        ],
    )?;
    Ok(dir.join("Latch.app"))
}

/// The app is there, its signature verifies, and it is the version wanted.
fn verify_app(app: &Path, want: &Version) -> Result<Version> {
    anyhow::ensure!(app.exists(), "the download holds no Latch.app");
    run(
        "/usr/bin/codesign",
        &["--verify", "--deep", "--strict", &app.display().to_string()],
    )
    .context("the new app's signature does not verify")?;
    let version = run(
        "/usr/bin/plutil",
        &[
            "-extract",
            "CFBundleShortVersionString",
            "raw",
            &app.join("Contents/Info.plist").display().to_string(),
        ],
    )?;
    let version =
        Version::parse(&version).with_context(|| format!("bundle version {version:?}"))?;
    anyhow::ensure!(
        version == *want,
        "the download says {version}, the release {want}"
    );
    Ok(version)
}

/// Move the new app over this one. The release's tarball is checked
/// against GitHub's digest again (and fetched again if the asset changed
/// since it was prepared), unpacked straight into the staging folder beside
/// this app and checked there: what was checked is exactly what goes in.
/// macOS keeps the running executable alive across the rename, so the swap
/// is safe while we are still up.
fn install_self(rel: &Release, token: Option<&str>) -> Result<()> {
    let bundle = bundle_path().ok_or_else(|| anyhow!("not running from an app bundle"))?;
    let tarball = fetch_asset(rel, MAC_ASSET, token)?;
    replace_bundle(&bundle, &tarball, |tarball, staged| {
        stage_from(tarball, staged, &rel.version)
    })?;
    let _ = run(
        "/usr/bin/xattr",
        &["-dr", "com.apple.quarantine", &bundle.display().to_string()],
    );
    Ok(())
}

/// Unpack the app from `tarball` as `staged` (`Latch.app` in a fresh
/// staging folder) and check it where it lies.
fn stage_from(tarball: &Path, staged: &Path, want: &Version) -> Result<()> {
    let dir = staged
        .parent()
        .ok_or_else(|| anyhow!("the staging folder has no parent"))?;
    let app = unpack_app(tarball, dir)?;
    anyhow::ensure!(app == staged, "unpacked to {}", app.display());
    verify_app(&app, want).map(|_| ())
}

/// Stage on the destination volume before moving the running app. A failed
/// cross-volume copy leaves the working app untouched, and the final swaps
/// are both same-volume renames.
fn replace_bundle(
    bundle: &std::path::Path,
    new: &std::path::Path,
    copy: impl FnOnce(&std::path::Path, &std::path::Path) -> Result<()>,
) -> Result<()> {
    let parent = bundle
        .parent()
        .ok_or_else(|| anyhow!("app has no parent"))?;
    let stage = parent.join(format!(".latch-update-{:016x}", rand::random::<u64>()));
    std::fs::create_dir(&stage).context("create update staging directory")?;
    let staged = stage.join("Latch.app");
    let parked = stage.join("previous.app");
    let outcome = (|| -> Result<()> {
        copy(new, &staged).context("stage the new app")?;
        std::fs::rename(bundle, &parked).context("move the current app aside")?;
        if let Err(e) = std::fs::rename(&staged, bundle) {
            std::fs::rename(&parked, bundle).with_context(|| {
                format!(
                    "restore the previous app; backup remains at {}",
                    parked.display()
                )
            })?;
            return Err(e).context("put the new app in place");
        }
        Ok(())
    })();
    // If rollback failed, keep the previous app so it can be recovered.
    if outcome.is_ok() || !parked.exists() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    outcome
}

/// Start the new copy and leave. `open` goes through LaunchServices, so the
/// new instance gets a proper app launch rather than inheriting this one.
fn relaunch() {
    if let Some(bundle) = bundle_path() {
        let _ = relaunch_command(&bundle)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn();
    }
    tracing::info!("relaunching into the new version");
    std::process::exit(0);
}

fn relaunch_command(bundle: &std::path::Path) -> std::process::Command {
    let mut command = std::process::Command::new("/bin/sh");
    // The bundle is an argument, never shell source (paths can contain $, ",
    // backticks and newlines). $0 is a diagnostic command name.
    command.args(["-c", "sleep 1; exec /usr/bin/open \"$1\"", "latch-relaunch"]);
    command.arg(bundle);
    command
}

/// Extract `latch-host.exe` from the verified Windows archive. An extracted
/// cache has no published digest, so never trust one from an earlier run.
pub(crate) fn host_exe(rel: &Release, token: Option<&str>) -> Result<Vec<u8>> {
    let zip = fetch_asset(rel, WINDOWS_ASSET, token)?;
    let out = std::process::Command::new("/usr/bin/unzip")
        .args(["-p", &zip.display().to_string(), HOST_EXE])
        .output()
        .context("unzip")?;
    if !out.status.success() || out.stdout.len() < 1024 * 1024 {
        bail!("{} holds no {HOST_EXE}", WINDOWS_ASSET);
    }
    Ok(out.stdout)
}

/// A host that already speaks `/v1/update`, and a release newer than it.
fn should_push(running: &Version, rel: &Release) -> bool {
    update::host_can_receive_update(running) && rel.is_newer_than(running)
}

pub fn old_host_message(name: &str, version: &Version) -> String {
    format!(
        "{name} runs Latch Host {version}, which cannot take an update over the network. Connect to it and choose PC → Update Latch Host in the toolbar: this machine installs the new version through the stream. After that, updates are automatic."
    )
}

/// What to say about a host this Mac cannot update when a newer release is
/// out: a Linux machine or a container, whose Latch Host is installed by
/// hand. `None` for a Windows PC (it is sent the update), a Mac (its own
/// app updates it) and a host already on the release.
pub fn manual_update_message(
    name: &str,
    os: &str,
    running: &Version,
    rel: &Release,
) -> Option<String> {
    if update::takes_pushed_host(os) || os.eq_ignore_ascii_case("macos") {
        return None;
    }
    rel.is_newer_than(running).then(|| {
        format!(
            "{name} runs Latch Host {running}, and {} is out. This Mac sends updates to Windows PCs only: install the new Latch Host on {name} by hand, the way it was set up.",
            rel.version
        )
    })
}

fn is_transient(e: &anyhow::Error) -> bool {
    let s = format!("{e:#}");
    s.contains("timed out")
        || s.contains("connection closed")
        || s.contains("Broken pipe")
        || s.contains("Connection reset")
        || s.contains("os error 32")
        || s.contains("os error 35")
}

/// Send the new host to the PC at `ip`. The host checks the digest, swaps
/// the file in and restarts.
fn push_host(rel: &Release, token: Option<&str>, ip: Ipv4Addr) -> Result<()> {
    let exe = host_exe(rel, token)?;
    let sha = update::sha256_hex(&exe);
    let version = rel.version.to_string();
    let host = SocketAddr::from((ip, CONTROL_PORT));
    let mut last = None;
    for attempt in 1..=3 {
        match send_host(ip, &version, &sha, &exe) {
            Ok(()) => return Ok(()),
            Err(e) => {
                // A host can take the file and restart before its reply
                // gets out, so the Mac sees a dropped connection and the
                // retry hears "already runs". Ask the host what it runs
                // before calling either a failure.
                if (attempt > 1 || is_transient(&e))
                    && runs_at_least(host, &rel.version, Duration::from_secs(15))
                {
                    return Ok(());
                }
                if attempt == 3 || !is_transient(&e) {
                    return Err(e);
                }
                tracing::warn!("host update attempt {attempt}/3: {e:#}");
                std::thread::sleep(Duration::from_secs(2 * attempt as u64));
                last = Some(e);
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow!("update failed")))
}

/// Whether the host at `addr` reports `want` or newer within `within`: it
/// may still be handing over to the new executable when first asked.
fn runs_at_least(addr: SocketAddr, want: &Version, within: Duration) -> bool {
    let deadline = Instant::now() + within;
    loop {
        let running = http::get_json::<Status>(addr, "/v1/status", Duration::from_secs(2))
            .ok()
            .and_then(|s| Version::parse(&s.version).ok());
        if running.is_some_and(|v| v >= *want) {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// The headers of a host-update push: this version's names and, because a
/// host older than 4.1 knows only the old ones and is the one this push is
/// most likely for, the old names too.
fn push_headers<'a>(version: &'a str, sha: &'a str) -> [(&'static str, &'a str); 5] {
    [
        (UPDATE_VERSION_HEADER, version),
        (UPDATE_SHA256_HEADER, sha),
        (legacy::UPDATE_VERSION_HEADER, version),
        (legacy::UPDATE_SHA256_HEADER, sha),
        ("Content-Type", "application/octet-stream"),
    ]
}

fn send_host(ip: Ipv4Addr, version: &str, sha: &str, exe: &[u8]) -> Result<()> {
    let r = http::request_with(
        (ip, CONTROL_PORT),
        "POST",
        UPDATE_PATH,
        &push_headers(version, sha),
        exe,
        Duration::from_secs(120),
    )?;
    if r.status != 200 {
        let why = r
            .parse::<Ack>()
            .ok()
            .and_then(|a| a.error)
            .unwrap_or_else(|| format!("HTTP {}", r.status));
        bail!("{why}");
    }
    Ok(())
}

/// For the settings card: when the last check happened.
pub fn ago(at: Option<Instant>) -> String {
    match at {
        None => "not checked yet".into(),
        Some(t) => {
            let s = t.elapsed().as_secs();
            if s < 90 {
                "checked just now".into()
            } else if s < 5400 {
                format!("checked {} min ago", s / 60)
            } else {
                format!("checked {} h ago", s / 3600)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relaunch_keeps_shell_metacharacters_in_the_path_literal() {
        let path = std::path::Path::new("/Applications/$(touch nope) \"quoted\" `name`.app");
        let command = relaunch_command(path);
        let args: Vec<_> = command.get_args().collect();
        assert_eq!(args[1], "sleep 1; exec /usr/bin/open \"$1\"");
        assert_eq!(args[3], path.as_os_str());
    }

    #[test]
    fn a_partial_update_copy_preserves_the_working_app() {
        let dir = std::env::temp_dir().join(format!("latch-swap-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let bundle = dir.join("Latch.app");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::write(bundle.join("version"), "original").unwrap();
        let outcome = replace_bundle(&bundle, &dir.join("download"), |_, staged| {
            std::fs::create_dir(staged)?;
            std::fs::write(staged.join("partial"), "incomplete")?;
            bail!("disk full");
        });
        assert!(outcome.is_err());
        assert_eq!(
            std::fs::read_to_string(bundle.join("version")).unwrap(),
            "original"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn a_staged_update_replaces_the_app_and_removes_the_backup() {
        let dir = std::env::temp_dir().join(format!("latch-swap-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        let bundle = dir.join("Latch.app");
        std::fs::create_dir(&bundle).unwrap();
        std::fs::write(bundle.join("version"), "original").unwrap();
        replace_bundle(&bundle, &dir.join("download"), |_, staged| {
            std::fs::create_dir(staged)?;
            std::fs::write(staged.join("version"), "updated")?;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(bundle.join("version")).unwrap(),
            "updated"
        );
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A minimal app bundle at `dir/Latch.app` that says `version` and
    /// carries `marker`, ad-hoc signed as a release build is.
    #[cfg(target_os = "macos")]
    fn fake_app(dir: &Path, version: &str, marker: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let app = dir.join("Latch.app");
        let contents = app.join("Contents");
        std::fs::create_dir_all(contents.join("MacOS")).unwrap();
        std::fs::create_dir_all(contents.join("Resources")).unwrap();
        let exe = contents.join("MacOS/Latch");
        std::fs::write(&exe, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::write(contents.join("Resources/marker"), marker).unwrap();
        std::fs::write(
            contents.join("Info.plist"),
            format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Latch</string>
<key>CFBundleIdentifier</key><string>com.example.latch-update-test</string>
<key>CFBundlePackageType</key><string>APPL</string>
<key>CFBundleShortVersionString</key><string>{version}</string>
</dict></plist>"#
            ),
        )
        .unwrap();
        let path = app.display().to_string();
        run(
            "/usr/bin/codesign",
            &["-s", "-", "--force", "--deep", &path],
        )
        .unwrap();
        app
    }

    /// A release tarball laid out as release.yml makes it.
    #[cfg(target_os = "macos")]
    fn tarball_of(dir: &Path, version: &str, marker: &str) -> PathBuf {
        let src = dir.join(format!("build-{marker}"));
        fake_app(&src, version, marker);
        std::fs::write(src.join("README-MACOS.md"), "read me").unwrap();
        let tarball = dir.join(format!("{marker}.tar.gz"));
        run(
            "/usr/bin/tar",
            &[
                "czf",
                &tarball.display().to_string(),
                "-C",
                &src.display().to_string(),
                "Latch.app",
                "README-MACOS.md",
            ],
        )
        .unwrap();
        tarball
    }

    #[cfg(target_os = "macos")]
    fn scratch() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("latch-selfup-{:016x}", rand::random::<u64>()));
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn an_app_left_unpacked_earlier_is_never_what_gets_checked() {
        let dir = scratch();
        let unpacked = dir.join("unpacked");
        // An earlier build of the same tag, valid in itself, left behind.
        fake_app(&unpacked, "9.9.9", "stale");
        let tarball = tarball_of(&dir, "9.9.8", "fresh");
        let want = Version::new(9, 9, 9);
        let e = prepare_in(&unpacked, &tarball, &want).unwrap_err();
        assert!(e.to_string().contains("says 9.9.8"), "{e:#}");
        assert_eq!(
            prepare_in(&unpacked, &tarball, &Version::new(9, 9, 8)).unwrap(),
            Version::new(9, 9, 8)
        );
        assert!(!unpacked.exists(), "nothing unpacked is kept");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn the_install_checks_the_app_it_unpacks_into_the_stage() {
        let dir = scratch();
        let apps = dir.join("Applications");
        let bundle = fake_app(&apps, "9.9.8", "running");
        let tarball = tarball_of(&dir, "9.9.9", "fresh");
        let marker =
            |app: &Path| std::fs::read_to_string(app.join("Contents/Resources/marker")).unwrap();
        // The wrong version: refused, and the running app is untouched.
        let e = replace_bundle(&bundle, &tarball, |t, staged| {
            stage_from(t, staged, &Version::new(9, 9, 7))
        })
        .unwrap_err();
        assert!(format!("{e:#}").contains("says 9.9.9"), "{e:#}");
        assert_eq!(marker(&bundle), "running");
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1);
        // The right one goes in, and only the app comes out of the archive.
        replace_bundle(&bundle, &tarball, |t, staged| {
            stage_from(t, staged, &Version::new(9, 9, 9))
        })
        .unwrap();
        assert_eq!(marker(&bundle), "fresh");
        assert_eq!(std::fs::read_dir(&apps).unwrap().count(), 1);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn the_bundle_is_found_only_inside_an_app() {
        // The test binary lives in target/, not in a .app.
        assert_eq!(bundle_path(), None);
    }

    #[test]
    fn ago_reads_naturally() {
        assert_eq!(ago(None), "not checked yet");
        assert_eq!(ago(Some(Instant::now())), "checked just now");
        let earlier = Instant::now() - Duration::from_secs(20 * 60);
        assert_eq!(ago(Some(earlier)), "checked 20 min ago");
        let earlier = Instant::now() - Duration::from_secs(3 * 3600);
        assert_eq!(ago(Some(earlier)), "checked 3 h ago");
    }

    fn rel(v: &str) -> Release {
        Release {
            version: Version::parse(v).unwrap(),
            tag: format!("v{v}"),
            prerelease: false,
            assets: vec![],
        }
    }

    #[test]
    fn a_host_older_than_3_1_is_not_posted_the_executable() {
        // Gaming-PC today: 3.0.0. GitHub latest: 3.0.1. POSTing 10 MB at that
        // host is a broken pipe; the Mac must skip it.
        assert!(!should_push(&Version::new(3, 0, 0), &rel("3.0.1")));
        assert!(!should_push(&Version::new(3, 0, 0), &rel("3.1.0")));
        assert!(!should_push(&Version::new(3, 0, 1), &rel("3.1.0")));
        assert!(!should_push(&Version::new(3, 1, 0), &rel("3.1.0")));
        assert!(should_push(&Version::new(3, 1, 0), &rel("3.2.0")));
        assert!(should_push(
            &Version::parse("3.1.1").unwrap(),
            &rel("3.2.0")
        ));
        let msg = old_host_message("Gaming-PC", &Version::new(3, 0, 0));
        assert!(msg.contains("Gaming-PC"), "{msg}");
        assert!(msg.contains("3.0.0"), "{msg}");
        assert!(msg.contains("Update Latch Host"), "{msg}");
        assert!(!msg.contains("Broken pipe"), "{msg}");
        assert!(!msg.contains("os error"), "{msg}");
    }

    #[test]
    fn a_push_carries_both_header_names_so_a_4_0_host_accepts_it() {
        let h = push_headers("4.1.1", "abc");
        let get = |name: &str| h.iter().find(|(k, _)| *k == name).map(|(_, v)| *v);
        assert_eq!(get("x-latch-version"), Some("4.1.1"));
        assert_eq!(get("x-latch-sha256"), Some("abc"));
        // What a 4.0 host reads, spelled out here so that renaming the
        // constant in legacy.rs cannot silently break the push.
        assert_eq!(get("x-brolink-version"), Some("4.1.1"));
        assert_eq!(get("x-brolink-sha256"), Some("abc"));
        assert_eq!(get("Content-Type"), Some("application/octet-stream"));
    }

    /// A one-reply HTTP server saying the host runs `version`.
    fn status_server(version: &'static str) -> SocketAddr {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for mut s in listener.incoming().flatten() {
                let mut buf = [0u8; 1024];
                let _ = s.read(&mut buf);
                let body = format!(r#"{{"app":"latch","version":"{version}"}}"#);
                let _ = write!(
                    s,
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        addr
    }

    #[test]
    fn a_host_already_on_the_release_counts_as_updated() {
        let want = Version::new(4, 0, 3);
        assert!(runs_at_least(status_server("4.0.3"), &want, Duration::ZERO));
        assert!(runs_at_least(status_server("4.1.0"), &want, Duration::ZERO));
        assert!(!runs_at_least(
            status_server("4.0.2"),
            &want,
            Duration::ZERO
        ));
        let closed = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        assert!(!runs_at_least(closed, &want, Duration::ZERO));
    }

    #[test]
    fn a_linux_host_behind_the_release_is_named_and_others_are_not() {
        let r = rel("4.1.2");
        let old = Version::new(4, 1, 1);
        let msg = manual_update_message("stuzha", "linux", &old, &r).expect("a line");
        assert!(msg.contains("stuzha") && msg.contains("4.1.1") && msg.contains("4.1.2"));
        assert_eq!(
            manual_update_message("stuzha", "linux", &Version::new(4, 1, 2), &r),
            None
        );
        for os in ["windows", "", "macOS"] {
            assert_eq!(manual_update_message("pc", os, &old, &r), None, "{os:?}");
        }
    }

    fn rel_with_mac_asset(v: &str, digest: &str) -> Release {
        Release {
            assets: vec![latch_core::update::Asset {
                name: MAC_ASSET.into(),
                url: String::new(),
                size: 1,
                sha256: Some(digest.into()),
            }],
            ..rel(v)
        }
    }

    #[test]
    fn a_failed_self_update_waits_and_one_that_cannot_work_waits_for_a_new_asset() {
        let now = Instant::now();
        let r = rel_with_mac_asset("4.1.2", "aa");
        assert!(self_update_due(None, &r, now));
        // A download that failed: again after a while, not every tick.
        let failed = SelfFailure::new(&r, &anyhow!("timed out"), now);
        assert!(!self_update_due(Some(&failed), &r, now));
        assert!(self_update_due(Some(&failed), &r, now + RETRY_AFTER));
        // A download that can never work (it says 4.1.1 inside): not again
        // for these bytes, at once for new ones or a new release.
        let unusable = SelfFailure::new(
            &r,
            &anyhow!("the download says 4.1.1, the release 4.1.2").context(Unusable),
            now,
        );
        assert!(!self_update_due(
            Some(&unusable),
            &r,
            now + 100 * RETRY_AFTER
        ));
        assert!(self_update_due(
            Some(&unusable),
            &rel_with_mac_asset("4.1.2", "bb"),
            now
        ));
        assert!(self_update_due(
            Some(&unusable),
            &rel_with_mac_asset("4.1.3", "aa"),
            now
        ));
    }

    #[test]
    fn pipe_and_eagain_are_retried() {
        assert!(is_transient(&anyhow!("connection closed")));
        assert!(is_transient(&anyhow!("timed out")));
        assert!(is_transient(&anyhow!("Broken pipe (os error 32)")));
        assert!(is_transient(&anyhow!(
            "Resource temporarily unavailable (os error 35)"
        )));
        assert!(!is_transient(&anyhow!(
            "GitHub refused the token (HTTP 401)"
        )));
        assert!(!is_transient(&anyhow!(
            "the upload is not a Windows executable"
        )));
    }
}
