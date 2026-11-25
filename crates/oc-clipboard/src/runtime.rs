use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::process::Command;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio::time::{MissedTickBehavior, interval, sleep, timeout};

use crate::model::{Clip, Dedupe};
use crate::{ClipboardSync, Config, backend, ipc, ownership, process, protocol, socket_path};

type Latest = watch::Receiver<Option<Arc<Clip>>>;

const REMOTE_BRIDGE: &str = "if command -v oc >/dev/null 2>&1; then exec oc clipboard bridge; else exec oc-clipboard-bridge; fi";

/// Start the Linux service.
///
/// Takes configuration, checks targets and the display, and returns a cancellable
/// handle after binding a user-only relay socket. No peer login is performed here.
pub(crate) async fn start(mut config: Config) -> Result<ClipboardSync> {
    for peer in &config.peers {
        validate_target(peer)?;
    }
    config.peers.sort();
    config.peers.dedup();
    ensure!(
        config.peers.len() <= 32,
        "at most 32 clipboard peers are supported"
    );
    tokio::task::spawn_blocking(backend::probe).await??;
    let bindings = ipc::bind(&socket_path::paths())?;
    let (shutdown, receiver) = watch::channel(false);
    let cancel = shutdown.clone();
    let (completed, finished) = watch::channel(None);
    let task = tokio::spawn(async move {
        let result = run(config, bindings, cancel, receiver).await;
        completed.send_replace(Some(
            result
                .as_ref()
                .map(|_| ())
                .map_err(|error| format!("{error:#}")),
        ));
        result
    });
    Ok(ClipboardSync {
        shutdown,
        finished,
        task: Some(task),
    })
}

fn validate_target(target: &str) -> Result<()> {
    ensure!(
        !target.is_empty()
            && !target.starts_with('-')
            && target
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b".-_@:%[]".contains(&byte)),
        "peer must be an SSH alias or user@host target, without options or remote commands"
    );
    Ok(())
}

async fn run(
    config: Config,
    bindings: ipc::Bindings,
    cancel: watch::Sender<bool>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    let ipc::Bindings {
        relay: listener,
        control,
        guard,
    } = bindings;
    let (ready, completion) = watch::channel(None);
    let mut control_task = tokio::spawn(ipc::serve(control, cancel.clone(), completion));
    let mut control_finished = false;
    let (latest, _) = watch::channel(None);
    let (incoming, mut received) = mpsc::channel::<Arc<Clip>>(1);
    let mut tasks = JoinSet::new();
    for peer in config.peers {
        tasks.spawn(peer_loop(
            peer,
            latest.subscribe(),
            incoming.clone(),
            shutdown.clone(),
        ));
    }
    let mut dedupe = Dedupe::default();
    let mut baseline = true;
    let mut tick = interval(Duration::from_millis(250));
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut result: Result<()> = async {
        loop {
            if *shutdown.borrow() {
                break;
            }
            tokio::select! {
                biased;
                _ = shutdown.changed() => break,
                result = &mut control_task => {
                    control_finished = true;
                    result??;
                    anyhow::bail!("clipboard control server stopped unexpectedly");
                }
                joined = tasks.join_next(), if !tasks.is_empty() => {
                    if let Some(joined) = joined {
                        joined.context("clipboard channel task failed")??;
                    }
                }
                accepted = listener.accept() => {
                    let (socket, _) = accepted?;
                    if tasks.len() >= 64 {
                        continue;
                    }
                    if ipc::ensure_uid(&socket).is_err() {
                        continue;
                    }
                    let (reader, writer) = socket.into_split();
                    let latest = latest.subscribe();
                    let incoming = incoming.clone();
                    let shutdown = shutdown.clone();
                    tasks.spawn(async move {
                        if let Err(error) = connection(reader, writer, latest, incoming, shutdown).await {
                            tracing::debug!(%error, "incoming clipboard channel closed");
                        }
                        Ok(())
                    });
                }
                Some(clip) = received.recv() => {
                    if !dedupe.accepts(&clip) {
                        dedupe.remember_id(clip.id);
                        continue;
                    }
                    let result = ownership::publish(&clip).await;
                    match result {
                        Ok(()) => {
                            dedupe.remember(&clip);
                            baseline = false;
                            latest.send_replace(Some(clip));
                        }
                        Err(error) => tracing::warn!(%error, "received clipboard could not be published"),
                    }
                }
                _ = tick.tick() => {
                    let result = tokio::task::spawn_blocking(backend::capture).await?;
                    match result {
                        Ok(Some(clip)) => {
                            if baseline {
                                dedupe.remember(&clip);
                                baseline = false;
                            } else if dedupe.accepts(&clip) {
                                dedupe.remember(&clip);
                                latest.send_replace(Some(Arc::new(clip)));
                            }
                        }
                        Ok(None) => {
                            dedupe.clear();
                            baseline = false;
                            latest.send_replace(None);
                        }
                        Err(error) => tracing::debug!(%error, "clipboard capture will retry"),
                    }
                }
            }
        }
        Ok(())
    }.await;
    let _ = cancel.send(true);
    drop(listener);
    drop(latest);
    drop(received);
    while let Some(joined) = tasks.join_next().await {
        let joined = joined
            .context("clipboard channel task failed during shutdown")
            .and_then(|result| result);
        if let Err(error) = joined {
            if result.is_ok() {
                result = Err(error);
            } else {
                tracing::warn!(%error, "additional clipboard channel shutdown failure");
            }
        }
    }
    drop(guard);
    ready.send_replace(Some(
        result
            .as_ref()
            .map(|_| ())
            .map_err(|error| format!("{error:#}")),
    ));
    if !control_finished {
        let control_result = control_task
            .await
            .context("clipboard control server failed")?;
        if result.is_ok() {
            control_result?;
        }
    }
    result
}

async fn connection<R, W>(
    mut reader: R,
    mut writer: W,
    mut latest: Latest,
    incoming: mpsc::Sender<Arc<Clip>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    if *shutdown.borrow() {
        return Ok(());
    }
    let receive = async {
        loop {
            let clip = match protocol::read(&mut reader).await {
                Ok(clip) => clip,
                Err(error) => return Err(error),
            };
            if incoming.send(Arc::new(clip)).await.is_err() {
                return Err(anyhow::anyhow!("clipboard service stopped"));
            }
        }
    };
    let send = async {
        loop {
            let current = latest.borrow_and_update().clone();
            if let Some(clip) = current {
                timeout(Duration::from_secs(15), protocol::write(&mut writer, &clip))
                    .await
                    .context("clipboard write timed out")??;
            }
            if latest.changed().await.is_err() {
                return Ok::<(), anyhow::Error>(());
            }
        }
    };
    tokio::select! {
        result = receive => result,
        result = send => result,
        _ = shutdown.changed() => Ok(()),
    }
}

fn ssh_command(peer: &str) -> Command {
    let mut command = Command::new("ssh");
    command
        .args([
            "-T",
            "-oBatchMode=yes",
            "-oStrictHostKeyChecking=yes",
            "-oPasswordAuthentication=no",
            "-oKbdInteractiveAuthentication=no",
            "-oClearAllForwardings=yes",
            "-oControlMaster=no",
            "-oControlPersist=no",
            "-oForkAfterAuthentication=no",
            "-oConnectTimeout=10",
            "-oServerAliveInterval=15",
            "-oServerAliveCountMax=2",
            "--",
            peer,
            REMOTE_BRIDGE,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true);
    command
}

async fn peer_loop(
    peer: String,
    latest: Latest,
    incoming: mpsc::Sender<Arc<Clip>>,
    mut shutdown: watch::Receiver<bool>,
) -> Result<()> {
    loop {
        if *shutdown.borrow() {
            return Ok(());
        }
        match ssh_command(&peer).spawn() {
            Ok(mut child) => {
                let reader = child.stdout.take().expect("piped SSH stdout");
                let writer = child.stdin.take().expect("piped SSH stdin");
                if let Err(error) = connection(
                    reader,
                    writer,
                    latest.clone(),
                    incoming.clone(),
                    shutdown.clone(),
                )
                .await
                {
                    tracing::warn!(%peer, %error, "SSH clipboard channel closed");
                }
                process::finish(&mut child, Duration::from_secs(3))
                    .await
                    .with_context(|| format!("clean up owned SSH channel for {peer}"))?;
            }
            Err(error) => tracing::warn!(%peer, %error, "could not open SSH clipboard channel"),
        }
        if *shutdown.borrow() {
            return Ok(());
        }
        tokio::select! {
            _ = shutdown.changed() => return Ok(()),
            _ = sleep(Duration::from_secs(3)) => {},
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[test]
    fn targets_cannot_inject_options_or_remote_commands() {
        for target in ["my-peer", "user@100.1.2.3", "user@[::1]"] {
            validate_target(target).unwrap();
        }
        for target in [
            "",
            "-oProxyCommand=bad",
            "host reboot",
            "host;reboot",
            "host\ncommand",
        ] {
            assert!(validate_target(target).is_err());
        }
        let command = ssh_command("my-peer");
        let args: Vec<_> = command
            .as_std()
            .get_args()
            .map(|arg| arg.to_str().unwrap())
            .collect();
        assert!(args.contains(&"-oBatchMode=yes"));
        assert!(args.contains(&"-oStrictHostKeyChecking=yes"));
        assert!(args.contains(&"-oControlMaster=no"));
        assert!(args.contains(&"-oControlPersist=no"));
        assert!(args.contains(&"-oForkAfterAuthentication=no"));
        assert_eq!(&args[args.len() - 3..], ["--", "my-peer", REMOTE_BRIDGE]);
    }

    #[tokio::test]
    async fn channel_sends_receives_and_cancels_without_a_display() {
        let (local, mut peer) = tokio::io::duplex(1024);
        let (reader, writer) = tokio::io::split(local);
        let (latest, updates) = watch::channel(None);
        let (incoming, mut received) = mpsc::channel(1);
        let (shutdown, stop) = watch::channel(false);
        let task = tokio::spawn(connection(reader, writer, updates, incoming, stop));
        let outgoing = Arc::new(Clip {
            id: Uuid::new_v4(),
            text: Some(b"local".to_vec()),
            png: None,
        });
        latest.send_replace(Some(outgoing.clone()));
        assert_eq!(protocol::read(&mut peer).await.unwrap(), *outgoing);
        let remote = Clip {
            id: Uuid::new_v4(),
            text: None,
            png: Some(b"\x89PNG\r\n\x1a\nremote".to_vec()),
        };
        protocol::write(&mut peer, &remote).await.unwrap();
        assert_eq!(*received.recv().await.unwrap(), remote);
        shutdown.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stopping_interrupts_a_blocked_channel_write() {
        let (local, _unread_peer) = tokio::io::duplex(1);
        let (reader, writer) = tokio::io::split(local);
        let (latest, updates) = watch::channel(None);
        let (incoming, _received) = mpsc::channel(1);
        let (shutdown, stop) = watch::channel(false);
        let task = tokio::spawn(connection(reader, writer, updates, incoming, stop));
        latest.send_replace(Some(Arc::new(Clip {
            id: Uuid::new_v4(),
            text: Some(b"cannot fit".to_vec()),
            png: None,
        })));
        tokio::task::yield_now().await;
        shutdown.send(true).unwrap();
        timeout(Duration::from_secs(1), task)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }
}
