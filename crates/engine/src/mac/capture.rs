//! Independent ScreenCaptureKit streams keep desktop sound alive across video restarts.

use super::ffi::*;
use crate::{
    audio::{self, Resampler},
    input::DisplayRect,
};
use anyhow::{anyhow, bail, ensure, Result};
use block2::RcBlock;
use dispatch2::{DispatchQueue, DispatchRetained};
use objc2::{
    define_class, msg_send, rc::Retained, runtime::ProtocolObject, AnyThread, DefinedClass,
};
use objc2_core_media::CMSampleBuffer;
use objc2_foundation::{NSArray, NSError, NSObject, NSObjectProtocol};
use objc2_screen_capture_kit::*;
use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    ptr,
    sync::{mpsc, Arc},
    time::{Duration, Instant},
};

pub const PERMISSION: &str = "Latch needs Screen Recording permission to share this Mac: System Settings → Privacy & Security → Screen Recording";
pub fn permission() -> Result<()> {
    // SAFETY: preflight only reads authorization; it neither requests permission nor starts capture.
    ensure!(unsafe { CGPreflightScreenCaptureAccess() }, PERMISSION);
    Ok(())
}
pub struct Picture {
    pub image: Owned,
    pub captured: Instant,
}
#[derive(Default)]
pub struct State {
    pub picture: Option<Picture>,
    pub generation: u64,
    pub stopped: Option<String>,
    pub frames: VecDeque<[i16; audio::FRAME * audio::CHANNELS]>,
    pending: VecDeque<i16>,
    resampler: Option<(u32, usize, Resampler)>,
}
#[derive(Default)]
pub struct Shared {
    pub state: Mutex<State>,
    pub ready: Condvar,
}
struct Ivars {
    shared: Arc<Shared>,
    audio: bool,
}

define_class!(
    // SAFETY: NSObject imposes no subclass requirements. All mutable callback state is synchronized.
    #[unsafe(super = NSObject)]
    #[ivars = Ivars]
    struct Output;
    // SAFETY: NSObjectProtocol has no additional requirements.
    unsafe impl NSObjectProtocol for Output {}
    // SAFETY: the method uses the exact SCStreamOutput ABI and borrows each sample only during it.
    unsafe impl SCStreamOutput for Output {
        #[unsafe(method(stream:didOutputSampleBuffer:ofType:))]
        fn sample(&self, _: &SCStream, sample: &CMSampleBuffer, kind: SCStreamOutputType) {
            let ivars = self.ivars();
            let raw = (sample as *const CMSampleBuffer).cast();
            let mut state = ivars.shared.state.lock();
            // SAFETY: ScreenCaptureKit guarantees a live CMSampleBuffer for the duration of this callback.
            unsafe {
                if ivars.audio && kind == SCStreamOutputType::Audio {
                    if let Err(error) = audio_sample(raw, &mut state) {
                        state.stopped = Some(error.to_string());
                    }
                } else if !ivars.audio && kind == SCStreamOutputType::Screen && complete(raw) {
                    let image = CMSampleBufferGetImageBuffer(raw);
                    if !image.is_null() {
                        state.picture = Some(Picture {
                            image: Owned::retain(image),
                            captured: capture_time(raw),
                        });
                        state.generation = state.generation.wrapping_add(1);
                    }
                }
            }
            ivars.shared.ready.notify_all();
        }
    }
    // SAFETY: delegate method ABI matches ScreenCaptureKit; NSError is copied into a Rust string.
    unsafe impl SCStreamDelegate for Output {
        #[unsafe(method(stream:didStopWithError:))]
        fn stopped(&self, _: &SCStream, error: &NSError) {
            self.ivars().shared.state.lock().stopped = Some(error.to_string());
            self.ivars().shared.ready.notify_all();
        }
    }
);

pub struct Capture {
    stream: Retained<SCStream>,
    output: Retained<Output>,
    _queue: DispatchRetained<DispatchQueue>,
    pub shared: Arc<Shared>,
    pub display: DisplayRect,
    audio: bool,
}
// SAFETY: SCStream supports background queues. This owner is exclusive, callback state is locked,
// and the output object is retained until outputs have been detached during Drop.
unsafe impl Send for Capture {}
struct Selection(Retained<SCDisplay>);
// SAFETY: SCDisplay is immutable shareable-content metadata; retaining it across callback queues is valid.
unsafe impl Send for Selection {}
impl Capture {
    pub fn new(width: u32, height: u32, fps: u32, audio: bool) -> Result<Self> {
        permission()?;
        let (tx, rx) = mpsc::sync_channel(1);
        let completion = RcBlock::new(
            move |content: *mut SCShareableContent, error: *mut NSError| {
                // SAFETY: SCK provides borrowed content/error objects until the completion returns.
                let result = unsafe {
                    if let Some(error) = error.as_ref() {
                        let code = error.code();
                        if code == -3801 || code == -3803 || code == -3814 {
                            Err(PERMISSION.to_string())
                        } else {
                            Err(error.to_string())
                        }
                    } else if let Some(content) = content.as_ref() {
                        let displays = content.displays();
                        if displays.count() == 0 {
                            Err(PERMISSION.to_string())
                        } else {
                            (0..displays.count())
                                .map(|i| displays.objectAtIndex(i))
                                .find(|d| d.displayID() == CGMainDisplayID())
                                .or_else(|| {
                                    (displays.count() > 0).then(|| displays.objectAtIndex(0))
                                })
                                .map(Selection)
                                .ok_or_else(|| PERMISSION.to_string())
                        }
                    } else {
                        Err(PERMISSION.to_string())
                    }
                };
                let _ = tx.send(result);
            },
        );
        // SAFETY: RcBlock is copied by the asynchronous API and owns its channel sender.
        unsafe { SCShareableContent::getShareableContentWithCompletionHandler(&completion) };
        let Selection(display) = rx
            .recv_timeout(Duration::from_secs(10))?
            .map_err(|e| anyhow!(e))?;
        let shared = Arc::new(Shared::default());
        let output = Output::alloc().set_ivars(Ivars {
            shared: shared.clone(),
            audio,
        });
        // SAFETY: NSObject initialization and the typed SCK setters use the documented value types.
        unsafe {
            let output: Retained<Output> = msg_send![super(output), init];
            let rect = display.frame();
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            config.setMinimumFrameInterval(time(1, fps));
            config.setQueueDepth(3);
            config.setPixelFormat(NV12);
            config.setShowsCursor(true);
            config.setScalesToFit(true);
            config.setPreservesAspectRatio(true);
            config.setColorMatrix(
                &*(kCVImageBufferYCbCrMatrix_ITU_R_709_2 as *const objc2_core_foundation::CFString),
            );
            config.setCapturesAudio(audio);
            config.setExcludesCurrentProcessAudio(true);
            config.setSampleRate(48_000);
            config.setChannelCount(2);
            let stream = SCStream::initWithFilter_configuration_delegate(
                SCStream::alloc(),
                &filter,
                &config,
                Some(ProtocolObject::from_ref(&*output)),
            );
            let queue = DispatchQueue::new("app.latch.capture", None);
            let capture = Self {
                stream,
                output,
                _queue: queue,
                shared,
                display: DisplayRect {
                    x: rect.origin.x as i32,
                    y: rect.origin.y as i32,
                    width: rect.size.width as u32,
                    height: rect.size.height as u32,
                },
                audio,
            };
            capture
                .stream
                .addStreamOutput_type_sampleHandlerQueue_error(
                    ProtocolObject::from_ref(&*capture.output),
                    if audio {
                        SCStreamOutputType::Audio
                    } else {
                        SCStreamOutputType::Screen
                    },
                    Some(&capture._queue),
                )
                .map_err(|e| anyhow!(e.to_string()))?;
            let (tx, rx) = mpsc::sync_channel(1);
            let started = RcBlock::new(move |error: *mut NSError| {
                let _ = tx.send(error.as_ref().map(|e| e.to_string()));
            });
            capture
                .stream
                .startCaptureWithCompletionHandler(Some(&started));
            if let Some(error) = rx.recv_timeout(Duration::from_secs(10))? {
                bail!("start ScreenCaptureKit: {error}");
            }
            Ok(capture)
        }
    }
}
impl Drop for Capture {
    fn drop(&mut self) {
        // SAFETY: output and queue remain retained during detachment. In-flight callbacks own
        // their receiver, whose Arc keeps state alive even after this owner is dropped.
        unsafe {
            let _ = self.stream.removeStreamOutput_type_error(
                ProtocolObject::from_ref(&*self.output),
                if self.audio {
                    SCStreamOutputType::Audio
                } else {
                    SCStreamOutputType::Screen
                },
            );
            self.stream.stopCaptureWithCompletionHandler(None);
        }
    }
}
unsafe fn complete(sample: Ref) -> bool {
    // SAFETY: sample is live; SCStreamFrameInfoStatus is an NSString dictionary key (toll-free CFString).
    unsafe {
        let list = CMSampleBufferGetSampleAttachmentsArray(sample, false);
        if list.is_null() || CFArrayGetCount(list) == 0 {
            return false;
        }
        let key = (SCStreamFrameInfoStatus as *const objc2_foundation::NSString).cast();
        let value = CFDictionaryGetValue(CFArrayGetValueAtIndex(list, 0), key);
        let mut status = -1i64;
        !value.is_null()
            && CFNumberGetValue(value, 4, (&mut status as *mut i64).cast())
            && status == 0
    }
}
unsafe fn capture_time(sample: Ref) -> Instant {
    // SAFETY: sample and the host clock are live; SCK presentation timestamps use the host clock.
    unsafe {
        let pts = CMSampleBufferGetPresentationTimeStamp(sample);
        let now = CMClockGetTime(CMClockGetHostTimeClock());
        let age = now.value as f64 / now.timescale as f64 - pts.value as f64 / pts.timescale as f64;
        let instant = Instant::now();
        if age.is_finite() && (0.0..60.0).contains(&age) {
            instant
                .checked_sub(Duration::from_secs_f64(age))
                .unwrap_or(instant)
        } else {
            instant
        }
    }
}
unsafe fn audio_sample(sample: Ref, state: &mut State) -> Result<()> {
    // SAFETY: borrowed format and audio data are used only while the sample/block is retained.
    unsafe {
        let format = CMSampleBufferGetFormatDescription(sample);
        ensure!(!format.is_null(), "audio sample has no format");
        let asbd = CMAudioFormatDescriptionGetStreamBasicDescription(format)
            .as_ref()
            .ok_or_else(|| anyhow!("audio format is unavailable"))?;
        ensure!(
            asbd.format == u32::from_be_bytes(*b"lpcm")
                && asbd.flags & 1 != 0
                && asbd.flags & 2 == 0
                && asbd.bits == 32
                && asbd.channels > 0
                && asbd.channels <= 32
                && asbd.rate.is_finite()
                && asbd.rate > 0.0,
            "ScreenCaptureKit audio is not native Float32 PCM"
        );
        // 8-byte aligned storage covers the AudioBufferList header plus its variable buffer array.
        let channels = asbd.channels as usize;
        let mut list = vec![0u64; 1 + channels * 2];
        let mut block = ptr::null();
        let status = CMSampleBufferGetAudioBufferListWithRetainedBlockBuffer(
            sample,
            ptr::null_mut(),
            list.as_mut_ptr().cast(),
            list.len() * 8,
            ptr::null(),
            ptr::null(),
            0,
            &mut block,
        );
        let _block = if block.is_null() {
            None
        } else {
            Some(Owned(block))
        };
        check(status, "read ScreenCaptureKit audio")?;
        let count = *(list.as_ptr().cast::<u32>()) as usize;
        ensure!(
            count == 1 || count == channels,
            "unexpected audio buffer layout"
        );
        let buffers = std::slice::from_raw_parts(list.as_ptr().add(1).cast::<AudioBuffer>(), count);
        let planar = asbd.flags & (1 << 5) != 0;
        ensure!(
            buffers.iter().all(|b| !b.data.is_null() && b.size % 4 == 0),
            "invalid audio buffer"
        );
        ensure!(
            (planar && count == channels && buffers.iter().all(|b| b.channels == 1))
                || (!planar && count == 1 && buffers[0].channels as usize == channels),
            "inconsistent audio channel layout"
        );
        let mut interleaved = Vec::new();
        if planar {
            let frames = buffers
                .iter()
                .map(|b| b.size as usize / 4)
                .min()
                .unwrap_or(0);
            for i in 0..frames {
                for b in buffers {
                    interleaved.push(*b.data.add(i));
                }
            }
        } else {
            interleaved.extend_from_slice(std::slice::from_raw_parts(
                buffers[0].data,
                buffers[0].size as usize / 4,
            ));
        }
        let rate = asbd.rate.round() as u32;
        if state
            .resampler
            .as_ref()
            .is_none_or(|(r, c, _)| (*r, *c) != (rate, channels))
        {
            state.pending.clear();
            state.resampler = Some((rate, channels, Resampler::new(rate, channels)));
        }
        let mut stereo = Vec::new();
        state
            .resampler
            .as_mut()
            .unwrap()
            .2
            .push(&interleaved, &mut stereo);
        let mut pcm = vec![0i16; stereo.len()];
        audio::f32_to_i16(&stereo, &mut pcm);
        state.pending.extend(pcm);
        while state.pending.len() >= audio::FRAME * audio::CHANNELS {
            let mut frame = [0; audio::FRAME * audio::CHANNELS];
            for sample in &mut frame {
                *sample = state.pending.pop_front().unwrap();
            }
            if state.frames.len() == 20 {
                state.frames.pop_front();
            }
            state.frames.push_back(frame);
        }
        Ok(())
    }
}
