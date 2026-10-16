//! Camera pipeline lifecycle management.
//!
//! The camera is a mutually exclusive resource, so the manager is a small
//! mode machine:
//!
//! - `Stopped`    — nothing running
//! - `Streaming`  — `rpicam-vid` → x264 → broadcast (live view)
//! - `Capturing`  — `rpicam-still --timelapse` writes full-resolution JPEGs
//!                  to disk; the live view is necessarily off
//!
//! Starting one mode stops the other. The broadcast channel is created once
//! and reused across streaming restarts, so viewers can be connected while
//! the camera is stopped or capturing — they simply wait for frames.

use std::path::PathBuf;
use std::process::{Child, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::Context;
use bytes::Bytes;
use serde::Serialize;
use tokio::sync::broadcast;

use crate::capture::{self, CameraSettings};
use crate::encode::{self, EncodedFrame, EncoderSettings};

/// Still-image capture settings.
#[derive(Clone, Copy, Debug)]
pub struct StillSettings {
    pub width: u32,
    pub height: u32,
    /// Default gap between stills; overridable per request.
    pub interval: Duration,
    /// Fixed exposure time in microseconds; 0 = auto. Short exposures freeze
    /// motion (at the cost of gain/noise, since the IMX296 is global shutter
    /// and blur is purely a function of exposure time).
    pub shutter_us: u32,
    /// Fixed analog gain; 0 = auto. The counterpart to `shutter_us`: short
    /// exposures need more gain to stay bright.
    pub gain: f32,
}

/// What the camera is currently doing.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    Stopped,
    Streaming,
    Capturing,
}

/// One running pipeline instance, in either mode.
enum Pipeline {
    Streaming {
        child: Child,
        /// Set before we kill the process so the capture reader thread can
        /// distinguish an intentional stop from a camera failure.
        stopping: Arc<AtomicBool>,
    },
    Capturing {
        child: Child,
        /// Subdirectory of `capture_dir` this run writes into.
        run: String,
        interval: Duration,
        shutter_us: u32,
        gain: f32,
    },
}

#[derive(Default)]
struct Inner {
    pipeline: Option<Pipeline>,
    /// Why the pipeline is not running, if it died unexpectedly. Cleared by a
    /// successful start/capture or an explicit stop.
    last_error: Option<String>,
}

/// Snapshot of the camera state, served by `GET /camera/status`.
#[derive(Serialize)]
pub struct CameraStatus {
    pub mode: Mode,
    /// Capture-mode details, present only while capturing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub images: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interval_secs: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shutter_us: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gain: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// One saved still, described relative to the capture directory.
#[derive(Serialize)]
pub struct ImageInfo {
    /// Path relative to the capture directory, `/`-separated.
    pub path: String,
    pub size: u64,
    /// File modification time, unix seconds.
    pub modified: u64,
}

/// Start/stop control for the camera and the capture sessions.
pub struct CameraManager {
    camera: CameraSettings,
    encoder: EncoderSettings,
    still: StillSettings,
    capture_dir: PathBuf,
    /// Encoded frames published by the (current) encoder thread. Created once,
    /// shared across streaming restarts.
    frames: broadcast::Sender<Arc<EncodedFrame>>,
    inner: Mutex<Inner>,
}

impl CameraManager {
    pub fn new(
        camera: CameraSettings,
        encoder: EncoderSettings,
        still: StillSettings,
        capture_dir: PathBuf,
        frames: broadcast::Sender<Arc<EncodedFrame>>,
    ) -> Self {
        Self {
            camera,
            encoder,
            still,
            capture_dir,
            frames,
            inner: Mutex::new(Inner::default()),
        }
    }

    /// Start live streaming if not already running. Stops capture mode first.
    /// Blocking work runs on the blocking pool; callers `await` this.
    pub async fn start(self: &Arc<Self>) -> anyhow::Result<()> {
        let manager = Arc::clone(self);
        tokio::task::spawn_blocking(move || manager.start_blocking())
            .await
            .context("camera start task panicked")?
    }

    fn start_blocking(self: Arc<Self>) -> anyhow::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        if let Some(Pipeline::Streaming { .. }) = inner.pipeline {
            return Ok(()); // already streaming
        }
        self.stop_pipeline(&mut inner);

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
            let msg =
                format!("rpicam-vid exited immediately: {status} (camera missing or in use?)");
            tracing::error!("{msg}");
            inner.last_error = Some(msg.clone());
            anyhow::bail!(msg);
        }

        inner.pipeline = Some(Pipeline::Streaming { child, stopping });
        tracing::info!("camera started (streaming)");
        Ok(())
    }

    /// Start a capture run: stop any current mode, then run
    /// `rpicam-still --timelapse`, writing JPEGs into a fresh run directory.
    /// Idempotent only in the sense that it always begins a *new* run.
    pub async fn capture(
        self: &Arc<Self>,
        interval: Duration,
        shutter_us: Option<u32>,
        gain: Option<f32>,
    ) -> anyhow::Result<String> {
        let manager = Arc::clone(self);
        tokio::task::spawn_blocking(move || manager.capture_blocking(interval, shutter_us, gain))
            .await
            .context("camera capture task panicked")?
    }

    fn capture_blocking(
        self: Arc<Self>,
        interval: Duration,
        shutter_us: Option<u32>,
        gain: Option<f32>,
    ) -> anyhow::Result<String> {
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        self.stop_pipeline(&mut inner);

        inner.last_error = None;
        std::fs::create_dir_all(&self.capture_dir)
            .with_context(|| format!("create capture dir {}", self.capture_dir.display()))?;
        // Epoch-seconds name: unique, sortable, and needs no date formatting.
        let run = format!("run-{}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs());
        let run_dir = self.capture_dir.join(&run);
        std::fs::create_dir(&run_dir)
            .with_context(|| format!("create run dir {}", run_dir.display()))?;

        // 0 means auto exposure; a request that omits `shutter` uses the
        // configured default, which may itself be auto.
        let shutter_us = shutter_us.unwrap_or(self.still.shutter_us);
        let gain = gain.unwrap_or(self.still.gain);
        let mut args: Vec<String> = vec![
            "--width".into(),
            self.still.width.to_string(),
            "--height".into(),
            self.still.height.to_string(),
            "--timeout".into(),
            "0".into(),
            "--timelapse".into(),
            interval.as_millis().to_string(),
            "--nopreview".into(),
        ];
        if shutter_us > 0 {
            args.push("--shutter".into());
            args.push(shutter_us.to_string());
        }
        if gain > 0.0 {
            args.push("--gain".into());
            args.push(gain.to_string());
        }
        // `--output` takes a value, so the path must follow it immediately:
        // keep this pair last.
        args.push("--output".into());
        args.push(run_dir.join("img_%04d.jpg").to_string_lossy().into_owned());

        let child = std::process::Command::new("rpicam-still")
            .args(&args)
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("failed to launch rpicam-still (is rpicam-apps installed?)")?;
        let mut child = child;

        std::thread::sleep(Duration::from_millis(300));
        if let Ok(Some(status)) = child.try_wait() {
            let msg =
                format!("rpicam-still exited immediately: {status} (camera missing or in use?)");
            tracing::error!("{msg}");
            inner.last_error = Some(msg.clone());
            anyhow::bail!(msg);
        }

        inner.pipeline = Some(Pipeline::Capturing {
            child,
            run: run.clone(),
            interval,
            shutter_us,
            gain,
        });
        let shutter_desc = if shutter_us > 0 { format!("{shutter_us} µs") } else { "auto".to_string() };
        let gain_desc = if gain > 0.0 { format!("{gain}") } else { "auto".to_string() };
        tracing::info!(
            "camera started (capturing every {} ms, shutter {shutter_desc}, gain {gain_desc}, into {run})",
            interval.as_millis()
        );
        Ok(run)
    }

    /// Stop whichever mode is running. Idempotent; clears any stale error.
    pub async fn stop(self: &Arc<Self>) {
        let manager = Arc::clone(self);
        let _ = tokio::task::spawn_blocking(move || manager.stop_blocking()).await;
    }

    fn stop_blocking(&self) {
        let mut inner = self.inner.lock().unwrap();
        self.stop_pipeline(&mut inner);
        inner.last_error = None;
    }

    /// Kill and reap the current pipeline, whatever mode it is in.
    fn stop_pipeline(&self, inner: &mut Inner) {
        match inner.pipeline.take() {
            Some(Pipeline::Streaming { mut child, stopping }) => {
                stopping.store(true, Ordering::SeqCst);
                let _ = child.kill();
                let _ = child.wait();
                tracing::info!("camera stopped (streaming)");
            }
            Some(Pipeline::Capturing { mut child, run, .. }) => {
                let _ = child.kill();
                let _ = child.wait();
                tracing::info!("camera stopped (capturing into {run})");
            }
            None => {}
        }
    }

    /// Current mode. Also reaps a dead child so a crash is reflected here
    /// (and recorded as `error`) without any other call.
    pub fn status(&self) -> CameraStatus {
        let mut inner = self.inner.lock().unwrap();
        self.reap(&mut inner);
        let (mode, run, interval_secs, shutter_us, gain) = match &inner.pipeline {
            None => (Mode::Stopped, None, None, None, None),
            Some(Pipeline::Streaming { .. }) => (Mode::Streaming, None, None, None, None),
            Some(Pipeline::Capturing { run, interval, shutter_us, gain, .. }) => (
                Mode::Capturing,
                Some(run.clone()),
                Some(interval.as_secs_f64()),
                Some(*shutter_us),
                Some(*gain),
            ),
        };
        let images = run.as_ref().map(|run| self.count_images(run));
        CameraStatus {
            mode,
            run,
            images,
            interval_secs,
            shutter_us,
            gain,
            error: inner.last_error.clone(),
        }
    }

    /// If the child exited, remove the pipeline. An exit we did not cause is
    /// an error worth remembering and reporting.
    fn reap(&self, inner: &mut Inner) {
        // Only the streaming pipeline has a reader thread that must be told
        // about intentional stops; capture mode has no such flag.
        let Some(mut pipeline) = inner.pipeline.take() else {
            return;
        };
        let exited = match &mut pipeline {
            Pipeline::Streaming { child, .. } => child.try_wait().map(|s| s.is_some()),
            Pipeline::Capturing { child, .. } => child.try_wait().map(|s| s.is_some()),
        };
        match exited {
            Ok(false) => inner.pipeline = Some(pipeline), // still running
            Ok(true) => {
                let stopping = match &pipeline {
                    Pipeline::Streaming { stopping, .. } => stopping.load(Ordering::SeqCst),
                    Pipeline::Capturing { .. } => false,
                };
                if !stopping {
                    let msg = "camera process exited unexpectedly".to_string();
                    tracing::error!("{msg}");
                    inner.last_error = Some(msg);
                }
            }

            Err(e) => {
                let msg = format!("failed to poll camera process: {e}");
                tracing::error!("{msg}");
                inner.last_error = Some(msg);
            }
        }
    }

    // ---- capture directory helpers -------------------------------------

    /// Resolve a `/`-separated path relative to the capture directory,
    /// refusing traversal outside it. The base may have been configured as a
    /// relative path, so canonicalize both sides before comparing.
    fn resolve(&self, path: &str) -> Option<PathBuf> {
        if path.is_empty() || path.split('/').any(|c| c.is_empty() || c == "." || c == "..") {
            return None;
        }
        let base = self.capture_dir.canonicalize().ok()?;
        let full = self.capture_dir.join(path);
        full.canonicalize()
            .ok()
            .filter(|c| c.starts_with(&base))
    }

    /// Latest saved stills, newest first.
    pub fn list_images(&self, limit: usize) -> Vec<ImageInfo> {
        let mut out = Vec::new();
        let Ok(runs) = std::fs::read_dir(&self.capture_dir) else {
            return out;
        };
        for run in runs.flatten() {
            let Ok(files) = std::fs::read_dir(run.path()) else {
                continue;
            };
            for file in files.flatten() {
                let Ok(meta) = file.metadata() else { continue };
                if !meta.is_file() {
                    continue;
                }
                let rel = file.path();
                let Ok(rel) = rel.strip_prefix(&self.capture_dir) else {
                    continue;
                };
                out.push(ImageInfo {
                    path: rel.to_string_lossy().replace('\\', "/"),
                    size: meta.len(),
                    modified: meta
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs())
                        .unwrap_or(0),
                });
            }
        }
        out.sort_by(|a, b| b.modified.cmp(&a.modified).then(b.path.cmp(&a.path)));
        out.truncate(limit);
        out
    }

    fn count_images(&self, run: &str) -> usize {
        std::fs::read_dir(self.capture_dir.join(run))
            .map(|files| files.flatten().filter(|f| f.path().extension().is_some_and(|e| e == "jpg")).count())
            .unwrap_or(0)
    }

    /// All image paths of one run, in capture order.
    fn run_images(&self, run: &str) -> anyhow::Result<Vec<PathBuf>> {
        let dir = self
            .resolve(run)
            .context("unknown or invalid run name")?;
        let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
            .context("failed to read run directory")?
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|e| e == "jpg"))
            .collect();
        files.sort();
        Ok(files)
    }

    /// Open one image for download.
    pub fn open_image(&self, path: &str) -> anyhow::Result<std::fs::File> {
        let full = self
            .resolve(path)
            .context("unknown or invalid image path")?;
        std::fs::File::open(&full).with_context(|| format!("open {path}"))
    }

    /// Delete one image (`a/b.jpg`) or a whole run (`a`).
    pub fn delete(&self, path: &str) -> anyhow::Result<()> {
        let full = self
            .resolve(path)
            .context("unknown or invalid path")?;
        if full.is_dir() {
            std::fs::remove_dir_all(&full).with_context(|| format!("delete {path}"))
        } else {
            std::fs::remove_file(&full).with_context(|| format!("delete {path}"))
        }
    }

    /// Tar one run's images into `tx`. Intended to run on a blocking thread:
    /// chunks are sent with bounded backpressure, and the consumer going
    /// away (client disconnect) makes the write fail, ending the tar early.
    pub fn tar_run(
        &self,
        run: &str,
        tx: tokio::sync::mpsc::Sender<Result<Bytes, String>>,
    ) -> anyhow::Result<()> {
        struct ChanWriter {
            tx: tokio::sync::mpsc::Sender<Result<Bytes, String>>,
        }
        impl std::io::Write for ChanWriter {
            fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
                self.tx
                    .blocking_send(Ok(Bytes::copy_from_slice(buf)))
                    .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "consumer gone"))?;
                Ok(buf.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }

        let files = self.run_images(run)?;
        let dir = self.resolve(run).context("unknown or invalid run name")?;
        let mut builder = tar::Builder::new(ChanWriter { tx });
        for file in files {
            let name = file
                .strip_prefix(&dir)
                .context("unexpected file location")?
                .to_string_lossy()
                .to_string();
            builder
                .append_path_with_name(&file, name)
                .context("failed to add file to archive")?;
        }
        builder.finish().context("failed to finish archive")?;
        Ok(())
    }

    /// The capture interval used when a request doesn't specify one.
    pub fn default_interval(&self) -> Duration {
        self.still.interval
    }

    /// The exposure time used when a request doesn't specify one (0 = auto).
    pub fn default_shutter_us(&self) -> u32 {
        self.still.shutter_us
    }

    /// The gain used when a request doesn't specify one (0 = auto).
    pub fn default_gain(&self) -> f32 {
        self.still.gain
    }
}
