//! Software H.264 encoding with x264.
//!
//! The Pi 5 has no hardware video encoder, so frames are encoded on the CPU.
//! We use the `veryfast` preset with the `zerolatency` tune and a baseline
//! profile (required for broad browser support), and force a keyframe every
//! 2 seconds so new viewers can join without waiting for scene changes.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::broadcast;
use tokio::sync::mpsc;
use x264::{Colorspace, Image, Plane, Preset, Setup, Tune};

use crate::capture::Frame;

/// One encoded H.264 frame in Annex-B format.
pub struct EncodedFrame {
    pub data: Vec<u8>,
    pub keyframe: bool,
    pub duration: Duration,
}

#[derive(Clone, Copy, Debug)]
pub struct EncoderSettings {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    /// Target bitrate in kilobits per second.
    pub bitrate_kbps: u32,
}

/// Spawn the encoder thread. It consumes raw I420 frames and publishes
/// encoded frames on the broadcast channel for every connected viewer.
pub fn spawn(
    settings: EncoderSettings,
    rx: mpsc::Receiver<Frame>,
    tx: broadcast::Sender<Arc<EncodedFrame>>,
) -> anyhow::Result<()> {
    std::thread::Builder::new()
        .name("x264-encode".into())
        .spawn(move || run(settings, rx, tx))?;
    Ok(())
}

fn run(settings: EncoderSettings, mut rx: mpsc::Receiver<Frame>, tx: broadcast::Sender<Arc<EncodedFrame>>) {
    let EncoderSettings {
        width,
        height,
        fps,
        bitrate_kbps,
    } = settings;
    let (w, h) = (width as i32, height as i32);

    let mut encoder = Setup::preset(Preset::Veryfast, Tune::None, false, true)
        .fps(fps, 1)
        .timebase(1, fps)
        .bitrate(bitrate_kbps as i32)
        .baseline()
        .max_keyframe_interval((fps * 2) as i32)
        .scenecut_threshold(0)
        .build(Colorspace::I420, w, h)
        .expect("failed to build x264 encoder");

    // SPS/PPS in Annex-B format. Browsers need them before the first frame
    // they decode, so we prepend them to every keyframe we send out.
    let headers = encoder
        .headers()
        .expect("failed to get x264 headers")
        .entirety()
        .to_vec();

    let frame_duration = Duration::from_nanos(1_000_000_000 / u64::from(fps));
    let mut pts: i64 = 0;

    loop {
        let frame = match rx.blocking_recv() {
            Some(frame) => frame,
            None => break, // capture side is gone; shut down
        };
        let expected = frame.buffer_size();
        if frame.data.len() != expected {
            tracing::warn!("dropping malformed frame ({} bytes, expected {expected})", frame.data.len());
            continue;
        }

        let stride = frame.stride as i32;
        let image = Image::new(
            Colorspace::I420,
            w,
            h,
            &[
                Plane {
                    stride,
                    data: &frame.data[..frame.chroma_offset()],
                },
                Plane {
                    stride: stride / 2,
                    data: &frame.data[frame.chroma_offset()..frame.chroma_offset() + frame.chroma_plane_size()],
                },
                Plane {
                    stride: stride / 2,
                    data: &frame.data[frame.chroma_offset() + frame.chroma_plane_size()..],
                },
            ],
        );

        let (data, picture) = match encoder.encode(pts, image) {
            Ok(out) => out,
            Err(e) => {
                tracing::error!("x264 encode error: {e:?}");
                continue;
            }
        };
        pts += 1;

        let encoded = data.entirety();
        if encoded.is_empty() {
            continue;
        }

        let keyframe = picture.keyframe();
        let mut payload = Vec::with_capacity(if keyframe { headers.len() + encoded.len() } else { encoded.len() });
        if keyframe {
            payload.extend_from_slice(&headers);
        }
        payload.extend_from_slice(encoded);

        let _ = tx.send(Arc::new(EncodedFrame {
            data: payload,
            keyframe,
            duration: frame_duration,
        }));
    }
}
