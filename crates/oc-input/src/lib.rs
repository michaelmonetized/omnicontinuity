//! Independent input sharing for Hyprland peers over Tailscale TCP.
//! `serve(Config)` opens explicitly selected evdev keyboards and relative mice,
//! listens on the configured tailnet address, and injects through `/dev/uinput`.
//! Both peers run this crate with the other peer's address and the corresponding edge.
//! Input access is required. Absolute touchpads/tablets are not translated.
//! Ctrl+Alt+Escape returns control locally. Disconnect and stop release held keys.
//! No lan-mouse source or protocol is used.
//!
//! ```no_run
//! # fn example() -> std::io::Result<()> {
//! let config = oc_input::Config::new(oc_input::Edge::Right,
//!     "100.64.0.2:4242".parse().unwrap(), "100.64.0.1:4242".parse().unwrap());
//! let mut server = oc_input::serve(config)?;
//! server.stop()?;
//! # Ok(()) }
//! ```

use evdev::{
    AttributeSet, Device, EventType, InputEvent, KeyCode, RelativeAxisCode, uinput::VirtualDevice,
};
use serde::Deserialize;
use std::{
    collections::{BTreeSet, VecDeque},
    io::{self, Read, Write},
    net::{IpAddr, Shutdown, SocketAddr, TcpListener, TcpStream},
    os::unix::net::UnixStream,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const PACKET_LEN: usize = 16;
const MAX_PENDING: usize = 256 * 1024;
const IDLE_TIMEOUT: Duration = Duration::from_secs(3);

/// The desktop edge next to the configured peer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Edge {
    Left,
    Right,
    Top,
    Bottom,
}
impl Edge {
    /// Return the facing edge. Takes no arguments; returns the opposite edge.
    pub fn opposite(self) -> Self {
        match self {
            Self::Left => Self::Right,
            Self::Right => Self::Left,
            Self::Top => Self::Bottom,
            Self::Bottom => Self::Top,
        }
    }
}

/// Logical desktop bounds, including a possibly negative monitor origin.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Rect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}
impl Rect {
    fn valid(self) -> bool {
        [self.x, self.y, self.width, self.height]
            .iter()
            .all(|v| v.is_finite())
            && self.width >= 4.0
            && self.height >= 4.0
    }
}

/// Detect outward motion at an edge. Takes desktop bounds, position and motion;
/// returns the normalized position along the edge, or `None` inside the desktop.
pub fn crossing(bounds: Rect, edge: Edge, position: (f64, f64), motion: (f64, f64)) -> Option<u16> {
    if !bounds.valid()
        || ![position.0, position.1, motion.0, motion.1]
            .iter()
            .all(|v| v.is_finite())
    {
        return None;
    }
    let (x, y) = position;
    let inside_x = x >= bounds.x && x < bounds.x + bounds.width;
    let inside_y = y >= bounds.y && y < bounds.y + bounds.height;
    let (crossed, along) = match edge {
        Edge::Left => (
            inside_y && x <= bounds.x && motion.0 < 0.0,
            (y - bounds.y) / (bounds.height - 1.0),
        ),
        Edge::Right => (
            inside_y && x >= bounds.x + bounds.width - 1.0 && motion.0 > 0.0,
            (y - bounds.y) / (bounds.height - 1.0),
        ),
        Edge::Top => (
            inside_x && y <= bounds.y && motion.1 < 0.0,
            (x - bounds.x) / (bounds.width - 1.0),
        ),
        Edge::Bottom => (
            inside_x && y >= bounds.y + bounds.height - 1.0 && motion.1 > 0.0,
            (x - bounds.x) / (bounds.width - 1.0),
        ),
    };
    crossed.then(|| (along.clamp(0.0, 1.0) * u16::MAX as f64).round() as u16)
}

/// Place the pointer just inside an edge. Takes bounds, entry edge and normalized
/// edge position; returns logical desktop coordinates, or invalid-bounds error.
pub fn entry_position(bounds: Rect, edge: Edge, along: u16) -> io::Result<(f64, f64)> {
    if !bounds.valid() {
        return Err(invalid("invalid desktop bounds"));
    }
    let t = along as f64 / u16::MAX as f64;
    Ok(match edge {
        Edge::Left => (bounds.x + 2.0, bounds.y + t * (bounds.height - 1.0)),
        Edge::Right => (
            bounds.x + bounds.width - 3.0,
            bounds.y + t * (bounds.height - 1.0),
        ),
        Edge::Top => (bounds.x + t * (bounds.width - 1.0), bounds.y + 2.0),
        Edge::Bottom => (
            bounds.x + t * (bounds.width - 1.0),
            bounds.y + bounds.height - 3.0,
        ),
    })
}

/// The fixed-size network messages. Events carry Linux evdev codes, never text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Packet {
    Enter { edge: Edge, along: u16 },
    Event { kind: u16, code: u16, value: i32 },
    Return { along: u16 },
    Leave,
    Ping,
}
impl Packet {
    /// Encode one message. Takes the message; returns a validated 16-byte network frame.
    pub fn encode(self) -> io::Result<[u8; PACKET_LEN]> {
        self.validate()?;
        let mut data = [0; PACKET_LEN];
        data[..4].copy_from_slice(b"OCIN");
        data[4] = 1;
        match self {
            Self::Enter { edge, along } => {
                data[5] = 1;
                data[6] = edge as u8;
                data[8..10].copy_from_slice(&along.to_be_bytes());
            }
            Self::Event { kind, code, value } => {
                data[5] = 2;
                data[6..8].copy_from_slice(&kind.to_be_bytes());
                data[8..10].copy_from_slice(&code.to_be_bytes());
                data[10..14].copy_from_slice(&value.to_be_bytes());
            }
            Self::Leave => data[5] = 3,
            Self::Ping => data[5] = 4,
            Self::Return { along } => {
                data[5] = 5;
                data[8..10].copy_from_slice(&along.to_be_bytes());
            }
        }
        Ok(data)
    }

    /// Decode one frame. Takes exactly 16 bytes; returns a message or a protocol error.
    pub fn decode(data: &[u8]) -> io::Result<Self> {
        if data.len() != PACKET_LEN || &data[..4] != b"OCIN" || data[4] != 1 || data[14..] != [0, 0]
        {
            return Err(invalid("invalid input frame header"));
        }
        let code = u16::from_be_bytes([data[8], data[9]]);
        let packet = match data[5] {
            1 => {
                if data[7] != 0 || data[10..14] != [0; 4] {
                    return Err(invalid("invalid entry frame"));
                }
                let edge = match data[6] {
                    0 => Edge::Left,
                    1 => Edge::Right,
                    2 => Edge::Top,
                    3 => Edge::Bottom,
                    _ => return Err(invalid("invalid edge")),
                };
                Self::Enter { edge, along: code }
            }
            2 => Self::Event {
                kind: u16::from_be_bytes([data[6], data[7]]),
                code,
                value: i32::from_be_bytes(data[10..14].try_into().unwrap()),
            },
            5 if data[6..8] == [0; 2] && data[10..14] == [0; 4] => Self::Return { along: code },
            3 | 4 if data[6..14] == [0; 8] => {
                if data[5] == 3 {
                    Self::Leave
                } else {
                    Self::Ping
                }
            }
            _ => return Err(invalid("unknown input message")),
        };
        packet.validate()?;
        Ok(packet)
    }

    fn validate(self) -> io::Result<()> {
        match self {
            Self::Event {
                kind: 1,
                code: 1..=767,
                value: 0..=2,
            } => Ok(()),
            Self::Event {
                kind: 2,
                code: 0 | 1 | 6 | 8 | 11 | 12,
                value: -32767..=32767,
            } => Ok(()),
            Self::Event { .. } => Err(invalid("unsupported input event")),
            _ => Ok(()),
        }
    }
}

/// Peer settings. `input_devices` names physical keyboard/mouse event nodes; an empty
/// list discovers accessible keyboards and relative mice at startup. Both socket IPs
/// must be Tailscale IPs. `monitor` optionally restricts sharing to one Hyprland output.
#[derive(Debug, Clone)]
pub struct Config {
    pub edge: Edge,
    pub peer: SocketAddr,
    pub listen: SocketAddr,
    pub input_devices: Vec<PathBuf>,
    pub monitor: Option<String>,
}
impl Config {
    /// Create settings. Takes the peer edge, peer socket and local tailnet socket;
    /// returns settings with automatic input discovery and all active outputs.
    pub fn new(edge: Edge, peer: SocketAddr, listen: SocketAddr) -> Self {
        Self {
            edge,
            peer,
            listen,
            input_devices: Vec::new(),
            monitor: None,
        }
    }
    fn validate(&self) -> io::Result<()> {
        if !tailnet(self.peer.ip())
            || !tailnet(self.listen.ip())
            || self.peer.port() == 0
            || self.listen.port() == 0
            || self.peer.ip() == self.listen.ip()
        {
            return Err(invalid(
                "use distinct Tailscale peer and local IPs with nonzero ports",
            ));
        }
        Ok(())
    }
}

fn tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let b = ip.octets();
            b[0] == 100 && b[1] & 0xc0 == 0x40
        }
        IpAddr::V6(ip) => ip.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

/// A background sharing service. Call `stop` to join it and report runtime errors.
pub struct InputServer {
    stop: Arc<AtomicBool>,
    task: Option<JoinHandle<io::Result<()>>>,
}
impl InputServer {
    /// Stop sharing. Takes no arguments; returns after devices and remote keys are released.
    pub fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(task) = self.task.take() {
            task.join()
                .map_err(|_| io::Error::other("input service panicked"))??;
        }
        Ok(())
    }
    /// Report worker completion. Takes no arguments; returns whether the worker has ended.
    pub fn is_finished(&self) -> bool {
        self.task.as_ref().is_none_or(|task| task.is_finished())
    }
}
impl Drop for InputServer {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Start input sharing. Takes tailnet peer settings; returns a stoppable service.
/// Opens evdev/uinput and checks Hyprland before spawning. No permissions are changed.
/// Connections are accepted only from the configured peer IP on the tailnet interface;
/// Tailscale provides peer authentication and encryption. Apply tailnet ACLs normally.
pub fn serve(config: Config) -> io::Result<InputServer> {
    config.validate()?;
    let hypr = Hyprland::connect()?;
    let bounds = hypr.bounds(config.monitor.as_deref())?;
    let devices = open_devices(&config.input_devices)?;
    let injector = Injector::new()?;
    let listener = TcpListener::bind(config.listen)?;
    listener.set_nonblocking(true)?;
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let task = thread::Builder::new()
        .name("oc-input".into())
        .spawn(move || {
            run(
                config,
                hypr,
                bounds,
                devices,
                injector,
                listener,
                worker_stop,
            )
        })?;
    Ok(InputServer {
        stop,
        task: Some(task),
    })
}

fn open_devices(paths: &[PathBuf]) -> io::Result<Vec<Device>> {
    let mut devices = if paths.is_empty() {
        evdev::enumerate()
            .map(|(_, d)| d)
            .filter(shareable)
            .collect::<Vec<_>>()
    } else {
        paths
            .iter()
            .map(Device::open)
            .collect::<io::Result<Vec<_>>>()?
    };
    if devices.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "no accessible evdev keyboard or relative mouse",
        ));
    }
    for device in &mut devices {
        if !shareable(device) {
            return Err(invalid(
                "select physical keyboards or relative mice; exclude oc-input devices",
            ));
        }
        device.set_nonblocking(true)?;
    }
    Ok(devices)
}
fn shareable(device: &Device) -> bool {
    !device.name().unwrap_or("").starts_with("oc-input")
        && (device
            .supported_keys()
            .is_some_and(|k| k.contains(KeyCode::KEY_A))
            || device.supported_relative_axes().is_some_and(|a| {
                a.contains(RelativeAxisCode::REL_X) && a.contains(RelativeAxisCode::REL_Y)
            }))
}

struct Injector {
    device: VirtualDevice,
    held: BTreeSet<u16>,
}
impl Injector {
    fn new() -> io::Result<Self> {
        let keys: AttributeSet<KeyCode> = (1..768).map(KeyCode).collect();
        let axes: AttributeSet<RelativeAxisCode> = [0, 1, 6, 8, 11, 12]
            .into_iter()
            .map(RelativeAxisCode)
            .collect();
        let device = VirtualDevice::builder()?
            .name("oc-input remote keyboard and mouse")
            .with_keys(&keys)?
            .with_relative_axes(&axes)?
            .build()?;
        Ok(Self {
            device,
            held: BTreeSet::new(),
        })
    }
    fn event(&mut self, kind: u16, code: u16, value: i32) -> io::Result<()> {
        self.device.emit(&[InputEvent::new(kind, code, value)])?;
        if kind == 1 {
            if value == 0 {
                self.held.remove(&code);
            } else {
                self.held.insert(code);
            }
        }
        Ok(())
    }
    fn release(&mut self) -> io::Result<()> {
        let events: Vec<_> = self
            .held
            .iter()
            .map(|code| InputEvent::new(1, *code, 0))
            .collect();
        self.device.emit(&events)?;
        self.held.clear();
        Ok(())
    }
}
impl Drop for Injector {
    fn drop(&mut self) {
        let _ = self.release();
    }
}

struct Hyprland {
    socket: PathBuf,
}
#[derive(Deserialize)]
struct Monitor {
    name: String,
    x: f64,
    y: f64,
    width: f64,
    height: f64,
    scale: f64,
    transform: u32,
    #[serde(default)]
    disabled: bool,
}
#[derive(Deserialize)]
struct Cursor {
    x: f64,
    y: f64,
}
impl Hyprland {
    fn connect() -> io::Result<Self> {
        let runtime = std::env::var_os("XDG_RUNTIME_DIR")
            .ok_or_else(|| invalid("XDG_RUNTIME_DIR is unset"))?;
        let instance = std::env::var("HYPRLAND_INSTANCE_SIGNATURE")
            .map_err(|_| invalid("Hyprland is not running"))?;
        if instance.contains('/') || instance == ".." {
            return Err(invalid("invalid Hyprland instance"));
        }
        Ok(Self {
            socket: PathBuf::from(runtime)
                .join("hypr")
                .join(instance)
                .join(".socket.sock"),
        })
    }
    fn request(&self, command: &str) -> io::Result<Vec<u8>> {
        let mut socket = UnixStream::connect(&self.socket)?;
        socket.set_read_timeout(Some(Duration::from_millis(250)))?;
        socket.set_write_timeout(Some(Duration::from_millis(250)))?;
        socket.write_all(command.as_bytes())?;
        let mut output = Vec::new();
        socket.take(1024 * 1024).read_to_end(&mut output)?;
        Ok(output)
    }
    fn bounds(&self, name: Option<&str>) -> io::Result<Rect> {
        let monitors: Vec<Monitor> = serde_json::from_slice(&self.request("j/monitors")?)?;
        let mut rects = monitors
            .into_iter()
            .filter(|m| !m.disabled && name.is_none_or(|n| n == m.name))
            .map(monitor_rect);
        let mut bounds = rects
            .next()
            .ok_or_else(|| invalid("no matching active Hyprland monitor"))??;
        for rect in rects {
            let rect = rect?;
            let x = bounds.x.min(rect.x);
            let y = bounds.y.min(rect.y);
            bounds = Rect {
                x,
                y,
                width: (bounds.x + bounds.width).max(rect.x + rect.width) - x,
                height: (bounds.y + bounds.height).max(rect.y + rect.height) - y,
            };
        }
        Ok(bounds)
    }
    fn position(&self) -> io::Result<(f64, f64)> {
        let c: Cursor = serde_json::from_slice(&self.request("j/cursorpos")?)?;
        Ok((c.x, c.y))
    }
    fn warp(&self, position: (f64, f64)) -> io::Result<()> {
        let result = self.request(&format!(
            "dispatch movecursor {:.0} {:.0}",
            position.0, position.1
        ))?;
        if result.starts_with(b"ok") {
            Ok(())
        } else {
            Err(io::Error::other("Hyprland rejected pointer placement"))
        }
    }
}
fn monitor_rect(m: Monitor) -> io::Result<Rect> {
    if !m.scale.is_finite() || m.scale <= 0.0 {
        return Err(invalid("invalid monitor scale"));
    }
    let (width, height) = if m.transform % 2 == 1 {
        (m.height, m.width)
    } else {
        (m.width, m.height)
    };
    let rect = Rect {
        x: m.x,
        y: m.y,
        width: width / m.scale,
        height: height / m.scale,
    };
    if rect.valid() {
        Ok(rect)
    } else {
        Err(invalid("invalid monitor geometry"))
    }
}

struct ConnectionState {
    socket: TcpStream,
    input: Vec<u8>,
    output: VecDeque<[u8; PACKET_LEN]>,
    offset: usize,
    received: Instant,
    ping: Instant,
}
impl ConnectionState {
    fn new(socket: TcpStream) -> io::Result<Self> {
        socket.set_nonblocking(true)?;
        socket.set_nodelay(true)?;
        Ok(Self {
            socket,
            input: Vec::new(),
            output: VecDeque::new(),
            offset: 0,
            received: Instant::now(),
            ping: Instant::now(),
        })
    }
    fn send(&mut self, packet: Packet) -> io::Result<()> {
        if (self.output.len() + 1) * PACKET_LEN > MAX_PENDING {
            return Err(io::Error::other("peer input queue is full"));
        }
        self.output.push_back(packet.encode()?);
        Ok(())
    }
    fn discard_pending(&mut self) {
        self.output.truncate(usize::from(self.offset > 0));
    }
    fn flush(&mut self) -> io::Result<()> {
        while let Some(packet) = self.output.front() {
            match self.socket.write(&packet[self.offset..]) {
                Ok(0) => return Err(io::Error::new(io::ErrorKind::WriteZero, "peer closed")),
                Ok(n) => {
                    self.offset += n;
                    if self.offset == PACKET_LEN {
                        self.output.pop_front();
                        self.offset = 0;
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }
    fn packets(&mut self) -> io::Result<Vec<Packet>> {
        let mut buffer = [0u8; 4096];
        match self.socket.read(&mut buffer) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "peer disconnected",
                ));
            }
            Ok(n) => {
                self.input.extend_from_slice(&buffer[..n]);
                self.received = Instant::now();
            }
            Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
            Err(e) => return Err(e),
        }
        let count = self.input.len() / PACKET_LEN;
        let packets = self.input[..count * PACKET_LEN]
            .as_chunks::<PACKET_LEN>()
            .0
            .iter()
            .map(|packet| Packet::decode(packet))
            .collect::<io::Result<Vec<_>>>()?;
        self.input.drain(..count * PACKET_LEN);
        if self.received.elapsed() > IDLE_TIMEOUT {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "peer heartbeat timed out",
            ));
        }
        if self.ping.elapsed() > Duration::from_millis(500) {
            self.send(Packet::Ping)?;
            self.ping = Instant::now();
        }
        self.flush()?;
        Ok(packets)
    }
}
impl Drop for ConnectionState {
    fn drop(&mut self) {
        let _ = self.socket.shutdown(Shutdown::Both);
    }
}

#[derive(Clone, Copy)]
enum Mode {
    Local,
    Sending,
    Receiving(Edge),
}
fn ungrab(devices: &mut [Device]) -> io::Result<()> {
    let mut error = None;
    for device in devices {
        if device.is_grabbed()
            && let Err(e) = device.ungrab()
        {
            error = Some(e);
        }
    }
    error.map_or(Ok(()), Err)
}

fn run(
    config: Config,
    hypr: Hyprland,
    mut bounds: Rect,
    mut devices: Vec<Device>,
    mut injector: Injector,
    listener: TcpListener,
    stop: Arc<AtomicBool>,
) -> io::Result<()> {
    let mut connection: Option<ConnectionState> = None;
    let mut mode = Mode::Local;
    let mut cooldown = Instant::now();
    let mut geometry_updated = Instant::now();
    let mut last_motion = (0.0, 0.0);
    let result = (|| {
        while !stop.load(Ordering::Acquire) {
            if geometry_updated.elapsed() > Duration::from_secs(1) {
                bounds = hypr.bounds(config.monitor.as_deref())?;
                geometry_updated = Instant::now();
            }
            match listener.accept() {
                Ok((socket, addr)) if addr.ip() == config.peer.ip() && connection.is_none() => {
                    connection = Some(ConnectionState::new(socket)?);
                }
                Ok(_) => {}
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => {}
                Err(e) => return Err(e),
            }
            if let Some(conn) = &mut connection {
                match conn.packets() {
                    Ok(packets) => {
                        for packet in packets {
                            match packet {
                                Packet::Enter { edge, along } => {
                                    if edge.opposite() != config.edge {
                                        return Err(invalid(
                                            "peer edge does not face the configured edge",
                                        ));
                                    }
                                    if matches!(mode, Mode::Sending)
                                        && config.listen.ip() < config.peer.ip()
                                    {
                                        continue;
                                    }
                                    if matches!(mode, Mode::Receiving(_)) {
                                        return Err(invalid("duplicate input session"));
                                    }
                                    ungrab(&mut devices)?;
                                    conn.discard_pending();
                                    injector.release()?;
                                    hypr.warp(entry_position(bounds, config.edge, along)?)?;
                                    mode = Mode::Receiving(config.edge);
                                }
                                Packet::Event { kind, code, value }
                                    if matches!(mode, Mode::Receiving(_)) =>
                                {
                                    injector.event(kind, code, value)?;
                                    if kind == 2 && code <= 1 {
                                        if code == 0 {
                                            last_motion.0 = value as f64;
                                        } else {
                                            last_motion.1 = value as f64;
                                        }
                                    }
                                }
                                Packet::Leave | Packet::Return { .. } => {
                                    conn.discard_pending();
                                    injector.release()?;
                                    ungrab(&mut devices)?;
                                    if matches!(mode, Mode::Sending) {
                                        let along = match packet {
                                            Packet::Return { along } => along,
                                            _ => u16::MAX / 2,
                                        };
                                        hypr.warp(entry_position(bounds, config.edge, along)?)?;
                                    }
                                    mode = Mode::Local;
                                    cooldown = Instant::now() + Duration::from_millis(300);
                                }
                                Packet::Ping => {}
                                Packet::Event { .. } => {}
                            }
                        }
                    }
                    Err(_) => {
                        injector.release()?;
                        ungrab(&mut devices)?;
                        connection = None;
                        mode = Mode::Local;
                        cooldown = Instant::now() + Duration::from_millis(300);
                    }
                }
            }
            if let Mode::Receiving(edge) = mode
                && let Some(along) = crossing(bounds, edge, hypr.position()?, last_motion)
            {
                injector.release()?;
                if let Some(conn) = &mut connection {
                    conn.send(Packet::Return { along })?;
                    conn.flush()?;
                }
                mode = Mode::Local;
                last_motion = (0.0, 0.0);
                cooldown = Instant::now() + Duration::from_millis(300);
            }
            let mut motion = (0.0, 0.0);
            let mut outgoing = Vec::new();
            for device in &mut devices {
                let events = match device.fetch_events() {
                    Ok(events) => events.collect::<Vec<_>>(),
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => Vec::new(),
                    Err(e) => return Err(e),
                };
                for event in events {
                    let packet = Packet::Event {
                        kind: event.event_type().0,
                        code: event.code(),
                        value: event.value(),
                    };
                    if event.event_type() == EventType::RELATIVE {
                        match event.code() {
                            0 => motion.0 += event.value() as f64,
                            1 => motion.1 += event.value() as f64,
                            _ => {}
                        }
                    }
                    if matches!(mode, Mode::Sending) && packet.validate().is_ok() {
                        outgoing.push(packet);
                    }
                }
            }
            let mut held = BTreeSet::new();
            for device in &devices {
                held.extend(device.get_key_state()?.iter().map(|k| k.code()));
            }
            let escape = held.contains(&KeyCode::KEY_ESC.code())
                && (held.contains(&KeyCode::KEY_LEFTCTRL.code())
                    || held.contains(&KeyCode::KEY_RIGHTCTRL.code()))
                && (held.contains(&KeyCode::KEY_LEFTALT.code())
                    || held.contains(&KeyCode::KEY_RIGHTALT.code()));
            if escape && !matches!(mode, Mode::Local) {
                injector.release()?;
                ungrab(&mut devices)?;
                if let Some(conn) = &mut connection {
                    conn.discard_pending();
                    conn.send(Packet::Leave)?;
                    conn.flush()?;
                }
                mode = Mode::Local;
                cooldown = Instant::now() + Duration::from_millis(500);
            } else if let Some(conn) = &mut connection {
                for packet in outgoing {
                    conn.send(packet)?;
                }
            }
            if matches!(mode, Mode::Local)
                && held.is_empty()
                && Instant::now() >= cooldown
                && motion != (0.0, 0.0)
                && let Some(along) = crossing(bounds, config.edge, hypr.position()?, motion)
            {
                if connection.is_none() {
                    match TcpStream::connect_timeout(&config.peer, Duration::from_millis(500)) {
                        Ok(socket) => connection = Some(ConnectionState::new(socket)?),
                        Err(_) => {
                            cooldown = Instant::now() + Duration::from_secs(1);
                            continue;
                        }
                    }
                }
                for device in &mut devices {
                    device.grab()?;
                }
                if let Some(conn) = &mut connection {
                    conn.send(Packet::Enter {
                        edge: config.edge,
                        along,
                    })?;
                    conn.flush()?;
                }
                mode = Mode::Sending;
            }
            if let Some(conn) = &mut connection
                && conn.flush().is_err()
            {
                injector.release()?;
                ungrab(&mut devices)?;
                connection = None;
                mode = Mode::Local;
                cooldown = Instant::now() + Duration::from_millis(300);
            }
            thread::sleep(Duration::from_millis(4));
        }
        Ok(())
    })();
    if let Some(conn) = &mut connection {
        let _ = conn.send(Packet::Leave);
        let _ = conn.flush();
    }
    let released = injector.release();
    let ungrabbed = ungrab(&mut devices);
    result.and(released).and(ungrabbed)
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    const BOUNDS: Rect = Rect {
        x: -100.0,
        y: -50.0,
        width: 200.0,
        height: 100.0,
    };
    #[test]
    fn all_edges_require_outward_motion_and_map_corners() {
        for (edge, pos, motion, along) in [
            (Edge::Left, (-100.0, -50.0), (-1.0, 0.0), 0),
            (Edge::Right, (99.0, 49.0), (1.0, 0.0), 65535),
            (Edge::Top, (-100.0, -50.0), (0.0, -1.0), 0),
            (Edge::Bottom, (99.0, 49.0), (0.0, 1.0), 65535),
        ] {
            assert_eq!(crossing(BOUNDS, edge, pos, motion), Some(along));
            assert_eq!(crossing(BOUNDS, edge, pos, (-motion.0, -motion.1)), None);
            assert_eq!(crossing(BOUNDS, edge, (0.0, 0.0), motion), None);
            let entry = entry_position(BOUNDS, edge, along).unwrap();
            assert!(entry.0 >= BOUNDS.x && entry.0 < BOUNDS.x + BOUNDS.width);
            assert!(entry.1 >= BOUNDS.y && entry.1 < BOUNDS.y + BOUNDS.height);
            assert_eq!(crossing(BOUNDS, edge, entry, motion), None);
        }
    }
    #[test]
    fn invalid_geometry_and_positions_never_cross() {
        for bounds in [
            Rect {
                width: 0.0,
                ..BOUNDS
            },
            Rect {
                height: f64::NAN,
                ..BOUNDS
            },
        ] {
            assert_eq!(
                crossing(bounds, Edge::Left, (-100.0, 0.0), (-1.0, 0.0)),
                None
            );
            assert!(entry_position(bounds, Edge::Left, 0).is_err());
        }
        assert_eq!(
            crossing(BOUNDS, Edge::Left, (-100.0, 50.0), (-1.0, 0.0)),
            None
        );
        assert_eq!(
            crossing(BOUNDS, Edge::Left, (f64::NAN, 0.0), (-1.0, 0.0)),
            None
        );
    }
    #[test]
    fn monitor_rotation_and_scale_use_logical_coordinates() {
        let monitor = Monitor {
            name: "a".into(),
            x: -500.0,
            y: 0.0,
            width: 2000.0,
            height: 1000.0,
            scale: 2.0,
            transform: 1,
            disabled: false,
        };
        assert_eq!(
            monitor_rect(monitor).unwrap(),
            Rect {
                x: -500.0,
                y: 0.0,
                width: 500.0,
                height: 1000.0
            }
        );
    }
    #[test]
    fn codec_roundtrips_all_packets() {
        let mut packets = vec![Packet::Leave, Packet::Ping, Packet::Return { along: 65535 }];
        for edge in [Edge::Left, Edge::Right, Edge::Top, Edge::Bottom] {
            for along in [0, 32768, 65535] {
                packets.push(Packet::Enter { edge, along });
            }
        }
        for (kind, code, value) in [
            (1, 30, 1),
            (1, 30, 0),
            (1, 30, 2),
            (1, 272, 1),
            (2, 0, -32767),
            (2, 12, 32767),
        ] {
            packets.push(Packet::Event { kind, code, value });
        }
        for packet in packets {
            assert_eq!(Packet::decode(&packet.encode().unwrap()).unwrap(), packet);
        }
        let bytes = Packet::Event {
            kind: 2,
            code: 0,
            value: -2,
        }
        .encode()
        .unwrap();
        assert_eq!(&bytes[10..14], &[255, 255, 255, 254]);
    }
    #[test]
    fn codec_rejects_truncation_unknown_versions_and_invalid_events() {
        let good = Packet::Ping.encode().unwrap();
        for len in 0..16 {
            assert!(Packet::decode(&good[..len]).is_err());
        }
        for index in [0, 4, 5, 6, 14, 15] {
            let mut bad = good;
            bad[index] = 99;
            assert!(Packet::decode(&bad).is_err());
        }
        for packet in [
            Packet::Event {
                kind: 1,
                code: 768,
                value: 1,
            },
            Packet::Event {
                kind: 1,
                code: 30,
                value: 3,
            },
            Packet::Event {
                kind: 3,
                code: 0,
                value: 1,
            },
            Packet::Event {
                kind: 2,
                code: 2,
                value: 1,
            },
        ] {
            assert!(packet.encode().is_err());
        }
    }
    #[test]
    fn tailnet_range_is_exact() {
        for ip in ["100.64.0.1", "100.127.255.254", "fd7a:115c:a1e0::1"] {
            assert!(tailnet(ip.parse().unwrap()));
        }
        for ip in [
            "100.63.255.255",
            "100.128.0.1",
            "127.0.0.1",
            "192.168.1.1",
            "::1",
            "fd7a:115c:a1e1::1",
        ] {
            assert!(!tailnet(ip.parse().unwrap()));
        }
    }
    #[test]
    fn tcp_framing_retains_split_packets_and_drains_multiple_packets() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut writer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut reader = ConnectionState::new(listener.accept().unwrap().0).unwrap();
        let first = Packet::Enter {
            edge: Edge::Left,
            along: 100,
        }
        .encode()
        .unwrap();
        writer.write_all(&first[..5]).unwrap();
        assert!(reader.packets().unwrap().is_empty());
        writer.write_all(&first[5..]).unwrap();
        writer.write_all(&Packet::Leave.encode().unwrap()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(1);
        let mut received = Vec::new();
        while received.len() < 2 && Instant::now() < deadline {
            received.extend(reader.packets().unwrap());
            thread::yield_now();
        }
        assert_eq!(
            received,
            [
                Packet::Enter {
                    edge: Edge::Left,
                    along: 100
                },
                Packet::Leave
            ]
        );
    }

    #[test]
    fn cancellation_finishes_partial_frame_before_leave() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(1))).unwrap();
        let mut connection = ConnectionState::new(listener.accept().unwrap().0).unwrap();
        let event = Packet::Event {
            kind: 1,
            code: 30,
            value: 1,
        };
        connection.send(event).unwrap();
        connection.send(Packet::Ping).unwrap();
        let prefix = &connection.output.front().unwrap()[..5];
        connection.socket.write_all(prefix).unwrap();
        connection.offset = 5;
        connection.discard_pending();
        connection.send(Packet::Leave).unwrap();
        connection.flush().unwrap();
        let mut bytes = [0; 32];
        peer.read_exact(&mut bytes).unwrap();
        assert_eq!(Packet::decode(&bytes[..16]).unwrap(), event);
        assert_eq!(Packet::decode(&bytes[16..]).unwrap(), Packet::Leave);
        assert!(connection.output.is_empty());
        assert_eq!(connection.offset, 0);
    }

    #[test]
    fn disconnected_and_silent_peers_are_detected() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let peer = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let mut connection = ConnectionState::new(listener.accept().unwrap().0).unwrap();
        connection.received = Instant::now() - IDLE_TIMEOUT - Duration::from_secs(1);
        assert_eq!(
            connection.packets().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
        drop(peer);
        assert_eq!(
            connection.packets().unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
    }
}
