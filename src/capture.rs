//! Camera capture: runs `rpicam-vid` (libcamera) and reads raw I420 frames from its stdout.
//!
//! `rpicam-vid --codec yuv420` emits a continuous stream of raw YUV420 (I420) frames:
//! a Y plane of `w * h` bytes followed by 2x2-subsampled U and V planes
//! (`w/2 * h/2` bytes each), with no framing or headers in between. We exploit
//! that by reading exactly one frame's worth of bytes at a time.

use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;

use anyhow::Context;

#[derive(Clone, Copy, Debug)]
pub struct CameraSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl CameraSettings {
    /// Size in bytes of one raw I420 frame.
    pub fn frame_size(&self) -> usize {
        (self.width * self.height * 3 / 2) as usize
    }
}

/// Spawn `rpicam-vid` and a dedicated reader thread.
///
/// The returned channel receives one `Vec<u8>` per captured frame, laid out as I420.
pub fn spawn(settings: &CameraSettings) -> anyhow::Result<mpsc::Receiver<Vec<u8>>> {
    let frame_size = settings.frame_size();
    let mut child = Command::new("rpicam-vid")
        .args([
            "--timeout",
            "0",
            "--nopreview",
            "--width",
            &settings.width.to_string(),
            "--height",
            &settings.height.to_string(),
            "--framerate",
            &settings.fps.to_string(),
            "--codec",
            "yuv420",
            "--output",
            "-",
            "--flush",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to launch rpicam-vid (is rpicam-apps installed?)")?;

    let mut stdout = child
        .stdout
        .take()
        .context("rpicam-vid produced no stdout")?;

    let (tx, rx) = mpsc::channel::<Vec<u8>>();

    std::thread::Builder::new()
        .name("camera-read".into())
        .spawn(move || {
            let mut buf = vec![0u8; frame_size];
            loop {
                match stdout.read_exact(&mut buf) {
                    Ok(()) => {
                        if tx.send(buf.clone()).is_err() {
                            break; // encoder gone; server is shutting down
                        }
                    }
                    Err(e) => {
                        tracing::error!("camera stream ended: {e}");
                        std::process::exit(1);
                    }
                }
            }
        })?;

    // Reap the child if we ever exit; otherwise it would linger.
    std::thread::Builder::new()
        .name("camera-reap".into())
        .spawn(move || {
            let _ = child.wait();
        })?;

    Ok(rx)
}
