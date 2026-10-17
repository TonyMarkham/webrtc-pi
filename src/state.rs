//! Shared application state.

use std::sync::Arc;

use bytes::Bytes;
use tokio::sync::broadcast;
use tokio::sync::watch;

use crate::camera::CameraManager;
use crate::encode::EncodedFrame;

#[derive(Clone)]
pub struct AppState {
    /// Encoded H.264 frames published by the encoder thread.
    pub frames: broadcast::Sender<Arc<EncodedFrame>>,
    /// Latest low-rate preview JPEG (see `preview`), for the desktop app.
    pub preview: watch::Sender<Option<Bytes>>,
    /// Start/stop control for the camera pipeline.
    pub camera: Arc<CameraManager>,
}
