use std::path::{Path, PathBuf};

/// Locate the authenticated user's relay socket.
///
/// Takes no arguments and returns the per-UID Linux runtime socket path. The
/// fixed location works from SSH without inheriting Wayland session variables.
pub(crate) fn paths() -> Paths {
    Paths::in_directory(Path::new(&format!(
        "/run/user/{}",
        rustix::process::geteuid().as_raw()
    )))
}

#[derive(Clone)]
pub(crate) struct Paths {
    pub relay: PathBuf,
    pub control: PathBuf,
    pub lock: PathBuf,
}

impl Paths {
    pub(crate) fn in_directory(directory: &Path) -> Self {
        Self {
            relay: directory.join("oc-clipboard.sock"),
            control: directory.join("oc-clipboard-control.sock"),
            lock: directory.join("oc-clipboard.lock"),
        }
    }
}
