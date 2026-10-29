//! Sharing setup and the platform's login service.
use crate::setup::Plan;
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::time::Duration;

pub fn run(p: &Plan<'_>) -> Result<()> {
    let result = (|| {
        crate::prep::migrate()?;
        #[cfg(target_os = "macos")]
        request_permissions();
        #[cfg(target_os = "linux")]
        retire_engine()?;
        set_autostart(true, p.exe)?;
        #[cfg(target_os = "macos")]
        if firewall_blocks(p.exe) {
            allow_through_firewall(p.exe)?;
        }
        Ok(())
    })();
    if let Some(log) = crate::setup::log_path() {
        let text = match &result {
            Ok(()) => {
                "Sharing setup finished. Permission changes may need an app restart.".to_string()
            }
            Err(e) => format!("Sharing setup failed: {e:#}"),
        };
        let _ = fs::write(log, text + "\n");
    }
    result
}

#[cfg(target_os = "linux")]
fn retire_engine() -> Result<()> {
    for unit in ["latch-engine.service", "brolink-engine.service"] {
        let _ = systemctl_user(&["disable", "--now", unit]);
        let file = dirs_home()?.join(".config/systemd/user").join(unit);
        if file.exists() {
            fs::remove_file(file)?;
        }
    }
    // Latch 4's drop-ins tied latch.service to the engine unit and waited
    // for its port; left in place, the service would require a unit that
    // no longer exists and never start. The owner's other lines stay.
    let dropins = dirs_home()?.join(".config/systemd/user/latch.service.d");
    if let Ok(entries) = fs::read_dir(&dropins) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "conf") {
                if let Ok(text) = fs::read_to_string(&path) {
                    let kept = without_engine_lines(&text);
                    if kept != text {
                        fs::write(&path, kept)?;
                    }
                }
            }
        }
    }
    let _ = systemctl_user(&["daemon-reload"]);
    let old = dirs_home()?.join(".config/sunshine");
    if old.exists() {
        retire_legacy_sunshine(&old)?;
    }
    let engine = latch_core::config::data_dir()?.join("engine");
    if engine.exists() {
        fs::remove_dir_all(engine)?;
    }
    Ok(())
}

/// A drop-in's text without the lines that tied the service to Latch 4's
/// engine unit or waited for the engine.
#[cfg(any(target_os = "linux", test))]
fn without_engine_lines(text: &str) -> String {
    text.lines()
        .filter(|l| {
            !(l.contains("latch-engine.service")
                || l.contains("brolink-engine.service")
                || l.contains("wait-engine.py"))
        })
        .map(|l| format!("{l}\n"))
        .collect()
}

#[cfg(any(target_os = "linux", test))]
fn is_latch4_sunshine(dir: &Path) -> bool {
    if dir.join("latch-web.json").exists() || dir.join("brolink-web.json").exists() {
        return true;
    }
    if let Ok(apps) = fs::read_to_string(dir.join("apps.json")) {
        if apps.contains("desktop.png") {
            return true;
        }
    }
    if let Ok(conf) = fs::read_to_string(dir.join("sunshine.conf")) {
        if conf.contains("latch-web.json") || conf.contains("brolink-web.json") {
            return true;
        }
    }
    false
}

#[cfg(any(target_os = "linux", test))]
fn dated_backup_path(dir: &Path) -> PathBuf {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let stamp = format_date(now);
    let name = dir
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("sunshine");
    let mut candidate = dir.with_file_name(format!("{name}.bak-{stamp}"));
    let mut counter = 1;
    while candidate.exists() {
        candidate = dir.with_file_name(format!("{name}.bak-{stamp}-{counter}"));
        counter += 1;
    }
    candidate
}

#[cfg(any(target_os = "linux", test))]
fn format_date(secs: u64) -> String {
    let mut days = secs / 86400;
    let rem_secs = secs % 86400;
    let hours = rem_secs / 3600;
    let mins = (rem_secs % 3600) / 60;
    let s = rem_secs % 60;
    days += 719468;
    let era = days / 146097;
    let doe = days - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{hours:02}{mins:02}{s:02}")
}

#[cfg(any(target_os = "linux", test))]
fn retire_legacy_sunshine(old: &Path) -> Result<()> {
    if !is_latch4_sunshine(old) {
        return Ok(());
    }
    let backup = dated_backup_path(old);
    fs::rename(old, backup).context("move legacy sunshine configuration to backup")?;
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
const FIREWALL: &str = "/usr/libexec/ApplicationFirewall/socketfilterfw";

/// Whether the macOS application firewall turns away connections to
/// `exe`: the control service then looks absent to every other machine
/// (no PIN entry, no status), while the Mac's own window says "Shared". A
/// rule for an ad-hoc signed app can lapse with each new build.
#[cfg(target_os = "macos")]
pub fn firewall_blocks(exe: &Path) -> bool {
    let run = |args: &[&str]| {
        Command::new(FIREWALL)
            .args(args)
            .stderr(Stdio::null())
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default()
    };
    firewall_on(&run(&["--getglobalstate"]))
        && app_blocked(&run(&["--getappblocked", &exe.display().to_string()]))
}

/// "Firewall is enabled. (State = 1)"; 2 is "block all but essentials".
#[cfg(any(target_os = "macos", test))]
fn firewall_on(global: &str) -> bool {
    global.contains("State = 1") || global.contains("State = 2")
}

/// "Incoming connection to <exe> is blocked." (or "is permitted.").
#[cfg(any(target_os = "macos", test))]
fn app_blocked(answer: &str) -> bool {
    answer.contains("is blocked")
}

/// Add `exe` to the firewall's list as allowed. That takes an
/// administrator, so macOS asks for the password; setup is something the
/// owner started at the Mac.
#[cfg(target_os = "macos")]
fn allow_through_firewall(exe: &Path) -> Result<()> {
    let out = Command::new("osascript")
        .args(["-e", &firewall_allow_script(&exe.display().to_string())])
        .stdin(Stdio::null())
        .output()
        .context("run osascript")?;
    anyhow::ensure!(
        out.status.success() && !firewall_blocks(exe),
        "the macOS firewall blocks Latch, so other machines cannot reach it ({}). Allow Latch in System Settings > Network > Firewall > Options",
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(())
}

/// The AppleScript that runs `socketfilterfw --add` and `--unblockapp` for
/// `exe` as an administrator.
#[cfg(any(target_os = "macos", test))]
fn firewall_allow_script(exe: &str) -> String {
    let exe = exe.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        "set exe to \"{exe}\"\ndo shell script \"{FIREWALL} --add \" & quoted form of exe & \" && {FIREWALL} --unblockapp \" & quoted form of exe with administrator privileges"
    )
}

pub fn set_autostart(enable: bool, exe: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        macos_launch_agent(enable, exe)
    }
    #[cfg(target_os = "linux")]
    {
        linux_user_unit(enable, exe)
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (enable, exe);
        Ok(())
    }
}

/// Write the login item without loading, unloading or starting anything.
/// The background service calls this every time it starts, and it must
/// never stop itself (`launchctl unload` of the job that is running kills
/// it) or start a second copy of itself.
pub fn register_autostart(exe: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        if !installed_app(exe) {
            // A build run from a scratch folder, a disk image or App
            // Translocation must not take the login item over: at the next
            // login it would start that copy, or nothing once it is gone.
            tracing::info!(
                "not pointing the login item at {}: not an installed Latch.app",
                exe.display()
            );
            return Ok(());
        }
        write_launch_agent(exe)?;
        retire_legacy_launch_agent();
        Ok(())
    }
    #[cfg(target_os = "linux")]
    {
        write_user_unit(exe)?;
        // Links the unit into default.target; without `--now` nothing starts.
        anyhow::ensure!(
            systemctl_user(&["enable", "latch.service"])?,
            "could not enable Latch service"
        );
        retire_legacy_user_unit();
        Ok(())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = exe;
        Ok(())
    }
}

pub fn autostart_enabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        launch_agent_path().is_ok_and(|p| p.exists())
            || legacy_launch_agent_path().is_ok_and(|p| p.exists())
    }
    #[cfg(target_os = "linux")]
    {
        user_unit_path().is_ok_and(|p| p.exists())
            || legacy_user_unit_path().is_ok_and(|p| p.exists())
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

#[cfg(any(target_os = "macos", test))]
const LAUNCH_AGENT_LABEL: &str = "com.bardbro.latch.node";

/// How to start the background service when nothing answers. A copy the
/// window starts itself has no supervisor: launchd and systemd restart
/// only what they started, and a second copy started beside theirs loses
/// the port race and leaves their job stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StartPlan {
    /// macOS: load the login item; its RunAtLoad starts the service.
    Bootstrap,
    /// macOS: start the loaded login item (nothing if it already runs).
    Kickstart,
    /// Linux: start the user unit, with its drop-ins' environment.
    SystemdStart,
    /// No login item to go through: start a copy directly.
    Spawn,
}

/// What login item this machine has for the service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginItem {
    None,
    /// The launch agent's file exists; whether launchd has it loaded, and
    /// whether it is marked disabled (`launchctl print-disabled`), which
    /// System Settings' Login Items does and which loads nothing.
    MacAgent {
        loaded: bool,
        disabled: bool,
    },
    /// The systemd user unit's file exists.
    LinuxUnit,
}

pub fn start_plan(item: LoginItem) -> StartPlan {
    match item {
        LoginItem::None | LoginItem::MacAgent { disabled: true, .. } => StartPlan::Spawn,
        LoginItem::MacAgent { loaded: true, .. } => StartPlan::Kickstart,
        LoginItem::MacAgent { loaded: false, .. } => StartPlan::Bootstrap,
        LoginItem::LinuxUnit => StartPlan::SystemdStart,
    }
}

fn login_item() -> LoginItem {
    #[cfg(target_os = "macos")]
    {
        if launch_agent_path().is_ok_and(|p| p.exists()) {
            return LoginItem::MacAgent {
                loaded: launch_agent_loaded(),
                disabled: launch_agent_disabled(),
            };
        }
    }
    #[cfg(target_os = "linux")]
    {
        if user_unit_path().is_ok_and(|p| p.exists()) {
            return LoginItem::LinuxUnit;
        }
    }
    LoginItem::None
}

/// Start the service through its login item. `None` when there is none to
/// go through (or macOS has it disabled), so the caller starts a copy
/// itself; otherwise how it went, naming what was asked.
pub fn start_supervised() -> Option<Result<&'static str>> {
    let item = login_item();
    let plan = start_plan(item);
    if let LoginItem::MacAgent { disabled: true, .. } = item {
        tracing::warn!(
            "macOS has Latch's login item turned off (System Settings > General > Login Items), so the service starts without it and does not come back after a restart"
        );
    }
    match plan {
        StartPlan::Spawn => None,
        #[cfg(target_os = "macos")]
        StartPlan::Bootstrap | StartPlan::Kickstart => Some((|| -> Result<&'static str> {
            let uid = uid()?;
            let args = if plan == StartPlan::Bootstrap {
                let plist = launch_agent_path()?;
                bootstrap_args(&uid, &plist.display().to_string())
            } else {
                kickstart_args(&uid)
            };
            anyhow::ensure!(launchctl(&args)?, "launchctl {} failed", args[0]);
            Ok("launchd")
        })()),
        #[cfg(target_os = "linux")]
        StartPlan::SystemdStart => Some((|| -> Result<&'static str> {
            // --no-block: the window calls this before it opens, and a unit
            // may wait for the engine before it starts.
            anyhow::ensure!(
                systemctl_user(&["start", "--no-block", "latch.service"])?,
                "systemctl --user start latch.service failed"
            );
            Ok("systemd")
        })()),
        _ => None,
    }
}

/// Whether launchd or systemd started this process, and so restarts it
/// after a failure.
pub fn supervised() -> bool {
    #[cfg(target_os = "macos")]
    {
        std::env::var("XPC_SERVICE_NAME").is_ok_and(|v| v == LAUNCH_AGENT_LABEL)
    }
    #[cfg(target_os = "linux")]
    {
        fs::read_to_string("/proc/self/cgroup").is_ok_and(|c| in_latch_unit(&c))
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        false
    }
}

/// `/proc/self/cgroup` of a process systemd runs as `latch.service`.
#[cfg(any(target_os = "linux", test))]
fn in_latch_unit(cgroup: &str) -> bool {
    cgroup
        .lines()
        .any(|l| l.trim_end().ends_with("/latch.service"))
}

/// `launchctl kickstart` without `-k`: it starts a job that is not
/// running and leaves one that is alone.
#[cfg(any(target_os = "macos", test))]
fn kickstart_args(uid: &str) -> Vec<String> {
    vec![
        "kickstart".into(),
        format!("gui/{uid}/{LAUNCH_AGENT_LABEL}"),
    ]
}

#[cfg(any(target_os = "macos", test))]
fn bootstrap_args(uid: &str, plist: &str) -> Vec<String> {
    vec!["bootstrap".into(), format!("gui/{uid}"), plist.into()]
}

#[cfg(any(target_os = "macos", test))]
fn enable_args(uid: &str) -> Vec<String> {
    vec!["enable".into(), format!("gui/{uid}/{LAUNCH_AGENT_LABEL}")]
}

/// Whether `exe` is the program of an app installed in /Applications or
/// ~/Applications, the only copies the service may point the login item at
/// by itself.
#[cfg(any(target_os = "macos", test))]
fn installed_app(exe: &Path) -> bool {
    let p = exe.to_string_lossy();
    p.contains("/Applications/")
        && p.contains(".app/Contents/MacOS/")
        && !p.contains("/AppTranslocation/")
}

/// The login item exists but macOS has it turned off (Login Items in
/// System Settings, or `launchctl disable`): the Mac looks shared, and is
/// not after the next restart. Setup turns it back on.
pub fn login_item_disabled() -> bool {
    #[cfg(target_os = "macos")]
    {
        launch_agent_path().is_ok_and(|p| p.exists()) && launch_agent_disabled()
    }
    #[cfg(not(target_os = "macos"))]
    {
        false
    }
}

/// Whether `launchctl print-disabled gui/<uid>` lists the launch agent as
/// disabled ("=> disabled", or "=> true" before macOS 13).
#[cfg(any(target_os = "macos", test))]
fn listed_disabled(print_disabled: &str) -> bool {
    let label = format!("\"{LAUNCH_AGENT_LABEL}\"");
    print_disabled.lines().any(|l| {
        l.trim().strip_prefix(&label).is_some_and(|rest| {
            let v = rest.trim().trim_start_matches("=>").trim();
            v == "disabled" || v == "true"
        })
    })
}

#[cfg(target_os = "macos")]
fn launch_agent_disabled() -> bool {
    let Ok(uid) = uid() else { return false };
    Command::new("launchctl")
        .args(["print-disabled", &format!("gui/{uid}")])
        .output()
        .is_ok_and(|o| listed_disabled(&String::from_utf8_lossy(&o.stdout)))
}

#[cfg(target_os = "macos")]
fn uid() -> Result<String> {
    let out = Command::new("id").arg("-u").output().context("id -u")?;
    let uid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    anyhow::ensure!(
        out.status.success() && !uid.is_empty() && uid.bytes().all(|b| b.is_ascii_digit()),
        "id -u said {uid:?}"
    );
    Ok(uid)
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[String]) -> Result<bool> {
    Ok(Command::new("launchctl")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .context("run launchctl")?
        .success())
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(format!("Library/LaunchAgents/{LAUNCH_AGENT_LABEL}.plist")))
}

/// The launch agent file 4.0 and older wrote.
#[cfg(target_os = "macos")]
fn legacy_launch_agent_path() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(format!(
        "Library/LaunchAgents/{}.plist",
        crate::legacy::MAC_LAUNCH_AGENT
    )))
}

/// Stop launchd starting the old agent at login, by removing its file. The
/// job launchd already has loaded is left alone: it may be the process that
/// is calling this.
#[cfg(target_os = "macos")]
fn retire_legacy_launch_agent() {
    if let Ok(old) = legacy_launch_agent_path() {
        if old.exists() {
            let _ = fs::remove_file(&old);
        }
    }
}

/// A copy running from the app folder 4.0 and older used
/// moves into `Latch.app` beside it and carries on from there. The release
/// publishes the new app under the old name as well, because a 4.0 updater
/// installs only a folder of that name, and the first run lands here.
///
/// The login item goes with it: the new launch agent is written (only if
/// the old one was there, so a Mac that never started at login does not
/// begin to), the old file removed, the old job unloaded and the new one
/// loaded. Run from the window, that is done before the window starts;
/// run by launchd under the old label, it only re-registers, because
/// unloading the label would end this very process, and `exec`s the new
/// path under the old label for this session. Returns only when there was
/// nothing to adopt or something failed, in which case the copy runs on
/// where it is.
#[cfg(target_os = "macos")]
pub fn bundle_path() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let app = exe.parent()?.parent()?.parent()?;
    (app.extension().is_some_and(|e| e == "app")).then(|| app.to_path_buf())
}

#[cfg(target_os = "macos")]
pub fn adopt_legacy_bundle(background: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let Some(old) = bundle_path().filter(|b| {
        b.file_name()
            .is_some_and(|n| n == crate::legacy::MAC_BUNDLE)
    }) else {
        return Ok(());
    };
    let new = old.with_file_name("Latch.app");
    if new.exists() {
        tracing::warn!(
            "{} is here as well as {}; leaving both",
            new.display(),
            old.display()
        );
        return Ok(());
    }
    fs::rename(&old, &new).with_context(|| format!("rename {}", old.display()))?;
    tracing::info!("moved {} to {}", old.display(), new.display());
    let exe = new.join("Contents/MacOS/Latch");
    let had_agent = legacy_launch_agent_path().is_ok_and(|p| p.exists())
        || launch_agent_path().is_ok_and(|p| p.exists());
    if had_agent {
        write_launch_agent(&exe)?;
    }
    retire_legacy_launch_agent();
    if !background {
        let uid = uid().unwrap_or_default();
        // The old job, which launchd runs from the path that no longer exists.
        let _ = Command::new("launchctl")
            .args([
                "bootout",
                &format!("gui/{uid}/{}", crate::legacy::MAC_LAUNCH_AGENT),
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        // A service nobody supervised, started by an earlier window.
        let _ = latch_core::http::request(
            ("127.0.0.1", latch_core::CONTROL_PORT),
            "POST",
            "/v1/quit",
            None,
            Duration::from_secs(2),
        );
        std::thread::sleep(Duration::from_millis(600));
        if had_agent {
            if let Ok(plist) = launch_agent_path() {
                let _ = Command::new("launchctl")
                    .args(["bootstrap", &format!("gui/{uid}")])
                    .arg(&plist)
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status();
            }
        }
    }
    let args: Vec<_> = std::env::args_os().skip(1).collect();
    let error = Command::new(&exe).args(args).exec();
    anyhow::bail!("could not start {}: {error}", exe.display())
}

/// The launch agent: start at login, and restart only after a failure. A
/// copy that finds the port taken exits 0 (see `Service::run`), which with
/// `SuccessfulExit = false` is where launchd leaves it; `KeepAlive = true`
/// respawned such a copy every ten seconds for the whole session.
#[cfg(any(target_os = "macos", test))]
fn launch_agent_plist(exe: &Path) -> String {
    let exe = exe
        .display()
        .to_string()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>com.bardbro.latch.node</string>
  <key>ProgramArguments</key>
  <array>
    <string>{exe}</string>
    <string>--background</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key>
  <dict>
    <key>SuccessfulExit</key><false/>
  </dict>
</dict>
</plist>
"#
    )
}

/// Write the plist when it differs from what is on disk. `Ok(true)` when
/// it was written.
#[cfg(target_os = "macos")]
fn write_launch_agent(exe: &Path) -> Result<bool> {
    let path = launch_agent_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let plist = launch_agent_plist(exe);
    if fs::read_to_string(&path).is_ok_and(|have| have == plist) {
        return Ok(false);
    }
    fs::write(&path, plist)?;
    Ok(true)
}

/// Whether launchd has the agent in this login session. `launchctl load`
/// on a loaded job prints an error but exits 0, so it cannot tell us.
#[cfg(target_os = "macos")]
fn launch_agent_loaded() -> bool {
    Command::new("launchctl")
        .args(["list", LAUNCH_AGENT_LABEL])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

#[cfg(target_os = "macos")]
fn macos_launch_agent(enable: bool, exe: &Path) -> Result<()> {
    let path = launch_agent_path()?;
    if !enable {
        // Not starting at login means removing the file. The service launchd
        // may be running now is left alone rather than killed mid-stream.
        if path.exists() {
            fs::remove_file(&path)?;
        }
        retire_legacy_launch_agent();
        return Ok(());
    }
    write_launch_agent(exe)?;
    retire_legacy_launch_agent();
    // Turning sharing on is the owner asking for the login item, so a label
    // macOS has marked disabled is enabled: the legacy `launchctl load`
    // skipped such a job without a word, and the service then ran with
    // nothing to restart it, and not at all after the next login.
    let uid = uid()?;
    let _ = launchctl(&enable_args(&uid));
    if launch_agent_loaded() {
        return Ok(());
    }
    // RunAtLoad starts the service now; if one already answers on the
    // port, the new copy exits 0 and launchd leaves it.
    let _ = launchctl(&bootstrap_args(&uid, &path.display().to_string()));
    anyhow::ensure!(
        launch_agent_loaded(),
        "macOS did not load Latch's login item; allow Latch in System Settings > General > Login Items"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn user_unit_path() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(".config/systemd/user/latch.service"))
}

/// The user unit 4.0 and older wrote.
#[cfg(target_os = "linux")]
fn legacy_user_unit_path() -> Result<PathBuf> {
    let home = dirs_home()?;
    Ok(home.join(format!(
        ".config/systemd/user/{}",
        crate::legacy::LINUX_USER_UNIT
    )))
}

/// Stop systemd starting the old unit at login: disabled and its file
/// removed. It is not stopped, because it may be the one running this.
#[cfg(target_os = "linux")]
fn retire_legacy_user_unit() {
    let Ok(old) = legacy_user_unit_path() else {
        return;
    };
    if old.exists() {
        let _ = systemctl_user(&["disable", crate::legacy::LINUX_USER_UNIT]);
        let _ = fs::remove_file(&old);
        let _ = systemctl_user(&["daemon-reload"]);
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn systemctl_user(args: &[&str]) -> Result<bool> {
    Ok(Command::new("systemctl")
        .arg("--user")
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()?
        .success())
}

/// Write the unit when it differs from what is on disk, and tell systemd.
/// `Restart=on-failure` leaves a copy that exited 0 because another one
/// already serves.
#[cfg(target_os = "linux")]
fn write_user_unit(exe: &Path) -> Result<()> {
    let path = user_unit_path()?;
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let unit = format!(
        "[Unit]\nDescription=Latch\nAfter=network.target\n\n[Service]\nExecStart=\"{}\" --background\nRestart=on-failure\n\n[Install]\nWantedBy=default.target\n",
        exe.display().to_string().replace('\\', "\\\\").replace('"', "\\\"")
            .replace('%', "%%").replace('$', "$$").replace('\n', "\\n").replace('\r', "\\r")
    );
    if fs::read_to_string(&path).is_ok_and(|have| have == unit) {
        return Ok(());
    }
    fs::write(&path, unit)?;
    anyhow::ensure!(
        systemctl_user(&["daemon-reload"])?,
        "could not reload user services"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn linux_user_unit(enable: bool, exe: &Path) -> Result<()> {
    let path = user_unit_path()?;
    if !enable {
        let _ = systemctl_user(&["disable", "latch.service"]);
        if path.exists() {
            fs::remove_file(&path)?;
        }
        retire_legacy_user_unit();
        let _ = systemctl_user(&["daemon-reload"]);
        return Ok(());
    }
    write_user_unit(exe)?;
    retire_legacy_user_unit();
    anyhow::ensure!(
        systemctl_user(&["enable", "--now", "latch.service"])?,
        "could not enable Latch service"
    );
    Ok(())
}

fn dirs_home() -> Result<PathBuf> {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("HOME is not set")
}

#[cfg(test)]
mod tests {
    #[test]
    fn upgrading_drops_only_the_engine_lines_from_a_drop_in() {
        let latch4 = "[Unit]\nRequires=latch-engine.service\nAfter=latch-engine.service\n\n[Service]\nEnvironment=DISPLAY=:1\nEnvironment=LATCH_DISPLAY_FOLLOWS_VIEWER=1\nExecStartPre=/usr/bin/python3 /usr/local/libexec/latch-wait-engine.py\nRestart=on-failure\n";
        let kept = without_engine_lines(latch4);
        assert_eq!(
            kept,
            "[Unit]\n\n[Service]\nEnvironment=DISPLAY=:1\nEnvironment=LATCH_DISPLAY_FOLLOWS_VIEWER=1\nRestart=on-failure\n"
        );
        assert_eq!(without_engine_lines(&kept), kept, "a second run changes nothing");
    }

    use super::*;

    #[test]
    fn launch_agent_restarts_only_after_a_failure_and_escapes_the_path() {
        let plist = launch_agent_plist(Path::new(
            "/Applications/B&L's <app>.app/Contents/MacOS/Latch",
        ));
        assert!(
            plist.contains("<key>SuccessfulExit</key><false/>"),
            "{plist}"
        );
        assert!(!plist.contains("<key>KeepAlive</key><true/>"), "{plist}");
        assert!(plist.contains("<string>--background</string>"));
        assert!(plist.contains("B&amp;L&apos;s &lt;app&gt;.app"), "{plist}");
        assert!(plist.contains("<key>RunAtLoad</key><true/>"));
    }

    #[test]
    fn the_service_starts_through_its_login_item_when_there_is_one() {
        use LoginItem::*;
        let mac = |loaded, disabled| MacAgent { loaded, disabled };
        assert_eq!(start_plan(mac(false, false)), StartPlan::Bootstrap);
        // Stopped by the window (Share off then on, or Stop then Start):
        // the job is still loaded but nothing runs.
        assert_eq!(start_plan(mac(true, false)), StartPlan::Kickstart);
        // Turned off in Login Items: nothing launchd would start.
        assert_eq!(start_plan(mac(false, true)), StartPlan::Spawn);
        assert_eq!(start_plan(mac(true, true)), StartPlan::Spawn);
        assert_eq!(start_plan(LinuxUnit), StartPlan::SystemdStart);
        assert_eq!(start_plan(None), StartPlan::Spawn);

        let kick = kickstart_args("501");
        assert_eq!(kick, ["kickstart", "gui/501/com.bardbro.latch.node"]);
        assert!(
            !kick.iter().any(|a| a == "-k"),
            "-k would kill a service that runs"
        );
        assert_eq!(
            bootstrap_args("501", "/Users/a/Library/LaunchAgents/x.plist"),
            [
                "bootstrap",
                "gui/501",
                "/Users/a/Library/LaunchAgents/x.plist"
            ]
        );
        assert_eq!(
            enable_args("501"),
            ["enable", "gui/501/com.bardbro.latch.node"]
        );
    }

    #[test]
    fn only_an_installed_app_takes_the_login_item_by_itself() {
        for ok in [
            "/Applications/Latch.app/Contents/MacOS/Latch",
            "/Users/ada/Applications/Latch.app/Contents/MacOS/Latch",
        ] {
            assert!(installed_app(Path::new(ok)), "{ok}");
        }
        for scratch in [
            "/Users/ada/dev/latch/dist.noindex/Latch-prev-2026-10-12.app/Contents/MacOS/Latch",
            "/Volumes/Latch/Latch.app/Contents/MacOS/Latch",
            "/private/var/folders/x/T/AppTranslocation/1234/d/Latch.app/Contents/MacOS/Latch",
            "/Users/ada/dev/latch/target/release/latch-host",
        ] {
            assert!(!installed_app(Path::new(scratch)), "{scratch}");
        }
    }

    #[test]
    fn a_disabled_login_item_and_a_systemd_service_are_recognised() {
        // `launchctl print-disabled gui/501` on the Mac, 2026-10-17.
        let listing = "disabled services = {\n\t\t\"com.apple.x\" => enabled\n\t\t\"com.bardbro.latch.node\" => disabled\n\t}\n";
        assert!(listed_disabled(listing));
        assert!(listed_disabled("\t\"com.bardbro.latch.node\" => true\n"));
        assert!(!listed_disabled(
            "\t\"com.bardbro.latch.node\" => enabled\n"
        ));
        assert!(!listed_disabled(
            "\t\"com.bardbro.latch.node.extra\" => disabled\n"
        ));
        assert!(in_latch_unit(
            "0::/user.slice/user-1004.slice/user@1004.service/app.slice/latch.service\n"
        ));
        assert!(in_latch_unit(
            "1:name=systemd:/user.slice/user@1000.service/latch.service\n"
        ));
        assert!(!in_latch_unit(
            "0::/user.slice/user-1004.slice/user@1004.service/app.slice/latch-engine.service\n"
        ));
        assert!(!in_latch_unit(
            "0::/user.slice/user-1004.slice/session-3.scope\n"
        ));
    }

    #[test]
    fn a_firewall_that_blocks_latch_is_recognised_and_opened_by_path() {
        assert!(firewall_on("Firewall is enabled. (State = 1)\n"));
        assert!(!firewall_on("Firewall is disabled. (State = 0)\n"));
        assert!(app_blocked(
            "Incoming connection to /Applications/Latch.app/Contents/MacOS/Latch is blocked.\n"
        ));
        assert!(!app_blocked(
            "Incoming connection to /Applications/Latch.app/Contents/MacOS/Latch is permitted.\n"
        ));
        let script = firewall_allow_script("/Applications/My \"Latch\".app/Contents/MacOS/Latch");
        assert!(
            script.starts_with(
                "set exe to \"/Applications/My \\\"Latch\\\".app/Contents/MacOS/Latch\"\n"
            ),
            "{script}"
        );
        assert!(script.contains("--add \" & quoted form of exe"), "{script}");
        assert!(
            script.contains("--unblockapp \" & quoted form of exe"),
            "{script}"
        );
        assert!(
            script.ends_with("with administrator privileges"),
            "{script}"
        );
    }
    #[test]
    fn non_latch_sunshine_is_left_alone_and_latch_sunshine_is_backed_up() {
        let tmp = std::env::temp_dir().join(format!("latch-test-sunshine-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();

        // 1. Non-Latch Sunshine: regular user sunshine with steam and default config
        let user_sunshine = tmp.join("user_sunshine");
        fs::create_dir_all(&user_sunshine).unwrap();
        fs::write(
            user_sunshine.join("sunshine.conf"),
            "port = 47989\nencoder = nvenc\n",
        )
        .unwrap();
        fs::write(
            user_sunshine.join("apps.json"),
            r#"{"apps":[{"name":"Steam"}]}"#,
        )
        .unwrap();

        assert!(!is_latch4_sunshine(&user_sunshine));
        retire_legacy_sunshine(&user_sunshine).unwrap();
        assert!(
            user_sunshine.exists(),
            "non-Latch sunshine directory must be left alone"
        );
        assert!(user_sunshine.join("apps.json").exists());

        // 2. Latch 4 Sunshine: identified by latch-web.json
        let latch_sunshine = tmp.join("latch_sunshine");
        fs::create_dir_all(&latch_sunshine).unwrap();
        fs::write(
            latch_sunshine.join("sunshine.conf"),
            "origin_web_ui_allowed = pc\n",
        )
        .unwrap();
        fs::write(latch_sunshine.join("latch-web.json"), "{}").unwrap();

        assert!(is_latch4_sunshine(&latch_sunshine));
        retire_legacy_sunshine(&latch_sunshine).unwrap();
        assert!(
            !latch_sunshine.exists(),
            "Latch 4 sunshine directory must be moved aside"
        );
        let backups: Vec<_> = fs::read_dir(&tmp)
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("latch_sunshine.bak-")
            })
            .collect();
        assert_eq!(backups.len(), 1, "exactly one backup directory created");
        assert!(backups[0].path().join("latch-web.json").exists());

        // 3. Latch 4 Sunshine: identified by apps.json with desktop.png
        let latch_sunshine_apps = tmp.join("latch_sunshine_apps");
        fs::create_dir_all(&latch_sunshine_apps).unwrap();
        fs::write(
            latch_sunshine_apps.join("apps.json"),
            r#"{"apps":[{"name":"Desktop","image-path":"desktop.png"}]}"#,
        )
        .unwrap();
        assert!(is_latch4_sunshine(&latch_sunshine_apps));
        retire_legacy_sunshine(&latch_sunshine_apps).unwrap();
        assert!(!latch_sunshine_apps.exists());

        let _ = fs::remove_dir_all(&tmp);
    }
}

#[cfg(target_os = "macos")]
mod permissions {
    use std::ffi::c_void;
    #[link(name = "CoreGraphics", kind = "framework")]
    unsafe extern "C" {
        fn CGPreflightScreenCaptureAccess() -> bool;
        fn CGRequestScreenCaptureAccess() -> bool;
    }
    #[link(name = "ApplicationServices", kind = "framework")]
    unsafe extern "C" {
        fn AXIsProcessTrusted() -> bool;
        fn AXIsProcessTrustedWithOptions(options: *const c_void) -> bool;
        static kAXTrustedCheckOptionPrompt: *const c_void;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        static kCFBooleanTrue: *const c_void;
        fn CFDictionaryCreate(
            allocator: *const c_void,
            keys: *const *const c_void,
            values: *const *const c_void,
            count: isize,
            key_callbacks: *const c_void,
            value_callbacks: *const c_void,
        ) -> *const c_void;
        fn CFRelease(value: *const c_void);
    }
    pub fn items() -> Vec<String> {
        let mut items = Vec::new();
        unsafe {
            if !CGPreflightScreenCaptureAccess() {
                items.push("Allow Latch to record the screen in System Settings → Privacy & Security → Screen Recording.".into());
            }
            if !AXIsProcessTrusted() {
                items.push("Allow Latch in System Settings → Privacy & Security → Accessibility to use the keyboard and mouse.".into());
            }
        }
        items
    }
    pub fn request() {
        unsafe {
            CGRequestScreenCaptureAccess();
            let options = CFDictionaryCreate(
                std::ptr::null(),
                &kAXTrustedCheckOptionPrompt,
                &kCFBooleanTrue,
                1,
                std::ptr::null(),
                std::ptr::null(),
            );
            if !options.is_null() {
                AXIsProcessTrustedWithOptions(options);
                CFRelease(options);
            }
        }
    }
}

#[cfg(target_os = "macos")]
pub fn permission_items() -> Vec<String> {
    permissions::items()
}
#[cfg(target_os = "macos")]
fn request_permissions() {
    permissions::request();
}
