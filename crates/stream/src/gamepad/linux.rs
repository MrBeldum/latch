//! Gamepads on Linux, through evdev: every `/dev/input/event*` device that
//! has the kernel's gamepad buttons (`BTN_SOUTH`) and a stick, one reading
//! thread each, blocked in `read` until the pad reports, so a change is
//! passed on the moment the kernel has it. Drivers that follow the kernel's
//! gamepad layout (xpad, hid-playstation, hid-nintendo, hid-microsoft) all
//! map the same way. Rumble is one `FF_RUMBLE` effect per pad, updated and
//! replayed.
//!
//! Desktop sessions give the signed-in user access to gamepads' event
//! devices (systemd's `uaccess`); a pad nobody may read is skipped.
//! Latch's own virtual pads (a host on this machine) are skipped too.

use super::{Backend, Feedback, PadKind, PadState, Shared};
use evdev::{
    AbsoluteAxisCode, Device, EventSummary, FFEffect, FFEffectData, FFEffectKind, FFReplay,
    FFTrigger, KeyCode,
};
use latch_proto::gamepad::button;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

/// How often `/dev/input` is looked at for new pads.
const SCAN: Duration = Duration::from_secs(1);
/// The `phys` Latch's host gives its virtual pads (`crates/engine/src/linux/uinput.rs`).
const LATCH_PHYS: &str = "latch-virtual-gamepad";

struct Linux {
    rumble: Arc<Mutex<HashMap<u8, FFEffect>>>,
}

pub(super) fn start(shared: Arc<Shared>) -> Option<Box<dyn Backend>> {
    let rumble = Arc::new(Mutex::new(HashMap::new()));
    let effects = rumble.clone();
    std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || scan(&shared, &effects))
        .ok()?;
    Some(Box::new(Linux { rumble }))
}

impl Backend for Linux {
    fn rumble(&self, slot: u8, f: Feedback) {
        if let Some(effect) = self.rumble.lock().get_mut(&slot) {
            let _ = if f.large == 0 && f.small == 0 {
                effect.stop()
            } else {
                effect
                    .update(rumble_effect(f.large, f.small))
                    .and_then(|()| effect.play(1))
            };
        }
    }
}

fn rumble_effect(strong: u16, weak: u16) -> FFEffectData {
    FFEffectData {
        direction: 0,
        trigger: FFTrigger::default(),
        // Until the host says otherwise; it always does when the game does.
        replay: FFReplay {
            length: u16::MAX,
            delay: 0,
        },
        kind: FFEffectKind::Rumble {
            strong_magnitude: strong,
            weak_magnitude: weak,
        },
    }
}

fn is_gamepad(dev: &Device) -> bool {
    let keys = dev
        .supported_keys()
        .is_some_and(|k| k.contains(KeyCode::BTN_SOUTH));
    let stick = dev
        .supported_absolute_axes()
        .is_some_and(|a| a.contains(AbsoluteAxisCode::ABS_X));
    keys && stick && dev.physical_path() != Some(LATCH_PHYS)
}

fn scan(shared: &Arc<Shared>, effects: &Arc<Mutex<HashMap<u8, FFEffect>>>) {
    let open: Arc<Mutex<HashSet<PathBuf>>> = Default::default();
    loop {
        for (path, dev) in evdev::enumerate() {
            if open.lock().contains(&path) || !is_gamepad(&dev) {
                continue;
            }
            open.lock().insert(path.clone());
            let shared = shared.clone();
            let effects = effects.clone();
            let open = open.clone();
            let _ = std::thread::Builder::new()
                .name("gamepad".into())
                .spawn(move || {
                    read(dev, &shared, &effects);
                    open.lock().remove(&path);
                });
        }
        std::thread::sleep(SCAN);
    }
}

fn kind(dev: &Device) -> PadKind {
    match dev.input_id().vendor() {
        0x045e => PadKind::Xbox,
        0x054c => PadKind::PlayStation,
        _ => PadKind::Generic,
    }
}

/// An axis's range, for scaling to the wire.
#[derive(Clone, Copy)]
struct Range {
    min: i32,
    max: i32,
}

impl Range {
    /// −1.0…1.0.
    fn centred(self, v: i32) -> f64 {
        let span = (self.max - self.min).max(1) as f64;
        ((v - self.min) as f64 / span) * 2.0 - 1.0
    }

    /// 0.0…1.0.
    fn unit(self, v: i32) -> f64 {
        let span = (self.max - self.min).max(1) as f64;
        (v - self.min) as f64 / span
    }
}

const KEYS: [(KeyCode, u32); 17] = [
    (KeyCode::BTN_SOUTH, button::SOUTH),
    (KeyCode::BTN_EAST, button::EAST),
    (KeyCode::BTN_NORTH, button::NORTH),
    (KeyCode::BTN_WEST, button::WEST),
    (KeyCode::BTN_TL, button::LEFT_SHOULDER),
    (KeyCode::BTN_TR, button::RIGHT_SHOULDER),
    (KeyCode::BTN_SELECT, button::BACK),
    (KeyCode::BTN_START, button::START),
    (KeyCode::BTN_MODE, button::GUIDE),
    (KeyCode::BTN_THUMBL, button::LEFT_STICK),
    (KeyCode::BTN_THUMBR, button::RIGHT_STICK),
    (KeyCode::BTN_DPAD_UP, button::DPAD_UP),
    (KeyCode::BTN_DPAD_DOWN, button::DPAD_DOWN),
    (KeyCode::BTN_DPAD_LEFT, button::DPAD_LEFT),
    (KeyCode::BTN_DPAD_RIGHT, button::DPAD_RIGHT),
    (KeyCode::BTN_TRIGGER_HAPPY1, button::PADDLE1),
    (KeyCode::BTN_TRIGGER_HAPPY2, button::PADDLE2),
];

fn read(mut dev: Device, shared: &Shared, effects: &Mutex<HashMap<u8, FFEffect>>) {
    let name = dev.name().unwrap_or("gamepad").to_string();
    let ranges: HashMap<u16, Range> = dev
        .get_absinfo()
        .map(|it| {
            it.map(|(code, info)| {
                (
                    code.0,
                    Range {
                        min: info.minimum(),
                        max: info.maximum(),
                    },
                )
            })
            .collect()
        })
        .unwrap_or_default();
    let Some(slot) = shared.connect(kind(&dev), name) else {
        return;
    };
    let rumbles = dev
        .supported_ff()
        .is_some_and(|ff| ff.contains(evdev::FFEffectCode::FF_RUMBLE));
    if rumbles {
        if let Ok(effect) = dev.upload_ff_effect(rumble_effect(0, 0)) {
            effects.lock().insert(slot, effect);
        }
    }
    let mut state = PadState::default();
    let mut hat = (0, 0);
    loop {
        let Ok(events) = dev.fetch_events() else {
            break;
        };
        let mut report = false;
        for e in events {
            match e.destructure() {
                EventSummary::Key(_, code, value) => {
                    if let Some((_, bit)) = KEYS.iter().find(|(k, _)| *k == code) {
                        if value != 0 {
                            state.buttons |= bit;
                        } else {
                            state.buttons &= !bit;
                        }
                    }
                }
                EventSummary::AbsoluteAxis(_, code, value) => {
                    let Some(&range) = ranges.get(&code.0) else {
                        continue;
                    };
                    match code {
                        AbsoluteAxisCode::ABS_X => {
                            state.left_x = super::stick(range.centred(value))
                        }
                        AbsoluteAxisCode::ABS_Y => {
                            state.left_y = super::stick(-range.centred(value))
                        }
                        AbsoluteAxisCode::ABS_RX => {
                            state.right_x = super::stick(range.centred(value))
                        }
                        AbsoluteAxisCode::ABS_RY => {
                            state.right_y = super::stick(-range.centred(value))
                        }
                        AbsoluteAxisCode::ABS_Z | AbsoluteAxisCode::ABS_BRAKE => {
                            state.left_trigger = super::trigger(range.unit(value))
                        }
                        AbsoluteAxisCode::ABS_RZ | AbsoluteAxisCode::ABS_GAS => {
                            state.right_trigger = super::trigger(range.unit(value))
                        }
                        AbsoluteAxisCode::ABS_HAT0X => hat.0 = value.signum(),
                        AbsoluteAxisCode::ABS_HAT0Y => hat.1 = value.signum(),
                        _ => {}
                    }
                    if matches!(
                        code,
                        AbsoluteAxisCode::ABS_HAT0X | AbsoluteAxisCode::ABS_HAT0Y
                    ) {
                        state.buttons &= !(button::DPAD_UP
                            | button::DPAD_DOWN
                            | button::DPAD_LEFT
                            | button::DPAD_RIGHT);
                        for (on, bit) in [
                            (hat.0 < 0, button::DPAD_LEFT),
                            (hat.0 > 0, button::DPAD_RIGHT),
                            (hat.1 < 0, button::DPAD_UP),
                            (hat.1 > 0, button::DPAD_DOWN),
                        ] {
                            if on {
                                state.buttons |= bit;
                            }
                        }
                    }
                }
                EventSummary::Synchronization(..) => report = true,
                _ => {}
            }
        }
        if report {
            shared.update(slot, state);
        }
    }
    effects.lock().remove(&slot);
    shared.disconnect(slot);
}
