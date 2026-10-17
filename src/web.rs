//! HTTP server: serves the embedded viewer page, the WebSocket signaling
//! endpoint used to exchange SDP offers/answers, the camera mode controls,
//! and download access to the captured stills.

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;

use crate::camera;
use crate::session;
use crate::state::AppState;

const INDEX_HTML: &str = include_str!("../static/index.html");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/camera/start", post(camera_start))
        .route("/camera/capture", post(camera_capture))
        .route("/camera/stop", post(camera_stop))
        .route("/camera/status", get(camera_status))
        .route("/images", get(images_list).delete(image_delete))
        .route("/images/file", get(image_file))
        .route("/images/archive.tar", get(images_tar))
        .route("/preview.jpg", get(preview_jpg))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

// ---- camera mode control --------------------------------------------------

async fn camera_start(State(state): State<AppState>) -> Response {
    match state.camera.start().await {
        Ok(()) => Json(serde_json::json!({ "mode": "streaming" })).into_response(),
        Err(e) => camera_error(e),
    }
}

#[derive(Deserialize)]
struct CaptureQuery {
    /// Frames per second (1..60). Omit to use the server default.
    fps: Option<u32>,
    /// Exposure time in microseconds; 0 = auto. Omit for the server default.
    shutter: Option<u32>,
    /// Analog gain; 0 = auto. Omit for the server default.
    gain: Option<f32>,
}

async fn camera_capture(
    State(state): State<AppState>,
    Query(q): Query<CaptureQuery>,
) -> Response {
    if let Some(fps) = q.fps {
        if fps == 0 || fps > 60 {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "fps must be 1..60" })),
            )
                .into_response();
        }
    }
    if let Some(s) = q.shutter {
        if s > 1_000_000 {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "shutter must be 0..1000000 microseconds" })),
            )
                .into_response();
        }
    }
    if let Some(g) = q.gain {
        if !(0.0..=32.0).contains(&g) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": "gain must be 0..32" })),
            )
                .into_response();
        }
    }
    let fps = q.fps.unwrap_or_else(|| state.camera.default_fps());
    let shutter = q.shutter.or_else(|| Some(state.camera.default_shutter_us()));
    let gain = q.gain.or_else(|| Some(state.camera.default_gain()));
    match state.camera.capture(fps, shutter, gain).await {
        Ok(run) => Json(serde_json::json!({ "mode": "capturing", "run": run })).into_response(),
        Err(e) => camera_error(e),
    }
}

async fn camera_stop(State(state): State<AppState>) -> Response {
    state.camera.stop().await;
    Json(serde_json::json!({ "mode": "stopped" })).into_response()
}

async fn camera_status(State(state): State<AppState>) -> Json<camera::CameraStatus> {
    Json(state.camera.status())
}

fn camera_error(e: anyhow::Error) -> Response {
    tracing::warn!("camera request failed: {e:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({ "error": e.to_string() })),
    )
        .into_response()
}

// ---- captured stills -------------------------------------------------------

#[derive(Deserialize)]
struct ImagesQuery {
    /// Max entries (default 500, capped at 5000).
    limit: Option<usize>,
}

async fn images_list(State(state): State<AppState>, Query(q): Query<ImagesQuery>) -> Response {
    let limit = q.limit.unwrap_or(500).min(5000);
    Json(state.camera.list_images(limit)).into_response()
}

#[derive(Deserialize)]
struct ImagePathQuery {
    path: String,
}

async fn image_file(
    State(state): State<AppState>,
    Query(q): Query<ImagePathQuery>,
) -> Response {
    let name = q.path.rsplit('/').next().unwrap_or("image.jpg").to_owned();
    match state.camera.open_image(&q.path) {
        Ok(file) => {
            let stream = file_body(file);
            let headers = [
                (header::CONTENT_TYPE, "image/jpeg".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"{name}\""),
                ),
            ];
            (headers, stream).into_response()
        }
        Err(e) => {
            tracing::info!("image fetch failed: {e:#}");
            (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": e.to_string() })))
                .into_response()
        }
    }
}

/// Read a std file on the blocking pool, yielding chunks as a response body.
fn file_body(file: std::fs::File) -> Body {
    use std::io::Read;
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(8);
    tokio::task::spawn_blocking(move || {
        let mut file = file;
        let mut buf = vec![0u8; 64 * 1024];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.blocking_send(Ok(Bytes::copy_from_slice(&buf[..n]))).is_err() {
                        break;
                    }
                }
                Err(e) => {
                    let _ = tx.blocking_send(Err(e.to_string()));
                    break;
                }
            }
        }
    });
    Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }))
}

/// Stream a whole capture run as a tar archive.
async fn images_tar(
    State(state): State<AppState>,
    Query(q): Query<ImagePathQuery>,
) -> Response {
    let manager = std::sync::Arc::clone(&state.camera);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, String>>(16);
    let run = q.path;
    let run_name = run.clone();
    tokio::task::spawn_blocking(move || {
        if let Err(e) = manager.tar_run(&run_name, tx.clone()) {
            let _ = tx.blocking_send(Err(format!("{e:#}")));
        }
    });
    let body = Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    let headers = [
        (header::CONTENT_TYPE, "application/x-tar".to_string()),
        (
            header::CONTENT_DISPOSITION,
            format!(
                "attachment; filename=\"{}.tar\"",
                run.rsplit('/').next().unwrap_or("run")
            ),
        ),
    ];
    (headers, body).into_response()
}

async fn image_delete(State(state): State<AppState>, Query(q): Query<ImagePathQuery>) -> Response {
    match state.camera.delete(&q.path) {
        Ok(()) => Json(serde_json::json!({ "deleted": q.path })).into_response(),
        Err(e) => {
            tracing::info!("delete failed: {e:#}");
            (
                StatusCode::NOT_FOUND,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

// ---- live preview for the companion app ------------------------------------

/// Latest low-rate JPEG preview: the encoder's tap while streaming, or the
/// newest still of the current run while capturing.
async fn preview_jpg(State(state): State<AppState>) -> Response {
    let latest = state.preview.borrow().clone();
    if let Some(jpeg) = latest {
        let headers = [
            (header::CONTENT_TYPE, "image/jpeg".to_string()),
            (header::CACHE_CONTROL, "no-store".to_string()),
        ];
        return (headers, jpeg).into_response();
    }
    if let Some(path) = state.camera.latest_still() {
        match std::fs::File::open(&path) {
            Ok(file) => {
                let headers = [
                    (header::CONTENT_TYPE, "image/jpeg".to_string()),
                    (header::CACHE_CONTROL, "no-store".to_string()),
                ];
                return (headers, file_body(file)).into_response();
            }
            Err(e) => tracing::debug!("preview still open failed: {e}"),
        }
    }
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({ "error": "no preview available; camera is stopped" })),
    )
        .into_response()
}

// ---- websocket signaling ---------------------------------------------------

#[derive(Deserialize)]
struct SignalMessage {
    r#type: String,
    sdp: String,
}

async fn ws_handler(ws: axum::extract::ws::WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, state: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Wait for the browser's offer (non-trickle: gathered candidates are
    // included in the SDP itself).
    let offer = loop {
        match receiver.next().await {
            Some(Ok(axum::extract::ws::Message::Text(text))) => {
                match serde_json::from_str::<SignalMessage>(&text) {
                    Ok(msg) if msg.r#type == "offer" => break msg.sdp,
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::info!("bad signaling message: {e}");
                        return;
                    }
                }
            }
            Some(Ok(_)) => continue,
            Some(Err(e)) => {
                tracing::info!("websocket error before offer: {e}");
                return;
            }
            None => return,
        }
    };

    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let mut stream_task = match session::start(&state, offer, shutdown_rx.clone()).await {
        Ok((answer_sdp, handle)) => {
            let answer = serde_json::json!({ "type": "answer", "sdp": answer_sdp });
            if let Err(e) = sender.send(axum::extract::ws::Message::text(answer.to_string())).await {
                tracing::info!("failed to send answer: {e}");
                let _ = shutdown_tx.send(true);
                let _ = handle.await;
                return;
            }
            tracing::info!("sent answer to viewer");
            handle
        }
        Err(e) => {
            tracing::error!("negotiation failed: {e}");
            let _ = sender
                .send(axum::extract::ws::Message::text(
                    serde_json::json!({ "type": "error", "sdp": e.to_string() }).to_string(),
                ))
                .await;
            return;
        }
    };

    // Keep the socket open while the session streams. If the browser goes
    // away, or the session ends, tear everything down.
    tokio::select! {
        _ = &mut stream_task => {}
        msg = receiver.next() => {
            let closed = matches!(msg, None | Some(Err(_)));
            let _ = shutdown_tx.send(true);
            let _ = stream_task.await;
            if !closed {
                // Drain any remaining messages until the socket closes.
                while let Some(result) = receiver.next().await {
                    if result.is_err() {
                        break;
                    }
                }
            }
        }
    }

    tracing::info!("viewer session ended");
}
