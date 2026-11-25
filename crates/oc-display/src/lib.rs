//! Extend a Hyprland desktop onto an OpenDisplay receiver.
//!
//! `extend(Target::Wifi(address)).await?` connects to port 9000 and requests
//! screen-sharing permission. Select the named OC-DISPLAY monitor in the portal.
//! The returned session owns the output; call `session.stop().await?` to remove it.
//! USB uses `Target::Usb { udid }` and the installed libusbmuxd library.

mod capture;
mod hyprland;
mod input;
pub mod protocol;
mod transport;

use std::{
    net::IpAddr,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use ashpd::desktop::{
    Session,
    screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType},
};
use openh264::{
    OpenH264API,
    encoder::{BitRate, Encoder, EncoderConfig, FrameRate, UsageType},
    formats::{RgbSliceU8, YUVBuffer},
};
use serde_json::{Value, json};

pub use protocol::Configuration;
use transport::Connection;

/// Receiver connection address.
/// Wi-Fi takes the advertised IP; USB takes the selected device UDID.
#[derive(Clone, Debug)]
pub enum Target {
    Wifi(IpAddr),
    Usb { udid: String },
}

/// Running extended display.
/// Owns the connection, capture worker, input device and virtual output.
pub struct DisplaySession {
    output_name: String,
    configuration: Configuration,
    stop: Arc<AtomicBool>,
    socket: Connection,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl DisplaySession {
    /// Output name.
    /// Takes no arguments and returns this session's Hyprland output name.
    pub fn output_name(&self) -> &str {
        &self.output_name
    }

    /// Initial video configuration.
    /// Takes no arguments and returns the configuration negotiated at startup.
    pub fn configuration(&self) -> Configuration {
        self.configuration
    }

    /// Check whether streaming stopped.
    /// Takes no arguments and returns true after the worker exits; `stop` still
    /// joins the worker and reports its result.
    pub fn is_finished(&self) -> bool {
        self.worker
            .as_ref()
            .is_none_or(thread::JoinHandle::is_finished)
    }

    /// Stop the extension.
    /// Consumes the session, waits for cleanup and returns any worker error.
    pub async fn stop(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        self.socket.shutdown();
        self.join().await
    }

    /// Wait for the receiver to disconnect.
    /// Consumes the session and returns after output removal or a worker error.
    pub async fn wait(mut self) -> Result<()> {
        self.join().await
    }

    async fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            tokio::task::spawn_blocking(move || {
                worker
                    .join()
                    .map_err(|_| anyhow::anyhow!("display worker panicked"))?
            })
            .await??;
        }
        Ok(())
    }
}

impl Drop for DisplaySession {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        self.socket.shutdown();
    }
}

/// Start an extended monitor.
/// `target` identifies an advertised Wi-Fi address or USB UDID. Returns a session
/// after the receiver hello, portal selection and first encoded frame succeed.
/// Requires Hyprland, the ScreenCast portal, PipeWire and writable `/dev/uinput`.
/// Cancelling this future stops startup and joins its worker after bounded I/O,
/// waiting for output cleanup before the cancelled future is dropped.
pub async fn extend(target: Target) -> Result<DisplaySession> {
    let stop = Arc::new(AtomicBool::new(false));
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (socket_tx, socket_rx) = tokio::sync::oneshot::channel();
    let worker_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("oc-display".into())
        .spawn(move || {
            let result = run(target, worker_stop.clone(), socket_tx, ready_tx);
            worker_stop.store(true, Ordering::Release);
            result
        })?;
    let mut startup = Startup {
        pending: Pending {
            stop: stop.clone(),
            socket: None,
            armed: true,
        },
        worker: Some(worker),
    };
    let socket = match socket_rx.await {
        Ok(socket) => socket,
        Err(_) => {
            startup.join()?;
            anyhow::bail!("display stopped before connecting");
        }
    };
    startup.pending.socket = Some(socket.try_clone()?);
    match ready_rx.await {
        Ok((output_name, configuration)) => {
            startup.pending.socket = None;
            startup.pending.armed = false;
            Ok(DisplaySession {
                output_name,
                configuration,
                stop,
                socket,
                worker: startup.worker.take(),
            })
        }
        Err(_) => {
            startup.join()?;
            anyhow::bail!("receiver disconnected before the first video frame")
        }
    }
}

struct Startup {
    pending: Pending,
    worker: Option<thread::JoinHandle<Result<()>>>,
}

impl Startup {
    fn join(&mut self) -> Result<()> {
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("display worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Startup {
    fn drop(&mut self) {
        if let Some(worker) = self.worker.take() {
            self.pending.cancel();
            match worker.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("oc: display startup cleanup: {error:#}"),
                Err(_) => eprintln!("oc: display startup worker panicked"),
            }
        }
    }
}

struct Pending {
    stop: Arc<AtomicBool>,
    socket: Option<Connection>,
    armed: bool,
}
impl Pending {
    fn cancel(&self) {
        if self.armed {
            self.stop.store(true, Ordering::Release);
            if let Some(socket) = &self.socket {
                socket.shutdown();
            }
        }
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn send(socket: &Mutex<Connection>, message: &Value) -> Result<()> {
    protocol::write_control(
        &mut *socket
            .lock()
            .map_err(|_| anyhow::anyhow!("connection lock poisoned"))?,
        message,
    )
}

fn receive(
    mut reader: Connection,
    writer: &Mutex<Connection>,
    stop: &AtomicBool,
    events: mpsc::SyncSender<Value>,
) -> Result<()> {
    while !stop.load(Ordering::Acquire) {
        let payload = protocol::read_frame(&mut reader)?;
        let Some(message) = protocol::control(&payload) else {
            continue;
        };
        match message["type"].as_str() {
            Some("ping") => {
                if let Some(t) = message.get("t") {
                    send(writer, &json!({"type":"pong", "t":t, "mt":now_ms()}))?;
                }
            }
            Some("closing" | "sleeping") => {
                events
                    .try_send(message)
                    .context("receiver input queue overflow")?;
                stop.store(true, Ordering::Release);
                break;
            }
            Some("hello" | "touch" | "scroll" | "pencil" | "proximity" | "kf") => {
                events
                    .try_send(message)
                    .context("receiver input queue overflow")?;
            }
            _ => {}
        }
    }
    Ok(())
}

fn run(
    target: Target,
    stop: Arc<AtomicBool>,
    socket_tx: tokio::sync::oneshot::Sender<Connection>,
    ready: tokio::sync::oneshot::Sender<(String, Configuration)>,
) -> Result<()> {
    let mut socket = Connection::connect(&target).context("connecting to OpenDisplay receiver")?;
    socket_tx
        .send(socket.try_clone()?)
        .map_err(|_| anyhow::anyhow!("startup cancelled"))?;
    let hello = transport::hello(&mut socket)?;
    let configuration = Configuration::from_hello(&hello)?;
    let reader = socket.try_clone()?;
    let writer = Arc::new(Mutex::new(socket));
    send(&writer, &json!({"type":"welcome", "pv":3, "min":1}))?;
    let shutdown = writer.lock().unwrap().try_clone()?;
    let failure = Arc::new(Mutex::new(None::<String>));
    let (events_tx, events_rx) = mpsc::sync_channel(256);
    thread::scope(|scope| -> Result<()> {
        let _connection = Pending {
            stop: stop.clone(),
            socket: Some(shutdown),
            armed: true,
        };
        let reader_stop = stop.clone();
        let reply = writer.clone();
        let reader_failure = failure.clone();
        scope.spawn(move || {
            let result = receive(reader, &reply, &reader_stop, events_tx);
            if let Err(error) = result
                && !reader_stop.load(Ordering::Acquire)
            {
                *reader_failure.lock().unwrap() = Some(format!("receiver disconnected: {error:#}"));
                reader_stop.store(true, Ordering::Release);
            }
        });
        let beat_stop = stop.clone();
        let beat_writer = writer.clone();
        let beat_failure = failure.clone();
        scope.spawn(move || {
            let mut last = Instant::now() - Duration::from_secs(2);
            while !beat_stop.load(Ordering::Acquire) {
                if last.elapsed() >= Duration::from_secs(2) {
                    if let Err(error) = send(&beat_writer, &json!({"type":"ping"})) {
                        if !beat_stop.load(Ordering::Acquire) {
                            *beat_failure.lock().unwrap() =
                                Some(format!("heartbeat failed: {error:#}"));
                            beat_stop.store(true, Ordering::Release);
                        }
                        break;
                    }
                    last = Instant::now();
                }
                thread::sleep(Duration::from_millis(25));
            }
        });
        let result = (|| -> Result<()> {
            let mut output = hyprland::Output::create(configuration)?;
            let mut input = input::Input::new().context("creating Hyprland input device")?;
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .build()?;
            let (portal, session, remote, node) =
                runtime.block_on(select_monitor(&output.name, &stop))?;
            let result = (|| -> Result<()> {
                let mut capture = capture::Capture::new(remote, node, configuration)?;
                let mut encoder = encoder(configuration)?;
                let mut ready = Some(ready);
                let mut last_frame = None;
                let mut dirty = false;
                let mut keyframe = true;
                let mut last_send = Instant::now() - Duration::from_secs(1);
                let mut last_capture = Instant::now();
                send(&writer, &configuration.message())?;
                send(&writer, &json!({"type":"cursor", "v":0}))?;
                while !stop.load(Ordering::Acquire) {
                    for event in events_rx.try_iter() {
                        match event["type"].as_str() {
                            Some("closing" | "sleeping") => return Ok(()),
                            Some("hello") => {
                                let next = Configuration::from_hello(&event)?;
                                send(&writer, &json!({"type":"welcome", "pv":3, "min":1}))?;
                                if next != output.configuration {
                                    output.configure(next)?;
                                    capture.reconfigure(next)?;
                                    encoder = self::encoder(next)?;
                                    last_frame = None;
                                    dirty = false;
                                    last_capture = Instant::now();
                                }
                                send(&writer, &next.message())?;
                                keyframe = true;
                            }
                            Some("kf") => keyframe = true,
                            _ => input.handle(&event, &output)?,
                        }
                    }
                    let frame = capture.next()?;
                    if let Some(frame) = frame {
                        last_frame = Some(frame);
                        dirty = true;
                        last_capture = Instant::now();
                    }
                    ensure!(
                        last_frame.is_some() || last_capture.elapsed() < Duration::from_secs(10),
                        "PipeWire did not produce a frame"
                    );
                    if (dirty || keyframe)
                        && last_send.elapsed()
                            >= Duration::from_secs_f64(
                                1.0 / output.configuration.frames_per_second as f64,
                            )
                        && let Some(frame) = &last_frame
                    {
                        ensure!(
                            frame.width == output.configuration.width as usize
                                && frame.height == output.configuration.height as usize,
                            "capture dimensions differ from configured output"
                        );
                        if keyframe {
                            encoder.force_intra_frame();
                        }
                        let source = RgbSliceU8::new(&frame.rgb, (frame.width, frame.height));
                        let yuv = YUVBuffer::from_rgb_source(source);
                        let bitstream = encoder.encode(&yuv)?;
                        let bytes = protocol::annex_b(&bitstream.to_vec())?;
                        let mut payload =
                            serde_json::to_vec(&json!({"cap":frame.captured_at,"snd":now_ms()}))?;
                        payload.extend_from_slice(&bytes);
                        protocol::write_frame(&mut *writer.lock().unwrap(), &payload)?;
                        keyframe = false;
                        dirty = false;
                        last_send = Instant::now();
                        if let Some(ready) = ready.take() {
                            let _ = ready.send((output.name.clone(), output.configuration));
                        }
                    }
                }
                Ok(())
            })();
            let result = if stop.load(Ordering::Acquire) && failure.lock().unwrap().is_none() {
                Ok(())
            } else {
                result
            };
            drop(input);
            let remove = output.remove();
            let close = runtime.block_on(async {
                tokio::time::timeout(Duration::from_secs(2), session.close())
                    .await
                    .context("closing screen-sharing session timed out")
                    .and_then(|result| result.map_err(Into::into))
            });
            drop(portal);
            result.and(close).and(remove)
        })();
        if let Some(error) = failure.lock().unwrap().take() {
            result.and(Err(anyhow::anyhow!(error)))
        } else {
            result
        }
    })
}

async fn select_monitor(
    name: &str,
    stop: &AtomicBool,
) -> Result<(Screencast, Session<Screencast>, std::os::fd::OwnedFd, u32)> {
    let portal = Screencast::new().await?;
    let session = portal.create_session(Default::default()).await?;
    let selection = async {
        portal
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    .set_sources(Some(SourceType::Monitor.into()))
                    .set_multiple(false)
                    .set_cursor_mode(CursorMode::Embedded),
            )
            .await?
            .response()?;
        let response = portal
            .start(&session, None, Default::default())
            .await?
            .response()?;
        ensure!(
            response.streams().len() == 1,
            "select only the {name} monitor"
        );
        let stream = &response.streams()[0];
        ensure!(
            stream.mapping_id() == Some(name),
            "select {name} in the screen-sharing dialog; refusing a different monitor"
        );
        let remote = portal
            .open_pipe_wire_remote(&session, Default::default())
            .await?;
        Ok((remote, stream.pipe_wire_node_id()))
    };
    let result = tokio::select! {
        result = tokio::time::timeout(Duration::from_secs(120), selection) => result.context("screen-sharing selection timed out").and_then(|value| value),
        _ = async { while !stop.load(Ordering::Acquire) { tokio::time::sleep(Duration::from_millis(25)).await; } } => Err(anyhow::anyhow!("startup cancelled")),
    };
    match result {
        Ok((remote, node)) => Ok((portal, session, remote, node)),
        Err(error) => {
            let _ = tokio::time::timeout(Duration::from_secs(2), session.close()).await;
            Err(error)
        }
    }
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

fn encoder(configuration: Configuration) -> Result<Encoder> {
    Ok(Encoder::with_api_config(
        OpenH264API::from_source(),
        EncoderConfig::new()
            .usage_type(UsageType::ScreenContentRealTime)
            .max_frame_rate(FrameRate::from_hz(configuration.frames_per_second as f32))
            .bitrate(BitRate::from_bps(8_000_000))
            .skip_frames(false),
    )?)
}

#[cfg(test)]
mod tests;
