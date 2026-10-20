//! The two things the stream needs from AppKit itself.
//!
//! winit gives every Mac app a standard menu whose Hide (⌘H), Hide Others
//! (⌥⌘H) and Quit (⌘Q) items take those keys before the window sees them:
//! mid-stream, ⌘Q meant for the PC (Ctrl+Q, quit an app there) quit Latch
//! and ⌘H hid it. While the stream has the keyboard those items keep their
//! place in the menu but lose their shortcuts.
//!
//! And the pasteboard's change count, which tells whether the Mac's
//! clipboard has changed since the PC last copied something. And whether
//! Caps Lock is on: egui has neither a key nor a modifier for it.
//!
//! Elsewhere both do nothing.

/// The app menu items whose shortcuts the stream takes over.
#[cfg(any(target_os = "macos", test))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Item {
    Hide,
    HideOthers,
    Quit,
}

/// The key an item answers to: none while the stream owns the keys.
#[cfg(any(target_os = "macos", test))]
fn equivalent(item: Item, stream_owns_keys: bool) -> &'static str {
    match (item, stream_owns_keys) {
        (_, true) => "",
        (Item::Hide | Item::HideOthers, false) => "h",
        (Item::Quit, false) => "q",
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use super::{equivalent, Item};
    use objc2::sel;
    use objc2_app_kit::{NSApplication, NSEvent, NSEventModifierFlags, NSPasteboard};
    use objc2_foundation::{MainThreadMarker, NSString};
    use std::cell::Cell;

    thread_local! {
        /// What the menu was last set to, so AppKit is only asked on a change.
        static OWNED: Cell<Option<bool>> = const { Cell::new(None) };
    }

    pub fn stream_owns_keys(owned: bool) {
        // Menus belong to the main thread; tests run elsewhere.
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        if OWNED.get() == Some(owned) {
            return;
        }
        OWNED.set(Some(owned));
        let app = NSApplication::sharedApplication(mtm);
        // SAFETY: plain AppKit getters and setters on the main thread.
        unsafe {
            let Some(bar) = app.mainMenu() else { return };
            let Some(menu) = bar.itemAtIndex(0).and_then(|i| i.submenu()) else {
                return;
            };
            for i in 0..menu.numberOfItems() {
                let Some(entry) = menu.itemAtIndex(i) else {
                    continue;
                };
                let item = match entry.action() {
                    Some(a) if a == sel!(hide:) => Item::Hide,
                    Some(a) if a == sel!(hideOtherApplications:) => Item::HideOthers,
                    Some(a) if a == sel!(terminate:) => Item::Quit,
                    _ => continue,
                };
                entry.setKeyEquivalent(&NSString::from_str(equivalent(item, owned)));
            }
        }
    }

    pub fn pasteboard_changes() -> Option<isize> {
        // SAFETY: NSPasteboard is usable from any thread; this only reads
        // its change count.
        Some(unsafe { NSPasteboard::generalPasteboard().changeCount() })
    }

    pub fn caps_lock() -> Option<bool> {
        // SAFETY: a class getter for the modifier keys' state right now,
        // usable from any thread.
        let flags = unsafe { NSEvent::modifierFlags_class() };
        Some(flags.contains(NSEventModifierFlags::NSEventModifierFlagCapsLock))
    }
}

#[cfg(not(target_os = "macos"))]
mod imp {
    pub fn stream_owns_keys(_owned: bool) {}

    pub fn pasteboard_changes() -> Option<isize> {
        None
    }

    pub fn caps_lock() -> Option<bool> {
        None
    }
}

/// While `owned`, ⌘Q, ⌘H and ⌥⌘H go to the stream (and so to the PC)
/// instead of quitting or hiding Latch. Cheap to call every frame.
pub fn stream_owns_keys(owned: bool) {
    imp::stream_owns_keys(owned);
}

/// The Mac clipboard's change count: it moves whenever anything is copied.
/// None where there is no such count.
pub fn pasteboard_changes() -> Option<isize> {
    imp::pasteboard_changes()
}

/// Whether Caps Lock is on on this Mac. None where that is not known.
pub fn caps_lock() -> Option<bool> {
    imp::caps_lock()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_stream_takes_quit_and_hide_and_gives_them_back() {
        for item in [Item::Hide, Item::HideOthers, Item::Quit] {
            assert_eq!(equivalent(item, true), "");
        }
        assert_eq!(equivalent(Item::Hide, false), "h");
        // Hide Others keeps its Option bit in the item's modifier mask.
        assert_eq!(equivalent(Item::HideOthers, false), "h");
        assert_eq!(equivalent(Item::Quit, false), "q");
        // Off the main thread (as here) it does nothing, and says so safely.
        stream_owns_keys(true);
        stream_owns_keys(false);
    }

    #[test]
    fn caps_lock_is_known_on_a_mac() {
        // Read through the window server, from any thread.
        assert_eq!(caps_lock().is_some(), cfg!(target_os = "macos"));
    }
}
