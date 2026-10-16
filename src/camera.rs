//! Camera pipeline lifecycle management.
//!
//! The camera is deliberately not started with the server. A `CameraManager`
//! (shared through `AppState`) starts and stops the capture → encode pipeline
//! on demand, driven by the HTTP API or `--auto-start` at boot. The broadcast
//! channel is created once and reused across pipeline restarts, so viewers can
//! be connected before, during, and after a camera stop — they simply wait for
//! frames whenever the pipeline is (re)started.

use std::process::Child;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::capture::{self, CameraSettings};
use crate::encode::{self, EncodedFrame, EncoderSettings};

/// One running pipeline instance: the `rpicam-vid` child plus the flag that
/// tells the capture reader thread whether a stop is intentional.
struct Pipeline {
    child: Child,
    stopping: Arc<AtomicBool>,
}

#[derive(Default)]
struct Inner {
    pipeline: Option<Pipeline>,
    /// Why the pipeline is not running, if it died unexpectedly. Cleared by a
    /// successful start or an explicit stop.
    last_error: Option<String>,
}

/// Snapshot of the camera pipeline state, served by `GET /camera/status`.
#[derive(Serialize)]
pub struct CameraStatus {
    pub running: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Start/stop control for the capture + encode pipeline.
pub struct CameraManager {
    camera: CameraSettings,
    encoder: EncoderSettings,
    /// Encoded frames published by the (current) encoder thread. Created once,
    /// shared across pipeline restarts.
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    inner: Mutex<Inner>,
}

impl CameraManager {
    pub fn new(
        camera: CameraSettings,
        encoder: EncoderSettings,
        frames: broadcast::Sender<Arc<EncodedFrame>>,
    ) -> Self {
        Self {
            camera,
            encoder,
            frames,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Start the pipeline if it is not already running. Idempotent.
    ///
    /// Blocking work (fork/exec, the startup liveness check) runs on the
    /// blocking pool; callers `await` this.
    pub async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        let manager = Arc::clone(self);
        tokio::task::spawn_blocking(move || manager.start_blocking())
            .await
            .context("camera start task panicked")?
    }

    fn start_blocking(self: Arc<Self>) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        if inner.pipeline.is_some() {
            return Ok(()); // already running
        }

        inner.last_error = None;
        let stopping = Arc::new(AtomicBool::new(false));
        let (rx, mut child) = match capture::spawn(&self.camera, Arc::clone(&stopping)) {
            Ok(ok) => ok,
            Err(e) => {
                let msg = format!("failed to launch rpicam-vid: {e:#}");
                inner.last_error = Some(msg.clone());
                anyhow::bail!(msg);
            }
        };
        encode::spawn(self.encoder, rx, self.frames.clone())?;

        // rpicam-vid can die immediately (camera busy, no camera attached).
        // Give it a moment so `start` surfaces that instead of lying.
        std::thread::sleep(Duration::from_millis(300));
        if let Ok(Some(status)) = child.try_wait() {
            let msg = format!(
                "rpicam-vid exited immediately: {status} (camera missing or in use?)"
            );
            tracing::error!("{msg}");
            inner.last_error = Some(msg.clone());
            anyhow::bail!(msg);
        }

        inner.pipeline = Some(Pipeline { child, stopping });
        tracing::info!("camera started");
        Ok(())
    }

    /// Stop the pipeline if it is running. Idempotent; clears any stale error.
    pub async fn stop(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || manager.stop_blocking()).await;
    }

    fn stop_blocking(&self) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(mut pipeline) = inner.pipeline.take() {
            pipeline.stopping.store(true, Ordering::SeqCst);
            // SIGKILL the child; the reader thread sees EOF on stdout, drops
            // the frame channel, and the encoder thread exits with it.
            let _ = pipeline.child.kill();
            let _ = pipeline.child.wait();
            tracing::info!("camera stopped");
        }
        inner.last_error = None;
    }

    /// Current pipeline state. Also reaps a dead child so a crash is
    /// reflected here (and recorded as `error`) without any other call.
    pub fn status(&self) -> CameraStatus {
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        CameraStatus {
            running: inner.pipeline.is_some(),
            error: inner.last_error.clone(),
        }
    }

    /// If the child exited, remove the pipeline. An exit we did not cause is
    /// an error worth remembering and reporting.
    fn reap(&self, inner: &mut Inner) {
        let Some(mut pipeline) = inner.pipeline.take() else {
            return;
        };
        match pipeline.child.try_wait() {
            Ok(None) => inner.pipeline = Some(pipeline), // still running
            Ok(Some(status)) => {
                if !pipeline.stopping.load(Ordering::SeqCst) {
                    let msg = format!("rpicam-vid exited unexpectedly: {status}");
                    tracing::error!("{msg}");
                    inner.last_error = Some(msg);
                }
            }
            Err(e) => {
                let msg = format!("failed to poll rpicam-vid: {e}");
                tracing::error!("{msg}");
                inner.last_error = Some(msg);
            }
        }
    }
}
