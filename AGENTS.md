# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

A WebRTC video streaming server for the Raspberry Pi 5, written in Rust.
`rpicam-vid` captures raw I420 frames, x264 encodes them in software (the Pi 5
has **no hardware video encoder** — only HEVC decode), and webrtc-rs streams
H.264 to browsers. Signaling is a WebSocket at `/ws`; an embedded viewer page
is served at `/`.

## Build & run

```sh
cargo check                 # typecheck (fast iteration)
cargo build --release       # ~3.5 min on the Pi
cargo run --release         # serves http://0.0.0.0:8080
```

System dependencies (already installed on this machine, but needed on a
fresh Pi 5): `build-essential pkg-config libx264-dev rpicam-apps
libclang-dev`. `libclang` is required because `x264-sys` runs bindgen at
build time.

`RUST_LOG=debug` enables verbose logging.

## Architecture

```
camera.rs    CameraManager: mode machine (stopped/streaming/capturing) that
             owns the camera process handles; starts rpicam-vid+x264 for
             streaming or rpicam-still --timelapse for full-res stills;
             also serves image listing/download/tar/delete for capture runs
capture.rs   spawns rpicam-vid --codec yuv420, reads exact-size I420 frames
             from stdout on a dedicated thread (std::sync::mpsc)
encode.rs    x264 encoder thread: I420 -> H.264 Annex-B, publishes frames on
             a tokio broadcast channel; prepends SPS/PPS to every keyframe
session.rs   one webrtc-rs PeerConnection per viewer: negotiation, track
             with its own SSRC, streams broadcast frames to the track
web.rs       axum: GET / (embedded page + gallery), GET /ws (signaling),
             /camera/{start,capture,stop,status}, /images[...]
state.rs     shared AppState (broadcast sender + CameraManager)
```

Data flow is single-producer/multi-consumer: capture → encoder thread →
`broadcast::Sender<Arc<EncodedFrame>>` → one subscriber task per viewer.
The broadcast channel is created at boot and survives pipeline restarts, so
viewers may be connected while the camera is stopped or capturing (they wait
for frames). The camera is exclusive: entering capture mode kills the
streaming pipeline and vice versa.

## Critical constraints — read before changing media code

- **webrtc crate is 0.21**, a full rewrite of the older 0.13 API
  (Sans-I/O `rtc` core, `PeerConnectionBuilder`, `PeerConnectionEventHandler`
  trait, `TrackLocalStaticSample` with explicit `ssrc`/`payload_type` per
  write). Do not rely on pre-0.21 examples or older LLM training knowledge;
  model code on the official `play-from-disk-h26x` example in the
  webrtc-rs/webrtc repo at the matching version.
- **x264 crate 0.5**: builder is `Setup` (not `EncoderBuilder`), tune is
  `Tune::None` + `zero_latency: true` (there is no `Zerolatency` variant),
  colorspaces are `Colorspace::I420` etc. `x264::Error` does not implement
  `Display` — format with `{:?}`.
- `rpicam-vid --codec yuv420` emits an unframed I420 byte stream: Y plane
  (`w*h`) then U then V (`w*h/4` each). Frames are read with `read_exact`;
  there is no framing to resync on.
- **Keyframes**: browsers can only start decoding on an IDR. The encoder
  forces one every 2 s (`max_keyframe_interval = 2*fps`, `scenecut 0`), and
  viewer tasks wait for a keyframe before sending. PLI/FIR feedback is NOT
  wired to the encoder (see README roadmap) — don't assume it is.
- **Non-trickle ICE**: the browser gathers all candidates before sending its
  offer; the server waits for `on_ice_gathering_state_change == Complete`
  before answering. Keep it that way or implement trickle both sides.

## Conventions

- Edition 2024, stable toolchain.
- Errors: `anyhow::Result` at app boundaries; `tracing` for logs (never
  `println!` outside of `x264`'s own output).
- Threads for blocking work (capture reads, x264), tokio for everything
  async. Don't do blocking I/O in async tasks.
- The camera pipeline is the failure point, but the server must survive it:
  if the camera process (`rpicam-vid` or `rpicam-still`) dies unexpectedly,
  the process stays up and the failure is reported by `GET /camera/status`
  (restart via the API). Never kill the process from the capture path — the
  HTTP API is the recovery mechanism. A `--zsl` warning from `rpicam-still`
  is informational (temporal denoise is skipped in timelapse mode).

## Testing

There is no camera attached in CI-like environments; `cargo check` and
`cargo build` are the automated gates. Anything beyond that requires real
hardware (`rpicam-hello --list-cameras` must report a camera) plus a browser.
Manual smoke test: run the server, open `http://<pi>:8080/`, confirm video
starts within ~2 s (one keyframe interval).
