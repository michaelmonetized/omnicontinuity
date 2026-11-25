use crate::{Config, invalid, pairing::Identity};
use sha2::{Digest, Sha512};
use std::io;
use std::net::IpAddr;
use std::time::{Duration, Instant};
use zbus::blocking::{Connection, Proxy};
use zbus::zvariant::OwnedObjectPath;

pub(crate) const FEATURES: u64 = 0;
pub(crate) const MODEL: &str = "OmniContinuity1,1";
pub(crate) const SOURCE_VERSION: &str = env!("CARGO_PKG_VERSION");
const DESTINATION: &str = "org.freedesktop.Avahi";
const GROUP: &str = "org.freedesktop.Avahi.EntryGroup";
const NO_REVERSE: u32 = 16;
const NO_COOKIE: u32 = 32;

pub(crate) fn txt_records(identity: &Identity) -> Vec<Vec<u8>> {
    let pk = identity
        .public_key()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    [
        format!("deviceid={}", identity.device_id()),
        format!("features=0x{FEATURES:X},0x0"),
        format!("model={MODEL}"),
        format!("srcvers={SOURCE_VERSION}"),
        "flags=0x0".into(),
        "pw=false".into(),
        format!("pk={pk}"),
    ]
    .into_iter()
    .map(String::into_bytes)
    .collect()
}

pub(crate) fn txt_wire(records: &[Vec<u8>]) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    for record in records {
        bytes.push(
            u8::try_from(record.len()).map_err(|_| invalid("mDNS TXT string exceeds 255 bytes"))?,
        );
        bytes.extend_from_slice(record);
    }
    Ok(bytes)
}

pub(crate) struct Advertisement {
    connection: Connection,
    path: OwnedObjectPath,
}

impl Advertisement {
    pub fn publish(config: &Config, port: u16, identity: &Identity) -> io::Result<Self> {
        let connection = zbus::blocking::connection::Builder::system()
            .map_err(io::Error::other)?
            .method_timeout(Duration::from_secs(3))
            .build()
            .map_err(io::Error::other)?;
        let server = Proxy::new(
            &connection,
            DESTINATION,
            "/",
            "org.freedesktop.Avahi.Server",
        )
        .map_err(io::Error::other)?;
        let addresses = local_addresses(config.bind_address)?;
        let interface = if config.bind_address.is_unspecified() {
            -1
        } else {
            addresses[0].0
        };
        let host = format!("oc-airplay-{}.local", identity.device_id().replace(':', ""));
        let path: OwnedObjectPath = server
            .call("EntryGroupNew", &())
            .map_err(io::Error::other)?;
        let advertisement = Self {
            connection: connection.clone(),
            path,
        };
        let group = advertisement.proxy()?;
        let records = txt_records(identity);
        txt_wire(&records)?;
        let protocol: i32 = if config.bind_address.is_ipv4() { 0 } else { 1 };
        for (index, address) in addresses {
            group
                .call::<_, _, ()>(
                    "AddAddress",
                    &(
                        index,
                        protocol,
                        NO_REVERSE,
                        host.as_str(),
                        address.to_string(),
                    ),
                )
                .map_err(io::Error::other)?;
        }
        group
            .call::<_, _, ()>(
                "AddService",
                &(
                    interface,
                    protocol,
                    NO_COOKIE,
                    config.name.as_str(),
                    "_airplay._tcp",
                    "local",
                    host.as_str(),
                    port,
                    records,
                ),
            )
            .map_err(io::Error::other)?;
        group
            .call::<_, _, ()>("Commit", &())
            .map_err(io::Error::other)?;
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let state: i32 = group.call("GetState", &()).map_err(io::Error::other)?;
            match state {
                2 => {
                    drop(group);
                    return Ok(advertisement);
                }
                3 => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        "AirPlay service name collision",
                    ));
                }
                4 => return Err(io::Error::other("Avahi registration failed")),
                _ if Instant::now() >= deadline => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "Avahi registration timed out",
                    ));
                }
                _ => std::thread::sleep(Duration::from_millis(25)),
            }
        }
    }

    fn proxy(&self) -> io::Result<Proxy<'_>> {
        Proxy::new(&self.connection, DESTINATION, self.path.as_str(), GROUP)
            .map_err(io::Error::other)
    }

    pub fn stop(&self) -> io::Result<()> {
        self.proxy()?
            .call::<_, _, ()>("Reset", &())
            .map_err(io::Error::other)
    }
}

impl Drop for Advertisement {
    fn drop(&mut self) {
        if let Ok(group) = self.proxy() {
            let _ = group.call::<_, _, ()>("Free", &());
        }
    }
}

fn local_addresses(bind: IpAddr) -> io::Result<Vec<(i32, IpAddr)>> {
    let mut addresses = std::ptr::null_mut();
    let result = unsafe { libc::getifaddrs(&mut addresses) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut current = addresses;
    let mut found = Vec::new();
    while !current.is_null() {
        let entry = unsafe { &*current };
        if !entry.ifa_addr.is_null() {
            let ip = match unsafe { (*entry.ifa_addr).sa_family } as i32 {
                libc::AF_INET => {
                    let address = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_in>()) };
                    Some(IpAddr::from(address.sin_addr.s_addr.to_ne_bytes()))
                }
                libc::AF_INET6 => {
                    let address = unsafe { &*(entry.ifa_addr.cast::<libc::sockaddr_in6>()) };
                    Some(IpAddr::from(address.sin6_addr.s6_addr))
                }
                _ => None,
            };
            let eligible = ip.is_some_and(|address| {
                if bind.is_unspecified() {
                    address.is_ipv4() == bind.is_ipv4()
                        && !address.is_loopback()
                        && !address.is_unspecified()
                        && entry.ifa_flags & libc::IFF_UP as u32 != 0
                        && entry.ifa_flags & libc::IFF_MULTICAST as u32 != 0
                } else {
                    address == bind
                }
            });
            if eligible {
                let index = unsafe { libc::if_nametoindex(entry.ifa_name) };
                if index != 0 {
                    let entry = (index as i32, ip.unwrap());
                    if !found.contains(&entry) {
                        found.push(entry);
                    }
                }
            }
        }
        current = entry.ifa_next;
    }
    unsafe {
        libc::freeifaddrs(addresses);
    }
    if found.is_empty() {
        Err(invalid(
            "No local multicast interface matches the bind address",
        ))
    } else {
        Ok(found)
    }
}

pub(crate) fn display_uuid(identity: &Identity) -> String {
    let hash = Sha512::digest(identity.public_key());
    let hex = hash[..16]
        .iter()
        .map(|byte| format!("{byte:02X}"))
        .collect::<String>();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mdns_txt_has_exact_length_prefixed_bytes() {
        assert_eq!(
            txt_wire(&[b"features=0x0,0x0".to_vec(), b"pw=false".to_vec()]).unwrap(),
            b"\x10features=0x0,0x0\x08pw=false"
        );
        assert!(txt_wire(&[vec![b'x'; 256]]).is_err());
    }

    #[test]
    fn advertised_bytes_match_identity_and_only_implemented_capabilities() {
        let identity = Identity::new();
        let records = txt_records(&identity);
        let wire = txt_wire(&records).unwrap();
        let mut cursor = 0;
        for record in &records {
            assert_eq!(wire[cursor] as usize, record.len());
            assert_eq!(&wire[cursor + 1..cursor + 1 + record.len()], record);
            cursor += record.len() + 1;
        }
        assert_eq!(cursor, wire.len());
        assert!(records.contains(&b"features=0x0,0x0".to_vec()));
        assert!(records.contains(&b"flags=0x0".to_vec()));
        assert!(records.contains(&format!("model={MODEL}").into_bytes()));
        assert!(records.contains(&format!("deviceid={}", identity.device_id()).into_bytes()));
        let features = records
            .iter()
            .find_map(|record| {
                std::str::from_utf8(record)
                    .ok()?
                    .strip_prefix("features=")
                    .map(str::to_owned)
            })
            .unwrap();
        assert_eq!(features, "0x0,0x0");
        assert_eq!(display_uuid(&identity).len(), 36);
    }

    #[test]
    fn loopback_bind_resolves_to_loopback_interface() {
        let addresses = local_addresses("127.0.0.1".parse().unwrap()).unwrap();
        assert!(addresses[0].0 > 0);
        assert_eq!(addresses[0].1.to_string(), "127.0.0.1");
        assert!(local_addresses("192.0.2.77".parse().unwrap()).is_err());
    }
}
