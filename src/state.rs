//! Shared application state.

use std::sync::Arc;

use tokio::sync::broadcast;

use crate::encode::EncodedFrame;

#[derive(Clone)]
pub struct AppState {
    /// Encoded H.264 frames published by the encoder thread.
    pub frames: broadcast::Sender<Arc<EncodedFrame>>,
}
