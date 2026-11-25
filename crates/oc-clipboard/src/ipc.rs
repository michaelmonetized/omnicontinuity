use std::fs::{File, OpenOptions};
use std::io::ErrorKind;
use std::os::unix::fs::{FileTypeExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail, ensure};
use rustix::fs::{FlockOperation, OFlags, flock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;
use tokio::task::JoinSet;
use tokio::time::timeout;

use crate::socket_path::Paths;

const STOP: &[u8; 4] = b"OCS1";
const STATUS: &[u8; 4] = b"OCQ1";
const DONE: &[u8; 4] = b"DONE";
const FAILED: &[u8; 4] = b"FAIL";
const RUNNING: &[u8; 4] = b"RUN1";
const STOPPED: &[u8; 4] = b"END1";

pub(crate) type Outcome = std::result::Result<(), String>;
pub(crate) type Completion = watch::Receiver<Option<Outcome>>;

pub(crate) struct Bindings {
    pub relay: UnixListener,
    pub control: UnixListener,
    pub guard: SocketGuard,
}

/// Own socket names and the service lock.
///
/// Cleanup removes only owned endpoint inodes, then explicitly unlocks the open
/// file description. Inherited descriptors cannot prolong ownership after that.
pub(crate) struct SocketGuard {
    sockets: Vec<(PathBuf, u64, u64)>,
    lock: File,
}

impl Drop for SocketGuard {
    fn drop(&mut self) {
        for (path, device, inode) in &self.sockets {
            if let Ok(metadata) = std::fs::symlink_metadata(path)
                && metadata.file_type().is_socket()
                && metadata.dev() == *device
                && metadata.ino() == *inode
            {
                let _ = std::fs::remove_file(path);
            }
        }
        let _ = flock(&self.lock, FlockOperation::Unlock);
    }
}

/// Bind this user's relay and control endpoints.
///
/// Takes private runtime paths and returns listeners plus an ownership guard.
/// An exclusive file lock protects stale-socket recovery and concurrent starts.
/// Files, symlinks, and live legacy listeners are never replaced.
pub(crate) fn bind(paths: &Paths) -> Result<Bindings> {
    validate_directory(paths.lock.parent().context("missing runtime directory")?)?;
    let lock = open_lock(&paths.lock, true)?;
    flock(&lock, FlockOperation::NonBlockingLockExclusive)
        .context("oc-clipboard is already running for this user")?;
    let mut guard = SocketGuard {
        sockets: Vec::new(),
        lock,
    };
    let relay = bind_socket(&paths.relay, &mut guard)?;
    let control = bind_socket(&paths.control, &mut guard)?;
    Ok(Bindings {
        relay,
        control,
        guard,
    })
}

fn validate_directory(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).with_context(|| {
        format!(
            "clipboard runtime directory is unavailable: {}",
            path.display()
        )
    })?;
    ensure!(
        metadata.is_dir()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o077 == 0,
        "clipboard runtime directory must be a private directory owned by this user"
    );
    Ok(())
}

fn open_lock(path: &Path, create: bool) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(create)
        .truncate(false)
        .mode(0o600)
        .custom_flags(OFlags::NOFOLLOW.bits() as i32)
        .open(path)?;
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == rustix::process::geteuid().as_raw()
            && metadata.mode() & 0o077 == 0,
        "clipboard lock must be a private regular file owned by this user"
    );
    Ok(file)
}

fn bind_socket(path: &Path, guard: &mut SocketGuard) -> Result<UnixListener> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            ensure!(
                metadata.file_type().is_socket()
                    && metadata.uid() == rustix::process::geteuid().as_raw(),
                "refusing to replace a non-socket or another user's clipboard endpoint"
            );
            match std::os::unix::net::UnixStream::connect(path) {
                Ok(_) => bail!("clipboard endpoint is active; refusing to replace it"),
                Err(error)
                    if matches!(
                        error.kind(),
                        ErrorKind::NotFound | ErrorKind::ConnectionRefused
                    ) => {}
                Err(error) => return Err(error).context("check old clipboard endpoint"),
            }
            std::fs::remove_file(path)?;
        }
        Err(error) if error.kind() == ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let socket = UnixListener::bind(path)?;
    let metadata = std::fs::symlink_metadata(path)?;
    guard
        .sockets
        .push((path.to_owned(), metadata.dev(), metadata.ino()));
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(socket)
}

/// Observe a service result without owning its task.
///
/// Takes a completion receiver and returns the original service error or success.
/// Cancelling this future leaves the service and its ownership state untouched.
pub(crate) async fn wait(mut completion: Completion) -> Result<()> {
    loop {
        if let Some(outcome) = completion.borrow_and_update().clone() {
            return outcome.map_err(anyhow::Error::msg);
        }
        completion
            .changed()
            .await
            .context("clipboard service ended without a shutdown result")?;
    }
}

/// Stop the running user's service through IPC.
///
/// Takes runtime paths and returns after socket cleanup and worker shutdown are
/// acknowledged. An absent unlocked service succeeds. No PID or signal is used.
pub(crate) async fn stop(paths: &Paths) -> Result<()> {
    timeout(Duration::from_secs(15), async {
        let mut socket = match UnixStream::connect(&paths.control).await {
            Ok(socket) => socket,
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                return ensure_stopped(paths).await;
            }
            Err(error) => return Err(error).context("connect to clipboard control socket"),
        };
        ensure_uid(&socket)?;
        socket.write_all(STOP).await?;
        let mut reply = [0; 4];
        socket
            .read_exact(&mut reply)
            .await
            .context("clipboard stop was not acknowledged")?;
        match &reply {
            DONE => Ok(()),
            FAILED => {
                let length = socket.read_u32().await? as usize;
                ensure!(length <= 4096, "invalid clipboard shutdown error length");
                let mut message = vec![0; length];
                socket.read_exact(&mut message).await?;
                bail!(
                    "clipboard shutdown failed: {}",
                    String::from_utf8_lossy(&message)
                )
            }
            _ => bail!("invalid clipboard stop acknowledgement"),
        }
    })
    .await
    .context("clipboard shutdown acknowledgement timed out")?
}

async fn ensure_stopped(paths: &Paths) -> Result<()> {
    match open_lock(&paths.lock, false) {
        Ok(lock) => {
            flock(&lock, FlockOperation::NonBlockingLockExclusive).context(
                "clipboard service owns its lock but its control endpoint is unavailable",
            )?;
            flock(&lock, FlockOperation::Unlock).context("release stopped clipboard probe lock")?;
        }
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|error| error.kind() == ErrorKind::NotFound) => {}
        Err(error) => return Err(error),
    }
    match UnixStream::connect(&paths.relay).await {
        Ok(_) => bail!(
            "clipboard relay is active without a control endpoint; stop that legacy service from its original session"
        ),
        Err(error)
            if matches!(
                error.kind(),
                ErrorKind::NotFound | ErrorKind::ConnectionRefused
            ) =>
        {
            Ok(())
        }
        Err(error) => Err(error).context("check clipboard relay state"),
    }
}

/// Query whether clipboard sync is active.
///
/// Takes runtime paths and returns a bounded, same-UID IPC status. Failure is
/// reported as false; callers owning a service use `wait` for its actual error.
pub(crate) async fn is_running(paths: &Paths) -> bool {
    timeout(Duration::from_millis(250), async {
        let mut socket = UnixStream::connect(&paths.control).await?;
        ensure_uid(&socket)?;
        socket.write_all(STATUS).await?;
        let mut reply = [0; 4];
        socket.read_exact(&mut reply).await?;
        Ok::<_, anyhow::Error>(&reply == RUNNING)
    })
    .await
    .is_ok_and(|result| result.unwrap_or(false))
}

pub(crate) fn ensure_uid(socket: &UnixStream) -> Result<()> {
    ensure!(
        socket.peer_cred()?.uid() == rustix::process::geteuid().as_raw(),
        "clipboard IPC peer UID mismatch"
    );
    Ok(())
}

/// Serve local control requests.
///
/// Takes a control listener, cancellation sender, and worker-cleanup completion.
/// Returns after bounded clients finish; stop replies wait for worker cleanup.
pub(crate) async fn serve(
    listener: UnixListener,
    cancel: watch::Sender<bool>,
    mut ready: Completion,
) -> Result<()> {
    let mut clients = JoinSet::new();
    while ready.borrow().is_none() {
        tokio::select! {
            biased;
            result = ready.changed() => {
                result.context("clipboard cleanup ended without a completion result")?;
            },
            result = clients.join_next(), if !clients.is_empty() => {
                if let Some(Err(error)) = result {
                    tracing::warn!(%error, "clipboard control task failed");
                }
            }
            accepted = listener.accept() => {
                let (socket, _) = accepted?;
                if clients.len() >= 32 || ensure_uid(&socket).is_err() {
                    continue;
                }
                let cancel = cancel.clone();
                let ready = ready.clone();
                clients.spawn(async move {
                    if let Err(error) = control(socket, cancel, ready).await {
                        tracing::debug!(%error, "clipboard control client closed");
                    }
                });
            }
        }
    }
    drop(listener);
    while let Some(result) = clients.join_next().await {
        if let Err(error) = result {
            tracing::warn!(%error, "clipboard control task failed during shutdown");
        }
    }
    Ok(())
}

async fn control(
    mut socket: UnixStream,
    cancel: watch::Sender<bool>,
    ready: Completion,
) -> Result<()> {
    let mut request = [0; 4];
    timeout(Duration::from_secs(2), socket.read_exact(&mut request)).await??;
    match &request {
        STATUS => {
            let reply = if *cancel.borrow() { STOPPED } else { RUNNING };
            timeout(Duration::from_secs(2), socket.write_all(reply)).await??;
        }
        STOP => {
            let _ = cancel.send(true);
            let outcome = wait(ready).await;
            timeout(Duration::from_secs(2), async {
                match outcome {
                    Ok(()) => socket.write_all(DONE).await?,
                    Err(error) => {
                        let message = error.to_string();
                        let bytes = message.as_bytes();
                        let bytes = &bytes[..bytes.len().min(4096)];
                        socket.write_all(FAILED).await?;
                        socket.write_u32(bytes.len() as u32).await?;
                        socket.write_all(bytes).await?;
                    }
                }
                socket.shutdown().await
            })
            .await??;
        }
        _ => bail!("invalid clipboard control request"),
    }
    Ok(())
}

#[cfg(test)]
pub(crate) mod tests;
