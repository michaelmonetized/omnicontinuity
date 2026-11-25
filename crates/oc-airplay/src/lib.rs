//! Clean-room AirPlay receiver built from public protocol descriptions.
//!
//! Discovery uses the system Avahi daemon without opening UDP 5353. Legacy
//! pair-verify and unencrypted H.264 mirroring are implemented. FairPlay key
//! exchange and HomeKit pairing are explicitly unsupported: current Apple
//! devices cannot complete encrypted screen mirroring with this crate yet.
//! The mDNS and `/info` feature masks are zero: this experimental transport
//! does not advertise an Apple-compatible screen or audio receiver. Apple
//! devices may consequently omit it from their AirPlay destination lists.
//!
//! Protocol references:
//! <https://openairplay.github.io/airplay-spec/> and
//! <https://jokerping.github.io/2022/08/03/airplay接收端开发/>.
//! No receiver implementation source or proprietary key tables were used.
//!
//! ```no_run
//! use oc_airplay::{Config, serve};
//!
//! # fn main() -> std::io::Result<()> {
//! let mut receiver = serve(Config::default())?;
//! println!("AirPlay control: {}", receiver.local_addr());
//! // Keep the handle alive for the caller's event loop.
//! receiver.stop()?;
//! # Ok(())
//! # }
//! ```
//!
//! For raw H.264, pass `VideoOutput::from_fd(file.as_fd())?` to
//! `serve_with_output`. Regular files may block; pipes and sockets must remain
//! nonblocking for the lifetime of the receiver. Output is Annex B H.264,
//! including SPS/PPS on each codec change. Audio currently has a typed
//! `AudioSink` contract only; it is neither received nor advertised.
//!
//! A receiver serves one sender at a time, has a 30-second idle timeout, and
//! does not schedule frames against NTP timestamps. Window output requires
//! ffplay in PATH; it is spawned lazily. There is no PIN or remembered-device
//! access control. Do not treat legacy pair-verify as a device allowlist.
//! Event-channel notifications are unsupported and no event port is offered.
//! Published addresses are selected at startup; restart after LAN changes.

mod discovery;
mod pairing;
mod rtsp;
mod server;
pub mod video;

pub use server::{Event, Handle, serve, serve_with_output};
pub use video::VideoOutput;

use std::io;
use std::net::{IpAddr, Ipv4Addr};

/// Receiver settings.
///
/// `name` is the advertised name, `port` is the RTSP port (zero asks the OS),
/// and `bind_address` selects the local address. Default returns the hostname,
/// an available port, and all IPv4 interfaces.
#[derive(Clone, Debug)]
pub struct Config {
    pub name: String,
    pub port: u16,
    pub bind_address: IpAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: hostname::get()
                .ok()
                .and_then(|name| name.into_string().ok())
                .filter(|name| !name.is_empty())
                .unwrap_or_else(|| "oc-airplay".into()),
            port: 0,
            bind_address: Ipv4Addr::UNSPECIFIED.into(),
        }
    }
}

/// AAC format for a future PipeWire adapter.
///
/// Describes the codec configuration supplied by an AirPlay sender. No audio
/// capability is advertised until an audio transport and decoder are installed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioFormat {
    pub codec: AacCodec,
    pub sample_rate: u32,
    pub channels: u8,
    pub audio_specific_config: Vec<u8>,
}

/// AAC variants used by AirPlay.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AacCodec {
    LowComplexity,
    EnhancedLowDelay,
}

/// One decoded-transport audio access unit.
///
/// `timestamp` is the RTP clock value, `sequence` is the RTP sequence, and
/// `data` contains a decrypted AAC access unit without an ADTS wrapper.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AudioPacket {
    pub timestamp: u32,
    pub sequence: u16,
    pub data: Vec<u8>,
}

/// Hook for an AAC decoder and PipeWire output.
///
/// Implementors accept codec settings and decrypted access units. This is an
/// integration contract; this version does not negotiate or receive audio.
pub trait AudioSink: Send {
    /// Configure audio.
    ///
    /// Takes sender format settings and returns success or an output error.
    fn configure(&mut self, format: &AudioFormat) -> io::Result<()>;

    /// Deliver audio.
    ///
    /// Takes one decrypted AAC unit and returns success or an output error.
    fn write_packet(&mut self, packet: &AudioPacket) -> io::Result<()>;
}

/// Feature name.
///
/// Takes no arguments and returns the CLI feature identifier.
pub fn feature_name() -> &'static str {
    "airplay"
}

fn invalid(message: impl Into<String>) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.into())
}
