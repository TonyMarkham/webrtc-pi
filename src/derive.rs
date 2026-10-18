//! Derivation of training-data variants from a capture run.
//!
//! Reads the colour JPEGs of a run directory (whatever their dimensions;
//! currently 1456x1088 from rpicam-still) and writes three derived
//! variants, each in its own subdirectory:
//!
//! - `half/<name>.jpg`       half-size colour  (exact 2x2 box downscale)
//! - `grey/<name>.jpg`       full-size greyscale (BT.601 luma)
//! - `half-grey/<name>.jpg`  half-size greyscale
//!
//! Transforms are defined here ONCE so training and inference can stay in
//! sync:
//!
//! - downscale: exact 2x2 box average (the source is exactly 2x the target,
//!   so a box filter is the canonical area average)
//! - greyscale: BT.601 luma, `0.299 R + 0.587 G + 0.114 B` (identical to
//!   OpenCV's COLOR_BGR2GRAY and PIL's convert("L"))
//!
//! Because both transforms are linear, halving-then-greyscaling equals
//! greyscaling-then-halving; we compute luma once and box it.
//!
//! Derivation is **incremental and resilient**: sources whose variants are
//! already up to date are skipped, and a file that cannot be decoded (e.g.
//! a still that was being written when the run was downloaded) is skipped
//! with a warning instead of failing the whole run. `needs_derive` compares
//! counts, so the next download picks up what was skipped.

use std::path::Path;

use anyhow::Context;

pub const QUALITY: i32 = 93;

/// Count the colour source images (`img_*.jpg`) in a directory.
fn count_sources(dir: &Path) -> usize {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter(|e| {
                    let p = e.path();
                    p.is_file()
                        && p.extension().is_some_and(|x| x == "jpg")
                        && e.file_name().to_string_lossy().starts_with("img_")
                })
                .count()
        })
        .unwrap_or(0)
}

/// Variants are considered derived when the `half` subdirectory contains a
/// variant for every colour source. This survives partial derivations: a
/// run that grew (or whose derivation failed partway) is picked up again on
/// the next download.
#[allow(dead_code)] // used by the server; the CLI derives unconditionally
pub fn needs_derive(run_dir: &Path) -> bool {
    count_sources(&run_dir.join("half")) != count_sources(run_dir)
}

/// Derive all variants for one run directory. Idempotent: sources with
/// up-to-date variants are skipped, existing variant files are otherwise
/// overwritten. Undecodable sources are skipped with a warning. Returns the
/// number of source images processed (derived or already current).
pub fn derive_run(dir: &Path) -> anyhow::Result<usize> {
    let mut sources: Vec<PathBuf> = std::fs::read_dir(dir)
        .with_context(|| format!("read {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.is_file()
                && p.extension().is_some_and(|e| e == "jpg")
                && p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("img_"))
        })
        .collect();
    sources.sort();
    if sources.is_empty() {
        return Ok(0);
    }

    let half_dir = dir.join("half");
    let grey_dir = dir.join("grey");
    let halfgrey_dir = dir.join("half-grey");
    std::fs::create_dir_all(&half_dir)?;
    std::fs::create_dir_all(&grey_dir)?;
    std::fs::create_dir_all(&halfgrey_dir)?;

    let mut decompressor = turbojpeg::Decompressor::new()?;
    let mut compressor = turbojpeg::Compressor::new()?;
    compressor.set_quality(QUALITY)?;

    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();

    let mut count = 0usize;
    for src in &sources {
        let name = src.file_name().unwrap().to_string_lossy().to_string();
        let half_out = half_dir.join(&name);

        // Skip sources whose variants are all present and at least as new
        // as the source (an actively-growing run re-derives only the tail).
        let grey_out = grey_dir.join(&name);
        let halfgrey_out = halfgrey_dir.join(&name);
        if let (Some(src_t), Some(half_t)) = (mtime(src), mtime(&half_out)) {
            if half_t >= src_t && grey_out.exists() && halfgrey_out.exists() {
                count += 1;
                continue;
            }
        }

        let derived = (|| -> anyhow::Result<()> {
            let jpeg = std::fs::read(src)
                .with_context(|| format!("read {}", src.display()))?;
            let header = decompressor.read_header(&jpeg)?;
            let (w, h) = (header.width, header.height);
            if w % 2 != 0 || h % 2 != 0 {
                anyhow::bail!("odd dimensions {w}x{h}, cannot halve");
            }

            // Full decode to RGBA.
            let mut full = turbojpeg::Image {
                pixels: vec![0u8; 4 * w * h],
                width: w,
                pitch: 4 * w,
                height: h,
                format: turbojpeg::PixelFormat::RGBA,
            };
            decompressor.decompress(&jpeg, full.as_deref_mut())?;

            // Greyscale at full size (BT.601 luma).
            let grey: Vec<u8> = full
                .pixels
                .chunks_exact(4)
                .map(|px| {
                    (0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32).round()
                        as u8
                })
                .collect();

            // Exact 2x2 box averages.
            let (hw, hh) = ((w / 2) as usize, (h / 2) as usize);
            let mut half = vec![0u8; 4 * hw * hh];
            let mut halfgrey = vec![0u8; hw * hh];
            for oy in 0..hh {
                for ox in 0..hw {
                    let (x0, x1) = (ox * 2, ox * 2 + 1);
                    let (y0, y1) = (oy * 2, oy * 2 + 1);
                    for c in 0..4 {
                        let sum = (full.pixels[(y0 * w as usize + x0) * 4 + c] as u32)
                            + (full.pixels[(y0 * w as usize + x1) * 4 + c] as u32)
                            + (full.pixels[(y1 * w as usize + x0) * 4 + c] as u32)
                            + (full.pixels[(y1 * w as usize + x1) * 4 + c] as u32);
                        half[(oy * hw + ox) * 4 + c] = (sum / 4) as u8;
                    }
                    let gsum = (grey[y0 * w as usize + x0] as u32)
                        + (grey[y0 * w as usize + x1] as u32)
                        + (grey[y1 * w as usize + x0] as u32)
                        + (grey[y1 * w as usize + x1] as u32);
                    halfgrey[oy * hw + ox] = (gsum / 4) as u8;
                }
            }

            // Encode and write the three variants. The compressor's
            // colorspace must match the input pixels: GRAY pixels can only
            // go into a grayscale JPEG ("unsupported color conversion
            // request" otherwise).
            compressor.set_colorspace(turbojpeg::Colorspace::YCbCr)?;
            let half_img = turbojpeg::Image {
                pixels: &half[..],
                width: hw,
                pitch: 4 * hw,
                height: hh,
                format: turbojpeg::PixelFormat::RGBA,
            };
            std::fs::write(&half_out, compressor.compress_to_vec(half_img.as_deref())?)?;

            compressor.set_colorspace(turbojpeg::Colorspace::Gray)?;
            let grey_img = turbojpeg::Image {
                pixels: &grey[..],
                width: w,
                pitch: w,
                height: h,
                format: turbojpeg::PixelFormat::GRAY,
            };
            std::fs::write(&grey_out, compressor.compress_to_vec(grey_img.as_deref())?)?;

            let halfgrey_img = turbojpeg::Image {
                pixels: &halfgrey[..],
                width: hw,
                pitch: hw,
                height: hh,
                format: turbojpeg::PixelFormat::GRAY,
            };
            std::fs::write(&halfgrey_out, compressor.compress_to_vec(halfgrey_img.as_deref())?)?;
            Ok(())
        })();

        match derived {
            Ok(()) => count += 1,
            // One undecodable file (typically the newest still, caught
            // mid-write) must not sink the run; `needs_derive` will retry
            // it on the next download.
            Err(e) => tracing::warn!("skipping {}: {e:#}", src.display()),
        }
    }

    Ok(count)
}

use std::path::PathBuf;
