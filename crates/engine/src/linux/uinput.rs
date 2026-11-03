//! Virtual Xbox pads through `/dev/uinput`: each viewer pad becomes an
//! input device the kernel cannot tell from a pad the `xpad` driver made
//! (same name, ids, buttons and axes), so SDL, Steam and Wine games map it
//! as an Xbox 360 pad. A game's rumble comes back as force-feedback effects
//! the device's own thread plays out and turns into motor levels.
//!
//! Opening `/dev/uinput` needs write access to it, which most systems give
//! only to root. Setup grants it (see `docs/LINUX.md`, "Gamepads").

use crate::gamepad::{map, Feedback, FeedbackFn, PadKind, PadState, VirtualBus};
use anyhow::{Context, Result};
use evdev::uinput::VirtualDevice;
use evdev::{
    AbsInfo, AbsoluteAxisCode, AttributeSet, BusType, EventSummary, FFEffectCode, FFEffectKind,
    InputId, KeyCode, UInputCode, UinputAbsSetup,
};
use std::collections::HashMap;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::time::{Duration, Instant};

const UINPUT: &str = "/dev/uinput";
const EV_SYN: u16 = 0;
const EV_KEY: u16 = 1;
const EV_ABS: u16 = 3;

pub struct Bus;

/// Checks that this user may make input devices.
pub fn open() -> Result<Bus> {
    std::fs::OpenOptions::new()
        .write(true)
        .open(UINPUT)
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::PermissionDenied => anyhow::anyhow!(
                "this user may not create gamepads ({UINPUT} is not writable); run `sudo latch-host --setup` to allow it"
            ),
            std::io::ErrorKind::NotFound => {
                anyhow::anyhow!("{UINPUT} is missing (the uinput kernel module is not loaded)")
            }
            _ => anyhow::anyhow!("cannot open {UINPUT}: {e}"),
        })?;
    Ok(Bus)
}

/// One virtual pad: states are written to a duplicate of its descriptor;
/// its thread owns the device, plays rumble, and destroys it on stop.
pub struct Target {
    events: OwnedFd,
    stop: OwnedFd,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Target {
    fn drop(&mut self) {
        let _ = unsafe { libc::write(self.stop.as_raw_fd(), [1u8].as_ptr().cast(), 1) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn device(slot: u8) -> Result<VirtualDevice> {
    let mut keys = AttributeSet::<KeyCode>::new();
    for (_, code) in map::EV_KEYS {
        keys.insert(KeyCode(code));
    }
    let mut ff = AttributeSet::<FFEffectCode>::new();
    ff.insert(FFEffectCode::FF_RUMBLE);
    let name = "Microsoft X-Box 360 pad";
    let mut builder = VirtualDevice::builder()?
        .name(name)
        .input_id(InputId::new(BusType::BUS_USB, 0x045e, 0x028e, 0x0114))
        // So a viewer on this machine does not take it for a real pad
        // (`crates/stream/src/gamepad/linux.rs`).
        .with_phys(c"latch-virtual-gamepad")?
        .with_keys(&keys)?
        .with_ff(&ff)?
        .with_ff_effects_max(16);
    for (code, _) in map::ev_axes(&PadState::default()) {
        let (min, max, fuzz, flat) = match code {
            0x02 | 0x05 => (0, 1023, 0, 0),
            0x10 | 0x11 => (-1, 1, 0, 0),
            _ => (-32768, 32767, 16, 128),
        };
        builder = builder.with_absolute_axis(&UinputAbsSetup::new(
            AbsoluteAxisCode(code),
            AbsInfo::new(0, min, max, fuzz, flat, 0),
        ))?;
    }
    builder
        .build()
        .with_context(|| format!("create virtual gamepad {}", slot + 1))
}

impl VirtualBus for Bus {
    type Pad = Target;
    const SLOTS: u8 = 16;

    fn kinds(&self) -> &'static [PadKind] {
        &[PadKind::Xbox]
    }

    fn plug(&mut self, slot: u8, _kind: PadKind, feedback: FeedbackFn) -> Result<Target> {
        let dev = device(slot)?;
        let events = unsafe { libc::dup(dev.as_raw_fd()) };
        if events < 0 {
            return Err(std::io::Error::last_os_error())
                .context("duplicate the gamepad's descriptor");
        }
        let events = unsafe { OwnedFd::from_raw_fd(events) };
        let mut fds = [0; 2];
        if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
            return Err(std::io::Error::last_os_error()).context("make the gamepad's stop pipe");
        }
        let (wake, stop) = unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        let thread = std::thread::Builder::new()
            .name(format!("uinput-pad-{slot}"))
            .spawn(move || rumble(dev, wake, slot, &feedback))?;
        Ok(Target {
            events,
            stop,
            thread: Some(thread),
        })
    }

    fn submit(&mut self, pad: &mut Target, state: &PadState) -> Result<()> {
        // The kernel drops values that did not change, so every state is
        // sent whole.
        let event = |type_, code, value| libc::input_event {
            time: libc::timeval {
                tv_sec: 0,
                tv_usec: 0,
            },
            type_,
            code,
            value,
        };
        let mut events: Vec<libc::input_event> = map::EV_KEYS
            .iter()
            .map(|(b, code)| event(EV_KEY, *code, (state.buttons & b != 0) as i32))
            .collect();
        events.extend(
            map::ev_axes(state)
                .iter()
                .map(|(code, v)| event(EV_ABS, *code, *v)),
        );
        events.push(event(EV_SYN, 0, 0));
        let bytes = std::mem::size_of_val(events.as_slice());
        let written = unsafe { libc::write(pad.events.as_raw_fd(), events.as_ptr().cast(), bytes) };
        if written != bytes as isize {
            return Err(std::io::Error::last_os_error()).context("write the gamepad's state");
        }
        Ok(())
    }
}

/// The device's thread: answers force-feedback uploads and erasures, plays
/// and stops effects (each for its own length), and sends the motors'
/// levels to `feedback` whenever they change, until `wake` is readable.
fn rumble(mut dev: VirtualDevice, wake: OwnedFd, slot: u8, feedback: &FeedbackFn) {
    // Effect id: strong and weak magnitude, and length (0: until stopped).
    let mut effects: HashMap<i16, (u16, u16, u16)> = HashMap::new();
    let mut playing: HashMap<i16, Option<Instant>> = HashMap::new();
    let mut levels = (0u16, 0u16);
    loop {
        let now = Instant::now();
        playing.retain(|_, end| end.is_none_or(|end| end > now));
        let next_end = playing.values().flatten().min().copied();
        let current = playing
            .keys()
            .filter_map(|id| effects.get(id))
            .fold((0u16, 0u16), |(s, w), &(strong, weak, _)| {
                (s.saturating_add(strong), w.saturating_add(weak))
            });
        if current != levels {
            levels = current;
            feedback(
                slot,
                Feedback {
                    large: levels.0,
                    small: levels.1,
                    ..Default::default()
                },
            );
        }
        let timeout = next_end.map_or(-1, |end| {
            end.saturating_duration_since(now)
                .as_millis()
                .clamp(1, 60_000) as i32
        });
        let mut fds = [
            libc::pollfd {
                fd: dev.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: wake.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        let n = unsafe { libc::poll(fds.as_mut_ptr(), 2, timeout) };
        if n < 0 {
            if std::io::Error::last_os_error().kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return;
        }
        if fds[1].revents != 0 {
            return;
        }
        if fds[0].revents & libc::POLLIN == 0 {
            continue;
        }
        let Ok(events) = dev.fetch_events() else {
            return;
        };
        let events: Vec<_> = events.collect();
        for e in events {
            match e.destructure() {
                EventSummary::UInput(ev, UInputCode::UI_FF_UPLOAD, _) => {
                    if let Ok(mut upload) = dev.process_ff_upload(ev) {
                        let data = upload.effect();
                        let (strong, weak) = match data.kind {
                            FFEffectKind::Rumble {
                                strong_magnitude,
                                weak_magnitude,
                            } => (strong_magnitude, weak_magnitude),
                            _ => (0, 0),
                        };
                        effects.insert(upload.effect_id(), (strong, weak, data.replay.length));
                        upload.set_retval(0);
                    }
                }
                EventSummary::UInput(ev, UInputCode::UI_FF_ERASE, _) => {
                    if let Ok(erase) = dev.process_ff_erase(ev) {
                        let id = erase.effect_id() as i16;
                        effects.remove(&id);
                        playing.remove(&id);
                    }
                }
                EventSummary::ForceFeedback(_, code, value) => {
                    let id = code.0 as i16;
                    if value > 0 {
                        let length = effects.get(&id).map_or(0, |e| e.2);
                        playing.insert(
                            id,
                            (length > 0)
                                .then(|| Instant::now() + Duration::from_millis(length as u64)),
                        );
                    } else {
                        playing.remove(&id);
                    }
                }
                _ => {}
            }
        }
    }
}
