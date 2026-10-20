//! Install and start the streaming engine on macOS and Linux.
//!
//! Windows still uses the elevated PowerShell script. Here there is no UAC
//! prompt: Latch unpacks Sunshine next to its own data, writes the same
//! conf keys, and keeps the process up from the background service.

use anyhow::{bail, Context, Result};
#[cfg(target_os = "macos")]
use latch_core::update::sha256_hex;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use crate::setup::{conceal_conf, Plan, DESKTOP_APPS_JSON};
use crate::streamer::{self, Install};

pub const MAC_DMG: &str = "Sunshine-macOS-arm64.dmg";
pub const MAC_DMG_SHA256: &str = "b630d35a184d8eaff39c5104f3c6a0c40e91ddc447ccf7a0c5b24706465fab6a";

/// Sunshine v2026.906 stores SHA-256(password + salt) as reversed uppercase
/// hexadecimal (http::save_user_creds / util::Hex). A separate credentials file
/// avoids modifying its paired-client state and keeps secrets out of argv.
fn write_engine_login(path: &Path, user: &str, password: &str) -> Result<()> {
    use std::io::Write;
    let salt = crate::config::random_password();
    let hash = latch_core::update::sha256_hex(format!("{password}{salt}").as_bytes());
    let hash: String = hash
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .rev()
        .map(|pair| std::str::from_utf8(pair).expect("hex"))
        .collect::<String>()
        .to_uppercase();
    let body =
        serde_json::to_vec(&serde_json::json!({"username":user, "salt":salt, "password":hash}))?;
    let tmp = path.with_extension(format!("{}.tmp", crate::config::random_password()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(&body)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&tmp, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn engine_dir() -> Result<PathBuf> {
    Ok(latch_core::config::data_dir()?.join("engine"))
}

pub fn conf_path() -> Result<PathBuf> {
    Ok(engine_dir()?.join("config").join("sunshine.conf"))
}

/// The engine login file an earlier version wrote, and what it is called now.
const OLD_LOGIN_FILE: &str = "brolink-web.json";
const LOGIN_FILE: &str = "latch-web.json";

/// An install that an older version set up has two things in the engine's
/// `sunshine.conf` that name it: the engine login file under its old
/// filename wherever setup put it, and any path
/// under the old data folder, which this copy has moved. Point both at what
/// is there now, and rename the login file, so the engine finds the login
/// it was set up with. Nothing happens on an install that was never moved.
pub fn repair_adopted_engine_config() {
    let (Ok(conf), Ok(data), Some(old)) = (
        conf_path(),
        latch_core::config::data_dir(),
        latch_core::legacy::data_dir(),
    ) else {
        return;
    };
    repair_conf(
        &conf,
        &old.display().to_string(),
        &data.display().to_string(),
    );
}

/// [`repair_adopted_engine_config`] for the conf at `conf`, with the old and
/// the new data folder as strings.
fn repair_conf(conf: &Path, old: &str, data: &str) {
    let Ok(text) = fs::read_to_string(conf) else {
        return;
    };
    let fixed: Vec<String> = text
        .lines()
        .map(|line| match line.split_once('=') {
            Some((key, value)) if key.trim() == "credentials_file" => {
                let path = PathBuf::from(value.trim().replace(old, data));
                if path.file_name().is_some_and(|n| n == OLD_LOGIN_FILE) {
                    let renamed = path.with_file_name(LOGIN_FILE);
                    if path.exists() && !renamed.exists() {
                        let _ = fs::rename(&path, &renamed);
                    }
                    return format!("credentials_file = {}", renamed.display());
                }
                format!("credentials_file = {}", path.display())
            }
            _ => line.replace(old, data),
        })
        .collect();
    let fixed = fixed.join("\n") + "\n";
    if fixed != text {
        let _ = fs::write(conf, fixed);
    }
}

/// Candidate engine installs, Latch's copy first.
pub fn candidates() -> Vec<Install> {
    let mut v = Vec::new();
    if let Ok(dir) = engine_dir() {
        v.push(Install { kind: "Latch", dir });
    }
    #[cfg(target_os = "linux")]
    {
        // Latch's renamed engine binary, then the distro package we wrap.
        for p in ["/usr/local/bin", "/usr/bin"] {
            v.push(Install {
                kind: "Latch",
                dir: PathBuf::from(p),
            });
        }
    }
    v
}

/// The names the engine's program may have, Latch's first, an earlier
/// version's after it, and Sunshine's own last.
fn engine_exe_names() -> impl Iterator<Item = &'static str> {
    ["LatchStreaming", "latch-engine"]
        .into_iter()
        .chain(crate::legacy::ENGINE_EXE_NAMES)
        .chain(std::iter::once("sunshine"))
}

pub fn exe_in(dir: &Path) -> PathBuf {
    for name in engine_exe_names() {
        let app = dir.join("Contents/MacOS").join(name);
        if app.exists() {
            return app;
        }
        let nested = dir.join("Sunshine.app/Contents/MacOS").join(name);
        if nested.exists() {
            return nested;
        }
        let bin = dir.join(name);
        if bin.exists() {
            return bin;
        }
    }
    dir.join("latch-engine")
}

pub fn run(p: &Plan<'_>) -> Result<()> {
    let log =
        crate::setup::log_path().unwrap_or_else(|| std::env::temp_dir().join("latch-setup.log"));
    let mut lines = Vec::new();
    let result = run_inner(p, &mut lines);
    let body = lines.join("\n") + "\n";
    let _ = fs::write(&log, &body);
    result.with_context(|| format!("see {}", log.display()))
}

fn run_inner(p: &Plan<'_>, log: &mut Vec<String>) -> Result<()> {
    fn step(log: &mut Vec<String>, msg: impl Into<String>) {
        let msg = msg.into();
        tracing::info!("{msg}");
        log.push(msg);
    }

    let dir = engine_dir()?;
    fs::create_dir_all(dir.join("config")).context("engine config dir")?;
    step(log, format!("engine directory {}", dir.display()));

    if p.install_engine || streamer::find().is_none() {
        install_engine(&dir, log)?;
    }

    // A native Linux node (deploy/native, stuzha) runs the engine as a
    // systemd user unit, with the desktop's environment and its priority.
    // Killing it there stopped the unit for good (a clean exit is no
    // failure to restart from), and the copy started here instead lacked
    // DISPLAY when setup ran over SSH: every stream failed while the port
    // still answered.
    let unit = engine_unit();
    if unit.is_none() {
        stop_engine();
    }
    let conf = conf_path()?;
    let existing = fs::read_to_string(&conf).unwrap_or_default();
    let credentials = conf.with_file_name(LOGIN_FILE);
    let profile = conceal_conf(&existing)
        .lines()
        .filter(|line| {
            line.split_once('=')
                .is_none_or(|(key, _)| key.trim() != "credentials_file")
        })
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(
        &conf,
        format!("{profile}\ncredentials_file = {}\n", credentials.display()),
    )
    .context("write sunshine.conf")?;
    let apps = conf
        .parent()
        .map(|p| p.join("apps.json"))
        .unwrap_or_else(|| dir.join("config").join("apps.json"));
    fs::write(&apps, DESKTOP_APPS_JSON).context("write apps.json")?;
    step(
        log,
        "wrote streaming profile (constant bitrate, Tailscale packet size)",
    );

    let Some(install) = streamer::find() else {
        bail!("the streaming engine is not installed");
    };
    if !p.sunshine_user.is_empty() && !p.sunshine_pass.is_empty() {
        write_engine_login(&credentials, p.sunshine_user, p.sunshine_pass)?;
        step(log, "engine login set");
    }

    if let Some(unit) = unit {
        // Through `Requires=`, Latch's own unit restarts with it.
        anyhow::ensure!(
            systemctl_user(&["restart", unit])?,
            "systemctl --user restart {unit} failed"
        );
        // Its start may wait for the X screen first.
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        while !streamer::running() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(250));
        }
        anyhow::ensure!(streamer::running(), "{unit} did not start listening");
        step(
            log,
            format!("restarted the streaming engine's unit, {unit}"),
        );
    } else {
        stop_engine();
        std::thread::sleep(Duration::from_millis(400));
        streamer::start(&install).context("start streaming engine")?;
        step(log, "streaming engine started");
    }

    if let Ok(exe) = std::env::current_exe() {
        set_autostart(true, &exe)?;
        step(log, "Latch starts at login so others can connect");
        #[cfg(target_os = "macos")]
        if firewall_blocks(&exe) {
            allow_through_firewall(&exe)?;
            step(log, "the macOS firewall lets other machines reach Latch");
        }
    }
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

fn install_engine(dir: &Path, log: &mut Vec<String>) -> Result<()> {
    if exe_in(dir).exists() {
        log.push("engine already unpacked".into());
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        install_macos(dir, log)
    }
    #[cfg(target_os = "linux")]
    {
        if engine_exe_names().any(|n| which(n).is_some()) {
            log.push("using the system streaming engine".into());
            return Ok(());
        }
        bail!(
            "the streaming engine is not installed. On this VPS use the Latch node Docker image, or run setup again."
        );
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = (dir, log);
        bail!("no streaming engine for this OS");
    }
}

/// Rename the engine so it does not appear as a second app in the Dock
/// or Activity Monitor. The on-disk `.app` folder keeps its upstream name
/// because the engine looks up resources next to that bundle.
pub fn conceal_info_plist(xml: &str) -> String {
    let mut xml = set_plist_string(xml, "CFBundleName", "Latch");
    xml = set_plist_string(&xml, "CFBundleDisplayName", "Latch");
    xml = set_plist_string(&xml, "CFBundleExecutable", "LatchStreaming");
    if xml.contains("LSUIElement") {
        xml
    } else {
        xml.replacen("</dict>", "  <key>LSUIElement</key>\n  <true/>\n</dict>", 1)
    }
}

fn set_plist_string(xml: &str, key: &str, value: &str) -> String {
    let needle = format!("<key>{key}</key>");
    let Some(at) = xml.find(&needle) else {
        return xml.to_string();
    };
    let rest = &xml[at + needle.len()..];
    let Some(s) = rest.find("<string>") else {
        return xml.to_string();
    };
    let inner = s + 8;
    let Some(e) = rest[inner..].find("</string>") else {
        return xml.to_string();
    };
    let start = at + needle.len() + inner;
    let end = start + e;
    format!("{}{value}{}", &xml[..start], &xml[end..])
}

#[cfg(target_os = "macos")]
fn install_macos(dir: &Path, log: &mut Vec<String>) -> Result<()> {
    let dest = dir.join("Sunshine.app");
    if engine_exe_names().any(|n| dest.join("Contents/MacOS").join(n).exists()) {
        conceal_engine_bundle(&dest)?;
        log.push("streaming engine already unpacked".into());
        return Ok(());
    }
    let dmg = dir.join(MAC_DMG);
    let url = format!(
        "https://github.com/LizardByte/Sunshine/releases/download/{}/{MAC_DMG}",
        crate::setup::ENGINE_TAG
    );
    log.push("downloading the streaming engine".into());
    let status = Command::new("curl")
        .args(["-fsSL", "-A", "latch", "-o"])
        .arg(&dmg)
        .arg(&url)
        .status()
        .context("download streaming engine")?;
    anyhow::ensure!(status.success(), "download of the streaming engine failed");
    let bytes = fs::read(&dmg).context("read engine archive")?;
    let got = sha256_hex(&bytes);
    anyhow::ensure!(
        got.eq_ignore_ascii_case(MAC_DMG_SHA256),
        "streaming engine digest mismatch: expected {MAC_DMG_SHA256}, got {got}"
    );
    let mount = std::env::temp_dir().join(format!("latch-engine-{}", std::process::id()));
    let _ = fs::create_dir_all(&mount);
    let attach = Command::new("hdiutil")
        .args(["attach", "-nobrowse", "-readonly", "-mountpoint"])
        .arg(&mount)
        .arg(&dmg)
        .status()
        .context("hdiutil attach")?;
    anyhow::ensure!(attach.success(), "could not mount the streaming engine");
    let copied = (|| {
        let src = mount.join("Sunshine.app");
        anyhow::ensure!(src.is_dir(), "engine app missing from the disk image");
        let _ = fs::remove_dir_all(&dest);
        let status = Command::new("cp")
            .args(["-R"])
            .arg(&src)
            .arg(&dest)
            .status()
            .context("copy streaming engine")?;
        anyhow::ensure!(status.success(), "copy of the streaming engine failed");
        Ok(())
    })();
    let _ = Command::new("hdiutil")
        .args(["detach", "-quiet"])
        .arg(&mount)
        .status();
    let _ = fs::remove_file(&dmg);
    copied?;
    conceal_engine_bundle(&dest)?;
    log.push("streaming engine unpacked into Latch's data folder".into());
    Ok(())
}

#[cfg(target_os = "macos")]
fn conceal_engine_bundle(app: &Path) -> Result<()> {
    let macos = app.join("Contents/MacOS");
    let dest = macos.join("LatchStreaming");
    // Upstream's name, or the name an earlier version gave it.
    for from in ["sunshine", "BroLinkStreaming"] {
        let src = macos.join(from);
        if src.exists() && !dest.exists() {
            fs::rename(&src, &dest).context("rename engine binary")?;
        }
    }
    let plist = app.join("Contents/Info.plist");
    if plist.exists() {
        let xml = fs::read_to_string(&plist).unwrap_or_default();
        fs::write(&plist, conceal_info_plist(&xml)).context("write engine Info.plist")?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn which(name: &str) -> Option<PathBuf> {
    Command::new("which")
        .arg(name)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .map(PathBuf::from)
}

/// The systemd user unit that runs the engine on this machine, when there is
/// one: then the engine is systemd's to stop and start, never Latch's.
pub fn engine_unit() -> Option<&'static str> {
    #[cfg(target_os = "linux")]
    {
        const UNIT: &str = "latch-engine.service";
        let out = Command::new("systemctl")
            .args([
                "--user",
                "show",
                "-p",
                "LoadState",
                "-p",
                "UnitFileState",
                UNIT,
            ])
            .stderr(Stdio::null())
            .output()
            .ok()
            .filter(|o| o.status.success())?;
        unit_installed(&String::from_utf8_lossy(&out.stdout)).then_some(UNIT)
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// `systemctl show -p LoadState -p UnitFileState` of a unit that exists
/// and is not masked. ("not-found" when nobody installed one.)
#[cfg(any(target_os = "linux", test))]
fn unit_installed(show: &str) -> bool {
    show.lines().any(|l| l.trim() == "LoadState=loaded")
}

pub fn stop_engine() {
    for name in engine_exe_names() {
        let _ = Command::new("pkill")
            .args(["-x", name])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
    }
}

/// Register or remove the login item from the control panel. Turning it on
/// also loads it now unless launchd or systemd already has it; turning it
/// off only removes the file, so a service they are running now keeps
/// running until logout.
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
pub fn adopt_legacy_bundle(background: bool) -> Result<()> {
    use std::os::unix::process::CommandExt;
    let Some(old) = latch_client::update::bundle_path().filter(|b| {
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
    bail!("could not start {}: {error}", exe.display())
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
    fn an_engine_systemd_runs_is_left_to_systemd() {
        assert!(unit_installed("LoadState=loaded\nUnitFileState=static\n"));
        assert!(unit_installed("LoadState=loaded\nUnitFileState=enabled\n"));
        assert!(!unit_installed("LoadState=not-found\nUnitFileState=\n"));
        assert!(!unit_installed("LoadState=masked\nUnitFileState=masked\n"));
        assert!(!unit_installed(""));
    }

    #[test]
    fn an_older_installs_engine_login_is_found_under_its_new_name() {
        let dir = std::env::temp_dir().join(format!("latch-repair-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let conf = dir.join("sunshine.conf");
        let old_login = dir.join("brolink-web.json");
        fs::write(&old_login, "{}").unwrap();
        fs::write(
            &conf,
            format!(
                "origin_web_ui_allowed = pc\ncredentials_file = {}\nlog_path = /old/share/brolink/sunshine.log\n",
                old_login.display()
            ),
        )
        .unwrap();
        repair_conf(&conf, "/old/share/brolink", "/new/share/latch");
        let fixed = fs::read_to_string(&conf).unwrap();
        assert!(
            fixed.contains(&format!(
                "credentials_file = {}",
                dir.join("latch-web.json").display()
            )),
            "{fixed}"
        );
        assert!(
            fixed.contains("log_path = /new/share/latch/sunshine.log"),
            "{fixed}"
        );
        assert!(fixed.contains("origin_web_ui_allowed = pc"), "{fixed}");
        assert!(dir.join("latch-web.json").exists() && !old_login.exists());
        // A second pass changes nothing.
        repair_conf(&conf, "/old/share/brolink", "/new/share/latch");
        assert_eq!(fs::read_to_string(&conf).unwrap(), fixed);
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn engine_login_uses_sunshine_hash_format_without_plaintext() {
        let dir =
            std::env::temp_dir().join(format!("latch-login-{}", crate::config::random_password()));
        fs::create_dir(&dir).unwrap();
        let path = dir.join("login.json");
        write_engine_login(&path, "user", "secret").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(!text.contains("secret"));
        assert_eq!(value["username"], "user");
        let expected = latch_core::update::sha256_hex(
            format!("secret{}", value["salt"].as_str().unwrap()).as_bytes(),
        );
        let stored = value["password"].as_str().unwrap();
        let reversed: String = stored
            .as_bytes()
            .as_chunks::<2>()
            .0
            .iter()
            .rev()
            .map(|b| std::str::from_utf8(b).unwrap())
            .collect();
        assert_eq!(reversed.to_lowercase(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn mac_dmg_pin_is_64_hex() {
        assert_eq!(MAC_DMG_SHA256.len(), 64);
        assert_eq!(MAC_DMG, "Sunshine-macOS-arm64.dmg");
        assert!(!crate::setup::ENGINE_TAG.is_empty());
        assert!(crate::setup::ENGINE_CONF
            .iter()
            .any(|(k, v)| *k == "amd_rc" && *v == "cbr"));
        assert!(crate::setup::ENGINE_CONF
            .iter()
            .any(|(k, v)| *k == "sw_preset" && *v == "ultrafast"));
        assert!(crate::setup::ENGINE_CONF
            .iter()
            .any(|(k, v)| *k == "amd_quality" && *v == "speed"));
    }

    #[test]
    fn exe_in_prefers_app_bundle_then_bare_binary() {
        let tmp = std::env::temp_dir().join(format!("latch-exe-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("Sunshine.app/Contents/MacOS")).unwrap();
        fs::write(tmp.join("Sunshine.app/Contents/MacOS/sunshine"), b"").unwrap();
        assert!(exe_in(&tmp).ends_with("Contents/MacOS/sunshine"));
        fs::write(tmp.join("Sunshine.app/Contents/MacOS/LatchStreaming"), b"").unwrap();
        assert!(exe_in(&tmp).ends_with("Contents/MacOS/LatchStreaming"));
        let _ = fs::remove_dir_all(&tmp);
    }

    #[test]
    fn conceal_info_plist_renames_the_engine_and_hides_it_from_the_dock() {
        let xml = r#"<?xml version="1.0"?>
<dict>
  <key>CFBundleName</key>
  <string>Sunshine</string>
  <key>CFBundleDisplayName</key>
  <string>Sunshine</string>
  <key>CFBundleExecutable</key>
  <string>sunshine</string>
  <key>CFBundleIdentifier</key>
  <string>dev.lizardbyte.sunshine</string>
</dict>
"#;
        let got = conceal_info_plist(xml);
        assert!(got.contains("<string>Latch</string>"), "{got}");
        assert!(got.contains("<string>LatchStreaming</string>"), "{got}");
        assert!(got.contains("LSUIElement"), "{got}");
        assert!(
            got.contains("dev.lizardbyte.sunshine"),
            "identifier stays so the engine still finds its files:\n{got}"
        );
        assert_eq!(conceal_info_plist(&got), got);
    }
}
