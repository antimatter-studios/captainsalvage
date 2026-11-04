//! Read-only access to a partition or disk image.
//!
//! `BlockSource` deliberately has no write method, and `FileSource` only ever
//! opens its path with `File::open`, which is `O_RDONLY`. Nothing in this crate
//! can modify the volume being examined.

use serde::Serialize;
use std::ffi::CString;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

/// Raw macOS devices reject requests that are not aligned to the sector size.
const DEVICE_ALIGNMENT: u64 = 512;

#[derive(Clone, Copy, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReadStats {
    pub bytes: u64,
    pub operations: u64,
    pub errors: u64,
}

pub trait BlockSource: Send + Sync {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()>;
    fn read_stats(&self) -> ReadStats;
}

#[derive(Default)]
struct Counters {
    bytes: AtomicU64,
    operations: AtomicU64,
    errors: AtomicU64,
}

impl Counters {
    fn record(&self, length: usize, result: &io::Result<()>) {
        self.operations.fetch_add(1, Ordering::Relaxed);
        match result {
            Ok(()) => {
                self.bytes.fetch_add(length as u64, Ordering::Relaxed);
            }
            Err(_) => {
                self.errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn snapshot(&self) -> ReadStats {
        ReadStats {
            bytes: self.bytes.load(Ordering::Relaxed),
            operations: self.operations.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

pub struct FileSource {
    file: File,
    path: PathBuf,
    counters: Counters,
}

impl FileSource {
    pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        Ok(Self {
            file: File::open(path.as_ref())?,
            path: path.as_ref().to_path_buf(),
            counters: Counters::default(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl BlockSource for FileSource {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let requested_end = offset + buffer.len() as u64;
        let start = offset - offset % DEVICE_ALIGNMENT;
        let end = requested_end.div_ceil(DEVICE_ALIGNMENT) * DEVICE_ALIGNMENT;
        let result = if start == offset && end == requested_end {
            self.file.read_exact_at(buffer, offset)
        } else {
            let mut aligned = vec![0u8; (end - start) as usize];
            self.file.read_exact_at(&mut aligned, start).map(|()| {
                let skip = (offset - start) as usize;
                buffer.copy_from_slice(&aligned[skip..skip + buffer.len()]);
            })
        };
        self.counters.record(buffer.len(), &result);
        result
    }

    fn read_stats(&self) -> ReadStats {
        self.counters.snapshot()
    }
}

/// In-memory source, used to test against synthetic volumes.
pub struct MemorySource {
    data: Vec<u8>,
    counters: Counters,
}

impl MemorySource {
    pub fn new(data: Vec<u8>) -> Self {
        Self { data, counters: Counters::default() }
    }
}

impl BlockSource for MemorySource {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        let start = offset as usize;
        let result = match self.data.get(start..start + buffer.len()) {
            Some(slice) => {
                buffer.copy_from_slice(slice);
                Ok(())
            }
            None => Err(io::Error::new(io::ErrorKind::UnexpectedEof, "read past end")),
        };
        self.counters.record(buffer.len(), &result);
        result
    }

    fn read_stats(&self) -> ReadStats {
        self.counters.snapshot()
    }
}

/// Asks the OS whether this process could write to `path`, without opening it.
pub fn os_denies_writes(path: &Path) -> bool {
    match CString::new(path.as_os_str().as_bytes()) {
        // SAFETY: `name` is a valid NUL-terminated string for the whole call.
        Ok(name) => unsafe { libc::access(name.as_ptr(), libc::W_OK) != 0 },
        Err(_) => true,
    }
}

/// Returns the `mount` output line for this device, if it is mounted.
pub fn mount_line(path: &Path) -> Option<String> {
    let text = path.to_str()?;
    let block_device = text.replace("/dev/rdisk", "/dev/disk");
    let needle = format!("{block_device} on ");
    let output = Command::new("/sbin/mount").output().ok()?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find(|line| line.starts_with(&needle))
        .map(str::to_string)
}
