//! Shows what Latch's viewer reads from this machine's gamepads: each pad
//! as it connects, and every state change, with the wall clock in
//! microseconds; `--rumble` also rumbles each pad for half a second every
//! two seconds.
//!
//!     pads [--seconds 20] [--rumble] [--quiet]   (--quiet: changes of the
//!                                                 south button only)

use latch_stream::gamepad::{Feedback, Pads};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn wall_us() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_micros())
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let seconds: u64 = args
        .windows(2)
        .find(|w| w[0] == "--seconds")
        .and_then(|w| w[1].parse().ok())
        .unwrap_or(20);
    let rumble = args.iter().any(|a| a == "--rumble");
    let quiet = args.iter().any(|a| a == "--quiet");
    let pads = Pads::start();
    let south = Arc::new(parking_lot::Mutex::new([false; 16]));
    pads.attach(Some(Arc::new(move |slot, pad| match pad {
        None => println!("{} pad {slot} gone", wall_us()),
        Some((kind, state)) => {
            let a = state.buttons & latch_proto::gamepad::button::SOUTH != 0;
            let mut last = south.lock();
            if !quiet || last[slot as usize] != a {
                println!("{} pad {slot} {kind:?} {state:?}", wall_us());
            }
            last[slot as usize] = a;
        }
    })));
    let start = Instant::now();
    let mut listed = Vec::new();
    while start.elapsed() < Duration::from_secs(seconds) {
        let now = pads.connected();
        if now != listed {
            println!("{} connected: {now:?}", wall_us());
            listed = now;
        }
        if rumble {
            let on = start.elapsed().as_millis() % 2000 < 500;
            for pad in &listed {
                pads.rumble(
                    pad.slot,
                    Feedback {
                        large: if on { 40_000 } else { 0 },
                        small: if on { 20_000 } else { 0 },
                        ..Default::default()
                    },
                );
            }
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    pads.quiet();
}
