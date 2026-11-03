//! Shared socket configuration. This module is the protocol crate's only I/O helper.
use std::{io, net::UdpSocket};

#[derive(Debug, Clone, Copy, Default)]
pub struct SocketBuffers {
    pub send: usize,
    pub receive: usize,
}

pub fn configure_socket(udp: &UdpSocket, send: usize, receive: usize) -> io::Result<SocketBuffers> {
    let socket = socket2::SockRef::from(udp);
    // macOS may refuse a request above kern.ipc.maxsockbuf rather than clamp it.
    // Still request MiB buffers on systems that support them, then read back.
    for (wanted, sending) in [(send, true), (receive, false)] {
        let mut size = wanted;
        loop {
            let result = if sending {
                socket.set_send_buffer_size(size)
            } else {
                socket.set_recv_buffer_size(size)
            };
            match result {
                Ok(()) => break,
                Err(_) if size > 256 * 1024 => size /= 2,
                Err(e) => return Err(e),
            }
        }
    }
    Ok(SocketBuffers {
        send: socket.send_buffer_size()?,
        receive: socket.recv_buffer_size()?,
    })
}

pub fn send_buffer_full(error: &io::Error) -> bool {
    #[cfg(unix)]
    let enobufs = libc::ENOBUFS;
    #[cfg(windows)]
    let enobufs = 10055; // WSAENOBUFS
    error.kind() == io::ErrorKind::WouldBlock || error.raw_os_error() == Some(enobufs)
}

/// Batches never wait to fill: the packetizer flushes at data/parity boundaries.
pub const BATCH_SIZE: usize = 32;

/// One sender per socket/thread. Linux segments equal-sized runs with
/// UDP_SEGMENT (GSO), Windows with UDP_SEND_MSG_SIZE (USO); a run is one
/// syscall that the stack or NIC splits into ordinary datagrams, so receivers
/// need no capability. A kernel or driver that rejects segmentation moves this
/// sender to the ordinary batch path for good; WouldBlock/ENOBUFS remain
/// caller retries, and other errors retry that batch unsegmented.
/// `LATCH_UDP_OFFLOAD=0` disables segmentation and receive coalescing.
pub struct SendBatch {
    #[cfg(any(target_os = "linux", windows))]
    offload: Option<offload::Sender>,
    pub segmented_batches: u64,
}
impl Default for SendBatch {
    fn default() -> Self {
        Self::with_offload(offload_enabled())
    }
}
impl SendBatch {
    pub fn with_offload(enabled: bool) -> Self {
        let _ = enabled;
        Self {
            #[cfg(any(target_os = "linux", windows))]
            offload: enabled.then(offload::Sender::default),
            segmented_batches: 0,
        }
    }
    pub fn send(
        &mut self,
        udp: &UdpSocket,
        datagrams: &[Vec<u8>],
        to: std::net::SocketAddr,
    ) -> io::Result<usize> {
        if datagrams.is_empty() {
            return Ok(0);
        }
        #[cfg(any(target_os = "linux", windows))]
        if let Some(sender) = self.offload.as_mut() {
            let count = segment_count(datagrams);
            if count > 1 {
                match sender.send(udp, &datagrams[..count], to) {
                    Ok(()) => {
                        self.segmented_batches += 1;
                        return Ok(count);
                    }
                    Err(e) if send_buffer_full(&e) => return Err(e),
                    Err(e) if offload::unsupported(&e) => self.offload = None,
                    Err(_) => {}
                }
            }
        }
        send_plain(udp, datagrams, to)
    }
}

fn offload_enabled() -> bool {
    std::env::var_os("LATCH_UDP_OFFLOAD").is_none_or(|v| v != "0")
}

#[cfg(any(target_os = "linux", windows))]
fn segment_count(datagrams: &[Vec<u8>]) -> usize {
    let size = datagrams[0].len();
    if size == 0 || size > crate::MAX_DATAGRAM {
        return 0;
    }
    let mut count = 0;
    for dg in datagrams.iter().take(BATCH_SIZE) {
        // One segmented send carries at most 64 KiB of payload.
        if dg.is_empty() || dg.len() > size || (count + 1) * size > 65_000 {
            break;
        }
        count += 1;
        if dg.len() < size {
            break;
        }
    }
    count
}

fn send_plain(
    udp: &UdpSocket,
    datagrams: &[Vec<u8>],
    to: std::net::SocketAddr,
) -> io::Result<usize> {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        let address = socket2::SockAddr::from(to);
        // SAFETY: all pointers refer to live slices/address storage for the
        // duration of sendmmsg. The kernel only reads payloads and iovecs.
        unsafe {
            let mut iov: [libc::iovec; BATCH_SIZE] = std::mem::zeroed();
            let mut headers: [libc::mmsghdr; BATCH_SIZE] = std::mem::zeroed();
            let count = datagrams.len().min(BATCH_SIZE);
            for i in 0..count {
                iov[i].iov_base = datagrams[i].as_ptr().cast_mut().cast();
                iov[i].iov_len = datagrams[i].len();
                headers[i].msg_hdr.msg_name = address.as_ptr().cast_mut().cast();
                headers[i].msg_hdr.msg_namelen = address.len();
                headers[i].msg_hdr.msg_iov = &mut iov[i];
                headers[i].msg_hdr.msg_iovlen = 1;
            }
            let n = libc::sendmmsg(
                udp.as_raw_fd(),
                headers.as_mut_ptr(),
                count as u32,
                libc::MSG_DONTWAIT,
            );
            if n < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(n as usize)
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let mut sent = 0;
        for datagram in datagrams {
            match udp.send_to(datagram, to) {
                Ok(_) => sent += 1,
                Err(_) if sent > 0 => break,
                Err(e) => return Err(e),
            }
        }
        Ok(sent)
    }
}

/// Reusable receive storage. Nonblocking batches return immediately with
/// whatever is available; neither recvmmsg nor recvmsg_x waits for a full batch.
pub struct ReceiveBatch {
    buffers: Vec<Vec<u8>>,
    #[cfg(any(target_os = "linux", windows))]
    offload: offload::Receiver,
    pub coalesced_datagrams: u64,
}
impl Default for ReceiveBatch {
    fn default() -> Self {
        Self::with_offload(offload_enabled())
    }
}
impl ReceiveBatch {
    pub fn with_offload(enabled: bool) -> Self {
        let _ = enabled;
        let size = if cfg!(any(target_os = "linux", windows)) {
            65536
        } else {
            crate::MAX_DATAGRAM + 1
        };
        Self {
            buffers: (0..BATCH_SIZE).map(|_| vec![0; size]).collect(),
            #[cfg(any(target_os = "linux", windows))]
            offload: offload::Receiver::new(enabled),
            coalesced_datagrams: 0,
        }
    }
    pub fn receive(
        &mut self,
        udp: &UdpSocket,
        mut emit: impl FnMut(&[u8], std::net::SocketAddr),
    ) -> io::Result<usize> {
        #[cfg(windows)]
        if let Some(result) = self.offload.receive(
            udp,
            &mut self.buffers[0],
            &mut self.coalesced_datagrams,
            &mut emit,
        ) {
            return result;
        }
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::fd::AsRawFd;
            #[cfg(target_os = "linux")]
            self.offload.configure(udp);
            // SAFETY: buffers, iovecs, headers and sockaddr storage are disjoint
            // and live through the call. Lengths are reset before each receive.
            unsafe {
                let mut addresses: [socket2::SockAddrStorage; BATCH_SIZE] = std::mem::zeroed();
                let mut iov: [libc::iovec; BATCH_SIZE] = std::mem::zeroed();
                let mut headers: [BatchHeader; BATCH_SIZE] = std::mem::zeroed();
                #[cfg(target_os = "linux")]
                let mut controls = [[0usize; 8]; BATCH_SIZE];
                for i in 0..BATCH_SIZE {
                    iov[i].iov_base = self.buffers[i].as_mut_ptr().cast();
                    iov[i].iov_len = self.buffers[i].len();
                    let h = header(&mut headers[i]);
                    h.msg_name = (&mut addresses[i] as *mut socket2::SockAddrStorage).cast();
                    h.msg_namelen = std::mem::size_of::<socket2::SockAddrStorage>() as _;
                    h.msg_iov = &mut iov[i];
                    h.msg_iovlen = 1;
                    #[cfg(target_os = "linux")]
                    {
                        h.msg_control = controls[i].as_mut_ptr().cast();
                        h.msg_controllen = std::mem::size_of_val(&controls[i]);
                    }
                }
                #[cfg(target_os = "linux")]
                let n = libc::recvmmsg(
                    udp.as_raw_fd(),
                    headers.as_mut_ptr(),
                    BATCH_SIZE as u32,
                    libc::MSG_DONTWAIT,
                    std::ptr::null_mut(),
                ) as isize;
                #[cfg(target_os = "macos")]
                let n = recvmsg_x(
                    udp.as_raw_fd(),
                    headers.as_mut_ptr(),
                    BATCH_SIZE as u32,
                    libc::MSG_DONTWAIT,
                );
                if n < 0 {
                    let error = io::Error::last_os_error();
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::ENOSYS) | Some(libc::EOPNOTSUPP)
                    ) {
                        let (len, from) = udp.recv_from(&mut self.buffers[0])?;
                        if len <= crate::MAX_DATAGRAM {
                            emit(&self.buffers[0][..len], from);
                        }
                        return Ok(1);
                    }
                    return Err(error);
                }
                for i in 0..n as usize {
                    #[cfg(target_os = "linux")]
                    let len = headers[i].msg_len as usize;
                    #[cfg(target_os = "macos")]
                    let len = headers[i].data_len;
                    let h = header(&mut headers[i]);
                    if h.msg_flags & (libc::MSG_TRUNC | libc::MSG_CTRUNC) != 0 {
                        continue;
                    }
                    if let Some(from) =
                        socket2::SockAddr::new(std::ptr::read(&addresses[i]), h.msg_namelen)
                            .as_socket()
                    {
                        #[cfg(target_os = "linux")]
                        let segment = offload::segment_size(h);
                        #[cfg(target_os = "macos")]
                        let segment = None;
                        emit_segments(
                            &self.buffers[i][..len],
                            from,
                            segment,
                            &mut self.coalesced_datagrams,
                            &mut emit,
                        );
                    }
                }
                Ok(n as usize)
            }
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let mut count = 0;
            for buf in self.buffers.iter_mut() {
                match udp.recv_from(buf) {
                    Ok((n, from)) => {
                        count += 1;
                        if n <= crate::MAX_DATAGRAM {
                            emit(&buf[..n], from);
                        }
                    }
                    Err(_) if count > 0 => break,
                    Err(e) => return Err(e),
                }
            }
            Ok(count)
        }
    }
}
#[cfg(target_os = "linux")]
type BatchHeader = libc::mmsghdr;
#[cfg(target_os = "linux")]
fn header(h: &mut BatchHeader) -> &mut libc::msghdr {
    &mut h.msg_hdr
}

// Darwin's private ABI, from XNU bsd/sys/socket_private.h. msghdr_x is
// msghdr followed by size_t msg_datalen. Only receive uses this API: send
// disallows destination addresses, incompatible with the shared host socket.
#[cfg(target_os = "macos")]
#[repr(C)]
struct BatchHeader {
    hdr: libc::msghdr,
    data_len: usize,
}
#[cfg(target_os = "macos")]
fn header(h: &mut BatchHeader) -> &mut libc::msghdr {
    &mut h.hdr
}
#[cfg(target_os = "macos")]
unsafe extern "C" {
    fn recvmsg_x(
        fd: libc::c_int,
        headers: *mut BatchHeader,
        count: libc::c_uint,
        flags: libc::c_int,
    ) -> isize;
}

#[cfg(target_os = "linux")]
#[path = "transport/linux.rs"]
mod offload;
#[cfg(windows)]
#[path = "transport/windows.rs"]
mod offload;

fn emit_segments(
    bytes: &[u8],
    from: std::net::SocketAddr,
    segment: Option<usize>,
    coalesced: &mut u64,
    mut emit: impl FnMut(&[u8], std::net::SocketAddr),
) {
    let size = segment.unwrap_or(bytes.len());
    if size == 0 {
        if segment.is_none() {
            emit(bytes, from);
        }
        return;
    }
    if size > crate::MAX_DATAGRAM {
        return;
    }
    if segment.is_some() && bytes.len() > size {
        *coalesced += bytes.len().div_ceil(size) as u64;
    }
    for dg in bytes.chunks(size) {
        emit(dg, from);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn batches_preserve_boundaries_and_never_wait_for_more() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_nonblocking(true).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let packets: Vec<_> = (1..=BATCH_SIZE).map(|n| vec![n as u8; n]).collect();
        let mut sent = 0;
        while sent < packets.len() {
            sent += SendBatch::default()
                .send(&tx, &packets[sent..], rx.local_addr().unwrap())
                .unwrap();
        }
        let mut batch = ReceiveBatch::default();
        let mut got = Vec::new();
        // A busy machine may not have delivered loopback datagrams yet.
        while got.len() < packets.len() {
            match batch.receive(&rx, |bytes, from| {
                assert_eq!(from, tx.local_addr().unwrap());
                got.push(bytes.to_vec());
            }) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => std::thread::yield_now(),
                result => {
                    result.unwrap();
                }
            }
        }
        assert_eq!(got, packets);
        assert_eq!(
            batch.receive(&rx, |_, _| panic!()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn segmented_runs_arrive_as_separate_datagrams() {
        let rx = UdpSocket::bind("127.0.0.1:0").unwrap();
        rx.set_nonblocking(true).unwrap();
        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        // Two full runs of equal datagrams, each ending in a short one.
        let mut packets: Vec<_> = (0..40).map(|n| vec![n as u8; 1200]).collect();
        packets[BATCH_SIZE - 1].truncate(700);
        packets[39].truncate(9);
        let mut sender = SendBatch::with_offload(true);
        let mut sent = 0;
        while sent < packets.len() {
            sent += sender
                .send(&tx, &packets[sent..], rx.local_addr().unwrap())
                .unwrap();
        }
        let mut batch = ReceiveBatch::with_offload(true);
        let mut got = Vec::new();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while got.len() < packets.len() && std::time::Instant::now() < deadline {
            match batch.receive(&rx, |bytes, _| got.push(bytes.to_vec())) {
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                result => {
                    result.unwrap();
                }
            }
        }
        assert_eq!(got, packets);
        // Loopback always accepts UDP_SEGMENT; Windows USO depends on the build.
        if cfg!(target_os = "linux") {
            assert_eq!(sender.segmented_batches, 2);
        }
        eprintln!(
            "segmented batches {}, coalesced datagrams {}",
            sender.segmented_batches, batch.coalesced_datagrams
        );
    }
}
