use std::io::{ErrorKind, Read};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail, ensure};
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::fs::{OFlags, fcntl_getfl, fcntl_setfl};
use uuid::Uuid;
use wl_clipboard_rs::{copy, paste};

use crate::model::{Clip, MAX_BYTES};

/// Probe Wayland clipboard support.
///
/// Takes no arguments and returns success when data-control is usable, including
/// an empty clipboard. Does not read or change clipboard contents.
pub(crate) fn probe() -> Result<()> {
    match paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified) {
        Ok(_) | Err(paste::Error::ClipboardEmpty) => Ok(()),
        Err(error) => Err(error).context("Wayland data-control clipboard is unavailable"),
    }
}

/// Capture the local selection.
///
/// Takes no arguments; returns bounded UTF-8 text and PNG or no supported data.
/// Sensitive selections are excluded. Reads have a two-second total deadline.
pub(crate) fn capture() -> Result<Option<Clip>> {
    let formats =
        match paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified) {
            Ok(formats) => formats,
            Err(paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => return Ok(None),
            Err(error) => return Err(error.into()),
        };
    if formats.iter().any(|format| sensitive(format)) {
        return Ok(None);
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    let text = read(
        paste::MimeType::TextWithPriority("text/plain;charset=utf-8"),
        MAX_BYTES,
        deadline,
    )?;
    let remaining = MAX_BYTES - text.as_ref().map_or(0, Vec::len);
    let png = if formats.contains("image/png") {
        read(paste::MimeType::Specific("image/png"), remaining, deadline)?
    } else {
        None
    };
    if text.is_none() && png.is_none() {
        return Ok(None);
    }
    let clip = Clip {
        id: Uuid::new_v4(),
        text,
        png,
    };
    clip.validate()?;
    Ok(Some(clip))
}

fn read(mime: paste::MimeType<'_>, limit: usize, deadline: Instant) -> Result<Option<Vec<u8>>> {
    let (mut pipe, _) = match paste::get_contents(
        paste::ClipboardType::Regular,
        paste::Seat::Unspecified,
        mime,
    ) {
        Ok(contents) => contents,
        Err(paste::Error::ClipboardEmpty | paste::Error::NoMimeType) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    fcntl_setfl(&pipe, fcntl_getfl(&pipe)? | OFlags::NONBLOCK)?;
    let mut bytes = Vec::new();
    let mut buffer = [0; 8192];
    loop {
        let remaining = deadline
            .checked_duration_since(Instant::now())
            .context("clipboard provider timed out")?;
        let timeout = Timespec::try_from(remaining)?;
        match poll(&mut [PollFd::new(&pipe, PollFlags::IN)], Some(&timeout)) {
            Ok(0) => bail!("clipboard provider timed out"),
            Ok(_) => {}
            Err(rustix::io::Errno::INTR) => continue,
            Err(error) => return Err(error.into()),
        }
        match pipe.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => {
                ensure!(bytes.len() + count <= limit, "clipboard exceeds 16 MiB");
                bytes.extend_from_slice(&buffer[..count]);
            }
            Err(error)
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::Interrupted) =>
            {
                continue;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(Some(bytes))
}

/// Serve a received selection in its dedicated keeper process.
///
/// Takes validated bytes and a readiness callback. Returns after another owner
/// replaces the selection; invokes the callback only after ownership is claimed.
pub(crate) fn serve(clip: &Clip, ready: impl FnOnce() -> Result<()>) -> Result<()> {
    clip.validate()?;
    let mut sources = Vec::with_capacity(2);
    for (mime, data) in [
        ("text/plain;charset=utf-8", &clip.text),
        ("image/png", &clip.png),
    ] {
        if let Some(data) = data {
            sources.push(copy::MimeSource {
                source: copy::Source::Bytes(data.clone().into_boxed_slice()),
                mime_type: copy::MimeType::Specific(mime.into()),
            });
        }
    }
    let mut options = copy::Options::new();
    options.foreground(true);
    let prepared = options
        .prepare_copy_multi(sources)
        .context("publish Wayland selection")?;
    ready()?;
    prepared.serve().context("serve copied clipboard bytes")
}

fn sensitive(format: &str) -> bool {
    let format = format.to_ascii_lowercase();
    ["concealed", "passwordmanagerhint", "keepass"]
        .iter()
        .any(|marker| format.contains(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn excludes_sensitive_clipboard_markers() {
        assert!(sensitive("x-kde-passwordManagerHint"));
        assert!(sensitive("application/x-nspasteboard-concealed-type"));
        assert!(!sensitive("text/plain;charset=utf-8"));
    }
}
