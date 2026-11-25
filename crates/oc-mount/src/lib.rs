//! Peer home directories and Mac cloud files over sshfs.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::ErrorKind;
use std::net::Ipv4Addr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use oc_core::Tailnet;

/// Which remote directory a mount exposes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MountKind {
    Home,
    Cloud,
}

/// One sshfs mount that should exist while the peer is online.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Desired {
    pub host: String,
    pub source: String,
    pub mountpoint: PathBuf,
    pub kind: MountKind,
}

/// What a reconcile did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub mounted: Vec<PathBuf>,
    pub unmounted: Vec<PathBuf>,
    pub unchanged: Vec<PathBuf>,
}

/// Runs mount and unmount. Tests supply a fake. The real runner uses sshfs.
pub trait Runner {
    /// Return the mountpoints currently mounted, or a discovery error.
    fn mounted(&self) -> Result<Vec<PathBuf>, String>;
    /// Identify an owned mount.
    ///
    /// Takes a mounted path and returns its verified remote source. `None`
    /// means ownership or source is unknown; reconciliation leaves it alone.
    fn source(&self, _mountpoint: &Path) -> Result<Option<String>, String> {
        Ok(None)
    }
    /// Mount `source` at `mountpoint`, returning an error if the command fails.
    fn mount(&mut self, source: &str, mountpoint: &Path) -> Result<(), String>;
    /// Unmount `mountpoint`, returning an error if the command fails.
    fn unmount(&mut self, mountpoint: &Path) -> Result<(), String>;
}

/// Mounts that should exist for the online peers.
///
/// Uses `home`, the remote `user`, and a discovered `tailnet`. Returns home and
/// eligible Mac cloud mounts, or an error for unsafe or conflicting names.
pub fn desired(home: &Path, user: &str, tailnet: &Tailnet) -> Result<Vec<Desired>, String> {
    validate_user(user)?;
    let mut hostnames = BTreeMap::new();
    for peer in &tailnet.peers {
        *hostnames.entry(peer.hostname.as_str()).or_insert(0) += 1;
    }

    let mut names = BTreeSet::new();
    let mut mounts = Vec::new();
    for peer in &tailnet.peers {
        let os = peer.os.trim().to_ascii_lowercase();
        if !peer.online
            || ["ios", "ipad", "iphone", "android"]
                .iter()
                .any(|mobile| os.contains(mobile))
            || !(os == "linux" || os.contains("mac"))
        {
            continue;
        }
        let Some(ipv4) = peer.ipv4() else {
            continue;
        };
        if Some(ipv4) == tailnet.this_device.ipv4() {
            continue;
        }

        let name = if hostnames[peer.hostname.as_str()] > 1 {
            peer.dns_name.split('.').next().unwrap_or_default()
        } else {
            peer.hostname.as_str()
        };
        validate_name(name)?;
        if !names.insert(name) {
            return Err(format!("Peers share the mount name {name:?}"));
        }

        mounts.push(Desired {
            host: name.to_owned(),
            source: format!("{user}@{ipv4}:"),
            mountpoint: home.join("Machines").join(name),
            kind: MountKind::Home,
        });
        if os.contains("mac") {
            mounts.push(Desired {
                host: name.to_owned(),
                source: format!("{user}@{ipv4}:Library/Mobile Documents"),
                mountpoint: home.join("Cloud").join(name).join(user),
                kind: MountKind::Cloud,
            });
        }
    }
    Ok(mounts)
}

/// Mount what is missing and drop what the peer left behind.
///
/// Uses `home`, remote `user`, `tailnet`, and `runner`. Creates missing mount
/// directories and returns sorted changes, or the first discovery or I/O error.
/// An occupied desired path without verified source ownership returns an error.
pub fn reconcile(
    home: &Path,
    user: &str,
    tailnet: &Tailnet,
    runner: &mut dyn Runner,
) -> Result<Report, String> {
    let desired = desired(home, user, tailnet)?;
    let wanted: BTreeMap<_, _> = desired
        .iter()
        .map(|entry| (&entry.mountpoint, entry.source.as_str()))
        .collect();
    let mounted: BTreeSet<_> = runner.mounted()?.into_iter().collect();
    let roots = [home.join("Machines"), home.join("Cloud")];
    let sources: BTreeMap<_, _> = mounted
        .iter()
        .filter(|path| managed_path(path, &roots, true))
        .map(|path| Ok((path.clone(), runner.source(path)?)))
        .collect::<Result<_, String>>()?;
    let remove: BTreeSet<_> = sources
        .iter()
        .filter(|(path, source)| {
            source.is_some()
                && managed_path(path, &roots, false)
                && !wanted
                    .get(path)
                    .is_some_and(|wanted| Some(*wanted) == source.as_deref())
        })
        .map(|(path, _)| path.clone())
        .collect();

    for entry in &desired {
        if mounted.contains(&entry.mountpoint)
            && sources.get(&entry.mountpoint).is_none_or(Option::is_none)
        {
            return Err(format!(
                "Cannot verify mount ownership and source at {}",
                entry.mountpoint.display()
            ));
        }
        if sources.keys().any(|path| {
            path != &entry.mountpoint
                && entry.mountpoint.starts_with(path)
                && !remove.contains(path)
        }) {
            return Err(format!(
                "Retained mount contains {}",
                entry.mountpoint.display()
            ));
        }
        inspect_mount_path(&entry.mountpoint, &sources)?;
    }
    for path in &remove {
        if mounted
            .iter()
            .any(|child| child != path && child.starts_with(path) && !remove.contains(child))
        {
            return Err(format!(
                "Refuse to unmount {} with a retained mount inside it",
                path.display()
            ));
        }
        inspect_mount_path(path, &sources)?;
    }

    let mut stale: Vec<_> = remove.iter().collect();
    stale.sort_by_key(|path| std::cmp::Reverse(path.components().count()));

    let mut report = Report::default();
    for path in stale {
        inspect_mount_path(path, &sources)?;
        if runner.source(path)? != sources.get(path).cloned().flatten() {
            return Err(format!("Mount source changed at {}", path.display()));
        }
        runner.unmount(path)?;
        report.unmounted.push(path.clone());
    }
    for entry in desired {
        if mounted.contains(&entry.mountpoint) && !remove.contains(&entry.mountpoint) {
            if runner.source(&entry.mountpoint)?.as_deref() != Some(entry.source.as_str()) {
                return Err(format!(
                    "Mount source changed during reconciliation at {}",
                    entry.mountpoint.display()
                ));
            }
            report.unchanged.push(entry.mountpoint);
            continue;
        }
        mount_directory(&entry.mountpoint, true)?;
        runner.mount(&entry.source, &entry.mountpoint)?;
        report.mounted.push(entry.mountpoint);
    }
    report.mounted.sort();
    report.unmounted.sort();
    report.unchanged.sort();
    Ok(report)
}

/// Discover the tailnet and reconcile mounts under the home directory.
///
/// Uses `home` and remote `user`. Runs Tailscale discovery, sshfs, and unmount
/// commands once, returning their changes or an error. Runs no background loop.
pub fn sync(home: &Path, user: &str) -> Result<Report, String> {
    validate_user(user)?;
    let tailnet = Tailnet::discover()?;
    reconcile(home, user, &tailnet, &mut SystemRunner::discover()?)
}

fn validate_user(user: &str) -> Result<(), String> {
    let mut bytes = user.bytes();
    if !bytes
        .next()
        .is_some_and(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
        || !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(format!("Invalid SSH user {user:?}"));
    }
    Ok(())
}

fn validate_name(name: &str) -> Result<(), String> {
    if name.is_empty() || matches!(name, "." | "..") || name.contains(['/', '\0']) {
        return Err(format!("Invalid mount name {name:?}"));
    }
    Ok(())
}

fn managed_path(path: &Path, roots: &[PathBuf], include_root: bool) -> bool {
    roots.iter().any(|root| {
        path.strip_prefix(root).is_ok_and(|relative| {
            (include_root || !relative.as_os_str().is_empty())
                && relative
                    .components()
                    .all(|part| matches!(part, Component::Normal(_)))
        })
    })
}

fn inspect_mount_path(
    path: &Path,
    mounted: &BTreeMap<PathBuf, Option<String>>,
) -> Result<(), String> {
    let parent = mounted
        .keys()
        .filter(|mountpoint| path.starts_with(mountpoint))
        .min_by_key(|mountpoint| mountpoint.components().count())
        .and_then(|mountpoint| mountpoint.parent());
    mount_directory(parent.unwrap_or(path), false)
}

fn mount_directory(path: &Path, create: bool) -> Result<(), String> {
    if !path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
    {
        return Err(format!(
            "Mount path must be absolute without '..': {}",
            path.display()
        ));
    }
    let mut current = PathBuf::new();
    for part in path.components() {
        current.push(part);
        let metadata = match fs::symlink_metadata(&current) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == ErrorKind::NotFound => {
                if !create {
                    return Ok(());
                }
                match fs::create_dir(&current) {
                    Ok(()) => {}
                    Err(error) if error.kind() == ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(format!("Create {}: {error}", current.display())),
                }
                fs::symlink_metadata(&current)
                    .map_err(|error| format!("Inspect {}: {error}", current.display()))?
            }
            Err(error) => return Err(format!("Inspect {}: {error}", current.display())),
        };
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(format!(
                "Mount path must use real directories: {}",
                current.display()
            ));
        }
    }
    Ok(())
}

struct SystemRunner {
    mounts: BTreeMap<PathBuf, Option<String>>,
}

impl SystemRunner {
    fn discover() -> Result<Self, String> {
        Ok(Self {
            mounts: Self::read_mounts()?,
        })
    }

    fn read_mounts() -> Result<BTreeMap<PathBuf, Option<String>>, String> {
        let mountinfo = fs::read("/proc/self/mountinfo")
            .map_err(|error| format!("Read mount table: {error}"))?;
        parse_mountinfo(&mountinfo)
    }

    fn verify_owned(&self, mountpoint: &Path) -> Result<(), String> {
        let current = Self::read_mounts()?;
        verify_owned_source(mountpoint, &self.mounts, &current)
    }
}

impl Runner for SystemRunner {
    fn mounted(&self) -> Result<Vec<PathBuf>, String> {
        Ok(self.mounts.keys().cloned().collect())
    }

    fn source(&self, mountpoint: &Path) -> Result<Option<String>, String> {
        let current = Self::read_mounts()?;
        if current.get(mountpoint) != self.mounts.get(mountpoint) {
            return Err(format!("Mount changed at {}", mountpoint.display()));
        }
        Ok(current.get(mountpoint).cloned().flatten())
    }

    fn mount(&mut self, source: &str, mountpoint: &Path) -> Result<(), String> {
        let current = Self::read_mounts()?;
        if current.contains_key(mountpoint)
            || current.iter().any(|(path, source)| {
                mountpoint.starts_with(path) && self.mounts.get(path) != Some(source)
            })
        {
            return Err(format!("Mount path changed at {}", mountpoint.display()));
        }
        mount_directory(mountpoint, false)?;
        run(&mut sshfs_command(source, mountpoint))?;
        let current = Self::read_mounts()?;
        if current.get(mountpoint).and_then(Option::as_deref) != Some(source) {
            return Err(format!(
                "Cannot verify new mount at {}",
                mountpoint.display()
            ));
        }
        self.mounts
            .insert(mountpoint.to_owned(), Some(source.to_owned()));
        Ok(())
    }

    fn unmount(&mut self, mountpoint: &Path) -> Result<(), String> {
        self.verify_owned(mountpoint)?;
        run(Command::new("fusermount3")
            .args(["-u", "--"])
            .arg(mountpoint))
        .or_else(|fuse_error| {
            self.verify_owned(mountpoint)
                .map_err(|error| format!("{fuse_error}; {error}"))?;
            run(Command::new("umount").arg("--").arg(mountpoint))
                .map_err(|error| format!("{fuse_error}; {error}"))
        })?;
        if Self::read_mounts()?.contains_key(mountpoint) {
            return Err(format!("Mount still occupied at {}", mountpoint.display()));
        }
        self.mounts.remove(mountpoint);
        Ok(())
    }
}

fn verify_owned_source(
    mountpoint: &Path,
    expected: &BTreeMap<PathBuf, Option<String>>,
    current: &BTreeMap<PathBuf, Option<String>>,
) -> Result<(), String> {
    if expected.get(mountpoint).is_none_or(Option::is_none)
        || expected.get(mountpoint) != current.get(mountpoint)
    {
        return Err(format!(
            "Mount ownership or source changed at {}",
            mountpoint.display()
        ));
    }
    Ok(())
}

fn sshfs_command(source: &str, mountpoint: &Path) -> Command {
    let mut command = Command::new("sshfs");
    command
        .args([
            "-o",
            "reconnect,ServerAliveInterval=15,ServerAliveCountMax=3,ConnectTimeout=8,BatchMode=yes",
        ])
        .arg("-o")
        .arg(format!("fsname=oc-mount:{source}"))
        .arg("--")
        .arg(source)
        .arg(mountpoint);
    command
}

fn run(command: &mut Command) -> Result<(), String> {
    let program = command.get_program().to_string_lossy().into_owned();
    let output = command
        .output()
        .map_err(|error| format!("{program}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{program} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(())
}

fn parse_mountinfo(mountinfo: &[u8]) -> Result<BTreeMap<PathBuf, Option<String>>, String> {
    let mut mounts = BTreeMap::new();
    for line in mountinfo
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        let fields: Vec<_> = line.split(|byte| *byte == b' ').collect();
        let separator = fields
            .iter()
            .position(|field| *field == b"-")
            .ok_or("Missing mount table separator")?;
        if separator < 6 || fields.len() < separator + 4 {
            return Err("Incomplete mount table entry".into());
        }
        let bytes = decode_mount_field(fields[4])?;
        #[cfg(unix)]
        let mountpoint = {
            use std::os::unix::ffi::OsStringExt;
            PathBuf::from(std::ffi::OsString::from_vec(bytes))
        };
        #[cfg(not(unix))]
        let mountpoint = PathBuf::from(
            String::from_utf8(bytes)
                .map_err(|error| format!("Invalid mountpoint encoding: {error}"))?,
        );
        let source = decode_mount_field(fields[separator + 2])?;
        let owned = if fields[separator + 1] == b"fuse.sshfs" && fields[3] == b"/" {
            String::from_utf8(source).ok().and_then(|source| {
                source
                    .strip_prefix("oc-mount:")
                    .filter(|source| valid_source(source))
                    .map(str::to_owned)
            })
        } else {
            None
        };
        if mounts.insert(mountpoint.clone(), owned).is_some() {
            return Err(format!("Multiple mounts at {}", mountpoint.display()));
        }
    }
    Ok(mounts)
}

fn valid_source(source: &str) -> bool {
    let Some((user, location)) = source.split_once('@') else {
        return false;
    };
    let Some((ip, remote)) = location.split_once(':') else {
        return false;
    };
    validate_user(user).is_ok()
        && ip
            .parse::<Ipv4Addr>()
            .is_ok_and(|ip| ip.octets()[0] == 100 && (64..=127).contains(&ip.octets()[1]))
        && matches!(remote, "" | "Library/Mobile Documents")
}

fn decode_mount_field(field: &[u8]) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::with_capacity(field.len());
    let mut remaining = field;
    while let Some((&byte, rest)) = remaining.split_first() {
        if byte == b'\\' {
            let escaped = remaining.get(1..4).ok_or("Truncated mountpoint escape")?;
            bytes.push(match escaped {
                b"040" => b' ',
                b"011" => b'\t',
                b"012" => b'\n',
                b"134" => b'\\',
                _ => return Err("Invalid mountpoint escape".into()),
            });
            remaining = &remaining[4..];
        } else {
            bytes.push(byte);
            remaining = rest;
        }
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests;
