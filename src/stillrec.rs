//! Still-image recorder: continuous capture mode.
//!
//! Consumes native-resolution I420 frames from `rpicam-vid` and writes every
//! one of them as a JPEG, producing continuous "video as individual images".
//!
//! The requested output size (e.g. 1440x1080) is produced by **center-cropping
//! the native frame** (1456x1088): the crop is zero-copy, because TurboJPEG
//! accepts YUV planes with arbitrary strides and plane pointers — we simply
//! point into the source buffer. No resampling ever happens: every output
//! pixel is a real sensor pixel.

use std::path::PathBuf;

use tokio::sync::mpsc;

use crate::capture::Frame;

/// Crop description: source frame `src_w x src_h` with row stride `src_stride`
/// (the libcamera buffer stride, which pads rows), centered window
/// `out_w x out_h` at offset (`x0`, `y0`). Offsets are even so the 4:2:0
/// chroma planes stay aligned.
#[derive(Clone, Copy, Debug)]
pub struct Crop {
    pub src_h: u32,
    pub src_stride: u32,
    pub out_w: u32,
    pub out_h: u32,
    pub x0: u32,
    pub y0: u32,
}

impl Crop {
    /// Centered even-aligned crop of `out_w x out_h` from a strided
    /// `src_w x src_h` frame. Returns `None` if the output does not fit
    /// inside the source.
    pub fn centered(src_w: u32, src_h: u32, src_stride: u32, out_w: u32, out_h: u32) -> Option<Crop> {
        if out_w == 0 || out_h == 0 || out_w > src_w || out_h > src_h {
            return None;
        }
        Some(Crop {
            src_h,
            src_stride,
            out_w,
            out_h,
            x0: ((src_w - out_w) / 2) & !1,
            y0: ((src_h - out_h) / 2) & !1,
        })
    }

    /// A "crop" that covers the whole frame (no cropping at all).
    pub fn identity(src_w: u32, src_h: u32, src_stride: u32) -> Crop {
        Crop { src_h, src_stride, out_w: src_w, out_h: src_h, x0: 0, y0: 0 }
    }
}

/// Spawn the encoder/writer thread. It owns the frame channel; when the
/// capture side stops, the channel closes and the thread exits after logging
/// how many images it wrote.
pub fn spawn(
    crop: Crop,
    dir: PathBuf,
    quality: i32,
    mut rx: mpsc::Receiver<Frame>,
) -> std::io::Result<()> {
    std::thread::Builder::new()
        .name("jpeg-write".into())
        .spawn(move || run(crop, dir, quality, &mut rx))?;
    Ok(())
}

fn run(crop: Crop, dir: PathBuf, quality: i32, rx: &mut mpsc::Receiver<Frame>) {
    let mut compressor = match turbojpeg::Compressor::new() {
        Ok(c) => c,
        Err(e) => {
            tracing::error!("failed to create JPEG compressor: {e}");
            return;
        }
    };
    if let Err(e) = compressor.set_quality(quality) {
        tracing::error!("invalid JPEG quality {quality}: {e}");
        return;
    }

    let (stride, height) = (crop.src_stride as usize, crop.src_h as usize);
    let c_stride = (stride / 2) as usize;
    let (x0, y0) = (crop.x0 as usize, crop.y0 as usize);
    let (ow, oh) = (crop.out_w as usize, crop.out_h as usize);
    let chroma_offset = stride * height;
    let chroma_size = c_stride * (height / 2);

    let mut index: u64 = 0;
    let mut write_failures = 0u32;

    while let Some(frame) = rx.blocking_recv() {
        if frame.data.len() < chroma_offset + 2 * chroma_size {
            tracing::warn!("dropping malformed frame ({} bytes)", frame.data.len());
            continue;
        }

        // Zero-copy crop: point each plane at its first cropped pixel and
        // give TurboJPEG the source stride. It reads `out` rows of `out`
        // pixels at that stride, skipping the cropped-away edges and the
        // row padding.
        let image = turbojpeg::YuvPlanesImage {
            y_plane: &frame.data[y0 * stride + x0..],
            u_plane: &frame.data[chroma_offset + (y0 / 2) * c_stride + x0 / 2..],
            v_plane: &frame.data[chroma_offset + chroma_size + (y0 / 2) * c_stride + x0 / 2..],
            width: ow,
            height: oh,
            y_stride: stride,
            u_stride: c_stride,
            v_stride: c_stride,
            subsamp: turbojpeg::Subsamp::Sub2x2,
        };
        let mut out = turbojpeg::OutputBuf::new_owned();
        if let Err(e) = compressor.compress_yuv_planes(&image, &mut out) {
            tracing::error!("JPEG encode failed: {e}");
            continue;
        }

        let path = dir.join(format!("img_{index:06}.jpg"));
        match std::fs::write(&path, &out) {
            Ok(()) => index += 1,
            Err(e) => {
                tracing::error!("failed to write {}: {e}", path.display());
                write_failures += 1;
                if write_failures >= 10 {
                    tracing::error!("10 consecutive write failures, stopping the recorder");
                    break;
                }
            }
        }
    }

    tracing::info!("recorder wrote {index} images to {}", dir.display());
}
