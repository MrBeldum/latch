//! A timed UDP impairment relay shared by the real session harnesses.
use latch_proto::sim::{Impairment, Link};
use parking_lot::Mutex;
use std::io::{Read, Write};
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A relay on 127.0.0.1: TCP passed through as is, UDP through two
/// impaired links. Returns its port (the same number for TCP and UDP).
pub fn relay(
    host_port: u16,
    impairment: Impairment,
    queue_delay_ms: Arc<std::sync::atomic::AtomicU32>,
) -> u16 {
    let (listener, udp) = loop {
        // UDP picks the port, as in `host`.
        let u = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
        let port = u.local_addr().unwrap().port();
        if let Ok(l) = TcpListener::bind((Ipv4Addr::LOCALHOST, port)) {
            break (l, u);
        }
    };
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for client in listener.incoming().flatten() {
            let server = TcpStream::connect((Ipv4Addr::LOCALHOST, host_port)).unwrap();
            let pipe = |mut from: TcpStream, mut to: TcpStream| {
                std::thread::spawn(move || {
                    let mut buf = [0u8; 16384];
                    while let Ok(n) = from.read(&mut buf) {
                        if n == 0 || to.write_all(&buf[..n]).is_err() {
                            break;
                        }
                    }
                    let _ = to.shutdown(std::net::Shutdown::Both);
                });
            };
            pipe(client.try_clone().unwrap(), server.try_clone().unwrap());
            pipe(server, client);
        }
    });
    // Viewer → relay socket → host from a second socket, so the host sees
    // the relay as the viewer. Per direction, one thread blocks on its socket
    // and puts what arrives into the link; another sleeps until the link's
    // next datagram is due and sends it then. Releasing only when something
    // else arrives (or when a socket timeout fires, about 15 ms on Windows)
    // held pings behind a thin stream and looked like a queue that never
    // drained.
    let to_host = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
    let host_addr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), host_port);
    let viewer: Arc<Mutex<Option<SocketAddr>>> = Arc::default();
    let up = Arc::new(Mutex::new(Link::new(impairment, 7)));
    let down = Arc::new(Mutex::new(Link::new(impairment, 11)));
    {
        let (from_viewer, link, viewer) = (udp.try_clone().unwrap(), up.clone(), viewer.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok((n, from)) = from_viewer.recv_from(&mut buf) {
                *viewer.lock() = Some(from);
                link.lock().send(Instant::now(), &buf[..n]);
            }
        });
    }
    {
        let (from_host, link) = (to_host.try_clone().unwrap(), down.clone());
        std::thread::spawn(move || {
            let mut buf = [0u8; 2048];
            while let Ok((n, _)) = from_host.recv_from(&mut buf) {
                // Model standing cross-traffic queueing independently of the
                // encoder's offered rate. All downstream traffic, including
                // ping/pong, waits in the same queue; TCP reports stay untouched.
                let delay = Duration::from_millis(u64::from(
                    queue_delay_ms.load(std::sync::atomic::Ordering::Relaxed),
                ));
                link.lock().send(Instant::now() + delay, &buf[..n]);
            }
        });
    }
    let deliver = move |link: Arc<Mutex<Link>>,
                        out: UdpSocket,
                        to: Box<dyn Fn() -> Option<SocketAddr> + Send>| {
        move || loop {
            let now = Instant::now();
            let (due, next) = {
                let mut link = link.lock();
                (link.receive(now), link.next_arrival())
            };
            if let Some(to) = to() {
                for d in due {
                    let _ = out.send_to(&d, to);
                }
            }
            // std's sleep is high resolution on every platform this runs on.
            let wait = next.map_or(Duration::from_millis(1), |t| {
                t.saturating_duration_since(now)
            });
            std::thread::sleep(wait.clamp(Duration::from_micros(100), Duration::from_millis(1)));
        }
    };
    let up = deliver(up, to_host, Box::new(move || Some(host_addr)));
    let down = deliver(down, udp, Box::new(move || *viewer.lock()));
    std::thread::spawn(up);
    std::thread::spawn(down);
    port
}
