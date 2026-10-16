//! HTTP server: serves the embedded viewer page and the WebSocket signaling
//! endpoint used to exchange SDP offers/answers.

use axum::extract::State;
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use axum::Router;
use futures::{SinkExt, StreamExt};
use serde::Deserialize;
use tokio::sync::watch;

use crate::session;
use crate::state::AppState;

const INDEX_HTML: &str = include_str!("../static/index.html");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/ws", get(ws_handler))
        .route("/camera/start", post(camera_start))
        .route("/camera/stop", post(camera_stop))
        .route("/camera/status", get(camera_status))
        .with_state(state)
}

async fn index() -> Html<&'static str> {
    Html(INDEX_HTML)
}

async fn camera_start(State(state): State<AppState>) -> Response {
    match state.camera.start().await {
        Ok(()) => Json(serde_json::json!({ "running": true })).into_response(),
        Err(e) => {
            tracing::warn!("camera start failed: {e:#}");
            (
                axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({ "running": false, "error": e.to_string() })),
            )
                .into_response()
        }
    }
}

async fn camera_stop(State(state): State<AppState>) -> Response {
    state.camera.stop().await;
    Json(serde_json::json!({ "running": false })).into_response()
}

async fn camera_status(State(state): State<AppState>) -> Json<crate::camera::CameraStatus> {
    Json(state.camera.status())
}

#[derive(Deserialize)]
struct SignalMessage {
    r#type: String,
    sdp: String,
}

async fn ws_handler(ws: axum::extract::ws::WebSocketUpgrade, State(state): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, state))
}

async fn handle_socket(socket: axum::extract::ws::WebSocket, state: AppState) {
    use axum::extract::ws::Message;

    let (mut sender, mut receiver) = socket.split();

    // Wait for the browser's offer (non-trickle: gathered candidates are
    // included in the SDP itself).
    let offer = loop {
        match receiver.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<SignalMessage>(&text) {
                Ok(msg) if msg.r#type == "offer" => break msg.sdp,
                Ok(_) => continue,
                Err(e) => {
                    tracing::info!("bad signaling message: {e}");
                    return;
                }
            },
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
            if let Err(e) = sender.send(Message::text(answer.to_string())).await {
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
                .send(Message::text(
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
