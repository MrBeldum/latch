//! A log file that does not grow without end.
//!
//! `init_logging` opened `panel.log` and `service.log` in append mode and
//! nothing ever pruned them. Both processes run for days — the panel for as
//! long as the window is open, the service until the machine restarts — so
//! the files only grew: Hermes reached a 32 MB `panel.log`, on the system
//! drive, for a program whose whole install is 14 MB.
//!
//! Writes roll over at `MAX_BYTES`. The previous contents move to
//! `<name>.1`, replacing whatever was there, so a log costs at most twice
//! `MAX_BYTES` on disk and the most recent lines always survive.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

/// The size a log may reach before it rolls over. Large enough to hold a
/// long session's worth of `info` lines, small enough that two of them are
/// never worth noticing.
pub const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// Where the previous generation of `path` is kept: `panel.log.1`. An
/// extension rather than a sibling name, so a directory listing keeps the
/// two together and a `*.log*` clean-up catches both.
pub fn rotated_path(path: &Path) -> PathBuf {
    let mut name = path.file_name().unwrap_or_default().to_os_string();
    name.push(".1");
    path.with_file_name(name)
}

/// An append-only log file that rolls over once it passes `MAX_BYTES`.
///
/// The handle is an `Option` only so that a rollover can drop it: see
/// `rotate`, which has to close the file before it can move it.
pub struct RotatingLog {
    path: PathBuf,
    file: Option<File>,
    written: u64,
    max_bytes: u64,
}

impl RotatingLog {
    /// Open `path` for appending, continuing to count from its current size
    /// so a log that is already oversized rolls over on the next write
    /// rather than growing until the process happens to restart.
    pub fn open(path: PathBuf) -> io::Result<Self> {
        Self::with_max(path, MAX_BYTES)
    }

    fn with_max(path: PathBuf, max_bytes: u64) -> io::Result<Self> {
        let file = Self::append_to(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            file: Some(file),
            written,
            max_bytes,
        })
    }

    fn append_to(path: &Path) -> io::Result<File> {
        OpenOptions::new().create(true).append(true).open(path)
    }

    /// Move what is on disk to `<name>.1` and start a new live file.
    ///
    /// The close-then-rename order is what makes this work on Windows.
    /// Truncating in place is not an option there: a handle opened for
    /// appending is asked for `FILE_APPEND_DATA` without `FILE_WRITE_DATA`,
    /// so `set_len` is denied and the rollover silently does nothing — on
    /// the one platform that grew the 32 MB log. Windows also refuses to
    /// rename a file that is still open, and to replace an existing
    /// destination, hence dropping the handle and clearing `.1` first.
    ///
    /// A failed move leaves the old file intact; writes are discarded until
    /// rotation succeeds so disk usage stays bounded.
    fn rotate(&mut self) -> io::Result<()> {
        if let Some(mut f) = self.file.take() {
            let _ = f.flush();
        }
        let prev = rotated_path(&self.path);
        let _ = std::fs::remove_file(&prev);
        let moved = std::fs::rename(&self.path, &prev);
        let file = Self::append_to(&self.path)?;
        // After a move this is a new, empty file; if the move failed it is
        // the old one, still oversized, and the next write tries again.
        self.written = file.metadata().map(|m| m.len()).unwrap_or(0);
        self.file = Some(file);
        moved
    }
}

impl Write for RotatingLog {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.written + buf.len() as u64 > self.max_bytes {
            // An unwritable backup must not allow unlimited append growth.
            if self.rotate().is_err() {
                // Preserve the existing log rather than grow it without a cap.
                return Ok(buf.len());
            }
        }
        let file = match self.file.as_mut() {
            Some(f) => f,
            // Only reachable if a rollover could not reopen the log.
            None => self.file.insert(Self::append_to(&self.path)?),
        };
        let available = self.max_bytes.saturating_sub(self.written) as usize;
        if available == 0 {
            return Ok(buf.len());
        }
        let n = file.write(&buf[..buf.len().min(available)])?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        match self.file.as_mut() {
            Some(f) => f.flush(),
            None => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmpdir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("latch-logfile-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn failed_rotation_and_oversized_writes_remain_bounded() {
        let dir = tmpdir("failed-roll");
        let path = dir.join("service.log");
        std::fs::create_dir(rotated_path(&path)).unwrap();
        let mut log = RotatingLog::with_max(path.clone(), 64).unwrap();
        for _ in 0..3 {
            log.write_all(&[b'x'; 100]).unwrap();
        }
        log.flush().unwrap();
        assert!(std::fs::metadata(path).unwrap().len() <= 64);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn rolls_over_once_past_the_limit_and_keeps_one_previous_file() {
        let dir = tmpdir("roll");
        let path = dir.join("panel.log");
        let mut log = RotatingLog::with_max(path.clone(), 64).unwrap();

        // Stay under the limit: one file, nothing rotated.
        log.write_all(&[b'a'; 40]).unwrap();
        log.flush().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 40);
        assert!(!rotated_path(&path).exists());

        // Cross it: the 40 bytes move aside and the live file holds only
        // what was written after the rollover.
        log.write_all(&[b'b'; 40]).unwrap();
        log.flush().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 40);
        assert_eq!(std::fs::read(rotated_path(&path)).unwrap(), vec![b'a'; 40]);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'b'; 40]);

        // A second rollover replaces the previous generation; two files is
        // the whole cost, however long the process runs.
        log.write_all(&[b'c'; 40]).unwrap();
        log.flush().unwrap();
        assert_eq!(std::fs::read(rotated_path(&path)).unwrap(), vec![b'b'; 40]);
        assert_eq!(std::fs::read(&path).unwrap(), vec![b'c'; 40]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_already_oversized_log_rolls_over_on_the_next_write() {
        // The case on Hermes: 32 MB written by a build that never rotated.
        let dir = tmpdir("oversized");
        let path = dir.join("service.log");
        std::fs::write(&path, vec![b'x'; 500]).unwrap();

        let mut log = RotatingLog::with_max(path.clone(), 64).unwrap();
        log.write_all(b"new").unwrap();
        log.flush().unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new");
        assert_eq!(std::fs::metadata(rotated_path(&path)).unwrap().len(), 500);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_previous_generation_sits_beside_the_log() {
        assert_eq!(
            rotated_path(Path::new("/var/latch/panel.log")),
            PathBuf::from("/var/latch/panel.log.1")
        );
    }
}
