//! Media threads ask the OS to run them promptly. A frame that waits for a
//! core, or lands on an efficiency core at a low clock, arrives late and
//! uneven; the threads that capture, encode, send, receive and decode are
//! short bursts once a frame and cost nothing to favour. Shared with
//! `latch-stream` by path, as neither crate depends on the other.

/// Raises the calling thread's scheduling priority as far as the OS allows
/// an ordinary process. Failure leaves the thread as it was.
pub fn media_thread() {
    #[cfg(target_os = "macos")]
    {
        // <pthread/qos.h>: QOS_CLASS_USER_INTERACTIVE, what the OS gives
        // work a person is waiting on; it keeps the thread on performance
        // cores and ahead of default-class work.
        const QOS_CLASS_USER_INTERACTIVE: u32 = 0x21;
        extern "C" {
            fn pthread_set_qos_class_self_np(class: u32, relative: i32) -> i32;
        }
        // SAFETY: changes only the calling thread's QoS class.
        unsafe {
            pthread_set_qos_class_self_np(QOS_CLASS_USER_INTERACTIVE, 0);
        }
    }
    #[cfg(windows)]
    {
        const THREAD_PRIORITY_HIGHEST: i32 = 2;
        #[link(name = "kernel32")]
        extern "system" {
            fn GetCurrentThread() -> isize;
            fn SetThreadPriority(thread: isize, priority: i32) -> i32;
        }
        // SAFETY: the pseudo-handle names the calling thread.
        unsafe {
            SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_HIGHEST);
        }
    }
    #[cfg(target_os = "linux")]
    {
        // Only a privileged process may raise its own priority; an ordinary
        // one is refused and carries on at its nice value.
        extern "C" {
            fn setpriority(which: i32, who: u32, priority: i32) -> i32;
        }
        // SAFETY: PRIO_PROCESS with who 0 is the calling thread on Linux.
        unsafe {
            setpriority(0, 0, -10);
        }
    }
}
