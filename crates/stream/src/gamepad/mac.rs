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
use objc2_game_controller::{
    GCController, GCControllerButtonInput, GCControllerDirectionPad, GCControllerElement, GCDevice,
    GCDualSenseGamepad, GCDualShockGamepad, GCExtendedGamepad, GCXboxGamepad,
};
use std::ptr::NonNull;
use std::sync::Arc;
use std::time::Duration;

/// How often the list of controllers is looked at.
const SCAN: Duration = Duration::from_millis(250);

struct Mac;

impl Backend for Mac {
    // GameController's rumble goes through Core Haptics engines per pad
    // (`GCDeviceHaptics`); not wired up yet, so the host's rumble is
    // dropped on a Mac.
    fn rumble(&self, _slot: u8, _feedback: Feedback) {}
}

pub(super) fn start(shared: Arc<Shared>) -> Option<Box<dyn Backend>> {
    std::thread::Builder::new()
        .name("gamepads".into())
        .spawn(move || scan(&shared))
        .ok()?;
    Some(Box::new(Mac))
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

fn scan(shared: &Arc<Shared>) {
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
