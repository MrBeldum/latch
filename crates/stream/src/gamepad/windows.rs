//! Gamepads on Windows, through Windows.Gaming.Input, read every
//! millisecond on one thread: Xbox pads (and anything else Windows maps to
//! one) as `Gamepad`s, with both motors and both trigger motors; every
//! other pad as a `RawGameController`, laid out as a DualShock 4 or
//! DualSense reports itself when it is a Sony pad, and by position
//! otherwise. Windows passes pads' input only to the app in front.

use super::{Backend, Feedback, PadKind, PadState, Shared};
use latch_proto::gamepad::button;
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use windows::Gaming::Input::{
    GameControllerSwitchPosition, Gamepad, GamepadButtons, GamepadVibration, RawGameController,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

/// How often the list of controllers is looked at.
const SCAN: Duration = Duration::from_millis(250);
/// How often each pad is read: within a millisecond of any change.
const POLL: Duration = Duration::from_millis(1);

struct Windows {
    gamepads: Arc<Mutex<HashMap<u8, Gamepad>>>,
}

impl Backend for Windows {
    fn rumble(&self, slot: u8, f: Feedback) {
        if let Some(pad) = self.gamepads.lock().get(&slot) {
            let unit = |v: u16| v as f64 / 65535.0;
            let _ = pad.SetVibration(GamepadVibration {
                LeftMotor: unit(f.large),
                RightMotor: unit(f.small),
                LeftTrigger: unit(f.left_trigger),
                RightTrigger: unit(f.right_trigger),
            });
        }
    }
}

pub(super) fn start(shared: Arc<Shared>) -> Option<Box<dyn Backend>> {
    let gamepads = Arc::new(Mutex::new(HashMap::new()));
    let rumble = gamepads.clone();
    std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || {
            crate::priority::media_thread();
            let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
            poll(&shared, &gamepads);
        })
        .ok()?;
    Some(Box::new(Windows { gamepads: rumble }))
}

enum Reader {
    Gamepad(Gamepad),
    Raw {
        raw: RawGameController,
        sony: bool,
        buttons: Vec<bool>,
        switches: Vec<GameControllerSwitchPosition>,
        axes: Vec<f64>,
    },
}

struct Pad {
    id: String,
    slot: u8,
    reader: Reader,
    last: u64,
}

fn poll(shared: &Shared, gamepads: &Mutex<HashMap<u8, Gamepad>>) {
    let mut pads: Vec<Pad> = Vec::new();
    let mut scanned = Instant::now() - SCAN;
    loop {
        if scanned.elapsed() >= SCAN {
            scanned = Instant::now();
            rescan(shared, gamepads, &mut pads);
        }
        for pad in &mut pads {
            if let Some((timestamp, state)) = read(&mut pad.reader) {
                if timestamp != pad.last {
                    pad.last = timestamp;
                    shared.update(pad.slot, state);
                }
            }
        }
        std::thread::sleep(POLL);
    }
}

fn rescan(shared: &Shared, gamepads: &Mutex<HashMap<u8, Gamepad>>, pads: &mut Vec<Pad>) {
    let Ok(list) = RawGameController::RawGameControllers() else {
        return;
    };
    let now: Vec<(String, RawGameController)> = list
        .into_iter()
        .filter_map(|raw| Some((raw.NonRoamableId().ok()?.to_string(), raw)))
        .collect();
    pads.retain(|pad| {
        let present = now.iter().any(|(id, _)| *id == pad.id);
        if !present {
            gamepads.lock().remove(&pad.slot);
            shared.disconnect(pad.slot);
        }
        present
    });
    for (id, raw) in now {
        if pads.iter().any(|p| p.id == id) {
            continue;
        }
        let name = raw
            .DisplayName()
            .map_or_else(|_| "gamepad".into(), |n| n.to_string());
        let vendor = raw.HardwareVendorId().unwrap_or(0);
        let (kind, reader) = match Gamepad::FromGameController(&raw) {
            Ok(gamepad) => (PadKind::Xbox, Reader::Gamepad(gamepad)),
            Err(_) => {
                let count = |n: windows::core::Result<i32>| n.unwrap_or(0).max(0) as usize;
                let sony = vendor == 0x054c;
                (
                    if sony {
                        PadKind::PlayStation
                    } else {
                        PadKind::Generic
                    },
                    Reader::Raw {
                        buttons: vec![false; count(raw.ButtonCount())],
                        switches: vec![Default::default(); count(raw.SwitchCount())],
                        axes: vec![0.0; count(raw.AxisCount())],
                        raw,
                        sony,
                    },
                )
            }
        };
        let Some(slot) = shared.connect(kind, name) else {
            continue;
        };
        if let Reader::Gamepad(g) = &reader {
            gamepads.lock().insert(slot, g.clone());
        }
        pads.push(Pad {
            id,
            slot,
            reader,
            last: 0,
        });
    }
}

fn dpad(position: GameControllerSwitchPosition) -> u32 {
    use GameControllerSwitchPosition as P;
    match position {
        P::Up => button::DPAD_UP,
        P::UpRight => button::DPAD_UP | button::DPAD_RIGHT,
        P::Right => button::DPAD_RIGHT,
        P::DownRight => button::DPAD_DOWN | button::DPAD_RIGHT,
        P::Down => button::DPAD_DOWN,
        P::DownLeft => button::DPAD_DOWN | button::DPAD_LEFT,
        P::Left => button::DPAD_LEFT,
        P::UpLeft => button::DPAD_UP | button::DPAD_LEFT,
        _ => 0,
    }
}

/// The reading's timestamp and the pad's state.
fn read(reader: &mut Reader) -> Option<(u64, PadState)> {
    match reader {
        Reader::Gamepad(g) => {
            let r = g.GetCurrentReading().ok()?;
            const TABLE: [(GamepadButtons, u32); 18] = [
                (GamepadButtons::A, button::SOUTH),
                (GamepadButtons::B, button::EAST),
                (GamepadButtons::X, button::WEST),
                (GamepadButtons::Y, button::NORTH),
                (GamepadButtons::LeftShoulder, button::LEFT_SHOULDER),
                (GamepadButtons::RightShoulder, button::RIGHT_SHOULDER),
                (GamepadButtons::View, button::BACK),
                (GamepadButtons::Menu, button::START),
                (GamepadButtons::LeftThumbstick, button::LEFT_STICK),
                (GamepadButtons::RightThumbstick, button::RIGHT_STICK),
                (GamepadButtons::DPadUp, button::DPAD_UP),
                (GamepadButtons::DPadDown, button::DPAD_DOWN),
                (GamepadButtons::DPadLeft, button::DPAD_LEFT),
                (GamepadButtons::DPadRight, button::DPAD_RIGHT),
                (GamepadButtons::Paddle1, button::PADDLE1),
                (GamepadButtons::Paddle2, button::PADDLE2),
                (GamepadButtons::Paddle3, button::PADDLE3),
                (GamepadButtons::Paddle4, button::PADDLE4),
            ];
            let buttons = TABLE
                .iter()
                .filter(|(b, _)| r.Buttons.contains(*b))
                .fold(0, |n, (_, bit)| n | bit);
            Some((
                r.Timestamp,
                PadState {
                    buttons,
                    left_x: super::stick(r.LeftThumbstickX),
                    left_y: super::stick(r.LeftThumbstickY),
                    right_x: super::stick(r.RightThumbstickX),
                    right_y: super::stick(r.RightThumbstickY),
                    left_trigger: super::trigger(r.LeftTrigger),
                    right_trigger: super::trigger(r.RightTrigger),
                    motion: None,
                    touch: None,
                },
            ))
        }
        Reader::Raw {
            raw,
            sony,
            buttons,
            switches,
            axes,
        } => {
            let timestamp = raw.GetCurrentReading(buttons, switches, axes).ok()?;
            Some((timestamp, raw_state(*sony, buttons, switches, axes)))
        }
    }
}

/// A raw controller's reading: axes are 0…1, 0.5 centred, down positive.
/// Sony pads report in this order (as SDL's DirectInput mapping has it):
/// Square, Cross, Circle, Triangle, L1, R1, L2, R2, Share, Options, L3,
/// R3, PS, touchpad, then on a DualSense Mute; axes LX, LY, RX, L2, R2, RY.
fn raw_state(
    sony: bool,
    buttons: &[bool],
    switches: &[GameControllerSwitchPosition],
    axes: &[f64],
) -> PadState {
    const SONY: [u32; 15] = [
        button::WEST,
        button::SOUTH,
        button::EAST,
        button::NORTH,
        button::LEFT_SHOULDER,
        button::RIGHT_SHOULDER,
        0,
        0,
        button::BACK,
        button::START,
        button::LEFT_STICK,
        button::RIGHT_STICK,
        button::GUIDE,
        button::TOUCHPAD,
        button::MISC,
    ];
    const GENERIC: [u32; 10] = [
        button::SOUTH,
        button::EAST,
        button::WEST,
        button::NORTH,
        button::LEFT_SHOULDER,
        button::RIGHT_SHOULDER,
        button::BACK,
        button::START,
        button::LEFT_STICK,
        button::RIGHT_STICK,
    ];
    let table: &[u32] = if sony { &SONY } else { &GENERIC };
    let mut bits = buttons
        .iter()
        .zip(table)
        .filter(|(on, _)| **on)
        .fold(0, |n, (_, bit)| n | bit);
    bits |= switches.first().copied().map_or(0, dpad);
    let axis = |i: usize| axes.get(i).copied().unwrap_or(0.5);
    let centred = |i: usize| axis(i) * 2.0 - 1.0;
    let (right_y, left_trigger, right_trigger) = if sony {
        (axis(5), axis(3), axis(4))
    } else {
        (axis(3), 0.0, 0.0)
    };
    PadState {
        buttons: bits,
        left_x: super::stick(centred(0)),
        left_y: super::stick(-centred(1)),
        right_x: super::stick(centred(2)),
        right_y: super::stick(-(right_y * 2.0 - 1.0)),
        left_trigger: super::trigger(left_trigger),
        right_trigger: super::trigger(right_trigger),
        motion: None,
        touch: None,
    }
}
