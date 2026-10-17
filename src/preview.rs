//! Low-rate JPEG preview for the companion desktop app.
//!
//! The x264 encoder thread taps the raw I420 frames it already decodes, at
//! most every [`INTERVAL_MS`], box-averages them down to [`TARGET_WIDTH`]
//! and TurboJPEG-encodes the result into a `watch` channel that keeps only
//! the latest frame. `GET /preview.jpg` serves that value. In capture mode
//! the endpoint instead serves the newest still of the current run (already
//! on disk), so the app gets a live-ish view in both modes.

use anyhow::Context;
use turbojpeg::YuvPlanesImage;

use crate::capture::Frame;

/// Approximate width of the generated preview image.
pub const TARGET_WIDTH: u32 = 640;
/// Minimum time between preview encodes.
pub const INTERVAL: std::time::Duration = std::time::Duration::from_millis(500);
/// Preview JPEG quality (1..100).
pub const QUALITY: i32 = 60;

/// Box-average an I420 `frame` (with row stride `stride` and width `width`)
/// down to roughly [`TARGET_WIDTH`] and JPEG-encode it.
///
/// Every output pixel is the average of a `factor x factor` block, which
/// avoids the aliasing of nearest-neighbor sampling. Chroma planes are
/// downsampled with the same block size on their own (half-resolution)
/// grid. The output planes are contiguous, exactly what TurboJPEG wants.
pub fn encode(frame: &Frame, width: u32, quality: i32) -> anyhow::Result<Vec<u8>> {
    let (w, h) = (width, frame.height);
    let stride = frame.stride as usize;

    // Even output size so the 4:2:0 chroma planes stay aligned.
    let out_w = TARGET_WIDTH.min(w) & !1;
    let out_h = ((out_w * h / w) & !1).max(2);
    let (ow, oh) = (out_w as usize, out_h as usize);

    let chroma_offset = frame.chroma_offset();
    let c_stride = frame.chroma_stride() as usize;
    let chroma_size = frame.chroma_plane_size();
    if frame.data.len() < chroma_offset + 2 * chroma_size {
        anyhow::bail!("malformed frame ({} bytes)", frame.data.len());
    }

    let mut y = vec![0u8; ow * oh];
    let mut u = vec![0u8; (ow / 2) * (oh / 2)];
    let mut v = vec![0u8; u.len()];

    downscale_plane(
        &frame.data[..chroma_offset],
        stride,
        w as usize,
        h as usize,
        ow,
        oh,
        &mut y,
    );
    downscale_plane(
        &frame.data[chroma_offset..chroma_offset + chroma_size],
        c_stride,
        w as usize / 2,
        h as usize / 2,
        ow / 2,
        oh / 2,
        &mut u,
    );
    downscale_plane(
        &frame.data[chroma_offset + chroma_size..],
        c_stride,
        w as usize / 2,
        h as usize / 2,
        ow / 2,
        oh / 2,
        &mut v,
    );

    let image = YuvPlanesImage {
        y_plane: y.as_slice(),
        u_plane: u.as_slice(),
        v_plane: v.as_slice(),
        width: ow,
        height: oh,
        y_stride: ow,
        u_stride: ow / 2,
        v_stride: ow / 2,
        subsamp: turbojpeg::Subsamp::Sub2x2,
    };
    let mut out = turbojpeg::OutputBuf::new_owned();
    let mut compressor = turbojpeg::Compressor::new().context("create JPEG compressor")?;
    compressor
        .set_quality(quality)
        .context("invalid preview JPEG quality")?;
    compressor
        .compress_yuv_planes(&image, &mut out)
        .context("preview JPEG encode")?;
    Ok(out.to_vec())
}

/// Box-average one 8-bit plane from `src` (row stride `stride`, `w x h`
/// content) into a contiguous `out` of `ow x oh`.
fn downscale_plane(
    src: &[u8],
    stride: usize,
    w: usize,
    h: usize,
    ow: usize,
    oh: usize,
    out: &mut [u8],
) {
    let fx = (w / ow).max(1);
    let fy = (h / oh).max(1);
    let area = (fx * fy) as u32;
    for oy in 0..oh {
        let y0 = oy * fy;
        let y1 = (y0 + fy).min(h);
        for ox in 0..ow {
            let x0 = ox * fx;
            let x1 = (x0 + fx).min(w);
            let mut sum = 0u32;
            for y in y0..y1 {
                let row = &src[y * stride..];
                for x in x0..x1 {
                    sum += u32::from(row[x]);
                }
            }
            out[oy * ow + ox] = (sum / area) as u8;
        }
    }
}
