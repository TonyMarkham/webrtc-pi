# AGENTS.md

Guidance for AI coding agents working in this repository.

## What this is

A WebRTC video streaming server for the Raspberry Pi 5, written in Rust.
`rpicam-vid` captures raw I420 frames, x264 encodes them in software (the Pi 5
has **no hardware video encoder** — only HEVC decode), and webrtc-rs streams
H.264 to browsers. Signaling is a WebSocket at `/ws`; an embedded viewer page
is served at `/`. A cross-platform GPUI companion app (`app/`) runs on a
workstation and controls the server over its HTTP API.

## Build & run

```sh
cargo check                 # typecheck (fast iteration)
cargo build --release       # ~3.5 min on the Pi
cargo run --release         # serves http://0.0.0.0:8080
```

The repo is a cargo workspace whose default build is the server only. The
GPUI app (`app/`, package `pi-control`) is a member but excluded from
`default-members`, so the Pi's normal build never touches it — build it
explicitly on a workstation with `cargo check -p pi-control` /
`cargo run -p pi-control`. The shared `Cargo.lock` covers both.

System dependencies (already installed on this machine, but needed on a
fresh Pi 5): `build-essential pkg-config libx264-dev rpicam-apps
libclang-dev cmake`. `libclang` is required because `x264-sys` runs bindgen
at build time; `cmake` because capture mode builds its bundled
libjpeg-turbo (see `.cargo/config.toml`, `TURBOJPEG_SOURCE=vendor` — the
system libturbojpeg is too old for the 3.x API).

`RUST_LOG=debug` enables verbose logging.

## Architecture

```
camera.rs    CameraManager: mode machine (stopped/streaming/capturing) that
             owns the camera process handles; streaming = rpicam-vid+x264,
             capture = rpicam-still --timelapse (full-res stills, still
             pipeline quality); also image listing/download/tar/delete
capture.rs   spawns rpicam-vid --codec yuv420, reads exact-size I420 frames
             from stdout on a dedicated thread; frames carry the libcamera
             row stride (width rounded up to 128 bytes)
preview.rs   preview tap: box-average downscale of an I420 frame to ~640px
             + TurboJPEG, published to a watch channel (latest only)
encode.rs    x264 encoder thread: I420 -> H.264 Annex-B, publishes frames on
             a tokio broadcast channel; prepends SPS/PPS to every keyframe;
             also throttles preview.rs to ~2 fps
session.rs   one webrtc-rs PeerConnection per viewer: negotiation, track
             with its own SSRC, streams broadcast frames to the track
web.rs       axum: GET / (embedded page + gallery), GET /ws (signaling),
             /camera/{start,capture,stop,status}, /images[...], /preview.jpg
state.rs     shared AppState (broadcast sender + preview sender + CameraManager)
```

Data flow is single-producer/multi-consumer: capture → encoder thread →
`broadcast::Sender<Arc<EncodedFrame>>` → one subscriber task per viewer.
The broadcast channel is created at boot and survives pipeline restarts, so
viewers may be connected while the camera is stopped or capturing (they wait
for frames). The camera is exclusive: entering capture mode kills the
streaming pipeline and vice versa. Capture mode has no raw-frame data flow —
`rpicam-still` writes JPEGs to disk itself, and `/preview.jpg` serves the
newest still of the current run in that mode.

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
  then U then V, with **padded row strides** — the stride is the width
  rounded up to a multiple of 128 bytes (1280 -> 1280, 1456 -> 1536; see
  `capture::Frame::stride_for`). Frames are read with `read_exact`; there
  is no framing to resync on.
- **Capture mode is `rpicam-still --timelapse`**, not rpicam-vid: stills
  must go through the still pipeline because the video pipeline leaves
  artifacts on moving parts. Consequences worth knowing: `--timeout 0` runs
  until the manager kills the process; the `%d` frame counter in the output
  name is **not zero-padded**, so file-name ordering is not numeric and
  `latest_still` goes by mtime; `fps` is a request, not a guarantee.
- **Keyframes**: browsers can only start decoding on an IDR. The encoder
  forces one every 2 s (`max_keyframe_interval = 2*fps`, `scenecut 0`), and
  viewer tasks wait for a keyframe before sending. PLI/FIR feedback is NOT
  wired to the encoder (see README roadmap) — don't assume it is.
- **Non-trickle ICE**: the browser gathers all candidates before sending its
  offer; the server waits for `on_ice_gathering_state_change == Complete`
  before answering. Keep it that way or implement trickle both sides.

## The GPUI companion app (`app/`, package `pi-control`)

- **gpui crate is 0.2** and still pre-1.0 — breaking changes between
  versions are common. Do not rely on LLM training knowledge of GPUI;
  model code on the `examples/` in the gpui crate source (hello_world,
  image_loading, gif_viewer) or the gpui.rs docs for the exact version in
  `Cargo.lock`.
- The app **cannot be compiled on the Pi** (no desktop GPU stack; the
  required system libraries aren't installed). `cargo check -p pi-control`
  must be run on a workstation. Server-side changes to the HTTP API must
  therefore be validated by careful reading plus `cargo check` of the
  server, and the app updated to match in the same change.
- The app polls the HTTP API on GPUI's background executor (`ureq`, sync)
  and applies results on the main thread via `WeakEntity::update`. Don't
  block the main thread with HTTP calls.
- **`libc` is pinned to 0.2.189** in `Cargo.lock`: 0.2.190 removed
  `libc::ENOATTR` on Linux, which breaks `xattr 0.2.3` (pulled in by
  gpui's `zed-async-tar` dependency). Don't bump libc past 0.2.189 until
  that's fixed upstream.

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
