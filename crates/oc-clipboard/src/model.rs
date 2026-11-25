use std::collections::{HashSet, VecDeque};

use anyhow::{Result, ensure};
use sha2::{Digest, Sha256};
use uuid::Uuid;

pub(crate) const MAX_BYTES: usize = 16 * 1024 * 1024;
const HISTORY: usize = 4096;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Clip {
    pub id: Uuid,
    pub text: Option<Vec<u8>>,
    pub png: Option<Vec<u8>>,
}

impl Clip {
    /// Validate portable clipboard data.
    ///
    /// Takes this clip and returns success for bounded UTF-8 text and PNG bytes.
    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.text.is_some() || self.png.is_some(),
            "empty clipboard frame"
        );
        ensure!(
            self.text.as_ref().map_or(0, Vec::len) + self.png.as_ref().map_or(0, Vec::len)
                <= MAX_BYTES,
            "clipboard exceeds 16 MiB"
        );
        if let Some(text) = &self.text {
            std::str::from_utf8(text)?;
        }
        if let Some(png) = &self.png {
            ensure!(
                png.starts_with(b"\x89PNG\r\n\x1a\n"),
                "invalid PNG signature"
            );
        }
        Ok(())
    }

    /// Fingerprint content.
    ///
    /// Takes this clip and returns SHA-256 over typed, length-delimited bytes,
    /// excluding its transport ID so local ownership changes cannot cause echoes.
    pub fn fingerprint(&self) -> [u8; 32] {
        let mut hash = Sha256::new();
        for (mime, data) in [
            (b"text/plain".as_slice(), &self.text),
            (b"image/png".as_slice(), &self.png),
        ] {
            hash.update([u8::from(data.is_some())]);
            if let Some(data) = data {
                hash.update((mime.len() as u64).to_be_bytes());
                hash.update(mime);
                hash.update((data.len() as u64).to_be_bytes());
                hash.update(data);
            }
        }
        hash.finalize().into()
    }
}

#[derive(Default)]
pub(crate) struct Dedupe {
    current: Option<[u8; 32]>,
    seen: HashSet<Uuid>,
    order: VecDeque<Uuid>,
}

impl Dedupe {
    /// Check incoming content.
    ///
    /// Takes a clip and returns whether its ID and content need publishing.
    pub fn accepts(&self, clip: &Clip) -> bool {
        !self.seen.contains(&clip.id) && self.current != Some(clip.fingerprint())
    }

    /// Remember successful publication.
    ///
    /// Takes an applied clip, records its fingerprint and bounded event ID, and
    /// returns nothing. Failed clipboard writes must never call this function.
    pub fn remember(&mut self, clip: &Clip) {
        self.current = Some(clip.fingerprint());
        self.remember_id(clip.id);
    }

    /// Remember a duplicate event.
    ///
    /// Takes a received event ID and returns nothing. Keeps delayed mesh echoes
    /// suppressed after the local clipboard changes, without changing its hash.
    pub fn remember_id(&mut self, id: Uuid) {
        if self.seen.insert(id) {
            self.order.push_back(id);
        }
        if self.order.len() > HISTORY {
            self.seen.remove(&self.order.pop_front().unwrap());
        }
    }

    /// Forget the local selection.
    ///
    /// Takes no arguments and returns nothing, allowing a later copy of the
    /// same bytes after the clipboard has been cleared or become unsupported.
    pub fn clear(&mut self) {
        self.current = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &str) -> Clip {
        Clip {
            id: Uuid::new_v4(),
            text: Some(value.as_bytes().to_vec()),
            png: None,
        }
    }

    #[test]
    fn fingerprint_and_dedupe_without_a_display() {
        let first = text("hello 世界\n");
        let echo = text("hello 世界\n");
        assert_eq!(first.fingerprint(), echo.fingerprint());
        assert_ne!(first.fingerprint(), text("hello 世界").fingerprint());
        let mut dedupe = Dedupe::default();
        assert!(dedupe.accepts(&first));
        dedupe.remember(&first);
        assert!(!dedupe.accepts(&echo));
        let second = text("another copy");
        dedupe.remember(&second);
        assert!(!dedupe.accepts(&first));
        assert!(dedupe.accepts(&echo));
        dedupe.remember(&echo);
        dedupe.clear();
        assert!(dedupe.accepts(&text("hello 世界\n")));
    }

    #[test]
    fn fingerprint_separates_types_and_absent_from_empty() {
        let bytes = b"\x89PNG\r\n\x1a\n".to_vec();
        let png = Clip {
            id: Uuid::new_v4(),
            text: None,
            png: Some(bytes.clone()),
        };
        let text = Clip {
            id: png.id,
            text: Some(bytes),
            png: None,
        };
        assert_ne!(png.fingerprint(), text.fingerprint());
        let empty_text = Clip {
            id: png.id,
            text: Some(vec![]),
            png: None,
        };
        let empty_png = Clip {
            id: png.id,
            text: None,
            png: Some(vec![]),
        };
        assert_ne!(empty_text.fingerprint(), empty_png.fingerprint());
    }

    #[test]
    fn dedupe_history_is_bounded() {
        let mut dedupe = Dedupe::default();
        let first = text("first");
        dedupe.remember(&first);
        for index in 0..HISTORY {
            dedupe.remember(&text(&index.to_string()));
        }
        assert_eq!(dedupe.seen.len(), HISTORY);
        assert!(dedupe.accepts(&first));
    }

    #[test]
    fn duplicate_event_cannot_replay_after_another_copy() {
        let mut dedupe = Dedupe::default();
        let first = text("first");
        dedupe.remember(&first);
        let echo = text("first");
        assert!(!dedupe.accepts(&echo));
        dedupe.remember_id(echo.id);
        dedupe.remember(&text("second"));
        assert!(!dedupe.accepts(&echo));
        assert!(dedupe.accepts(&text("first")));
    }
}
