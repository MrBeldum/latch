//! The viewer's microphone, played where this machine's programs can
//! record it: a virtual input device. Linux makes one itself (a null sink
//! and a "Latch Microphone" source over it); on Windows and macOS sound
//! can only become a microphone through a virtual cable driver (VB-CABLE,
//! BlackHole), so the program embedding the server plays into that
//! cable's output with its own audio library ([`MicOpener`]).

use latch_proto::control::AudioFormat;

/// What a viewer's microphone is asked to send: 5 ms of 48 kHz mono, whose
/// worst case (485 bytes) fits a datagram with room to spare.
pub fn format() -> AudioFormat {
    AudioFormat {
        rate: crate::audio::RATE,
        channels: 1,
        frame_samples: crate::audio::FRAME as u32,
        codec: "lpcm-rice".into(),
    }
}

/// Takes one coded frame, in order; an empty one for each lost frame.
pub type MicPush = Box<dyn FnMut(&[u8]) + Send>;

/// Plays one session's microphone.
pub struct MicOut {
    pub describe: String,
    pub push: MicPush,
}

/// Opens a [`MicOut`] for a format, where the engine has no player of its
/// own for this system.
pub type MicOpener = Box<dyn Fn(&AudioFormat) -> anyhow::Result<MicOut> + Send + Sync>;

/// A player for a viewer's microphone: this system's own, else `opener`'s.
pub fn open(opener: Option<&MicOpener>) -> anyhow::Result<MicOut> {
    #[cfg(target_os = "linux")]
    {
        let _ = opener;
        crate::linux::open_mic(&format())
    }
    #[cfg(not(target_os = "linux"))]
    match opener {
        Some(open) => open(&format()),
        None => anyhow::bail!("this host cannot play a microphone"),
    }
}
