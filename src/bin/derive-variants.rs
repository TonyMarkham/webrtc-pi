//! Derive training-data variants from a capture run.
//!
//! Reads the 1440x1080 colour JPEGs of a run directory and writes three
//! derived variants, each in its own subdirectory:
//!
//! - `half/<name>.jpg`       720x540 colour  (exact 2x2 box downscale)
//! - `grey/<name>.jpg`       1440x1080 greyscale (BT.601 luma)
//! - `half-grey/<name>.jpg`  720x540 greyscale
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
//! Usage: `derive-variants <run-dir> [run-dir ...]`
//!
//! Requires JPEGs to be the standard capture naming (`img_*.jpg`). Existing
//! variant subdirectories are overwritten in place, so the tool is
//! idempotent.

use std::path::PathBuf;

use anyhow::Context;

const QUALITY: i32 = 93;

fn main() -> anyhow::Result<()> {
    let dirs: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    if dirs.is_empty() {
        eprintln!("usage: derive-variants <run-dir> [run-dir ...]");
        std::process::exit(2);
    }

    let mut total = 0usize;
    for dir in &dirs {
        let count = derive_run(dir).with_context(|| format!("{}", dir.display()))?;
        total += count;
    }
    println!("derived variants for {total} images");
    Ok(())
}

fn derive_run(dir: &PathBuf) -> anyhow::Result<usize> {
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
        eprintln!("{}: no img_*.jpg files, skipping", dir.display());
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

    let mut count = 0usize;
    let started = std::time::Instant::now();
    for src in &sources {
        let name = src.file_name().unwrap().to_string_lossy().to_string();
        let jpeg = std::fs::read(src)
            .with_context(|| format!("read {}", src.display()))?;
        let header = decompressor.read_header(&jpeg)?;
        let (w, h) = (header.width, header.height);
        if w % 2 != 0 || h % 2 != 0 {
            anyhow::bail!("{}: odd dimensions {w}x{h}, cannot halve", src.display());
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
                (0.299 * px[0] as f32 + 0.587 * px[1] as f32 + 0.114 * px[2] as f32).round() as u8
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

        // Encode and write the three variants. The compressor's colorspace
        // must match the input pixels: GRAY pixels can only go into a
        // grayscale JPEG ("unsupported color conversion request" otherwise).
        compressor.set_colorspace(turbojpeg::Colorspace::YCbCr)?;
        let half_img = turbojpeg::Image {
            pixels: &half[..],
            width: hw,
            pitch: 4 * hw,
            height: hh,
            format: turbojpeg::PixelFormat::RGBA,
        };
        std::fs::write(half_dir.join(&name), compressor.compress_to_vec(half_img.as_deref())?)?;

        compressor.set_colorspace(turbojpeg::Colorspace::Gray)?;
        let grey_img = turbojpeg::Image {
            pixels: &grey[..],
            width: w,
            pitch: w,
            height: h,
            format: turbojpeg::PixelFormat::GRAY,
        };
        std::fs::write(grey_dir.join(&name), compressor.compress_to_vec(grey_img.as_deref())?)?;

        let halfgrey_img = turbojpeg::Image {
            pixels: &halfgrey[..],
            width: hw,
            pitch: hw,
            height: hh,
            format: turbojpeg::PixelFormat::GRAY,
        };
        std::fs::write(
            halfgrey_dir.join(&name),
            compressor.compress_to_vec(halfgrey_img.as_deref())?,
        )?;

        count += 1;
        if count % 100 == 0 {
            let rate = count as f64 / started.elapsed().as_secs_f64();
            println!("  {count} images ({rate:.1}/s)");
        }
    }

    Ok(count)
}
