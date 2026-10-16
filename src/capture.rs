//! Camera capture: runs `rpicam-vid` (libcamera) and reads raw I420 frames from its stdout.
//!
//! `rpicam-vid --codec yuv420` emits a continuous stream of raw YUV420 (I420) frames:
//! a Y plane of `w * h` bytes followed by 2x2-subsampled U and V planes
//! (`w/2 * h/2` bytes each), with no framing or headers in between. We exploit
//! that by reading exactly one frame's worth of bytes at a time.

use std::io::Read;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Context;
use tokio::sync::mpsc;

#[derive(Clone, Copy, Debug)]
pub struct CameraSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// One raw I420 frame as delivered by `rpicam-vid`.
///
/// Rows are **strided**: `rpicam-vid` writes each plane with the libcamera
/// buffer stride, which is `width` rounded up to a multiple of 128 bytes
/// (verified on PiSP: 1280 -> 1280, 1456 -> 1536). The image content is the
/// left `width` pixels of each row.
#[derive(Clone)]
pub struct Frame {
    pub data: Vec<u8>,
    pub height: u32,
    pub stride: u32,
}

impl Frame {
    pub fn stride_for(width: u32) -> u32 {
        width.div_ceil(128) * 128
    }

    /// Byte offset of the start of the U plane (V follows it, same size).
    pub fn chroma_offset(&self) -> usize {
        (self.stride * self.height) as usize
    }

    pub fn chroma_stride(&self) -> u32 {
        self.stride / 2
    }

    pub fn chroma_plane_size(&self) -> usize {
        (self.chroma_stride() * (self.height / 2)) as usize
    }

    /// Size in bytes of one raw I420 frame including row padding.
    pub fn buffer_size(&self) -> usize {
        (self.stride * self.height * 3 / 2) as usize
    }
}

/// Spawn `rpicam-vid` and a dedicated reader thread.
///
/// The returned channel receives one `Vec<u8>` per captured frame, laid out as
/// I420. It is bounded: a consumer that cannot keep up back-pressures the
/// reader, and `rpicam-vid` then drops frames on its end rather than memory
/// growing without bound.
///
/// The caller owns the child so it can stop the pipeline later; when it does,
/// it must set `stopping` first so the reader thread treats the resulting EOF
/// as intentional rather than a camera failure.
///
/// `extra_args` are appended to the fixed options; they must not include
/// `--output`, which is always passed last (it takes a value).
pub fn spawn(
    settings: &CameraSettings,
    stopping: Arc<AtomicBool>,
    extra_args: &[String],
) -> anyhow::Result<(mpsc::Receiver<Frame>, Child)> {
    let stride = Frame::stride_for(settings.width);
    let height = settings.height;
    let buffer_size = (stride * height * 3 / 2) as usize;
    let mut args: Vec<String> = vec![
        "--timeout".into(),
        "0".into(),
        "--nopreview".into(),
        "--width".into(),
        settings.width.to_string(),
        "--height".into(),
        settings.height.to_string(),
        "--framerate".into(),
        settings.fps.to_string(),
        "--codec".into(),
        "yuv420".into(),
    ];
    args.extend_from_slice(extra_args);
    // `--output` takes a value, so the path must follow it immediately: keep
    // this pair last.
    args.push("--output".into());
    args.push("-".into());
    args.push("--flush".into());

    let mut child = Command::new("rpicam-vid")
        .args(&args)
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to launch rpicam-vid (is rpicam-apps installed?)")?;

    let mut stdout = child
        .stdout
        .take()
        .context("rpicam-vid produced no stdout")?;

    let (tx, rx) = mpsc::channel(4);

    std::thread::Builder::new()
        .name("camera-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; buffer_size];
            loop {
                match stdout.read_exact(&mut buf) {
                    Ok(()) => {
                        let frame = Frame {
                            data: buf.clone(),
                            height,
                            stride,
                        };
                        if tx.blocking_send(frame).is_err() {
                            break; // consumer gone; pipeline is shutting down
                        }
                    }
                    Err(e) => {
                        if stopping.load(Ordering::SeqCst) {
                            tracing::info!("camera stream ended (stopped)");
                        } else {
                            // Do not exit the process: the HTTP API must stay
                            // up so the camera can be restarted. The failure
                            // is surfaced via the camera manager's status.
                            tracing::error!("camera stream ended unexpectedly: {e}");
                        }
                        break;
                    }
                }
            }
        })?;

    Ok((rx, child))
}
