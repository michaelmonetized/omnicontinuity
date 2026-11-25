//! iPhone camera for oc.
//!
//! Safari on the phone sends JPEG frames over the tailnet. This process
//! repeats the newest frame at 24 fps into ffmpeg, which writes a steady
//! picture to the v4l2loopback device apps already know how to open.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use axum::Router;
use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum_server::tls_rustls::RustlsConfig;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

mod setup;
#[cfg(target_os = "linux")]
mod v4l2;

const WIDTH: u16 = 1280;
const HEIGHT: u16 = 720;
const FPS: u64 = 24;
const MAX_JPEG: usize = 2 * 1024 * 1024;
const PAGE: &str = include_str!("../web/index.html");
const CAMERA_FILTER: &str = concat!(
    "scale=1280:720:force_original_aspect_ratio=increase:",
    "in_color_matrix=bt601:out_color_matrix=bt601:in_range=full:out_range=limited,",
    "crop=1280:720,format=yuv420p,",
    "setparams=range=limited:color_primaries=bt709:color_trc=iec61966-2-1:colorspace=bt470bg",
);
const SAMPLE_RGB: [u8; 3] = [0x22, 0x66, 0xcc];

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
        let sof =
            (0xC0..=0xCF).contains(&marker) && marker != 0xC4 && marker != 0xC8 && marker != 0xCC;
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
                CAMERA_FILTER,
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
    let mut tick = tokio::time::interval(Duration::from_secs_f64(1.0 / FPS as f64));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
            .kill_on_drop(true)
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
    let read = tokio::time::timeout(Duration::from_secs(15), readback(&hub.device, &png)).await;
    feeder.abort();
    let _ = feeder.await;
    read.map_err(|_| "camera readback timed out after 15 seconds".to_owned())??;
    check_sample_color(&png).await?;
    println!("self-test ok {}", png.display());
    Ok(())
}

/// Verify the sample picture's color.
///
/// Accepts the readback PNG path. Decodes it without color overrides and returns
/// an error unless every RGB channel is within three levels of the sample.
async fn check_sample_color(png: &Path) -> Result<(), String> {
    let mut command = Command::new("ffmpeg");
    command
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(png)
        .args([
            "-frames:v",
            "1",
            "-pix_fmt",
            "rgb24",
            "-f",
            "rawvideo",
            "pipe:1",
        ])
        .kill_on_drop(true);
    let output = tokio::time::timeout(Duration::from_secs(10), command.output())
        .await
        .map_err(|_| "camera color check timed out".to_owned())?
        .map_err(|e| format!("camera color decode: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "camera color decode: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let expected_len = usize::from(WIDTH) * usize::from(HEIGHT) * 3;
    if output.stdout.len() != expected_len {
        return Err(format!(
            "camera color check: expected {expected_len} RGB bytes, got {}",
            output.stdout.len()
        ));
    }
    for (index, pixel) in output.stdout.as_chunks::<3>().0.iter().enumerate() {
        if pixel
            .iter()
            .zip(SAMPLE_RGB)
            .any(|(&actual, expected)| actual.abs_diff(expected) > 3)
        {
            return Err(format!(
                "camera color check: pixel {index} was {pixel:?}, expected {SAMPLE_RGB:?} within 3 levels"
            ));
        }
    }
    Ok(())
}

async fn serve(
    hub: Arc<Hub>,
    listener: std::net::TcpListener,
    cert: &Path,
    key: &Path,
    public_url: &str,
) -> Result<(), String> {
    let config = RustlsConfig::from_pem_file(cert, key)
        .await
        .map_err(|e| format!("tls: {e}"))?;
    let app = Router::new()
        .route("/", get(index))
        .route("/health", get(health))
        .route("/ws", get(ws))
        .with_state(hub.clone());
    println!("listening {public_url}");
    let feeder = tokio::spawn(feed(hub));
    let server = axum_server::from_tcp_rustls(listener, config).serve(app.into_make_service());
    let result = tokio::select! {
        result = server => result.map_err(|e| format!("server: {e}")),
        result = tokio::signal::ctrl_c() => result.map_err(|e| format!("shutdown signal: {e}")),
    };
    feeder.abort();
    let _ = feeder.await;
    result
}

/// Camera paths and device selection.
#[derive(Debug)]
pub struct Options {
    pub device: PathBuf,
    pub state_dir: PathBuf,
    pub ffmpeg_log: Option<PathBuf>,
    pub png: Option<PathBuf>,
    pub self_test: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            device: PathBuf::from("/dev/video42"),
            state_dir: PathBuf::from("var/camera"),
            ffmpeg_log: None,
            png: None,
            self_test: false,
        }
    }
}

/// Run the iPhone camera or its device readback test.
///
/// Accepts camera options. Discovers the local Tailscale identity, reserves its
/// IPv4 port 8443 before setup, and returns when stopped or when an error occurs.
/// Self-test writes a sample frame and reads a PNG back from the actual device.
pub async fn run(options: Options) -> Result<(), String> {
    if !cfg!(target_os = "linux") {
        return Err("the V4L2 camera requires Linux".to_owned());
    }
    let tailnet = oc_core::Tailnet::discover()?;
    if tailnet.backend_state != "Running" {
        return Err(format!("Tailscale is {}", tailnet.backend_state));
    }
    let ip = tailnet
        .this_device
        .ipv4()
        .ok_or("this machine has no Tailscale IPv4 address")?;
    let listener = std::net::TcpListener::bind((ip, 8443))
        .map_err(|e| format!("cannot reserve {ip}:8443; leave any existing camera running: {e}"))?;
    listener
        .set_nonblocking(true)
        .map_err(|e| format!("camera listener: {e}"))?;
    setup::device(&options.device)?;
    #[cfg(target_os = "linux")]
    v4l2::configure(&options.device)?;
    let log_path = options
        .ffmpeg_log
        .unwrap_or_else(|| options.state_dir.join("ffmpeg.log"));
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("log dir: {e}"))?;
    }
    let hub = Arc::new(Hub {
        device: options.device,
        log_path,
        latest: Mutex::new(None),
        frames: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
    });
    if options.self_test {
        let png = options
            .png
            .unwrap_or_else(|| options.state_dir.join("self-test.png"));
        if let Some(parent) = png.parent() {
            std::fs::create_dir_all(parent).map_err(|e| format!("png dir: {e}"))?;
        }
        return self_test(hub, png).await;
    }
    let hostname = tailnet.this_device.dns_name;
    let (cert, key) = setup::certificates(&options.state_dir, &hostname)?;
    let public_url = format!("https://{hostname}:8443/");
    serve(hub, listener, &cert, &key, &public_url).await
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Run an offline frame conversion.
    ///
    /// Accepts stdin bytes and ffmpeg arguments. Returns stdout after requiring
    /// a successful exit; no device, window or network endpoint is opened.
    fn convert(input: &[u8], args: &[&str]) -> Vec<u8> {
        use std::io::Write;

        let mut child = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error"])
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input).unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn camera_samples_preserve_color_through_an_unforced_yu12_reader() {
        for color in [
            "0x2266cc", "black", "white", "red", "lime", "blue", "0x808080",
        ] {
            let jpeg = convert(
                &[],
                &[
                    "-f",
                    "lavfi",
                    "-i",
                    &format!("color=c={color}:s=1280x720"),
                    "-frames:v",
                    "1",
                    "-q:v",
                    "4",
                    "-f",
                    "mjpeg",
                    "pipe:1",
                ],
            );
            let reference = convert(
                &jpeg,
                &[
                    "-f",
                    "mjpeg",
                    "-i",
                    "pipe:0",
                    "-frames:v",
                    "1",
                    "-pix_fmt",
                    "rgb24",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ],
            );
            let yuv = convert(
                &jpeg,
                &[
                    "-f",
                    "mjpeg",
                    "-i",
                    "pipe:0",
                    "-vf",
                    CAMERA_FILTER,
                    "-frames:v",
                    "1",
                    "-pix_fmt",
                    "yuv420p",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ],
            );
            let rgb = convert(
                &yuv,
                &[
                    "-f",
                    "rawvideo",
                    "-pixel_format",
                    "yuv420p",
                    "-video_size",
                    "1280x720",
                    "-i",
                    "pipe:0",
                    "-frames:v",
                    "1",
                    "-pix_fmt",
                    "rgb24",
                    "-f",
                    "rawvideo",
                    "pipe:1",
                ],
            );
            assert_eq!(yuv.len(), usize::from(WIDTH) * usize::from(HEIGHT) * 3 / 2);
            assert_eq!(rgb.len(), reference.len());
            let error = rgb
                .iter()
                .zip(&reference)
                .map(|(&actual, &expected)| actual.abs_diff(expected))
                .max()
                .unwrap();
            assert!(error <= 3, "{color}: maximum channel error {error}");
        }
    }

    #[test]
    fn integrated_tls_selects_a_crypto_provider_without_panicking() {
        let builder = rustls::ServerConfig::builder();
        assert!(!builder.crypto_provider().cipher_suites.is_empty());
    }

    #[test]
    fn accepts_an_ffmpeg_frame_and_rejects_damaged_or_oversized_payloads() {
        let jpeg = sample_jpeg().unwrap();
        assert_eq!(accept_jpeg(&jpeg), Some((WIDTH, HEIGHT)));
        assert_eq!(accept_jpeg(&jpeg[..jpeg.len() - 2]), None);
        let mut damaged = jpeg.clone();
        damaged[0] = 0;
        assert_eq!(accept_jpeg(&damaged), None);
        let mut oversized = jpeg;
        oversized.resize(MAX_JPEG + 1, 0);
        assert_eq!(accept_jpeg(&oversized), None);
    }

    #[test]
    fn rejects_empty_frames_and_reads_png_dimensions() {
        assert_eq!(accept_jpeg(&[]), None);
        assert_eq!(png_size(&[]), None);
        let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        png.extend_from_slice(&u32::from(WIDTH).to_be_bytes());
        png.extend_from_slice(&u32::from(HEIGHT).to_be_bytes());
        assert_eq!(png_size(&png), Some((1280, 720)));
        assert_eq!(png_size(&png[..23]), None);
    }
}
