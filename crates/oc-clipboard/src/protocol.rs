use anyhow::{Result, ensure};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use uuid::Uuid;

use crate::model::{Clip, MAX_BYTES};

const MAGIC: &[u8; 4] = b"OCC1";

/// Write a clipboard frame.
///
/// Takes an authenticated writer and validated clip; returns after flushing its
/// versioned header and bounded raw bytes. This protocol is specific to oc.
pub(crate) async fn write(writer: &mut (impl AsyncWrite + Unpin), clip: &Clip) -> Result<()> {
    clip.validate()?;
    writer.write_all(MAGIC).await?;
    writer.write_all(clip.id.as_bytes()).await?;
    for data in [&clip.text, &clip.png] {
        writer
            .write_u32(data.as_ref().map_or(u32::MAX, |data| data.len() as u32))
            .await?;
    }
    for data in [&clip.text, &clip.png].into_iter().flatten() {
        writer.write_all(data).await?;
    }
    writer.flush().await?;
    Ok(())
}

/// Read a clipboard frame.
///
/// Takes an authenticated reader and returns a validated clip. Lengths are
/// checked together before any payload allocation; partial frames fail closed.
pub(crate) async fn read(reader: &mut (impl AsyncRead + Unpin)) -> Result<Clip> {
    let mut magic = [0; 4];
    reader.read_exact(&mut magic).await?;
    ensure!(&magic == MAGIC, "invalid oc clipboard protocol/version");
    let mut id = [0; 16];
    reader.read_exact(&mut id).await?;
    let lengths = [reader.read_u32().await?, reader.read_u32().await?];
    let total: u64 = lengths
        .iter()
        .filter(|&&len| len != u32::MAX)
        .map(|&len| u64::from(len))
        .sum();
    ensure!(total <= MAX_BYTES as u64, "clipboard frame exceeds 16 MiB");
    let mut parts = Vec::with_capacity(2);
    for len in lengths {
        parts.push(if len == u32::MAX {
            None
        } else {
            let mut data = vec![0; len as usize];
            reader.read_exact(&mut data).await?;
            Some(data)
        });
    }
    let clip = Clip {
        id: Uuid::from_bytes(id),
        text: parts.remove(0),
        png: parts.remove(0),
    };
    clip.validate()?;
    Ok(clip)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn text_and_png_roundtrip_over_a_duplex_channel() {
        let original = Clip {
            id: Uuid::new_v4(),
            text: Some("世界\n".as_bytes().to_vec()),
            png: Some(b"\x89PNG\r\n\x1a\nraw bytes\0".to_vec()),
        };
        let expected = original.clone();
        let (mut sender, mut receiver) = tokio::io::duplex(16);
        let (sent, received) = tokio::join!(write(&mut sender, &original), read(&mut receiver));
        sent.unwrap();
        assert_eq!(received.unwrap(), expected);
    }

    #[tokio::test]
    async fn oversized_header_is_rejected_before_reading_payload() {
        let mut bytes = MAGIC.to_vec();
        bytes.extend_from_slice(Uuid::nil().as_bytes());
        bytes.extend_from_slice(&(MAX_BYTES as u32).to_be_bytes());
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        assert!(
            read(&mut bytes.as_slice())
                .await
                .unwrap_err()
                .to_string()
                .contains("16 MiB")
        );
    }

    #[tokio::test]
    async fn invalid_magic_truncation_and_invalid_content_fail() {
        assert!(read(&mut b"bad!".as_slice()).await.is_err());
        assert!(read(&mut b"OCC1".as_slice()).await.is_err());
        let invalid = Clip {
            id: Uuid::nil(),
            text: Some(vec![0xff]),
            png: None,
        };
        assert!(write(&mut tokio::io::sink(), &invalid).await.is_err());
        let invalid = Clip {
            id: Uuid::nil(),
            text: None,
            png: Some(b"not png".to_vec()),
        };
        assert!(write(&mut tokio::io::sink(), &invalid).await.is_err());
    }
}
