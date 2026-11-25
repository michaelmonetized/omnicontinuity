//! Linux device and Tailscale certificate preparation previously in camera/run.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// Run a setup command. Accepts a command and returns its output or a useful error.
fn checked(command: &mut Command) -> Result<Output, String> {
    let output = command.output().map_err(|e| format!("{command:?}: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "{command:?} ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(output)
}

/// Prepare a loopback camera. Accepts its path and returns success only for the expected label and access.
pub(super) fn device(path: &Path) -> Result<(), String> {
    if !path.exists() {
        if path != Path::new("/dev/video42") {
            return Err(format!("camera device {} does not exist", path.display()));
        }
        checked(Command::new("sudo").args([
            "-n",
            "modprobe",
            "v4l2loopback",
            "devices=1",
            "video_nr=42",
            "exclusive_caps=1",
            "max_buffers=8",
            "card_label=iPhone Camera",
        ]))?;
        checked(Command::new("sudo").args([
            "-n",
            "udevadm",
            "trigger",
            "--subsystem-match=video4linux",
            "--action=change",
        ]))?;
        checked(Command::new("sudo").args(["-n", "udevadm", "settle", "--timeout=3"]))?;
    }
    let name = path.file_name().ok_or("camera device has no filename")?;
    let label =
        std::fs::read_to_string(Path::new("/sys/class/video4linux").join(name).join("name"))
            .map_err(|e| format!("camera label for {}: {e}", path.display()))?;
    if label.trim() != "iPhone Camera" {
        return Err(format!(
            "{} is labeled {:?}, expected iPhone Camera",
            path.display(),
            label.trim()
        ));
    }
    if let Err(error) = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
    {
        if error.kind() != std::io::ErrorKind::PermissionDenied {
            return Err(format!("open {}: {error}", path.display()));
        }
        let uid = checked(Command::new("id").arg("-u"))?;
        let uid = String::from_utf8_lossy(&uid.stdout)
            .trim()
            .parse::<u32>()
            .map_err(|e| format!("current user ID: {e}"))?;
        checked(
            Command::new("sudo")
                .args(["-n", "setfacl", "-m", &format!("u:{uid}:rw")])
                .arg(path),
        )?;
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(path)
            .map_err(|e| format!("open {} after granting access: {e}", path.display()))?;
    }
    Ok(())
}

/// Obtain TLS files. Accepts a state directory and MagicDNS name; returns certificate and private key paths.
pub(super) fn certificates(state: &Path, hostname: &str) -> Result<(PathBuf, PathBuf), String> {
    if hostname.is_empty()
        || !hostname.contains('.')
        || !hostname
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return Err("this machine has no valid MagicDNS hostname".to_owned());
    }
    let directory = state.join("certs");
    std::fs::create_dir_all(&directory).map_err(|e| format!("certificate directory: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("certificate directory permissions: {e}"))?;
    }
    let directory = directory
        .canonicalize()
        .map_err(|e| format!("certificate path: {e}"))?;
    let cert = directory.join("cert.pem");
    let key = directory.join("key.pem");
    let cert_args = [
        "cert",
        "--cert-file",
        cert.to_str().ok_or("certificate path is not UTF-8")?,
        "--key-file",
        key.to_str().ok_or("key path is not UTF-8")?,
        "--min-validity",
        "24h",
        hostname,
    ];
    if checked(Command::new("tailscale").args(cert_args)).is_err() {
        checked(
            Command::new("sudo")
                .args(["-n", "tailscale"])
                .args(cert_args),
        )?;
        let uid = checked(Command::new("id").arg("-u"))?;
        let gid = checked(Command::new("id").arg("-g"))?;
        let owner = format!(
            "{}:{}",
            String::from_utf8_lossy(&uid.stdout).trim(),
            String::from_utf8_lossy(&gid.stdout).trim()
        );
        checked(
            Command::new("sudo")
                .args(["-n", "chown", &owner])
                .arg(&cert)
                .arg(&key),
        )?;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| format!("private key permissions: {e}"))?;
    }
    Ok((cert, key))
}
