use crate::{Interface, Result, protocol};
use socket2::{Domain, Protocol, Socket, Type};
use std::{
    collections::{BTreeMap, BTreeSet},
    io,
    net::{Ipv6Addr, SocketAddr, SocketAddrV6, UdpSocket},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

const SERVICE: &str = "_airdrop._tcp.local";
const GROUP: Ipv6Addr = Ipv6Addr::new(0xff02, 0, 0, 0, 0, 0, 0, 0xfb);
const MAX_RECORDS: usize = 256;

fn socket(interface: &Interface) -> Result<UdpSocket> {
    let socket = Socket::new(Domain::IPV6, Type::DGRAM, Some(Protocol::UDP))?;
    socket.set_only_v6(true)?;
    socket.set_reuse_address(true)?;
    socket.set_reuse_port(true)?;
    socket.bind(&SocketAddrV6::new(Ipv6Addr::UNSPECIFIED, 5353, 0, 0).into())?;
    socket.set_multicast_if_v6(interface.index)?;
    socket.set_multicast_hops_v6(255)?;
    socket.set_multicast_loop_v6(false)?;
    socket.join_multicast_v6(&GROUP, interface.index)?;
    socket.set_read_timeout(Some(Duration::from_millis(100)))?;
    Ok(socket.into())
}
fn group(interface: &Interface) -> SocketAddrV6 {
    SocketAddrV6::new(GROUP, 5353, 0, interface.index)
}
fn on_interface(source: SocketAddr, interface: &Interface) -> bool {
    matches!(source, SocketAddr::V6(addr) if addr.scope_id() == interface.index && addr.ip().is_unicast_link_local())
}

#[derive(Clone, Debug)]
struct Record {
    name: String,
    ttl: u32,
    data: Data,
}
#[derive(Clone, Debug)]
enum Data {
    Ptr(String),
    Srv(u16, String),
    Txt(Vec<String>),
    Aaaa(Ipv6Addr),
    Other,
}
#[derive(Debug)]
struct Packet {
    response: bool,
    questions: Vec<String>,
    records: Vec<Record>,
}

fn name(bytes: &[u8], position: &mut usize) -> Result<String> {
    let mut labels = Vec::new();
    let mut offset = *position;
    let mut jumped = false;
    let mut length = 0;
    for _ in 0..128 {
        let size = *bytes
            .get(offset)
            .ok_or_else(|| protocol("truncated DNS name"))? as usize;
        if size == 0 {
            if !jumped {
                *position = offset + 1;
            }
            return Ok(labels.join(".").to_ascii_lowercase());
        }
        if size & 0xc0 == 0xc0 {
            let low = *bytes
                .get(offset + 1)
                .ok_or_else(|| protocol("truncated DNS pointer"))? as usize;
            if !jumped {
                *position = offset + 2;
                jumped = true;
            }
            offset = ((size & 0x3f) << 8) | low;
            continue;
        }
        if size > 63 {
            return Err(protocol("invalid DNS label"));
        }
        let label = bytes
            .get(offset + 1..offset + 1 + size)
            .ok_or_else(|| protocol("truncated DNS label"))?;
        length += size + 1;
        if length > 255 {
            return Err(protocol("DNS name exceeds limit"));
        }
        labels.push(
            std::str::from_utf8(label)
                .map_err(|_| protocol("non UTF-8 DNS label"))?
                .to_owned(),
        );
        offset += size + 1;
    }
    Err(protocol("DNS compression loop"))
}
fn u16_at(bytes: &[u8], offset: usize) -> Result<u16> {
    let v = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| protocol("truncated DNS field"))?;
    Ok(u16::from_be_bytes([v[0], v[1]]))
}
fn parse(bytes: &[u8]) -> Result<Packet> {
    if bytes.len() < 12 {
        return Err(protocol("truncated DNS header"));
    }
    let response = bytes[2] & 0x80 != 0;
    let questions = u16_at(bytes, 4)? as usize;
    let records = usize::from(u16_at(bytes, 6)?)
        + usize::from(u16_at(bytes, 8)?)
        + usize::from(u16_at(bytes, 10)?);
    if questions + records > MAX_RECORDS {
        return Err(protocol("too many DNS records"));
    }
    let mut position = 12;
    let mut question_names = Vec::new();
    for _ in 0..questions {
        question_names.push(name(bytes, &mut position)?);
        u16_at(bytes, position + 2)?;
        position += 4;
    }
    let mut answer = Vec::new();
    for _ in 0..records {
        let label = name(bytes, &mut position)?;
        let kind = u16_at(bytes, position)?;
        let class = u16_at(bytes, position + 2)? & 0x7fff;
        let ttl = (u32::from(u16_at(bytes, position + 4)?) << 16)
            | u32::from(u16_at(bytes, position + 6)?);
        let length = u16_at(bytes, position + 8)? as usize;
        position += 10;
        let end = position
            .checked_add(length)
            .filter(|&v| v <= bytes.len())
            .ok_or_else(|| protocol("truncated DNS record"))?;
        let data = match kind {
            12 => Data::Ptr(name(bytes, &mut position)?),
            33 if length >= 7 => {
                let port = u16_at(bytes, position + 4)?;
                position += 6;
                Data::Srv(port, name(bytes, &mut position)?)
            }
            16 => {
                let mut entries = Vec::new();
                while position < end {
                    let size = bytes[position] as usize;
                    position += 1;
                    let entry = bytes
                        .get(position..position + size)
                        .filter(|_| position + size <= end)
                        .ok_or_else(|| protocol("truncated DNS TXT"))?;
                    entries.push(String::from_utf8_lossy(entry).into_owned());
                    position += size;
                }
                Data::Txt(entries)
            }
            28 if length == 16 => {
                let octets: [u8; 16] = bytes[position..end]
                    .try_into()
                    .map_err(|_| protocol("invalid AAAA"))?;
                Data::Aaaa(Ipv6Addr::from(octets))
            }
            _ => Data::Other,
        };
        if position > end {
            return Err(protocol("DNS name crosses record boundary"));
        }
        position = end;
        if class == 1 {
            answer.push(Record {
                name: label,
                ttl,
                data,
            });
        }
    }
    Ok(Packet {
        response,
        questions: question_names,
        records: answer,
    })
}

fn wire_name(bytes: &mut Vec<u8>, name: &str) {
    for label in name.trim_end_matches('.').split('.') {
        bytes.push(label.len() as u8);
        bytes.extend_from_slice(label.as_bytes());
    }
    bytes.push(0);
}
fn query(label: &str, kind: u16) -> Vec<u8> {
    let mut bytes = vec![0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0];
    wire_name(&mut bytes, label);
    bytes.extend_from_slice(&kind.to_be_bytes());
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes
}
fn rr(bytes: &mut Vec<u8>, label: &str, kind: u16, class: u16, ttl: u32, data: &[u8]) {
    wire_name(bytes, label);
    bytes.extend_from_slice(&kind.to_be_bytes());
    bytes.extend_from_slice(&class.to_be_bytes());
    bytes.extend_from_slice(&ttl.to_be_bytes());
    bytes.extend_from_slice(&(data.len() as u16).to_be_bytes());
    bytes.extend_from_slice(data);
}
fn announcement(interface: &Interface, port: u16, goodbye: bool) -> Vec<u8> {
    let instance = format!("{}.{SERVICE}", interface.service_id);
    let host = format!("oc-drop-{}.local", interface.service_id);
    let mut bytes = vec![0, 0, 0x84, 0, 0, 0, 0, 5, 0, 0, 0, 0];
    let mut pointer = Vec::new();
    wire_name(&mut pointer, &instance);
    let ttl = |value| if goodbye { 0 } else { value };
    rr(&mut bytes, SERVICE, 12, 1, ttl(4500), &pointer);
    let mut srv = vec![0, 0, 0, 0];
    srv.extend_from_slice(&port.to_be_bytes());
    wire_name(&mut srv, &host);
    rr(&mut bytes, &instance, 33, 0x8001, ttl(120), &srv);
    rr(
        &mut bytes,
        &instance,
        16,
        0x8001,
        ttl(4500),
        b"\x09flags=136",
    );
    rr(
        &mut bytes,
        &host,
        28,
        0x8001,
        ttl(120),
        &interface.address.octets(),
    );
    let mut nsec = Vec::new();
    wire_name(&mut nsec, &host);
    nsec.extend_from_slice(b"\x00\x06\x00\x00\x00\x08\x00\x01");
    rr(&mut bytes, &host, 47, 0x8001, ttl(120), &nsec);
    bytes
}

#[derive(Default)]
struct Cache {
    services: BTreeMap<String, Instant>,
    servers: BTreeMap<String, (u16, String, Instant)>,
    addresses: BTreeMap<String, (Ipv6Addr, Instant)>,
    flags: BTreeMap<String, (u32, Instant)>,
}
impl Cache {
    fn update(&mut self, records: Vec<Record>) {
        for record in records {
            if self.services.len() + self.servers.len() + self.addresses.len() + self.flags.len()
                > 1024
            {
                break;
            }
            let expires = Instant::now() + Duration::from_secs(record.ttl.min(4500).into());
            match record.data {
                Data::Ptr(instance)
                    if record.name == SERVICE && instance.ends_with(&format!(".{SERVICE}")) =>
                {
                    self.services.insert(instance, expires);
                }
                Data::Srv(port, host) if record.name.ends_with(&format!(".{SERVICE}")) => {
                    self.servers.insert(record.name, (port, host, expires));
                }
                Data::Aaaa(address) if address.is_unicast_link_local() => {
                    self.addresses.insert(record.name, (address, expires));
                }
                Data::Txt(entries) => {
                    if let Some(flags) = entries
                        .iter()
                        .find_map(|s| s.strip_prefix("flags=").and_then(|s| s.parse().ok()))
                    {
                        self.flags.insert(record.name, (flags, expires));
                    }
                }
                _ => {}
            }
        }
    }
    fn endpoints(&self, interface: &Interface) -> Vec<SocketAddrV6> {
        let now = Instant::now();
        let mut result = BTreeSet::new();
        for (instance, expires) in &self.services {
            if *expires <= now || instance.starts_with(&format!("{}.", interface.service_id)) {
                continue;
            }
            if self
                .flags
                .get(instance)
                .is_some_and(|(flags, expires)| *expires > now && flags & 0x80 == 0)
            {
                continue;
            }
            let Some((port, host, expires)) = self.servers.get(instance) else {
                continue;
            };
            if *port == 0 || *expires <= now {
                continue;
            }
            let Some((address, expires)) = self.addresses.get(host) else {
                continue;
            };
            if *expires <= now || *address == interface.address {
                continue;
            }
            result.insert(SocketAddrV6::new(*address, *port, 0, interface.index));
        }
        result.into_iter().take(64).collect()
    }
}

pub(crate) fn browse(interface: &Interface, timeout: Duration) -> Result<Vec<SocketAddrV6>> {
    let socket = socket(interface)?;
    let deadline = Instant::now() + timeout;
    let mut next_query = Instant::now();
    let mut cache = Cache::default();
    let mut bytes = [0; 9000];
    while Instant::now() < deadline {
        if Instant::now() >= next_query {
            socket.send_to(&query(SERVICE, 12), group(interface))?;
            for instance in cache.services.keys() {
                socket.send_to(&query(instance, 255), group(interface))?;
            }
            for (_, host, _) in cache.servers.values() {
                socket.send_to(&query(host, 28), group(interface))?;
            }
            next_query = Instant::now() + Duration::from_secs(1);
        }
        match socket.recv_from(&mut bytes) {
            Ok((count, source)) if on_interface(source, interface) => {
                if let Ok(packet) = parse(&bytes[..count])
                    && packet.response
                {
                    cache.update(packet.records);
                }
            }
            Ok(_) => {}
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(cache.endpoints(interface))
}

pub(crate) struct Advertisement {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
}
impl Advertisement {
    pub fn start(interface: &Interface, port: u16) -> Result<Self> {
        let socket = socket(interface)?;
        let interface = interface.clone();
        let bytes = announcement(&interface, port, false);
        socket.send_to(&bytes, group(&interface))?;
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let worker = thread::spawn(move || {
            let instance = format!("{}.{SERVICE}", interface.service_id);
            let host = format!("oc-drop-{}.local", interface.service_id);
            let mut buffer = [0; 9000];
            let mut next = Instant::now() + Duration::from_secs(1);
            let mut last = Instant::now();
            while !flag.load(Ordering::Acquire) {
                let mut queried = false;
                if let Ok((count, source)) = socket.recv_from(&mut buffer)
                    && on_interface(source, &interface)
                    && let Ok(packet) = parse(&buffer[..count])
                {
                    queried = !packet.response
                        && packet
                            .questions
                            .iter()
                            .any(|name| name == SERVICE || name == &instance || name == &host);
                }
                if Instant::now() >= next
                    || (queried && last.elapsed() >= Duration::from_millis(100))
                {
                    let _ = socket.send_to(&bytes, group(&interface));
                    last = Instant::now();
                    next = Instant::now() + Duration::from_secs(10);
                }
            }
            let _ = socket.send_to(&announcement(&interface, port, true), group(&interface));
        });
        Ok(Self {
            stop,
            worker: Some(worker),
        })
    }
}
impl Drop for Advertisement {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn interface() -> Interface {
        Interface {
            index: 33,
            address: "fe80::1234".parse().unwrap(),
            service_id: "001122334455".into(),
        }
    }
    #[test]
    fn browse_question_bytes() {
        assert_eq!(
            query(SERVICE, 12),
            b"\0\0\0\0\0\x01\0\0\0\0\0\0\x08_airdrop\x04_tcp\x05local\0\0\x0c\0\x01"
        );
    }
    #[test]
    fn installed_advert_record_shape() {
        let bytes = announcement(&interface(), 8771, false);
        assert_eq!(&bytes[..12], &[0, 0, 0x84, 0, 0, 0, 0, 5, 0, 0, 0, 0]);
        let packet = parse(&bytes).unwrap();
        assert!(packet.response);
        assert_eq!(packet.records.len(), 5);
        assert!(
            matches!(&packet.records[0].data, Data::Ptr(s) if s == "001122334455._airdrop._tcp.local")
        );
        assert!(matches!(&packet.records[1].data, Data::Srv(8771, _)));
        assert!(bytes.windows(10).any(|v| v == b"\x09flags=136"));
    }
    #[test]
    fn compressed_names_and_loops() {
        let bytes = b"\x03one\x05local\0\xc0\0";
        let mut offset = 11;
        assert_eq!(name(bytes, &mut offset).unwrap(), "one.local");
        assert_eq!(offset, 13);
        assert!(name(b"\xc0\0", &mut 0).is_err());
        assert!(name(b"\xc0", &mut 0).is_err());
        for count in 0..announcement(&interface(), 8771, false).len() {
            let _ = parse(&announcement(&interface(), 8771, false)[..count]);
        }
    }
    #[test]
    fn goodbye_expires_and_scope_is_awdl_only() {
        let other = Interface {
            service_id: "aabbccddeeff".into(),
            address: "fe80::42".parse().unwrap(),
            ..interface()
        };
        let mut cache = Cache::default();
        cache.update(parse(&announcement(&other, 8771, false)).unwrap().records);
        let endpoints = cache.endpoints(&interface());
        assert_eq!(endpoints.len(), 1);
        assert_eq!(endpoints[0].scope_id(), 33);
        cache.update(parse(&announcement(&other, 8771, true)).unwrap().records);
        assert!(cache.endpoints(&interface()).is_empty());
    }
}
