//! Taking a new `latch-host.exe` from a Mac on the tailnet and swapping it
//! in.
//!
//! The Mac does the fetching (it has the GitHub login; the PC has none) and
//! POSTs the executable to `/v1/update` with its version and SHA-256. The
//! service checks the digest, that the bytes are a Windows executable and a
//! newer version, and stages the file beside itself. Then it renames:
//! Windows lets a running executable be renamed but not overwritten. The new
//! exe is started with `--replaces <pid>`, which makes it wait for the old
//! service to let go of the port, and the old one exits after answering.
//! A new exe that cannot start, or exits at once, is moved aside and the
//! old one put back, still serving.
//!
//! One that starts but then never serves (it panics on its first tick, or
//! hangs) is caught by the old executable itself: the old service starts it
//! from its retired file with `--probation` before it exits, and that
//! watcher puts the old version back unless the new one answers with its
//! version within [`PROBATION`]. The retired file stays until the new
//! service has run for a while (see [`tidy`]).

use anyhow::{Context, Result};
use latch_core::api::{UPDATE_MAX_BYTES, UPDATE_SHA256_HEADER, UPDATE_VERSION_HEADER};
use latch_core::http::Request;
use latch_core::legacy;
use latch_core::update;
use semver::Version;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Child;
use std::time::{Duration, Instant};

/// Anything smaller is not the host.
const MIN_SIZE: usize = 1024 * 1024;
/// How long a new copy must stay up before the old one lets go. It waits
/// for the old one's port, so it is still running after this unless it
/// could not start at all.
pub const STARTUP_GRACE: Duration = Duration::from_secs(2);
/// How long a new service has, once the old one has let go, to answer on
/// the control port with its version and keep answering for
/// [`HEALTHY_FOR`]. Otherwise the previous executable is put back.
pub const PROBATION: Duration = Duration::from_secs(60);
const HEALTHY_FOR: Duration = Duration::from_secs(15);

/// Why an upload was not staged, with the HTTP status it earns.
#[derive(Debug, PartialEq, Eq)]
pub enum Rejected {
    /// The PC already runs this version or a newer one.
    NotNewer(String),
    /// Headers or bytes are wrong, or the file could not be written.
    Bad(String),
}

impl Rejected {
    pub fn status(&self) -> u16 {
        match self {
            Rejected::NotNewer(_) => 409,
            Rejected::Bad(_) => 400,
        }
    }
    pub fn message(&self) -> &str {
        match self {
            Rejected::NotNewer(m) | Rejected::Bad(m) => m,
        }
    }
}

/// Where the upload waits: `latch-host.exe.new`.
pub fn staged(exe: &Path) -> PathBuf {
    with_suffix(exe, ".new")
}

/// Where the running executable goes during the swap: `latch-host.exe.old`.
pub fn retired(exe: &Path) -> PathBuf {
    with_suffix(exe, ".old")
}

fn with_suffix(exe: &Path, suffix: &str) -> PathBuf {
    let mut name = exe.file_name().unwrap_or_default().to_os_string();
    name.push(suffix);
    exe.with_file_name(name)
}

/// Whether to put an update off: only while a viewer is actually connected.
/// A Desktop left launched for a viewer to resume is no reason; refusing
/// then kept a PC from updating until it next restarted.
pub fn refuse_self_update(engine_running: bool, someone_watching: bool) -> bool {
    engine_running && someone_watching
}

/// Check the upload and stage it beside `exe`. Returns the version it
/// carries.
pub fn stage(req: &Request, exe: &Path) -> Result<Version, Rejected> {
    let version = req
        .header(UPDATE_VERSION_HEADER)
        .or_else(|| req.header(legacy::UPDATE_VERSION_HEADER))
        .ok_or_else(|| Rejected::Bad(format!("missing {UPDATE_VERSION_HEADER}")))?;
    let version = Version::parse(version.trim())
        .map_err(|_| Rejected::Bad(format!("{version:?} is not a version")))?;
    let current = update::current();
    if version <= current {
        return Err(Rejected::NotNewer(format!(
            "this PC already runs Latch Host {current}"
        )));
    }
    let want = req
        .header(UPDATE_SHA256_HEADER)
        .or_else(|| req.header(legacy::UPDATE_SHA256_HEADER))
        .ok_or_else(|| Rejected::Bad(format!("missing {UPDATE_SHA256_HEADER}")))?;
    if req.body.len() < MIN_SIZE
        || req.body.len() > UPDATE_MAX_BYTES
        || !is_host_executable(&req.body)
    {
        return Err(Rejected::Bad(
            "the upload is not a Windows executable".into(),
        ));
    }
    let got = update::sha256_hex(&req.body);
    if !got.eq_ignore_ascii_case(want.trim()) {
        return Err(Rejected::Bad(
            "the upload does not match the SHA-256 it came with".into(),
        ));
    }
    let path = staged(exe);
    std::fs::write(&path, &req.body)
        .map_err(|e| Rejected::Bad(format!("could not write {}: {e}", path.display())))?;
    Ok(version)
}

/// Validate the DOS/PE headers and executable architecture before touching
/// the running host. A DOS `MZ` prefix alone also accepts corrupt files,
/// 32-bit programs and DLLs, none of which can replace the x64 service.
fn is_host_executable(bytes: &[u8]) -> bool {
    if bytes.get(..2) != Some(b"MZ") {
        return false;
    }
    let Some(offset) = bytes.get(0x3c..0x40) else {
        return false;
    };
    let offset = u32::from_le_bytes(offset.try_into().unwrap()) as usize;
    if offset < 0x40 {
        return false;
    }
    let Some(header) = bytes.get(offset..).and_then(|b| b.get(..26)) else {
        return false;
    };
    let sections = u16::from_le_bytes([header[6], header[7]]) as usize;
    let optional_size = u16::from_le_bytes([header[20], header[21]]) as usize;
    let flags = u16::from_le_bytes([header[22], header[23]]);
    header[..4] == *b"PE\0\0"
        && header[4..6] == [0x64, 0x86] // IMAGE_FILE_MACHINE_AMD64
        && (1..=96).contains(&sections)
        && optional_size >= 112
        && flags & 0x0002 != 0 // IMAGE_FILE_EXECUTABLE_IMAGE
        && flags & 0x2000 == 0 // IMAGE_FILE_DLL
        && header[24..26] == [0x0b, 0x02] // PE32+
        && bytes.get(offset..).is_some_and(|b| b.len() >= 24 + optional_size + sections * 40)
}

/// A new service started, and where the one it replaces now is.
#[derive(Debug)]
pub struct Handover {
    /// The new service's process.
    pub pid: u32,
    /// The executable that was running, renamed out of the way.
    pub retired: PathBuf,
}

/// Swap the staged executable in and start it. The caller exits afterwards;
/// the new process waits for that before it takes the port.
pub fn apply(exe: &Path) -> Result<Handover> {
    apply_within(exe, STARTUP_GRACE)
}

/// Start `exe` as the service that takes over from this one: `--replaces`
/// makes it wait for this process to let go of the port. Fails if it has
/// already exited after `grace` (a crash, a missing library). Returns its
/// process id.
pub fn hand_over(exe: &Path, grace: Duration) -> Result<u32> {
    let mut child = start_service(exe, &["--replaces", &std::process::id().to_string()])
        .context("start the new service")?;
    std::thread::sleep(grace);
    if let Ok(Some(status)) = child.try_wait() {
        anyhow::bail!("the new service exited at once ({status})");
    }
    Ok(child.id())
}

/// `exe --background <args>`, detached from this process.
fn start_service(exe: &Path, args: &[&str]) -> std::io::Result<Child> {
    let mut c = std::process::Command::new(exe);
    c.arg("--background")
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008 | 0x0800_0000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }
    c.spawn()
}

fn apply_within(exe: &Path, grace: Duration) -> Result<Handover> {
    let new = staged(exe);
    anyhow::ensure!(new.exists(), "nothing is staged at {}", new.display());
    let old = {
        let p = retired(exe);
        let _ = std::fs::remove_file(&p);
        if p.exists() {
            // A leftover panel from a previous update still has `.old`
            // mapped; retire beside it instead of failing the swap.
            with_suffix(exe, &format!(".old-{}", std::process::id()))
        } else {
            p
        }
    };
    rename_retry(exe, &old).context("retire the running executable")?;
    if let Err(e) = rename_retry(&new, exe) {
        rename_retry(&old, exe).context("restore the previous executable after a failed swap")?;
        return Err(e).context("move the new executable in");
    }
    // Keep the old service installed if the replacement cannot run: this
    // process is still serving requests, and a PC nobody can reach in
    // person must not be left with a service that never starts.
    // One that has already exited could not start (a crash, a missing
    // library): it would leave the PC with no service after this one quits.
    match hand_over(exe, grace) {
        Ok(pid) => Ok(Handover { pid, retired: old }),
        Err(why) => {
            rename_retry(exe, &new).context("move the failed replacement aside")?;
            rename_retry(&old, exe).context("restore the previous executable")?;
            Err(why.context("the previous executable is back in place"))
        }
    }
}

/// Start the watcher for the new service from the executable it retired,
/// which is what it puts back if it has to. Call before exiting.
pub fn start_probation(exe: &Path, h: &Handover, version: &str) -> Result<()> {
    let exe = exe.display().to_string();
    start_service(
        &h.retired,
        &[
            "--probation",
            &h.pid.to_string(),
            "--probation-version",
            version,
            "--probation-exe",
            &exe,
        ],
    )
    .map(drop)
    .context("start the update watcher")
}

/// The watcher (`--background --probation <pid>`), run from the retired
/// executable: wait for the new service at `exe` to answer with `version`
/// and keep answering; if it does not, stop it, put this executable back at
/// `exe` and start it again. A PC nobody can reach in person must not be
/// left with a service that started and then died.
pub fn probation(exe: &Path, new_pid: u32, version: &str) -> Result<()> {
    let retired = std::env::current_exe().context("own path")?;
    let verdict = watch(
        answering_version,
        version,
        PROBATION,
        HEALTHY_FOR,
        Duration::from_millis(500),
    );
    let Err(why) = verdict else {
        tracing::info!("Latch Host {version} is up and answering; the update is done");
        return Ok(());
    };
    tracing::error!("Latch Host {version} {why}; putting the previous version back");
    // A hung one still holds the port; a dead one's id may already belong
    // to something else, so only one that still listens is stopped.
    if port_held() {
        kill(new_pid);
    }
    restore_previous(exe, &retired)?;
    start_service(exe, &["--replaces", &new_pid.to_string()])
        .context("start the previous version again")?;
    tracing::info!("the previous version is back in place and starting");
    Ok(())
}

/// Wait until `probe` has said `want` for `healthy_for` without a gap, or
/// give up after `limit`, saying what went wrong.
fn watch(
    probe: impl Fn() -> Option<String>,
    want: &str,
    limit: Duration,
    healthy_for: Duration,
    every: Duration,
) -> std::result::Result<(), String> {
    let start = Instant::now();
    let mut up_since: Option<Instant> = None;
    let mut ever_up = false;
    loop {
        match probe() {
            Some(v) if v == want => {
                ever_up = true;
                if up_since.get_or_insert_with(Instant::now).elapsed() >= healthy_for {
                    return Ok(());
                }
            }
            _ => up_since = None,
        }
        if start.elapsed() >= limit {
            return Err(if ever_up {
                "started, then stopped answering".into()
            } else {
                format!("did not answer within {} s", limit.as_secs())
            });
        }
        std::thread::sleep(every);
    }
}

/// The version the service on this machine reports, if one answers.
fn answering_version() -> Option<String> {
    let st: latch_core::api::Status = latch_core::http::get_json(
        ("127.0.0.1", latch_core::CONTROL_PORT),
        "/v1/status",
        Duration::from_secs(2),
    )
    .ok()?;
    Some(st.version)
}

fn port_held() -> bool {
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], latch_core::CONTROL_PORT));
    std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(500)).is_ok()
}

fn kill(pid: u32) {
    #[cfg(windows)]
    let mut c = {
        use std::os::windows::process::CommandExt;
        let mut c = std::process::Command::new("taskkill");
        c.args(["/F", "/PID", &pid.to_string()])
            .creation_flags(0x0800_0000);
        c
    };
    #[cfg(not(windows))]
    let mut c = {
        let mut c = std::process::Command::new("kill");
        c.args(["-9", &pid.to_string()]);
        c
    };
    let _ = c
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();
    std::thread::sleep(Duration::from_millis(500));
}

/// Put the retired executable back at `exe`, and the one that failed where
/// an upload waits (`.new`), so the logs can still be matched to it.
fn restore_previous(exe: &Path, retired: &Path) -> Result<()> {
    let failed = staged(exe);
    let _ = std::fs::remove_file(&failed);
    rename_retry(exe, &failed).context("move the failed version aside")?;
    rename_retry(retired, exe).context("put the previous version back")
}

/// Remove what the last update retired. The service calls this once it
/// has run for a while, not when it starts: until the new version has
/// proven itself, the retired one is what [`probation`] puts back.
pub fn tidy(exe: &Path) {
    let _ = std::fs::remove_file(retired(exe));
    let Some(dir) = exe.parent() else { return };
    let prefix = format!(
        "{}.old",
        exe.file_name().unwrap_or_default().to_string_lossy()
    );
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let name = e.file_name();
            let name = name.to_string_lossy();
            let owned = name == prefix
                || name
                    .strip_prefix(&format!("{prefix}-"))
                    .is_some_and(|suffix| {
                        !suffix.is_empty() && suffix.bytes().all(|b| b.is_ascii_digit())
                    });
            if owned {
                let _ = std::fs::remove_file(e.path());
            }
        }
    }
}

/// Defender often holds a just-written exe open; one rename then fails
/// with a sharing violation and the Mac already got 200.
fn rename_retry(from: &Path, to: &Path) -> std::io::Result<()> {
    let mut last = None;
    for _ in 0..20 {
        match std::fs::rename(from, to) {
            Ok(()) => return Ok(()),
            Err(e) if sharing(&e) => {
                last = Some(e);
                std::thread::sleep(Duration::from_millis(250));
            }
            Err(e) => return Err(e),
        }
    }
    Err(last.unwrap())
}

fn sharing(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        ErrorKind::PermissionDenied | ErrorKind::ResourceBusy
    ) || matches!(e.raw_os_error(), Some(5 | 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(version: Option<&str>, sha: Option<&str>, body: Vec<u8>) -> Request {
        let mut headers = Vec::new();
        if let Some(v) = version {
            headers.push((UPDATE_VERSION_HEADER.to_string(), v.to_string()));
        }
        if let Some(s) = sha {
            headers.push((UPDATE_SHA256_HEADER.to_string(), s.to_string()));
        }
        Request {
            method: "POST".into(),
            path: latch_core::api::UPDATE_PATH.into(),
            headers,
            body,
        }
    }

    fn fake_exe() -> Vec<u8> {
        let mut b = vec![0u8; MIN_SIZE + 10];
        b[0] = b'M';
        b[1] = b'Z';
        b[0x3c..0x40].copy_from_slice(&0x80u32.to_le_bytes());
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        b[0x84..0x86].copy_from_slice(&0x8664u16.to_le_bytes());
        b[0x86..0x88].copy_from_slice(&1u16.to_le_bytes());
        b[0x94..0x96].copy_from_slice(&240u16.to_le_bytes());
        b[0x96..0x98].copy_from_slice(&2u16.to_le_bytes());
        b[0x98..0x9a].copy_from_slice(&0x020bu16.to_le_bytes());
        b
    }

    #[test]
    fn corrupt_and_incompatible_executables_are_refused() {
        assert!(is_host_executable(&fake_exe()));
        for (offset, value) in [(0x80, 0), (0x84, 0), (0x86, 0), (0x97, 0x20), (0x99, 1)] {
            let mut b = fake_exe();
            b[offset] = value;
            assert!(!is_host_executable(&b));
        }
        let mut b = fake_exe();
        b[0x3c..0x40].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(!is_host_executable(&b));
        assert!(!is_host_executable(b"MZ"));
    }

    #[test]
    fn a_new_service_must_answer_with_its_version_and_keep_answering() {
        use std::cell::Cell;
        let ms = Duration::from_millis;
        let want = "4.2.0";
        // Up at once and staying up.
        assert_eq!(
            watch(|| Some(want.into()), want, ms(400), ms(60), ms(10)),
            Ok(())
        );
        // Never answers: the panic on the first tick, or a hang.
        let e = watch(|| None, want, ms(100), ms(30), ms(10)).unwrap_err();
        assert!(e.contains("did not answer"), "{e}");
        // The old version still answers: the new one never took over.
        assert!(watch(|| Some("4.1.1".into()), want, ms(100), ms(30), ms(10)).is_err());
        // Answers, then dies before it has proven itself.
        let calls = Cell::new(0);
        let flaky = || {
            calls.set(calls.get() + 1);
            (calls.get() < 3).then(|| want.to_string())
        };
        let e = watch(flaky, want, ms(150), ms(100), ms(10)).unwrap_err();
        assert!(e.contains("stopped answering"), "{e}");
    }

    #[test]
    fn an_active_session_blocks_self_update() {
        assert!(refuse_self_update(true, true));
        assert!(!refuse_self_update(true, false));
        assert!(!refuse_self_update(false, true));
        assert!(!refuse_self_update(false, false));
    }

    #[test]
    fn names_sit_beside_the_executable() {
        let exe = Path::new(r"C:\Users\Ada\AppData\Local\Latch\latch-host.exe");
        assert_eq!(
            staged(exe),
            Path::new(r"C:\Users\Ada\AppData\Local\Latch\latch-host.exe.new")
        );
        assert_eq!(
            retired(exe),
            Path::new(r"C:\Users\Ada\AppData\Local\Latch\latch-host.exe.old")
        );
    }

    #[test]
    fn a_push_from_a_4_0_mac_is_accepted_under_the_old_header_names() {
        let dir = std::env::temp_dir().join(format!("latch-legacy-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("brolink-host.exe");
        let body = fake_exe();
        let newer = format!("{}.0.0", update::current().major + 1);
        let request = Request {
            method: "POST".into(),
            path: latch_core::api::UPDATE_PATH.into(),
            headers: vec![
                ("x-brolink-version".to_string(), newer.clone()),
                ("x-brolink-sha256".to_string(), update::sha256_hex(&body)),
            ],
            body: body.clone(),
        };
        let v = stage(&request, &exe).unwrap();
        assert_eq!(v.to_string(), newer);
        assert_eq!(std::fs::read(staged(&exe)).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn uploads_are_checked_before_anything_is_written() {
        let dir = std::env::temp_dir().join(format!("latch-upd-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("latch-host.exe");
        let body = fake_exe();
        let sha = update::sha256_hex(&body);
        let newer = format!("{}.0.0", update::current().major + 1);

        let r = stage(&req(None, Some(&sha), body.clone()), &exe).unwrap_err();
        assert_eq!(r.status(), 400);
        let r = stage(&req(Some("soon"), Some(&sha), body.clone()), &exe).unwrap_err();
        assert_eq!(r.status(), 400);
        let r = stage(&req(Some("0.0.1"), Some(&sha), body.clone()), &exe).unwrap_err();
        assert_eq!(r.status(), 409, "{}", r.message());
        let current = update::current().to_string();
        let r = stage(&req(Some(&current), Some(&sha), body.clone()), &exe).unwrap_err();
        assert_eq!(
            r,
            Rejected::NotNewer(format!("this PC already runs Latch Host {current}"))
        );
        let r = stage(&req(Some(&newer), None, body.clone()), &exe).unwrap_err();
        assert_eq!(r.status(), 400);
        let r = stage(&req(Some(&newer), Some("00"), body.clone()), &exe).unwrap_err();
        assert!(r.message().contains("SHA-256"), "{}", r.message());
        let mut not_pe = body.clone();
        not_pe[0] = b'X';
        let r = stage(
            &req(Some(&newer), Some(&update::sha256_hex(&not_pe)), not_pe),
            &exe,
        )
        .unwrap_err();
        assert!(
            r.message().contains("Windows executable"),
            "{}",
            r.message()
        );
        let small = b"MZ tiny".to_vec();
        let r = stage(
            &req(Some(&newer), Some(&update::sha256_hex(&small)), small),
            &exe,
        )
        .unwrap_err();
        assert_eq!(r.status(), 400);
        assert!(
            !staged(&exe).exists(),
            "nothing may be written for a refused upload"
        );

        let v = stage(
            &req(Some(&newer), Some(&sha.to_uppercase()), body.clone()),
            &exe,
        )
        .unwrap();
        assert_eq!(v.to_string(), newer);
        assert_eq!(std::fs::read(staged(&exe)).unwrap(), body);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failed_launch_restores_the_previous_executable() {
        let dir = std::env::temp_dir().join(format!("latch-swap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("latch-host.exe");
        std::fs::write(&exe, b"old").unwrap();
        assert!(apply(&exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        // A corrupt replacement must not leave a broken executable at
        // the path Windows starts on the next logon.
        std::fs::write(staged(&exe), b"new").unwrap();
        assert!(apply(&exe).is_err());
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        assert_eq!(std::fs::read(staged(&exe)).unwrap(), b"new");
        // A replacement that starts and dies at once is put back too.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = |body: &str| {
                std::fs::write(staged(&exe), body).unwrap();
                std::fs::set_permissions(staged(&exe), std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            };
            script("#!/bin/sh\nexit 3\n");
            let e = apply_within(&exe, Duration::from_secs(5)).unwrap_err();
            assert!(format!("{e:#}").contains("exited at once"), "{e:#}");
            assert_eq!(std::fs::read(&exe).unwrap(), b"old");
            assert!(staged(&exe).exists());
            // One that keeps running is the new service.
            script("#!/bin/sh\nexec sleep 2\n");
            let h = apply_within(&exe, Duration::from_millis(300)).unwrap();
            assert!(std::fs::read(&exe).unwrap().starts_with(b"#!/bin/sh"));
            assert_eq!(std::fs::read(retired(&exe)).unwrap(), b"old");
            assert_eq!(h.retired, retired(&exe));
            assert!(h.pid > 0);
            // It then never served: the watcher puts the old one back.
            restore_previous(&exe, &h.retired).unwrap();
            assert_eq!(std::fs::read(&exe).unwrap(), b"old");
            assert!(std::fs::read(staged(&exe))
                .unwrap()
                .starts_with(b"#!/bin/sh"));
            assert!(!retired(&exe).exists());
        }
        std::fs::write(retired(&exe), b"retired").unwrap();
        std::fs::write(dir.join("latch-host.exe.old-notes"), b"keep").unwrap();
        tidy(&exe);
        assert!(!retired(&exe).exists());
        assert!(dir.join("latch-host.exe.old-notes").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
