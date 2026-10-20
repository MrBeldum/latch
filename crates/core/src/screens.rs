//! Pixel sizes of the screens a Mac can have, and the sizes a stream asks
//! for. The Mac asks the PC for its own screen or for one of the standard
//! 16:9 sizes, and a PC whose only display is virtual can only switch to a
//! size that display lists. Setup on the PC lists these.

/// The standard sizes behind the quality choices, smallest first: 540p,
/// 720p and 900p for a PC that cannot encode many pixels quickly (software
/// encoding on a small CPU) or a network that cannot carry them, then
/// 1080p, 1440p and 4K.
pub const STANDARD: [(u32, u32); 6] = [
    (960, 540),
    (1280, 720),
    (1600, 900),
    (1920, 1080),
    (2560, 1440),
    (3840, 2160),
];

/// Native pixel sizes of Apple displays since 2015, then the common
/// external monitors. Points times the native scale factor, whatever
/// "looks like" resolution the Mac is set to.
pub const SCREENS: &[(u32, u32)] = &[
    (2304, 1440), // MacBook 12"
    (2560, 1600), // MacBook Pro 13" 2016–2022, MacBook Air 13" 2018–2020
    (2560, 1664), // MacBook Air 13" M2 and later
    (2880, 1800), // MacBook Pro 15" 2015–2019
    (2880, 1864), // MacBook Air 15"
    (3024, 1964), // MacBook Pro 14"
    (3072, 1920), // MacBook Pro 16" 2019
    (3456, 2234), // MacBook Pro 16" 2021 and later
    (4096, 2304), // iMac 21.5" 4K
    (4480, 2520), // iMac 24"
    (5120, 2880), // iMac 27" 5K, Studio Display
    (6016, 3384), // Pro Display XDR
    (1920, 1080),
    (1920, 1200),
    (2560, 1080),
    (2560, 1440),
    (3440, 1440),
    (3840, 1600),
    (3840, 2160),
    (5120, 1440),
    (5120, 2160),
];

/// `native` with both sides even (video codecs need that) and never
/// smaller than a codec can be asked for.
pub fn even(native: (u32, u32)) -> (u32, u32) {
    let round = |v: u32| (v / 2 * 2).max(2);
    (round(native.0), round(native.1))
}

/// Every size a Mac may ask for: each screen as it is and the standard
/// sizes, without repeats, largest first.
pub fn stream_modes() -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = SCREENS
        .iter()
        .chain(STANDARD.iter())
        .map(|&s| even(s))
        .collect();
    out.sort_by(|a, b| (b.0 * b.1).cmp(&(a.0 * a.1)).then(b.cmp(a)));
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn even_rounds_down_and_never_reaches_zero() {
        assert_eq!(even((3024, 1964)), (3024, 1964));
        assert_eq!(even((3025, 1965)), (3024, 1964));
        assert_eq!(even((0, 0)), (2, 2), "a degenerate screen is not a panic");
    }

    #[test]
    fn stream_modes_cover_every_screen_and_standard_size_without_repeats() {
        let modes = stream_modes();
        assert!(modes.contains(&(3024, 1964)));
        for s in STANDARD {
            assert!(modes.contains(&s), "{s:?} is a listed mode");
        }
        assert!(
            !modes.contains(&(1920, 1246)),
            "sizes scaled to a screen's shape are no longer asked for"
        );
        let mut sorted = modes.clone();
        sorted.dedup();
        assert_eq!(sorted, modes, "no repeats");
        assert_eq!(modes[0], (6016, 3384), "largest first");
        assert!(
            modes.len() < 90,
            "{} modes is more than a display driver should be asked to list",
            modes.len()
        );
    }
}
