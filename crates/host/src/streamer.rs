//! Sunshine (or Apollo, its fork) on this PC: where it is, whether it is up,
//! and the two web-API calls Latch needs. The API is HTTPS with a
//! self-signed certificate on localhost, so calls go through `curl.exe -k`
//! rather than a TLS stack of our own.

use anyhow::{bail, Context, Result};
use latch_core::{SUNSHINE_PORT, SUNSHINE_WEB_PORT};
use std::net::{SocketAddr, TcpStream};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Where Latch unpacks its own copy of the streaming engine. Setup
/// installs here; an engine installed by someone else stays where it is.
pub const ENGINE_DIR: &str = r"C:\Program Files\Latch\engine";

/// Install directories, in the order they are preferred when several
/// exist. Latch's own engine wins: it is the one setup configured. The
/// folder an earlier version used comes next: it is Latch's engine until
/// setup moves it.
pub const INSTALL_DIRS: [(&str, &str); 4] = [
    ("Latch", ENGINE_DIR),
    ("Latch", crate::legacy::WINDOWS_ENGINE_DIR),
    ("Sunshine", r"C:\Program Files\Sunshine"),
    ("Apollo", r"C:\Program Files\Apollo"),
];

/// Whether `install` is the engine in the folder an earlier version used.
/// Setup moves it to [`ENGINE_DIR`] and registers the service under its new
/// name, so it has to run the engine step even though an engine is up.
pub fn in_old_folder(install: &Install) -> bool {
    install.dir.as_path() == Path::new(crate::legacy::WINDOWS_ENGINE_DIR)
}

/// GitHub repository the setup script fetches the engine archive from.
pub const REPO: &str = "LizardByte/Sunshine";

#[derive(Debug, Clone)]
pub struct Install {
    pub kind: &'static str,
    pub dir: PathBuf,
}

impl Install {
    pub fn exe(&self) -> PathBuf {
        #[cfg(windows)]
        {
            self.dir.join("sunshine.exe")
        }
        #[cfg(not(windows))]
        {
            crate::unix_setup::exe_in(&self.dir)
        }
    }

    pub fn conf(&self) -> PathBuf {
        #[cfg(windows)]
        {
            self.dir.join("config").join("sunshine.conf")
        }
        #[cfg(not(windows))]
        {
            crate::unix_setup::conf_path()
                .unwrap_or_else(|_| self.dir.join("config").join("sunshine.conf"))
        }
    }
}

/// The end of Sunshine's log, which says which encoder it settled on and
/// whether it could capture audio. `None` when it cannot be read.
pub fn log_text(install: &Install) -> Option<String> {
    #[cfg(windows)]
    let paths = vec![install.conf().with_file_name("sunshine.log")];
    #[cfg(not(windows))]
    let paths = {
        let mut paths = vec![install.conf().with_file_name("sunshine.log")];
        if let Ok(home) = std::env::var("HOME") {
            paths.push(std::path::PathBuf::from(home).join(".config/sunshine/sunshine.log"));
        }
        paths.push(std::path::PathBuf::from(
            "/root/.config/sunshine/sunshine.log",
        ));
        paths
    };
    paths.into_iter().find_map(|p| read_tail(&p, 512 * 1024))
}

/// The encoder family Sunshine settled on at its last start, from its log:
/// "nvenc", "amf", "quicksync", "software", or `None` when the log says
/// nothing. Software means no GPU encoder worked, which makes every stream
/// slow whatever the network does. The last `Found H.264 encoder: <name>
/// [<family>]` line counts, or the HEVC one when there is no H.264 line.
pub fn encoder_in(log: &str) -> Option<String> {
    let family = |line: &str| -> Option<String> {
        let start = line.rfind('[')? + 1;
        let end = line[start..].find(']')? + start;
        let f = line[start..end].trim();
        (!f.is_empty()).then(|| f.to_string())
    };
    for key in ["Found H.264 encoder:", "Found HEVC encoder:"] {
        if let Some(line) = log.lines().rev().find(|l| l.contains(key)) {
            if let Some(f) = family(line) {
                return Some(f);
            }
        }
    }
    None
}

/// Why Sunshine's audio capture failed, in its own words, when the last
/// thing its log says about audio is a failure rather than a working
/// capture format. A PC with no monitor or speakers usually has no audio
/// endpoint at all; Sunshine then streams silence and says so here.
pub fn audio_problem_in(log: &str) -> Option<String> {
    const FAILED: [&str; 8] = [
        "Unable to initialize audio capture",
        "There will be no audio",
        "Couldn't get default audio endpoint",
        "Couldn't find audio sink",
        "Audio sink not found",
        "Couldn't find supported format for audio",
        "Couldn't initialize audio client",
        "Couldn't initialize audio capture client",
    ];
    const WORKED: [&str; 2] = ["Audio capture format is", "Opus initialized"];
    let mut problem = None;
    for line in log.lines() {
        // Encoder probe (and the engine's own "reset default device" at
        // boot) logs audio failures that the engine then tells us to ignore.
        if line.contains("Ignore any errors mentioned above")
            || WORKED.iter().any(|k| line.contains(k))
        {
            problem = None;
        } else if FAILED.iter().any(|k| line.contains(k)) {
            problem = Some(log_message(line));
        }
    }
    problem
}

/// `[2026-09-07 10:00:00.001]: Error: Couldn't …` without its prefix.
fn log_message(line: &str) -> String {
    let l = line.trim();
    let l = match (l.starts_with('['), l.find("]: ")) {
        (true, Some(i)) => &l[i + 3..],
        _ => l,
    };
    ["Error: ", "Warning: ", "Info: ", "Fatal: "]
        .iter()
        .find_map(|p| l.strip_prefix(p))
        .unwrap_or(l)
        .trim()
        .to_string()
}

/// The last `max` bytes of a file as text, from a line boundary.
fn read_tail(path: &std::path::Path, max: u64) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    if len > max {
        f.seek(SeekFrom::Start(len - max)).ok()?;
    }
    let mut buf = Vec::new();
    f.read_to_end(&mut buf).ok()?;
    let text = String::from_utf8_lossy(&buf).into_owned();
    Some(match text.find('\n') {
        Some(i) if len > max => text[i + 1..].to_string(),
        _ => text,
    })
}

/// The size of the X screen the engine captures on Linux. Sunshine records
/// the whole root window of `$DISPLAY` and scales it to whatever size a
/// viewer asked for, so this is the one size that arrives unscaled. `None`
/// elsewhere, when no X server answers, and when the desktop resizes itself
/// to each viewer (`LATCH_DISPLAY_FOLLOWS_VIEWER=1`: a prep command switches
/// the X screen to the size asked for), so viewers ask for their own size.
#[cfg(target_os = "linux")]
pub fn capture_size() -> Option<[u32; 2]> {
    use x11rb::connection::Connection;
    if std::env::var_os("LATCH_DISPLAY_FOLLOWS_VIEWER").is_some_and(|v| v == "1") {
        return None;
    }
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?;
    Some([root.width_in_pixels.into(), root.height_in_pixels.into()])
}

#[cfg(not(target_os = "linux"))]
pub fn capture_size() -> Option<[u32; 2]> {
    None
}

/// The frame rate the engine keeps up with here at 1080p, 0 for no known
/// limit: 30 when it encodes in software on fewer than four cores (a viewer
/// asking for a smaller picture raises it in proportion). On a two-core
/// Ampere VPS, desktop sessions asked for at 60 fps averaged 33 to 47
/// encoded frames a second by Sunshine's own per-session counts; Sunshine
/// took 136% CPU streaming a still desktop at 60 fps and 78% streaming a
/// busier one at 30, leaving the desktop itself the rest of two cores.
pub fn max_fps(encoder: &str, cores: usize) -> u32 {
    if encoder == "software" && cores < 4 {
        30
    } else {
        0
    }
}

pub fn find() -> Option<Install> {
    #[cfg(windows)]
    {
        INSTALL_DIRS
            .iter()
            .map(|(kind, dir)| Install {
                kind,
                dir: PathBuf::from(dir),
            })
            .find(|i| i.exe().exists())
    }
    #[cfg(not(windows))]
    {
        crate::unix_setup::candidates()
            .into_iter()
            .find(|i| i.exe().exists())
    }
}

/// Start the streaming engine. On Windows this runs it as the logged-on
/// user (the service wrapper launches it as SYSTEM, which cannot see the
/// user's playback device) and puts a default endpoint back after the
/// engine's own startup clears it.
pub fn start(install: &Install) -> Result<()> {
    #[cfg(windows)]
    {
        let _ = install;
        crate::audio::take_over_engine().context("take over streaming engine")?;
        let deadline = std::time::Instant::now() + Duration::from_secs(12);
        while std::time::Instant::now() < deadline {
            if running() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(250));
        }
        if running() {
            return Ok(());
        }
        anyhow::bail!("streaming engine did not start listening")
    }
    #[cfg(not(windows))]
    {
        if running() {
            return Ok(());
        }
        let conf = install.conf();
        let mut c = Command::new(install.exe());
        if conf.exists() {
            c.arg(&conf);
        }
        c.stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null());
        let mut child = c.spawn().context("start streaming engine")?;
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let deadline = std::time::Instant::now() + Duration::from_secs(8);
        while std::time::Instant::now() < deadline {
            if running() {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(200));
        }
        anyhow::bail!("streaming engine did not start listening")
    }
}

/// Sunshine's GameStream port answers on loopback.
pub fn running() -> bool {
    let addr: SocketAddr = ([127, 0, 0, 1], SUNSHINE_PORT).into();
    TcpStream::connect_timeout(&addr, Duration::from_millis(400)).is_ok()
}

pub struct Api<'a> {
    pub user: &'a str,
    pub pass: &'a str,
}

impl Api<'_> {
    /// Everything Sunshine will say about its own capture, minus the login
    /// it is protected by. `/api/logs` answers with plain text, so the tail
    /// is taken from the raw body rather than a JSON field.
    pub fn display_diagnostics(&self) -> serde_json::Value {
        let mut result = serde_json::Map::new();
        match self.call("GET", "/api/config", None) {
            Ok(serde_json::Value::Object(config)) => {
                let kept = config
                    .into_iter()
                    .filter(|(k, _)| !is_secret(k))
                    .collect::<serde_json::Map<_, _>>();
                result.insert("config".into(), kept.into());
            }
            Ok(other) => {
                result.insert("config".into(), other);
            }
            Err(e) => {
                result.insert("config_error".into(), e.to_string().into());
            }
        }
        match self.raw("GET", "/api/logs") {
            Ok(log) => {
                // Some builds wrap the log in JSON, others return the file.
                let text = serde_json::from_str::<serde_json::Value>(log.trim())
                    .ok()
                    .and_then(|v| {
                        ["content", "logs", "log"]
                            .iter()
                            .find_map(|k| v.get(*k).and_then(|l| l.as_str()).map(String::from))
                    })
                    .unwrap_or(log);
                result.insert("log".into(), tail(&text, 150).into());
            }
            Err(e) => {
                result.insert("log_error".into(), e.to_string().into());
            }
        }
        result.into()
    }

    fn call(&self, method: &str, path: &str, body: Option<&str>) -> Result<serde_json::Value> {
        parse_reply(&self.request(method, path, body)?)
    }

    /// A GET whose reply is read as text: not every endpoint answers JSON.
    fn raw(&self, method: &str, path: &str) -> Result<String> {
        self.request(method, path, None)
    }

    fn request(&self, method: &str, path: &str, body: Option<&str>) -> Result<String> {
        let curl = if cfg!(windows) { "curl.exe" } else { "curl" };
        let mut c = Command::new(curl);
        c.args(["-sk", "--max-time", "8", "--config", "-"])
            .args(["-X", method, "-H", "Content-Type: application/json"])
            .arg(format!("https://localhost:{SUNSHINE_WEB_PORT}{path}"));
        if let Some(b) = body {
            c.args(["-d", b]);
        }
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            c.creation_flags(0x0800_0000);
        }
        use std::io::Write;
        use std::process::Stdio;
        // Curl config quoting keeps the password out of process listings.
        let credential = format!("{}:{}", self.user, self.pass)
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
            .replace('\r', "\\r");
        let mut child = c
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("run curl")?;
        let written = child
            .stdin
            .take()
            .context("curl stdin")
            .and_then(|mut stdin| {
                writeln!(stdin, "user = \"{credential}\"").context("write curl config")
            });
        if written.is_err() {
            let _ = child.kill();
        }
        let out = child.wait_with_output().context("wait for curl")?;
        written?;
        if !out.status.success() {
            bail!("curl: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// True when the saved login is accepted.
    pub fn ok(&self) -> bool {
        self.call("GET", "/api/apps", None).is_ok()
    }

    pub fn session_active(&self) -> bool {
        // /api/apps only lists configured apps; it carries no running state.
        latch_core::http::request(
            ("127.0.0.1", SUNSHINE_PORT),
            "GET",
            "/serverinfo",
            None,
            Duration::from_secs(2),
        )
        .ok()
        .filter(|r| r.status == 200)
        .and_then(|r| session_in_serverinfo(&r.body))
        .unwrap_or(true)
    }

    /// Migrate managed installations once, without interrupting a running app.
    pub fn upgrade_stream_profile(&self) -> Result<bool> {
        if self.session_active() {
            return Ok(false);
        }
        let current = self.call("GET", "/api/config", None)?;
        let desired = stream_profile(&current, &crate::setup::engine_conf())?;
        if desired != current {
            self.call("POST", "/api/config", Some(&desired.to_string()))?;
            // Sunshine closes this HTTP connection as it restarts.
            let _ = self.raw("POST", "/api/restart");
            std::thread::sleep(Duration::from_secs(1));
            let deadline = std::time::Instant::now() + Duration::from_secs(20);
            loop {
                if self.ok() {
                    break;
                }
                if std::time::Instant::now() >= deadline {
                    bail!("streaming engine did not return after applying the display profile");
                }
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        Ok(true)
    }

    /// Accept the PIN the Mac is pairing with. Sunshine says no until the
    /// Mac has actually started pairing, so callers retry.
    ///
    /// Releases up to 2026.5 take `{pin, name}` and answer at once; later
    /// ones list the waiting requests on `GET /api/pin`, want the request's
    /// `pairing_id` too, and from October 2026 keep the post open until the
    /// Mac has finished the handshake. The Mac only goes on with it once
    /// this has answered, so a post still open after [`PIN_TAKEN_AFTER`]
    /// is left to finish on its own and reported as [`Pin::Taken`].
    pub fn submit_pin(&self, pin: &str, name: &str) -> Result<Pin> {
        let pending = pending_pairings(self.call("GET", "/api/pin", None).ok());
        for id in pending {
            let mut body = serde_json::json!({ "pin": pin, "name": name });
            if let Some(id) = id {
                body["pairing_id"] = id.into();
            }
            let (tx, rx) = std::sync::mpsc::channel();
            let (user, pass) = (self.user.to_string(), self.pass.to_string());
            std::thread::spawn(move || {
                let api = Api {
                    user: &user,
                    pass: &pass,
                };
                let _ = tx.send(api.call("POST", "/api/pin", Some(&body.to_string())));
            });
            let v = match rx.recv_timeout(PIN_TAKEN_AFTER) {
                Ok(reply) => reply?,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(Pin::Taken),
                Err(e) => bail!("PIN post: {e}"),
            };
            if v["status"].as_bool() == Some(true) || v["status"].as_str() == Some("true") {
                return Ok(Pin::Paired);
            }
        }
        bail!("the streaming engine did not accept the PIN (is the Mac pairing right now?)")
    }

    /// End whatever is streaming, so a power action does not cut a session
    /// off mid-frame.
    pub fn close_app(&self) {
        let _ = self.call("POST", "/api/apps/close", None);
    }

    /// Names of the paired clients, with the uuid to unpair them by.
    pub fn clients(&self) -> Vec<(String, String)> {
        self.call("GET", "/api/clients/list", None)
            .map(|v| parse_clients(&v))
            .unwrap_or_default()
    }

    pub fn unpair(&self, uuid: &str) -> Result<()> {
        let body = serde_json::json!({ "uuid": uuid }).to_string();
        self.call("POST", "/api/clients/unpair", Some(&body))
            .map(|_| ())
    }

    /// Whether the virtual gamepad driver (ViGEmBus) is installed, if the
    /// running Sunshine can tell us.
    pub fn gamepad_driver(&self) -> Option<bool> {
        let v = self.call("GET", "/api/vigembus/status", None).ok()?;
        v["installed"].as_bool().or_else(|| v["status"].as_bool())
    }

    pub fn install_gamepad_driver(&self) -> Result<()> {
        self.call("POST", "/api/vigembus/install", None).map(|_| ())
    }
}

/// What became of a PIN handed to the engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Pin {
    /// The engine says the Mac is paired.
    Paired,
    /// The engine took the PIN and waits for the Mac to finish the
    /// handshake, which it does once it hears the PIN was entered.
    Taken,
}

/// How long a PIN post may stay open before it counts as taken. An engine
/// that turns a PIN down (or one that never holds the post) answers well
/// within this; one that took it waits for the Mac.
pub const PIN_TAKEN_AFTER: Duration = Duration::from_millis(600);

/// The pairing ids `GET /api/pin` lists on a Sunshine master build. Every
/// other reply (a release Sunshine answers 200 `{"error":"Not Found"}`, an
/// old one 404, curl failing) means "no ids": still send the PIN once
/// without one, which is what those versions want.
fn pending_pairings(reply: Option<serde_json::Value>) -> Vec<Option<String>> {
    let ids: Vec<Option<String>> = reply
        .as_ref()
        .and_then(|v| v["pairings"].as_array())
        .map(|a| {
            a.iter()
                .filter_map(|p| p["id"].as_str().map(|s| Some(s.to_string())))
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        vec![None]
    } else {
        ids
    }
}

/// `current` with the keys of `conf` (this platform's
/// [`crate::setup::engine_conf`]) in force. Unrelated engine settings are
/// kept; metadata from GET is never written.
fn stream_profile(current: &serde_json::Value, conf: &[(&str, &str)]) -> Result<serde_json::Value> {
    let mut config = current
        .as_object()
        .context("engine config is not an object")?
        .clone();
    let changed = conf
        .iter()
        .any(|(k, v)| config.get(*k).and_then(|v| v.as_str()) != Some(*v));
    if !changed {
        return Ok(current.clone());
    }
    for k in ["status", "platform", "version"] {
        config.remove(k);
    }
    for (k, v) in conf {
        config.insert((*k).into(), (*v).into());
    }
    Ok(config.into())
}

/// Sunshine's config carries its own web login and the pairing secrets.
/// Display diagnostics travel to the Mac, so those keys never leave the PC.
fn is_secret(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    [
        "pass", "user", "salt", "token", "key", "cert", "secret", "pin",
    ]
    .iter()
    .any(|needle| k.contains(needle))
}

/// The last `lines` lines: a capture failure is at the end of the log.
fn tail(text: &str, lines: usize) -> String {
    let all: Vec<&str> = text.lines().collect();
    all[all.len().saturating_sub(lines)..].join("\n")
}

fn parse_reply(text: &str) -> Result<serde_json::Value> {
    let v: serde_json::Value = serde_json::from_str(text.trim()).with_context(|| {
        if text.trim().is_empty() {
            "empty reply (wrong login?)".to_string()
        } else {
            format!(
                "unexpected reply: {}",
                text.trim().chars().take(120).collect::<String>()
            )
        }
    })?;
    if v.get("status").and_then(|s| s.as_bool()) == Some(false) {
        if let Some(e) = v.get("error").and_then(|e| e.as_str()) {
            bail!("{e}");
        }
    }
    Ok(v)
}

/// Whether the GameStream `/serverinfo` XML says an app is running: its
/// `currentgame` is the running app's id, 0 when nothing streams. `None`
/// when the reply does not say.
pub(crate) fn session_in_serverinfo(xml: &str) -> Option<bool> {
    xml.split_once("<currentgame>")?
        .1
        .split_once("</currentgame>")?
        .0
        .trim()
        .parse::<u32>()
        .ok()
        .map(|id| id != 0)
}

fn parse_clients(v: &serde_json::Value) -> Vec<(String, String)> {
    v.get("named_certs")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|c| {
                    Some((
                        c.get("name")?.as_str()?.to_string(),
                        c.get("uuid")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn streaming_profile_replaces_caps_and_fixed_modes_without_losing_other_settings() {
        let original = serde_json::json!({"status":true,"version":"x", "platform":"windows",
            "encoder":"amdvce", "max_bitrate":"1000", "dd_resolution_option":"manual", "output_name":"virtual"});
        let profile = stream_profile(&original, crate::setup::ENGINE_CONF).unwrap();
        assert_eq!(profile["max_bitrate"], "0");
        assert_eq!(profile["amd_rc"], "cbr");
        assert_eq!(profile["amd_quality"], "speed");
        assert_eq!(profile["sw_preset"], "ultrafast");
        assert_eq!(profile["min_threads"], "4");
        assert_eq!(profile["fec_percentage"], "20");
        assert_eq!(profile["packetsize"], "1184");
        assert_eq!(profile["dd_resolution_option"], "auto");
        assert_eq!(profile["dd_refresh_rate_option"], "auto");
        assert_eq!(profile["encoder"], "amdvce");
        assert_eq!(profile["output_name"], "virtual");
        assert!(profile.get("platform").is_none());
        assert_eq!(
            stream_profile(&profile, crate::setup::ENGINE_CONF).unwrap(),
            profile
        );
        assert!(stream_profile(&serde_json::Value::Null, crate::setup::ENGINE_CONF).is_err());
    }

    #[test]
    fn a_linux_engine_drops_the_frame_rate_floor_and_reapplies_it_elsewhere() {
        // The node kit's VPS profile: a 60 fps floor under every stream.
        let vps = serde_json::json!({"status":true, "capture":"x11", "encoder":"software",
            "minimum_fps_target":"60", "sw_preset":"ultrafast", "min_threads":"2"});
        let linux = crate::setup::engine_conf_for(true);
        let profile = stream_profile(&vps, &linux).unwrap();
        assert_eq!(profile["minimum_fps_target"], "0");
        assert_eq!(profile["sw_preset"], "superfast");
        assert_eq!(profile["capture"], "x11", "the deployment's own keys stay");
        assert_eq!(profile["encoder"], "software");
        assert_eq!(stream_profile(&profile, &linux).unwrap(), profile);
        let other = crate::setup::engine_conf_for(false);
        let elsewhere = stream_profile(&profile, &other).unwrap();
        assert_eq!(elsewhere["minimum_fps_target"], "60");
        assert_eq!(elsewhere["sw_preset"], "ultrafast");
    }

    #[test]
    fn discovery_prefers_the_latch_engine_over_sunshine() {
        assert_eq!(INSTALL_DIRS[0], ("Latch", ENGINE_DIR));
        assert_eq!(
            INSTALL_DIRS[1],
            ("Latch", crate::legacy::WINDOWS_ENGINE_DIR)
        );
        assert_eq!(INSTALL_DIRS[2].0, "Sunshine");
        assert_eq!(INSTALL_DIRS[3].0, "Apollo");
        let existing = [ENGINE_DIR, r"C:\Program Files\Sunshine"];
        let winner = INSTALL_DIRS
            .iter()
            .find(|(_, d)| existing.contains(d))
            .expect("both exist");
        assert_eq!(*winner, ("Latch", ENGINE_DIR));
        let _ = SUNSHINE_PORT;
    }

    #[test]
    fn an_engine_in_the_old_folder_is_recognised_so_setup_moves_it() {
        let old = Install {
            kind: "Latch",
            dir: PathBuf::from(crate::legacy::WINDOWS_ENGINE_DIR),
        };
        let new = Install {
            kind: "Latch",
            dir: PathBuf::from(ENGINE_DIR),
        };
        assert!(in_old_folder(&old));
        assert!(!in_old_folder(&new));
    }

    #[test]
    fn only_software_encoding_on_a_small_cpu_caps_the_frame_rate() {
        assert_eq!(max_fps("software", 2), 30);
        assert_eq!(max_fps("software", 1), 30);
        assert_eq!(max_fps("software", 4), 0);
        assert_eq!(max_fps("nvenc", 2), 0);
        assert_eq!(max_fps("", 2), 0, "unknown encoder: no limit yet");
    }

    #[test]
    fn serverinfo_reports_a_running_session_and_unknown_state_stays_unknown() {
        assert_eq!(
            session_in_serverinfo("<root><currentgame>0</currentgame></root>"),
            Some(false)
        );
        assert_eq!(
            session_in_serverinfo("<root><currentgame>881448767</currentgame></root>"),
            Some(true)
        );
        assert_eq!(
            session_in_serverinfo("<root><currentgame>bad</currentgame></root>"),
            None
        );
        assert_eq!(session_in_serverinfo("<root/>"), None);
    }

    #[test]
    fn the_encoder_family_is_read_from_the_log() {
        let log = "[2026-09-07 10:00:00.001]: Info: // Testing for available encoders //\n\
                   [2026-09-07 10:00:01.002]: Info: Found H.264 encoder: h264_nvenc [nvenc]\n\
                   [2026-09-07 10:00:01.003]: Info: Found HEVC encoder: hevc_nvenc [nvenc]\n\
                   [2026-09-07 10:00:01.004]: Info: Found AV1 encoder: av1_nvenc [nvenc]\n";
        assert_eq!(encoder_in(log).as_deref(), Some("nvenc"));
        let sw = "Info: Found H.264 encoder: libx264 [software]\nInfo: Found HEVC encoder: libx265 [software]\n";
        assert_eq!(encoder_in(sw).as_deref(), Some("software"));
        // Two starts: the later one counts.
        let two = format!("{log}{sw}");
        assert_eq!(encoder_in(&two).as_deref(), Some("software"));
        let hevc_only = "Info: Found HEVC encoder: hevc_amf [amf]\n";
        assert_eq!(encoder_in(hevc_only).as_deref(), Some("amf"));
        assert_eq!(encoder_in("Info: nothing about encoders\n"), None);
        assert_eq!(encoder_in("Found H.264 encoder: x []"), None);
        assert_eq!(
            read_tail(std::path::Path::new("/nonexistent/sunshine.log"), 10),
            None
        );
    }

    #[test]
    fn a_missing_audio_device_is_read_from_the_log() {
        let bad = "[2026-09-08 20:00:00.000]: Info: Found H.264 encoder: h264_nvenc [nvenc]\n\
                   [2026-09-08 20:00:05.000]: Error: Couldn't get default audio endpoint [0x80070490]\n\
                   [2026-09-08 20:00:05.001]: Error: Unable to initialize audio capture. The stream will not have audio.\n";
        assert_eq!(
            audio_problem_in(bad).as_deref(),
            Some("Unable to initialize audio capture. The stream will not have audio.")
        );
        // A later start that captured fine clears it.
        let good = format!(
            "{bad}[2026-09-08 21:00:00.000]: Info: Audio capture format is [48kHz, 32-bit float, 2 channels]\n"
        );
        assert_eq!(audio_problem_in(&good), None);
        assert_eq!(audio_problem_in("Info: nothing about audio\n"), None);
        let sink = "[x]: Warning: Audio sink not found: Steam Streaming Speakers\n";
        assert_eq!(
            audio_problem_in(sink).as_deref(),
            Some("Audio sink not found: Steam Streaming Speakers")
        );
        let probe = "[2026-09-20 07:03:42.108]: Error: Couldn't get default audio endpoint [0x80070490]\n\
                     [2026-09-20 07:03:43.263]: Info: // Ignore any errors mentioned above, they are not relevant. //\n\
                     [2026-09-20 07:03:43.264]: Info: Found H.264 encoder: h264_amf [amdvce]\n";
        assert_eq!(
            audio_problem_in(probe),
            None,
            "startup probe failures are not a missing sound device"
        );
        // Hermes: probe error, then ignore, and no later capture failure.
        let hermes = "[2026-09-20 07:03:42.108]: Error: Couldn't get default audio endpoint [0x80070490]\n\
                      [2026-09-20 07:03:43.263]: Info: // Ignore any errors mentioned above, they are not relevant. //\n\
                      [2026-09-20 07:03:43.264]: Info: Found H.264 encoder: h264_amf [amdvce]\n\
                      [2026-09-20 07:03:43.400]: Info: Found HEVC encoder: hevc_amf [amdvce]\n";
        assert_eq!(audio_problem_in(hermes), None);
        assert_eq!(
            log_message("  Couldn't capture audio [0x1]  "),
            "Couldn't capture audio [0x1]"
        );
    }

    #[test]
    fn replies_are_parsed_and_errors_surfaced() {
        assert!(parse_reply(r#"{"status":true}"#).is_ok());
        let e = parse_reply(r#"{"status":false,"error":"Invalid PIN"}"#).unwrap_err();
        assert!(e.to_string().contains("Invalid PIN"));
        let e = parse_reply("").unwrap_err();
        assert!(e.to_string().contains("wrong login"), "{e}");
        assert!(parse_reply("<html>401</html>").is_err());
    }

    #[test]
    fn pin_is_sent_once_without_an_id_unless_sunshine_lists_some() {
        // Release Sunshine: 200 with a "Not Found" body, no pairings key.
        let not_found = serde_json::json!({"error": "Not Found", "status_code": 404});
        assert_eq!(pending_pairings(Some(not_found)), vec![None]);
        assert_eq!(pending_pairings(None), vec![None]);
        assert_eq!(
            pending_pairings(Some(serde_json::json!({"pairings": []}))),
            vec![None]
        );
        let master = serde_json::json!({"pairings": [{"id": "a", "name": "mac"}, {"id": "b"}]});
        assert_eq!(
            pending_pairings(Some(master)),
            vec![Some("a".to_string()), Some("b".to_string())]
        );
    }

    #[test]
    fn diagnostics_keep_the_capture_settings_and_drop_the_login() {
        assert!(is_secret("username"));
        assert!(is_secret("password"));
        assert!(is_secret("origin_web_ui_allowed_pass"));
        assert!(is_secret("salt"));
        assert!(is_secret("pkey"));
        assert!(is_secret("cert"));
        for keep in [
            "output_name",
            "adapter_name",
            "capture",
            "encoder",
            "hevc_mode",
            "dd_configuration_option",
            "resolutions",
        ] {
            assert!(!is_secret(keep), "{keep} is what the diagnosis needs");
        }
    }

    #[test]
    fn the_log_tail_is_the_end_of_the_log() {
        let log = (1..=200)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        let cut = tail(&log, 150);
        assert!(cut.starts_with("51\n52\n"), "{}", &cut[..8]);
        assert!(cut.ends_with("\n200"));
        assert_eq!(cut.lines().count(), 150);
        assert_eq!(tail("one\ntwo", 150), "one\ntwo");
        assert_eq!(tail("", 150), "");
    }

    #[test]
    fn clients_list_is_read_leniently() {
        let v: serde_json::Value = serde_json::from_str(
            r#"{"status":true,"named_certs":[{"name":"Example Mac","uuid":"abc"},{"uuid":"nameless"},{"name":"x","uuid":"y","extra":1}]}"#,
        )
        .unwrap();
        assert_eq!(
            parse_clients(&v),
            vec![
                ("Example Mac".to_string(), "abc".to_string()),
                ("x".into(), "y".into())
            ]
        );
        assert!(parse_clients(&serde_json::json!({"status": true})).is_empty());
    }
}
