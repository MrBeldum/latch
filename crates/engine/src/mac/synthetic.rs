//! Permission-free VideoToolbox host for the existing synthetic test source.
use super::{
    encoder::{Depth, Encoder},
    ffi::*,
};
use crate::{
    software::PictureEncoder,
    video::{Codec, VideoConfig},
};
use anyhow::{Context, Result};
use latch_codec::Yuv420;
use std::{ptr, time::Instant};

pub(crate) struct Synthetic {
    encoder: Encoder,
    image: Owned,
    codec: Codec,
    last: (u64, Option<u64>),
}
impl Synthetic {
    pub fn open(width: u32, height: u32, config: &VideoConfig) -> Result<Self> {
        let config = VideoConfig {
            width,
            height,
            ..config.clone()
        };
        anyhow::ensure!(
            config.color.bit_depth == 8,
            "synthetic VideoToolbox input is SDR8"
        );
        let encoder = Encoder::new(&config, Depth::Eight)?;
        // SAFETY: owned NV12 IOSurface, sized for every supplied YUV picture.
        let image = unsafe {
            let empty = dictionary(&[]);
            let attrs = dictionary(&[(kCVPixelBufferIOSurfacePropertiesKey, empty.0)]);
            let mut image = ptr::null();
            check(
                CVPixelBufferCreate(
                    ptr::null(),
                    width as usize,
                    height as usize,
                    NV12,
                    attrs.0,
                    &mut image,
                ),
                "create test IOSurface",
            )?;
            Owned::new(image)?
        };
        Ok(Self {
            encoder,
            image,
            codec: config.codec,
            last: (0, None),
        })
    }
}
impl PictureEncoder for Synthetic {
    fn name(&self) -> String {
        format!("VideoToolbox {} (synthetic IOSurface)", self.codec.name())
    }
    fn hardware(&self) -> bool {
        self.encoder.hardware
    }
    fn codec(&self) -> Codec {
        self.codec
    }
    fn encode(&mut self, picture: &Yuv420, keyframe: bool, out: &mut Vec<u8>) -> Result<bool> {
        // SAFETY: exclusively owned buffer, unlocked before VT retains it; encode waits
        // for its callback before the next write. Both planes use their actual strides.
        unsafe {
            check(
                CVPixelBufferLockBaseAddress(self.image.0, 0),
                "lock test IOSurface",
            )?;
            let y = CVPixelBufferGetBaseAddressOfPlane(self.image.0, 0);
            let uv = CVPixelBufferGetBaseAddressOfPlane(self.image.0, 1);
            let ys = CVPixelBufferGetBytesPerRowOfPlane(self.image.0, 0);
            let cs = CVPixelBufferGetBytesPerRowOfPlane(self.image.0, 1);
            for row in 0..picture.height as usize {
                ptr::copy_nonoverlapping(
                    picture.y.as_ptr().add(row * picture.y_stride),
                    y.add(row * ys),
                    picture.width as usize,
                );
            }
            for row in 0..picture.height as usize / 2 {
                for col in 0..picture.width as usize / 2 {
                    *uv.add(row * cs + col * 2) = picture.u[row * picture.c_stride + col];
                    *uv.add(row * cs + col * 2 + 1) = picture.v[row * picture.c_stride + col];
                }
            }
            check(
                CVPixelBufferUnlockBaseAddress(self.image.0, 0),
                "unlock test IOSurface",
            )?;
        }
        let frame = self
            .encoder
            .encode(self.image.0, Instant::now(), keyframe)?
            .context("VideoToolbox dropped synthetic frame")?;
        self.last = (frame.picture, frame.recovery);
        out.extend_from_slice(&frame.data);
        Ok(frame.keyframe)
    }
    fn set_bitrate(&mut self, kbps: u32) {
        if let Err(error) = self.encoder.bitrate(kbps) {
            tracing::warn!(%error, "synthetic VT bitrate");
        }
    }
    fn recovers(&self) -> bool {
        self.encoder.recovers()
    }
    fn recover_from(&mut self, picture: u64) -> bool {
        self.encoder.recover_from(picture)
    }
    fn last_picture(&self) -> (u64, Option<u64>) {
        self.last
    }
}
