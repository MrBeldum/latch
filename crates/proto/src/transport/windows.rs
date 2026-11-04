use std::{
    io,
    net::{SocketAddr, UdpSocket},
    os::windows::io::AsRawSocket,
};
use windows_sys::Win32::Networking::WinSock::*;

fn error() -> io::Error {
    // SAFETY: reads the calling thread's Winsock error.
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

fn extension<T: Copy>(udp: &UdpSocket, id: windows_sys::core::GUID) -> io::Result<T> {
    // SAFETY: GUID input and pointer-sized function output are valid for
    // WSAIoctl. T is only instantiated with the documented extension types.
    unsafe {
        let mut function: T = std::mem::zeroed();
        let mut bytes = 0;
        if WSAIoctl(
            udp.as_raw_socket() as _,
            SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&id as *const windows_sys::core::GUID).cast(),
            std::mem::size_of_val(&id) as _,
            (&mut function as *mut T).cast(),
            std::mem::size_of::<T>() as _,
            &mut bytes,
            std::ptr::null_mut(),
            None,
        ) == SOCKET_ERROR
        {
            return Err(error());
        }
        Ok(function)
    }
}

/// Older Windows builds and drivers without USO reject the option or message.
pub(super) fn unsupported(error: &io::Error) -> bool {
    error.kind() == io::ErrorKind::Unsupported
        || matches!(
            error.raw_os_error(),
            Some(WSAEINVAL | WSAEOPNOTSUPP | WSAENOPROTOOPT | WSAEMSGSIZE)
        )
}

/// WSASendMsg is an extension function: look it up once per sender.
#[derive(Default)]
pub(super) struct Sender {
    function: Option<LPFN_WSASENDMSG>,
}
impl Sender {
    pub(super) fn send(
        &mut self,
        udp: &UdpSocket,
        datagrams: &[Vec<u8>],
        to: SocketAddr,
    ) -> io::Result<()> {
        let function = match self.function {
            Some(function) => function,
            None => *self
                .function
                .insert(extension::<LPFN_WSASENDMSG>(udp, WSAID_WSASENDMSG)?),
        };
        send(function, udp, datagrams, to)
    }
}

fn send(
    function: LPFN_WSASENDMSG,
    udp: &UdpSocket,
    datagrams: &[Vec<u8>],
    to: SocketAddr,
) -> io::Result<()> {
    let function = function.ok_or(io::ErrorKind::Unsupported)?;
    let address = socket2::SockAddr::from(to);
    // SAFETY: WSAMSG references live, aligned control/address/payload storage;
    // synchronous WSASendMsg consumes it before returning. Per-message USO
    // avoids changing segmentation on the shared audio/input socket.
    unsafe {
        let mut buffers: [WSABUF; super::BATCH_SIZE] = std::mem::zeroed();
        let count = datagrams.len().min(super::BATCH_SIZE);
        for (buffer, dg) in buffers.iter_mut().zip(datagrams) {
            buffer.len = dg.len() as u32;
            buffer.buf = dg.as_ptr().cast_mut();
        }
        let mut control = [0usize; 8];
        let header = control.as_mut_ptr().cast::<CMSGHDR>();
        (*header).cmsg_len = std::mem::size_of::<CMSGHDR>() + 4;
        (*header).cmsg_level = IPPROTO_UDP;
        (*header).cmsg_type = UDP_SEND_MSG_SIZE;
        header.add(1).cast::<u32>().write(datagrams[0].len() as u32);
        let msg = WSAMSG {
            name: address.as_ptr().cast_mut().cast(),
            namelen: address.len() as _,
            lpBuffers: buffers.as_mut_ptr(),
            dwBufferCount: count as _,
            Control: WSABUF {
                len: (std::mem::size_of::<CMSGHDR>() + std::mem::size_of::<usize>()) as _,
                buf: control.as_mut_ptr().cast(),
            },
            dwFlags: 0,
        };
        let mut bytes = 0;
        if function(
            udp.as_raw_socket() as _,
            &msg,
            0,
            &mut bytes,
            std::ptr::null_mut(),
            None,
        ) == SOCKET_ERROR
        {
            Err(error())
        } else {
            Ok(())
        }
    }
}

pub(super) struct Receiver {
    enabled: bool,
    configured: bool,
    function: LPFN_WSARECVMSG,
}
impl Receiver {
    pub(super) fn new(enabled: bool) -> Self {
        Self {
            enabled,
            configured: false,
            function: None,
        }
    }
    pub(super) fn receive(
        &mut self,
        udp: &UdpSocket,
        buffer: &mut [u8],
        coalesced: &mut u64,
        emit: &mut impl FnMut(&[u8], SocketAddr),
    ) -> Option<io::Result<usize>> {
        if !self.configured {
            self.configured = true;
            if self.enabled {
                self.function = extension::<LPFN_WSARECVMSG>(udp, WSAID_WSARECVMSG)
                    .ok()
                    .flatten();
                if self.function.is_some() {
                    let size = buffer.len() as u32;
                    // SAFETY: setsockopt reads a live DWORD value.
                    if unsafe {
                        setsockopt(
                            udp.as_raw_socket() as _,
                            IPPROTO_UDP,
                            UDP_RECV_MAX_COALESCED_SIZE,
                            (&size as *const u32).cast(),
                            4,
                        )
                    } == SOCKET_ERROR
                    {
                        self.function = None;
                    }
                }
            }
        }
        let function = self.function?;
        // SAFETY: synchronous receive owns all writable buffers and addresses.
        unsafe {
            let mut address: socket2::SockAddrStorage = std::mem::zeroed();
            let mut control = [0usize; 8];
            let mut buf = WSABUF {
                len: buffer.len() as _,
                buf: buffer.as_mut_ptr(),
            };
            let mut msg = WSAMSG {
                name: (&mut address as *mut socket2::SockAddrStorage).cast(),
                namelen: std::mem::size_of_val(&address) as _,
                lpBuffers: &mut buf,
                dwBufferCount: 1,
                Control: WSABUF {
                    len: std::mem::size_of_val(&control) as _,
                    buf: control.as_mut_ptr().cast(),
                },
                dwFlags: 0,
            };
            let mut bytes = 0;
            if function(
                udp.as_raw_socket() as _,
                &mut msg,
                &mut bytes,
                std::ptr::null_mut(),
                None,
            ) == SOCKET_ERROR
            {
                let e = error();
                if e.kind() == io::ErrorKind::WouldBlock {
                    return Some(Err(e));
                }
                // Disable coalescing before scalar receive, so an aggregate can
                // never reach the wire parser as one datagram after fallback.
                let zero = 0u32;
                if setsockopt(
                    udp.as_raw_socket() as _,
                    IPPROTO_UDP,
                    UDP_RECV_MAX_COALESCED_SIZE,
                    (&zero as *const u32).cast(),
                    4,
                ) == SOCKET_ERROR
                {
                    return Some(Err(e));
                }
                self.function = None;
                return None;
            }
            if msg.dwFlags & (MSG_TRUNC | MSG_CTRUNC) != 0 {
                return Some(Ok(1));
            }
            let mut segment = None;
            let header_size = std::mem::size_of::<CMSGHDR>();
            let mut offset = 0usize;
            while offset + header_size <= msg.Control.len as usize {
                let h = &*msg.Control.buf.add(offset).cast::<CMSGHDR>();
                if h.cmsg_len < header_size || offset + h.cmsg_len > msg.Control.len as usize {
                    break;
                }
                if h.cmsg_level == IPPROTO_UDP
                    && h.cmsg_type == UDP_COALESCED_INFO as i32
                    && h.cmsg_len >= header_size + 4
                {
                    segment = Some(std::ptr::read_unaligned(
                        msg.Control.buf.add(offset + header_size).cast::<u32>(),
                    ) as usize);
                }
                offset += h.cmsg_len.next_multiple_of(std::mem::size_of::<usize>());
            }
            if let Some(from) = socket2::SockAddr::new(address, msg.namelen as _).as_socket() {
                super::emit_segments(&buffer[..bytes as usize], from, segment, coalesced, emit);
            }
            Some(Ok(1))
        }
    }
}
