//! `pulseaudio` 0.3.2 speaks the native protocol entirely in Rust, including
//! authentication and recording. The server converts its monitor to our
//! exact format, so this also works with PipeWire's PulseAudio server and
//! needs neither libpulse nor a `parec` subprocess.

use crate::audio::{AudioCapture, CHANNELS, FRAME, RATE};
use anyhow::{ensure, Context};
use futures::executor::block_on;
use pulseaudio::{protocol, Client, RecordStream};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::time::Duration;

const BYTES: usize = FRAME * CHANNELS * 2;

struct Frames {
    bytes: [u8; BYTES],
    filled: usize,
    sender: SyncSender<[i16; FRAME * CHANNELS]>,
}
impl Frames {
    fn push(&mut self, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let count = bytes.len().min(BYTES - self.filled);
            self.bytes[self.filled..self.filled + count].copy_from_slice(&bytes[..count]);
            self.filled += count;
            bytes = &bytes[count..];
            if self.filled == BYTES {
                let mut samples = [0; FRAME * CHANNELS];
                for (sample, pair) in samples.iter_mut().zip(self.bytes.as_chunks::<2>().0) {
                    *sample = i16::from_le_bytes([pair[0], pair[1]]);
                }
                // A bounded queue absorbs native-protocol batches without blocking.
                // The reactor must never block behind the video thread.
                let _ = self.sender.try_send(samples);
                self.filled = 0;
            }
        }
    }
}

pub(super) struct PulseCapture {
    receiver: Receiver<[i16; FRAME * CHANNELS]>,
    stream: RecordStream,
    _client: Client,
    description: String,
}
impl PulseCapture {
    pub(super) fn new() -> anyhow::Result<Self> {
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
        let description = format!(
            "PulseAudio monitor ({})",
            sink.monitor_source_name
                .as_ref()
                .map_or_else(|| monitor.to_string(), |n| n.to_string_lossy().into_owned())
        );
        let params = protocol::RecordStreamParams {
            source_index: Some(monitor),
            sample_spec: protocol::SampleSpec {
                format: protocol::SampleFormat::S16Le,
                channels: CHANNELS as u8,
                sample_rate: RATE,
            },
            channel_map: protocol::ChannelMap::stereo(),
            buffer_attr: protocol::stream::BufferAttr {
                max_length: (BYTES * 8) as u32,
                fragment_size: BYTES as u32,
                ..Default::default()
            },
            flags: protocol::stream::StreamFlags {
                adjust_latency: true,
                ..Default::default()
            },
            ..Default::default()
        };
        let (sender, receiver) = mpsc::sync_channel(64);
        let mut frames = Frames {
            bytes: [0; BYTES],
            filled: 0,
            sender,
        };
        let stream =
            block_on(client.create_record_stream(params, move |bytes: &[u8]| frames.push(bytes)))?;
        let spec = stream.sample_spec();
        ensure!(
            spec.format == protocol::SampleFormat::S16Le
                && spec.sample_rate == RATE
                && spec.channels == CHANNELS as u8,
            "PulseAudio changed the requested recording format"
        );
        Ok(Self {
            receiver,
            stream,
            _client: client,
            description,
        })
    }
}
impl AudioCapture for PulseCapture {
    fn read(
        &mut self,
        out: &mut [i16; FRAME * CHANNELS],
        timeout: Duration,
    ) -> anyhow::Result<bool> {
        match self.receiver.recv_timeout(timeout) {
            Ok(samples) => {
                *out = samples;
                Ok(true)
            }
            Err(RecvTimeoutError::Timeout) => Ok(false),
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
        let mut frames = Frames {
            bytes: [0; BYTES],
            filled: 0,
            sender,
        };
        let mut bytes = Vec::new();
        for sample in 0..FRAME * CHANNELS {
            bytes.extend_from_slice(&(sample as i16 - 400).to_le_bytes());
        }
        frames.push(&bytes[..3]);
        assert!(receiver.try_recv().is_err());
        frames.push(&bytes[3..]);
        let samples = receiver.recv().unwrap();
        assert_eq!(samples[0], -400);
        assert_eq!(samples[959], 559);
        frames.push(&bytes);
        frames.push(&bytes);
        assert!(receiver.try_recv().is_ok());
        assert!(receiver.try_recv().is_err());
    }
}
