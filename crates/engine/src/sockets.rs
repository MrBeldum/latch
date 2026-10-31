//! Media sockets get buffers that hold a whole keyframe. A keyframe leaves
//! the host as one burst of hundreds of datagrams; with the systems'
//! default buffers (208 KB on Linux, 768 KB for a receiver on macOS, 64 KB
//! on Windows) the tail of a large one is dropped on the host before it is
//! sent, or on the viewer while its receive thread is busy, and a lost
//! keyframe costs a round trip and another keyframe. Shared with
//! `latch-stream` by path, as neither crate depends on the other.

use std::net::UdpSocket;

/// Asks for this much buffer each way, then less until the system agrees
/// (macOS refuses anything above `kern.ipc.maxsockbuf`, Linux caps it at
/// `net.core.rmem_max`/`wmem_max` silently).
const WANTED: [i32; 4] = [8 << 20, 4 << 20, 2 << 20, 1 << 20];

/// Grows the socket's send and receive buffers as far as the system lets
/// it. Failure leaves the defaults.
pub fn grow(socket: &UdpSocket) {
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    let _ = socket;
    #[cfg(any(target_os = "macos", target_os = "linux", windows))]
    for option in [SO_RCVBUF, SO_SNDBUF] {
        for bytes in WANTED {
            if set(socket, option, bytes) {
                break;
            }
        }
    }
}

#[cfg(target_os = "macos")]
const SOL_SOCKET: i32 = 0xffff;
#[cfg(target_os = "macos")]
const SO_SNDBUF: i32 = 0x1001;
#[cfg(target_os = "macos")]
const SO_RCVBUF: i32 = 0x1002;

#[cfg(target_os = "linux")]
const SOL_SOCKET: i32 = 1;
#[cfg(target_os = "linux")]
const SO_SNDBUF: i32 = 7;
#[cfg(target_os = "linux")]
const SO_RCVBUF: i32 = 8;

#[cfg(windows)]
const SOL_SOCKET: i32 = 0xffff;
#[cfg(windows)]
const SO_SNDBUF: i32 = 0x1001;
#[cfg(windows)]
const SO_RCVBUF: i32 = 0x1002;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn set(socket: &UdpSocket, option: i32, bytes: i32) -> bool {
    use std::os::fd::AsRawFd;
    extern "C" {
        fn setsockopt(
            socket: i32,
            level: i32,
            name: i32,
            value: *const std::ffi::c_void,
            len: u32,
        ) -> i32;
    }
    // SAFETY: the descriptor is open for the call; the value is an int.
    unsafe {
        setsockopt(
            socket.as_raw_fd(),
            SOL_SOCKET,
            option,
            (&bytes as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        ) == 0
    }
}

#[cfg(windows)]
fn set(socket: &UdpSocket, option: i32, bytes: i32) -> bool {
    use std::os::windows::io::AsRawSocket;
    #[link(name = "ws2_32")]
    extern "system" {
        fn setsockopt(socket: usize, level: i32, name: i32, value: *const u8, len: i32) -> i32;
    }
    // SAFETY: the socket is open for the call; the value is an int.
    unsafe {
        setsockopt(
            socket.as_raw_socket() as usize,
            SOL_SOCKET,
            option,
            (&bytes as *const i32).cast(),
            std::mem::size_of::<i32>() as i32,
        ) == 0
    }
}
