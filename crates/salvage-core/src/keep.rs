//! Keeps the bytes of one file as an archive is unpacked, so a check can also show the file or
//! copy it out. The same unpacking that checks a file feeds this, so what is shown or copied is
//! exactly what was checked.
//!
//! Bytes go to memory, up to a limit, for a preview, or to a file on the destination for a
//! copy. Nothing is written anywhere unless a copy asked for it.

use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

#[derive(Default)]
struct Kept {
    memory: Vec<u8>,
    memory_limit: usize,
    file: Option<File>,
    hasher: Sha256,
    bytes: u64,
    truncated: bool,
    error: Option<String>,
}

/// Where unpacked bytes go. Cloning shares the same place.
#[derive(Clone, Default)]
pub struct Keep(Option<Arc<Mutex<Kept>>>);

/// What was kept once unpacking is over.
pub struct KeptBytes {
    pub bytes: u64,
    pub data: Vec<u8>,
    /// More was unpacked than the memory limit allowed, so `data` is the start only.
    pub truncated: bool,
    pub sha256: String,
    /// Writing to the destination failed, so the copy is not whole.
    pub error: Option<String>,
}

impl Keep {
    /// Keeps nothing: a plain check.
    pub fn nothing() -> Self {
        Self(None)
    }

    /// Keeps the first `limit` bytes in memory.
    pub fn in_memory(limit: usize) -> Self {
        Self(Some(Arc::new(Mutex::new(Kept {
            memory_limit: limit,
            ..Kept::default()
        }))))
    }

    /// Writes every byte to `file`.
    pub fn to_file(file: File) -> Self {
        Self(Some(Arc::new(Mutex::new(Kept {
            file: Some(file),
            ..Kept::default()
        }))))
    }

    fn add(&self, data: &[u8]) {
        let Some(kept) = &self.0 else { return };
        let Ok(mut kept) = kept.lock() else { return };
        kept.bytes += data.len() as u64;
        kept.hasher.update(data);
        let room = kept.memory_limit.saturating_sub(kept.memory.len());
        if kept.file.is_none() {
            if data.len() > room {
                kept.truncated = true;
            }
            let take = data.len().min(room);
            kept.memory.extend_from_slice(&data[..take]);
        }
        if kept.error.is_none() {
            let written = kept.file.as_mut().map(|file| file.write_all(data));
            if let Some(Err(error)) = written {
                kept.error = Some(error.to_string());
            }
        }
    }

    /// What was kept, with the file flushed to the destination.
    pub fn finish(self) -> KeptBytes {
        // A reader or writer may still hold a clone, so the contents are taken, not unwrapped.
        let taken = self
            .0
            .and_then(|kept| kept.lock().ok().map(|mut kept| std::mem::take(&mut *kept)))
            .unwrap_or_default();
        let mut error = taken.error;
        if let (None, Some(file)) = (&error, &taken.file) {
            if let Err(failed) = file.sync_all() {
                error = Some(failed.to_string());
            }
        }
        KeptBytes {
            bytes: taken.bytes,
            data: taken.memory,
            truncated: taken.truncated,
            sha256: taken
                .hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect(),
            error,
        }
    }
}

/// A reader that keeps a copy of everything read through it.
pub struct Tee<R> {
    inner: R,
    keep: Keep,
}

impl<R> Tee<R> {
    pub fn new(inner: R, keep: &Keep) -> Self {
        Self {
            inner,
            keep: keep.clone(),
        }
    }
}

impl<R: Read> Read for Tee<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        self.keep.add(&buffer[..count]);
        Ok(count)
    }
}

/// A writer that keeps a copy of everything written through it.
pub struct TeeWriter<W> {
    inner: W,
    keep: Keep,
}

impl<W> TeeWriter<W> {
    pub fn new(inner: W, keep: &Keep) -> Self {
        Self {
            inner,
            keep: keep.clone(),
        }
    }
}

impl<W: Write> Write for TeeWriter<W> {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        let count = self.inner.write(data)?;
        self.keep.add(&data[..count]);
        Ok(count)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn memory_keeps_the_start_and_says_when_it_stopped() {
        let keep = Keep::in_memory(4);
        let mut reader = Tee::new(&b"abcdefgh"[..], &keep);
        io::copy(&mut reader, &mut io::sink()).unwrap();
        drop(reader);
        let kept = keep.finish();
        assert_eq!(
            (kept.bytes, kept.data.as_slice(), kept.truncated),
            (8, &b"abcd"[..], true)
        );
        assert_eq!(
            kept.sha256,
            "9c56cc51b374c3ba189210d5b6d4bf57790d351c96c47c02190ecf1e430635ab"
        );
    }

    #[test]
    fn nothing_keeps_nothing() {
        let keep = Keep::nothing();
        let mut reader = Tee::new(&b"abc"[..], &keep);
        io::copy(&mut reader, &mut io::sink()).unwrap();
        drop(reader);
        assert_eq!(keep.finish().bytes, 0);
    }
}
