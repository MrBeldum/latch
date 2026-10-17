//! Sunshine (or Apollo, its fork) on this PC: where it is, whether it is up,
//! and the two web-API calls Latch needs. The API is HTTPS with a
//! self-signed certificate on localhost, so calls go through `curl.exe -k`
//! rather than a TLS stack of our own.

use anyhow::{bail, Context, Result};
use latch_core::{SUNSHINE_PORT, SUNSHINE_WEB_PORT};
use std::net::{IpAddr, SocketAddr, TcpStream};
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
        // -S keeps curl's own error message under -s: without it every
        // failure read "curl: ", whatever went wrong.
        c.args(["-sSk", "--max-time", "8", "--config", "-"])
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
            bail!("{}", curl_failure(out.status.code(), &out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// True when the saved login is accepted.
    pub fn ok(&self) -> bool {
        self.call("GET", "/api/apps", None).is_ok()
    }

    /// Migrate managed installations once, without interrupting a viewer.
    /// Applying the profile restarts the engine, which ends whatever app is
    /// launched; one nobody watches (the Desktop a viewer left to resume) is
    /// closed first, so its undo commands run, and the next viewer starts
    /// it again.
    pub fn upgrade_stream_profile(&self, install: Option<&Install>) -> Result<bool> {
        let current = self.call("GET", "/api/config", None)?;
        let desired = stream_profile(&current, &crate::setup::engine_conf())?;
        if desired == current {
            return Ok(true);
        }
        let s = session(install);
        if s.maybe_watched() {
            return Ok(false);
        }
        if s.app_running == Some(true) {
            self.close_app();
        }
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
    ///
    /// The PIN goes to the request that came from `peer` (or, where every
    /// address looks local, from a machine called `name`), never simply to
    /// the first one listed: that may be another device's, or an abandoned
    /// one, and handing it this PIN fails both.
    pub fn submit_pin(&self, pin: &str, name: &str, peer: IpAddr) -> Result<Pin> {
        let listed = self.call("GET", "/api/pin", None).ok();
        let id = match choose_pairing(listed.as_ref(), peer, name) {
            Pairing::Unlisted => None,
            Pairing::Id(id) => Some(id),
            Pairing::NotYet => return Ok(Pin::NotYet),
        };
        let mut body = serde_json::json!({ "pin": pin, "name": name });
        if let Some(id) = &id {
            body["pairing_id"] = id.as_str().into();
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
        // Only an engine with pairing ids holds the post open; one without
        // answers, however slowly, and curl's own time limit bounds that.
        let reply = if id.is_some() {
            match rx.recv_timeout(PIN_TAKEN_AFTER) {
                Ok(reply) => reply,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => return Ok(Pin::Taken),
                Err(e) => bail!("PIN post: {e}"),
            }
        } else {
            rx.recv().context("PIN post")?
        };
        let v = reply?;
        if v["status"].as_bool() == Some(true) || v["status"].as_str() == Some("true") {
            return Ok(Pin::Paired);
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
    /// The engine lists the requests waiting for a PIN, and none is this
    /// Mac's yet: it asks a moment after it starts pairing, and a slow path
    /// can bring the PIN first. Nothing was sent; the Mac tries again.
    NotYet,
}

/// How long a PIN post may stay open before it counts as taken. An engine
/// that turns a PIN down (or one that never holds the post) answers well
/// within this; one that took it waits for the Mac.
pub const PIN_TAKEN_AFTER: Duration = Duration::from_millis(600);

/// Which of the engine's waiting pairing requests a PIN is for.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Pairing {
    /// The engine keeps no list (a release Sunshine answers 200
    /// `{"error":"Not Found"}`, an old one 404, or curl failed): send the
    /// PIN once without an id, which is what those versions want.
    Unlisted,
    /// The request with this id.
    Id(String),
    /// The engine lists requests, but none is this machine's: an empty
    /// list included, since 2026.10 turns down a PIN without an id.
    NotYet,
}

/// Pick the request `GET /api/pin` lists for the machine at `peer` called
/// `name`. Sunshine lists each as `{id, name, address}`, oldest first, and
/// keeps one for five minutes after its device gave up. The address decides
/// when there is one to go by; a local peer (the window, or every peer of a
/// Docker node, which reaches the engine through loopback too) falls back
/// to the name. A request from another address or under another name is
/// never picked: this machine's own may simply not have arrived yet.
fn choose_pairing(reply: Option<&serde_json::Value>, peer: IpAddr, name: &str) -> Pairing {
    let Some(list) = reply.and_then(|v| v["pairings"].as_array()) else {
        return Pairing::Unlisted;
    };
    struct Entry<'a> {
        id: &'a str,
        name: Option<&'a str>,
        address: Option<IpAddr>,
    }
    let entries: Vec<Entry> = list
        .iter()
        .filter_map(|p| {
            Some(Entry {
                id: p["id"].as_str().filter(|id| !id.is_empty())?,
                name: p["name"].as_str().filter(|n| !n.is_empty()),
                address: p["address"].as_str().and_then(|a| {
                    a.trim_start_matches('[')
                        .trim_end_matches(']')
                        .parse::<IpAddr>()
                        .ok()
                        .map(|ip| ip.to_canonical())
                }),
            })
        })
        .collect();
    let peer = peer.to_canonical();
    let one = |found: Vec<&Entry>| match found.as_slice() {
        [only] => Some(Pairing::Id(only.id.to_string())),
        _ => None,
    };
    let named = |e: &&Entry| e.name == Some(name);
    if !peer.is_loopback() {
        let from_peer: Vec<&Entry> = entries.iter().filter(|e| e.address == Some(peer)).collect();
        if from_peer.len() > 1 {
            return one(from_peer.into_iter().filter(named).collect()).unwrap_or(Pairing::NotYet);
        }
        if let Some(found) = one(from_peer) {
            return found;
        }
    }
    // By name, among the requests that do not plainly come from elsewhere.
    let local = |e: &&Entry| e.address.is_none_or(|a| a.is_loopback() || a == peer);
    if let Some(found) = one(entries.iter().filter(local).filter(named).collect()) {
        return found;
    }
    // A build that lists bare ids: the only one there is.
    match entries.as_slice() {
        [only] if only.name.is_none() && only.address.is_none() => Pairing::Id(only.id.to_string()),
        _ => Pairing::NotYet,
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

/// What a failed curl run says: its own message, which names its exit code
/// ("curl: (7) Failed to connect…", "curl: (28) Operation timed out…"), or
/// the code alone when it printed nothing.
fn curl_failure(code: Option<i32>, stderr: &[u8]) -> String {
    let msg = String::from_utf8_lossy(stderr);
    let msg = msg.trim();
    match code {
        _ if !msg.is_empty() => msg.to_string(),
        Some(code) => format!("curl exited with code {code}"),
        None => "curl was stopped".into(),
    }
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

/// What the engine says about a stream right now. An app being launched is
/// not someone watching: a viewer that disconnects leaves its Desktop
/// running, to be resumed, until the engine restarts or something closes
/// it (Sunshine treats a Desktop with no command as a placebo that never
/// ends by itself). Only the engine's log says whether a viewer is there.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Session {
    /// An app is launched (`currentgame` in `/serverinfo`); `None` when the
    /// engine did not answer.
    pub app_running: Option<bool>,
    /// The last line of the engine's log that starts or ends a session is
    /// a viewer connecting; `None` when the log has neither.
    pub viewer: Option<bool>,
}

impl Session {
    /// Someone may be watching, counting what is unknown as yes: what
    /// restarting the engine (which ends any stream) has to wait for.
    pub fn maybe_watched(&self) -> bool {
        self.app_running.unwrap_or(true) && self.viewer.unwrap_or(true)
    }

    /// The log says a viewer is connected and the engine does not say
    /// nothing runs. Swapping Latch's own executable never touches the
    /// engine, so only this defers an update; an unknown never blocks one
    /// forever.
    pub fn watched(&self) -> bool {
        self.viewer == Some(true) && self.app_running != Some(false)
    }
}

/// Ask the engine whether an app runs, and read its log for a viewer.
pub fn session(install: Option<&Install>) -> Session {
    let app_running = latch_core::http::request(
        ("127.0.0.1", SUNSHINE_PORT),
        "GET",
        "/serverinfo",
        None,
        Duration::from_secs(2),
    )
    .ok()
    .filter(|r| r.status == 200)
    .and_then(|r| session_in_serverinfo(&r.body));
    let viewer = install
        .and_then(log_text)
        .as_deref()
        .and_then(viewer_connected_in);
    Session {
        app_running,
        viewer,
    }
}

/// Whether a viewer is connected, from the engine's log: the last line that
/// starts a session (`CLIENT CONNECTED`) or ends one decides. A session ends
/// with `CLIENT DISCONNECTED` when the viewer quits, but only with `<ip>:
/// Ping Timeout` when it vanished (a Mac that slept, Wi-Fi that dropped).
/// `None` when the log holds neither, as after the engine restarted (it
/// rewrites its log at every start).
pub(crate) fn viewer_connected_in(log: &str) -> Option<bool> {
    // "Initial Ping Timeout" (a viewer that never got going) ends one too.
    const ENDED: [&str; 2] = ["CLIENT DISCONNECTED", "Ping Timeout"];
    log.lines().rev().find_map(|line| {
        if ENDED.iter().any(|k| line.contains(k)) {
            Some(false)
        } else if line.contains("CLIENT CONNECTED") {
            Some(true)
        } else {
            None
        }
    })
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

    /// stuzha's own log, 2026-10-17: two sessions that ended with a ping
    /// timeout (the Mac slept) and none with a disconnect line, while
    /// `/serverinfo` still said Desktop was running.
    const STUZHA_LOG: &str = "\
[2026-10-17 16:32:42.610]: Info: Executing Undo Cmd: [/usr/local/bin/stuzha-display 1920 1080]
[2026-10-17 16:32:44.410]: Info: Found H.264 encoder: libx264 [software]
[2026-10-17 16:32:44.916]: Info: Executing [Desktop]
[2026-10-17 16:32:45.085]: Info: CLIENT CONNECTED
[2026-10-17 16:33:08.397]: Info: 100.111.100.57: Ping Timeout
[2026-10-17 16:36:36.882]: Info: Found H.264 encoder: libx264 [software]
[2026-10-17 16:36:37.224]: Info: CLIENT CONNECTED
[2026-10-17 16:37:28.162]: Info: 100.111.100.57: Ping Timeout
";

    #[test]
    fn a_viewer_is_connected_only_until_its_session_ends() {
        let connected = "[2026-10-14 19:00:00.000]: Info: CLIENT CONNECTED\n";
        let quit = format!("{connected}[2026-10-14 19:21:54.306]: Info: CLIENT DISCONNECTED\n");
        assert_eq!(viewer_connected_in(connected), Some(true));
        assert_eq!(viewer_connected_in(&quit), Some(false));
        assert_eq!(
            viewer_connected_in(&format!("{quit}{connected}")),
            Some(true),
            "a later session counts"
        );
        assert_eq!(viewer_connected_in(STUZHA_LOG), Some(false));
        assert_eq!(
            viewer_connected_in("[x]: Info: 100.64.0.5:51234: Initial Ping Timeout\n"),
            Some(false)
        );
        // A fresh log (the engine restarted) says nothing either way.
        assert_eq!(
            viewer_connected_in("[x]: Info: Executing [Desktop]\n"),
            None
        );
        assert_eq!(viewer_connected_in(""), None);
    }

    #[test]
    fn a_desktop_left_to_resume_is_nobody_watching() {
        let s = |app_running, viewer| Session {
            app_running,
            viewer,
        };
        // stuzha after the Mac slept: Desktop still launched, nobody there.
        let resumable = s(Some(true), viewer_connected_in(STUZHA_LOG));
        assert!(!resumable.watched());
        assert!(!resumable.maybe_watched());
        assert!(!crate::update::refuse_self_update(
            true,
            resumable.watched()
        ));

        let live = s(Some(true), Some(true));
        assert!(live.watched() && live.maybe_watched());
        assert!(crate::update::refuse_self_update(true, live.watched()));
        // The engine did not answer, but its log has a viewer: watched.
        assert!(s(None, Some(true)).watched());
        // Nothing launched: whatever the log says, nobody is watching.
        assert!(!s(Some(false), Some(true)).watched());
        assert!(!s(Some(false), None).maybe_watched());
        // Unknowns never block an update, which leaves the engine alone...
        assert!(!s(None, None).watched());
        assert!(!s(Some(true), None).watched());
        // ...but do hold back an engine restart, which would end a stream.
        assert!(s(None, None).maybe_watched());
        assert!(s(Some(true), None).maybe_watched());
        assert!(!s(None, Some(false)).maybe_watched());
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
    fn a_failed_engine_call_says_why() {
        assert_eq!(
            curl_failure(
                Some(7),
                b"curl: (7) Failed to connect to localhost port 47990: Connection refused\n"
            ),
            "curl: (7) Failed to connect to localhost port 47990: Connection refused"
        );
        assert_eq!(curl_failure(Some(28), b""), "curl exited with code 28");
        assert_eq!(curl_failure(None, b"  "), "curl was stopped");
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
    fn the_pin_goes_to_the_requesters_own_pairing() {
        let mac: IpAddr = "100.100.1.2".parse().unwrap();
        let name = "Daniel\u{2019}s MacBook Pro";
        let pick =
            |reply: serde_json::Value, peer: IpAddr| choose_pairing(Some(&reply), peer, name);
        // Release Sunshine: 200 with a "Not Found" body, no pairings key.
        let not_found = serde_json::json!({"error": "Not Found", "status_code": 404});
        assert_eq!(pick(not_found, mac), Pairing::Unlisted);
        assert_eq!(choose_pairing(None, mac, name), Pairing::Unlisted);
        // 2026.10 lists none yet: posting without an id would be refused.
        assert_eq!(
            pick(serde_json::json!({"pairings": []}), mac),
            Pairing::NotYet
        );

        let ipad = serde_json::json!({"id": "a", "name": "iPad", "address": "100.64.0.5"});
        let ours = serde_json::json!({"id": "b", "name": name, "address": "100.100.1.2"});
        // Another device's request is listed first (oldest first).
        assert_eq!(
            pick(serde_json::json!({"pairings": [ipad, ours]}), mac),
            Pairing::Id("b".into())
        );
        // Only someone else's is there: ours has not arrived yet.
        assert_eq!(
            pick(serde_json::json!({"pairings": [ipad]}), mac),
            Pairing::NotYet
        );
        // An IPv4-mapped address is the same machine.
        let mapped = serde_json::json!({"id": "c", "name": "x", "address": "::ffff:100.100.1.2"});
        assert_eq!(
            pick(serde_json::json!({"pairings": [ipad, mapped]}), mac),
            Pairing::Id("c".into())
        );
        // Two from one address: the name tells them apart, or nothing does.
        let moonlight =
            serde_json::json!({"id": "d", "name": "Moonlight", "address": "100.100.1.2"});
        assert_eq!(
            pick(serde_json::json!({"pairings": [moonlight, ours]}), mac),
            Pairing::Id("b".into())
        );
        assert_eq!(
            pick(serde_json::json!({"pairings": [moonlight, moonlight]}), mac),
            Pairing::NotYet
        );

        // A Docker node: every peer, and every request, is loopback.
        let local: IpAddr = "127.0.0.1".parse().unwrap();
        let node =
            |id: &str, n: &str| serde_json::json!({"id": id, "name": n, "address": "127.0.0.1"});
        assert_eq!(
            pick(
                serde_json::json!({"pairings": [node("a", "iPad"), node("b", name)]}),
                local
            ),
            Pairing::Id("b".into())
        );
        assert_eq!(
            pick(
                serde_json::json!({"pairings": [node("a", name), node("b", name)]}),
                local
            ),
            Pairing::NotYet
        );
        // The same name from another address is someone else.
        let twin = serde_json::json!({"id": "e", "name": name, "address": "100.64.0.9"});
        assert_eq!(
            pick(serde_json::json!({"pairings": [twin]}), mac),
            Pairing::NotYet
        );

        // Builds that list bare ids: the only one, never a guess among two.
        assert_eq!(
            pick(serde_json::json!({"pairings": [{"id": "f"}]}), mac),
            Pairing::Id("f".into())
        );
        assert_eq!(
            pick(
                serde_json::json!({"pairings": [{"id": "f"}, {"id": "g"}]}),
                mac
            ),
            Pairing::NotYet
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
