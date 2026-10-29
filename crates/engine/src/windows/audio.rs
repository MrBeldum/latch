use super::runtime::Runtime;
use crate::audio::{f32_to_i16, AudioCapture, Resampler, CHANNELS, FRAME};
use anyhow::{bail, Context, Result};
use std::{
    collections::VecDeque,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use windows::Win32::{
    Foundation::*,
    Media::{Audio::*, KernelStreaming::*, Multimedia::*},
    System::{Com::*, Threading::*},
};

struct EventHandle(HANDLE);
impl Drop for EventHandle {
    fn drop(&mut self) {
        // SAFETY: This wrapper uniquely owns the event handle.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}
struct Loopback {
    capture: IAudioCaptureClient,
    client: IAudioClient,
    event: Option<EventHandle>,
    rate: u32,
    channels: usize,
    bits: u16,
    valid_bits: u16,
    block: usize,
    float: bool,
    resampler: Resampler,
    pending: VecDeque<f32>,
}
impl Loopback {
    fn open(event_driven: bool) -> Result<Self> {
        // SAFETY: Called in the audio worker's MTA; COM objects stay on it.
        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .context("create audio device enumerator")?;
            let device = enumerator
                .GetDefaultAudioEndpoint(eRender, eConsole)
                .context("default render endpoint")?;
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .context("activate WASAPI client")?;
            let event = if event_driven {
                Some(EventHandle(
                    CreateEventW(None, false, false, None).context("create loopback event")?,
                ))
            } else {
                None
            };
            let format = client.GetMixFormat().context("WASAPI mix format")?;
            if format.is_null() {
                bail!("WASAPI returned a null mix format");
            }
            // Copy the packed fields before freeing the task allocation.
            let wave = format.read_unaligned();
            let (tag, rate, channels, bits, block) = (
                wave.wFormatTag,
                wave.nSamplesPerSec,
                wave.nChannels as usize,
                wave.wBitsPerSample,
                wave.nBlockAlign as usize,
            );
            let (subformat, valid_bits, mask) = if tag == WAVE_FORMAT_EXTENSIBLE as u16
                && wave.cbSize as usize
                    >= std::mem::size_of::<WAVEFORMATEXTENSIBLE>()
                        - std::mem::size_of::<WAVEFORMATEX>()
            {
                let ext = (format as *const WAVEFORMATEXTENSIBLE).read_unaligned();
                (
                    Some(ext.SubFormat),
                    ext.Samples.wValidBitsPerSample,
                    ext.dwChannelMask,
                )
            } else {
                (None, bits, 0)
            };
            let float = tag == WAVE_FORMAT_IEEE_FLOAT as u16
                || subformat == Some(KSDATAFORMAT_SUBTYPE_IEEE_FLOAT);
            let pcm = tag == WAVE_FORMAT_PCM as u16 || subformat == Some(KSDATAFORMAT_SUBTYPE_PCM);
            let valid = rate > 0
                && channels > 0
                && block >= channels * usize::from(bits / 8)
                && ((float && matches!(bits, 32 | 64))
                    || (pcm && matches!(bits, 8 | 16 | 24 | 32)));
            let flags = AUDCLNT_STREAMFLAGS_LOOPBACK
                | if event.is_some() {
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                } else {
                    0
                };
            let initialized = if valid {
                client
                    .Initialize(AUDCLNT_SHAREMODE_SHARED, flags, 0, 0, format, None)
                    .context("initialize WASAPI loopback")
            } else {
                Err(anyhow::anyhow!("unsupported mix format: tag={tag}, bits={bits}, channels={channels}, rate={rate}"))
            };
            CoTaskMemFree(Some(format.cast()));
            initialized?;
            if let Some(event) = &event {
                client
                    .SetEventHandle(event.0)
                    .context("set loopback event")?;
            }
            let capture: IAudioCaptureClient =
                client.GetService().context("get WASAPI capture service")?;
            client.Start().context("start WASAPI loopback")?;
            Ok(Self {
                capture,
                client,
                event,
                rate,
                channels,
                bits,
                valid_bits,
                block,
                float,
                resampler: Resampler::with_channel_mask(rate, channels, mask),
                pending: VecDeque::new(),
            })
        }
    }
    fn packet(&mut self) -> Result<bool> {
        // SAFETY: WASAPI buffers are read only until ReleaseBuffer. Every acquired
        // packet is released before an error can leave this method.
        unsafe {
            if self
                .capture
                .GetNextPacketSize()
                .context("WASAPI packet size")?
                == 0
            {
                return Ok(false);
            }
            let (mut data, mut frames, mut flags) = (std::ptr::null_mut(), 0, 0);
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                .context("WASAPI capture buffer")?;
            if frames == 0 {
                self.capture
                    .ReleaseBuffer(0)
                    .context("release empty WASAPI packet")?;
                return Ok(false);
            }
            let mut samples = vec![0.0; frames as usize * self.channels];
            if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 == 0 && !data.is_null() {
                let bytes = std::slice::from_raw_parts(data, frames as usize * self.block);
                let size = usize::from(self.bits / 8);
                for (i, sample) in samples.iter_mut().enumerate() {
                    let at = (i / self.channels) * self.block + (i % self.channels) * size;
                    let b = &bytes[at..at + size];
                    *sample = if self.float {
                        if self.bits == 32 {
                            f32::from_le_bytes([b[0], b[1], b[2], b[3]])
                        } else {
                            f64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
                                as f32
                        }
                    } else if self.bits == 8 {
                        (b[0] as f32 - 128.0) / 128.0
                    } else {
                        let mut value = 0i32;
                        for (k, byte) in b.iter().enumerate() {
                            value |= i32::from(*byte) << (k * 8);
                        }
                        value = (value << (32 - self.bits)) >> (32 - self.bits);
                        let valid = self.valid_bits.clamp(1, self.bits);
                        (value >> (self.bits - valid)) as f32 / (1u64 << (valid - 1)) as f32
                    };
                    if !sample.is_finite() {
                        *sample = 0.0;
                    }
                }
            }
            let released = self
                .capture
                .ReleaseBuffer(frames)
                .context("release WASAPI packet");
            let mut stereo = Vec::new();
            self.resampler.push(&samples, &mut stereo);
            self.pending.extend(stereo);
            released?;
            Ok(true)
        }
    }
    fn read(&mut self, timeout: Duration) -> Result<Option<[i16; FRAME * CHANNELS]>> {
        let deadline = Instant::now() + timeout;
        loop {
            while self.pending.len() < FRAME * CHANNELS && self.packet()? {}
            if self.pending.len() >= FRAME * CHANNELS {
                let samples: Vec<f32> = self.pending.drain(..FRAME * CHANNELS).collect();
                let mut out = [0; FRAME * CHANNELS];
                f32_to_i16(&samples, &mut out);
                return Ok(Some(out));
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Ok(None);
            }
            let wait = remaining.min(Duration::from_millis(5));
            if let Some(event) = &self.event {
                // SAFETY: The event remains owned and valid throughout this wait.
                let status =
                    unsafe { WaitForSingleObject(event.0, wait.as_millis().max(1) as u32) };
                if status == WAIT_FAILED {
                    return Err(windows::core::Error::from_win32())
                        .context("wait for loopback audio");
                }
            } else {
                thread::sleep(wait);
            }
        }
    }
}
impl Drop for Loopback {
    fn drop(&mut self) {
        // SAFETY: Stop on the same MTA that activated the client.
        unsafe {
            let _ = self.client.Stop();
        }
    }
}
struct Request {
    timeout: Duration,
    reply: mpsc::SyncSender<Result<Option<[i16; FRAME * CHANNELS]>>>,
}
struct Proxy {
    requests: mpsc::Sender<Request>,
    description: String,
}
impl AudioCapture for Proxy {
    fn read(&mut self, out: &mut [i16; FRAME * CHANNELS], timeout: Duration) -> Result<bool> {
        let (reply, response) = mpsc::sync_channel(1);
        self.requests
            .send(Request { timeout, reply })
            .context("audio worker stopped")?;
        match response.recv().context("audio worker stopped")?? {
            Some(frame) => {
                *out = frame;
                Ok(true)
            }
            None => Ok(false),
        }
    }
    fn describe(&self) -> String {
        self.description.clone()
    }
}
fn open_loopback() -> Result<Loopback> {
    Loopback::open(true).or_else(|e| {
        tracing::debug!("WASAPI events unavailable: {e:#}; polling");
        Loopback::open(false)
    })
}
pub(super) fn open() -> Result<Box<dyn AudioCapture>> {
    let (requests, receive) = mpsc::channel::<Request>();
    let (ready, startup) = mpsc::sync_channel(1);
    thread::Builder::new()
        .name("latch-wasapi".into())
        .spawn(move || {
            let initialized = (|| {
                let runtime = Runtime::new(false)?;
                let capture = open_loopback()?;
                Ok::<_, anyhow::Error>((runtime, capture))
            })();
            let (_runtime, mut capture) = match initialized {
                Ok(v) => v,
                Err(e) => {
                    let _ = ready.send(Err(e));
                    return;
                }
            };
            let description = format!(
                "WASAPI default render loopback ({} Hz, {} channels)",
                capture.rate, capture.channels
            );
            if ready.send(Ok(description)).is_err() {
                return;
            }
            for request in receive {
                let result = capture.read(request.timeout);
                let invalidated = result
                    .as_ref()
                    .err()
                    .and_then(|e| e.downcast_ref::<windows::core::Error>())
                    .is_some_and(|e| {
                        e.code() == AUDCLNT_E_DEVICE_INVALIDATED
                            || e.code() == AUDCLNT_E_RESOURCES_INVALIDATED
                    });
                let result = if invalidated {
                    match open_loopback() {
                        Ok(new) => {
                            capture = new;
                            Ok(None)
                        }
                        Err(e) => Err(e.context("reopen invalidated audio device")),
                    }
                } else {
                    result
                };
                let _ = request.reply.send(result);
            }
        })
        .context("spawn audio worker")?;
    let description = startup
        .recv()
        .context("audio worker stopped during startup")??;
    Ok(Box::new(Proxy {
        requests,
        description,
    }))
}
#[cfg(test)]
mod tests {
    #[test]
    fn loopback_opens_when_audio_endpoint_exists() -> anyhow::Result<()> {
        match super::open() {
            Ok(capture) => eprintln!("{}", capture.describe()),
            Err(e) => {
                if !super::super::unavailable(&e, "WASAPI loopback") {
                    return Err(e);
                }
            }
        }
        Ok(())
    }
}
