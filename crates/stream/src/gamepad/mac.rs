//! Gamepads on a Mac, through the GameController framework: Xbox Wireless,
//! DualSense, DualShock 4, Switch Pro and MFi pads, over USB or Bluetooth.
//! Each pad's value-changed handler runs on Latch's own serial dispatch
//! queue, not the main one, so a change is passed on the moment the
//! framework has it even while the window is drawing. The framework gives
//! the pads to the frontmost app only, which is what a stream wants.

use super::{Backend, Feedback, PadKind, PadState, Shared};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use latch_proto::gamepad::button;
use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{msg_send, AnyThread};
use objc2_core_haptics::{
    CHHapticAdvancedPatternPlayer, CHHapticDynamicParameter,
    CHHapticDynamicParameterIDHapticIntensityControl, CHHapticEngine, CHHapticEvent,
    CHHapticEventParameter, CHHapticEventParameterIDHapticIntensity,
    CHHapticEventParameterIDHapticSharpness, CHHapticEventTypeHapticContinuous, CHHapticPattern,
    CHHapticPatternPlayer, CHHapticTimeImmediate,
};
use objc2_foundation::NSArray;
use objc2_game_controller::{
    GCController, GCControllerButtonInput, GCControllerDirectionPad, GCControllerElement, GCDevice,
    GCDeviceHaptics, GCDualSenseGamepad, GCDualShockGamepad, GCExtendedGamepad, GCHapticsLocality,
    GCHapticsLocalityDefault, GCHapticsLocalityLeftHandle, GCHapticsLocalityLeftTrigger,
    GCHapticsLocalityRightHandle, GCHapticsLocalityRightTrigger, GCXboxGamepad,
};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

/// How often the list of controllers is looked at.
const SCAN: Duration = Duration::from_millis(250);

/// The longest a continuous haptic event may be; the player loops it.
const EVENT: f64 = 30.0;

/// Each connected pad's rumble, by slot.
type Rumbles = Arc<Mutex<HashMap<u8, Haptics>>>;

struct Mac {
    rumbles: Rumbles,
}

impl Backend for Mac {
    fn rumble(&self, slot: u8, feedback: Feedback) {
        if let Some(h) = self.rumbles.lock().get_mut(&slot) {
            h.play(&feedback);
        }
    }
}

pub(super) fn start(shared: Arc<Shared>) -> Option<Box<dyn Backend>> {
    let rumbles = Rumbles::default();
    let pads = rumbles.clone();
    std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || scan(&shared, &pads))
        .ok()?;
    Some(Box::new(Mac { rumbles }))
}

/// Where a pad's rumble plays. The engines are made the first time the
/// host rumbles the pad, and again after the system resets them.
struct Haptics {
    controller: Retained<GCController>,
    motors: Option<Vec<Motor>>,
}

// SAFETY: Core Haptics engines and players may be driven from any thread,
// and the mutex they sit behind lets one caller at a time at them; the
// controller is only asked for its haptics.
unsafe impl Send for Haptics {}

/// One actuator: an engine and a player looping a full-strength continuous
/// event, scaled by the intensity control.
struct Motor {
    /// Which of the host's levels this actuator plays.
    level: fn(&Feedback) -> u16,
    engine: Retained<CHHapticEngine>,
    player: Retained<ProtocolObject<dyn CHHapticAdvancedPatternPlayer>>,
    playing: bool,
}

impl Haptics {
    fn play(&mut self, feedback: &Feedback) {
        if self.motors.is_none() {
            if *feedback == Feedback::default() {
                return;
            }
            self.motors = Some(motors(&self.controller));
        }
        let motors = self.motors.as_mut().expect("made above");
        if motors
            .iter_mut()
            .any(|m| m.set((m.level)(feedback)).is_err())
        {
            // A reset engine forgets its players; make new ones next time.
            tracing::debug!("gamepad haptics stopped; remaking them");
            for m in motors.iter() {
                unsafe { m.engine.stopWithCompletionHandler(std::ptr::null_mut()) };
            }
            self.motors = None;
        }
    }
}

impl Drop for Haptics {
    fn drop(&mut self) {
        for m in self.motors.iter().flatten() {
            unsafe { m.engine.stopWithCompletionHandler(std::ptr::null_mut()) };
        }
    }
}

impl Motor {
    fn set(&mut self, level: u16) -> Result<(), ()> {
        unsafe {
            if level == 0 {
                if self.playing {
                    self.playing = false;
                    self.player
                        .stopAtTime_error(CHHapticTimeImmediate)
                        .map_err(drop)?;
                }
                return Ok(());
            }
            let intensity = CHHapticDynamicParameter::initWithParameterID_value_relativeTime(
                CHHapticDynamicParameter::alloc(),
                CHHapticDynamicParameterIDHapticIntensityControl,
                f32::from(level) / 65535.0,
                0.0,
            );
            let intensity = NSArray::from_retained_slice(&[intensity]);
            let send = || {
                self.player
                    .sendParameters_atTime_error(&intensity, CHHapticTimeImmediate)
            };
            send().map_err(drop)?;
            if !self.playing {
                self.engine.startAndReturnError().map_err(drop)?;
                self.player
                    .startAtTime_error(CHHapticTimeImmediate)
                    .map_err(drop)?;
                self.playing = true;
                // A player starts from its pattern's own intensity.
                send().map_err(drop)?;
            }
        }
        Ok(())
    }
}

/// The pad's actuators: its handles (heavy motor left, light right), else
/// one engine for the whole pad, plus trigger motors where it has them.
fn motors(controller: &GCController) -> Vec<Motor> {
    let Some(haptics) = (unsafe { controller.haptics() }) else {
        return Vec::new();
    };
    let make = |locality: &GCHapticsLocality, level: fn(&Feedback) -> u16, sharpness: f32| {
        motor(&haptics, locality, level, sharpness)
    };
    let mut out: Vec<Motor> = unsafe {
        [
            make(GCHapticsLocalityLeftHandle, |f| f.large, 0.2),
            make(GCHapticsLocalityRightHandle, |f| f.small, 0.8),
        ]
    }
    .into_iter()
    .flatten()
    .collect();
    if out.is_empty() {
        out.extend(unsafe { make(GCHapticsLocalityDefault, |f| f.large.max(f.small), 0.5) });
    }
    out.extend(
        unsafe {
            [
                make(GCHapticsLocalityLeftTrigger, |f| f.left_trigger, 0.6),
                make(GCHapticsLocalityRightTrigger, |f| f.right_trigger, 0.6),
            ]
        }
        .into_iter()
        .flatten(),
    );
    out
}

fn motor(
    haptics: &GCDeviceHaptics,
    locality: &GCHapticsLocality,
    level: fn(&Feedback) -> u16,
    sharpness: f32,
) -> Option<Motor> {
    unsafe {
        // `createEngineWithLocality:` is in macOS 11's GameController but
        // the bindings only declare it for iOS.
        let engine: Option<Retained<CHHapticEngine>> =
            msg_send![haptics, createEngineWithLocality: locality];
        let engine = engine?;
        let parameters = NSArray::from_retained_slice(&[
            CHHapticEventParameter::initWithParameterID_value(
                CHHapticEventParameter::alloc(),
                CHHapticEventParameterIDHapticIntensity,
                1.0,
            ),
            CHHapticEventParameter::initWithParameterID_value(
                CHHapticEventParameter::alloc(),
                CHHapticEventParameterIDHapticSharpness,
                sharpness,
            ),
        ]);
        let event = CHHapticEvent::initWithEventType_parameters_relativeTime_duration(
            CHHapticEvent::alloc(),
            CHHapticEventTypeHapticContinuous,
            &parameters,
            0.0,
            EVENT,
        );
        let pattern = CHHapticPattern::initWithEvents_parameters_error(
            CHHapticPattern::alloc(),
            &NSArray::from_retained_slice(&[event]),
            &NSArray::new(),
        )
        .ok()?;
        engine.startAndReturnError().ok()?;
        let player = engine
            .createAdvancedPlayerWithPattern_error(&pattern)
            .ok()?;
        player.setLoopEnabled(true);
        player.setLoopEnd(EVENT);
        Some(Motor {
            level,
            engine,
            player,
            playing: false,
        })
    }
}

fn kind(controller: &GCController) -> PadKind {
    let category = unsafe { controller.productCategory() }.to_string();
    if category.contains("Xbox") {
        PadKind::Xbox
    } else if category.contains("DualSense") || category.contains("DualShock") {
        PadKind::PlayStation
    } else {
        PadKind::Generic
    }
}

fn scan(shared: &Arc<Shared>, rumbles: &Rumbles) {
    let queue: DispatchRetained<DispatchQueue> =
        DispatchQueue::new("io.github.MrBeldum.Latch.gamepads", None);
    let mut known: Vec<(Retained<GCController>, u8)> = Vec::new();
    loop {
        let now = unsafe { GCController::controllers() };
        let now: Vec<Retained<GCController>> = now.iter().collect();
        known.retain(|(c, slot)| {
            let present = now
                .iter()
                .any(|n| Retained::as_ptr(n) == Retained::as_ptr(c));
            if !present {
                rumbles.lock().remove(slot);
                shared.disconnect(*slot);
            }
            present
        });
        for controller in now {
            if known
                .iter()
                .any(|(c, _)| Retained::as_ptr(c) == Retained::as_ptr(&controller))
            {
                continue;
            }
            let Some(gamepad) = (unsafe { controller.extendedGamepad() }) else {
                continue;
            };
            let name = unsafe { controller.vendorName() }
                .map_or_else(|| "gamepad".to_string(), |n| n.to_string());
            let Some(slot) = shared.connect(kind(&controller), name) else {
                continue;
            };
            let to = shared.clone();
            let handler = RcBlock::new(
                move |pad: NonNull<GCExtendedGamepad>, _: NonNull<GCControllerElement>| {
                    to.update(slot, read(unsafe { pad.as_ref() }));
                },
            );
            unsafe {
                controller.setHandlerQueue(&queue);
                gamepad.setValueChangedHandler(RcBlock::as_ptr(&handler));
            }
            shared.update(slot, read(&gamepad));
            rumbles.lock().insert(
                slot,
                Haptics {
                    controller: controller.clone(),
                    motors: None,
                },
            );
            known.push((controller, slot));
        }
        std::thread::sleep(SCAN);
    }
}

fn pressed(b: &GCControllerButtonInput) -> bool {
    unsafe { b.isPressed() }
}

fn axes(d: &GCControllerDirectionPad) -> (i16, i16) {
    unsafe {
        (
            super::stick(d.xAxis().value() as f64),
            super::stick(d.yAxis().value() as f64),
        )
    }
}

/// The pad's state now. Called on the handler queue.
fn read(g: &GCExtendedGamepad) -> PadState {
    let mut buttons = 0;
    let mut set = |on: bool, bit: u32| {
        if on {
            buttons |= bit;
        }
    };
    unsafe {
        set(pressed(&g.buttonA()), button::SOUTH);
        set(pressed(&g.buttonB()), button::EAST);
        set(pressed(&g.buttonX()), button::WEST);
        set(pressed(&g.buttonY()), button::NORTH);
        set(pressed(&g.leftShoulder()), button::LEFT_SHOULDER);
        set(pressed(&g.rightShoulder()), button::RIGHT_SHOULDER);
        set(pressed(&g.buttonMenu()), button::START);
        let optional = [
            (g.buttonOptions(), button::BACK),
            (g.buttonHome(), button::GUIDE),
            (g.leftThumbstickButton(), button::LEFT_STICK),
            (g.rightThumbstickButton(), button::RIGHT_STICK),
        ];
        for (b, bit) in optional {
            set(b.is_some_and(|b| pressed(&b)), bit);
        }
        let dpad = g.dpad();
        set(pressed(&dpad.up()), button::DPAD_UP);
        set(pressed(&dpad.down()), button::DPAD_DOWN);
        set(pressed(&dpad.left()), button::DPAD_LEFT);
        set(pressed(&dpad.right()), button::DPAD_RIGHT);
        if let Some(ds) = g.downcast_ref::<GCDualSenseGamepad>() {
            set(pressed(&ds.touchpadButton()), button::TOUCHPAD);
        } else if let Some(ds4) = g.downcast_ref::<GCDualShockGamepad>() {
            set(
                ds4.touchpadButton().is_some_and(|b| pressed(&b)),
                button::TOUCHPAD,
            );
        } else if let Some(x) = g.downcast_ref::<GCXboxGamepad>() {
            let extra = [
                (x.buttonShare(), button::MISC),
                (x.paddleButton1(), button::PADDLE1),
                (x.paddleButton2(), button::PADDLE2),
                (x.paddleButton3(), button::PADDLE3),
                (x.paddleButton4(), button::PADDLE4),
            ];
            for (b, bit) in extra {
                set(b.is_some_and(|b| pressed(&b)), bit);
            }
        }
        let (left_x, left_y) = axes(&g.leftThumbstick());
        let (right_x, right_y) = axes(&g.rightThumbstick());
        PadState {
            buttons,
            left_x,
            left_y,
            right_x,
            right_y,
            left_trigger: super::trigger(g.leftTrigger().value() as f64),
            right_trigger: super::trigger(g.rightTrigger().value() as f64),
            motion: None,
            touch: None,
        }
    }
}
