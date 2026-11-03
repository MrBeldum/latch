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

/// Sends a prefix, returning its datagram count. Callers must retry only the
/// unsent suffix. Linux amortizes the syscall with sendmmsg; other systems
/// retain send_to (Darwin's sendmsg_x requires a connected socket).
pub fn send_batch(
    udp: &UdpSocket,
    datagrams: &[Vec<u8>],
    to: std::net::SocketAddr,
) -> io::Result<usize> {
    if datagrams.is_empty() {
        return Ok(0);
    }
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
    buffers: Box<[[u8; crate::MAX_DATAGRAM + 1]; BATCH_SIZE]>,
}
impl Default for ReceiveBatch {
    fn default() -> Self {
        Self {
            buffers: Box::new([[0; crate::MAX_DATAGRAM + 1]; BATCH_SIZE]),
        }
    }
}
impl ReceiveBatch {
    pub fn receive(
        &mut self,
        udp: &UdpSocket,
        mut emit: impl FnMut(&[u8], std::net::SocketAddr),
    ) -> io::Result<usize> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use std::os::fd::AsRawFd;
            // SAFETY: buffers, iovecs, headers and sockaddr storage are disjoint
            // and live through the call. Lengths are reset before each receive.
            unsafe {
                let mut addresses: [socket2::SockAddrStorage; BATCH_SIZE] = std::mem::zeroed();
                let mut iov: [libc::iovec; BATCH_SIZE] = std::mem::zeroed();
                let mut headers: [BatchHeader; BATCH_SIZE] = std::mem::zeroed();
                for i in 0..BATCH_SIZE {
                    iov[i].iov_base = self.buffers[i].as_mut_ptr().cast();
                    iov[i].iov_len = self.buffers[i].len();
                    let h = header(&mut headers[i]);
                    h.msg_name = (&mut addresses[i] as *mut socket2::SockAddrStorage).cast();
                    h.msg_namelen = std::mem::size_of::<socket2::SockAddrStorage>() as _;
                    h.msg_iov = &mut iov[i];
                    h.msg_iovlen = 1;
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
                    if h.msg_flags & libc::MSG_TRUNC != 0 || len > crate::MAX_DATAGRAM {
                        continue;
                    }
                    if let Some(from) =
                        socket2::SockAddr::new(std::ptr::read(&addresses[i]), h.msg_namelen)
                            .as_socket()
                    {
                        emit(&self.buffers[i][..len], from);
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
            sent += send_batch(&tx, &packets[sent..], rx.local_addr().unwrap()).unwrap();
        }
        let mut batch = ReceiveBatch::default();
        let mut got = Vec::new();
        while got.len() < packets.len() {
            batch
                .receive(&rx, |bytes, from| {
                    assert_eq!(from, tx.local_addr().unwrap());
                    got.push(bytes.to_vec());
                })
                .unwrap();
        }
        assert_eq!(got, packets);
        assert_eq!(
            batch.receive(&rx, |_, _| panic!()).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }
}
