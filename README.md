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
| Capture | `rpicam-vid --codec yuv420` raw I420 frames piped to the server for streaming; `rpicam-still --timelapse` at native sensor resolution for capture mode (still-pipeline quality, 1:1 pixels) |
| Encode | [`x264`](https://crates.io/crates/x264) (baseline profile, 2s keyframe interval); [`turbojpeg`](https://crates.io/crates/turbojpeg) for the low-rate preview |
| WebRTC | [`webrtc-rs`](https://github.com/webrtc-rs/webrtc) 0.21 |
| Signaling | Embedded page + WebSocket (`/ws`), non-trickle SDP exchange |
| HTTP | `axum` |
| Companion app | [`gpui`](https://gpui.rs) desktop client (`app/`, cross-platform; polls the HTTP API and a low-rate JPEG preview) |

## Setup (on the Pi)

```sh
# 1. System packages: C toolchain, x264 headers, camera apps, clang (for bindgen)
sudo apt update
sudo apt install -y build-essential pkg-config libx264-dev rpicam-apps libclang-dev
# capture mode builds its bundled libjpeg-turbo from source (see .cargo/config.toml)
sudo apt install -y cmake

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
The camera is a mutually exclusive resource, so the server has three modes:

- **streaming** — live H.264 view in the browser (`rpicam-vid` + x264)
- **capturing** — one full-quality still per interval via `rpicam-still
  --timelapse`; the live view is necessarily off
- **stopped** — idle

Starting one mode stops the other.

Options:

```sh
cargo run --release -- --help
```

- `--width/--height` — capture size for streaming (default 1280x720)
- `--fps` — capture/encode framerate (default 30)
- `--bitrate` — H.264 target bitrate in kbps (default 2500)
- `--port` — HTTP port (default 8080)
- `--auto-start` — start streaming at boot
- `--capture-dir` — where capture runs are written (default `captures`)
- `--sensor-width/--sensor-height` — native sensor frame size (default 1456x1088);
  capture-mode stills are taken at these dimensions, 1:1 sensor pixels
- `--capture-fps` — default capture rate (default 30)
- `--shutter` — fixed exposure in µs for stills (default 0 = auto)
- `--gain` — fixed analog gain for stills (default 0 = auto)
- `--jpeg-quality` — JPEG quality for stills (default 93)

Note: `rpicam-vid` writes YUV frames with the libcamera row stride (width
rounded up to a multiple of 128 bytes); the pipeline handles that internally.
Streaming bitrate is governed by `--bitrate`.

### Why capture uses rpicam-still, not rpicam-vid

Recording frames out of the **video** pipeline leaves visible artifacts on
moving parts (video-grade tuning and noise reduction). Capture mode therefore
runs `rpicam-still --timelapse <interval>`: every still goes through the
proper still-capture pipeline (still tuning, stronger denoise, exposure
settle), at full native sensor resolution — no ISP resampling, 1:1 pixels.
`fps` is the *requested* rate; `rpicam-still` may not sustain high rates and
then simply captures as fast as it can. Stills keep rpicam-still's EXIF
(shutter, gain, …).

### Camera control API

| Route | Method | Effect |
| --- | --- | --- |
| `/camera/start` | POST | Switch to streaming. Idempotent. |
| `/camera/capture?fps=30&shutter=1000&gain=8` | POST | Switch to capture mode: stop the live view and run `rpicam-still --timelapse`, writing one full-quality still per interval into a new run directory (`captures/run-<unix-seconds>/img_N.jpg`). Stills are captured at native sensor resolution (`--sensor-width/--sensor-height`, 1456x1088) — 1:1 sensor pixels, still-pipeline processing. `fps` (1..60) is the requested captures-per-second (converted to the timelapse interval; the real rate is bounded by `rpicam-still`), `shutter` (µs, 0 = auto) and `gain` (0 = auto) are optional and override the CLI defaults. Use a short shutter to freeze motion — the IMX296 is global-shutter, so blur is purely exposure time — and raise gain (or lighting) to compensate. |
| `/camera/stop` | POST | Stop whichever mode is active. Idempotent. |
| `/camera/status` | GET | `{"mode": "streaming"\|"capturing"\|"stopped"}`, plus `run`, `images`, `interval_secs` while capturing and `error` after a failure. |
| `/images?limit=500` | GET | List saved stills, newest first. |
| `/images/file?path=<run/img.jpg>` | GET | Download one image. |
| `/images/archive.tar?path=<run>` | GET | Stream one run as a tar archive. |
| `/images?path=<run-or-file>` | DELETE | Delete a run directory or single image. |
| `/preview.jpg` | GET | Latest low-rate preview JPEG for the companion app: an encoder tap (~2 fps, 640-wide) while streaming, the newest still of the current run while capturing. 503 when the camera is stopped. |

The broadcast channel survives restarts: viewers connected while the camera is
stopped or capturing simply wait (and start decoding on the next keyframe,
within ~2 s of returning to streaming mode). If the camera process dies
unexpectedly the server stays up and reports the failure via `/camera/status`.

Set `RUST_LOG=debug` for verbose logging. Works over the LAN with host
candidates only; the Google STUN server is included for wider routability.

### Companion desktop app (`pi-control`)

The repository also contains a cross-platform [GPUI](https://gpui.rs) desktop
app in [`app/`](app/) that acts as a remote control for the server: camera
mode buttons, capture parameter steppers (fps / shutter / gain), a live
preview pane (fed by `/preview.jpg`, ~2 fps while streaming, newest still
while capturing), and a capture gallery with per-run tar download, per-image
open-in-browser and delete.

The app is a workspace member but **not** part of the default build, since it
needs a desktop GPU stack and is meant to be built on a workstation, not on
the Pi. From a checkout of this repository on macOS/Linux/Windows:

```sh
cargo run -p pi-control -- --server http://<pi-ip>:8080
# or: export PI_SERVER=http://<pi-ip>:8080
```

GPUI's Linux build additionally needs the usual GUI dev packages
(`libwayland-dev`, `libxkbcommon-dev`, `libfontconfig1-dev`, `libasound2-dev`,
`libudev-dev` and friends; on macOS/Windows no extras are required).

Note that the live H.264 stream itself stays in the browser — the app
deliberately does not speak WebRTC; it is for control and monitoring.

### Deriving training-data variants

`derive-variants` generates downscaled/greyscale variants from a capture run,
with the transforms defined once (exact 2x2 box average for the 1/2 scale,
BT.601 luma for greyscale) so training and inference preprocess identically:

```sh
./target/release/derive-variants captures/run-<timestamp>
# writes half/, grey/, half-grey/ inside the run directory
```

| Variant | Size | Content |
| --- | --- | --- |
| `half/` | 728x544 | colour |
| `grey/` | 1456x1088 | greyscale |
| `half-grey/` | 728x544 | greyscale |

## Limitations / roadmap

- Video only — no audio track yet.
- Keyframe requests (PLI) from browsers are not acted upon; the encoder emits
  a keyframe every 2 seconds instead. A custom interceptor could forward PLIs
  to the encoder (`rtcp-processing` example in webrtc-rs shows how).
- Capture mode writes JPEGs until stopped; there is no automatic disk cap and
  no thumbnails (the gallery serves full-size images, fine over a LAN).
- One media track per viewer; a switching/SFU design would share encoded
  packets across peer connections.
