use crate::discovery::{
    Advertisement, FEATURES, MODEL, SOURCE_VERSION, display_uuid, txt_records, txt_wire,
};
use crate::pairing::{Identity, Pairing};
use crate::rtsp::{Framer, Request, Response};
use crate::video::{self, Decoder, Sink, VideoOutput};
use crate::{Config, invalid};
use plist::{Dictionary, Value};
use std::io::{self, Cursor, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream, UdpSocket};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, SyncSender, TryRecvError};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Receiver activity and failures.
///
/// Events describe transport activity, not physical display or Apple-device
/// acceptance. Unsupported requests identify the missing protocol operation.
#[derive(Clone, Debug)]
pub enum Event {
    Connected(SocketAddr),
    Disconnected(SocketAddr),
    Unsupported(&'static str),
    Error(String),
}

/// Running receiver.
///
/// Owns the listener, Avahi advertisement, and media child process. Dropping
/// the handle stops the receiver. Events are bounded; slow readers may miss them.
pub struct Handle {
    address: SocketAddr,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<io::Result<()>>>,
    events: Receiver<Event>,
}

impl Handle {
    /// Bound control address.
    ///
    /// Takes no arguments and returns the actual RTSP address and selected port.
    pub fn local_addr(&self) -> SocketAddr {
        self.address
    }

    /// Read the next pending event.
    ///
    /// Takes no arguments and returns an event, an empty queue, or disconnection.
    pub fn try_event(&self) -> Result<Event, TryRecvError> {
        self.events.try_recv()
    }

    /// Stop and join the receiver.
    ///
    /// Takes no arguments, closes session sockets and ffplay, withdraws mDNS,
    /// and returns any worker or advertisement error. Repeated calls are safe.
    pub fn stop(&mut self) -> io::Result<()> {
        self.stop.store(true, Ordering::Release);
        self.worker.take().map_or(Ok(()), |worker| {
            worker
                .join()
                .unwrap_or_else(|_| Err(io::Error::other("AirPlay worker panicked")))
        })
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        let _ = self.stop();
    }
}

/// Start an AirPlay receiver with an ffplay window.
///
/// Takes name, port, and bind address. Returns a stoppable handle or a bind,
/// configuration, or Avahi registration error. Does not replace occupied ports.
/// A window opens only when supported video arrives. FairPlay is unsupported.
pub fn serve(config: Config) -> io::Result<Handle> {
    serve_with_output(config, VideoOutput::Window)
}

/// Start an AirPlay receiver with a selected video destination.
///
/// Takes receiver settings and an ffplay or duplicated-fd destination. Returns
/// a stoppable handle or an initialization error. Requires the system Avahi
/// daemon; this function never binds the mDNS port itself.
pub fn serve_with_output(config: Config, output: VideoOutput) -> io::Result<Handle> {
    if config.name.trim().is_empty()
        || config.name.len() > 63
        || config.name.chars().any(char::is_control)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "AirPlay name must contain 1 to 63 UTF-8 bytes without control characters",
        ));
    }
    let listener = TcpListener::bind(SocketAddr::new(config.bind_address, config.port))?;
    output.validate()?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    let identity = Identity::new();
    let advertisement = Advertisement::publish(&config, address.port(), &identity)?;
    let stop = Arc::new(AtomicBool::new(false));
    let (sender, events) = mpsc::sync_channel(128);
    let worker_stop = stop.clone();
    let worker = thread::Builder::new()
        .name("oc-airplay".into())
        .spawn(move || {
            let result = run(
                listener,
                config,
                identity,
                output,
                worker_stop,
                sender.clone(),
            );
            if let Err(error) = &result {
                let _ = sender.try_send(Event::Error(error.to_string()));
            }
            let cleanup = advertisement.stop();
            result.and(cleanup)
        })?;
    Ok(Handle {
        address,
        stop,
        worker: Some(worker),
        events,
    })
}

fn run(
    listener: TcpListener,
    config: Config,
    identity: Identity,
    output: VideoOutput,
    stop: Arc<AtomicBool>,
    events: SyncSender<Event>,
) -> io::Result<()> {
    while !stop.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((stream, peer)) => {
                let _ = events.try_send(Event::Connected(peer));
                if let Err(error) =
                    connection(stream, peer, &config, &identity, &output, &stop, &events)
                    && !stop.load(Ordering::Acquire)
                {
                    let _ = events.try_send(Event::Error(error.to_string()));
                }
                let _ = events.try_send(Event::Disconnected(peer));
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5))
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

fn connection(
    mut stream: TcpStream,
    peer: SocketAddr,
    config: &Config,
    identity: &Identity,
    output: &VideoOutput,
    stop: &AtomicBool,
    events: &SyncSender<Event>,
) -> io::Result<()> {
    stream.set_nonblocking(true)?;
    stream.set_nodelay(true)?;
    let bind = stream.local_addr()?.ip();
    let mut session = Session::new(config, identity, bind, peer.ip());
    let mut framer = Framer::default();
    let mut buffer = [0; 8192];
    let mut active = Instant::now();
    let mut upgrade = false;
    let mut video = Video::default();
    while !stop.load(Ordering::Acquire) {
        match stream.read(&mut buffer) {
            Ok(0) => {
                return if upgrade {
                    video.finish()
                } else {
                    framer.finish()
                };
            }
            Ok(count) => {
                active = Instant::now();
                if upgrade {
                    video.feed(&buffer[..count], output, &config.name, stop)?;
                } else {
                    framer.push(&buffer[..count])?;
                    while let Some(request) = framer.next()? {
                        if let Some(value) = request.headers.get("session")
                            && (value.split(';').next() != Some("1") || session.transport.is_none())
                        {
                            send(&mut stream, &Response::new(454).encode(&request), stop)?;
                            continue;
                        }
                        let response = session.route(&request, events);
                        send(&mut stream, &response.encode(&request), stop)?;
                        if session.close {
                            return Ok(());
                        }
                        if session.upgrade && response.status == 200 {
                            upgrade = true;
                            video.feed(&framer.take_pending(), output, &config.name, stop)?;
                            break;
                        }
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
        if let Some(transport) = &mut session.transport
            && transport.pump(output, &config.name, stop)?
        {
            active = Instant::now();
        }
        if active.elapsed() > Duration::from_secs(30) {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "AirPlay session idle timeout",
            ));
        }
        thread::sleep(Duration::from_millis(2));
    }
    Ok(())
}

fn send(stream: &mut TcpStream, mut bytes: &[u8], stop: &AtomicBool) -> io::Result<()> {
    let deadline = Instant::now() + Duration::from_secs(2);
    while !bytes.is_empty() {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "Receiver stopped",
            ));
        }
        match stream.write(bytes) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Control connection closed",
                ));
            }
            Ok(size) => bytes = &bytes[size..],
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                thread::sleep(Duration::from_millis(2))
            }
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

struct Session<'a> {
    config: &'a Config,
    identity: &'a Identity,
    bind: IpAddr,
    peer: IpAddr,
    pairing: Pairing,
    transport: Option<Transport>,
    upgrade: bool,
    close: bool,
}

impl<'a> Session<'a> {
    fn new(config: &'a Config, identity: &'a Identity, bind: IpAddr, peer: IpAddr) -> Self {
        Self {
            config,
            identity,
            bind,
            peer,
            pairing: Pairing::default(),
            transport: None,
            upgrade: false,
            close: false,
        }
    }

    fn unsupported(events: &SyncSender<Event>, operation: &'static str) -> Response {
        let _ = events.try_send(Event::Unsupported(operation));
        Response::new(501).body(
            "text/plain",
            format!("Unsupported: {operation}\n").into_bytes(),
        )
    }

    fn route(&mut self, request: &Request, events: &SyncSender<Event>) -> Response {
        match (request.method.as_str(), request.path()) {
            ("OPTIONS", _) => {
                let mut response = Response::new(200);
                response.headers.push((
                    "Public",
                    "OPTIONS, GET, POST, SETUP, RECORD, GET_PARAMETER, TEARDOWN".into(),
                ));
                response
            }
            ("GET", "/info") => plist_response(info(self.config, self.identity)),
            ("GET", "/stream.xml") => {
                let mut bytes = Vec::new();
                if stream_info().to_writer_xml(&mut bytes).is_err() {
                    return Response::new(500);
                }
                Response::new(200).body("text/x-apple-plist+xml", bytes)
            }
            ("POST", "/pair-setup") => {
                if request
                    .headers
                    .get("content-type")
                    .is_some_and(|value| value != "application/octet-stream")
                {
                    return Self::unsupported(events, "HomeKit/PIN pair-setup");
                }
                if self.transport.is_some() {
                    return Response::new(455);
                }
                if request.body.len() != 32 {
                    return Response::new(400);
                }
                Response::new(200).body(
                    "application/octet-stream",
                    self.identity.public_key().to_vec(),
                )
            }
            ("POST", "/pair-verify") => {
                if self.transport.is_some() {
                    return Response::new(455);
                }
                if request
                    .headers
                    .get("content-type")
                    .is_some_and(|value| value != "application/octet-stream")
                {
                    return Self::unsupported(events, "HomeKit pair-verify");
                }
                match self.pairing.exchange(self.identity, &request.body) {
                    Ok(bytes) => Response::new(200).body("application/octet-stream", bytes),
                    Err(_) => Response::new(470),
                }
            }
            ("POST", "/fp-setup") => Self::unsupported(events, "FairPlay SAP and key unwrapping"),
            ("POST", "/auth-setup") => Self::unsupported(events, "MFi authentication"),
            ("SETUP", _) => match read_dictionary(&request.body) {
                Ok(body) => self.setup(body, events),
                Err(_) => Response::new(400),
            },
            ("POST", "/stream") => {
                let Ok(body) = read_dictionary(&request.body) else {
                    return Response::new(400);
                };
                if encrypted(&body) {
                    return Self::unsupported(events, "Encrypted mirror stream");
                }
                if self.transport.is_some() {
                    return Response::new(409);
                }
                self.upgrade = true;
                Response::new(200)
            }
            ("RECORD", _) => {
                if self
                    .transport
                    .as_ref()
                    .is_none_or(|transport| !transport.video_enabled)
                {
                    return Response::new(455);
                }
                let mut response = Response::new(200);
                response.headers.push(("Session", "1".into()));
                response
            }
            ("GET_PARAMETER", _) => {
                if request.body.is_empty() {
                    Response::new(200)
                } else {
                    Self::unsupported(events, "Audio parameters")
                }
            }
            ("POST", "/feedback") => Response::new(200),
            ("TEARDOWN", _) => {
                self.transport = None;
                self.close = true;
                Response::new(200)
            }
            ("ANNOUNCE" | "SET_PARAMETER" | "FLUSH" | "FLUSHBUFFERED", _) => {
                Self::unsupported(events, "Audio transport")
            }
            _ => Response::new(404),
        }
    }

    fn setup(&mut self, body: Dictionary, events: &SyncSender<Event>) -> Response {
        if encrypted(&body) {
            return Self::unsupported(events, "FairPlay encrypted session key");
        }
        if let Some(protocol) = body.get("timingProtocol")
            && protocol.as_string() != Some("NTP")
        {
            return Response::new(461);
        }
        let streams = match body.get("streams") {
            Some(Value::Array(streams)) if streams.len() == 1 => {
                let Some(stream) = streams[0].as_dictionary() else {
                    return Response::new(400);
                };
                if encrypted(stream) {
                    return Self::unsupported(events, "Encrypted mirror stream");
                }
                match stream.get("type").and_then(Value::as_unsigned_integer) {
                    Some(110) => true,
                    Some(96 | 103) => {
                        return Self::unsupported(
                            events,
                            "AAC audio transport (AudioSink hook only)",
                        );
                    }
                    _ => return Response::new(461),
                }
            }
            Some(_) => return Response::new(400),
            None => false,
        };
        if self
            .transport
            .as_ref()
            .is_some_and(|transport| streams && transport.video_enabled)
        {
            return Response::new(409);
        }
        if self.transport.is_none() {
            match Transport::bind(self.bind, self.peer) {
                Ok(transport) => self.transport = Some(transport),
                Err(error) => {
                    let _ = events.try_send(Event::Error(error.to_string()));
                    return Response::new(500);
                }
            }
        }
        let transport = self.transport.as_mut().unwrap();
        if streams {
            transport.video_enabled = true;
        }
        let result = if streams {
            dict([(
                "streams",
                Value::Array(vec![dict([
                    ("type", Value::from(110u64)),
                    ("dataPort", Value::from(transport.video_port as u64)),
                ])]),
            )])
        } else {
            dict([("timingPort", Value::from(transport.timing_port as u64))])
        };
        let mut response = plist_response(result);
        response.headers.push(("Session", "1".into()));
        response
    }
}

fn encrypted(body: &Dictionary) -> bool {
    [
        "ekey",
        "eiv",
        "param1",
        "param2",
        "shk",
        "rsaaeskey",
        "fpaeskey",
    ]
    .iter()
    .any(|key| body.contains_key(key))
        || body
            .get("et")
            .is_some_and(|value| value.as_unsigned_integer() != Some(0))
}

fn read_dictionary(bytes: &[u8]) -> io::Result<Dictionary> {
    Value::from_reader(Cursor::new(bytes))
        .map_err(invalid_plist)?
        .into_dictionary()
        .ok_or_else(|| invalid("Expected a property-list dictionary"))
}

fn invalid_plist(error: plist::Error) -> io::Error {
    invalid(error.to_string())
}

fn dict<const N: usize>(entries: [(&str, Value); N]) -> Value {
    Value::Dictionary(
        entries
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn plist_response(value: Value) -> Response {
    let mut bytes = Vec::new();
    match value.to_writer_binary(&mut bytes) {
        Ok(()) => Response::new(200).body("application/x-apple-binary-plist", bytes),
        Err(_) => Response::new(500),
    }
}

fn stream_info() -> Value {
    dict([
        ("width", Value::from(1920u64)),
        ("height", Value::from(1080u64)),
        ("refreshRate", Value::Real(1.0 / 60.0)),
        ("overscanned", Value::Boolean(false)),
        ("version", Value::from(SOURCE_VERSION)),
    ])
}

fn info(config: &Config, identity: &Identity) -> Value {
    dict([
        ("name", Value::from(config.name.clone())),
        ("deviceID", Value::from(identity.device_id())),
        ("macAddress", Value::from(identity.device_id())),
        ("model", Value::from(MODEL)),
        ("manufacturer", Value::from("Omnicontinuity")),
        ("sourceVersion", Value::from(SOURCE_VERSION)),
        ("features", Value::from(FEATURES)),
        ("statusFlags", Value::from(0u64)),
        ("pk", Value::Data(identity.public_key().to_vec())),
        (
            "txtAirPlay",
            Value::Data(txt_wire(&txt_records(identity)).unwrap()),
        ),
        (
            "displays",
            Value::Array(vec![dict([
                ("uuid", Value::from(display_uuid(identity))),
                ("width", Value::from(1920u64)),
                ("height", Value::from(1080u64)),
                ("widthPixels", Value::from(1920u64)),
                ("heightPixels", Value::from(1080u64)),
                ("refreshRate", Value::Real(1.0 / 60.0)),
                ("maxFPS", Value::from(60u64)),
                ("overscanned", Value::Boolean(false)),
            ])]),
        ),
    ])
}

#[derive(Default)]
struct Video {
    framer: video::Framer,
    decoder: Decoder,
    sink: Option<Sink>,
}

impl Video {
    fn finish(&self) -> io::Result<()> {
        self.framer.finish()
    }

    fn feed(
        &mut self,
        bytes: &[u8],
        output: &VideoOutput,
        name: &str,
        stop: &AtomicBool,
    ) -> io::Result<()> {
        self.framer.push(bytes)?;
        while let Some(packet) = self.framer.next()? {
            let data = self.decoder.decode(packet)?;
            if !data.is_empty() {
                if self.sink.is_none() {
                    self.sink = Some(output.open(name)?);
                }
                self.sink.as_mut().unwrap().write(&data, stop)?;
            }
        }
        Ok(())
    }
}

struct Transport {
    mirror: TcpListener,
    timing: UdpSocket,
    video_port: u16,
    timing_port: u16,
    peer: IpAddr,
    video_enabled: bool,
    video_stream: Option<TcpStream>,
    video: Video,
}

impl Transport {
    fn bind(bind: IpAddr, peer: IpAddr) -> io::Result<Self> {
        let mirror = TcpListener::bind(SocketAddr::new(bind, 0))?;
        let timing = UdpSocket::bind(SocketAddr::new(bind, 0))?;
        mirror.set_nonblocking(true)?;
        timing.set_nonblocking(true)?;
        Ok(Self {
            video_port: mirror.local_addr()?.port(),
            timing_port: timing.local_addr()?.port(),
            mirror,
            timing,
            peer,
            video_enabled: false,
            video_stream: None,
            video: Video::default(),
        })
    }

    fn pump(&mut self, output: &VideoOutput, name: &str, stop: &AtomicBool) -> io::Result<bool> {
        let mut active = false;
        if self.video_enabled && self.video_stream.is_none() {
            self.video_stream = accept_peer(&self.mirror, self.peer)?;
        }
        let mut buffer = [0; 8192];
        if let Some(stream) = &mut self.video_stream {
            match stream.read(&mut buffer) {
                Ok(0) => {
                    self.video.finish()?;
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "Mirror stream closed",
                    ));
                }
                Ok(size) => {
                    self.video.feed(&buffer[..size], output, name, stop)?;
                    active = true;
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error),
            }
        }
        match self.timing.recv_from(&mut buffer) {
            Ok((size, peer)) if peer.ip() == self.peer => {
                if let Some(response) = timing_reply(&buffer[..size], ntp_now(), ntp_now()) {
                    match self.timing.send_to(&response, peer) {
                        Ok(_) => {}
                        Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                        Err(error) => return Err(error),
                    }
                }
            }
            Ok(_) => {}
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => return Err(error),
        }
        Ok(active)
    }
}

fn accept_peer(listener: &TcpListener, peer: IpAddr) -> io::Result<Option<TcpStream>> {
    match listener.accept() {
        Ok((stream, address)) if address.ip() == peer => {
            stream.set_nonblocking(true)?;
            Ok(Some(stream))
        }
        Ok(_) => Ok(None),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
            ) =>
        {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

fn ntp_now() -> u64 {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let seconds = now.as_secs().wrapping_add(2_208_988_800) as u32;
    ((seconds as u64) << 32) | (((now.subsec_nanos() as u64) << 32) / 1_000_000_000)
}

fn timing_reply(request: &[u8], received: u64, sent: u64) -> Option<[u8; 32]> {
    if request.len() != 32 || request[0] >> 6 != 2 || request[1] & 0x7f != 0x52 {
        return None;
    }
    let mut response = [0; 32];
    response[0] = 0x80;
    response[1] = (request[1] & 0x80) | 0x53;
    response[2..4].copy_from_slice(&request[2..4]);
    response[8..16].copy_from_slice(&request[24..32]);
    response[16..24].copy_from_slice(&received.to_be_bytes());
    response[24..32].copy_from_slice(&sent.to_be_bytes());
    Some(response)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;

    fn request(method: &str, path: &str, value: Option<Value>) -> Request {
        let mut body = Vec::new();
        if let Some(value) = value {
            value.to_writer_binary(&mut body).unwrap();
        }
        Request {
            method: method.into(),
            target: path.into(),
            protocol: "RTSP/1.0".into(),
            headers: Default::default(),
            body,
        }
    }

    fn mirror_setup() -> Value {
        dict([(
            "streams",
            Value::Array(vec![dict([
                ("type", Value::from(110u64)),
                ("streamConnectionID", Value::from(u64::MAX)),
            ])]),
        )])
    }

    fn response_value(response: Response) -> Value {
        assert_eq!(response.status, 200);
        assert!(response.body.starts_with(b"bplist00"));
        Value::from_reader(Cursor::new(response.body)).unwrap()
    }

    #[test]
    fn info_txt_and_binary_plist_describe_the_same_receiver() {
        let identity = Identity::new();
        let config = Config {
            name: "m1pro16 <&>".into(),
            ..Config::default()
        };
        let value = response_value(plist_response(info(&config, &identity)));
        let info = value.as_dictionary().unwrap();
        assert_eq!(info["name"].as_string(), Some("m1pro16 <&>"));
        assert_eq!(info["features"].as_unsigned_integer(), Some(FEATURES));
        assert_eq!(info["features"].as_unsigned_integer(), Some(0));
        assert_eq!(info["statusFlags"].as_unsigned_integer(), Some(0));
        assert_eq!(info["model"].as_string(), Some(MODEL));
        assert_eq!(info["sourceVersion"].as_string(), Some(SOURCE_VERSION));
        assert_eq!(info["pk"].as_data(), Some(identity.public_key().as_slice()));
        assert_eq!(
            info["txtAirPlay"].as_data(),
            Some(txt_wire(&txt_records(&identity)).unwrap().as_slice())
        );
    }

    #[test]
    fn setup_allocates_available_ports_and_teardown_releases_them() {
        let identity = Identity::new();
        let config = Config::default();
        let ip = "127.0.0.1".parse().unwrap();
        let mut session = Session::new(&config, &identity, ip, ip);
        let (events, _) = mpsc::sync_channel(10);
        assert_eq!(
            session.route(&request("RECORD", "/", None), &events).status,
            455
        );
        let result = response_value(session.route(&request("SETUP", "/", Some(dict([]))), &events));
        let timing_port = result.as_dictionary().unwrap()["timingPort"]
            .as_unsigned_integer()
            .unwrap() as u16;
        assert!(!result.as_dictionary().unwrap().contains_key("eventPort"));
        assert!(UdpSocket::bind(("127.0.0.1", timing_port)).is_err());
        assert_eq!(
            session.route(&request("RECORD", "/", None), &events).status,
            455
        );
        let result =
            response_value(session.route(&request("SETUP", "/", Some(mirror_setup())), &events));
        let port = result.as_dictionary().unwrap()["streams"]
            .as_array()
            .unwrap()[0]
            .as_dictionary()
            .unwrap()["dataPort"]
            .as_unsigned_integer()
            .unwrap() as u16;
        assert!(TcpListener::bind(("127.0.0.1", port)).is_err());
        assert_eq!(
            session
                .route(&request("SETUP", "/", Some(mirror_setup())), &events)
                .status,
            409
        );
        assert_eq!(
            session.route(&request("RECORD", "/", None), &events).status,
            200
        );
        assert_eq!(
            session
                .route(&request("TEARDOWN", "/", None), &events)
                .status,
            200
        );
        assert!(session.close);
        assert!(TcpListener::bind(("127.0.0.1", port)).is_ok());
        assert!(UdpSocket::bind(("127.0.0.1", timing_port)).is_ok());
    }

    #[test]
    fn encrypted_and_audio_setup_never_claim_success_or_allocate_ports() {
        let identity = Identity::new();
        let config = Config::default();
        let ip = "127.0.0.1".parse().unwrap();
        let mut session = Session::new(&config, &identity, ip, ip);
        let (events, received) = mpsc::sync_channel(10);
        for value in [
            dict([("ekey", Value::Data(vec![0; 72]))]),
            dict([("param1", Value::Data(vec![]))]),
            dict([("et", Value::from(32u64))]),
            dict([(
                "streams",
                Value::Array(vec![dict([("type", Value::from(96u64))])]),
            )]),
        ] {
            assert_eq!(
                session
                    .route(&request("SETUP", "/", Some(value)), &events)
                    .status,
                501
            );
            assert!(session.transport.is_none());
            assert!(matches!(
                received.try_recv().unwrap(),
                Event::Unsupported(_)
            ));
        }
        assert_eq!(
            session
                .route(&request("POST", "/fp-setup", None), &events)
                .status,
            501
        );
        assert!(!session.pairing.verified);
    }

    #[test]
    fn unsupported_volume_parameters_do_not_report_a_working_audio_output() {
        let identity = Identity::new();
        let config = Config::default();
        let ip = "127.0.0.1".parse().unwrap();
        let mut session = Session::new(&config, &identity, ip, ip);
        let (events, received) = mpsc::sync_channel(2);
        let mut volume = request("GET_PARAMETER", "/", None);
        volume.body = b"volume\r\n".to_vec();
        let response = session.route(&volume, &events);
        assert_eq!(response.status, 501);
        assert!(!response.body.windows(7).any(|bytes| bytes == b"volume:"));
        assert!(matches!(
            received.try_recv().unwrap(),
            Event::Unsupported("Audio parameters")
        ));
        assert_eq!(
            session
                .route(&request("GET_PARAMETER", "/", None), &events)
                .status,
            200
        );
    }

    #[test]
    fn wrong_stream_type_or_malformed_plist_fails() {
        let identity = Identity::new();
        let config = Config::default();
        let ip = "127.0.0.1".parse().unwrap();
        let mut session = Session::new(&config, &identity, ip, ip);
        let (events, _) = mpsc::sync_channel(10);
        for value in [
            Value::Array(vec![]),
            dict([("streams", Value::from(110u64))]),
        ] {
            assert_eq!(
                session
                    .route(&request("SETUP", "/", Some(value)), &events)
                    .status,
                400
            );
        }
        assert_eq!(
            session
                .route(
                    &request(
                        "SETUP",
                        "/",
                        Some(dict([("timingProtocol", Value::from("PTP"))]))
                    ),
                    &events
                )
                .status,
            461
        );
        assert!(session.transport.is_none());
    }

    #[test]
    fn occupied_control_port_is_an_error_before_advertising() {
        let occupied = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = occupied.local_addr().unwrap();
        let result = serve(Config {
            name: "test".into(),
            port: address.port(),
            bind_address: address.ip(),
        });
        assert_eq!(result.err().unwrap().kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn timing_response_preserves_sequence_and_origin_with_network_byte_order() {
        let mut query = [0; 32];
        query[0] = 0x80;
        query[1] = 0xd2;
        query[2..4].copy_from_slice(&[0x12, 0x34]);
        query[24..32].copy_from_slice(&0x123456789abcdef0u64.to_be_bytes());
        let response = timing_reply(&query, 0x1122334455667788, 0xaabbccddeeff0011).unwrap();
        assert_eq!(&response[..4], &[0x80, 0xd3, 0x12, 0x34]);
        assert_eq!(&response[8..16], &query[24..32]);
        assert_eq!(&response[16..24], &0x1122334455667788u64.to_be_bytes());
        assert_eq!(&response[24..32], &0xaabbccddeeff0011u64.to_be_bytes());
        assert!(timing_reply(&query[..31], 0, 0).is_none());
        query[1] = 0xd3;
        assert!(timing_reply(&query, 0, 0).is_none());
    }

    fn mirror_packet(kind: u16, payload: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0; 128];
        bytes[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes[4..6].copy_from_slice(&kind.to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn finite_loopback_session_delivers_h264_and_stops_with_idle_connections() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let address = listener.local_addr().unwrap();
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let output = VideoOutput::from_fd(writer.as_fd()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let (events, _) = mpsc::sync_channel(10);
        let worker = thread::spawn(move || {
            run(
                listener,
                Config::default(),
                Identity::new(),
                output,
                worker_stop,
                events,
            )
        });
        let mut handle = Handle {
            address,
            stop,
            worker: Some(worker),
            events: mpsc::sync_channel(1).1,
        };
        let mut control = TcpStream::connect(address).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut setup = Vec::new();
        mirror_setup().to_writer_binary(&mut setup).unwrap();
        control
            .write_all(
                format!(
                    "SETUP / RTSP/1.0\r\nCSeq: 1\r\nContent-Length: {}\r\n\r\n",
                    setup.len()
                )
                .as_bytes(),
            )
            .unwrap();
        control.write_all(&setup).unwrap();
        let response = read_response(&mut control);
        let value = Value::from_reader(Cursor::new(response)).unwrap();
        let port = value.as_dictionary().unwrap()["streams"]
            .as_array()
            .unwrap()[0]
            .as_dictionary()
            .unwrap()["dataPort"]
            .as_unsigned_integer()
            .unwrap() as u16;
        let mut mirror = TcpStream::connect(("127.0.0.1", port)).unwrap();
        let codec = [
            1, 66, 0, 31, 255, 225, 0, 3, 0x67, 0x42, 0, 1, 0, 2, 0x68, 1,
        ];
        mirror.write_all(&mirror_packet(1, &codec)).unwrap();
        mirror
            .write_all(&mirror_packet(0, &[0, 0, 0, 2, 0x65, 9]))
            .unwrap();
        let mut bytes = [0; 19];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(
            bytes,
            [
                0, 0, 0, 1, 0x67, 0x42, 0, 0, 0, 0, 1, 0x68, 1, 0, 0, 0, 1, 0x65, 9
            ]
        );
        let started = Instant::now();
        handle.stop().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        handle.stop().unwrap();
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(50)).is_err());
    }

    #[test]
    fn stream_upgrade_keeps_binary_bytes_received_with_handshake() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let address = listener.local_addr().unwrap();
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let output = VideoOutput::from_fd(writer.as_fd()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = stop.clone();
        let worker = thread::spawn(move || {
            let (stream, peer) = listener.accept().unwrap();
            let (events, _) = mpsc::sync_channel(10);
            connection(
                stream,
                peer,
                &Config::default(),
                &Identity::new(),
                &output,
                &worker_stop,
                &events,
            )
        });
        let mut handle = Handle {
            address,
            stop,
            worker: Some(worker),
            events: mpsc::sync_channel(1).1,
        };
        let mut control = TcpStream::connect(address).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut body = Vec::new();
        dict([]).to_writer_binary(&mut body).unwrap();
        let mut wire = format!(
            "POST /stream HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        )
        .into_bytes();
        wire.extend_from_slice(&body);
        wire.extend_from_slice(&mirror_packet(
            1,
            &[
                1, 66, 0, 31, 255, 225, 0, 3, 0x67, 0x42, 0, 1, 0, 2, 0x68, 1,
            ],
        ));
        control.write_all(&wire).unwrap();
        assert!(read_response(&mut control).is_empty());
        let mut bytes = [0; 13];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(bytes[4], 0x67);
        assert_eq!(bytes[11], 0x68);
        handle.stop().unwrap();
    }

    fn read_response(stream: &mut TcpStream) -> Vec<u8> {
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            head.push(byte[0]);
            assert!(head.len() < 16 * 1024);
        }
        let head = String::from_utf8(head).unwrap();
        assert!(head.contains("200 OK"), "{head}");
        let size = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse::<usize>()
            .unwrap();
        let mut body = vec![0; size];
        stream.read_exact(&mut body).unwrap();
        body
    }
}

#[cfg(test)]
mod host_tests {
    use super::*;
    use std::os::fd::AsFd;
    use std::os::unix::net::UnixStream;
    use std::process::{Command, Stdio};

    #[test]
    #[ignore = "requires the host Avahi daemon and a multicast interface; finite registration test"]
    fn host_avahi_resolves_actual_port_txt_and_control_then_stops() {
        let (writer, _reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        let name = format!("oc-airplay-test-{}", std::process::id());
        let mut handle = serve_with_output(
            Config {
                name: name.clone(),
                ..Config::default()
            },
            VideoOutput::from_fd(writer.as_fd()).unwrap(),
        )
        .unwrap();
        let connection = zbus::blocking::connection::Builder::system()
            .unwrap()
            .method_timeout(Duration::from_secs(3))
            .build()
            .unwrap();
        let server = zbus::blocking::Proxy::new(
            &connection,
            "org.freedesktop.Avahi",
            "/",
            "org.freedesktop.Avahi.Server",
        )
        .unwrap();
        type Resolved = (
            i32,
            i32,
            String,
            String,
            String,
            String,
            i32,
            String,
            u16,
            Vec<Vec<u8>>,
            u32,
        );
        let resolved: Resolved = server
            .call(
                "ResolveService",
                &(
                    -1i32,
                    0i32,
                    name.as_str(),
                    "_airplay._tcp",
                    "local",
                    0i32,
                    0u32,
                ),
            )
            .unwrap();
        assert_eq!(resolved.2, name);
        assert_eq!(resolved.8, handle.local_addr().port());
        assert!(resolved.5.starts_with("oc-airplay-"));
        let address = SocketAddr::new(resolved.7.parse().unwrap(), resolved.8);
        let mut control = TcpStream::connect_timeout(&address, Duration::from_secs(2)).unwrap();
        control
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        control
            .write_all(b"GET /info RTSP/1.0\r\nCSeq: 1\r\n\r\n")
            .unwrap();
        let mut response = Vec::new();
        while !response.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            control.read_exact(&mut byte).unwrap();
            response.push(byte[0]);
            assert!(response.len() < 16384);
        }
        let head = String::from_utf8(response).unwrap();
        assert!(head.starts_with("RTSP/1.0 200 OK"));
        let size = head
            .lines()
            .find_map(|line| line.strip_prefix("Content-Length: "))
            .unwrap()
            .parse()
            .unwrap();
        let mut bytes = vec![0; size];
        control.read_exact(&mut bytes).unwrap();
        let info = read_dictionary(&bytes).unwrap();
        let actual_txt = info["txtAirPlay"].as_data().unwrap();
        let mut expected_records = resolved.9;
        let mut actual_records = Vec::new();
        let mut remaining = actual_txt;
        while !remaining.is_empty() {
            let size = remaining[0] as usize;
            actual_records.push(remaining[1..size + 1].to_vec());
            remaining = &remaining[size + 1..];
        }
        expected_records.sort();
        actual_records.sort();
        assert_eq!(expected_records, actual_records);
        handle.stop().unwrap();
        assert!(TcpStream::connect_timeout(&address, Duration::from_millis(100)).is_err());
    }

    #[test]
    #[ignore = "requires ffmpeg with libx264; finite encode/transport/decode test"]
    fn host_ffmpeg_decodes_the_h264_delivered_to_the_caller_fd() {
        let encoded = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "lavfi",
                "-i",
                "color=c=red:s=64x48:r=2",
                "-frames:v",
                "2",
                "-c:v",
                "libx264",
                "-preset",
                "ultrafast",
                "-tune",
                "zerolatency",
                "-f",
                "h264",
                "pipe:1",
            ])
            .output()
            .unwrap();
        assert!(
            encoded.status.success(),
            "{}",
            String::from_utf8_lossy(&encoded.stderr)
        );
        let mut starts = Vec::new();
        let mut cursor = 0;
        while cursor + 3 <= encoded.stdout.len() {
            let rest = &encoded.stdout[cursor..];
            let length = if rest.starts_with(&[0, 0, 0, 1]) {
                4
            } else if rest.starts_with(&[0, 0, 1]) {
                3
            } else {
                cursor += 1;
                continue;
            };
            starts.push((cursor, length));
            cursor += length;
        }
        let nals = starts
            .iter()
            .enumerate()
            .map(|(index, (start, length))| {
                let end = starts
                    .get(index + 1)
                    .map_or(encoded.stdout.len(), |next| next.0);
                &encoded.stdout[start + length..end]
            })
            .collect::<Vec<_>>();
        let sps = nals.iter().find(|nal| nal[0] & 31 == 7).unwrap();
        let pps = nals.iter().find(|nal| nal[0] & 31 == 8).unwrap();
        let mut codec = vec![1, sps[1], sps[2], sps[3], 255, 225];
        codec.extend_from_slice(&(sps.len() as u16).to_be_bytes());
        codec.extend_from_slice(sps);
        codec.push(1);
        codec.extend_from_slice(&(pps.len() as u16).to_be_bytes());
        codec.extend_from_slice(pps);
        let mut frame = Vec::new();
        for nal in nals.iter().filter(|nal| !matches!(nal[0] & 31, 7 | 8)) {
            frame.extend_from_slice(&(nal.len() as u32).to_be_bytes());
            frame.extend_from_slice(nal);
        }
        let packet = |kind: u16, payload: &[u8]| {
            let mut bytes = vec![0; 128];
            bytes[..4].copy_from_slice(&(payload.len() as u32).to_le_bytes());
            bytes[4..6].copy_from_slice(&kind.to_le_bytes());
            bytes.extend_from_slice(payload);
            bytes
        };
        let (writer, mut reader) = UnixStream::pair().unwrap();
        writer.set_nonblocking(true).unwrap();
        reader
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let output = VideoOutput::from_fd(writer.as_fd()).unwrap();
        let mut video = Video::default();
        video
            .feed(&packet(1, &codec), &output, "test", &AtomicBool::new(false))
            .unwrap();
        video
            .feed(&packet(0, &frame), &output, "test", &AtomicBool::new(false))
            .unwrap();
        drop(video);
        drop(output);
        drop(writer);
        let mut received = Vec::new();
        reader.read_to_end(&mut received).unwrap();
        assert!(!received.is_empty());
        assert_eq!(decode(&encoded.stdout), decode(&received));
    }

    fn decode(bytes: &[u8]) -> Vec<u8> {
        let mut child = Command::new("ffmpeg")
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-f",
                "h264",
                "-i",
                "pipe:0",
                "-frames:v",
                "2",
                "-f",
                "rawvideo",
                "-pix_fmt",
                "rgb24",
                "pipe:1",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(bytes).unwrap();
        let result = child.wait_with_output().unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(result.stdout.len(), 64 * 48 * 3 * 2);
        result.stdout
    }
}
