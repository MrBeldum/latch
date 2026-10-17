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

use anyhow::{Context, Result};
use latch_core::api::{UPDATE_MAX_BYTES, UPDATE_SHA256_HEADER, UPDATE_VERSION_HEADER};
use latch_core::http::Request;
use latch_core::legacy;
use latch_core::update;
use semver::Version;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Anything smaller is not the host.
const MIN_SIZE: usize = 1024 * 1024;
/// How long a new copy must stay up before the old one lets go. It waits
/// for the old one's port, so it is still running after this unless it
/// could not start at all.
const STARTUP_GRACE: Duration = Duration::from_secs(2);

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

/// Swap the staged executable in and start it. The caller exits afterwards;
/// the new process waits for that before it takes the port.
pub fn apply(exe: &Path) -> Result<()> {
    apply_within(exe, STARTUP_GRACE)
}

fn apply_within(exe: &Path, grace: Duration) -> Result<()> {
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
    let restore = |why: anyhow::Error| -> Result<()> {
        rename_retry(exe, &new).context("move the failed replacement aside")?;
        rename_retry(&old, exe).context("restore the previous executable")?;
        Err(why.context("the previous executable is back in place"))
    };
    let mut c = std::process::Command::new(exe);
    c.arg("--background")
        .arg("--replaces")
        .arg(std::process::id().to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        c.creation_flags(0x0000_0008 | 0x0800_0000); // DETACHED_PROCESS | CREATE_NO_WINDOW
    }
    let mut child = match c.spawn() {
        Ok(child) => child,
        Err(e) => return restore(anyhow::Error::new(e).context("start the new service")),
    };
    // One that has already exited could not start (a crash, a missing
    // library): it would leave the PC with no service after this one quits.
    std::thread::sleep(grace);
    if let Ok(Some(status)) = child.try_wait() {
        return restore(anyhow::anyhow!("the new service exited at once ({status})"));
    }
    Ok(())
}

/// Remove what the last update retired, once its process has gone.
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
            let e = apply_within(&exe, Duration::from_millis(500)).unwrap_err();
            assert!(format!("{e:#}").contains("exited at once"), "{e:#}");
            assert_eq!(std::fs::read(&exe).unwrap(), b"old");
            assert!(staged(&exe).exists());
            // One that keeps running is the new service.
            script("#!/bin/sh\nexec sleep 2\n");
            apply_within(&exe, Duration::from_millis(300)).unwrap();
            assert!(std::fs::read(&exe).unwrap().starts_with(b"#!/bin/sh"));
            assert_eq!(std::fs::read(retired(&exe)).unwrap(), b"old");
        }
        std::fs::write(retired(&exe), b"retired").unwrap();
        std::fs::write(dir.join("latch-host.exe.old-notes"), b"keep").unwrap();
        tidy(&exe);
        assert!(!retired(&exe).exists());
        assert!(dir.join("latch-host.exe.old-notes").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
