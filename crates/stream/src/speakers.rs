//! Which speaker each channel of an output device plays on, as the OS says,
//! so [`latch_codec::audio::mix_onto`] can route each channel of the
//! stream (WAVE order: FL FR FC LFE BL BR SL SR) to its own speaker. The
//! orders differ: CoreAudio's 7.1 is L R C LFE Ls Rs Rls Rrs (sides
//! before backs), ALSA's 5.1 is FL FR RL RR FC LFE (centre and LFE last),
//! and a Windows endpoint names its speakers in its mix format's channel
//! mask. A device that names none is taken as Latch's own layout.

use latch_codec::audio::{speakers, Speaker};

/// The speaker of each of the `channels` channels `device` is opened with.
pub(crate) fn of_device(device: &cpal::Device, channels: usize) -> Vec<Speaker> {
    use cpal::traits::DeviceTrait;
    let id = device.id().ok();
    let named = id.and_then(|id| os::speakers(id.id(), channels));
    fit(named, channels)
}

/// `named` cut or padded to `channels`; Latch's own layout when it names
/// no speaker.
pub(crate) fn fit(named: Option<Vec<Speaker>>, channels: usize) -> Vec<Speaker> {
    match named {
        Some(mut s) if s.iter().any(Option::is_some) => {
            s.resize(channels, None);
            s
        }
        _ => speakers(channels),
    }
}

/// How the layout reads in the log: "FL FR FC LFE SL SR BL BR".
pub(crate) fn describe(layout: &[Speaker]) -> String {
    const NAMES: [&str; 18] = [
        "FL", "FR", "FC", "LFE", "BL", "BR", "FLC", "FRC", "BC", "SL", "SR", "TC", "TFL", "TFC",
        "TFR", "TBL", "TBC", "TBR",
    ];
    layout
        .iter()
        .map(|s| {
            s.and_then(|b| NAMES.get(b as usize).copied())
                .unwrap_or("-")
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A WAVEFORMATEXTENSIBLE channel mask: its set bits, lowest first, are
/// the channels' speakers; channels past them have none.
#[cfg_attr(not(windows), allow(dead_code))]
pub(crate) fn from_mask(mask: u32, channels: usize) -> Vec<Speaker> {
    let mut out: Vec<Speaker> = (0..18u8)
        .filter(|b| mask & (1 << b) != 0)
        .map(Some)
        .collect();
    out.resize(channels, None);
    out
}

/// CoreAudio channel labels (`AudioChannelLabel`). Left Surround and Right
/// Surround are the back pair (as in 5.1) unless the layout also has Rear
/// Surround Left/Right; then, as in CoreAudio's 7.1, they are the sides.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn from_core_audio(labels: &[u32]) -> Vec<Speaker> {
    const LEFT_SURROUND: u32 = 5;
    const REAR_SURROUND_LEFT: u32 = 33;
    let rears = labels.contains(&REAR_SURROUND_LEFT);
    labels
        .iter()
        .map(|&label| match label {
            LEFT_SURROUND | 6 if rears => Some(label as u8 + 4), // SL, SR
            // Left … Top Back Right: the WAVE bit one below.
            1..=18 => Some(label as u8 - 1),
            REAR_SURROUND_LEFT => Some(4),
            34 => Some(5),
            38 => Some(0),  // Left Total
            39 => Some(1),  // Right Total
            42 => Some(2),  // Mono
            301 => Some(0), // Headphones Left
            302 => Some(1), // Headphones Right
            _ => None,
        })
        .collect()
}

/// ALSA channel-map positions (`SND_CHMAP_*`), which PulseAudio's and
/// PipeWire's ALSA plugins report as their sinks' channel maps.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn from_alsa(positions: &[u32]) -> Vec<Speaker> {
    positions
        .iter()
        .map(|&p| match p {
            2 => Some(2),   // mono
            3 => Some(0),   // FL
            4 => Some(1),   // FR
            5 => Some(4),   // RL
            6 => Some(5),   // RR
            7 => Some(2),   // FC
            8 => Some(3),   // LFE
            9 => Some(9),   // SL
            10 => Some(10), // SR
            11 => Some(8),  // RC
            12 => Some(6),  // FLC
            13 => Some(7),  // FRC
            18 => Some(12), // FLH
            19 => Some(13), // FCH
            20 => Some(14), // FRH
            21 => Some(11), // TC
            22 => Some(12), // TFL
            23 => Some(14), // TFR
            24 => Some(13), // TFC
            25 => Some(15), // TRL
            26 => Some(17), // TRR
            27 => Some(16), // TRC
            _ => None,
        })
        .collect()
}

/// ALSA's order when a device has no channel map to ask (the `pulse`
/// plugin before it could say, a driver without one): FL FR RL RR FC LFE
/// SL SR, cut to the count.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn alsa_default(channels: usize) -> Vec<Speaker> {
    let order: &[u32] = match channels {
        1 => &[2],
        2 | 3 => &[3, 4],
        4 | 5 => &[3, 4, 5, 6],
        6 | 7 => &[3, 4, 5, 6, 7, 8],
        _ => &[3, 4, 5, 6, 7, 8, 9, 10],
    };
    let mut out = from_alsa(order);
    out.resize(channels, None);
    out
}

#[cfg(target_os = "macos")]
mod os {
    use super::*;
    use std::ffi::c_void;

    type OSStatus = i32;
    const fn code(s: &[u8; 4]) -> u32 {
        u32::from_be_bytes(*s)
    }
    const SYSTEM_OBJECT: u32 = 1;
    const SCOPE_GLOBAL: u32 = code(b"glob");
    const SCOPE_OUTPUT: u32 = code(b"outp");
    const TRANSLATE_UID_TO_DEVICE: u32 = code(b"uidd");
    const PREFERRED_CHANNEL_LAYOUT: u32 = code(b"srnd");
    const PREFERRED_CHANNELS_FOR_STEREO: u32 = code(b"dch2");
    const CHANNEL_LAYOUT_FOR_TAG: u32 = code(b"cmpl");
    const CHANNEL_LAYOUT_FOR_BITMAP: u32 = code(b"cmpb");
    const TAG_USE_DESCRIPTIONS: u32 = 0;
    const TAG_USE_BITMAP: u32 = 1 << 16;
    const UTF8: u32 = 0x0800_0100;
    /// `AudioChannelLayout` up to its descriptions; each description is
    /// 20 bytes with its label first.
    const LAYOUT_HEADER: usize = 12;
    const DESCRIPTION: usize = 20;

    #[repr(C)]
    struct Address {
        selector: u32,
        scope: u32,
        element: u32,
    }

    #[link(name = "CoreAudio", kind = "framework")]
    unsafe extern "C" {
        fn AudioObjectGetPropertyDataSize(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
        ) -> OSStatus;
        fn AudioObjectGetPropertyData(
            object: u32,
            address: *const Address,
            qualifier_size: u32,
            qualifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> OSStatus;
    }
    #[link(name = "AudioToolbox", kind = "framework")]
    unsafe extern "C" {
        fn AudioFormatGetPropertyInfo(
            property: u32,
            specifier_size: u32,
            specifier: *const c_void,
            size: *mut u32,
        ) -> OSStatus;
        fn AudioFormatGetProperty(
            property: u32,
            specifier_size: u32,
            specifier: *const c_void,
            size: *mut u32,
            data: *mut c_void,
        ) -> OSStatus;
    }
    #[link(name = "CoreFoundation", kind = "framework")]
    unsafe extern "C" {
        fn CFStringCreateWithBytes(
            alloc: *const c_void,
            bytes: *const u8,
            len: isize,
            encoding: u32,
            external: u8,
        ) -> *const c_void;
        fn CFRelease(cf: *const c_void);
    }

    fn device_for_uid(uid: &str) -> Option<u32> {
        // SAFETY: The CFString lives across the call and is released once;
        // the property writes one AudioObjectID into `device`.
        unsafe {
            let s = CFStringCreateWithBytes(
                std::ptr::null(),
                uid.as_ptr(),
                uid.len() as isize,
                UTF8,
                0,
            );
            if s.is_null() {
                return None;
            }
            let address = Address {
                selector: TRANSLATE_UID_TO_DEVICE,
                scope: SCOPE_GLOBAL,
                element: 0,
            };
            let mut device = 0u32;
            let mut size = 4u32;
            let status = AudioObjectGetPropertyData(
                SYSTEM_OBJECT,
                &address,
                std::mem::size_of::<*const c_void>() as u32,
                (&s as *const *const c_void).cast(),
                &mut size,
                (&mut device as *mut u32).cast(),
            );
            CFRelease(s);
            (status == 0 && device != 0).then_some(device)
        }
    }

    /// A device property of variable size, as bytes (u32-aligned).
    fn property(device: u32, selector: u32) -> Option<Vec<u32>> {
        let address = Address {
            selector,
            scope: SCOPE_OUTPUT,
            element: 0,
        };
        let mut size = 0u32;
        // SAFETY: The size query writes one u32; the data call writes at
        // most `size` bytes into a buffer of at least that many.
        unsafe {
            if AudioObjectGetPropertyDataSize(device, &address, 0, std::ptr::null(), &mut size) != 0
                || size == 0
            {
                return None;
            }
            let mut buf = vec![0u32; (size as usize).div_ceil(4)];
            (AudioObjectGetPropertyData(
                device,
                &address,
                0,
                std::ptr::null(),
                &mut size,
                buf.as_mut_ptr().cast(),
            ) == 0)
                .then_some(buf)
        }
    }

    /// AudioToolbox's expansion of a layout tag or bitmap into channel
    /// descriptions.
    fn expand(property: u32, specifier: u32) -> Option<Vec<u32>> {
        let mut size = 0u32;
        // SAFETY: As for `property`: the size first, then at most that.
        unsafe {
            if AudioFormatGetPropertyInfo(property, 4, (&specifier as *const u32).cast(), &mut size)
                != 0
                || size == 0
            {
                return None;
            }
            let mut buf = vec![0u32; (size as usize).div_ceil(4)];
            (AudioFormatGetProperty(
                property,
                4,
                (&specifier as *const u32).cast(),
                &mut size,
                buf.as_mut_ptr().cast(),
            ) == 0)
                .then_some(buf)
        }
    }

    /// The labels of an `AudioChannelLayout` that lists its descriptions.
    fn labels(layout: &[u32]) -> Option<Vec<u32>> {
        let count = *layout.get(2)? as usize;
        (0..count)
            .map(|i| layout.get((LAYOUT_HEADER + i * DESCRIPTION) / 4).copied())
            .collect()
    }

    pub(super) fn speakers(uid: &str, channels: usize) -> Option<Vec<Speaker>> {
        let device = device_for_uid(uid)?;
        if let Some(layout) = property(device, PREFERRED_CHANNEL_LAYOUT) {
            let tag = *layout.first()?;
            let described = match tag {
                TAG_USE_DESCRIPTIONS => Some(layout),
                TAG_USE_BITMAP => expand(CHANNEL_LAYOUT_FOR_BITMAP, *layout.get(1)?),
                tag => expand(CHANNEL_LAYOUT_FOR_TAG, tag),
            };
            if let Some(found) = described.as_deref().and_then(labels) {
                let found = from_core_audio(&found);
                if found.iter().any(Option::is_some) {
                    return Some(found);
                }
            }
        }
        // No layout: the pair a stereo stream plays on (1-based), which on
        // an interface may not be its first two channels.
        let pair = property(device, PREFERRED_CHANNELS_FOR_STEREO)?;
        let mut out = vec![None; channels];
        for (bit, &ch) in pair.iter().take(2).enumerate() {
            if let Some(s) = out.get_mut((ch as usize).wrapping_sub(1)) {
                *s = Some(bit as u8);
            }
        }
        Some(out)
    }
}

#[cfg(windows)]
mod os {
    use super::*;
    use windows::core::PCWSTR;
    use windows::Win32::Media::Audio::*;
    use windows::Win32::Media::KernelStreaming::WAVE_FORMAT_EXTENSIBLE;
    use windows::Win32::System::Com::*;

    /// The endpoint's mix format's channel mask: WASAPI hands the shared
    /// engine's channels to the endpoint's speakers in its order, and cpal
    /// opens with no mask of its own (direct out).
    pub(super) fn speakers(endpoint: &str, channels: usize) -> Option<Vec<Speaker>> {
        let wide: Vec<u16> = endpoint.encode_utf16().chain([0]).collect();
        // SAFETY: COM is initialized for this thread first (another mode
        // already set is fine); the mix format is read before it is freed.
        unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).ok()?;
            let device = enumerator.GetDevice(PCWSTR(wide.as_ptr())).ok()?;
            let client: IAudioClient = device.Activate(CLSCTX_ALL, None).ok()?;
            mask_speakers(&client, channels)
        }
    }

    /// The speakers of `client`'s mix format, when it names them.
    pub(crate) fn mask_speakers(client: &IAudioClient, channels: usize) -> Option<Vec<Speaker>> {
        // SAFETY: The format is copied out before CoTaskMemFree.
        unsafe {
            let format = client.GetMixFormat().ok()?;
            if format.is_null() {
                return None;
            }
            let wave = format.read_unaligned();
            let mask = (wave.wFormatTag == WAVE_FORMAT_EXTENSIBLE as u16 && wave.cbSize >= 22)
                .then(|| {
                    (format as *const WAVEFORMATEXTENSIBLE)
                        .read_unaligned()
                        .dwChannelMask
                });
            CoTaskMemFree(Some(format.cast()));
            Some(from_mask(mask.filter(|m| *m != 0)?, channels))
        }
    }
}

#[cfg(target_os = "linux")]
mod os {
    use super::*;
    use alsa::pcm::{Access, Format, HwParams, PCM};
    use alsa::Direction;

    /// The channel map ALSA (or the PulseAudio or PipeWire plugin behind
    /// `default`) reports for the PCM opened with `channels`; ALSA's own
    /// order where it reports none.
    pub(super) fn speakers(pcm_id: &str, channels: usize) -> Option<Vec<Speaker>> {
        Some(chmap(pcm_id, channels).unwrap_or_else(|| alsa_default(channels)))
    }

    fn chmap(pcm_id: &str, channels: usize) -> Option<Vec<Speaker>> {
        let pcm = PCM::new(pcm_id, Direction::Playback, true).ok()?;
        {
            let hw = HwParams::any(&pcm).ok()?;
            hw.set_channels(channels as u32).ok()?;
            hw.set_access(Access::RWInterleaved).ok()?;
            let _ = hw.set_format(Format::float());
            pcm.hw_params(&hw).ok()?;
        }
        let map: Vec<alsa::pcm::ChmapPosition> = (&pcm.get_chmap().ok()?).into();
        let positions: Vec<u32> = map.into_iter().map(|p| p as u32).collect();
        (positions.len() == channels).then(|| from_alsa(&positions))
    }
}

#[cfg(not(any(target_os = "macos", windows, target_os = "linux")))]
mod os {
    use super::*;
    pub(super) fn speakers(_: &str, _: usize) -> Option<Vec<Speaker>> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn core_audio_layouts_name_sides_and_backs_by_their_pairs() {
        // kAudioChannelLayoutTag_AudioUnit_7_1 / MPEG_7_1_C:
        // L R C LFE Ls Rs Rls Rrs, so Ls and Rs are the sides.
        assert_eq!(
            describe(&from_core_audio(&[1, 2, 3, 4, 5, 6, 33, 34])),
            "FL FR FC LFE SL SR BL BR"
        );
        // 5.1 (L R C LFE Ls Rs): the surrounds are WAVE's back pair.
        assert_eq!(
            describe(&from_core_audio(&[1, 2, 3, 4, 5, 6])),
            "FL FR FC LFE BL BR"
        );
        // WAVE_7_1 as CoreAudio spells it: L R C LFE Rls Rrs Ls Rs.
        assert_eq!(
            describe(&from_core_audio(&[1, 2, 3, 4, 33, 34, 5, 6])),
            "FL FR FC LFE BL BR SL SR"
        );
        // Surround Direct is the side pair; unknown and discrete labels
        // have no speaker; headphones are a pair.
        assert_eq!(
            describe(&from_core_audio(&[10, 11, 0xFFFF_FFFF, 0x10000, 301, 302])),
            "SL SR - - FL FR"
        );
    }

    #[test]
    fn alsa_maps_and_its_default_order() {
        // FL FR RL RR FC LFE SL SR
        assert_eq!(
            describe(&from_alsa(&[3, 4, 5, 6, 7, 8, 9, 10])),
            "FL FR BL BR FC LFE SL SR"
        );
        assert_eq!(describe(&alsa_default(6)), "FL FR BL BR FC LFE");
        assert_eq!(describe(&alsa_default(8)), "FL FR BL BR FC LFE SL SR");
        assert_eq!(describe(&alsa_default(2)), "FL FR");
        assert_eq!(describe(&alsa_default(3)), "FL FR -");
        assert_eq!(describe(&from_alsa(&[2, 0, 1])), "FC - -");
    }

    #[test]
    fn windows_masks_name_channels_in_bit_order() {
        assert_eq!(describe(&from_mask(0x63f, 8)), "FL FR FC LFE BL BR SL SR");
        // "5.1 Surround" on Windows: the side pair.
        assert_eq!(describe(&from_mask(0x60f, 6)), "FL FR FC LFE SL SR");
        // Fewer bits than channels: the rest play on no speaker.
        assert_eq!(describe(&from_mask(0x3, 4)), "FL FR - -");
    }

    #[test]
    fn a_device_naming_no_speaker_is_latchs_layout() {
        assert_eq!(fit(None, 6), speakers(6));
        assert_eq!(fit(Some(vec![None; 8]), 8), speakers(8));
        assert_eq!(
            fit(Some(vec![Some(0), Some(1)]), 4),
            [Some(0), Some(1), None, None]
        );
    }

    #[test]
    fn seven_one_reaches_core_audios_speakers_by_position() {
        // Review finding: WAVE's backs must not play on CoreAudio's sides.
        let device = from_core_audio(&[1, 2, 3, 4, 5, 6, 33, 34]);
        let rows = latch_codec::audio::mix_onto(8, 0x63f, &device);
        // rows[out][in]: device channel 4 (Ls, a side) takes WAVE SL (6),
        // device channel 6 (Rls, a back) takes WAVE BL (4).
        assert_eq!(rows[4][6], 1.0);
        assert_eq!(rows[6][4], 1.0);
        assert_eq!(rows[4][4], 0.0);
        // And ALSA's 5.1: WAVE's centre (2) on channel 4, LFE (3) on 5.
        let rows = latch_codec::audio::mix_onto(6, 0x3f, &alsa_default(6));
        assert_eq!((rows[4][2], rows[5][3], rows[2][4]), (1.0, 1.0, 1.0));
    }
}

#[cfg(test)]
mod devices {
    /// `cargo test -p latch-stream -- --ignored --nocapture speakers`:
    /// what each output device here is taken to be.
    #[test]
    #[ignore = "lists this machine's output devices"]
    fn list_output_speakers() {
        use cpal::traits::{DeviceTrait, HostTrait};
        for device in cpal::default_host().output_devices().unwrap() {
            let Ok(config) = device.default_output_config() else {
                continue;
            };
            let layout = super::of_device(&device, config.channels() as usize);
            println!("{device}: {}", super::describe(&layout));
        }
    }
}
