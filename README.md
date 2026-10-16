# webrtc-pi

WebRTC camera streaming server for the Raspberry Pi 5, written in Rust.

Point any browser at the Pi and watch the camera feed with sub-second latency.

```
rpicam-vid (libcamera) ──raw I420──▶ x264 (software) ──H.264──▶ broadcast ──▶ WebRTC peer (one per viewer)
                                                                                        ▲
              browser ◀── WebSocket signaling (SDP offer/answer) ──────────────────────┘
```

## Why software encoding?

The Pi 5 has **no hardware video encoder** — its ISP only does HEVC *decode*.
H.264 is therefore produced by x264 on the CPU. The four Cortex-A76 cores
comfortably handle 720p30; 1080p30 works too but runs hot. The `veryfast`
preset + `zerolatency` tune keep encoding latency to a frame or two.

## Stack

| Component | Choice |
| --- | --- |
| Capture | `rpicam-vid --codec yuv420`, raw I420 frames piped to the server |
| Encode | [`x264`](https://crates.io/crates/x264) (baseline profile, 2s keyframe interval) |
| WebRTC | [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) 0.21 |
| Signaling | Embedded page + WebSocket (`/ws`), non-trickle SDP exchange |
| HTTP | `axum` |

## Setup (on the Pi)

```sh
# 1. System packages: C toolchain, x264 headers, camera apps, clang (for bindgen)
sudo apt update
sudo apt install -y build-essential pkg-config libx264-dev rpicam-apps libclang-dev

# Make sure your user can access the camera
sudo usermod -aG video,gpio $USER   # log out/in afterwards

# 2. Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# 3. Build
cargo build --release
```

## Run

```sh
cargo run --release
# then open http://<pi-ip>:8080/ from any device on the network
```

The server starts with the camera **stopped**; use `--auto-start` to capture
from boot, or `POST /camera/start` (the viewer page has buttons for this).

Options:

```sh
cargo run --release -- --help
```

- `--width/--height` — capture size (default 1280x720)
- `--fps` — capture/encode framerate (default 30)
- `--bitrate` — H.264 target bitrate in kbps (default 2500)
- `--port` — HTTP port (default 8080)
- `--auto-start` — start the camera pipeline at boot

### Camera control API

| Route | Method | Effect |
| --- | --- | --- |
| `/camera/start` | POST | Start capture + encode. Idempotent. `500` with an `error` field if the camera fails to come up. |
| `/camera/stop` | POST | Stop capture + encode. Idempotent. |
| `/camera/status` | GET | `{"running": true}` or `{"running": false, "error": "..."}` |

The broadcast channel survives restarts: viewers connected while the camera is
stopped simply wait (and start decoding on the next keyframe, within ~2 s of a
start). If `rpicam-vid` dies unexpectedly the server stays up and reports the
failure via `/camera/status`.

Set `RUST_LOG=debug` for verbose logging. Works over the LAN with host
candidates only; the Google STUN server is included for wider routability.

## Limitations / roadmap

- Video only — no audio track yet.
- Keyframe requests (PLI) from browsers are not acted upon; the encoder emits
  a keyframe every 2 seconds instead. A custom interceptor could forward PLIs
  to the encoder (`rtcp-processing` example in webrtc-rs shows how).
- The camera can now be stopped when idle (`POST /camera/stop`), but it does
  not pause by itself with zero viewers; an auto-idle policy could build on
  that.
- One media track per viewer; a switching/SFU design would share encoded
  packets across peer connections.
