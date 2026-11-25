use anyhow::{Context, Result};
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::UnixStream;

use crate::{ipc, socket_path, stdio::Stdio};

pub(crate) async fn run() -> Result<()> {
    let socket = UnixStream::connect(socket_path::paths().relay)
        .await
        .context("oc clipboard service is not running")?;
    ipc::ensure_uid(&socket)?;
    let input = Stdio::new(std::io::stdin())?;
    let output = Stdio::new(std::io::stdout())?;
    relay(input, output, socket).await
}

/// Relay clipboard bytes without interpreting control commands.
///
/// Takes the authenticated channel's reader/writer and local relay socket, then
/// returns on either side's EOF. Cancellation drops IO, without blocked threads.
pub(crate) async fn relay<R, W>(mut input: R, mut output: W, socket: UnixStream) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut reader, mut writer) = socket.into_split();
    tokio::select! {
        result = tokio::io::copy(&mut input, &mut writer) => { result?; },
        result = tokio::io::copy(&mut reader, &mut output) => { result?; },
    }
    output.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::{Duration, timeout};

    #[tokio::test]
    async fn bridge_is_bidirectional_and_exits_on_service_eof_with_idle_stdin() {
        let (socket, mut service) = UnixStream::pair().unwrap();
        let (mut channel, bridge_channel) = tokio::io::duplex(128);
        let (input, output) = tokio::io::split(bridge_channel);
        let bridge = tokio::spawn(relay(input, output, socket));
        channel.write_all(b"toward clipboard").await.unwrap();
        let mut sent = [0; 16];
        service.read_exact(&mut sent).await.unwrap();
        assert_eq!(&sent, b"toward clipboard");
        service.write_all(b"toward peer").await.unwrap();
        let mut received = [0; 11];
        channel.read_exact(&mut received).await.unwrap();
        assert_eq!(&received, b"toward peer");
        drop(service);
        timeout(Duration::from_secs(1), bridge)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn stdio_adapter_restores_flags_and_does_not_block_on_idle_input() {
        let (socket, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
        let original = rustix::fs::fcntl_getfl(&socket).unwrap();
        let mut adapter = Stdio::new(&socket).unwrap();
        assert!(
            rustix::fs::fcntl_getfl(&socket)
                .unwrap()
                .contains(rustix::fs::OFlags::NONBLOCK)
        );
        assert!(
            timeout(Duration::from_millis(10), adapter.read_u8())
                .await
                .is_err()
        );
        drop(adapter);
        assert_eq!(rustix::fs::fcntl_getfl(&socket).unwrap(), original);
    }
}
