//! AirDrop on an already configured Linux `awdl0` interface.
//!
//! Calls block. Receiving explicitly accepts one file transfer in Everyone mode;
//! identities are ephemeral and do not claim Apple ID or Contacts Only support.
//! No helper process, privilege escalation, BLE or interface configuration is used.
//! Requires the system OpenSSL and libarchive libraries.
//!
//! Framing references: the installed Omdrop MIT helpers (Brent Kearney, 2026),
//! `/usr/lib/omdrop/airdrop-send.py`, `awdl-airdrop-adv.py`,
//! `awdl-mdns-respond.py`, and the plugin's `bin/airdrop-serve.py`.
//! Tests use independently encoded fixtures, memory streams and loopback TLS.

mod archive;
mod dns;
mod http;
mod transport;

use plist::{Dictionary, Value};
use std::{
    collections::HashSet,
    fs::File,
    io::{BufReader, Cursor, Read, Seek, Write},
    net::SocketAddrV6,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use uuid::Uuid;

pub use transport::Interface;

/// An AirDrop operation failure, including interface, peer and protocol failures.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("awdl0 is unavailable: {0}; configure it outside oc-drop")]
    Interface(String),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("TLS: {0}")]
    Tls(#[from] openssl::error::ErrorStack),
    #[error("property list: {0}")]
    Plist(#[from] plist::Error),
    #[error("AirDrop protocol: {0}")]
    Protocol(String),
    #[error("no visible AirDrop peer named {0:?}")]
    PeerNotFound(String),
    #[error("several peers match {0:?}; use a unique full name")]
    AmbiguousPeer(String),
    #[error("peer refused {stage} (HTTP {status})")]
    Refused { stage: &'static str, status: u16 },
    #[error("AirDrop receive timed out")]
    Timeout,
}

/// The result returned by this crate's blocking operations.
pub type Result<T> = std::result::Result<T, Error>;

const OP_TIMEOUT: Duration = Duration::from_secs(20);
const SMALL_LIMIT: u64 = 1024 * 1024;
const MAX_FILES: usize = 4096;

/// Receive settings. The call accepts one transfer without another consent prompt.
#[derive(Clone, Debug)]
pub struct ReceiveOptions {
    /// Display name returned by Discover and Ask.
    pub name: String,
    /// Time allowed for discovery and delivery, including active connections.
    pub timeout: Duration,
    /// Maximum encoded, decoded and extracted bytes, each bounded separately.
    pub max_bytes: u64,
    /// TCP port advertised on awdl0. The installed firewall permits 8771.
    pub port: u16,
}

impl Default for ReceiveOptions {
    fn default() -> Self {
        Self {
            name: local_name(),
            timeout: Duration::from_secs(120),
            max_bytes: 1024 * 1024 * 1024,
            port: 8771,
        }
    }
}

/// List names currently answering Discover on awdl0.
///
/// Takes no arguments. Returns unique names found during a five second mDNS
/// browse. Discovery may take up to twenty seconds per advertised endpoint.
/// Returns a clear error if awdl0 is down or lacks a link-local IPv6 address.
pub fn peers() -> Result<Vec<String>> {
    let interface = Interface::awdl()?;
    let identity = transport::Identity::new()?;
    let mut names: Vec<_> = visible(&interface, &identity)?
        .into_iter()
        .map(|p| p.name)
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// Send files as one transfer to a visible AirDrop name.
///
/// `name` matches case insensitively, preferring an exact name over a substring;
/// `paths` contains regular files with distinct UTF-8 base names. Directories
/// must be archived first. Returns only after Upload receives HTTP 200.
/// The peer must accept self-signed identities (Everyone mode).
pub fn send<P: AsRef<Path>>(name: &str, paths: &[P]) -> Result<()> {
    let interface = Interface::awdl()?;
    let files = files(paths)?;
    let identity = transport::Identity::new()?;
    let peer = choose_peer(name, visible(&interface, &identity)?)?;
    let stream = identity.connect(&interface, peer.address, Duration::from_secs(90))?;
    let mut connection = BufReader::new(stream);
    let transfer = Uuid::new_v4().to_string().to_uppercase();
    let ask = ask_body(&interface.service_id, &local_name(), &transfer, &files)?;
    http::post(connection.get_mut(), "/Ask", &peer.address, &ask)?;
    let response = http::read_message(&mut connection, SMALL_LIMIT)?;
    require_ok("Ask", response.status()?)?;
    connection
        .get_ref()
        .get_ref()
        .set_read_timeout(Some(OP_TIMEOUT))?;
    connection
        .get_ref()
        .get_ref()
        .set_write_timeout(Some(OP_TIMEOUT))?;
    let total = files.iter().try_fold(0u64, |sum, file| {
        sum.checked_add(file.size)
            .ok_or_else(|| protocol("file sizes overflow"))
    })?;
    http::upload_headers(connection.get_mut(), &peer.address, total, &transfer)?;
    {
        let chunked = http::ChunkedWriter::new(connection.get_mut());
        let mut dvzip = archive::DvzipWriter::new(chunked);
        archive::write_cpio(&mut dvzip, files)?;
        dvzip.finish()?.finish()?;
    }
    let response = http::read_message(&mut connection, SMALL_LIMIT)?;
    require_ok("Upload", response.status()?)
}

/// Receive one file transfer into a caller chosen directory.
///
/// `directory` is created if needed. Returns paths published there, preserving
/// existing files by adding numeric suffixes. Accepts Everyone-mode senders for
/// up to two minutes, with a 1 GiB limit; does not open received files or URLs.
/// File transfers only are advertised. Use `receive_with_options` for limits.
pub fn receive(directory: impl AsRef<Path>) -> Result<Vec<PathBuf>> {
    receive_with_options(directory, ReceiveOptions::default())
}

/// Receive one transfer with explicit display name, port, byte and time limits.
///
/// `directory` receives the files; `options` controls the receiving window.
/// Returns paths after complete validation and extraction. A failed transfer
/// removes its outputs. This call is explicit consent to receive files.
pub fn receive_with_options(
    directory: impl AsRef<Path>,
    options: ReceiveOptions,
) -> Result<Vec<PathBuf>> {
    let interface = Interface::awdl()?;
    if options.name.is_empty()
        || options.name.len() > 255
        || options.max_bytes == 0
        || options.timeout.is_zero()
    {
        return Err(protocol("invalid receive name, timeout or byte limit"));
    }
    let directory = directory.as_ref();
    std::fs::create_dir_all(directory)?;
    let directory = std::fs::canonicalize(directory)?;
    let staging = tempfile::tempdir_in(&directory)?;
    let identity = transport::Identity::new()?;
    let listener = interface.listen(options.port)?;
    let port = listener.local_addr()?.port();
    let advertisement = dns::Advertisement::start(&interface, port)?;
    let deadline = Instant::now()
        .checked_add(options.timeout)
        .ok_or_else(|| protocol("receive timeout overflow"))?;
    while Instant::now() < deadline {
        let (socket, remote) = match listener.accept() {
            Ok(connection) => connection,
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(e) => return Err(e.into()),
        };
        if !matches!(remote, std::net::SocketAddr::V6(addr) if addr.ip().is_unicast_link_local() && addr.scope_id() == interface.index)
        {
            continue;
        }
        let remaining = deadline
            .saturating_duration_since(Instant::now())
            .min(OP_TIMEOUT)
            .max(Duration::from_millis(1));
        socket.set_read_timeout(Some(remaining))?;
        socket.set_write_timeout(Some(remaining))?;
        let stream = match identity.accept(socket, deadline) {
            Ok(stream) => stream,
            Err(_) => continue,
        };
        if let Some(paths) = serve_connection(stream, &directory, staging.path(), &options)? {
            drop(advertisement);
            return Ok(paths);
        }
    }
    Err(Error::Timeout)
}

fn serve_connection<S: Read + Write>(
    stream: S,
    directory: &Path,
    staging: &Path,
    options: &ReceiveOptions,
) -> Result<Option<Vec<PathBuf>>> {
    let mut stream = BufReader::new(stream);
    let mut accepted: Option<String> = None;
    loop {
        let head = match http::read_head(&mut stream) {
            Ok(head) => head,
            Err(_) => return Ok(None),
        };
        let path = match head.request_path() {
            Ok(path) => path,
            Err(_) => {
                http::answer(stream.get_mut(), 400, &[])?;
                return Ok(None);
            }
        };
        if path == "/Upload" {
            let Some(ref transfer) = accepted else {
                http::answer(stream.get_mut(), 403, &[])?;
                return Ok(None);
            };
            if head.header("transferid") != Some(transfer)
                || head
                    .header("content-type")
                    .map(|v| v.split(';').next().unwrap_or("").trim())
                    != Some("application/x-dvzip")
            {
                http::answer(stream.get_mut(), 400, &[])?;
                return Ok(None);
            }
            let result = (|| {
                http::validate_framing(&head, options.max_bytes)?;
                http::continue_request(stream.get_mut(), &head)?;
                let mut raw = tempfile::tempfile_in(staging)?;
                http::read_body(&mut stream, &head, &mut raw, options.max_bytes)?;
                raw.rewind()?;
                let mut cpio = tempfile::tempfile_in(staging)?;
                archive::decode_dvzip(raw, &mut cpio, options.max_bytes)?;
                cpio.rewind()?;
                archive::extract(&cpio, directory, staging, options.max_bytes)
            })();
            match result {
                Ok(paths) => {
                    if let Err(e) = http::answer(stream.get_mut(), 200, &[]) {
                        for path in &paths {
                            let _ = std::fs::remove_file(path);
                        }
                        return Err(e);
                    }
                    return Ok(Some(paths));
                }
                Err(_) => {
                    http::answer(stream.get_mut(), 422, &[])?;
                    return Ok(None);
                }
            }
        }
        if path != "/Discover" && path != "/Ask" {
            http::answer(stream.get_mut(), 404, &[])?;
            return Ok(None);
        }
        let mut body = Vec::new();
        if http::validate_framing(&head, SMALL_LIMIT).is_err() {
            http::answer(stream.get_mut(), 400, &[])?;
            return Ok(None);
        }
        http::continue_request(stream.get_mut(), &head)?;
        if http::read_body(&mut stream, &head, &mut body, SMALL_LIMIT).is_err() {
            http::answer(stream.get_mut(), 400, &[])?;
            return Ok(None);
        }
        let value = match Value::from_reader(Cursor::new(body)) {
            Ok(Value::Dictionary(value)) => Value::Dictionary(value),
            _ => {
                http::answer(stream.get_mut(), 400, &[])?;
                return Ok(None);
            }
        };
        let response = receiver_body(&options.name)?;
        if path == "/Discover" {
            http::answer(stream.get_mut(), 200, &response)?;
            return Ok(None);
        }
        match validate_ask(&value, options.max_bytes) {
            Ok(transfer) if accepted.is_none() => accepted = Some(transfer),
            _ => {
                http::answer(stream.get_mut(), 400, &[])?;
                return Ok(None);
            }
        }
        http::answer(stream.get_mut(), 200, &response)?;
    }
}

fn validate_ask(value: &Value, limit: u64) -> Result<String> {
    let transfer = value
        .get("TransferID")
        .and_then(|v| v.get("id"))
        .and_then(Value::as_string)
        .ok_or_else(|| protocol("Ask has no TransferID"))?;
    Uuid::parse_str(transfer).map_err(|_| protocol("invalid TransferID"))?;
    if value
        .get("TransferType")
        .and_then(|v| v.get("files"))
        .and_then(Value::as_dictionary)
        .is_none()
    {
        return Err(protocol("only file transfers are supported"));
    }
    let files = value
        .get("Files")
        .and_then(Value::as_array)
        .filter(|v| !v.is_empty() && v.len() <= MAX_FILES)
        .ok_or_else(|| protocol("Ask has no files or too many files"))?;
    let mut total = 0u64;
    for file in files {
        if file.get("FileIsDirectory").and_then(Value::as_boolean) == Some(true) {
            return Err(protocol("archive directories before sending"));
        }
        let size = file
            .get("FileSize")
            .and_then(Value::as_unsigned_integer)
            .ok_or_else(|| protocol("Ask has no FileSize"))?;
        total = total
            .checked_add(size)
            .filter(|&size| size <= limit)
            .ok_or_else(|| protocol("Ask exceeds receive limit"))?;
    }
    Ok(transfer.to_owned())
}

#[derive(Debug)]
struct Peer {
    name: String,
    address: SocketAddrV6,
}

fn visible(interface: &Interface, identity: &transport::Identity) -> Result<Vec<Peer>> {
    let endpoints = dns::browse(interface, Duration::from_secs(5))?;
    let mut peers = Vec::new();
    for endpoint in endpoints {
        let result = (|| {
            let stream = identity.connect(interface, endpoint, OP_TIMEOUT)?;
            let mut stream = BufReader::new(stream);
            http::post(stream.get_mut(), "/Discover", &endpoint, &discover_body()?)?;
            let response = http::read_message(&mut stream, SMALL_LIMIT)?;
            require_ok("Discover", response.status()?)?;
            let value = Value::from_reader(Cursor::new(response.body))?;
            Ok::<_, Error>(
                value
                    .get("ReceiverComputerName")
                    .and_then(Value::as_string)
                    .map(str::to_owned),
            )
        })();
        if let Ok(Some(name)) = result {
            peers.push(Peer {
                name,
                address: endpoint,
            });
        }
    }
    Ok(peers)
}

fn choose_peer(name: &str, peers: Vec<Peer>) -> Result<Peer> {
    if name.trim().is_empty() {
        return Err(Error::PeerNotFound(name.into()));
    }
    let name_lower = name.to_lowercase();
    let exact = peers.iter().any(|p| p.name.to_lowercase() == name_lower);
    let mut matches = peers.into_iter().filter(|p| {
        if exact {
            p.name.to_lowercase() == name_lower
        } else {
            p.name.to_lowercase().contains(&name_lower)
        }
    });
    let peer = matches
        .next()
        .ok_or_else(|| Error::PeerNotFound(name.into()))?;
    if matches.next().is_some() {
        return Err(Error::AmbiguousPeer(name.into()));
    }
    Ok(peer)
}

struct SendFile {
    file: File,
    name: String,
    size: u64,
}

fn files<P: AsRef<Path>>(paths: &[P]) -> Result<Vec<SendFile>> {
    if paths.is_empty() || paths.len() > MAX_FILES {
        return Err(protocol("provide 1 to 4096 files"));
    }
    let mut names = HashSet::new();
    paths
        .iter()
        .map(|path| {
            let path = path.as_ref();
            let name = path
                .file_name()
                .and_then(|v| v.to_str())
                .filter(|v| !v.contains(['\r', '\n', '\0', '\\']))
                .ok_or_else(|| protocol("file needs a valid UTF-8 base name"))?
                .to_owned();
            if name.starts_with("._") || !names.insert(name.clone()) {
                return Err(protocol("duplicate or AppleDouble file name"));
            }
            let file = File::open(path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() {
                return Err(protocol(
                    "only regular files can be sent; archive folders first",
                ));
            }
            Ok(SendFile {
                file,
                name,
                size: metadata.len(),
            })
        })
        .collect()
}

fn dict<const N: usize>(pairs: [(&str, Value); N]) -> Value {
    Value::Dictionary(
        pairs
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect::<Dictionary>(),
    )
}
fn binary(value: Value) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    value.to_writer_binary(&mut bytes)?;
    Ok(bytes)
}
fn discover_body() -> Result<Vec<u8>> {
    binary(dict([(
        "DeviceSupportFlags",
        Value::Integer(111611u64.into()),
    )]))
}
fn receiver_body(name: &str) -> Result<Vec<u8>> {
    binary(dict([
        ("ReceiverComputerName", name.into()),
        ("ReceiverModelName", "MacBookPro18,3".into()),
        (
            "ReceiverMediaCapabilities",
            Value::Data(b"{\"Version\": 1}".to_vec()),
        ),
    ]))
}
fn ask_body(sender_id: &str, name: &str, transfer: &str, files: &[SendFile]) -> Result<Vec<u8>> {
    let entries = files
        .iter()
        .map(|f| {
            dict([
                ("FileName", f.name.clone().into()),
                ("FileType", uti(&f.name).into()),
                ("FileSize", Value::Integer(f.size.into())),
                ("FileBomPath", format!("./{}", f.name).into()),
                ("FileIsDirectory", false.into()),
                ("ConvertMediaFormats", false.into()),
            ])
        })
        .collect();
    binary(dict([
        ("TransferID", dict([("id", transfer.into())])),
        ("TransferType", dict([("files", dict([]))])),
        ("SenderID", sender_id.into()),
        ("BundleID", "com.apple.finder".into()),
        ("SenderComputerName", name.into()),
        ("SenderModelName", "MacBookPro18,3".into()),
        ("Items", Value::Array(vec![])),
        ("Files", Value::Array(entries)),
        ("ConvertMediaFormats", false.into()),
    ]))
}
fn uti(name: &str) -> &'static str {
    match Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "jpg" | "jpeg" => "public.jpeg",
        "png" => "public.png",
        "heic" => "public.heic",
        "mov" => "com.apple.quicktime-movie",
        "mp4" => "public.mpeg-4",
        "pdf" => "com.adobe.pdf",
        "txt" => "public.plain-text",
        _ => "public.data",
    }
}
fn local_name() -> String {
    std::fs::read_to_string("/proc/sys/kernel/hostname")
        .map(|s| s.trim().to_owned())
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "Omnicontinuity".into())
}
fn protocol(message: impl Into<String>) -> Error {
    Error::Protocol(message.into())
}
fn require_ok(stage: &'static str, status: u16) -> Result<()> {
    if status == 200 {
        Ok(())
    } else {
        Err(Error::Refused { stage, status })
    }
}

trait ValueExt {
    fn get(&self, key: &str) -> Option<&Value>;
}
impl ValueExt for Value {
    fn get(&self, key: &str) -> Option<&Value> {
        self.as_dictionary()?.get(key)
    }
}

#[cfg(test)]
mod tests;
