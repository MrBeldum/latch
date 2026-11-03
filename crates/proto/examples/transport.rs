//! Codec-free production packetizer/socket/reassembler benchmark.
//! transport receive <bind:port> | send <peer:port> <Mbps|0=max> <seconds> [fps]
use latch_proto::{
    transport,
    video::{OutFrame, Packetizer, Reassembled, Reassembler},
    wire,
};
use std::{
    io,
    net::UdpSocket,
    time::{Duration, Instant},
};

#[cfg(unix)]
fn cpu() -> f64 {
    // SAFETY: getrusage writes one initialized rusage value.
    unsafe {
        let mut r: libc::rusage = std::mem::zeroed();
        libc::getrusage(libc::RUSAGE_SELF, &mut r);
        (r.ru_utime.tv_sec + r.ru_stime.tv_sec) as f64
            + (r.ru_utime.tv_usec + r.ru_stime.tv_usec) as f64 / 1e6
    }
}
#[cfg(not(unix))]
fn cpu() -> f64 {
    0.0
}

fn main() -> io::Result<()> {
    let args: Vec<_> = std::env::args().collect();
    let receiving = args.get(1).is_some_and(|a| a == "receive");
    let peer = args
        .get(2)
        .expect("receive bind:port | send peer:port Mbps seconds [fps]");
    let remote: std::net::SocketAddr = peer.parse().unwrap();
    let send_bind = if remote.ip().is_loopback() {
        "0.0.0.0:0".into()
    } else {
        format!("0.0.0.0:{}", remote.port())
    };
    let udp = UdpSocket::bind(if receiving { peer.as_str() } else { &send_bind })?;
    let buffers = transport::configure_socket(&udp, 8 << 20, 8 << 20)?;
    eprintln!("buffers {buffers:?}");
    #[cfg(target_os = "macos")]
    {
        unsafe extern "C" {
            fn pthread_set_qos_class_self_np(class: u32, relative: i32) -> i32;
        }
        // SAFETY: raises only this benchmark thread, as production media threads do.
        unsafe {
            pthread_set_qos_class_self_np(0x21, 0);
        }
    }
    let cpu_start = cpu();
    let mut packets = 0u64;
    let mut bytes = 0u64;
    let mut frames = 0u64;
    let mut errors = 0u64;
    let start;
    if receiving {
        udp.set_nonblocking(true)?;
        let mut batch = transport::ReceiveBatch::default();
        let mut rx = Reassembler::new(1);
        let mut first = None;
        let waiting = Instant::now();
        let mut last = waiting;
        let mut punched = waiting - Duration::from_secs(1);
        loop {
            if first.is_none() && punched.elapsed() >= Duration::from_millis(100) {
                if let Some(peer) = args.get(3) {
                    let _ = udp.send_to(b"probe", peer);
                }
                punched = Instant::now();
            }
            let mut any = false;
            batch
                .receive(&udp, |dg, _| {
                    any = true;
                    let now = Instant::now();
                    first.get_or_insert(now);
                    packets += 1;
                    if let Ok(wire::Datagram::Video { shard, .. }) = wire::parse(dg) {
                        rx.push(&shard, now);
                        while let Some(event) = rx.poll(now) {
                            if let Reassembled::Frame(f) = event {
                                bytes += f.data.len() as u64;
                                frames += 1;
                                std::hint::black_box(f);
                            }
                        }
                    }
                })
                .or_else(|e| {
                    if e.kind() == io::ErrorKind::WouldBlock {
                        Ok(0)
                    } else {
                        Err(e)
                    }
                })?;
            if any {
                last = Instant::now();
            } else {
                if first.is_some() && last.elapsed() >= Duration::from_millis(300) {
                    break;
                }
                if first.is_none() && waiting.elapsed().as_secs() > 15 {
                    return Err(io::ErrorKind::TimedOut.into());
                }
                #[cfg(unix)]
                {
                    use std::os::fd::AsRawFd;
                    let mut fd = libc::pollfd {
                        fd: udp.as_raw_fd(),
                        events: libc::POLLIN,
                        revents: 0,
                    };
                    // SAFETY: poll receives a valid pollfd for this open socket.
                    unsafe {
                        libc::poll(&mut fd, 1, 10);
                    }
                }
                #[cfg(not(unix))]
                std::thread::sleep(Duration::from_micros(50));
            }
        }
        start = first.unwrap();
        eprintln!("reassembly {:?}", rx.stats());
    } else {
        let peer: std::net::SocketAddr = peer.parse().unwrap();
        let mbps: f64 = args[3].parse().unwrap();
        let seconds: f64 = args[4].parse().unwrap();
        let fps: f64 = args.get(5).map_or(120.0, |s| s.parse().unwrap());
        let size = if mbps == 0.0 {
            312500
        } else {
            (mbps * 1e6 / 8.0 / fps) as usize
        };
        let data: Vec<_> = (0..size)
            .map(|i| (i.wrapping_mul(131) >> 3) as u8)
            .collect();
        let mut tx = Packetizer::new(1);
        let frame = OutFrame {
            data: &data,
            keyframe: true,
            host_delay: 0,
        };
        start = Instant::now();
        while start.elapsed().as_secs_f64() < seconds {
            tx.packetize_batches(&frame, |batch| {
                let mut sent = 0;
                while sent < batch.len() {
                    match transport::send_batch(&udp, &batch[sent..], peer) {
                        Ok(n) => {
                            sent += n;
                            packets += n as u64;
                        }
                        Err(_) => {
                            errors += (batch.len() - sent) as u64;
                            break;
                        }
                    }
                }
            });
            frames += 1;
            bytes += size as u64;
            if mbps > 0.0 {
                let due = start + Duration::from_secs_f64(frames as f64 / fps);
                if let Some(wait) = due.checked_duration_since(Instant::now()) {
                    std::thread::sleep(wait);
                }
            }
        }
    }
    let elapsed = start.elapsed().as_secs_f64() - if receiving { 0.3 } else { 0.0 };
    let cpu_seconds = cpu() - cpu_start;
    let mbps = bytes as f64 * 8.0 / elapsed / 1e6;
    println!("{{\"receive\":{receiving},\"seconds\":{elapsed:.3},\"media_mbps\":{mbps:.3},\"pps\":{:.0},\"cpu_percent\":{:.2},\"cpu_us_per_Mbit\":{:.2},\"frames\":{frames},\"packets\":{packets},\"errors\":{errors}}}", packets as f64 / elapsed, cpu_seconds / elapsed * 100.0, cpu_seconds * 1e6 / (bytes as f64 * 8.0 / 1e6));
    Ok(())
}
