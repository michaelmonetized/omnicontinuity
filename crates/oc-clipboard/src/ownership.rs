use std::path::Path;
use std::process::Stdio as ProcessStdio;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::process::Command;
use tokio::time::timeout;

use crate::{backend, model::Clip, process, protocol, stdio::Stdio};

const READY: &[u8; 4] = b"RDY1";

/// Publish through a dedicated copy owner.
///
/// Takes a validated clip and returns once the same oc executable has claimed
/// ownership in a separate process group. Closing sync cannot destroy its data;
/// the keeper exits naturally when another application replaces the selection.
pub(crate) async fn publish(clip: &Clip) -> Result<()> {
    clip.validate()?;
    let executable = std::env::current_exe().context("locate oc clipboard ownership executable")?;
    let child = keeper_command(&executable)
        .spawn()
        .context("start oc clipboard ownership keeper")?;
    let mut pending = process::PendingChild::new(child);
    let input = pending
        .child()
        .stdin
        .take()
        .context("clipboard keeper stdin unavailable")?;
    let output = pending
        .child()
        .stdout
        .take()
        .context("clipboard keeper stdout unavailable")?;
    let result = timeout(Duration::from_secs(5), handoff(clip, input, output))
        .await
        .context("clipboard ownership acknowledgement timed out")
        .and_then(|result| result);
    match result {
        Ok(()) => {
            let mut child = pending.release();
            tokio::spawn(async move {
                let _ = child.wait().await;
            });
            Ok(())
        }
        Err(error) => {
            process::terminate(pending.child()).await.with_context(|| {
                format!("clean up unacknowledged clipboard keeper after: {error:#}")
            })?;
            drop(pending.release());
            Err(error)
        }
    }
}

fn keeper_command(executable: &Path) -> Command {
    let mut command = Command::new(executable);
    command
        .args(["clipboard", "own"])
        .process_group(0)
        .stdin(ProcessStdio::piped())
        .stdout(ProcessStdio::piped())
        .stderr(ProcessStdio::null());
    command
}

async fn handoff(
    clip: &Clip,
    mut input: impl AsyncWrite + Unpin,
    mut output: impl AsyncRead + Unpin,
) -> Result<()> {
    protocol::write(&mut input, clip).await?;
    input.shutdown().await?;
    drop(input);
    let mut reply = [0; 4];
    output
        .read_exact(&mut reply)
        .await
        .context("clipboard keeper exited before claiming the selection")?;
    ensure!(
        &reply == READY,
        "invalid clipboard ownership acknowledgement"
    );
    Ok(())
}

pub(crate) async fn own() -> Result<()> {
    let mut input = Stdio::new(std::io::stdin())?;
    let clip = protocol::read(&mut input).await?;
    drop(input);
    tokio::task::spawn_blocking(move || {
        backend::serve(&clip, || {
            use std::io::Write;
            let mut output = std::io::stdout().lock();
            output.write_all(READY)?;
            output.flush()?;
            Ok(())
        })
    })
    .await?
}

#[cfg(test)]
mod tests {
    use super::*;
    use uuid::Uuid;

    #[tokio::test]
    async fn keeper_requires_acknowledged_text_and_png_handoff() {
        let clip = Clip {
            id: Uuid::new_v4(),
            text: Some("same bytes 世界".as_bytes().to_vec()),
            png: Some(b"\x89PNG\r\n\x1a\nbytes".to_vec()),
        };
        let (channel, mut keeper) = tokio::io::duplex(128);
        let (output, input) = tokio::io::split(channel);
        let pending = handoff(&clip, input, output);
        let copied = async {
            assert_eq!(protocol::read(&mut keeper).await.unwrap(), clip);
            let mut byte = [0];
            assert_eq!(keeper.read(&mut byte).await.unwrap(), 0);
            keeper.write_all(READY).await.unwrap();
        };
        let (result, ()) = tokio::join!(pending, copied);
        result.unwrap();
    }

    #[tokio::test]
    async fn keeper_failure_cannot_be_reported_as_success() {
        let clip = Clip {
            id: Uuid::new_v4(),
            text: Some(b"copy".to_vec()),
            png: None,
        };
        assert!(
            handoff(&clip, tokio::io::sink(), b"nope".as_slice())
                .await
                .is_err()
        );
        assert!(
            handoff(&clip, tokio::io::sink(), b"".as_slice())
                .await
                .is_err()
        );
        let command = keeper_command(Path::new("/path/to/oc"));
        assert_eq!(command.as_std().get_program(), "/path/to/oc");
        let args: Vec<_> = command.as_std().get_args().collect();
        assert_eq!(args, ["clipboard", "own"]);
    }
}
