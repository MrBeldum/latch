//! Put a real playback device back as Windows' default after the engine
//! starts. The engine's own init disables Steam Streaming Speakers when
//! they are the default, which leaves a PC with no other active device
//! with no default endpoint and no sound on the stream.

use std::time::{Duration, Instant};

/// The logon task that runs the take-over in the signed-in user's session.
/// Setup run from session 0 (over SSH) starts the engine through it.
pub const ENGINE_TASK: &str = "LatchEngineUser";

#[cfg(windows)]
mod win {
    use super::ENGINE_TASK as TASK;
    use anyhow::{bail, Context, Result};
    use std::os::windows::process::CommandExt;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::sync::atomic::{AtomicBool, Ordering};

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;

    /// The helper scripts, written into the data directory when they are
    /// not there as this build has them, and the logon task that runs
    /// them, registered once per process. The service calls this every 30
    /// seconds while the engine has no sound, and it used to rewrite all
    /// three files and re-register the task each time.
    pub fn install_helpers() -> Result<PathBuf> {
        static TASK_REGISTERED: AtomicBool = AtomicBool::new(false);
        let dir = latch_core::config::data_dir()?;
        let script = dir.join("take-over-engine.ps1");
        write_if_changed(&script, include_str!("../windows/take-over-engine.ps1"))?;
        let launcher = dir.join("take-over-engine.vbs");
        write_if_changed(&launcher, include_str!("../windows/take-over-engine.vbs"))?;
        // Earlier versions pointed the task at a .cmd, which kept a terminal
        // on the desktop for as long as the helper ran. A task an
        // administrator registered cannot be re-pointed from here, so the
        // .cmd stays, reduced to handing off to the windowless launcher.
        write_if_changed(
            &dir.join("take-over-engine.cmd"),
            include_str!("../windows/take-over-engine.cmd"),
        )?;
        if !TASK_REGISTERED.load(Ordering::Relaxed) {
            let tr = format!("wscript.exe //B //Nologo \"{}\"", launcher.display());
            if create_logon_task(&tr) {
                TASK_REGISTERED.store(true, Ordering::Relaxed);
                // An earlier version's task runs its own copy of the helper
                // from the old folder, and two would race to start the
                // engine. One an administrator registered survives this;
                // setup removes it.
                delete_logon_task(crate::legacy::WINDOWS_ENGINE_TASK);
            } else {
                tracing::info!("could not register {TASK}; the host will start the engine itself");
            }
        }
        Ok(script)
    }

    fn write_if_changed(path: &Path, text: &str) -> Result<()> {
        if std::fs::read(path).is_ok_and(|have| have == text.as_bytes()) {
            return Ok(());
        }
        std::fs::write(path, text).with_context(|| path.display().to_string())
    }

    fn delete_logon_task(name: &str) {
        let _ = Command::new("schtasks")
            .args(["/Delete", "/TN", name, "/F"])
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
    }

    fn create_logon_task(tr: &str) -> bool {
        // These helpers live in the user's profile and must never gain an
        // elevated token through a scheduled task, including on upgrades.
        let args = [
            "/Create", "/TN", TASK, "/TR", tr, "/SC", "ONLOGON", "/IT", "/F", "/RL", "LIMITED",
        ];
        Command::new("schtasks")
            .args(args)
            .creation_flags(CREATE_NO_WINDOW)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    /// The Windows session this process runs in; 0 is the one without a
    /// desktop, where services and SSH shells run.
    pub fn session_id() -> u32 {
        let mut id = 0u32;
        let _ = unsafe {
            windows::Win32::System::RemoteDesktop::ProcessIdToSessionId(std::process::id(), &mut id)
        };
        id
    }

    /// Start the engine as the logged-on user and restore a default playback
    /// device. Safe to call often: a running user-session engine is left
    /// alone aside from putting the default back.
    pub fn run_take_over() -> Result<()> {
        let script = install_helpers()?;
        if session_id() == 0 {
            let status = Command::new("schtasks")
                .args(["/Run", "/TN", TASK])
                .creation_flags(CREATE_NO_WINDOW)
                .status()
                .context("run logon engine task")?;
            if !status.success() {
                bail!("could not run {TASK}");
            }
            return Ok(());
        }
        let status = Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(&script)
            .creation_flags(CREATE_NO_WINDOW)
            .status()
            .context("take over engine")?;
        if !status.success() {
            bail!("engine take-over failed");
        }
        Ok(())
    }
}

#[cfg(windows)]
pub use win::{install_helpers, session_id};

/// Start the engine as the logged-on user (the engine is not running) and
/// put a default playback device back. The engine starts afresh, so any
/// backoff [`retry_take_over`] was keeping starts afresh too.
pub fn take_over_engine() -> anyhow::Result<()> {
    *AUDIO_RETRY.lock() = None;
    #[cfg(windows)]
    {
        win::run_take_over()
    }
    #[cfg(not(windows))]
    Ok(())
}

/// The take-over again for a running engine whose log says it has no sound,
/// at most as often as [`retry_due`] allows; `problem` is that complaint,
/// and an empty one (the sound works) forgets the backoff. Each run starts
/// PowerShell and can do nothing on a PC with no playback device at all,
/// where the complaint never clears.
pub fn retry_take_over(problem: &str) {
    let mut state = AUDIO_RETRY.lock();
    if problem.is_empty() {
        *state = None;
        return;
    }
    let now = Instant::now();
    let Some(wait) = retry_due(state.as_ref(), problem, now) else {
        return;
    };
    *state = Some(AudioRetry {
        problem: problem.to_string(),
        last: now,
        wait,
    });
    drop(state);
    #[cfg(windows)]
    {
        if let Err(e) = win::run_take_over() {
            tracing::info!("audio take-over: {e:#}");
        }
    }
}

/// The first wait after a take-over that did not bring the sound back, and
/// the longest; each run in between doubles it.
const RETRY_FIRST: Duration = Duration::from_secs(30);
const RETRY_CAP: Duration = Duration::from_secs(10 * 60);

/// The last take-over run for an audio problem: what it was for, when, and
/// how long to wait before the next.
struct AudioRetry {
    problem: String,
    last: Instant,
    wait: Duration,
}

static AUDIO_RETRY: parking_lot::Mutex<Option<AudioRetry>> = parking_lot::Mutex::new(None);

/// Whether the take-over should run now for `problem`, and if so the wait
/// before the one after: at once for a problem it has not run for, then
/// after 30 s, 1, 2, 4 and 8 minutes, then every 10.
fn retry_due(prev: Option<&AudioRetry>, problem: &str, now: Instant) -> Option<Duration> {
    match prev {
        Some(p) if p.problem == problem => {
            (now.duration_since(p.last) >= p.wait).then(|| (p.wait * 2).min(RETRY_CAP))
        }
        _ => Some(RETRY_FIRST),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn after(prev: &AudioRetry, secs: u64) -> Instant {
        prev.last + Duration::from_secs(secs)
    }

    #[test]
    fn a_new_audio_problem_runs_the_take_over_at_once() {
        let now = Instant::now();
        assert_eq!(retry_due(None, "no sink", now), Some(RETRY_FIRST));
        let prev = AudioRetry {
            problem: "no sink".into(),
            last: now,
            wait: Duration::from_secs(480),
        };
        assert_eq!(
            retry_due(Some(&prev), "no endpoint", after(&prev, 1)),
            Some(RETRY_FIRST),
            "a different complaint is new"
        );
    }

    #[test]
    fn the_same_problem_waits_out_a_backoff_that_doubles_up_to_ten_minutes() {
        let mut prev = AudioRetry {
            problem: "no sink".into(),
            last: Instant::now(),
            wait: RETRY_FIRST,
        };
        assert_eq!(retry_due(Some(&prev), "no sink", after(&prev, 29)), None);
        let mut waits = vec![];
        for _ in 0..8 {
            let now = after(&prev, prev.wait.as_secs());
            let wait = retry_due(Some(&prev), "no sink", now).expect("due");
            waits.push(wait.as_secs());
            prev = AudioRetry {
                problem: "no sink".into(),
                last: now,
                wait,
            };
        }
        assert_eq!(waits, [60, 120, 240, 480, 600, 600, 600, 600]);
    }
}
