//! Device identity and live Tailscale discovery shared by oc features.

use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr};
use std::process::Command;

use serde::Deserialize;

/// One device reported by Tailscale, including offline peers.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Device {
    #[serde(rename = "HostName")]
    pub hostname: String,
    #[serde(rename = "DNSName", default)]
    pub dns_name: String,
    #[serde(rename = "OS")]
    pub os: String,
    #[serde(rename = "TailscaleIPs", default)]
    pub ips: Vec<IpAddr>,
    #[serde(rename = "Online", default)]
    pub online: bool,
}

impl Device {
    /// Tailscale IPv4 address.
    ///
    /// Takes no arguments and returns the first address in Tailscale's IPv4 range.
    pub fn ipv4(&self) -> Option<Ipv4Addr> {
        self.ips.iter().find_map(|ip| match ip {
            IpAddr::V4(ip) if ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]) => {
                Some(*ip)
            }
            _ => None,
        })
    }
}

/// This machine and every peer in the current Tailscale network map.
#[derive(Debug)]
pub struct Tailnet {
    pub backend_state: String,
    pub this_device: Device,
    pub peers: Vec<Device>,
}

#[derive(Deserialize)]
struct Status {
    #[serde(rename = "BackendState")]
    backend_state: String,
    #[serde(rename = "Self")]
    this_device: Device,
    #[serde(rename = "Peer", default)]
    peers: Option<BTreeMap<String, Device>>,
}

impl Tailnet {
    /// Parse a Tailscale network map.
    ///
    /// Accepts the bytes from `tailscale status --json`. Returns this device and
    /// peers sorted by hostname and DNS name, or an error for invalid status data.
    pub fn from_json(json: &[u8]) -> Result<Self, String> {
        let status: Status =
            serde_json::from_slice(json).map_err(|e| format!("Tailscale status JSON: {e}"))?;
        let mut this_device = status.this_device;
        this_device.dns_name = this_device.dns_name.trim_end_matches('.').to_owned();
        let mut peers: Vec<_> = status.peers.unwrap_or_default().into_values().collect();
        for peer in &mut peers {
            peer.dns_name = peer.dns_name.trim_end_matches('.').to_owned();
        }
        peers.sort_by(|a, b| {
            a.hostname
                .cmp(&b.hostname)
                .then(a.dns_name.cmp(&b.dns_name))
        });
        Ok(Self {
            backend_state: status.backend_state,
            this_device,
            peers,
        })
    }

    /// Discover this machine and its peers.
    ///
    /// Takes no arguments. Runs `tailscale status --json` and returns its network
    /// map, or an error when Tailscale cannot run or returns invalid data.
    pub fn discover() -> Result<Self, String> {
        let output = Command::new("tailscale")
            .args(["status", "--json"])
            .output()
            .map_err(|e| format!("tailscale status: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "tailscale status ({}): {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Self::from_json(&output.stdout)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_self_offline_peers_and_duplicate_hostnames() {
        let tailnet = Tailnet::from_json(br#"{
            "BackendState":"Running",
            "Self":{"HostName":"host","DNSName":"host.example.ts.net.","OS":"linux","TailscaleIPs":["fd7a::1","100.74.233.121"],"Online":true},
            "Peer":{
                "b":{"HostName":"mac","DNSName":"mac-2.example.ts.net.","OS":"macOS","Online":false},
                "a":{"HostName":"mac","DNSName":"mac.example.ts.net.","OS":"macOS","Online":true}
            }
        }"#).unwrap();
        assert_eq!(
            tailnet.this_device.ipv4(),
            Some(Ipv4Addr::new(100, 74, 233, 121))
        );
        assert_eq!(tailnet.this_device.dns_name, "host.example.ts.net");
        assert_eq!(tailnet.peers.len(), 2);
        assert!(!tailnet.peers[0].online);
        assert!(tailnet.peers[1].online);
    }

    #[test]
    fn accepts_null_peers_but_rejects_missing_self() {
        let tailnet = Tailnet::from_json(
            br#"{"BackendState":"Running","Self":{"HostName":"solo","OS":"linux"},"Peer":null}"#,
        )
        .unwrap();
        assert!(tailnet.peers.is_empty());
        assert_eq!(tailnet.this_device.ipv4(), None);
        assert!(Tailnet::from_json(br#"{"BackendState":"Stopped"}"#).is_err());
    }
}
