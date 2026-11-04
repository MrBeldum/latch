use std::{
    io,
    net::{SocketAddr, UdpSocket},
    os::fd::AsRawFd,
};

/// EIO: no checksum offload on the route's device; the others: no GSO, or
/// segments larger than the route's MTU.
pub(super) fn unsupported(error: &io::Error) -> bool {
    matches!(
        error.raw_os_error(),
        Some(libc::EIO | libc::EINVAL | libc::EOPNOTSUPP | libc::ENOPROTOOPT | libc::EMSGSIZE)
    )
}

#[derive(Default)]
pub(super) struct Sender;
impl Sender {
    pub(super) fn send(
        &mut self,
        udp: &UdpSocket,
        datagrams: &[Vec<u8>],
        to: SocketAddr,
    ) -> io::Result<()> {
        send(udp, datagrams, to)
    }
}

fn send(udp: &UdpSocket, datagrams: &[Vec<u8>], to: SocketAddr) -> io::Result<()> {
    let address = socket2::SockAddr::from(to);
    // SAFETY: iovecs and aligned control/address storage live through sendmsg;
    // the kernel reads the concatenated payload and splits at the u16 size.
    unsafe {
        let mut iov: [libc::iovec; super::BATCH_SIZE] = std::mem::zeroed();
        let count = datagrams.len().min(super::BATCH_SIZE);
        for (iov, dg) in iov.iter_mut().zip(datagrams) {
            iov.iov_base = dg.as_ptr().cast_mut().cast();
            iov.iov_len = dg.len();
        }
        let mut control = [0usize; 8];
        let mut msg: libc::msghdr = std::mem::zeroed();
        msg.msg_name = address.as_ptr().cast_mut().cast();
        msg.msg_namelen = address.len();
        msg.msg_iov = iov.as_mut_ptr();
        msg.msg_iovlen = count;
        msg.msg_control = control.as_mut_ptr().cast();
        msg.msg_controllen = libc::CMSG_SPACE(2) as usize;
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        (*cmsg).cmsg_level = libc::IPPROTO_UDP;
        (*cmsg).cmsg_type = libc::UDP_SEGMENT;
        (*cmsg).cmsg_len = libc::CMSG_LEN(2) as usize;
        std::ptr::write_unaligned(
            libc::CMSG_DATA(cmsg).cast::<u16>(),
            datagrams[0].len() as u16,
        );
        if libc::sendmsg(udp.as_raw_fd(), &msg, libc::MSG_DONTWAIT) < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

pub(super) struct Receiver {
    enabled: bool,
    configured: bool,
}
impl Receiver {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            configured: false,
        }
    }
    pub(super) fn configure(&mut self, udp: &UdpSocket) {
        if self.configured {
            return;
        }
        self.configured = true;
        let enabled = i32::from(self.enabled);
        // SAFETY: setsockopt reads a live integer. Rejection leaves ordinary
        // datagrams on the same recvmmsg path; no wire capability is required.
        unsafe {
            libc::setsockopt(
                udp.as_raw_fd(),
                libc::IPPROTO_UDP,
                libc::UDP_GRO,
                (&enabled as *const i32).cast(),
                std::mem::size_of_val(&enabled) as _,
            );
        }
    }
}

pub(super) fn segment_size(msg: &libc::msghdr) -> Option<usize> {
    // SAFETY: recvmsg filled this bounded control buffer; CMSG_NXTHDR checks
    // the next header against msg_controllen. Check data length before read.
    unsafe {
        let mut cmsg = libc::CMSG_FIRSTHDR(msg);
        while !cmsg.is_null() {
            if (*cmsg).cmsg_level == libc::IPPROTO_UDP
                && (*cmsg).cmsg_type == libc::UDP_GRO
                && (*cmsg).cmsg_len >= libc::CMSG_LEN(4) as usize
            {
                return Some(
                    std::ptr::read_unaligned(libc::CMSG_DATA(cmsg).cast::<u32>()) as usize,
                );
            }
            cmsg = libc::CMSG_NXTHDR(msg, cmsg);
        }
    }
    None
}
