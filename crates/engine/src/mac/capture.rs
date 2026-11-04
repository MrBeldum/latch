//! Independent ScreenCaptureKit streams keep desktop sound alive across video restarts.

use super::ffi::*;
use crate::{
    audio::{self, Resampler},
    input::DisplayRect,
};
use anyhow::{anyhow, bail, ensure, Result};
use block2::RcBlock;
use dispatch2::{DispatchQoS, DispatchQueue, DispatchQueueAttr, DispatchRetained};
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
    /// Where the captured display was, in global points, at the start.
    pub display: DisplayRect,
    id: u32,
    audio: bool,
}
// SAFETY: SCStream supports background queues. This owner is exclusive, callback state is locked,
// and the output object is retained until outputs have been detached during Drop.
unsafe impl Send for Capture {}
struct Selection(Retained<SCDisplay>);
// SAFETY: SCDisplay is immutable shareable-content metadata; retaining it across callback queues is valid.
unsafe impl Send for Selection {}
/// What a video capture asks ScreenCaptureKit for.
#[derive(Debug, Clone, Copy)]
pub struct Request {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// A CGDirectDisplayID; the main display when `None` or gone.
    pub display: Option<u32>,
    /// NV12 or P010 (see `encoder::Depth`).
    pub pixel_format: u32,
    pub hdr: bool,
}
impl Capture {
    pub fn video(request: Request) -> Result<Self> {
        Self::new(request, false)
    }
    pub fn audio() -> Result<Self> {
        Self::new(
            Request {
                width: 2,
                height: 2,
                fps: 1,
                display: None,
                pixel_format: NV12,
                hdr: false,
            },
            true,
        )
    }
    fn new(request: Request, audio: bool) -> Result<Self> {
        let Request {
            width,
            height,
            fps,
            display: wanted,
            pixel_format,
            hdr,
        } = request;
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
                            let find = |id: u32| {
                                (0..displays.count())
                                    .map(|i| displays.objectAtIndex(i))
                                    .find(|d| d.displayID() == id)
                            };
                            wanted
                                .and_then(find)
                                .or_else(|| find(CGMainDisplayID()))
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
            let id = display.displayID();
            let rect = bounds(id);
            let filter = SCContentFilter::initWithDisplay_excludingWindows(
                SCContentFilter::alloc(),
                &display,
                &NSArray::new(),
            );
            let config = SCStreamConfiguration::new();
            config.setWidth(width as usize);
            config.setHeight(height as usize);
            let interval = frame_interval(fps, refresh(id));
            config.setMinimumFrameInterval(CMTime {
                value: (interval * 1e6) as i64,
                timescale: 1_000_000,
                flags: objc2_core_media::CMTimeFlags(1),
                epoch: 0,
            });
            // The surfaces SCK renders into. VideoToolbox holds up to two
            // while they encode (see encoder.rs), the pipeline the newest,
            // and the window server needs two more to render the next
            // refresh into without waiting. More surfaces add memory, not
            // delay: each picture is delivered as soon as it is drawn.
            config.setQueueDepth(5);
            configure_color(&config, pixel_format, hdr)?;
            config.setShowsCursor(true);
            config.setScalesToFit(true);
            config.setPreservesAspectRatio(true);
            // Constant CGColors live for the process; SCK borrows this colour.
            // The untyped send avoids adding a CoreGraphics binding dependency.
            let black = CGColorGetConstantColor(kCGColorBlack);
            let _: () = msg_send![&config, setBackgroundColor: black];
            let fitted = DisplayRect {
                width: rect.width,
                height: rect.height,
                ..Default::default()
            }
            .fitted(width, height);
            config.setDestinationRect(objc2_core_foundation::CGRect::new(
                objc2_core_foundation::CGPoint::new(f64::from(fitted.x), f64::from(fitted.y)),
                objc2_core_foundation::CGSize::new(
                    f64::from(fitted.width),
                    f64::from(fitted.height),
                ),
            ));
            config.setColorMatrix(
                &*((if hdr {
                    kCVImageBufferYCbCrMatrix_ITU_R_2020
                } else {
                    kCVImageBufferYCbCrMatrix_ITU_R_709_2
                }) as *const objc2_core_foundation::CFString),
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
            let attr = DispatchQueueAttr::with_qos_class(None, DispatchQoS::UserInteractive, 0);
            let queue = DispatchQueue::new("app.latch.capture", Some(&attr));
            let capture = Self {
                stream,
                output,
                _queue: queue,
                shared,
                display: rect,
                id,
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
impl Capture {
    /// Whether the display changed size or place since capture started.
    /// SCK can keep running across a mode or arrangement change, scaled to
    /// the old fit, while input would still map to the old geometry.
    pub fn moved(&self) -> bool {
        moved(self.id, self.display)
    }
}
/// The shortest gap SCK may leave between two pictures, in seconds. A
/// stream as fast as the display, or one whose refresh is not known
/// (ProMotion reports none), takes every refresh. A slower one takes every
/// k-th, with half a refresh of slack so timing jitter never skips one,
/// which a gap of exactly 1/fps does.
fn frame_interval(fps: u32, refresh: f64) -> f64 {
    let fps = f64::from(fps.max(1));
    if refresh.is_nan() || refresh <= 0.0 || fps * 1.05 >= refresh {
        return 0.0;
    }
    let every = (refresh / fps).floor().max(1.0);
    (every - 0.5) / refresh
}
/// The display's refresh rate in Hz; zero when it has no fixed one.
fn refresh(id: u32) -> f64 {
    // SAFETY: the copied mode is released after its rate is read.
    unsafe {
        let mode = CGDisplayCopyDisplayMode(id);
        if mode.is_null() {
            return 0.0;
        }
        let rate = CGDisplayModeGetRefreshRate(mode);
        CGDisplayModeRelease(mode);
        rate
    }
}
fn moved(id: u32, display: DisplayRect) -> bool {
    bounds(id) != display
}
/// A display's frame in global points, as CGEvent positions use it; zero
/// when the display is gone.
fn bounds(id: u32) -> DisplayRect {
    // SAFETY: CGDisplayBounds only reads the window server's display configuration.
    let rect = unsafe { CGDisplayBounds(id) };
    DisplayRect {
        x: rect.origin.x as i32,
        y: rect.origin.y as i32,
        width: rect.size.width as u32,
        height: rect.size.height as u32,
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn frame_interval_never_throttles_below_the_display_or_the_stream() {
        // As fast as the display, or a display of unknown rate: every refresh.
        assert_eq!(frame_interval(120, 120.0), 0.0);
        assert_eq!(frame_interval(240, 120.0), 0.0);
        assert_eq!(frame_interval(60, 0.0), 0.0);
        assert_eq!(frame_interval(118, 120.0), 0.0);
        // Every second refresh of a 120 Hz display, with half a refresh to spare.
        let gap = frame_interval(60, 120.0);
        assert!(gap > 1.0 / 120.0 && gap < 2.0 / 120.0, "{gap}");
        // Not a divisor: every refresh, and the pacer takes 100 of them.
        assert!(frame_interval(100, 120.0) < 1.0 / 120.0);
        assert!(frame_interval(30, 144.0) < 4.0 / 144.0);
        assert!(frame_interval(30, 144.0) > 3.0 / 144.0);
        // SAFETY: reads the main display's ID and mode only.
        let main = refresh(unsafe { CGMainDisplayID() });
        assert!(main >= 0.0);
    }
    #[test]
    fn display_bounds_are_stable_until_the_display_changes() {
        // SAFETY: reads the main display's ID only.
        let id = unsafe { CGMainDisplayID() };
        let now = bounds(id);
        assert!(!moved(id, now));
        // What a capture started before a mode or arrangement change saw.
        assert!(moved(
            id,
            DisplayRect {
                width: now.width + 640,
                ..now
            }
        ));
        assert!(moved(
            id,
            DisplayRect {
                x: now.x - 1,
                ..now
            }
        ));
    }
}

fn configure_color(config: &SCStreamConfiguration, pixel_format: u32, hdr: bool) -> Result<()> {
    // SAFETY: availability is checked before the macOS 15 selector; SCK takes
    // the colour-space name, not a CGColorSpace object. Set format last because
    // captureDynamicRange may reset both format and colour space.
    unsafe {
        if hdr {
            anyhow::ensure!(
                config.respondsToSelector(objc2::sel!(setCaptureDynamicRange:)),
                "HDR capture requires macOS 15"
            );
            let _: () = msg_send![config,setCaptureDynamicRange: 2isize];
            config.setColorSpaceName(
                &*(kCGColorSpaceITUR_2100_PQ as *const objc2_core_foundation::CFString),
            );
        }
        config.setPixelFormat(pixel_format);
    }
    Ok(())
}
#[cfg(test)]
mod hdr_tests {
    use super::*;
    #[test]
    fn hdr_capture_configuration_uses_canonical_pq_and_p010() {
        let config = unsafe { SCStreamConfiguration::new() };
        configure_color(&config, P010, true).unwrap();
        unsafe {
            let range: isize = msg_send![&config, captureDynamicRange];
            assert_eq!(range, 2);
            assert_eq!(config.pixelFormat(), P010);
            assert_eq!(
                &*config.colorSpaceName(),
                &*(kCGColorSpaceITUR_2100_PQ as *const objc2_core_foundation::CFString)
            );
        }
    }
}
