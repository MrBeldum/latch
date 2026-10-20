//! Remote power actions, and keeping the machine from idle-sleeping.

use anyhow::Result;
use latch_core::api::PowerAction;
#[cfg(target_os = "macos")]
use std::time::{Duration, Instant};

/// Keep the machine from idle-sleeping while the service runs, so Tailscale
/// stays up, but only while it is plugged in. Sleep the owner asks for
/// (the Start menu, the Apple menu, the lid) still works. Returns whether
/// the machine is now kept awake, so the log says so only when it is.
///
/// Windows: an execution-state request, which belongs to the calling thread;
/// call it from the background service's `refresh_loop`, its one caller.
/// macOS: a PreventUserIdleSystemSleep assertion, which belongs to the
/// process and goes with it. Elsewhere nothing: a server does not idle-sleep.
pub fn keep_awake(on: bool) -> bool {
    #[cfg(windows)]
    {
        use windows::Win32::System::Power::{
            GetSystemPowerStatus, SetThreadExecutionState, ES_CONTINUOUS, ES_SYSTEM_REQUIRED,
            SYSTEM_POWER_STATUS,
        };
        // An execution-state request is reversible and belongs to this
        // thread; it never rewrites the owner's power plan (setup does that
        // once, on purpose), so turning the setting off truly turns it off.
        // The setting promises "while plugged in": a laptop on battery is
        // left to sleep. Desktops report AC power.
        unsafe {
            let mut power = SYSTEM_POWER_STATUS::default();
            let plugged_in = GetSystemPowerStatus(&mut power).is_ok() && power.ACLineStatus == 1;
            if on && plugged_in {
                SetThreadExecutionState(ES_CONTINUOUS | ES_SYSTEM_REQUIRED);
            } else {
                SetThreadExecutionState(ES_CONTINUOUS);
            }
            on && plugged_in
        }
    }
    #[cfg(target_os = "macos")]
    {
        mac::keep_awake(on && mac::on_ac_power())
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = on;
        false
    }
}

/// IOKit's power assertions and power source, without a crate for them.
#[cfg(target_os = "macos")]
mod mac {
    use std::ffi::{c_char, c_void, CStr, CString};
    use std::sync::Mutex;

    type CFTypeRef = *const c_void;
    type CFStringRef = *const c_void;
    const UTF8: u32 = 0x0800_0100; // kCFStringEncodingUTF8
    const ASSERTION_ON: u32 = 255; // kIOPMAssertionLevelOn

    #[link(name = "IOKit", kind = "framework")]
    extern "C" {
        fn IOPSCopyPowerSourcesInfo() -> CFTypeRef;
        fn IOPSGetProvidingPowerSourceType(snapshot: CFTypeRef) -> CFStringRef;
        fn IOPMAssertionCreateWithName(
            kind: CFStringRef,
            level: u32,
            name: CFStringRef,
            id: *mut u32,
        ) -> i32;
        fn IOPMAssertionRelease(id: u32) -> i32;
    }

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFRelease(cf: CFTypeRef);
        fn CFStringCreateWithCString(
            alloc: *const c_void,
            s: *const c_char,
            encoding: u32,
        ) -> CFStringRef;
        fn CFStringGetCString(s: CFStringRef, buf: *mut c_char, size: isize, encoding: u32) -> u8;
    }

    /// The assertion this process holds, if any.
    static HELD: Mutex<Option<u32>> = Mutex::new(None);

    /// Hold the assertion when `want`, release it otherwise. Returns
    /// whether it is held.
    pub fn keep_awake(want: bool) -> bool {
        let mut held = HELD.lock().unwrap_or_else(|e| e.into_inner());
        match (*held, want) {
            (Some(_), true) => true,
            (None, false) => false,
            (Some(id), false) => {
                unsafe { IOPMAssertionRelease(id) };
                *held = None;
                false
            }
            (None, true) => {
                // Not PreventSystemSleep (`caffeinate -s`): that one keeps
                // the Mac in dark wake when someone asks it to sleep.
                let (Some(kind), Some(name)) = (
                    cf_string("PreventUserIdleSystemSleep"),
                    cf_string("Latch: keep this Mac reachable while it is shared"),
                ) else {
                    return false;
                };
                let mut id = 0u32;
                let ok = unsafe { IOPMAssertionCreateWithName(kind, ASSERTION_ON, name, &mut id) };
                unsafe {
                    CFRelease(kind);
                    CFRelease(name);
                }
                if ok == 0 {
                    *held = Some(id);
                }
                held.is_some()
            }
        }
    }

    /// The Mac draws from a power adapter (or a UPS), not its battery.
    pub fn on_ac_power() -> bool {
        unsafe {
            let info = IOPSCopyPowerSourcesInfo();
            if info.is_null() {
                // A Mac with no power-source information is a desktop.
                return true;
            }
            let source = IOPSGetProvidingPowerSourceType(info);
            let mut buf = [0 as c_char; 64];
            let read = !source.is_null()
                && CFStringGetCString(source, buf.as_mut_ptr(), buf.len() as isize, UTF8) != 0;
            CFRelease(info);
            !read || CStr::from_ptr(buf.as_ptr()).to_bytes() != b"Battery Power"
        }
    }

    fn cf_string(s: &str) -> Option<CFStringRef> {
        let c = CString::new(s).ok()?;
        let r = unsafe { CFStringCreateWithCString(std::ptr::null(), c.as_ptr(), UTF8) };
        (!r.is_null()).then_some(r)
    }
}

/// What kind of machine this is, as far as remote power goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A Windows PC: Wake-on-LAN brings it back.
    Pc,
    /// A Mac: nothing in Latch can wake it once it sleeps.
    Mac,
    /// A Linux machine on its own hardware: someone at it can wake it.
    Linux,
    /// A Linux virtual machine (a cloud VPS): asleep, it has no wake
    /// source but its provider's console.
    LinuxVm,
    /// A container (the Docker node): there is no systemd to sleep,
    /// restart or power off anything.
    Container,
}

/// This machine's [`Kind`], looked at once.
pub fn kind() -> Kind {
    static KIND: std::sync::OnceLock<Kind> = std::sync::OnceLock::new();
    *KIND.get_or_init(|| {
        if cfg!(windows) {
            Kind::Pc
        } else if cfg!(target_os = "macos") {
            Kind::Mac
        } else {
            let exists = |p: &str| std::path::Path::new(p).exists();
            let vm = std::process::Command::new("systemd-detect-virt")
                .args(["--vm", "--quiet"])
                .status()
                .is_ok_and(|s| s.success());
            linux_kind(
                exists("/run/systemd/system"),
                exists("/.dockerenv") || exists("/run/.containerenv"),
                vm,
            )
        }
    })
}

fn linux_kind(systemd: bool, container: bool, vm: bool) -> Kind {
    if container || !systemd {
        Kind::Container
    } else if vm {
        Kind::LinuxVm
    } else {
        Kind::Linux
    }
}

/// Why `name`, a machine of `kind`, will not do `action` for another
/// machine; `None` when it will. Sleep is refused where nothing can wake
/// the machine again: it would drop off the tailnet until someone reached
/// it by hand (stuzha, an OCI VM in s2idle, would need the cloud console).
/// Shut down stays: the owner allowed it, and the Mac asks first.
pub fn refusal(kind: Kind, action: PowerAction, name: &str) -> Option<String> {
    match (kind, action) {
        (Kind::Container, _) => Some(format!(
            "{name} runs in a container, which cannot sleep, restart or shut down the machine it runs on"
        )),
        (Kind::LinuxVm, PowerAction::Sleep) => Some(format!(
            "{name} is a virtual machine: asleep, only its provider's console could wake it. Restart it instead"
        )),
        (Kind::Mac, PowerAction::Sleep) => Some(format!(
            "Latch cannot wake {name} once it sleeps, so it does not put it to sleep from here. Sleep it at the Mac"
        )),
        _ => None,
    }
}

/// On Linux, whether logind lets this service do `action`: `Some(answer)`
/// when it says anything but "yes" ("challenge" needs a password nobody
/// will type, "no" is never). `None` when it agrees or cannot be asked.
pub fn logind_refuses(action: PowerAction) -> Option<String> {
    if !cfg!(target_os = "linux") {
        return None;
    }
    let method = match action {
        PowerAction::Sleep => "CanSuspend",
        PowerAction::Restart => "CanReboot",
        PowerAction::Shutdown => "CanPowerOff",
    };
    let out = std::process::Command::new("busctl")
        .args([
            "--system",
            "call",
            "org.freedesktop.login1",
            "/org/freedesktop/login1",
            "org.freedesktop.login1.Manager",
            method,
        ])
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let reply = String::from_utf8_lossy(&out.stdout);
    let answer = logind_answer(&reply)?;
    (answer != "yes").then(|| answer.to_string())
}

/// `s "yes"`, busctl's way of printing a string reply, as `yes`.
fn logind_answer(reply: &str) -> Option<&str> {
    reply
        .trim()
        .strip_prefix("s ")?
        .trim()
        .strip_prefix('"')?
        .strip_suffix('"')
}

/// Carry out `action`. Sleep returns once the request is accepted (the OS
/// suspends a moment later); restart and shutdown schedule themselves a few
/// seconds out so the reply has left the machine. On a Mac, restart and
/// shutdown go through System Events and say why when macOS will not.
pub fn perform(action: PowerAction) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        use windows::Win32::Foundation::BOOLEAN;
        use windows::Win32::System::Power::SetSuspendState;
        match action {
            PowerAction::Sleep => {
                // Wake events stay enabled: that is what lets a magic packet
                // bring the machine back.
                let ok = unsafe { SetSuspendState(BOOLEAN(0), BOOLEAN(1), BOOLEAN(0)) };
                anyhow::ensure!(
                    ok.0 != 0,
                    "SetSuspendState refused: {}",
                    std::io::Error::last_os_error()
                );
                Ok(())
            }
            PowerAction::Restart | PowerAction::Shutdown => {
                let flag = if action == PowerAction::Restart {
                    "/r"
                } else {
                    "/s"
                };
                // /f: a remote user cannot answer a "save changes?" dialog, and
                // a PC left on with one open is the worst outcome.
                let status = std::process::Command::new("shutdown")
                    .args([flag, "/f", "/t", "3"])
                    .creation_flags(0x0800_0000)
                    .status()?;
                anyhow::ensure!(status.success(), "shutdown.exe exited with {status}");
                Ok(())
            }
        }
    }
    #[cfg(not(windows))]
    unix_perform(action)
}

#[cfg(not(windows))]
fn unix_perform(action: PowerAction) -> Result<()> {
    match action {
        PowerAction::Sleep => {
            #[cfg(target_os = "macos")]
            {
                let status = std::process::Command::new("pmset")
                    .arg("sleepnow")
                    .status()?;
                anyhow::ensure!(status.success(), "pmset sleepnow failed");
                Ok(())
            }
            #[cfg(target_os = "linux")]
            {
                let status = std::process::Command::new("systemctl")
                    .arg("suspend")
                    .status()?;
                anyhow::ensure!(status.success(), "systemctl suspend failed");
                Ok(())
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            anyhow::bail!("sleep is not supported on this OS")
        }
        PowerAction::Restart | PowerAction::Shutdown => {
            #[cfg(target_os = "macos")]
            {
                let apple = if action == PowerAction::Restart {
                    "restart"
                } else {
                    "shut down"
                };
                system_events(apple, Duration::from_secs(3))
            }
            #[cfg(target_os = "linux")]
            {
                let unit = if action == PowerAction::Restart {
                    "reboot"
                } else {
                    "poweroff"
                };
                let status = std::process::Command::new("systemctl").arg(unit).status()?;
                anyhow::ensure!(status.success(), "systemctl {unit} failed");
                Ok(())
            }
            #[cfg(not(any(target_os = "macos", target_os = "linux")))]
            anyhow::bail!("{} is not supported on this OS", action.label())
        }
    }
}

/// Ask System Events to `verb` the Mac, waiting at most `limit`. Sending it
/// Apple events needs the Automation permission, which macOS asks for on
/// the Mac's own screen, where nobody may be; and an app with unsaved work
/// can cancel the restart. Either way the Mac stays on, so say so.
#[cfg(target_os = "macos")]
fn system_events(verb: &str, limit: Duration) -> Result<()> {
    use std::process::{Command, Stdio};
    let mut child = Command::new("osascript")
        .args([
            "-e",
            &format!("tell application \"System Events\" to {verb}"),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let start = Instant::now();
    loop {
        if child.try_wait()?.is_some() {
            break;
        }
        if start.elapsed() >= limit {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("{}", osascript_failure("-1712"));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let out = child.wait_with_output()?;
    anyhow::ensure!(
        out.status.success(),
        "{}",
        osascript_failure(&String::from_utf8_lossy(&out.stderr))
    );
    Ok(())
}

/// What an osascript error means for the person who asked.
#[cfg(any(target_os = "macos", test))]
fn osascript_failure(stderr: &str) -> String {
    const SETTINGS: &str = "System Settings > Privacy & Security > Automation";
    if stderr.contains("-1743") {
        format!("Latch is not allowed to control System Events on this Mac; allow it in {SETTINGS}")
    } else if stderr.contains("-1712") {
        format!("macOS is asking on the Mac's screen whether Latch may control System Events, and nobody answered; allow it in {SETTINGS}")
    } else if stderr.contains("-128") {
        "an app on the Mac cancelled it, likely over unsaved work".into()
    } else {
        format!(
            "osascript: {}",
            stderr.trim().lines().next().unwrap_or("failed")
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sleep_is_refused_where_nothing_could_wake_the_machine() {
        use PowerAction::*;
        assert!(refusal(Kind::Pc, Sleep, "PC").is_none());
        assert!(refusal(Kind::Linux, Sleep, "box").is_none());
        let vm = refusal(Kind::LinuxVm, Sleep, "stuzha").unwrap();
        assert!(vm.contains("stuzha is a virtual machine"), "{vm}");
        assert!(refusal(Kind::LinuxVm, Restart, "stuzha").is_none());
        assert!(refusal(Kind::LinuxVm, Shutdown, "stuzha").is_none());
        assert!(refusal(Kind::Mac, Sleep, "MacBook").is_some());
        assert!(refusal(Kind::Mac, Restart, "MacBook").is_none());
        for a in [Sleep, Restart, Shutdown] {
            assert!(refusal(Kind::Container, a, "node").is_some());
        }
        assert_eq!(linux_kind(true, false, true), Kind::LinuxVm);
        assert_eq!(linux_kind(true, false, false), Kind::Linux);
        assert_eq!(linux_kind(false, false, false), Kind::Container);
        assert_eq!(linux_kind(true, true, false), Kind::Container);
    }

    /// IOKit is reached without a crate; this is the check that the
    /// declarations match it. The assertion is held for a moment only.
    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "takes a real power assertion; run on demand on a Mac"]
    fn a_mac_holds_and_lets_go_of_its_assertion() {
        let _ = mac::on_ac_power();
        assert!(mac::keep_awake(true));
        assert!(mac::keep_awake(true), "held once, not again");
        assert!(!mac::keep_awake(false));
    }

    #[test]
    fn logind_and_osascript_answers_are_read() {
        assert_eq!(logind_answer("s \"yes\"\n"), Some("yes"));
        assert_eq!(logind_answer("s \"challenge\""), Some("challenge"));
        assert_eq!(logind_answer("garbage"), None);
        assert!(osascript_failure(
            "0:46: execution error: Not authorized to send Apple events to System Events. (-1743)"
        )
        .contains("not allowed"));
        assert!(osascript_failure("execution error: User canceled. (-128)").contains("cancelled"));
        assert!(osascript_failure("-1712").contains("nobody answered"));
        assert_eq!(osascript_failure("boom\nmore"), "osascript: boom");
    }
}
