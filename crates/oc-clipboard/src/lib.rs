//! Sync text and PNG through trusted, noninteractive SSH connections.
//!
//! Call `start(Config { peers: vec!["user@host".into()] }).await?`, then call
//! `stop().await?` to shut down, including from a separate invocation. Both hosts
//! must run this library through `oc`, with `oc` on the peer's SSH command PATH.
//! `bridge()` implements `oc clipboard bridge`; the old helper remains supported.
//! SSH uses existing keys, agents, and configured multiplexed connections. Host
//! keys must already be trusted. No credentials or host-key policy are changed.
//! A running Tokio runtime and a Wayland session with data-control are required.
//! The bridge relays bytes into the running service; it never owns a display.
//! Stopping sync preserves the last published clipboard until another app copies.

#[cfg(any(target_os = "linux", test))]
mod model;
#[cfg(any(target_os = "linux", test))]
mod protocol;

#[cfg(target_os = "linux")]
mod backend;
#[cfg(target_os = "linux")]
mod bridge;
#[cfg(target_os = "linux")]
mod ipc;
#[cfg(target_os = "linux")]
mod ownership;
#[cfg(target_os = "linux")]
mod process;
#[cfg(target_os = "linux")]
mod runtime;
#[cfg(target_os = "linux")]
mod socket_path;
#[cfg(target_os = "linux")]
mod stdio;

use anyhow::Result;

/// SSH destinations.
///
/// `peers` contains SSH aliases or `user@host` targets, using ~/.ssh/config for
/// ports, identities, jump hosts, and existing authenticated connections.
#[derive(Clone, Debug, Default)]
pub struct Config {
    pub peers: Vec<String>,
}

#[cfg(target_os = "linux")]
static SERVICE: tokio::sync::Mutex<Option<ClipboardSync>> = tokio::sync::Mutex::const_new(None);

#[cfg(target_os = "linux")]
struct ClipboardSync {
    shutdown: tokio::sync::watch::Sender<bool>,
    finished: ipc::Completion,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
}

/// Start clipboard sync.
///
/// Takes peer SSH destinations and returns after starting this process's service. Fails if
/// Wayland/data-control is unavailable, a target is invalid, or this user's
/// clipboard service already owns the socket. Peer outages reconnect in the
/// background and are reported through tracing. Keep the Tokio runtime alive
/// until `stop` finishes. macOS is unsupported this pass.
pub async fn start(config: Config) -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut active = SERVICE.lock().await;
        anyhow::ensure!(
            active.is_none(),
            "oc-clipboard is already running in this process"
        );
        *active = Some(runtime::start(config).await?);
        Ok(())
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = config;
        anyhow::bail!(
            "oc-clipboard is unsupported on this platform; Linux Wayland is required (macOS backend is not implemented)"
        )
    }
}

/// Stop clipboard sync.
///
/// Takes no arguments and stops this user's service, including a different oc
/// process, through authenticated local IPC. Returns after acknowledged worker
/// and socket cleanup, or an error. Calling it when stopped succeeds. Stalled SSH
/// children created by this service are terminated and reaped; existing masters
/// and unrelated processes are never signalled. Ownership keepers remain until the
/// selection changes, preserving copied bytes after the service process exits.
pub async fn stop() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut active = SERVICE.lock().await;
        if let Some(mut service) = active.take() {
            let _ = service.shutdown.send(true);
            if let Some(task) = service.task.take() {
                task.await??;
            }
        } else {
            ipc::stop(&socket_path::paths()).await?;
        }
    }
    Ok(())
}

/// Stop only the service owned by this process.
///
/// Takes no arguments and joins its owned service, preserving shutdown errors.
/// Never sends IPC to another instance, even if a newer service has replaced the
/// finished one. Safe for shelf cleanup. Returns success when nothing is owned.
pub async fn stop_local() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let mut active = SERVICE.lock().await;
        if let Some(mut service) = active.take() {
            let _ = service.shutdown.send(true);
            if let Some(task) = service.task.take() {
                task.await??;
            }
        }
    }
    Ok(())
}

/// Wait for this process's clipboard service.
///
/// Takes no arguments and returns its original completion result after control
/// replies and cleanup finish. Cancellation never stops or relinquishes the
/// service. Call `stop_local` to reap ownership before restarting. Fails if this
/// process has no owned service; never waits on an unrelated process.
pub async fn wait() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        let completion = SERVICE
            .lock()
            .await
            .as_ref()
            .map(|service| service.finished.clone())
            .ok_or_else(|| anyhow::anyhow!("this process does not own a clipboard service"))?;
        ipc::wait(completion).await
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("oc-clipboard requires Linux Wayland; macOS is not implemented")
}

/// Query whether clipboard sync is running.
///
/// Takes no arguments and returns owned-task liveness, or bounded per-user IPC
/// status when nothing is owned. False does not discard a failed task's error;
/// owners must use `wait` or `stop_local` to collect it.
pub async fn is_running() -> bool {
    #[cfg(target_os = "linux")]
    {
        if let Some(service) = SERVICE.lock().await.as_ref() {
            return service.finished.borrow().is_none()
                && service
                    .task
                    .as_ref()
                    .is_some_and(|task| !task.is_finished());
        }
        ipc::is_running(&socket_path::paths()).await
    }
    #[cfg(not(target_os = "linux"))]
    false
}

/// Relay an authenticated SSH channel.
///
/// Takes no arguments and forwards binary stdin/stdout to this user's running
/// clipboard service. Used by `oc clipboard bridge` and the legacy helper. No
/// display is opened and no control commands are accepted on this data channel.
pub async fn bridge() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        bridge::run().await
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("oc-clipboard bridge requires Linux Wayland; macOS is not implemented")
}

/// Keep one received selection available.
///
/// Takes one framed clip on stdin, acknowledges ownership on stdout, and serves
/// only those bytes until another owner replaces the selection. Implements the
/// internal `oc clipboard own` subprocess; never starts sync, IPC, or SSH.
pub async fn own() -> Result<()> {
    #[cfg(target_os = "linux")]
    {
        ownership::own().await
    }
    #[cfg(not(target_os = "linux"))]
    anyhow::bail!("oc-clipboard ownership requires Linux Wayland; macOS is not implemented")
}

#[cfg(target_os = "linux")]
impl Drop for ClipboardSync {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        let _ = self.shutdown.send(true);
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn public_api_rejects_invalid_peers_before_opening_a_display() {
        let error = start(Config {
            peers: vec!["host remote-command".into()],
        })
        .await
        .unwrap_err();
        assert!(error.to_string().contains("SSH alias"));
        stop_local().await.unwrap();
        stop_local().await.unwrap();

        let directory = ipc::tests::TestDirectory::new();
        let paths = directory.paths();
        *SERVICE.lock().await = Some(ipc::tests::service(&paths, Ok(())));
        assert!(is_running().await);
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(10), wait())
                .await
                .is_err()
        );
        assert!(SERVICE.lock().await.is_some());
        assert!(!*SERVICE.lock().await.as_ref().unwrap().shutdown.borrow());
        ipc::stop(&paths).await.unwrap();
        wait().await.unwrap();
        assert!(!is_running().await);
        assert!(SERVICE.lock().await.is_some());

        let mut replacement = ipc::tests::service(&paths, Ok(()));
        stop_local().await.unwrap();
        assert!(ipc::is_running(&paths).await);
        assert!(!*replacement.shutdown.borrow());
        ipc::stop(&paths).await.unwrap();
        replacement.task.take().unwrap().await.unwrap().unwrap();

        *SERVICE.lock().await = Some(ipc::tests::service(
            &paths,
            Err("owned worker failed".into()),
        ));
        SERVICE
            .lock()
            .await
            .as_ref()
            .unwrap()
            .shutdown
            .send(true)
            .unwrap();
        assert!(
            wait()
                .await
                .unwrap_err()
                .to_string()
                .contains("owned worker failed")
        );
        assert!(!is_running().await);
        assert!(
            stop_local()
                .await
                .unwrap_err()
                .to_string()
                .contains("owned worker failed")
        );
        assert!(SERVICE.lock().await.is_none());
    }
}
