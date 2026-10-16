//! WebRTC video server for the Raspberry Pi 5.
//!
//! Pipeline: `rpicam-vid` (libcamera) -> raw I420 frames -> x264 (software
//! encode, since the Pi 5 has no hardware encoder) -> broadcast -> one
//! webrtc-rs peer connection per viewer. Viewers connect with any browser
//! at `http://<pi>:<port>/`; signaling happens over a WebSocket at `/ws`.

mod camera;
mod capture;
mod encode;
mod session;
mod state;
mod web;

use std::sync::Arc;

use anyhow::Context;
use clap::Parser;
use tokio::sync::broadcast;

#[derive(Parser)]
#[command(name = "webrtc-pi", about = "WebRTC camera streaming server for Raspberry Pi 5")]
struct Args {
    /// Capture width in pixels.
    #[arg(long, default_value_t = 1280)]
    width: u32,
    /// Capture height in pixels.
    #[arg(long, default_value_t = 720)]
    height: u32,
    /// Capture/encode framerate.
    #[arg(long, default_value_t = 30)]
    fps: u32,
    /// H.264 target bitrate in kbps.
    #[arg(long, default_value_t = 2500)]
    bitrate: u32,
    /// HTTP/WebSocket listen port.
    #[arg(long, default_value_t = 8080)]
    port: u16,
    /// Start the camera pipeline at boot instead of waiting for
    /// `POST /camera/start`.
    #[arg(long)]
    auto_start: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let args = Args::parse();

    let camera_settings = capture::CameraSettings {
        width: args.width,
        height: args.height,
        fps: args.fps,
    };
    let encoder_settings = encode::EncoderSettings {
        width: args.width,
        height: args.height,
        fps: args.fps,
        bitrate_kbps: args.bitrate,
    };

    let (frames_tx, _) = broadcast::channel::<Arc<encode::EncodedFrame>>(64);
    let camera = Arc::new(camera::CameraManager::new(
        camera_settings,
        encoder_settings,
        frames_tx.clone(),
    ));

    if args.auto_start {
        camera
            .start()
            .await
            .context("failed to auto-start the camera pipeline")?;
        tracing::info!("camera auto-started (--auto-start)");
    }

    let state = state::AppState {
        frames: frames_tx,
        camera,
    };
    let app = web::router(state);

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", args.port)).await?;
    tracing::info!(
        "serving viewer at http://0.0.0.0:{} ({}x{} @{}fps, {}kbps)",
        args.port,
        args.width,
        args.height,
        args.fps,
        args.bitrate
    );

    axum::serve(listener, app).await?;
    Ok(())
}
