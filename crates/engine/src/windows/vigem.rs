//! Virtual Xbox 360 and DualShock 4 pads through the ViGEmBus driver,
//! spoken to directly: the bus's device interface takes a handful of
//! IOCTLs, laid out as in ViGEmClient's `BusShared.h` (MIT). Each pad is a
//! target on the bus with a serial number of its own. The bus unplugs every
//! target a handle plugged in when that handle closes, so a host that dies
//! leaves no pads behind.
//!
//! Setup installs the driver (see `crates/host/src/setup.rs`); without it
//! [`open`] says so, and the viewer shows that the host takes no gamepads.

use crate::gamepad::{map, Feedback, FeedbackFn, PadKind, PadState, VirtualBus};
use anyhow::{bail, Context, Result};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use windows::core::{GUID, PCWSTR};
use windows::Win32::Devices::DeviceAndDriverInstallation::{
    CM_Get_Device_Interface_ListW, CM_Get_Device_Interface_List_SizeW,
    CM_GET_DEVICE_INTERFACE_LIST_PRESENT, CR_SUCCESS,
};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_INVALID_PARAMETER, ERROR_IO_PENDING, GENERIC_READ, GENERIC_WRITE, HANDLE,
    WAIT_OBJECT_0,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_FLAG_NO_BUFFERING, FILE_FLAG_OVERLAPPED,
    FILE_FLAG_WRITE_THROUGH, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::Win32::System::IO::{CancelIoEx, DeviceIoControl, GetOverlappedResult, OVERLAPPED};

const BUS_INTERFACE: GUID = GUID::from_u128(0x96E42B22_F5E9_42F8_B043_ED0F932F014F);
/// `VIGEM_COMMON_VERSION`: the IOCTL interface this speaks.
const COMMON_VERSION: u32 = 1;

/// `CTL_CODE(FILE_DEVICE_BUS_EXTENDER, 0x801 + index, METHOD_BUFFERED, access)`.
const fn ioctl(index: u32, read: bool) -> u32 {
    const FILE_DEVICE_BUS_EXTENDER: u32 = 0x2a;
    let access = 2 | if read { 1 } else { 0 };
    FILE_DEVICE_BUS_EXTENDER << 16 | access << 14 | (0x801 + index) << 2
}
const PLUGIN_TARGET: u32 = ioctl(0x000, false);
const UNPLUG_TARGET: u32 = ioctl(0x001, false);
const CHECK_VERSION: u32 = ioctl(0x002, false);
const WAIT_DEVICE_READY: u32 = ioctl(0x003, false);
const XUSB_REQUEST_NOTIFICATION: u32 = ioctl(0x200, true);
const XUSB_SUBMIT_REPORT: u32 = ioctl(0x201, false);
const DS4_SUBMIT_REPORT: u32 = ioctl(0x202, false);
const DS4_REQUEST_NOTIFICATION: u32 = ioctl(0x203, false);

const XBOX360_WIRED: u32 = 0;
const DUALSHOCK4_WIRED: u32 = 2;

#[repr(C)]
struct PluginTarget {
    size: u32,
    serial: u32,
    target_type: u32,
    vendor: u16,
    product: u16,
}

/// `VIGEM_UNPLUG_TARGET`, `VIGEM_WAIT_DEVICE_READY` and
/// `VIGEM_CHECK_VERSION` (whose second field is the version).
#[repr(C)]
struct Serial {
    size: u32,
    serial: u32,
}

#[repr(C)]
struct Submit<const N: usize> {
    size: u32,
    serial: u32,
    report: [u8; N],
}

/// `XUSB_REQUEST_NOTIFICATION` (large, small, LED) or
/// `DS4_REQUEST_NOTIFICATION` (small, large, red, green, blue).
#[repr(C)]
struct Notification<const N: usize> {
    size: u32,
    serial: u32,
    data: [u8; N],
}

const _: () = {
    assert!(std::mem::size_of::<PluginTarget>() == 16);
    // XUSB_SUBMIT_REPORT; DS4_SUBMIT_REPORT pads its 10-byte report to 20.
    assert!(std::mem::size_of::<Submit<12>>() == 20);
    assert!(std::mem::size_of::<Submit<10>>() == 20);
    // XUSB_REQUEST_NOTIFICATION and DS4_REQUEST_NOTIFICATION, padded.
    assert!(std::mem::size_of::<Notification<3>>() == 12);
    assert!(std::mem::size_of::<Notification<5>>() == 16);
};

/// The bus handle, shared by the pads and their notification threads.
struct Handle(HANDLE);
// The handle is only ever passed to thread-safe kernel calls.
unsafe impl Send for Handle {}
unsafe impl Sync for Handle {}
impl Drop for Handle {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// An event for one overlapped request at a time.
struct Event(HANDLE);
unsafe impl Send for Event {}
impl Event {
    fn new() -> Result<Self> {
        Ok(Self(unsafe {
            CreateEventW(None, false, false, PCWSTR::null())?
        }))
    }
}
impl Drop for Event {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// One request: `input` in, `output` back, waited for.
fn call<I, O>(
    bus: &Handle,
    event: &Event,
    code: u32,
    input: &I,
    output: Option<&mut O>,
) -> windows::core::Result<()> {
    let mut ov = OVERLAPPED {
        hEvent: event.0,
        ..Default::default()
    };
    let (out_ptr, out_len) = match output {
        Some(o) => (Some(o as *mut O as *mut _), std::mem::size_of::<O>() as u32),
        None => (None, 0),
    };
    let mut transferred = 0;
    unsafe {
        let sent = DeviceIoControl(
            bus.0,
            code,
            Some(input as *const I as *const _),
            std::mem::size_of::<I>() as u32,
            out_ptr,
            out_len,
            None,
            Some(&mut ov),
        );
        match sent {
            Err(e) if e.code() != ERROR_IO_PENDING.to_hresult() => return Err(e),
            _ => {}
        }
        GetOverlappedResult(bus.0, &ov, &mut transferred, true)
    }
}

pub struct Bus {
    handle: Arc<Handle>,
    event: Event,
}

/// Opens the first ViGEm bus and checks it speaks this interface.
pub fn open() -> Result<Bus> {
    let mut len = 0u32;
    let cr = unsafe {
        CM_Get_Device_Interface_List_SizeW(
            &mut len,
            &BUS_INTERFACE,
            PCWSTR::null(),
            CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
        )
    };
    let mut list = vec![0u16; len.max(1) as usize];
    let cr = if cr == CR_SUCCESS {
        unsafe {
            CM_Get_Device_Interface_ListW(
                &BUS_INTERFACE,
                PCWSTR::null(),
                &mut list,
                CM_GET_DEVICE_INTERFACE_LIST_PRESENT,
            )
        }
    } else {
        cr
    };
    let end = list.iter().position(|&c| c == 0).unwrap_or(0);
    if cr != CR_SUCCESS || end == 0 {
        bail!(
            "the ViGEmBus gamepad driver is not installed; run Latch's setup as administrator on the PC"
        );
    }
    let handle = unsafe {
        CreateFileW(
            PCWSTR(list.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL
                | FILE_FLAG_NO_BUFFERING
                | FILE_FLAG_WRITE_THROUGH
                | FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .context("open the ViGEmBus gamepad driver")?;
    let bus = Bus {
        handle: Arc::new(Handle(handle)),
        event: Event::new()?,
    };
    let check = Serial {
        size: 8,
        serial: COMMON_VERSION,
    };
    call::<_, ()>(&bus.handle, &bus.event, CHECK_VERSION, &check, None).context(
        "this ViGEmBus speaks another interface version; reinstall it with Latch's setup",
    )?;
    Ok(bus)
}

/// One plugged-in target; unplugs when dropped.
pub struct Target {
    handle: Arc<Handle>,
    serial: u32,
    kind: PadKind,
    stop: Arc<AtomicBool>,
    notifications: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Target {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Ok(event) = Event::new() {
            let unplug = Serial {
                size: 8,
                serial: self.serial,
            };
            let _ = call::<_, ()>(&self.handle, &event, UNPLUG_TARGET, &unplug, None);
        }
        if let Some(t) = self.notifications.take() {
            let _ = t.join();
        }
    }
}

/// Serial numbers tried for one pad: other programs (DS4Windows, Steam)
/// may hold some.
const SERIALS: u32 = 64;

impl VirtualBus for Bus {
    type Pad = Target;
    /// Xbox 360 pads take XInput's four slots, shared with any real pads;
    /// DualShock 4 pads have no such limit.
    const SLOTS: u8 = 8;

    fn kinds(&self) -> &'static [PadKind] {
        &[PadKind::Xbox, PadKind::PlayStation]
    }

    fn plug(&mut self, slot: u8, kind: PadKind, feedback: FeedbackFn) -> Result<Target> {
        let (target_type, vendor, product) = match kind {
            PadKind::PlayStation => (DUALSHOCK4_WIRED, 0x054c, 0x05c4),
            _ => (XBOX360_WIRED, 0x045e, 0x028e),
        };
        let mut serial = None;
        let mut last = None;
        for n in 0..SERIALS {
            let s = (slot as u32 + n) % SERIALS + 1;
            let plugin = PluginTarget {
                size: 16,
                serial: s,
                target_type,
                vendor,
                product,
            };
            match call::<_, ()>(&self.handle, &self.event, PLUGIN_TARGET, &plugin, None) {
                Ok(()) => {
                    serial = Some(s);
                    break;
                }
                Err(e) => last = Some(e),
            }
        }
        let Some(serial) = serial else {
            bail!(
                "ViGEmBus took no {} pad: {}",
                kind.name(),
                last.map_or_else(String::new, |e| e.message())
            );
        };
        let stop = Arc::new(AtomicBool::new(false));
        let mut target = Target {
            handle: self.handle.clone(),
            serial,
            kind,
            stop: stop.clone(),
            notifications: None,
        };
        let ready = Serial { size: 8, serial };
        match call::<_, ()>(&self.handle, &self.event, WAIT_DEVICE_READY, &ready, None) {
            // Drivers before 1.17 have no such request and are ready at once.
            Err(e) if e.code() != ERROR_INVALID_PARAMETER.to_hresult() => {
                return Err(e).context("the virtual pad did not come up");
            }
            _ => {}
        }
        // A target refuses reports (ERROR_NO_MORE_ITEMS) until Windows has
        // opened it, a few milliseconds after it is "ready": the first state
        // must not be lost to that.
        let started = std::time::Instant::now();
        while let Err(e) = self.report(&target, &PadState::default()) {
            if started.elapsed() > Duration::from_secs(1) {
                return Err(e).context("the virtual pad never took a report");
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        let handle = self.handle.clone();
        target.notifications = Some(
            std::thread::Builder::new()
                .name(format!("vigem-pad-{slot}"))
                .spawn(move || notifications(&handle, serial, slot, kind, &stop, &feedback))?,
        );
        Ok(target)
    }

    fn submit(&mut self, pad: &mut Target, state: &PadState) -> Result<()> {
        self.report(pad, state)
            .context("ViGEmBus refused the pad's state")
    }
}

impl Bus {
    fn report(&self, pad: &Target, state: &PadState) -> windows::core::Result<()> {
        match pad.kind {
            PadKind::PlayStation => {
                let r = Submit {
                    size: std::mem::size_of::<Submit<10>>() as u32,
                    serial: pad.serial,
                    report: map::ds4(state),
                };
                call::<_, ()>(&self.handle, &self.event, DS4_SUBMIT_REPORT, &r, None)
            }
            _ => {
                let r = Submit {
                    size: std::mem::size_of::<Submit<12>>() as u32,
                    serial: pad.serial,
                    report: map::xusb(state),
                };
                call::<_, ()>(&self.handle, &self.event, XUSB_SUBMIT_REPORT, &r, None)
            }
        }
    }
}

/// Keeps a notification request pending for one target and hands each
/// answer (the game's rumble, the pad's LED or light bar) to `feedback`,
/// until `stop`.
fn notifications(
    bus: &Handle,
    serial: u32,
    slot: u8,
    kind: PadKind,
    stop: &AtomicBool,
    feedback: &FeedbackFn,
) {
    crate::priority::media_thread();
    let Ok(event) = Event::new() else {
        return;
    };
    let (code, size) = match kind {
        PadKind::PlayStation => (
            DS4_REQUEST_NOTIFICATION,
            std::mem::size_of::<Notification<5>>(),
        ),
        _ => (
            XUSB_REQUEST_NOTIFICATION,
            std::mem::size_of::<Notification<3>>(),
        ),
    };
    let mut buf = Notification::<5> {
        size: size as u32,
        serial,
        data: [0; 5],
    };
    while !stop.load(Ordering::Acquire) {
        buf.size = size as u32;
        buf.serial = serial;
        let mut ov = OVERLAPPED {
            hEvent: event.0,
            ..Default::default()
        };
        let ptr = &mut buf as *mut Notification<5> as *mut _;
        let sent = unsafe {
            DeviceIoControl(
                bus.0,
                code,
                Some(ptr as *const _),
                size as u32,
                Some(ptr),
                size as u32,
                None,
                Some(&mut ov),
            )
        };
        if let Err(e) = &sent {
            if e.code() != ERROR_IO_PENDING.to_hresult() {
                std::thread::sleep(Duration::from_millis(50));
                continue;
            }
        }
        // Waits in slices so a stopping pad can cancel its own request.
        loop {
            if unsafe { WaitForSingleObject(event.0, 100) } == WAIT_OBJECT_0 {
                break;
            }
            if stop.load(Ordering::Acquire) {
                unsafe {
                    let _ = CancelIoEx(bus.0, Some(&ov));
                }
                break;
            }
        }
        let mut transferred = 0;
        let done = unsafe { GetOverlappedResult(bus.0, &ov, &mut transferred, true) };
        if stop.load(Ordering::Acquire) {
            return;
        }
        if done.is_err() {
            std::thread::sleep(Duration::from_millis(50));
            continue;
        }
        let d = buf.data;
        let motor = |v: u8| v as u16 * 257;
        feedback(
            slot,
            match kind {
                PadKind::PlayStation => Feedback {
                    large: motor(d[1]),
                    small: motor(d[0]),
                    led: Some([d[2], d[3], d[4]]),
                    ..Default::default()
                },
                _ => Feedback {
                    large: motor(d[0]),
                    small: motor(d[1]),
                    player: if d[2] < 4 { d[2] + 1 } else { 0 },
                    ..Default::default()
                },
            },
        );
    }
}
