//! `pulseaudio` 0.3.2 speaks the native protocol entirely in Rust, including
//! authentication and recording. The server converts its monitor to our
//! exact format, so this also works with PipeWire's PulseAudio server and
//! needs neither libpulse nor a `parec` subprocess.

use crate::audio::{AudioCapture, Layout, CHANNELS, FRAME, RATE};
use anyhow::{ensure, Context};
use futures::executor::block_on;
use pulseaudio::{protocol, Client, ClientError, RecordStream};
use std::future::Future;
use std::pin::Pin;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::task::Poll;
use std::time::{Duration, Instant};

/// How often a recording that sends nothing is asked whether it still exists.
const PROBE_EVERY: Duration = Duration::from_secs(1);

type Probe = Pin<Box<dyn Future<Output = Result<(), ClientError>> + Send>>;

/// A suspended sink's monitor sends nothing, and a recording the server
/// killed (`pactl kill-source-output`, a removed source) also sends nothing:
/// this client ignores the kill message and keeps the stream's queue alive.
/// While nothing arrives, a latency query tells the two apart, because the
/// server answers it with an error for a stream it no longer has.
#[derive(Default)]
struct Health {
    probe: Option<Probe>,
    asked: Option<Instant>,
}
impl Health {
    /// Never blocks: an unanswered probe is polled again on the next call.
    fn check(&mut self, start: impl FnOnce() -> Probe) -> anyhow::Result<()> {
        if self.probe.is_none() && self.asked.is_none_or(|t| t.elapsed() >= PROBE_EVERY) {
            self.probe = Some(start());
            self.asked = Some(Instant::now());
        }
        let Some(probe) = &mut self.probe else {
            return Ok(());
        };
        let mut context = std::task::Context::from_waker(futures::task::noop_waker_ref());
        match probe.as_mut().poll(&mut context) {
            Poll::Pending => Ok(()),
            Poll::Ready(result) => {
                self.probe = None;
                result.context("PulseAudio ended the recording")
            }
        }
    }
}

/// Whole frames out of the server's byte batches.
struct Frames {
    bytes: Vec<u8>,
    filled: usize,
    sender: SyncSender<Vec<i16>>,
}
impl Frames {
    fn new(channels: usize, sender: SyncSender<Vec<i16>>) -> Self {
        Self {
            bytes: vec![0; FRAME * channels * 2],
            filled: 0,
            sender,
        }
    }
    fn push(&mut self, mut bytes: &[u8]) {
        let size = self.bytes.len();
        while !bytes.is_empty() {
            let count = bytes.len().min(size - self.filled);
            self.bytes[self.filled..self.filled + count].copy_from_slice(&bytes[..count]);
            self.filled += count;
            bytes = &bytes[count..];
            if self.filled == size {
                let samples: Vec<i16> = self
                    .bytes
                    .as_chunks::<2>()
                    .0
                    .iter()
                    .map(|pair| i16::from_le_bytes(*pair))
                    .collect();
                // A bounded queue absorbs native-protocol batches without blocking.
                // The reactor must never block behind the video thread.
                let _ = self.sender.try_send(samples);
                self.filled = 0;
            }
        }
    }
}

pub(super) struct PulseCapture {
    receiver: Receiver<Vec<i16>>,
    channels: usize,
    stream: RecordStream,
    _client: Client,
    description: String,
    health: Health,
}
impl PulseCapture {
    pub(super) fn new(layout: Layout) -> anyhow::Result<Self> {
        let client =
            Client::from_env(c"Latch desktop monitor").context("connect to PulseAudio/PipeWire")?;
        let server = block_on(client.server_info())?;
        let sink_name = server
            .default_sink_name
            .context("PulseAudio has no default sink")?;
        let sink = block_on(client.sink_info_by_name(sink_name))?;
        let monitor = sink
            .monitor_source_index
            .context("default sink has no monitor source")?;
        let channels = layout.pick(usize::from(sink.sample_spec.channels));
        let description = format!(
            "PulseAudio monitor ({}, {channels} channels)",
            sink.monitor_source_name
                .as_ref()
                .map_or_else(|| monitor.to_string(), |n| n.to_string_lossy().into_owned())
        );
        let params = protocol::RecordStreamParams {
            source_index: Some(monitor),
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::S16Le,
                channels: channels as u8,
                sample_rate: RATE,
            },
            channel_map: channel_map(channels),
            buffer_attr: protocol::stream::BufferAttr {
                max_length: (FRAME * channels * 2 * 8) as u32,
                fragment_size: (FRAME * channels * 2) as u32,
                ..Default::default()
            },
            flags: protocol::stream::StreamFlags {
                adjust_latency: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let (sender, receiver) = mpsc::sync_channel(64);
        let mut frames = Frames::new(channels, sender);
        let stream =
            block_on(client.create_record_stream(params, move |bytes: &[u8]| frames.push(bytes)))?;
        let spec = stream.sample_spec();
        ensure!(
            spec.format == protocol::SampleFormat::S16Le
                && spec.sample_rate == RATE
                && spec.channels == channels as u8,
            "PulseAudio changed the requested recording format"
        );
        Ok(Self {
            receiver,
            channels,
            stream,
            _client: client,
            description,
            health: Health::default(),
        })
    }
}
/// Latch's channel order (WAVE) in PulseAudio's names.
fn channel_map(channels: usize) -> protocol::ChannelMap {
    use protocol::ChannelPosition::*;
    match channels {
        6 => protocol::ChannelMap::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            Lfe,
            RearLeft,
            RearRight,
        ]),
        8 => protocol::ChannelMap::new([
            FrontLeft,
            FrontRight,
            FrontCenter,
            Lfe,
            RearLeft,
            RearRight,
            SideLeft,
            SideRight,
        ]),
        1 => protocol::ChannelMap::mono(),
        _ => protocol::ChannelMap::stereo(),
    }
}

impl AudioCapture for PulseCapture {
    fn read(
        &mut self,
        out: &mut [i16; FRAME * CHANNELS],
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        let mut frame = Vec::new();
        let got = self.read_frame(&mut frame, timeout)?;
        if got {
            anyhow::ensure!(frame.len() == out.len(), "this recording is not stereo");
            out.copy_from_slice(&frame);
        }
        Ok(got)
    }
    fn channels(&self) -> usize {
        self.channels
    }
    fn read_frame(&mut self, out: &mut Vec<i16>, timeout: Duration) -> anyhow::Result<bool> {
        match self.receiver.recv_timeout(timeout) {
            Ok(samples) => {
                *out = samples;
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => {
                let stream = &self.stream;
                self.health.check(|| {
                    let stream = stream.clone();
                    Box::pin(async move { stream.timing_info().await.map(drop) })
                })?;
                Ok(false)
            }
            Err(RecvTimeoutError::Disconnected) => {
                anyhow::bail!("PulseAudio recording disconnected")
            }
        }
    }
    fn describe(&self) -> String {
        self.description.clone()
    }
}
impl Drop for PulseCapture {
    fn drop(&mut self) {
        let _ = block_on(self.stream.clone().delete());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn assembles_partial_little_endian_frames_without_blocking() {
        let (sender, receiver) = mpsc::sync_channel(1);
        let mut frames = Frames::new(CHANNELS, sender);
        let mut bytes = Vec::new();
        for sample in 0..FRAME * CHANNELS {
            bytes.extend_from_slice(&(sample as i16 - 400).to_le_bytes());
        }
        frames.push(&bytes[..3]);
        assert!(receiver.try_recv().is_err());
        frames.push(&bytes[3..]);
        let samples = receiver.recv().unwrap();
        assert_eq!(samples[0], -400);
        assert_eq!(
            samples[FRAME * CHANNELS - 1],
            (FRAME * CHANNELS) as i16 - 401
        );
        frames.push(&bytes);
        frames.push(&bytes);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }

    #[test]
    fn a_silent_recording_the_server_killed_is_an_error() {
        let alive = || -> Probe { Box::pin(std::future::ready(Ok(()))) };
        let mut health = Health::default();
        health.check(alive).unwrap();
        health
            .check(|| panic!("asked again within PROBE_EVERY"))
            .unwrap();
        // An unanswered probe is not an error, and is not asked twice.
        let mut health = Health::default();
        health.check(|| Box::pin(std::future::pending())).unwrap();
        health.asked = Instant::now().checked_sub(PROBE_EVERY);
        health
            .check(|| panic!("asked again while unanswered"))
            .unwrap();
        // What the server says about a stream it killed.
        let mut health = Health::default();
        let error = health
            .check(|| {
                Box::pin(std::future::ready(Err(ClientError::ServerError(
                    protocol::PulseError::NoEntity,
                ))))
            })
            .unwrap_err();
        assert!(format!("{error:#}").contains("ended the recording"));
    }
}
