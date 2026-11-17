//! What the NTFS tests share: images made by the rust-fs-ntfs driver, and the engine calls that
//! open, scan and read them.
#![allow(dead_code)]

use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::{BlockSource, MemorySource, ReadStats};
use captainsalvage::workspace::Workspace;
use fs_core::{BlockDevice, BlockRead};
use fs_ntfs::block_io::BlockIo;
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::{Volume, VolumeAPI};
use image::ImageEncoder;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

pub const IMAGE_BYTES: usize = 16 * 1024 * 1024;
pub const CLUSTER: usize = 4096;
pub static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
pub struct Image(pub Arc<Mutex<Vec<u8>>>);

impl BlockRead for Image {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        let data = self.0.lock().unwrap();
        let start = usize::try_from(offset).unwrap();
        let bytes = data
            .get(start..start + output.len())
            .ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: data.len() as u64,
            })?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
}
impl BlockDevice for Image {
    fn write_at(&self, offset: u64, input: &[u8]) -> fs_core::Result<()> {
        self.0.lock().unwrap()[offset as usize..offset as usize + input.len()]
            .copy_from_slice(input);
        Ok(())
    }
    fn is_writable(&self) -> bool {
        true
    }
}
impl BlockIo for Image {
    fn read_exact_at(&mut self, offset: u64, output: &mut [u8]) -> Result<(), String> {
        self.read_at(offset, output).map_err(|e| e.to_string())
    }
    fn write_all_at(&mut self, offset: u64, input: &[u8]) -> Result<(), String> {
        self.write_at(offset, input).map_err(|e| e.to_string())
    }
    fn size(&self) -> u64 {
        self.size_bytes()
    }
}

pub fn pattern(seed: u8, length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

/// A picture that is big enough to be stored in clusters, so a file test has bytes to walk.
pub fn noise_png() -> Vec<u8> {
    let mut state = 0x2545_F491u32;
    let pixels: Vec<u8> = (0..64 * 64 * 3)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            state as u8
        })
        .collect();
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(&pixels, 64, 64, image::ExtendedColorType::Rgb8)
        .unwrap();
    bytes
}

pub fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the formatted and populated image should hold, by name.
pub struct Fixture {
    pub image: Vec<u8>,
    pub files: Vec<(&'static str, Vec<u8>)>,
}

impl Fixture {
    pub fn content(&self, name: &str) -> &[u8] {
        &self.files.iter().find(|(file, _)| *file == name).unwrap().1
    }
}

pub fn format() -> Image {
    let mut image = Image(Arc::new(Mutex::new(vec![0; IMAGE_BYTES])));
    fs_ntfs::mkfs::format_filesystem(
        &mut image,
        IMAGE_BYTES as u64,
        CLUSTER as u32,
        1024,
        Some("SALVAGE"),
        Some(0xCA55_0000_0000_2024),
    )
    .unwrap();
    image
}

/// Directories, resident and non-resident files, a fragmented file, a deleted file whose
/// clusters are untouched, and a deleted resident file.
pub fn fixture() -> Fixture {
    let image = format();
    let volume = Volume::open_writable(image.clone()).unwrap();
    let mut files: Vec<(&'static str, Vec<u8>)> = Vec::new();
    volume.create_directory("/", "docs").unwrap();
    volume.create_directory("/docs", "deep").unwrap();
    let put = |files: &mut Vec<(&'static str, Vec<u8>)>, path: &'static str, bytes: Vec<u8>| {
        let (parent, name) = path.rsplit_once('/').unwrap();
        volume
            .create_file(if parent.is_empty() { "/" } else { parent }, name)
            .unwrap();
        volume.write_file_at(path, 0, &bytes).unwrap();
        files.push((path, bytes));
    };
    put(
        &mut files,
        "/docs/note.txt",
        b"A resident note, kept inside its FILE record.".to_vec(),
    );
    put(&mut files, "/docs/big.bin", pattern(7, 40_000));
    put(&mut files, "/docs/deep/leaf.txt", b"leaf".to_vec());
    put(&mut files, "/pic.png", noise_png());
    // A file grown after something else took the clusters beside it has two runs.
    let first = pattern(17, 8192);
    let rest = pattern(19, 11_808);
    put(&mut files, "/frag.bin", first.clone());
    put(&mut files, "/wall.bin", pattern(13, 4096));
    volume
        .write_file_at("/frag.bin", first.len() as u64, &rest)
        .unwrap();
    files
        .iter_mut()
        .find(|(name, _)| *name == "/frag.bin")
        .unwrap()
        .1 = [first, rest].concat();
    put(&mut files, "/gone.bin", pattern(23, 12_000));
    put(&mut files, "/gone.txt", b"a deleted resident file".to_vec());
    volume.remove_file("/gone.bin").unwrap();
    volume.remove_file("/gone.txt").unwrap();
    volume.flush().unwrap();
    drop(volume);
    let bytes = image.0.lock().unwrap().clone();
    Fixture {
        image: bytes,
        files,
    }
}

pub fn workspace(label: &str) -> Workspace {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-ntfs-session-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    Workspace::at(&path).unwrap()
}

pub fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

pub fn scan(session: &Session) {
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions {
            chunk_clusters: 256,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
}

pub fn open(image: Vec<u8>, label: &str) -> Arc<Session> {
    Session::with_source(
        Path::new("/test/ntfs-image"),
        Arc::new(MemorySource::new(image)),
        workspace(label),
    )
    .unwrap()
}

/// A source that counts every byte it is asked to read and refuses anything else.
pub struct Counting {
    pub inner: MemorySource,
    pub reads: AtomicU64,
}

impl BlockSource for Counting {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        self.inner.read_exact_at(offset, buffer)
    }
    fn read_stats(&self) -> ReadStats {
        self.inner.read_stats()
    }
}

pub fn find(engine: &Engine, name: &str) -> Value {
    let rows = call(engine, "search", json!({"query": name}));
    rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("{name} not found in {rows}"))
        .clone()
}
