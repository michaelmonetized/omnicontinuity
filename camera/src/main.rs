//! iPhone camera test.
//!
//! Safari on the phone sends JPEG frames over the tailnet. This process
//! repeats the newest frame at 24 fps into ffmpeg, which writes a steady
//! picture to the v4l2loopback device apps already know how to open.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::State;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::Router;
use axum_server::tls_rustls::RustlsConfig;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

const WIDTH: u16 = 1280;
const HEIGHT: u16 = 720;
const FPS: u64 = 24;
const MAX_JPEG: usize = 2 * 1024 * 1024;
const PAGE: &str = include_str!("../web/index.html");

struct Hub {
    device: PathBuf,
    log_path: PathBuf,
    latest: Mutex<Option<Vec<u8>>>,
    frames: AtomicU64,
    accepted: AtomicU64,
}

struct Writer {
    child: Child,
    stdin: ChildStdin,
}

/// Width and height of one complete JPEG.
///
/// Walks marker segments from the start-of-image to the first start-of-frame.
/// Returns nothing unless the buffer begins with SOI and ends with EOI, so a
/// bad socket payload never reaches ffmpeg.
fn jpeg_size(data: &[u8]) -> Option<(u16, u16)> {
    let n = data.len();
    if n < 11 || data[0] != 0xFF || data[1] != 0xD8 || data[n - 2] != 0xFF || data[n - 1] != 0xD9 {
        return None;
    }
    let mut i = 2usize;
    while i + 4 <= n {
        if data[i] != 0xFF {
            return None;
        }
        while i < n && data[i] == 0xFF {
            i += 1;
        }
        if i >= n {
            return None;
        }
        let marker = data[i];
        i += 1;
        if marker == 0xD8 || marker == 0xD9 || marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            continue;
        }
        if i + 2 > n {
            return None;
        }
        let len = u16::from_be_bytes([data[i], data[i + 1]]) as usize;
        if len < 2 || i + len > n {
            return None;
        }
        let sof = (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
        if sof {
            if len < 7 {
                return None;
            }
            let h = u16::from_be_bytes([data[i + 3], data[i + 4]]);
            let w = u16::from_be_bytes([data[i + 5], data[i + 6]]);
            return Some((w, h));
        }
        i += len;
    }
    None
}

fn accept_jpeg(data: &[u8]) -> Option<(u16, u16)> {
    if data.len() > MAX_JPEG {
        return None;
    }
    let (w, h) = jpeg_size(data)?;
    if (16..=4096).contains(&w) && (16..=4096).contains(&h) {
        Some((w, h))
    } else {
        None
    }
}

impl Writer {
    fn spawn(device: &Path, log_path: &Path) -> Result<Self, String> {
        let log = std::fs::File::create(log_path).map_err(|e| format!("ffmpeg log: {e}"))?;
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer+genpts",
                "-flags",
                "low_delay",
                "-probesize",
                "32",
                "-analyzeduration",
                "0",
                "-use_wallclock_as_timestamps",
                "1",
                "-f",
                "mjpeg",
                "-framerate",
                "24",
                "-i",
                "pipe:0",
                "-vf",
                "scale=1280:720:force_original_aspect_ratio=increase,crop=1280:720,format=yuv420p",
                "-pix_fmt",
                "yuv420p",
                "-r",
                "24",
                "-f",
                "v4l2",
            ])
            .arg(device)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(log)
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("ffmpeg: {e}"))?;
        let stdin = child.stdin.take().ok_or("ffmpeg stdin missing")?;
        Ok(Self { child, stdin })
    }

    fn dead(&mut self) -> bool {
        matches!(self.child.try_wait(), Ok(Some(_)))
    }
}

async fn feed(hub: Arc<Hub>) {
    let mut tick = tokio::time::interval(Duration::from_millis(1000 / FPS));
    let mut writer: Option<Writer> = None;
    loop {
        tick.tick().await;
        let jpeg = hub.latest.lock().await.clone();
        let Some(jpeg) = jpeg else {
            continue;
        };
        let needs_writer = match writer.as_mut() {
            Some(current) => current.dead(),
            None => true,
        };
        if needs_writer {
            match Writer::spawn(&hub.device, &hub.log_path) {
                Ok(next) => writer = Some(next),
                Err(err) => {
                    eprintln!("oc-camera: {err}");
                    writer = None;
                    continue;
                }
            }
        }
        if let Some(current) = writer.as_mut() {
            if current.stdin.write_all(&jpeg).await.is_err() {
                writer = None;
            } else {
                hub.frames.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
}

async fn phone(mut socket: WebSocket, hub: Arc<Hub>) {
    eprintln!("oc-camera: phone connected");
    while let Some(message) = socket.recv().await {
        let message = match message {
            Ok(message) => message,
            Err(_) => break,
        };
        let Message::Binary(bytes) = message else {
            continue;
        };
        if accept_jpeg(&bytes).is_none() {
            continue;
        }
        *hub.latest.lock().await = Some(bytes.to_vec());
        hub.accepted.fetch_add(1, Ordering::Relaxed);
    }
    eprintln!("oc-camera: phone disconnected, holding the last frame");
}

async fn index() -> Html<&'static str> {
    Html(PAGE)
}

async fn health(State(hub): State<Arc<Hub>>) -> String {
    format!(
        "accepted={} published={}\n",
        hub.accepted.load(Ordering::Relaxed),
        hub.frames.load(Ordering::Relaxed)
    )
}

async fn ws(State(hub): State<Arc<Hub>>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade
        .max_message_size(MAX_JPEG)
        .max_frame_size(MAX_JPEG)
        .on_upgrade(move |socket| socket_task(socket, hub))
}

async fn socket_task(socket: WebSocket, hub: Arc<Hub>) {
    phone(socket, hub).await;
}

fn arg(flag: &str) -> Result<String, String> {
    let mut args = std::env::args().skip(1);
    while let Some(item) = args.next() {
        if item == flag {
            return args.next().ok_or_else(|| format!("missing value for {flag}"));
        }
    }
    Err(format!("missing {flag}"))
}

fn has_flag(flag: &str) -> bool {
    std::env::args().skip(1).any(|item| item == flag)
}

/// One solid JPEG from ffmpeg, used to prove the virtual camera without a phone.
fn sample_jpeg() -> Result<Vec<u8>, String> {
    let output = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-f",
            "lavfi",
            "-i",
            "color=c=0x2266cc:s=1280x720",
            "-frames:v",
            "1",
            "-q:v",
            "4",
            "-f",
            "mjpeg",
            "pipe:1",
        ])
        .output()
        .map_err(|e| format!("sample frame: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "sample frame failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }
    let (w, h) = accept_jpeg(&output.stdout).ok_or("sample frame is not a 1280x720 JPEG")?;
    if w != WIDTH || h != HEIGHT {
        return Err(format!("sample frame is {w}x{h}"));
    }
    Ok(output.stdout)
}

/// Width and height from a PNG header.
fn png_size(data: &[u8]) -> Option<(u32, u32)> {
    if data.len() < 24 || &data[0..8] != b"\x89PNG\r\n\x1a\n" || &data[12..16] != b"IHDR" {
        return None;
    }
    let w = u32::from_be_bytes(data[16..20].try_into().ok()?);
    let h = u32::from_be_bytes(data[20..24].try_into().ok()?);
    Some((w, h))
}

/// Read one frame back from the virtual camera and require a 1280x720 PNG.
async fn readback(device: &Path, png: &Path) -> Result<(), String> {
    let mut last = String::from("no attempt");
    for _ in 0..8 {
        if png.exists() {
            let _ = std::fs::remove_file(png);
        }
        let result = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-fflags",
                "nobuffer",
                "-f",
                "v4l2",
                "-i",
            ])
            .arg(device)
            .args(["-frames:v", "1", "-y"])
            .arg(png)
            .output()
            .await
            .map_err(|e| format!("readback: {e}"))?;
        if result.status.success() {
            let bytes = std::fs::read(png).map_err(|e| format!("read {png:?}: {e}"))?;
            if let Some((w, h)) = png_size(&bytes) {
                if w == u32::from(WIDTH) && h == u32::from(HEIGHT) {
                    return Ok(());
                }
                last = format!("frame was {w}x{h}");
            } else {
                last = format!("output was not a png ({} bytes)", bytes.len());
            }
        } else {
            last = String::from_utf8_lossy(&result.stderr).trim().to_string();
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
    }
    Err(format!("camera readback failed: {last}"))
}

async fn self_test(hub: Arc<Hub>, png: PathBuf) -> Result<(), String> {
    let jpeg = sample_jpeg()?;
    *hub.latest.lock().await = Some(jpeg);
    let feeder = tokio::spawn(feed(hub.clone()));
    let read = readback(&hub.device, &png).await;
    feeder.abort();
    read?;
    println!("self-test ok {}", png.display());
    Ok(())
}

async fn serve(hub: Arc<Hub>, bind: &str, cert: &Path, key: &Path, public_url: &str) -> Result<(), String> {
    tokio::spawn(feed(hub.clone()));
    let config = RustlsConfig::from_pem_file(cert, key)
        .await
        .map_err(|e| format!("tls: {e}"))?;
    let addr: std::net::SocketAddr = bind.parse().map_err(|e| format!("bind address: {e}"))?;
    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/ws", get(ws))
        .with_state(hub);
    println!("listening {public_url}");
    axum_server::bind_rustls(addr, config)
        .serve(app.into_make_service())
        .await
        .map_err(|e| format!("server: {e}"))
}

#[tokio::main]
async fn main() -> Result<(), String> {
    let device = PathBuf::from(arg("--device")?);
    let log_path = PathBuf::from(arg("--ffmpeg-log")?);
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("log dir: {e}"))?;
    }
    let hub = Arc::new(Hub {
        device,
        log_path,
        latest: Mutex::new(None),
        frames: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
    });
    if has_flag("--self-test") {
        let png = PathBuf::from(arg("--png")?);
        if let Some(parent) = png.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("png dir: {e}"))?;
        }
        return self_test(hub, png).await;
    }
    let bind = arg("--bind")?;
    let cert = PathBuf::from(arg("--cert")?);
    let key = PathBuf::from(arg("--key")?);
    let public_url = arg("--public-url")?;
    tokio::spawn(async {
        let _ = tokio::signal::ctrl_c().await;
        std::process::exit(0);
    });
    serve(hub, &bind, &cert, &key, &public_url).await
}
