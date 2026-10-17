//! Pi Control: GPUI companion app for the webrtc-pi camera server.
//!
//! Talks to the server's HTTP API (`/camera/*`, `/images*`, `/preview.jpg`);
//! polls status + gallery every 2 s and the low-rate JPEG preview whenever
//! the camera is running. Live H.264 video stays in the browser — the app
//! is for control and monitoring.

mod pi;

use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use gpui::{
    div, img, prelude::*, px, rgb, size, App, Application, Bounds, Context, CursorStyle,
    Decorations, Image, ImageFormat, MouseButton, MouseDownEvent, ObjectFit, Render, ResizeEdge,
    SharedString, Tiling, Window, WindowBounds, WindowDecorations, WindowOptions,
};

// Palette (dark).
const BG: u32 = 0x14161b;
const PANEL: u32 = 0x1d2027;
const BORDER: u32 = 0x33384a;
const TEXT: u32 = 0xd7dae0;
const TEXT_DIM: u32 = 0x8b91a0;
const ACCENT: u32 = 0x3b82f6;
const OK: u32 = 0x34d399;
const WARN: u32 = 0xfbbf24;
const ERROR: u32 = 0xf87171;

#[derive(Parser)]
#[command(name = "pi-control", about = "Companion controller for the webrtc-pi camera server")]
struct Args {
    /// Base URL of the webrtc-pi server.
    #[arg(long, env = "PI_SERVER", default_value = "http://127.0.0.1:8080")]
    server: String,
}

/// Capture-rate presets for the stepper.
const FPS_STEPS: [u32; 7] = [1, 2, 5, 10, 15, 30, 60];
/// Exposure presets in microseconds; 0 = auto.
const SHUTTER_STEPS: [u32; 9] = [0, 50, 100, 250, 500, 1_000, 2_000, 5_000, 10_000];
/// Analog gain presets; 0 = auto.
const GAIN_STEPS: [f32; 6] = [0.0, 1.0, 2.0, 4.0, 8.0, 16.0];

/// One capture run with the images currently known from the server.
struct RunView {
    name: String,
    images: Vec<pi::ImageInfo>,
}

struct CameraApp {
    client: Arc<pi::Client>,
    status: Option<pi::CameraStatus>,
    runs: Vec<RunView>,
    command_error: Option<SharedString>,
    busy: bool,
    preview: Option<Arc<Image>>,
    fps_idx: usize,
    shutter_idx: usize,
    gain_idx: usize,
}

impl CameraApp {
    fn new(client: Arc<pi::Client>, cx: &mut Context<Self>) -> Self {
        let app = Self {
            client: Arc::clone(&client),
            status: None,
            runs: Vec::new(),
            command_error: None,
            busy: false,
            preview: None,
            fps_idx: FPS_STEPS.len() - 2,
            shutter_idx: 0,
            gain_idx: 0,
        };
        app.spawn_status_loop(cx);
        app.spawn_preview_loop(cx);
        app
    }

    /// Poll status + gallery every 2 s.
    fn spawn_status_loop(&self, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            loop {
                let client = Arc::clone(&client);
                let poll = cx
                    .background_executor()
                    .spawn(async move { client.poll() })
                    .await;
                if this
                    .update(cx, |app, cx| {
                        app.apply_poll(poll);
                        cx.notify();
                    })
                    .is_err()
                {
                    break; // entity dropped; app is quitting
                }
                cx.background_executor().timer(Duration::from_secs(2)).await;
            }
        })
        .detach();
    }

    /// Poll the preview JPEG. Fast while streaming, slow in capture mode
    /// (where the server serves the newest full-res still), not at all when
    /// the camera is stopped.
    fn spawn_preview_loop(&self, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            loop {
                let client = Arc::clone(&client);
                let (jpeg, mode) = cx
                    .background_executor()
                    .spawn(async move {
                        let mode = client.status().ok().map(|s| s.mode);
                        if mode.as_deref() == Some("stopped") {
                            (None, mode)
                        } else {
                            (client.preview().ok(), mode)
                        }
                    })
                    .await;
                if this
                    .update(cx, |app, cx| {
                        app.preview = jpeg.map(|bytes| Arc::new(Image::from_bytes(ImageFormat::Jpeg, bytes)));
                        cx.notify();
                    })
                    .is_err()
                {
                    break;
                }
                let interval = if mode.as_deref() == Some("streaming") {
                    Duration::from_millis(500)
                } else {
                    Duration::from_secs(2)
                };
                cx.background_executor().timer(interval).await;
            }
        })
        .detach();
    }

    fn apply_poll(&mut self, poll: pi::Poll) {
        match poll.status {
            Ok(status) => {
                self.status = Some(status);
                if !self.busy {
                    self.command_error = None;
                }
            }
            Err(e) => {
                // Only surface connection problems when we have nothing to show.
                if self.status.is_none() {
                    self.command_error = Some(e.into());
                }
            }
        }
        if let Ok(images) = poll.images {
            self.rebuild_runs(images);
        }
    }

    /// Group the newest-first image list into runs.
    fn rebuild_runs(&mut self, images: Vec<pi::ImageInfo>) {
        let mut runs: Vec<RunView> = Vec::new();
        for image in images {
            let run = image.path.split('/').next().unwrap_or("run").to_owned();
            match runs.last_mut() {
                Some(last) if last.name == run => last.images.push(image),
                _ => runs.push(RunView { name: run, images: vec![image] }),
            }
        }
        self.runs = runs;
    }

    fn mode(&self) -> &str {
        self.status.as_ref().map_or("connecting", |s| s.mode.as_str())
    }

    fn run_command(&mut self, command: pi::Command, cx: &mut Context<Self>) {
        if self.busy {
            return;
        }
        self.busy = true;
        self.command_error = None;
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.command(command) })
                .await;
            this.update(cx, |app, cx| {
                app.busy = false;
                if let Err(e) = result {
                    app.command_error = Some(e.into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    fn delete_path(&mut self, path: String, cx: &mut Context<Self>) {
        let client = Arc::clone(&self.client);
        cx.spawn(async move |this, cx| {
            let result = cx
                .background_executor()
                .spawn(async move { client.delete(&path) })
                .await;
            this.update(cx, |app, cx| {
                if let Err(e) = result {
                    app.command_error = Some(e.into());
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    // ---- rendering ------------------------------------------------------

    fn button(
        &self,
        id: &'static str,
        label: &str,
        enabled: bool,
        danger: bool,
        cx: &mut Context<Self>,
        on_click: impl Fn(&mut Self, &mut Context<Self>) + 'static,
    ) -> gpui::Stateful<gpui::Div> {
        let (fg, bg) = if !enabled {
            (rgb(TEXT_DIM), rgb(PANEL))
        } else if danger {
            (rgb(0x14161b), rgb(ERROR))
        } else {
            (rgb(0xffffff), rgb(ACCENT))
        };
        div()
            .id(id)
            .px_3()
            .py_1p5()
            .rounded_md()
            .text_sm()
            .text_color(fg)
            .bg(bg)
            .when(enabled, |el| el.cursor_pointer().hover(|s| s.opacity(0.85)))
            .on_click(cx.listener(move |app, _, _, cx| {
                on_click(app, cx);
            }))
            .child(SharedString::from(label.to_owned()))
    }

    /// A labeled `- value +` stepper. `on_change` is a plain `fn` pointer so
    /// it can be wired into both arrow buttons without cloning.
    fn stepper(
        &self,
        id: &'static str,
        label: &str,
        value: &str,
        can_decrease: bool,
        can_increase: bool,
        cx: &mut Context<Self>,
        on_change: fn(&mut Self, i32),
    ) -> gpui::Stateful<gpui::Div> {
        let arrow = |glyph: &'static str, enabled: bool| {
            let color = if enabled { rgb(TEXT) } else { rgb(BORDER) };
            div()
                .id(SharedString::from(format!("{id}-{glyph}")))
                .px_2()
                .cursor_pointer()
                .text_color(color)
                .child(glyph)
        };
        div()
            .id(id)
            .flex()
            .items_center()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(SharedString::from(label.to_owned())),
            )
            .child(arrow("−", can_decrease).on_click(cx.listener(move |app, _, _, cx| {
                on_change(app, -1);
                cx.notify();
            })))
            .child(
                div()
                    .min_w_12()
                    .text_center()
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .child(SharedString::from(value.to_owned())),
            )
            .child(arrow("+", can_increase).on_click(cx.listener(move |app, _, _, cx| {
                on_change(app, 1);
                cx.notify();
            })))
    }

    fn render_header(&mut self) -> gpui::AnyElement {
        let (dot, mode_text) = match self.mode() {
            "streaming" => (rgb(OK), "streaming"),
            "capturing" => (rgb(WARN), "capturing"),
            "stopped" => (rgb(TEXT_DIM), "stopped"),
            _ => (rgb(TEXT_DIM), "connecting…"),
        };
        let status = self.status.clone();
        let error = self
            .status
            .as_ref()
            .and_then(|s| s.error.clone().map(SharedString::from))
            .or_else(|| self.command_error.clone());

        div()
            .flex()
            .items_center()
            .gap_3()
            .child(
                div()
                    .size_2p5()
                    .rounded_full()
                    .bg(dot),
            )
            .child(
                div()
                    .text_lg()
                    .text_color(rgb(TEXT))
                    .child("Pi Camera"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .child(SharedString::from(self.client.base().to_owned())),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(rgb(TEXT))
                    .child(SharedString::from(mode_text.to_owned())),
            )
            .child(match &status {
                Some(s) if s.mode == "capturing" => div()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .child(SharedString::from(format!(
                        "run {} · {} images{}{}",
                        s.run.as_deref().unwrap_or("?"),
                        s.images.unwrap_or(0),
                        s.shutter_us.map_or_else(String::new, |us| format!(" · {us} µs")),
                        s.gain.map_or_else(String::new, |g| format!(" · gain {g}")),
                    )))
                    .into_any_element(),
                _ => div().into_any_element(),
            })
            .when_some(error, |el, e| {
                el.child(
                    div()
                        .flex_1()
                        .text_right()
                        .text_sm()
                        .text_color(rgb(ERROR))
                        .child(e),
                )
            })
            .into_any_element()
    }

    fn render_controls(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let mode = self.mode().to_owned();
        let busy = self.busy;
        let streaming = mode == "streaming";

        let start = self
            .button("start", "Start streaming", streaming || busy, false, cx, |app, cx| {
                app.run_command(pi::Command::Start, cx);
            })
            .into_any_element();
        let capture = self
            .button("capture", "Start capture", busy, false, cx, |app, cx| {
                let shutter = SHUTTER_STEPS[app.shutter_idx];
                let gain = GAIN_STEPS[app.gain_idx];
                app.run_command(
                    pi::Command::Capture {
                        fps: FPS_STEPS[app.fps_idx],
                        shutter_us: shutter,
                        gain,
                    },
                    cx,
                );
            })
            .into_any_element();
        let stop = self
            .button("stop", "Stop", mode == "stopped" || busy, true, cx, |app, cx| {
                app.run_command(pi::Command::Stop, cx);
            })
            .into_any_element();

        let fps = FPS_STEPS[self.fps_idx];
        let shutter = SHUTTER_STEPS[self.shutter_idx];
        let gain = GAIN_STEPS[self.gain_idx];
        let shutter_label = if shutter == 0 { "auto".to_owned() } else { format!("{shutter} µs") };
        let gain_label = if gain == 0.0 { "auto".to_owned() } else { format!("{gain:.0}") };

        div()
            .flex()
            .items_center()
            .flex_wrap()
            .gap_3()
            .p_3()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .child(start)
            .child(capture)
            .child(stop)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_4()
                    .ml_auto()
                    .child(self.stepper(
                        "fps",
                        "fps",
                        &fps.to_string(),
                        self.fps_idx > 0,
                        self.fps_idx + 1 < FPS_STEPS.len(),
                        cx,
                        |app, delta| {
                            app.fps_idx = (app.fps_idx as i32 + delta).clamp(0, FPS_STEPS.len() as i32 - 1) as usize;
                        },
                    ))
                    .child(self.stepper(
                        "shutter",
                        "shutter",
                        &shutter_label,
                        self.shutter_idx > 0,
                        self.shutter_idx + 1 < SHUTTER_STEPS.len(),
                        cx,
                        |app, delta| {
                            app.shutter_idx = (app.shutter_idx as i32 + delta).clamp(0, SHUTTER_STEPS.len() as i32 - 1) as usize;
                        },
                    ))
                    .child(self.stepper(
                        "gain",
                        "gain",
                        &gain_label,
                        self.gain_idx > 0,
                        self.gain_idx + 1 < GAIN_STEPS.len(),
                        cx,
                        |app, delta| {
                            app.gain_idx = (app.gain_idx as i32 + delta).clamp(0, GAIN_STEPS.len() as i32 - 1) as usize;
                        },
                    )),
            )
            .into_any_element()
    }

    fn render_preview(&mut self) -> gpui::AnyElement {
        let mode = self.mode().to_owned();
        let placeholder = move || {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .text_color(rgb(TEXT_DIM))
                .child(match mode.as_str() {
                    "streaming" | "capturing" => "waiting for first frame…",
                    _ => "camera stopped",
                })
        };
        let preview = self.preview.clone();
        div()
            .flex_1()
            .min_h_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgb(0x0b0c10))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .overflow_hidden()
            .child(match preview {
                Some(image) => img(image)
                    .id("preview")
                    .size_full()
                    .object_fit(ObjectFit::Contain)
                    .into_any_element(),
                None => placeholder().into_any_element(),
            })
            .into_any_element()
    }

    fn render_gallery(&mut self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let total: usize = self.runs.iter().map(|r| r.images.len()).sum();
        let mut list = div().flex().flex_col().gap_2();

        for run in &self.runs {
            let count = run.images.len();
            list = list
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .mt_1()
                        .child(
                            div()
                                .text_sm()
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(run.name.clone())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_DIM))
                                .child(SharedString::from(format!("{count} images"))),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("dl-{}", run.name)))
                                .text_xs()
                                .text_color(rgb(ACCENT))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_click(cx.listener({
                                    let url = self
                                        .client
                                        .url(&format!("/images/archive.tar?path={}", run.name));
                                    move |_, _, _, cx| cx.open_url(&url)
                                }))
                                .child("download .tar"),
                        )
                        .child(
                            div()
                                .id(SharedString::from(format!("rm-{}", run.name)))
                                .text_xs()
                                .text_color(rgb(ERROR))
                                .cursor_pointer()
                                .hover(|s| s.underline())
                                .on_click(cx.listener({
                                    let run_name = run.name.clone();
                                    move |app, _, _, cx| {
                                        app.runs.retain(|r| r.name != run_name);
                                        app.delete_path(run_name.clone(), cx);
                                        cx.notify();
                                    }
                                }))
                                .child("delete"),
                        ),
                );
            for image in &run.images {
                let name = image.path.rsplit('/').next().unwrap_or(&image.path);
                let url = self.client.url(&format!("/images/file?path={}", image.path));
                list = list.child(
                    div()
                        .id(SharedString::from(format!("img-{}", image.path)))
                        .flex()
                        .items_center()
                        .gap_2()
                        .pl_4()
                        .py_0p5()
                        .rounded_sm()
                        .hover(|s| s.bg(rgb(PANEL)))
                        .cursor_pointer()
                        .on_click(cx.listener({
                            let url = url.clone();
                            move |_, _, _, cx| cx.open_url(&url)
                        }))
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT))
                                .child(SharedString::from(name.to_owned())),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_DIM))
                                .child(SharedString::from(format_human_size(image.size))),
                        )
                        .child(
                            div()
                                .text_xs()
                                .text_color(rgb(TEXT_DIM))
                                .child(SharedString::from(format_utc(image.modified))),
                        ),
                );
            }
        }

        div()
            .h_64()
            .flex_none()
            .flex()
            .flex_col()
            .p_3()
            .bg(rgb(PANEL))
            .border_1()
            .border_color(rgb(BORDER))
            .rounded_lg()
            .gap_1()
            .child(
                div()
                    .text_xs()
                    .text_color(rgb(TEXT_DIM))
                    .child(SharedString::from(format!(
                        "captures — {total} images shown (click to open in browser)"
                    ))),
            )
            .child(
                div()
                    .id("gallery")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(list),
            )
            .into_any_element()
    }
}

impl Render for CameraApp {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let content = div()
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .p_3()
            .gap_3()
            .child(self.render_header())
            .child(self.render_controls(cx))
            .child(self.render_preview())
            .child(self.render_gallery(cx));

        let root = div()
            .relative()
            .flex()
            .flex_col()
            .size_full()
            .bg(rgb(BG))
            .text_color(rgb(TEXT));

        match window.window_decorations() {
            // The window manager draws the titlebar; nothing to do.
            Decorations::Server => root.child(content),
            // No server-side decorations available (Wayland compositors
            // without SSD, X11 with a compositor): draw our own titlebar
            // and resize edges.
            Decorations::Client { tiling } => {
                let controls = window.window_controls();
                root.child(self.render_titlebar(controls.minimize, controls.maximize, cx))
                    .child(content)
                    .child(self.render_resize_edges(tiling, cx))
            }
        }
    }
}

impl CameraApp {
    /// A small client-side titlebar: drag region + window controls. Only
    /// rendered when the platform reports client-side decorations.
    fn render_titlebar(
        &mut self,
        can_minimize: bool,
        can_maximize: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let control = |id: &'static str, glyph: &'static str| {
            div()
                .id(id)
                .size_7()
                .flex()
                .items_center()
                .justify_center()
                .rounded_sm()
                .text_color(rgb(TEXT_DIM))
                .child(glyph)
        };
        div()
            .id("titlebar")
            .flex()
            .items_center()
            .h_9()
            .flex_none()
            .bg(rgb(PANEL))
            .on_mouse_down(MouseButton::Left, cx.listener(
                |_, event: &MouseDownEvent, window, _| {
                    // Double-click toggles maximize; single press hands the
                    // window move to the compositor.
                    if event.click_count >= 2 {
                        window.zoom_window();
                    } else {
                        window.start_window_move();
                    }
                },
            ))
            .child(
                div()
                    .pl_3()
                    .text_sm()
                    .text_color(rgb(TEXT_DIM))
                    .child("Pi Camera Control"),
            )
            .child(div().flex_1())
            .when(can_minimize, |bar| {
                bar.child(
                    control("tb-min", "–")
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_click(|_, window, _| window.minimize_window()),
                )
            })
            .when(can_maximize, |bar| {
                bar.child(
                    control("tb-max", "□")
                        .cursor_pointer()
                        .hover(|s| s.bg(rgb(BORDER)).text_color(rgb(TEXT)))
                        .on_mouse_down(MouseButton::Left, |_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_click(|_, window, _| window.zoom_window()),
                )
            })
            .child(
                control("tb-close", "×")
                    .cursor_pointer()
                    .hover(|s| s.bg(rgb(ERROR)).text_color(rgb(0xffffff)))
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(|_, window, _| window.remove_window()),
            )
            .into_any_element()
    }

    /// Invisible strips along the window edges that start compositor-driven
    /// resize on press (incl. corners). Painted after the content so they
    /// sit on top; skipped on tiled (snapped) edges.
    fn render_resize_edges(&mut self, tiling: Tiling, cx: &mut Context<Self>) -> gpui::AnyElement {
        let strip = |id: &'static str,
                     tiling_ok: bool,
                     cursor: CursorStyle,
                     edge: ResizeEdge,
                     style: fn(gpui::Stateful<gpui::Div>) -> gpui::Stateful<gpui::Div>| {
            let base = div()
                .id(id)
                .absolute()
                .block_mouse_except_scroll()
                .cursor(cursor)
                .on_mouse_down(MouseButton::Left, cx.listener(move |_, _, window, _| {
                    window.start_window_resize(edge);
                }));
            if tiling_ok {
                style(base).into_any_element()
            } else {
                div().into_any_element()
            }
        };
        div()
            .child(strip("rz-top", !tiling.top, CursorStyle::ResizeUpDown, ResizeEdge::Top, |d| {
                d.top_0().left_0().right_0().h_1p5()
            }))
            .child(strip("rz-bottom", !tiling.bottom, CursorStyle::ResizeUpDown, ResizeEdge::Bottom, |d| {
                d.bottom_0().left_0().right_0().h_1p5()
            }))
            .child(strip("rz-left", !tiling.left, CursorStyle::ResizeLeftRight, ResizeEdge::Left, |d| {
                d.top_0().bottom_0().left_0().w_1p5()
            }))
            .child(strip("rz-right", !tiling.right, CursorStyle::ResizeLeftRight, ResizeEdge::Right, |d| {
                d.top_0().bottom_0().right_0().w_1p5()
            }))
            .child(strip("rz-tl", !(tiling.top || tiling.left), CursorStyle::ResizeUpLeftDownRight, ResizeEdge::TopLeft, |d| {
                d.top_0().left_0().size_3()
            }))
            .child(strip("rz-tr", !(tiling.top || tiling.right), CursorStyle::ResizeUpRightDownLeft, ResizeEdge::TopRight, |d| {
                d.top_0().right_0().size_3()
            }))
            .child(strip("rz-bl", !(tiling.bottom || tiling.left), CursorStyle::ResizeUpRightDownLeft, ResizeEdge::BottomLeft, |d| {
                d.bottom_0().left_0().size_3()
            }))
            .child(strip("rz-br", !(tiling.bottom || tiling.right), CursorStyle::ResizeUpLeftDownRight, ResizeEdge::BottomRight, |d| {
                d.bottom_0().right_0().size_3()
            }))
            .into_any_element()
    }
}

/// `1234567` → `1.2 MB`.
fn format_human_size(bytes: u64) -> String {
    let (value, unit) = match bytes {
        0..=1023 => (bytes as f64, "B"),
        1_024..=1_048_575 => (bytes as f64 / 1_024.0, "KB"),
        _ => (bytes as f64 / 1_048_576.0, "MB"),
    };
    format!("{value:.1} {unit}")
}

/// Unix seconds → `YYYY-MM-DD HH:MM:SS` (UTC), no chrono dependency.
fn format_utc(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, rem % 3600 / 60, rem % 60);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mth = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mth <= 2 { y + 1 } else { y };
    format!("{y:04}-{mth:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

fn main() {
    let args = Args::parse();

    Application::new().run(move |cx: &mut App| {
        let client = Arc::new(pi::Client::new(&args.server));
        let bounds = Bounds::centered(None, size(px(960.), px(720.)), cx);
        cx.open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(gpui::TitlebarOptions {
                    title: Some("Pi Camera Control".into()),
                    ..Default::default()
                }),
                // Linux (Wayland/X11): draw our own titlebar; falls back to
                // server decorations automatically where unavailable.
                window_decorations: Some(WindowDecorations::Client),
                window_min_size: Some(size(px(720.), px(540.))),
                ..Default::default()
            },
            |_, cx| cx.new(|cx| CameraApp::new(client, cx)),
        )
        .unwrap();
        cx.activate(true);
    });
}
