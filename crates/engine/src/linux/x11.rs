use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixStream;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use x11rb::rust_connection::{DefaultStream, PollMode, RustConnection, Stream};
use x11rb::utils::RawFdContainer;

/// x11rb's own stream, except that a wait for the server gives up after
/// `silence` without a byte. Every round trip waits here, so the request
/// then fails with a connection error instead of hanging.
pub(super) struct Bounded {
    pub(super) stream: DefaultStream,
    pub(super) silence: Duration,
    pub(super) deadline: std::sync::Mutex<Option<Instant>>,
}

impl Bounded {
    fn remaining(&self) -> io::Result<Duration> {
        let deadline = *self.deadline.lock().unwrap();
        match deadline {
            Some(deadline) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "X11 initialization timed out",
                    ));
                }
                Ok(remaining.min(self.silence))
            }
            None => Ok(self.silence),
        }
    }
}

impl Stream for Bounded {
    fn poll(&self, mode: PollMode) -> io::Result<()> {
        let mut events = 0;
        if mode.readable() {
            events |= libc::POLLIN;
        }
        if mode.writable() {
            events |= libc::POLLOUT;
        }
        if wait_fd(self.stream.as_raw_fd(), events, self.remaining()?)? {
            Ok(())
        } else {
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("the X server did not answer for {:?}", self.silence),
            ))
        }
    }
    fn read(&self, buf: &mut [u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.remaining()?;
        self.stream.read(buf, fds)
    }
    fn write(&self, buf: &[u8], fds: &mut Vec<RawFdContainer>) -> io::Result<usize> {
        self.remaining()?;
        self.stream.write(buf, fds)
    }
    fn write_vectored(
        &self,
        bufs: &[io::IoSlice<'_>],
        fds: &mut Vec<RawFdContainer>,
    ) -> io::Result<usize> {
        self.remaining()?;
        self.stream.write_vectored(bufs, fds)
    }
}

impl AsRawFd for Bounded {
    fn as_raw_fd(&self) -> RawFd {
        self.stream.as_raw_fd()
    }
}

pub(super) type X11 = RustConnection<Bounded>;

/// One deadline covers DNS, socket connect, authority lookup and X11 setup.
pub(super) fn connect(silence: Duration) -> anyhow::Result<(X11, usize)> {
    let deadline = Instant::now() + silence;
    let mut command = std::process::Command::new(std::env::current_exe()?);
    command.arg(HELPER);
    connect_with(command, silence, deadline)
}

fn connect_with(
    command: Command,
    silence: Duration,
    deadline: Instant,
) -> anyhow::Result<(X11, usize)> {
    let (stream, screen, name, data) = bootstrap(command, deadline)?;
    let stream = Bounded {
        stream,
        silence,
        deadline: std::sync::Mutex::new(Some(deadline)),
    };
    let conn = RustConnection::connect_to_stream_with_auth_info(stream, screen, name, data)?;
    *conn.stream().deadline.lock().unwrap() = None;
    Ok((conn, screen))
}

/// Waits up to `timeout` for `events` on `fd`; false when none came.
pub(super) fn wait_fd(fd: RawFd, events: i16, timeout: Duration) -> io::Result<bool> {
    let deadline = Instant::now() + timeout;
    loop {
        let mut pollfd = libc::pollfd {
            fd,
            events,
            revents: 0,
        };
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(false);
        }
        let millis = remaining.as_millis().max(1).min(i32::MAX as u128) as i32;
        // SAFETY: pollfd is a live, initialized object for the duration of poll.
        let result = unsafe { libc::poll(&mut pollfd, 1, millis) };
        if result >= 0 {
            return Ok(result > 0);
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

const HELPER: &str = "--internal-x11-bootstrap";
type Bootstrap = (DefaultStream, usize, Vec<u8>, Vec<u8>);

/// Executables embedding the Linux engine must dispatch this before normal startup.
/// Cookies travel only over a private inherited socket, never stdout or arguments.
pub fn run_helper() -> bool {
    if std::env::args_os().nth(1).as_deref() != Some(std::ffi::OsStr::new(HELPER)) {
        return false;
    }
    let _ = helper();
    true
}

fn helper() -> anyhow::Result<()> {
    use x11rb::reexports::x11rb_protocol::{parse_display::parse_display, xauth::get_auth};
    // SAFETY: the dispatcher owns stdin, a socket inherited from bootstrap().
    let ipc = unsafe { UnixStream::from_raw_fd(0) };
    let (ipc, _) = DefaultStream::from_unix_stream(ipc)?;
    let display = parse_display(None)?;
    for address in display.connect_instruction() {
        if let Ok((stream, (family, peer))) = DefaultStream::connect(&address) {
            // Preserve x11rb's authority selection and absent-file behavior.
            let (name, data) = get_auth(family, &peer, display.display)
                .ok()
                .flatten()
                .unwrap_or_default();
            let payload = serde_json::to_vec(&(usize::from(display.screen), name, data))?;
            let mut fds = vec![OwnedFd::from(stream)];
            let mut bytes = payload.as_slice();
            while !bytes.is_empty() {
                match ipc.write(bytes, &mut fds) {
                    Ok(0) => anyhow::bail!("closed bootstrap socket"),
                    Ok(n) => bytes = &bytes[n..],
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => {
                        ipc.poll(PollMode::Writable)?
                    }
                    Err(e) => return Err(e.into()),
                }
            }
            return Ok(());
        }
    }
    anyhow::bail!("cannot connect to X11")
}

// A killed process in uninterruptible kernel I/O may not exit immediately.
// Keep at most one helper, refuse retries until it can be reaped, and never
// turn the initialization timeout into an unbounded waitpid.
static HELPER_SLOT: std::sync::Mutex<Option<std::process::Child>> = std::sync::Mutex::new(None);
struct Helper<'a>(std::sync::MutexGuard<'a, Option<std::process::Child>>);
impl Drop for Helper<'_> {
    fn drop(&mut self) {
        if let Some(child) = self.0.as_mut() {
            let _ = child.kill();
            let deadline = Instant::now() + Duration::from_millis(100);
            loop {
                if matches!(child.try_wait(), Ok(Some(_))) {
                    *self.0 = None;
                    break;
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        }
    }
}

fn bootstrap(mut command: Command, deadline: Instant) -> anyhow::Result<Bootstrap> {
    let mut slot = HELPER_SLOT.try_lock().map_err(|_| {
        anyhow::anyhow!("X11 initialization already in progress; retry after it completes")
    })?;
    if let Some(child) = slot.as_mut() {
        anyhow::ensure!(child.try_wait()?.is_some(), "previous X11 initializer is still exiting after timeout; check stalled filesystem/DNS and retry");
        *slot = None;
    }
    let (parent, child) = UnixStream::pair()?;
    let (ipc, _) = DefaultStream::from_unix_stream(parent)?;
    *slot = Some(
        command
            .stdin(Stdio::from(OwnedFd::from(child)))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?,
    );
    let _child = Helper(slot);
    drop(command); // Close the parent's copy of the child endpoint so EOF is observable.
    let mut payload = Vec::new();
    let mut fds = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        anyhow::ensure!(!remaining.is_zero() && wait_fd(ipc.as_raw_fd(), libc::POLLIN, remaining)?,
            "X11 initialization timed out (DISPLAY connect/DNS or XAUTHORITY read); check the X server and use a readable authority file");
        let mut buf = [0; 4096];
        match ipc.read(&mut buf, &mut fds) {
            Ok(0) => break,
            Ok(n) => payload.extend_from_slice(&buf[..n]),
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
            Err(e) => return Err(e.into()),
        }
        anyhow::ensure!(
            payload.len() <= 300_000 && fds.len() <= 1,
            "invalid X11 bootstrap response"
        );
    }
    anyhow::ensure!(fds.len() == 1 && !payload.is_empty(),
        "X11 bootstrap failed; check DISPLAY, XAUTHORITY and X server access; executables embedding latch-engine must dispatch linux::run_helper before startup");
    // Do not include serde errors: the payload contains an authentication cookie.
    let (screen, name, data): (usize, Vec<u8>, Vec<u8>) = serde_json::from_slice(&payload)
        .map_err(|_| anyhow::anyhow!("invalid X11 bootstrap response"))?;
    let (stream, _) = DefaultStream::from_unix_stream(UnixStream::from(fds.pop().unwrap()))?;
    Ok((stream, screen, name, data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::os::unix::net::UnixListener;
    use x11rb::protocol::xproto;
    use x11rb::x11_utils::Serialize;
    static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn helper_entry() {
        if std::env::var_os("LATCH_TEST_BOOTSTRAP").is_some() {
            helper().unwrap();
        }
        let _ = run_helper as fn() -> bool;
        let _ = connect as fn(Duration) -> anyhow::Result<(X11, usize)>;
    }

    fn command(display: &str, authority: &std::path::Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        let module = module_path!().split_once("::").unwrap().1;
        command
            .args(["--exact", &format!("{module}::helper_entry"), "--nocapture"])
            .env("LATCH_TEST_BOOTSTRAP", "1")
            .env("DISPLAY", display)
            .env("XAUTHORITY", authority);
        command
    }

    fn setup(socket: &mut impl Write) {
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
    }

    #[test]
    fn local_socket_transfers_selected_cookie_and_remains_usable() {
        let _test = TEST_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("latch-bootstrap-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let number = (40000 + std::process::id() % 10000).to_string();
        std::fs::create_dir_all("/tmp/.X11-unix").unwrap();
        let path = std::path::PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
        let auth = dir.join("authority");
        let listener = UnixListener::bind(&path).unwrap();
        // Wrong display/method precede the matching wildcard entry. x11rb must select it.
        let mut entries = Vec::new();
        for (number, name, cookie) in [
            (
                b"9".as_slice(),
                b"MIT-MAGIC-COOKIE-1".as_slice(),
                b"wrong".as_slice(),
            ),
            (number.as_bytes(), b"OTHER", b"wrong"),
            (number.as_bytes(), b"MIT-MAGIC-COOKIE-1", b"selected-cookie"),
        ] {
            entries.extend_from_slice(&65535u16.to_be_bytes());
            for field in [b"".as_slice(), number, name, cookie] {
                entries.extend_from_slice(&(field.len() as u16).to_be_bytes());
                entries.extend_from_slice(field);
            }
        }
        std::fs::write(&auth, entries).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut header = [0; 12];
            socket.read_exact(&mut header).unwrap();
            let name_len = u16::from_ne_bytes([header[6], header[7]]) as usize;
            let data_len = u16::from_ne_bytes([header[8], header[9]]) as usize;
            let mut name = vec![0; (name_len + 3) & !3];
            let mut data = vec![0; (data_len + 3) & !3];
            socket.read_exact(&mut name).unwrap();
            socket.read_exact(&mut data).unwrap();
            assert_eq!(&name[..name_len], b"MIT-MAGIC-COOKIE-1");
            assert_eq!(&data[..data_len], b"selected-cookie");
            setup(&mut socket);
            let mut request = [0; 4];
            socket.read_exact(&mut request).unwrap();
            assert_eq!(request[0], 43); // GetInputFocus after helper has exited.
            let mut reply = xproto::GetInputFocusReply {
                sequence: 1,
                ..Default::default()
            }
            .serialize()
            .to_vec();
            reply.resize(32, 0);
            socket.write_all(&reply).unwrap();
            let mut eof = [0];
            assert_eq!(socket.read(&mut eof).unwrap(), 0);
        });
        let timeout = Duration::from_secs(10);
        let (conn, screen) = connect_with(
            command(&format!("unix/:{number}"), &auth),
            timeout,
            Instant::now() + timeout,
        )
        .unwrap();
        assert_eq!(screen, 0);
        xproto::ConnectionExt::get_input_focus(&conn)
            .unwrap()
            .reply()
            .unwrap();
        drop(conn);
        server.join().unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn silent_setup_and_fifo_authority_fail_and_close_the_actual_socket() {
        let _test = TEST_LOCK.lock().unwrap();
        for fifo in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let path = std::env::temp_dir().join(format!("latch-auth-fifo-{}", std::process::id()));
            if fifo {
                let path_c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).unwrap();
                // SAFETY: path_c is a NUL-terminated live string.
                assert_eq!(unsafe { libc::mkfifo(path_c.as_ptr(), 0o600) }, 0);
            }
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(10)))
                    .unwrap();
                let mut all = Vec::new();
                socket.read_to_end(&mut all).unwrap();
                if fifo {
                    assert!(all.is_empty(), "authority blocks before setup");
                } else {
                    assert!(all.len() >= 12, "server received setup but never answered");
                }
            });
            let timeout = Duration::from_secs(1);
            let error = match connect_with(
                command(
                    &format!("127.0.0.1:{}", port - 6000),
                    if fifo {
                        &path
                    } else {
                        std::path::Path::new("/dev/null")
                    },
                ),
                timeout,
                Instant::now() + timeout,
            ) {
                Ok(_) => panic!("silent initialization succeeded"),
                Err(e) => e,
            };
            assert!(
                format!("{error:#}").contains(if fifo { "XAUTHORITY" } else { "X server" }),
                "{error:#}"
            );
            server.join().unwrap(); // EOF proves no helper retains the socket.
            if fifo {
                std::fs::remove_file(path).unwrap();
            }
        }
    }
    #[test]
    fn helper_guard_kills_and_reaps_on_failure() {
        let _test = TEST_LOCK.lock().unwrap();
        let child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        let pid = child.id() as libc::pid_t;
        let slot = std::sync::Mutex::new(Some(child));
        drop(Helper(slot.lock().unwrap()));
        // SAFETY: waitpid with WNOHANG writes only to this live status value.
        let mut status = 0;
        assert_eq!(
            unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) },
            -1
        );
        assert_eq!(
            io::Error::last_os_error().raw_os_error(),
            Some(libc::ECHILD)
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn abstract_socket_success_and_full_connect_queue_timeout() {
        let _test = TEST_LOCK.lock().unwrap();
        use std::os::linux::net::SocketAddrExt;
        use std::os::unix::net::SocketAddr;
        let number = 50000 + std::process::id() % 5000;
        let address = SocketAddr::from_abstract_name(format!("/tmp/.X11-unix/X{number}")).unwrap();
        let listener = UnixListener::bind_addr(&address).unwrap();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut header = [0; 12];
            socket.read_exact(&mut header).unwrap();
            setup(&mut socket);
            let mut eof = [0];
            assert_eq!(socket.read(&mut eof).unwrap(), 0);
            listener
        });
        let timeout = Duration::from_secs(10);
        let (conn, _) = connect_with(
            command(
                &format!("unix/:{number}"),
                std::path::Path::new("/dev/null"),
            ),
            timeout,
            Instant::now() + timeout,
        )
        .unwrap();
        drop(conn);
        let listener = server.join().unwrap();
        // A zero backlog still admits one connection on Linux. Fill it before
        // starting the helper: its blocking Unix connect must be killed.
        // SAFETY: listener owns a live socket descriptor.
        assert_eq!(unsafe { libc::listen(listener.as_raw_fd(), 0) }, 0);
        let _queued = UnixStream::connect_addr(&address).unwrap();
        let timeout = Duration::from_secs(1);
        let error = bootstrap(
            command(
                &format!("unix/:{number}"),
                std::path::Path::new("/dev/null"),
            ),
            Instant::now() + timeout,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("DISPLAY connect/DNS"));
    }
    #[test]
    fn partial_setup_bytes_do_not_reset_the_initialization_deadline() {
        let _test = TEST_LOCK.lock().unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (mut socket, _) = listener.accept().unwrap();
            socket
                .set_read_timeout(Some(Duration::from_secs(10)))
                .unwrap();
            let mut request = [0; 12];
            socket.read_exact(&mut request).unwrap();
            let mut bytes = Vec::new();
            setup(&mut bytes);
            for byte in bytes {
                if socket.write_all(&[byte]).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        });
        let timeout = Duration::from_secs(1);
        let error = connect_with(
            command(
                &format!("127.0.0.1:{}", port - 6000),
                std::path::Path::new("/dev/null"),
            ),
            timeout,
            Instant::now() + timeout,
        )
        .err()
        .unwrap();
        assert!(error.downcast_ref::<x11rb::errors::ConnectError>().is_some_and(|e| matches!(e, x11rb::errors::ConnectError::IoError(e) if e.kind() == io::ErrorKind::TimedOut)), "{error:#}");
        server.join().unwrap();
    }

    #[test]
    fn unreaped_helper_prevents_another_spawn() {
        let _test = TEST_LOCK.lock().unwrap();
        let child = Command::new("/bin/sh")
            .args(["-c", "exec sleep 60"])
            .spawn()
            .unwrap();
        *HELPER_SLOT.lock().unwrap() = Some(child);
        let error = bootstrap(
            Command::new("/nonexistent-must-not-spawn"),
            Instant::now() + Duration::from_secs(1),
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("previous X11 initializer"));
        drop(Helper(HELPER_SLOT.lock().unwrap()));
        assert!(HELPER_SLOT.lock().unwrap().is_none());
    }
}
