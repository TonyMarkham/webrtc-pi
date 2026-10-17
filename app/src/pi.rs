//! HTTP client for the webrtc-pi server.
//!
//! Synchronous `ureq` calls, meant to be run on GPUI's background executor;
//! the results are applied to the UI on the main thread.

use std::io::Read;
use std::time::Duration;

use serde::Deserialize;
use ureq::{Agent, AgentBuilder};

/// `GET /camera/status`.
#[derive(Clone, Debug, Deserialize)]
pub struct CameraStatus {
    pub mode: String,
    #[serde(default)]
    pub run: Option<String>,
    #[serde(default)]
    pub images: Option<usize>,
    /// Capture rate while capturing; not shown in the UI yet.
    #[allow(dead_code)]
    #[serde(default)]
    pub fps: Option<u32>,
    #[serde(default)]
    pub shutter_us: Option<u32>,
    #[serde(default)]
    pub gain: Option<f32>,
    #[serde(default)]
    pub error: Option<String>,
}

/// One entry of `GET /images`.
#[derive(Clone, Debug, Deserialize)]
pub struct ImageInfo {
    /// Path relative to the capture directory, e.g. `run-173…/img_000042.jpg`.
    pub path: String,
    pub size: u64,
    /// File modification time, unix seconds.
    pub modified: u64,
}

/// Camera mode commands.
#[derive(Clone, Copy, Debug)]
pub enum Command {
    Start,
    Stop,
    Capture {
        fps: u32,
        shutter_us: u32,
        gain: f32,
    },
}

/// Result of one combined status + images poll.
pub struct Poll {
    pub status: Result<CameraStatus, String>,
    pub images: Result<Vec<ImageInfo>, String>,
}

#[derive(Clone)]
pub struct Client {
    agent: Agent,
    base: String,
}

impl Client {
    pub fn new(base: &str) -> Self {
        Self {
            agent: AgentBuilder::new()
                .timeout_connect(Duration::from_secs(3))
                .timeout(Duration::from_secs(30))
                .build(),
            base: base.trim_end_matches('/').to_owned(),
        }
    }

    pub fn base(&self) -> &str {
        &self.base
    }

    /// Absolute URL for a server path (for opening in a browser).
    pub fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    fn request(&self, method: &str, path: &str) -> ureq::Request {
        let url = self.url(path);
        match method {
            "POST" => self.agent.post(&url),
            "DELETE" => self.agent.delete(&url),
            _ => self.agent.get(&url),
        }
    }

    /// One status + images poll in a single request burst.
    pub fn poll(&self) -> Poll {
        Poll {
            status: self.status(),
            images: self.images(500),
        }
    }

    pub fn status(&self) -> Result<CameraStatus, String> {
        self.request("GET", "/camera/status")
            .call()
            .and_then(|r| r.into_json().map_err(Into::into))
            .map_err(describe)
    }

    pub fn images(&self, limit: usize) -> Result<Vec<ImageInfo>, String> {
        self.request("GET", "/images")
            .query("limit", &limit.to_string())
            .call()
            .and_then(|r| r.into_json().map_err(Into::into))
            .map_err(describe)
    }

    pub fn command(&self, command: Command) -> Result<(), String> {
        let result = match command {
            Command::Start => self.request("POST", "/camera/start").call(),
            Command::Stop => self.request("POST", "/camera/stop").call(),
            Command::Capture { fps, shutter_us, gain } => {
                let mut req = self.request("POST", "/camera/capture").query("fps", &fps.to_string());
                if shutter_us > 0 {
                    req = req.query("shutter", &shutter_us.to_string());
                }
                if gain > 0.0 {
                    req = req.query("gain", &gain.to_string());
                }
                req.call()
            }
        };
        result.map(|_| ()).map_err(describe)
    }

    pub fn delete(&self, path: &str) -> Result<(), String> {
        self.request("DELETE", "/images")
            .query("path", path)
            .call()
            .map(|_| ())
            .map_err(describe)
    }

    /// Fetch the latest preview JPEG. Errors (including the 503 the server
    /// returns when nothing is running) are returned as strings.
    pub fn preview(&self) -> Result<Vec<u8>, String> {
        let mut reader = self
            .request("GET", "/preview.jpg")
            .call()
            .map_err(describe)?
            .into_reader();
        let mut buf = Vec::new();
        reader
            .read_to_end(&mut buf)
            .map_err(|e| format!("preview read failed: {e}"))?;
        Ok(buf)
    }
}

/// Turn a `ureq` error into a user-facing message; status errors include the
/// server's JSON error body.
fn describe(e: ureq::Error) -> String {
    match e {
        ureq::Error::Status(code, resp) => {
            let body = resp.into_string().unwrap_or_default();
            let message = serde_json::from_str::<serde_json::Value>(&body)
                .ok()
                .and_then(|v| v.get("error").and_then(|e| e.as_str()).map(String::from))
                .unwrap_or(body);
            if message.is_empty() {
                format!("HTTP {code}")
            } else {
                format!("HTTP {code}: {message}")
            }
        }
        ureq::Error::Transport(t) => format!("connection failed: {t}"),
    }
}
