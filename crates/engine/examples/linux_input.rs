//! Destructive only to an isolated test server: refuses any DISPLAY but :9.
//! `linux_input [--keys]` verifies XTest against an independent X connection.
//! `--keys` also exercises VK mapping once the parallel keymap work lands.
#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    #[cfg(target_os = "linux")]
    if latch_engine::linux::run_helper() {
        return Ok(());
    }
    use anyhow::ensure;
    use latch_engine::input::{DisplayRect, Event};
    use latch_engine::software::{Bgra, CpuCapture};
    use std::time::{Duration, Instant};
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::ConnectionExt as _;
    use x11rb::protocol::{xproto, Event as XEvent};
    use x11rb::rust_connection::RustConnection;

    struct Cleanup {
        conn: RustConnection,
        window: u32,
        previous: Option<latch_engine::display::Previous>,
    }
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = self.conn.destroy_window(self.window);
            let _ = self.conn.flush();
            if let Some(previous) = &self.previous {
                if let Err(error) = latch_engine::linux::restore_display(previous) {
                    eprintln!("restore failed: {error:#}");
                }
            }
        }
    }
    let display = std::env::var("DISPLAY")?;
    ensure!(
        display == ":9" || display == ":9.0",
        "this test only injects and resizes DISPLAY=:9"
    );
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|s| s == "--text-only") {
        let text = args.get(2).ok_or_else(|| anyhow::anyhow!("supply text"))?;
        let mut sink = latch_engine::linux::open_input()?;
        sink.apply(&Event::Text(text.clone()), DisplayRect::default())?;
        return Ok(());
    }
    let (conn, screen) = x11rb::connect(None)?;
    let root = conn.setup().roots[screen].root;
    let original = conn.get_geometry(root)?.reply()?;
    let mut resized_capture = latch_engine::linux::X11Capture::new()?;
    let mut resized_frame = Bgra::default();
    resized_capture.capture_now(&mut resized_frame)?;
    let previous = latch_engine::linux::prepare_display(1280, 720, 60)?;
    let window = conn.generate_id()?;
    let mut cleanup = Cleanup {
        conn,
        window,
        previous,
    };
    ensure!(
        (
            cleanup.conn.get_geometry(root)?.reply()?.width,
            cleanup.conn.get_geometry(root)?.reply()?.height
        ) == (1280, 720),
        "existing-mode resize failed"
    );
    resized_capture.capture_now(&mut resized_frame)?;
    ensure!(
        (resized_frame.width, resized_frame.height) == (1280, 720),
        "capture missed shrink"
    );
    latch_engine::linux::prepare_display(3024, 1964, 60)?;
    resized_capture.capture_now(&mut resized_frame)?;
    ensure!(
        (resized_frame.width, resized_frame.height) == (3024, 1964),
        "capture missed grow"
    );
    ensure!(
        (
            cleanup.conn.get_geometry(root)?.reply()?.width,
            cleanup.conn.get_geometry(root)?.reply()?.height
        ) == (3024, 1964),
        "created-mode resize failed"
    );
    if let Some(previous) = &cleanup.previous {
        latch_engine::linux::restore_display(previous)?;
    }
    cleanup.previous = None;
    resized_capture.capture_now(&mut resized_frame)?;
    ensure!(
        (resized_frame.width, resized_frame.height)
            == (original.width.into(), original.height.into()),
        "capture missed restore"
    );
    drop(resized_capture);
    let restored = cleanup.conn.get_geometry(root)?.reply()?;
    ensure!(
        (restored.width, restored.height) == (original.width, original.height),
        "restore failed"
    );
    println!(
        "RandR: existing 1280x720, created 3024x1964_60, restored {}x{}",
        restored.width, restored.height
    );
    cleanup
        .conn
        .create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            window,
            root,
            0,
            0,
            restored.width,
            restored.height,
            0,
            xproto::WindowClass::INPUT_OUTPUT,
            x11rb::COPY_FROM_PARENT,
            &xproto::CreateWindowAux::new()
                .background_pixel(0x204080)
                .event_mask(
                    xproto::EventMask::KEY_PRESS
                        | xproto::EventMask::KEY_RELEASE
                        | xproto::EventMask::BUTTON_PRESS
                        | xproto::EventMask::BUTTON_RELEASE
                        | xproto::EventMask::POINTER_MOTION,
                ),
        )?
        .check()?;
    cleanup.conn.map_window(window)?.check()?;
    cleanup
        .conn
        .set_input_focus(xproto::InputFocus::PARENT, window, x11rb::CURRENT_TIME)?
        .check()?;
    let min = cleanup.conn.setup().min_keycode;
    let count = cleanup.conn.setup().max_keycode - min + 1;
    let before = cleanup.conn.get_keyboard_mapping(min, count)?.reply()?;
    let mut sink = latch_engine::linux::open_input()?;
    let rect = DisplayRect {
        x: 0,
        y: 0,
        width: restored.width.into(),
        height: restored.height.into(),
    };
    sink.apply(
        &Event::MousePosition {
            x: 100,
            y: 100,
            width: restored.width,
            height: restored.height,
        },
        rect,
    )?;
    sink.apply(&Event::MouseMove { dx: 23, dy: -17 }, rect)?;
    let pointer = cleanup.conn.query_pointer(root)?.reply()?;
    ensure!(
        (pointer.root_x, pointer.root_y) == (123, 83),
        "pointer landed at {},{}",
        pointer.root_x,
        pointer.root_y
    );
    for button in 1..=5 {
        sink.apply(&Event::MouseButton { button, down: true }, rect)?;
    }
    sink.release_all();
    let pointer = cleanup.conn.query_pointer(root)?.reply()?;
    ensure!(
        !pointer.mask.intersects(
            xproto::KeyButMask::BUTTON1 | xproto::KeyButMask::BUTTON2 | xproto::KeyButMask::BUTTON3
        ),
        "buttons left held"
    );
    sink.apply(
        &Event::Scroll {
            vertical: 60,
            horizontal: 60,
        },
        rect,
    )?;
    sink.apply(
        &Event::Scroll {
            vertical: 60,
            horizontal: 60,
        },
        rect,
    )?;
    sink.apply(&Event::Text("Aéλ".into()), rect)?;
    let test_keys = std::env::args().any(|s| s == "--keys");
    if test_keys {
        sink.apply(
            &Event::Key {
                vk: 0x41,
                down: true,
                modifiers: 0,
            },
            rect,
        )?;
        sink.release_all();
    }
    cleanup.conn.get_input_focus()?.reply()?;
    let after = cleanup.conn.get_keyboard_mapping(min, count)?.reply()?;
    ensure!(
        before.keysyms == after.keysyms,
        "temporary Unicode mapping not restored"
    );
    let mut keys = [0usize; 2];
    let mut buttons = [0usize; 10];
    let mut released = [0usize; 10];
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        while let Some(event) = cleanup.conn.poll_for_event()? {
            match event {
                XEvent::KeyPress(_) => keys[0] += 1,
                XEvent::KeyRelease(_) => keys[1] += 1,
                XEvent::ButtonPress(e) => buttons[usize::from(e.detail)] += 1,
                XEvent::ButtonRelease(e) => released[usize::from(e.detail)] += 1,
                XEvent::Error(e) => anyhow::bail!("test receiver X error {e:?}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    ensure!(
        keys[0] >= 3 && keys[0] == keys[1],
        "unbalanced key events: {keys:?}"
    );
    for b in [1, 2, 3, 4, 7, 8, 9] {
        ensure!(
            buttons[b] == 1 && released[b] == 1,
            "button {b}: {} presses {} releases",
            buttons[b],
            released[b]
        );
    }
    let mut capture = latch_engine::linux::X11Capture::new()?;
    let mut frame = Bgra::default();
    capture.capture_now(&mut frame)?;
    let pixel = &frame.data[400 * frame.stride + 500 * 4..400 * frame.stride + 500 * 4 + 3];
    ensure!(
        pixel == [0x80, 0x40, 0x20],
        "captured known pixel differs: {pixel:?}"
    );
    ensure!(
        !capture.grab(&mut frame, Duration::from_millis(20))?,
        "unchanged desktop reported damage"
    );
    sink.apply(&Event::MouseMove { dx: 10, dy: 0 }, rect)?;
    ensure!(
        capture.grab(&mut frame, Duration::from_millis(50))?,
        "cursor move did not change capture"
    );
    let gc = cleanup.conn.generate_id()?;
    cleanup
        .conn
        .create_gc(gc, window, &xproto::CreateGCAux::new().foreground(0xabcdef))?
        .check()?;
    cleanup
        .conn
        .poly_fill_rectangle(
            window,
            gc,
            &[xproto::Rectangle {
                x: 500,
                y: 400,
                width: 8,
                height: 8,
            }],
        )?
        .check()?;
    ensure!(
        capture.grab(&mut frame, Duration::from_millis(50))?,
        "window drawing did not damage capture"
    );
    ensure!(
        frame.data[400 * frame.stride + 500 * 4..400 * frame.stride + 500 * 4 + 3]
            == [0xef, 0xcd, 0xab],
        "damaged pixels were not updated"
    );
    cleanup.conn.free_gc(gc)?.check()?;
    println!("XTest: pointer=123,83; keys={keys:?}; buttons={buttons:?}; releases={released:?}; Unicode mapping restored");
    println!(
        "Capture: known BGR pixel verified, idle reports no change, cursor motion reports change"
    );
    Ok(())
}
#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("linux_input requires Linux/X11");
}
