//! This machine's microphone, sent to the host when the person turned it
//! on and the host has somewhere to play it (`Accept.mic`): the default
//! input device, mixed to mono and resampled to the host's 48 kHz, coded
//! lossless in 5 ms frames (`lpcm-rice`) with the same parity as the
//! host's sound, as datagram kind 9. macOS asks the person once whether
//! Latch may use the microphone; until they agree the device delivers
//! silence.

use crate::audio::keep_open;
use anyhow::{bail, Result};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use latch_proto::audio::AudioPacketizer;
use latch_proto::control::AudioFormat;
use parking_lot::Mutex;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::Duration;

/// How soon a missing or refusing input device is tried again.
const INPUT_RETRY: Duration = Duration::from_secs(2);

/// The input device, as the input thread found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MicState {
    Opening,
    Sending { device: String },
    Failed(String),
}

/// Sends the microphone while it lives.
pub struct Mic {
    _stop: mpsc::Sender<()>,
    state: Arc<Mutex<MicState>>,
}

impl Mic {
    /// Starts sending the default input device to `host` as `format`
    /// asks (48 kHz mono `lpcm-rice` in Latch 5.2 hosts).
    pub fn start(
        format: &AudioFormat,
        session: u32,
        udp: UdpSocket,
        host: SocketAddr,
    ) -> Result<Self> {
        if format.codec != "lpcm-rice"
            || format.channels != 1
            || format.frame_samples == 0
            || format.frame_samples as usize > latch_codec::audio::MAX_SAMPLES
            || !(8_000..=192_000).contains(&format.rate)
        {
            bail!("the host wants a microphone format Latch does not send: {format:?}");
        }
        let (stop, stopped) = mpsc::channel();
        let state = Arc::new(Mutex::new(MicState::Opening));
        let shared = Sender {
            packetizer: AudioPacketizer::mic(session),
            udp,
            host,
            frame: format.frame_samples as usize,
            rate: format.rate,
            pending: Vec::new(),
            coded: Vec::new(),
            sent: 0,
        };
        let shared = Arc::new(Mutex::new(shared));
        {
            let state = state.clone();
            std::thread::Builder::new()
                .name("mic-in".into())
                .spawn(move || {
                    let rebuild = Arc::new(AtomicBool::new(false));
                    keep_open(&stopped, &rebuild, INPUT_RETRY, || {
                        open_input(&shared, &state, &rebuild)
                    });
                })?;
        }
        Ok(Self { _stop: stop, state })
    }

    pub fn state(&self) -> MicState {
        self.state.lock().clone()
    }
}

/// Frames, codes and sends what the device captures.
struct Sender {
    packetizer: AudioPacketizer,
    udp: UdpSocket,
    host: SocketAddr,
    frame: usize,
    rate: u32,
    pending: Vec<i16>,
    coded: Vec<u8>,
    sent: u64,
}

impl Sender {
    fn push(&mut self, samples: impl IntoIterator<Item = f32>) {
        self.pending.extend(
            samples
                .into_iter()
                .map(|s| (s.clamp(-1.0, 1.0) * 32767.0).round() as i16),
        );
        let mut at = 0;
        while self.pending.len() - at >= self.frame {
            self.coded.clear();
            latch_codec::audio::encode_channels(
                &self.pending[at..at + self.frame],
                1,
                &mut self.coded,
            );
            at += self.frame;
            let pts = (self.sent * 1_000_000 / u64::from(self.rate)) as u32;
            self.sent += self.frame as u64;
            let (udp, host) = (&self.udp, self.host);
            self.packetizer.packetize(pts, &self.coded, |datagram| {
                let _ = udp.send_to(datagram, host);
            });
        }
        self.pending.drain(..at);
    }
}

/// Mono at the device's rate to mono at the host's, linearly.
struct Resample {
    step: f64,
    pos: f64,
    last: f32,
}

impl Resample {
    fn new(from: u32, to: u32) -> Self {
        Self {
            step: f64::from(from.max(1)) / f64::from(to.max(1)),
            pos: 0.0,
            last: 0.0,
        }
    }

    fn push(&mut self, mono: impl Iterator<Item = f32>, out: &mut Vec<f32>) {
        for x in mono {
            while self.pos < 1.0 {
                out.push(self.last + (x - self.last) * self.pos as f32);
                self.pos += self.step;
            }
            self.pos -= 1.0;
            self.last = x;
        }
    }
}

fn open_input(
    shared: &Arc<Mutex<Sender>>,
    state: &Arc<Mutex<MicState>>,
    rebuild: &Arc<AtomicBool>,
) -> Option<cpal::Stream> {
    let fail = |why: String| {
        let mut s = state.lock();
        if !matches!(&*s, MicState::Failed(w) if *w == why) {
            tracing::warn!("microphone: {why}");
        }
        *s = MicState::Failed(why);
    };
    let Some(device) = cpal::default_host().default_input_device() else {
        fail("this machine has no microphone".into());
        return None;
    };
    let name = device.to_string();
    let supported = match device.default_input_config() {
        Ok(c) => c,
        Err(e) => {
            fail(format!("{name} has no input configuration: {e}"));
            return None;
        }
    };
    let rate = supported.sample_rate().max(8_000);
    let channels = usize::from(supported.channels().max(1));
    let config = cpal::StreamConfig {
        channels: channels as u16,
        sample_rate: rate,
        buffer_size: cpal::BufferSize::Default,
    };
    let target = shared.lock().rate;
    let err_fn = {
        let rebuild = rebuild.clone();
        move |e: cpal::Error| {
            if matches!(
                e.kind(),
                cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::StreamInvalidated
            ) {
                tracing::info!("microphone: {e}; opening the default input again");
                rebuild.store(true, Ordering::Relaxed);
            } else {
                tracing::warn!("microphone: {e}");
            }
        }
    };
    // Every format arrives here as f32 frames of the device's channels.
    let consume = {
        let shared = shared.clone();
        let mut resample = Resample::new(rate, target);
        let mut out = Vec::new();
        move |data: &[f32]| {
            out.clear();
            let mono = data
                .chunks_exact(channels)
                .map(|f| f.iter().sum::<f32>() / channels as f32);
            resample.push(mono, &mut out);
            shared.lock().push(out.iter().copied());
        }
    };
    let stream = match supported.sample_format() {
        cpal::SampleFormat::I16 => {
            let mut consume = consume;
            let mut f = Vec::new();
            device.build_input_stream(
                config,
                move |data: &[i16], _: &cpal::InputCallbackInfo| {
                    f.clear();
                    f.extend(data.iter().map(|&s| f32::from(s) / 32768.0));
                    consume(&f)
                },
                err_fn,
                None,
            )
        }
        cpal::SampleFormat::I32 => {
            let mut consume = consume;
            let mut f = Vec::new();
            device.build_input_stream(
                config,
                move |data: &[i32], _: &cpal::InputCallbackInfo| {
                    f.clear();
                    f.extend(data.iter().map(|&s| s as f32 / 2_147_483_648.0));
                    consume(&f)
                },
                err_fn,
                None,
            )
        }
        _ => {
            let mut consume = consume;
            device.build_input_stream(
                config,
                move |data: &[f32], _: &cpal::InputCallbackInfo| consume(data),
                err_fn,
                None,
            )
        }
    };
    let stream = match stream {
        Ok(s) => s,
        Err(e) => {
            fail(format!(
                "{name} refused {rate} Hz, {channels} channels: {e}"
            ));
            return None;
        }
    };
    if let Err(e) = stream.play() {
        fail(format!("{name} would not start: {e}"));
        return None;
    }
    tracing::info!("microphone: sending {name} ({rate} Hz, {channels} channels)");
    *state.lock() = MicState::Sending { device: name };
    Some(stream)
}

#[cfg(test)]
mod tests {
    use super::*;
    use latch_proto::audio::{AudioOut, AudioReceiver};
    use latch_proto::wire::{self, Datagram};
    use std::time::Instant;

    #[test]
    fn captured_sound_arrives_as_mono_frames_the_host_puts_back_in_order() {
        let socket = UdpSocket::bind("127.0.0.1:0").unwrap();
        socket
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut sender = Sender {
            packetizer: AudioPacketizer::mic(7),
            udp: UdpSocket::bind("127.0.0.1:0").unwrap(),
            host: socket.local_addr().unwrap(),
            frame: 240,
            rate: 48_000,
            pending: Vec::new(),
            coded: Vec::new(),
            sent: 0,
        };
        // 44.1 kHz stereo device: a ramp, resampled and mixed.
        let mut resample = Resample::new(44_100, 48_000);
        let stereo: Vec<f32> = (0..4410).map(|i| (i / 2) as f32 / 4410.0).collect();
        let mut out = Vec::new();
        resample.push(stereo.chunks(2).map(|f| (f[0] + f[1]) / 2.0), &mut out);
        assert!((out.len() as i64 - 2400).abs() <= 1, "{}", out.len());
        sender.push(out.iter().copied());
        // Ten 5 ms frames and two parity packets.
        let mut receiver = AudioReceiver::with_format(7, 240, 48_000);
        let mut buf = [0u8; 1500];
        let (mut frames, mut parity) = (0, 0);
        while frames + parity < 12 {
            let n = socket.recv(&mut buf).unwrap();
            let Ok(Datagram::Mic { packet, .. }) = wire::parse(&buf[..n]) else {
                panic!("not a microphone datagram");
            };
            if packet.parity {
                parity += 1;
            } else {
                frames += 1;
            }
            receiver.push(&packet, Instant::now());
        }
        let mut pcm = Vec::new();
        let mut last = i16::MIN;
        for _ in 0..10 {
            let Some(AudioOut::Frame { payload, .. }) = receiver.pop(Instant::now()) else {
                panic!("a frame went missing");
            };
            assert_eq!(
                latch_codec::audio::decode_channels(&payload, &mut pcm),
                Ok(1)
            );
            assert_eq!(pcm.len(), 240);
            assert!(pcm[0] >= last && pcm.windows(2).all(|w| w[1] >= w[0]));
            last = pcm[239];
        }
    }
}
