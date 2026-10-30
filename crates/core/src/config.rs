//! Where each app keeps its few settings: one TOML file in the per-user data
//! directory (`%LOCALAPPDATA%\Latch` on Windows, `~/Library/Application
//! Support/io.github.MrBeldum.Latch` on macOS, `~/.local/share/latch` on Linux).

use anyhow::{Context, Result};
use serde::{de::DeserializeOwned, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Mutex, Once,
};

static CONFIG_LOCK: Mutex<()> = Mutex::new(());
static TEMP_ID: AtomicU64 = AtomicU64::new(0);

/// The per-user data directory, created private to this user.
///
/// `LATCH_DATA_DIR` replaces it when set. The workspace's
/// `.cargo/config.toml` sets it for `cargo test` and `cargo run`, so a test
/// run on a machine with Latch installed leaves that install's settings
/// and logs alone.
///
/// The first call by a copy that finds the folder 4.0 and older used, and
/// none of its own, takes that folder over: see [`adopt`].
pub fn data_dir() -> Result<PathBuf> {
    let explicit = crate::legacy::env("LATCH_DATA_DIR").map(PathBuf::from);
    let adopting = explicit.is_none();
    let dir = if let Some(dir) = explicit {
        Some(dir)
    } else if cfg!(windows) {
        std::env::var_os("LOCALAPPDATA")
            .map(PathBuf::from)
            .map(|p| p.join(crate::APP_NAME))
    } else {
        directories::ProjectDirs::from("io.github", "MrBeldum", crate::APP_NAME)
            .map(|d| d.data_dir().to_path_buf())
    }
    .context("no per-user data directory")?;
    if adopting {
        static ONCE: Once = Once::new();
        ONCE.call_once(|| {
            for old in [crate::legacy::latch_data_dir(), crate::legacy::data_dir()]
                .into_iter()
                .flatten()
            {
                match adopt(&old, &dir) {
                    Ok(true) => tracing::info!("took over {} as {}", old.display(), dir.display()),
                    Ok(false) => {}
                    Err(e) => tracing::warn!("could not take over {}: {e:#}", old.display()),
                }
            }
        });
    }
    std::fs::create_dir_all(&dir).with_context(|| dir.display().to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(dir)
}

/// Make the folder an older install left at `old` the one at `new`, when
/// `old` exists and `new` does not. Everything in it comes along (settings,
/// logs, and whatever an older version kept there). Returns whether it did.
///
/// A folder is moved. On Windows the old one holds the running
/// `latch-host.exe` of an install that has not been set up again, and
/// Windows will not move a folder with a running program in it, so it is
/// copied without its executables and the old one is left for setup to
/// remove.
pub fn adopt(old: &Path, new: &Path) -> Result<bool> {
    if new.exists() || !old.is_dir() {
        return Ok(false);
    }
    if let Some(parent) = new.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if !cfg!(windows) && std::fs::rename(old, new).is_ok() {
        return Ok(true);
    }
    // Staged beside the destination, so that a copy cut short never leaves a
    // half-filled folder that the next start mistakes for the real one.
    let stage = new.with_extension(format!("adopting-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&stage);
    let copied = copy_tree(old, &stage)
        .and_then(|()| std::fs::rename(&stage, new).with_context(|| new.display().to_string()));
    if copied.is_err() {
        let _ = std::fs::remove_dir_all(&stage);
    }
    copied.map(|()| true)
}

/// Copy `from` to `to`, leaving out programs and the swap files an update
/// makes of them. A file that cannot be read (another process holds it
/// open) is skipped, not fatal: it is a log, not a setting.
fn copy_tree(from: &Path, to: &Path) -> Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let name = entry.file_name();
        let lower = name.to_string_lossy().to_ascii_lowercase();
        let src = entry.path();
        let dst = to.join(&name);
        if entry.file_type()?.is_dir() {
            copy_tree(&src, &dst)?;
        } else if lower.ends_with(".exe")
            || lower.ends_with(".exe.old")
            || lower.ends_with(".exe.new")
        {
            continue;
        } else if let Err(e) = std::fs::copy(&src, &dst) {
            tracing::warn!("skipped {}: {e}", src.display());
        }
    }
    Ok(())
}

/// Read `name` from the data directory; defaults when the file is missing
/// or cannot be read, because refusing to start over a setting is worse
/// than re-picking it. One value this build does not understand (a choice
/// a newer build added, a typo) costs only that value: see [`salvage`].
pub fn load<T: DeserializeOwned + Serialize + Default>(name: &str) -> T {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    lock_config(name)
        .and_then(|_lock| read_unlocked(name))
        .unwrap_or_else(|e| {
            tracing::warn!("{e:#}; using defaults");
            T::default()
        })
}

/// Read `name` exactly as it is: the default only when there is no file,
/// an error for anything else, and the file left as it was. For a reader
/// that already holds good settings and should keep them rather than take
/// defaults over a file someone is still editing.
pub fn try_load<T: DeserializeOwned + Default>(name: &str) -> Result<T> {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let path = data_dir()?.join(name);
    let Some(bytes) = read_bytes(&path)? else {
        return Ok(T::default());
    };
    let text =
        String::from_utf8(bytes).with_context(|| format!("{} is not text", path.display()))?;
    toml::from_str(&text).with_context(|| path.display().to_string())
}

/// The file's bytes; `None` when there is no file. Any other failure (no
/// permission, a sharing violation, a disk error) is an error: that file
/// may hold every setting, and treating it as missing would overwrite it.
fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("could not read {}", path.display())),
    }
}

fn read_unlocked<T: DeserializeOwned + Serialize + Default>(name: &str) -> Result<T> {
    let path = data_dir()?.join(name);
    let Some(bytes) = read_bytes(&path)? else {
        return Ok(T::default());
    };
    let Ok(text) = String::from_utf8(bytes) else {
        tracing::warn!("{} is not text; using defaults", path.display());
        set_aside(&path, true);
        return Ok(T::default());
    };
    let (value, dropped) = match parse_lenient::<T>(&text) {
        Ok((value, dropped)) if dropped.is_empty() => return Ok(value),
        Ok(salvaged) => salvaged,
        Err(e) => {
            tracing::warn!("{} is unreadable ({e:#}); using defaults", path.display());
            set_aside(&path, true);
            return Ok(T::default());
        }
    };
    // Well-formed, but some value does not fit (one a newer build wrote, or
    // a typo). Keep everything else, and the original beside it.
    tracing::warn!(
        "{}: could not read {}; using the default for {}",
        path.display(),
        dropped.join(", "),
        if dropped.len() == 1 { "it" } else { "them" }
    );
    set_aside(&path, false);
    if let Err(e) = save_unlocked(name, &value) {
        tracing::warn!("could not rewrite {}: {e:#}", path.display());
    }
    Ok(value)
}

/// Read settings text: as a whole when it fits `T`, else what [`salvage`]
/// keeps of it, with the keys it left out (none when it all fit). An error
/// only when the text is not TOML at all.
pub fn parse_lenient<T: DeserializeOwned + Serialize + Default>(
    text: &str,
) -> Result<(T, Vec<String>)> {
    match toml::from_str(text) {
        Ok(value) => Ok((value, Vec::new())),
        Err(e) => {
            let disk = text.parse::<toml::Table>().map_err(|_| e)?;
            Ok(salvage(&disk))
        }
    }
}

/// Keep a damaged settings file as `<stem>.bad-<pid>-<n>`: moved when it is
/// to be replaced by defaults, copied when what could be read of it is
/// written back in its place (so there is never a moment without it).
fn set_aside(path: &Path, moved: bool) {
    let backup = path.with_extension(format!(
        "bad-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let kept = if moved {
        std::fs::rename(path, &backup)
    } else {
        std::fs::copy(path, &backup).map(|_| ())
    };
    match kept {
        Ok(()) => tracing::warn!("damaged config preserved at {}", backup.display()),
        Err(e) => tracing::warn!("could not preserve damaged config: {e}"),
    }
}

/// What `T` can take from a well-formed file that, as a whole, it cannot:
/// starting from the defaults, each key goes in whole if `T` still reads,
/// else a table key by key, else not at all. Returns the value and the
/// keys left out (`stream.resolution`). Every settings struct here is
/// `#[serde(default)]`, so any part of one is still one.
fn salvage<T: DeserializeOwned + Serialize + Default>(disk: &toml::Table) -> (T, Vec<String>) {
    let reads = |t: &toml::Table| T::deserialize(toml::Value::Table(t.clone())).is_ok();
    let mut good = toml::Table::try_from(T::default()).unwrap_or_default();
    let mut dropped = Vec::new();
    graft(&mut good, &[], disk, &reads, &mut dropped);
    let value = T::deserialize(toml::Value::Table(good)).unwrap_or_default();
    (value, dropped)
}

fn graft(
    root: &mut toml::Table,
    at: &[String],
    disk: &toml::Table,
    reads: &dyn Fn(&toml::Table) -> bool,
    dropped: &mut Vec<String>,
) {
    for (key, value) in disk {
        let mut path = at.to_vec();
        path.push(key.clone());
        let mut trial = root.clone();
        put(&mut trial, &path, value.clone());
        if reads(&trial) {
            *root = trial;
            continue;
        }
        if let toml::Value::Table(inner) = value {
            // Its parts, then, into the table the default has there (or an
            // empty one, if that much reads).
            let mut trial = root.clone();
            if !matches!(get(&trial, &path), Some(toml::Value::Table(_))) {
                put(&mut trial, &path, toml::Value::Table(toml::Table::new()));
            }
            if reads(&trial) {
                *root = trial;
                graft(root, &path, inner, reads, dropped);
                continue;
            }
        }
        dropped.push(path.join("."));
    }
}

fn get<'a>(root: &'a toml::Table, path: &[String]) -> Option<&'a toml::Value> {
    let (last, parents) = path.split_last()?;
    let mut table = root;
    for key in parents {
        table = table.get(key)?.as_table()?;
    }
    table.get(last)
}

/// Set `path` in `root`; its parents are tables already (see `graft`).
fn put(root: &mut toml::Table, path: &[String], value: toml::Value) {
    let Some((last, parents)) = path.split_last() else {
        return;
    };
    let mut table = root;
    for key in parents {
        match table.get_mut(key).and_then(|v| v.as_table_mut()) {
            Some(t) => table = t,
            None => return,
        }
    }
    table.insert(last.clone(), value);
}

/// An OS-level advisory file lock held while updating or saving a config file.
pub struct ConfigLock {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(windows)]
    file: std::fs::File,
}

#[cfg(unix)]
impl Drop for ConfigLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        unsafe {
            libc::flock(self.file.as_raw_fd(), libc::LOCK_UN);
        }
    }
}

#[cfg(windows)]
impl Drop for ConfigLock {
    fn drop(&mut self) {
        use std::os::windows::io::AsRawHandle;
        let handle = self.file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut overlapped =
            std::mem::MaybeUninit::<windows_sys::Win32::System::IO::OVERLAPPED>::zeroed();
        unsafe {
            windows_sys::Win32::Storage::FileSystem::UnlockFileEx(
                handle,
                0,
                !0,
                !0,
                overlapped.as_mut_ptr(),
            );
        }
    }
}

pub fn lock_config(name: &str) -> Result<ConfigLock> {
    let dir = data_dir()?;
    let lock_path = dir.join(format!("{name}.lock"));
    #[cfg(unix)]
    {
        use std::os::unix::{fs::OpenOptionsExt, io::AsRawFd};
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&lock_path)
            .with_context(|| format!("open lock file {}", lock_path.display()))?;
        let ret = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
        if ret != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("flock lock file {}", lock_path.display()));
        }
        Ok(ConfigLock { file })
    }
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("open lock file {}", lock_path.display()))?;
        let handle = file.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        let mut overlapped =
            std::mem::MaybeUninit::<windows_sys::Win32::System::IO::OVERLAPPED>::zeroed();
        let ret = unsafe {
            windows_sys::Win32::Storage::FileSystem::LockFileEx(
                handle,
                windows_sys::Win32::Storage::FileSystem::LOCKFILE_EXCLUSIVE_LOCK,
                0,
                !0,
                !0,
                overlapped.as_mut_ptr(),
            )
        };
        if ret == 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("LockFileEx {}", lock_path.display()));
        }
        Ok(ConfigLock { file })
    }
    #[cfg(not(any(unix, windows)))]
    {
        Ok(ConfigLock {})
    }
}

/// Apply a read-modify-write transaction without allowing another in-process
/// settings or discovery writer to replace the snapshot underneath it.
/// A file that exists but cannot be read is left alone: the edit fails
/// rather than replace it with defaults.
pub fn update<T: Serialize + DeserializeOwned + Default + PartialEq>(
    name: &str,
    edit: impl FnOnce(&mut T),
) -> Result<()> {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _file_guard = lock_config(name)?;
    let mut value: T = read_unlocked(name)?;
    let before = toml::to_string(&value)?;
    edit(&mut value);
    if toml::to_string(&value)? != before {
        save_unlocked(name, &value)?;
    }
    Ok(())
}

pub fn save<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let _guard = CONFIG_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let _file_guard = lock_config(name)?;
    save_unlocked(name, value)
}

fn save_unlocked<T: Serialize>(name: &str, value: &T) -> Result<()> {
    let path = data_dir()?.join(name);
    let text = toml::to_string_pretty(value)?;
    let tmp = path.with_extension(format!(
        "tmp-{}-{}",
        std::process::id(),
        TEMP_ID.fetch_add(1, Ordering::Relaxed)
    ));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&tmp, &path).with_context(|| path.display().to_string())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// This machine's name as the OS shows it.
pub fn machine_name() -> String {
    #[cfg(windows)]
    if let Some(n) = std::env::var_os("COMPUTERNAME") {
        let n = n.to_string_lossy().trim().to_string();
        if !n.is_empty() {
            return n;
        }
    }
    #[cfg(target_os = "macos")]
    if let Ok(out) = std::process::Command::new("scutil")
        .args(["--get", "ComputerName"])
        .output()
    {
        let n = String::from_utf8_lossy(&out.stdout).trim().to_string();
        if out.status.success() && !n.is_empty() {
            return n;
        }
    }
    std::process::Command::new("hostname")
        .output()
        .ok()
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .trim()
                .trim_end_matches(".local")
                .to_string()
        })
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "This computer".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    struct Cfg {
        n: u32,
    }

    #[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
    enum Size {
        Small,
        #[default]
        Native,
    }

    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    struct Stream {
        size: Size,
        fps: u32,
    }

    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    struct Machine {
        name: String,
        mac: Option<String>,
        seen: Option<u64>,
    }

    /// The shape of client.toml: settings with an enum, and machines.
    #[derive(Debug, Default, Serialize, Deserialize, PartialEq)]
    #[serde(default)]
    struct Settings {
        stream: Stream,
        prompt: bool,
        pcs: std::collections::BTreeMap<String, Machine>,
    }

    fn backups(stem: &str) -> Vec<PathBuf> {
        let prefix = format!("{stem}.bad-");
        std::fs::read_dir(data_dir().unwrap())
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect()
    }

    /// A newer build's choice ("P720" read by a build without it) used to
    /// cost the whole file: every saved machine and wake address.
    #[test]
    fn one_value_this_build_cannot_read_costs_only_that_value() {
        let stem = format!("salvage-{}", std::process::id());
        let name = format!("{stem}.toml");
        let path = data_dir().unwrap().join(&name);
        let text = "prompt = true\n[stream]\nsize = \"P9999\"\nfps = 45\n\
                    [pcs.n1]\nname = \"PC\"\nmac = \"aa:bb:cc:dd:ee:ff\"\n\
                    [pcs.n2]\nname = \"Mac\"\nseen = \"yesterday\"\n";
        std::fs::write(&path, text).unwrap();
        let got: Settings = load(&name);
        assert_eq!(got.stream.size, Size::Native);
        assert_eq!(got.stream.fps, 45);
        assert!(got.prompt);
        assert_eq!(got.pcs["n1"].mac.as_deref(), Some("aa:bb:cc:dd:ee:ff"));
        assert_eq!(got.pcs["n2"].name, "Mac");
        assert_eq!(got.pcs["n2"].seen, None, "only the bad field goes");
        // The file now reads cleanly, and the original is kept once.
        assert_eq!(try_load::<Settings>(&name).unwrap(), got);
        let kept = backups(&stem);
        assert_eq!(kept.len(), 1);
        assert_eq!(std::fs::read_to_string(&kept[0]).unwrap(), text);
        assert_eq!(load::<Settings>(&name), got);
        assert_eq!(backups(&stem).len(), 1, "no new copy on a clean read");
        for p in kept {
            std::fs::remove_file(p).unwrap();
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_strict_read_never_moves_or_defaults_a_damaged_file() {
        let stem = format!("strict-{}", std::process::id());
        let name = format!("{stem}.toml");
        let path = data_dir().unwrap().join(&name);
        assert_eq!(try_load::<Cfg>(&name).unwrap(), Cfg::default(), "missing");
        std::fs::write(&path, "n = [oops").unwrap();
        assert!(try_load::<Cfg>(&name).is_err());
        std::fs::write(&path, "n = \"seven\"").unwrap();
        assert!(try_load::<Cfg>(&name).is_err());
        std::fs::write(&path, b"n = 1\n\xff").unwrap();
        assert!(try_load::<Cfg>(&name).is_err());
        assert!(path.exists(), "left where it was");
        assert!(backups(&stem).is_empty());
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn a_file_that_is_not_text_is_set_aside_like_junk() {
        let stem = format!("binary-{}", std::process::id());
        let name = format!("{stem}.toml");
        let path = data_dir().unwrap().join(&name);
        std::fs::write(&path, b"n = 1\n\xff").unwrap();
        assert_eq!(load::<Cfg>(&name), Cfg::default());
        let kept = backups(&stem);
        assert_eq!(kept.len(), 1);
        assert!(!path.exists());
        for p in kept {
            std::fs::remove_file(p).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_not_overwritten() {
        use std::os::unix::fs::PermissionsExt;
        let name = format!("locked-{}.toml", std::process::id());
        save(&name, &Cfg { n: 7 }).unwrap();
        let path = data_dir().unwrap().join(&name);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&path).is_ok() {
            // Root reads anything; nothing to show here.
            std::fs::remove_file(path).unwrap();
            return;
        }
        assert!(update::<Cfg>(&name, |c| c.n = 9).is_err());
        assert!(try_load::<Cfg>(&name).is_err());
        assert_eq!(load::<Cfg>(&name), Cfg::default());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(load::<Cfg>(&name), Cfg { n: 7 });
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn save_then_load_round_trips_and_junk_falls_back() {
        let stem = format!("test-{}", std::process::id());
        let name = format!("{stem}.toml");
        let dir = data_dir().unwrap();
        save(&name, &Cfg { n: 7 }).unwrap();
        assert_eq!(load::<Cfg>(&name), Cfg { n: 7 });
        std::fs::write(dir.join(&name), "not = [toml").unwrap();
        assert_eq!(load::<Cfg>(&name), Cfg::default());
        // Loading junk moves it aside as `<stem>.bad-*`, exactly once.
        let prefix = format!("{stem}.bad-");
        let preserved: Vec<PathBuf> = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n.starts_with(&prefix))
            })
            .collect();
        assert_eq!(
            preserved.len(),
            1,
            "damaged config is preserved exactly once"
        );
        for path in preserved {
            std::fs::remove_file(path).unwrap();
        }
        assert!(!dir.join(&name).exists());
        assert_eq!(load::<Cfg>("does-not-exist.toml"), Cfg::default());
    }

    #[test]
    fn concurrent_transactions_preserve_all_updates_and_private_permissions() {
        let name = format!("transaction-test-{}.toml", std::process::id());
        save(&name, &Cfg { n: 0 }).unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let name = &name;
                scope.spawn(move || {
                    for _ in 0..10 {
                        update::<Cfg>(name, |c| c.n += 1).unwrap();
                    }
                });
            }
        });
        assert_eq!(load::<Cfg>(&name).n, 80);
        let path = data_dir().unwrap().join(&name);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn config_process_writer() {
        let Ok(name) = std::env::var("LATCH_TEST_CONFIG_NAME") else {
            return;
        };
        for _ in 0..20 {
            update::<Cfg>(&name, |c| c.n += 1).unwrap();
        }
    }

    #[test]
    fn transactions_serialize_across_processes() {
        let name = format!("processes-{}.toml", std::process::id());
        save(&name, &Cfg { n: 0 }).unwrap();
        let spawn = || {
            std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "config::tests::config_process_writer"])
                .env("LATCH_TEST_CONFIG_NAME", &name)
                .spawn()
                .unwrap()
        };
        let mut first = spawn();
        let mut second = spawn();
        assert!(first.wait().unwrap().success());
        assert!(second.wait().unwrap().success());
        assert_eq!(load::<Cfg>(&name).n, 40);
        std::fs::remove_file(data_dir().unwrap().join(name)).unwrap();
    }

    #[test]
    fn file_lock_coordinates_exclusive_access() {
        use std::time::Duration;
        let name = format!("flock-test-{}.toml", std::process::id());
        let lock1 = lock_config(&name).expect("acquire lock1");
        let (tx, rx) = std::sync::mpsc::channel();
        let name_clone = name.clone();
        let t = std::thread::spawn(move || {
            let _lock2 = lock_config(&name_clone).expect("acquire lock2");
            tx.send(true).unwrap();
        });
        assert_eq!(
            rx.recv_timeout(Duration::from_millis(50)),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        drop(lock1);
        assert_eq!(rx.recv_timeout(Duration::from_millis(500)), Ok(true));
        t.join().unwrap();
        let _ = std::fs::remove_file(data_dir().unwrap().join(format!("{name}.lock")));
    }

    /// `.cargo/config.toml` points `cargo test` at `target/latch-data`.
    /// Without it every test that loads or saves settings would read and
    /// rewrite the real install's files on the machine running the tests.
    #[test]
    fn tests_run_against_a_data_directory_under_target() {
        let want = std::env::var_os("LATCH_DATA_DIR").expect("cargo test sets LATCH_DATA_DIR");
        let dir = data_dir().unwrap();
        assert_eq!(dir, PathBuf::from(want));
        assert!(
            dir.components().any(|c| c.as_os_str() == "target"),
            "{}",
            dir.display()
        );
    }

    #[test]
    fn a_new_install_takes_over_the_folder_an_older_one_left() {
        let root = std::env::temp_dir().join(format!("latch-adopt-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let old = root.join("old");
        let new = root.join("nested/new");
        std::fs::create_dir_all(old.join("engine/config")).unwrap();
        std::fs::write(old.join("host.toml"), "sunshine_user = \"brolink\"\n").unwrap();
        std::fs::write(old.join("engine/config/state.json"), "{}").unwrap();
        std::fs::write(old.join("brolink-host.exe"), "program").unwrap();

        assert!(adopt(&old, &new).unwrap());
        assert_eq!(
            std::fs::read_to_string(new.join("host.toml")).unwrap(),
            "sunshine_user = \"brolink\"\n"
        );
        assert!(new.join("engine/config/state.json").exists());
        if cfg!(windows) {
            // Copied: programs stay behind in the old folder.
            assert!(!new.join("brolink-host.exe").exists());
            assert!(old.join("host.toml").exists());
        } else {
            assert!(new.join("brolink-host.exe").exists());
            assert!(!old.exists());
        }

        // Once there is a folder of its own, nothing is taken again.
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("host.toml"), "other").unwrap();
        assert!(!adopt(&old, &new).unwrap());
        assert_ne!(
            std::fs::read_to_string(new.join("host.toml")).unwrap(),
            "other"
        );
        // No old folder, nothing to take.
        assert!(!adopt(&root.join("missing"), &root.join("elsewhere")).unwrap());
        assert!(!root.join("elsewhere").exists());
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn copying_a_folder_leaves_programs_out_and_is_staged() {
        let root = std::env::temp_dir().join(format!("latch-copy-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let old = root.join("old");
        std::fs::create_dir_all(&old).unwrap();
        for f in ["a.toml", "x.exe", "x.exe.old", "x.exe.new", "X.EXE"] {
            std::fs::write(old.join(f), f).unwrap();
        }
        copy_tree(&old, &root.join("new")).unwrap();
        let mut got: Vec<String> = std::fs::read_dir(root.join("new"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        got.sort();
        assert_eq!(got, ["a.toml"]);
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn machine_name_is_never_empty() {
        assert!(!machine_name().is_empty());
    }
}
