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
