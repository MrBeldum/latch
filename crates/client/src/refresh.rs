//! Read the current display mode, never a list of supported (inactive) modes.
#[cfg(target_os = "macos")]
pub fn hz() -> Option<u32> {
    crate::macos::display_hz()
}

#[cfg(windows)]
pub fn hz() -> Option<u32> {
    use windows_sys::Win32::Graphics::Gdi::*;
    // The foreground window is ours when initiating a connection.
    let monitor = unsafe {
        MonitorFromWindow(
            windows_sys::Win32::UI::WindowsAndMessaging::GetForegroundWindow(),
            MONITOR_DEFAULTTOPRIMARY,
        )
    };
    let mut info: MONITORINFOEXW = unsafe { std::mem::zeroed() };
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    let mut mode: DEVMODEW = unsafe { std::mem::zeroed() };
    mode.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
    let ok = unsafe {
        GetMonitorInfoW(monitor, &mut info.monitorInfo) != 0
            && EnumDisplaySettingsW(info.szDevice.as_ptr(), ENUM_CURRENT_SETTINGS, &mut mode) != 0
    };
    (ok && mode.dmDisplayFrequency > 1).then_some(mode.dmDisplayFrequency)
}

#[cfg(target_os = "linux")]
pub fn hz() -> Option<u32> {
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return None;
    }
    let mut command = std::process::Command::new(std::env::current_exe().ok()?);
    command.arg("--internal-display-hz");
    query_child(command, std::time::Duration::from_secs(2))
}

/// Run before either executable initializes its UI or service. Isolation in a
/// killable process bounds DNS, socket connect, Xauthority reads and replies,
/// including libraries that offer no cancellation or socket deadline.
#[cfg(target_os = "linux")]
pub fn run_helper() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new("--internal-display-hz"))
    {
        return false;
    }
    if let Some(hz) = query_x11() {
        println!("latch-display-hz={hz}");
    }
    true
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn query_child(mut command: std::process::Command, timeout: std::time::Duration) -> Option<u32> {
    use std::process::Stdio;
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) => return None,
            Ok(None) if std::time::Instant::now() < deadline => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let output = child.wait_with_output().ok()?;
    String::from_utf8(output.stdout)
        .ok()?
        .lines()
        .find_map(|line| line.strip_prefix("latch-display-hz=")?.parse::<u32>().ok())
        .filter(|hz| *hz > 0)
}

#[cfg(any(target_os = "linux", all(test, unix)))]
fn query_x11() -> Option<u32> {
    use x11rb::{connection::Connection, protocol::randr::ConnectionExt};
    // Xwayland describes a virtual output, not the compositor's real mode.
    if std::env::var_os("WAYLAND_DISPLAY").is_some() {
        return None;
    }
    // Connection is initiated by a click on this display. Query in the worker,
    // never in update(): X11 replies can block.
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots.get(screen)?.root;
    let resources = conn
        .randr_get_screen_resources_current(root)
        .ok()?
        .reply()
        .ok()?;
    use x11rb::protocol::xproto::ConnectionExt as _;
    let pointer = conn.query_pointer(root).ok()?.reply().ok()?;
    let crtc = resources.crtcs.iter().find_map(|id| {
        let c = conn
            .randr_get_crtc_info(*id, resources.config_timestamp)
            .ok()?
            .reply()
            .ok()?;
        let x = i32::from(pointer.root_x) - i32::from(c.x);
        let y = i32::from(pointer.root_y) - i32::from(c.y);
        (c.mode != 0 && x >= 0 && y >= 0 && x < i32::from(c.width) && y < i32::from(c.height))
            .then_some(c)
    })?;
    let mode = resources.modes.iter().find(|m| m.id == crtc.mode)?;
    let pixels = u64::from(mode.htotal) * u64::from(mode.vtotal);
    (pixels > 0)
        .then(|| (u64::from(mode.dot_clock) / pixels) as u32)
        .filter(|hz| *hz > 0)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn x11_query_helper() {
        if std::env::var_os("LATCH_TEST_X11_HELPER").is_some() {
            if let Some(hz) = query_x11() {
                println!("latch-display-hz={hz}");
            }
        }
    }

    #[test]
    fn child_reads_the_active_randr_mode() {
        use std::io::{Read, Write};
        use x11rb::protocol::{randr, xproto};
        use x11rb::x11_utils::Serialize;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        assert!(port >= 6000);
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(std::time::Duration::from_secs(10)))
                .unwrap();
            let mut setup_request = [0; 12];
            socket.read_exact(&mut setup_request).unwrap();
            let mut setup = xproto::Setup {
                status: 1,
                protocol_major_version: 11,
                resource_id_mask: 0xffff,
                roots: vec![xproto::Screen {
                    root: 1,
                    ..Default::default()
                }],
                ..Default::default()
            };
            setup.length = ((setup.serialize().len() - 8) / 4) as u16;
            socket.write_all(&setup.serialize()).unwrap();
            let mut sequence = 0;
            loop {
                let mut header = [0; 4];
                if socket.read_exact(&mut header).is_err() {
                    break;
                }
                let len = usize::from(u16::from_ne_bytes([header[2], header[3]])) * 4;
                let mut body = vec![0; len - 4];
                socket.read_exact(&mut body).unwrap();
                sequence += 1;
                let mut reply = match (header[0], header[1]) {
                    (98, _) => xproto::QueryExtensionReply {
                        sequence,
                        present: true,
                        major_opcode: 140,
                        ..Default::default()
                    }
                    .serialize()
                    .to_vec(),
                    (140, 25) => {
                        let mut resources = randr::GetScreenResourcesCurrentReply {
                            sequence,
                            crtcs: vec![2],
                            modes: vec![
                                randr::ModeInfo {
                                    id: 3,
                                    dot_clock: 148_500_000,
                                    htotal: 2200,
                                    vtotal: 1125,
                                    ..Default::default()
                                },
                                randr::ModeInfo {
                                    id: 4,
                                    dot_clock: 594_000_000,
                                    htotal: 2200,
                                    vtotal: 1125,
                                    ..Default::default()
                                },
                            ],
                            ..Default::default()
                        };
                        resources.length = ((resources.serialize().len() - 32) / 4) as u32;
                        resources.serialize()
                    }
                    (38, _) => xproto::QueryPointerReply {
                        sequence,
                        root: 1,
                        root_x: 100,
                        root_y: 100,
                        ..Default::default()
                    }
                    .serialize()
                    .to_vec(),
                    (140, 20) => randr::GetCrtcInfoReply {
                        sequence,
                        mode: 3,
                        width: 1920,
                        height: 1080,
                        ..Default::default()
                    }
                    .serialize(),
                    other => panic!("unexpected X11 request {other:?}"),
                };
                reply.resize(reply.len().max(32), 0);
                if socket.write_all(&reply).is_err() {
                    break;
                }
            }
        });
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "refresh::tests::x11_query_helper", "--nocapture"])
            .env("LATCH_TEST_X11_HELPER", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env("DISPLAY", format!("127.0.0.1:{}", port - 6000))
            .env("XAUTHORITY", "/dev/null");
        assert_eq!(
            query_child(command, std::time::Duration::from_secs(10)),
            Some(60)
        );
        server.join().unwrap();
    }

    #[test]
    fn silent_x11_and_blocked_auth_queries_are_killed_and_reaped() {
        use std::io::Read;
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        for blocked_auth in [false, true] {
            // X11 TCP uses port 6000 + display number.
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            assert!(port >= 6000);
            let dir =
                std::env::temp_dir().join(format!("latch-xauth-{:016x}", rand::random::<u64>()));
            std::fs::create_dir(&dir).unwrap();
            let auth = dir.join("authority");
            if blocked_auth {
                assert!(std::process::Command::new("mkfifo")
                    .arg(&auth)
                    .status()
                    .unwrap()
                    .success());
            } else {
                std::fs::write(&auth, []).unwrap();
            }
            let (observed, received) = mpsc::channel();
            let (release, held) = mpsc::channel();
            let server = std::thread::spawn(move || {
                listener.set_nonblocking(true).unwrap();
                let deadline = Instant::now() + Duration::from_secs(10);
                loop {
                    match listener.accept() {
                        Ok((mut socket, _)) => {
                            if !blocked_auth {
                                socket
                                    .set_read_timeout(Some(Duration::from_secs(10)))
                                    .unwrap();
                                let mut setup = [0; 12];
                                socket.read_exact(&mut setup).unwrap();
                                assert!(setup[0] == b'l' || setup[0] == b'B');
                            }
                            observed.send(()).unwrap();
                            let _ = held.recv_timeout(Duration::from_secs(10));
                            break;
                        }
                        Err(e)
                            if e.kind() == std::io::ErrorKind::WouldBlock
                                && Instant::now() < deadline =>
                        {
                            std::thread::sleep(Duration::from_millis(10));
                        }
                        Err(e) => panic!("helper never connected: {e}"),
                    }
                }
            });
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command
                .args(["--exact", "refresh::tests::x11_query_helper", "--nocapture"])
                .env("LATCH_TEST_X11_HELPER", "1")
                .env_remove("WAYLAND_DISPLAY")
                .env("DISPLAY", format!("127.0.0.1:{}", port - 6000))
                .env("XAUTHORITY", &auth);
            let started = Instant::now();
            assert_eq!(query_child(command, Duration::from_secs(2)), None);
            received.recv_timeout(Duration::from_secs(10)).unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(10),
                "query failed to honor its deadline"
            );
            release.send(()).unwrap();
            server.join().unwrap();
            std::fs::remove_dir_all(dir).unwrap();
        }
    }
}
