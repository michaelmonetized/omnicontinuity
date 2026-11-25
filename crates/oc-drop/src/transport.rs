use crate::{Error, OP_TIMEOUT, Result, protocol};
use openssl::{
    asn1::Asn1Time,
    ec::{EcGroup, EcKey},
    hash::MessageDigest,
    nid::Nid,
    pkey::{PKey, Private},
    ssl::{SslAcceptor, SslConnector, SslMethod, SslStream, SslVerifyMode, SslVersion},
    x509::{X509, X509NameBuilder},
};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    fs,
    io::{self, Read, Write},
    net::{Ipv6Addr, SocketAddrV6, TcpListener, TcpStream},
    time::{Duration, Instant},
};

/// Read-only snapshot of the existing awdl0 link and its link-local address.
#[derive(Clone, Debug)]
pub struct Interface {
    /// Kernel interface index used for IPv6 multicast and link-local scopes.
    pub index: u32,
    /// Existing IPv6 link-local address.
    pub address: Ipv6Addr,
    /// Twelve hexadecimal digits derived from the existing interface MAC.
    pub service_id: String,
}

impl Interface {
    /// Inspect awdl0 without changing it.
    ///
    /// Takes no arguments. Returns the interface index, address and service ID,
    /// or a clear error for a missing/down link or missing IPv6 address.
    pub fn awdl() -> Result<Self> {
        let flags = fs::read_to_string("/sys/class/net/awdl0/flags")
            .map_err(|e| Error::Interface(e.to_string()))?;
        let flags = u32::from_str_radix(flags.trim().trim_start_matches("0x"), 16)
            .map_err(|e| Error::Interface(e.to_string()))?;
        if flags & libc::IFF_UP as u32 == 0 {
            return Err(Error::Interface("awdl0 is down".into()));
        }
        let index = fs::read_to_string("/sys/class/net/awdl0/ifindex")
            .map_err(|e| Error::Interface(e.to_string()))?
            .trim()
            .parse::<u32>()
            .map_err(|e| Error::Interface(e.to_string()))?;
        let addresses = fs::read_to_string("/proc/net/if_inet6")
            .map_err(|e| Error::Interface(e.to_string()))?;
        let address = addresses
            .lines()
            .find_map(|line| {
                let fields: Vec<_> = line.split_whitespace().collect();
                if fields.len() != 6 || fields[5] != "awdl0" {
                    return None;
                }
                let flags = u32::from_str_radix(fields[4], 16).ok()?;
                if flags & (libc::IFA_F_TENTATIVE | libc::IFA_F_DADFAILED) != 0 {
                    return None;
                }
                let value = u128::from_str_radix(fields[0], 16).ok()?;
                let address = Ipv6Addr::from(value);
                address.is_unicast_link_local().then_some(address)
            })
            .ok_or_else(|| {
                Error::Interface("awdl0 has no usable link-local IPv6 address".into())
            })?;
        let service_id = fs::read_to_string("/sys/class/net/awdl0/address")
            .map_err(|e| Error::Interface(e.to_string()))?
            .trim()
            .replace(':', "");
        if service_id.len() != 12 || !service_id.bytes().all(|v| v.is_ascii_hexdigit()) {
            return Err(Error::Interface("invalid awdl0 MAC address".into()));
        }
        Ok(Self {
            index,
            address,
            service_id,
        })
    }

    pub(crate) fn socket_address(&self, port: u16) -> SocketAddrV6 {
        SocketAddrV6::new(self.address, port, 0, self.index)
    }

    pub(crate) fn listen(&self, port: u16) -> Result<TcpListener> {
        let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        socket.set_only_v6(true)?;
        socket.set_reuse_address(true)?;
        socket.bind(&self.socket_address(port).into())?;
        socket.listen(16)?;
        socket.set_nonblocking(true)?;
        Ok(socket.into())
    }
}

pub(crate) struct Identity {
    key: PKey<Private>,
    cert: X509,
}

impl Identity {
    pub fn new() -> Result<Self> {
        let group = EcGroup::from_curve_name(Nid::X9_62_PRIME256V1)?;
        let key = PKey::from_ec_key(EcKey::generate(&group)?)?;
        let mut name = X509NameBuilder::new()?;
        name.append_entry_by_text("CN", "AirDrop")?;
        let name = name.build();
        let mut cert = X509::builder()?;
        cert.set_version(2)?;
        let serial =
            openssl::bn::BigNum::from_slice(uuid::Uuid::new_v4().as_bytes())?.to_asn1_integer()?;
        cert.set_serial_number(&serial)?;
        cert.set_subject_name(&name)?;
        cert.set_issuer_name(&name)?;
        cert.set_pubkey(&key)?;
        cert.set_not_before(Asn1Time::days_from_now(0)?.as_ref())?;
        cert.set_not_after(Asn1Time::days_from_now(7)?.as_ref())?;
        cert.sign(&key, MessageDigest::sha256())?;
        Ok(Self {
            key,
            cert: cert.build(),
        })
    }

    pub fn connect(
        &self,
        interface: &Interface,
        address: SocketAddrV6,
        timeout: Duration,
    ) -> Result<SslStream<TcpStream>> {
        if !address.ip().is_unicast_link_local() || address.scope_id() != interface.index {
            return Err(protocol("peer must be link-local on awdl0"));
        }
        let socket = Socket::new(Domain::IPV6, Type::STREAM, Some(Protocol::TCP))?;
        socket.bind(&interface.socket_address(0).into())?;
        socket.connect_timeout(&address.into(), OP_TIMEOUT)?;
        let stream: TcpStream = socket.into();
        stream.set_read_timeout(Some(timeout))?;
        stream.set_write_timeout(Some(timeout))?;
        let mut connector = SslConnector::builder(SslMethod::tls_client())?;
        connector.set_min_proto_version(Some(SslVersion::TLS1_2))?;
        connector.set_verify(SslVerifyMode::NONE);
        connector.set_private_key(&self.key)?;
        connector.set_certificate(&self.cert)?;
        let mut configuration = connector.build().configure()?;
        configuration.set_verify_hostname(false);
        configuration.set_use_server_name_indication(false);
        configuration
            .connect("AirDrop", stream)
            .map_err(|e| protocol(format!("TLS connection failed: {e}")))
    }

    pub fn accept(
        &self,
        stream: TcpStream,
        deadline: Instant,
    ) -> Result<SslStream<DeadlineStream>> {
        let mut acceptor = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
        acceptor.set_min_proto_version(Some(SslVersion::TLS1_2))?;
        acceptor.set_verify(SslVerifyMode::NONE);
        acceptor.set_private_key(&self.key)?;
        acceptor.set_certificate(&self.cert)?;
        acceptor.check_private_key()?;
        acceptor
            .build()
            .accept(DeadlineStream { stream, deadline })
            .map_err(|e| protocol(format!("TLS accept failed: {e}")))
    }
}

#[derive(Debug)]
pub(crate) struct DeadlineStream {
    stream: TcpStream,
    deadline: Instant,
}
impl DeadlineStream {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .map(|d| d.min(OP_TIMEOUT))
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "receive deadline elapsed"))
    }
}
impl Read for DeadlineStream {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.stream.set_read_timeout(Some(self.remaining()?))?;
        self.stream.read(bytes)
    }
}
impl Write for DeadlineStream {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.stream.set_write_timeout(Some(self.remaining()?))?;
        self.stream.write(bytes)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}
