//! Per-viewer WebRTC session: peer connection setup, signaling exchange,
//! and streaming encoded frames from the broadcast channel.

use std::sync::Arc;
use std::time::Instant;

use anyhow::Context;
use bytes::Bytes;
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MIME_TYPE_H264, MediaEngine};
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters,
    RtpCodecKind,
};
use tokio::sync::{broadcast, mpsc, watch};
use webrtc::media_stream::track_local::TrackLocal;
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState,
    RTCPeerConnectionState,
};
use webrtc::rtp_transceiver::RtpSender;

use crate::encode::EncodedFrame;
use crate::state::AppState;

/// H.264 constrained-baseline, packetization-mode 1 — what browsers expect.
fn h264_codec() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line:
                "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    }
}

#[derive(Clone)]
struct Handler {
    gather_done_tx: mpsc::Sender<()>,
    pc_done_tx: mpsc::Sender<()>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_done_tx.try_send(());
        }
    }

    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        tracing::info!("peer connection state: {state}");
        match state {
            RTCPeerConnectionState::Failed
            | RTCPeerConnectionState::Disconnected
            | RTCPeerConnectionState::Closed => {
                let _ = self.pc_done_tx.try_send(());
            }
            _ => {}
        }
    }
}

/// Negotiate a peer connection against a browser offer and spawn the
/// frame-streaming task. Returns the answer SDP (to send back over the
/// signaling channel) and a handle to the streaming task.
pub async fn start(
    state: &AppState,
    offer_sdp: String,
    shutdown_rx: watch::Receiver<bool>,
) -> anyhow::Result<(String, tokio::task::JoinHandle<()>)> {
    let mut media_engine = MediaEngine::default();
    media_engine.register_codec(h264_codec(), RtpCodecKind::Video)?;
    let registry = register_default_interceptors(Registry::new(), &mut media_engine)?;

    let config = rtc::peer_connection::configuration::RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer {
            urls: vec!["stun:stun.l.google.com:19302".to_owned()],
            ..Default::default()
        }])
        .build();

    let (gather_done_tx, mut gather_done_rx) = mpsc::channel(1);
    let (pc_done_tx, pc_done_rx) = mpsc::channel(1);
    let handler = Arc::new(Handler {
        gather_done_tx,
        pc_done_tx,
    });

    let pc = PeerConnectionBuilder::new()
        .with_configuration(config)
        .with_media_engine(media_engine)
        .with_interceptor_registry(registry)
        .with_handler(handler)
        .with_udp_addrs(vec!["0.0.0.0:0".to_owned()])
        .build()
        .await?;

    // One track per viewer, each with its own SSRC.
    let ssrc: u32 = rand::random();
    let codec = h264_codec();
    let track = Arc::new(TrackLocalStaticSample::new(
        Instant::now(),
        MediaStreamTrack::new(
            "pi-camera".to_owned(),
            "pi-camera-video".to_owned(),
            "pi-camera-video".to_owned(),
            RtpCodecKind::Video,
            vec![RTCRtpEncodingParameters {
                rtp_coding_parameters: RTCRtpCodingParameters {
                    ssrc: Some(ssrc),
                    ..Default::default()
                },
                codec: codec.rtp_codec.clone(),
                ..Default::default()
            }],
        ),
    )?);
    let sender: Arc<dyn RtpSender> = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await?;

    // write_sample stamps the payload type on every packet; it must match
    // the codec negotiated for this sender.
    let payload_type = sender
        .get_parameters()
        .await?
        .rtp_parameters
        .codecs
        .first()
        .map(|c| c.payload_type)
        .ok_or_else(|| anyhow::anyhow!("sender has no negotiated codec"))?;

    let offer: RTCSessionDescription =
        serde_json::from_value(serde_json::json!({ "type": "offer", "sdp": offer_sdp }))
            .context("invalid offer SDP JSON")?;
    pc.set_remote_description(offer).await?;

    let answer = pc.create_answer(None).await?;
    pc.set_local_description(answer).await?;

    // Non-trickle ICE: wait for candidate gathering to finish, then hand the
    // complete answer back over the (already open) WebSocket.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), gather_done_rx.recv()).await;
    let local = pc
        .local_description()
        .await
        .ok_or_else(|| anyhow::anyhow!("local description missing after answer"))?;

    let answer_sdp = local.sdp.clone();

    let frames = state.frames.subscribe();
    let handle = tokio::spawn(stream(
        pc, track, ssrc, payload_type, frames, shutdown_rx, pc_done_rx,
    ));

    Ok((answer_sdp, handle))
}

/// Stream encoded frames to one viewer until the peer connection dies,
/// the WebSocket shuts down, or the encoder stops.
async fn stream(
    pc: impl PeerConnection,
    track: Arc<TrackLocalStaticSample>,
    ssrc: u32,
    payload_type: u8,
    mut frames: broadcast::Receiver<Arc<EncodedFrame>>,
    mut shutdown: watch::Receiver<bool>,
    mut pc_done: mpsc::Receiver<()>,
) {
    // Drain whatever is buffered so we start near live. We cannot send
    // anything until a keyframe arrives; the encoder emits one every 2s.
    let mut started = false;
    loop {
        match frames.try_recv() {
            Ok(frame) if frame.keyframe => {
                started = true;
                break;
            }
            Ok(_) => continue,
            Err(broadcast::error::TryRecvError::Empty) => break,
            Err(broadcast::error::TryRecvError::Lagged(_)) => continue,
            Err(broadcast::error::TryRecvError::Closed) => return,
        }
    }
    if !started {
        tracing::debug!("viewer waiting for first keyframe");
    }

    let mut warned_lag = false;
    let mut first = true;
    loop {
        let frame = tokio::select! {
            f = frames.recv() => match f {
                Ok(frame) => frame,
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    if !warned_lag {
                        tracing::warn!("viewer fell behind, dropped {n} frames");
                        warned_lag = true;
                    }
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = shutdown.changed() => break,
            _ = pc_done.recv() => break,
        };

        if !frame.keyframe && !started {
            continue; // still waiting for the viewer's first keyframe
        }
        started = true;

        let sample = Sample {
            data: Bytes::from(frame.data.clone()),
            duration: if first {
                std::time::Duration::ZERO
            } else {
                frame.duration
            },
            ..Sample::new(Instant::now())
        };
        if let Err(e) = track
            .sample_writer(ssrc, payload_type)
            .write_sample(&sample)
            .await
        {
            tracing::info!("stopping viewer stream: {e}");
            break;
        }
        first = false;
    }

    let _ = pc.close().await;
}
