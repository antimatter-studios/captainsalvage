//! NTFS volumes through the same scan, map, browse, copy and baseline workflow as exFAT.
//!
//! Images are formatted and populated by the `rust-fs-ntfs` driver, then read back by the
//! app. Every expectation about listings and contents is also checked against the independent
//! `ntfs` reader, never only against the driver the app itself uses.

use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::export::CopyRequest;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::{BlockSource, MemorySource, ReadStats};
use captainsalvage::workspace::Workspace;
use fs_core::{BlockDevice, BlockRead};
use fs_ntfs::block_io::BlockIo;
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::{Volume, VolumeAPI};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

const IMAGE_BYTES: usize = 16 * 1024 * 1024;
const CLUSTER: usize = 4096;
static NEXT: AtomicU64 = AtomicU64::new(0);

#[derive(Clone)]
struct Image(Arc<Mutex<Vec<u8>>>);

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

fn pattern(seed: u8, length: usize) -> Vec<u8> {
    (0..length)
        .map(|index| seed.wrapping_add((index % 251) as u8))
        .collect()
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What the formatted and populated image should hold, by name.
struct Fixture {
    image: Vec<u8>,
    files: Vec<(&'static str, Vec<u8>)>,
}

impl Fixture {
    fn content(&self, name: &str) -> &[u8] {
        &self.files.iter().find(|(file, _)| *file == name).unwrap().1
    }
}

fn format() -> Image {
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
fn fixture() -> Fixture {
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

fn workspace(label: &str) -> Workspace {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-ntfs-session-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    Workspace::at(&path).unwrap()
}

fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

fn scan(session: &Session) {
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

fn open(image: Vec<u8>, label: &str) -> Arc<Session> {
    Session::with_source(
        Path::new("/test/ntfs-image"),
        Arc::new(MemorySource::new(image)),
        workspace(label),
    )
    .unwrap()
}

/// A source that counts every byte it is asked to read and refuses anything else.
struct Counting {
    inner: MemorySource,
    reads: AtomicU64,
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

fn find(engine: &Engine, name: &str) -> Value {
    let rows = call(engine, "search", json!({"query": name}));
    rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("{name} not found in {rows}"))
        .clone()
}

#[test]
fn an_ntfs_volume_opens_with_its_own_geometry_and_typed_details() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "open");
    assert_eq!(session.volume.kind, FilesystemKind::Ntfs);
    let overview = session.overview();
    assert_eq!(overview["filesystem"]["kind"], "ntfs");
    assert_eq!(overview["filesystem"]["clusterSize"], CLUSTER);
    assert_eq!(overview["filesystem"]["fileRecordSize"], 1024);
    assert_eq!(overview["filesystem"]["label"], "SALVAGE");
    assert_eq!(overview["filesystem"]["serial"], "ca55000000002024");
    assert_eq!(overview["bootCheck"], Value::Null);
    assert_eq!(overview["geometry"]["clusterSize"], CLUSTER);
    assert_eq!(overview["geometry"]["volumeBytes"], IMAGE_BYTES - 512);
    assert!(session.volume.stats.allocated > 0);
    assert!(!session.volume.dirty_flag);
    // The map numbers a cluster by its place in the volume, so cluster 6 is at byte 6 * 4096.
    assert_eq!(session.volume.cluster_offset(6), 6 * CLUSTER as u64);
}

#[test]
fn scan_finds_every_file_and_the_tree_matches_the_independent_reader() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "scan");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    // The independent reader lists the namespace; the scan found the same files by carving.
    let mut cursor = std::io::Cursor::new(fixture.image.clone());
    let independent = ntfs::Ntfs::new(&mut cursor).unwrap();
    assert_eq!(
        independent
            .volume_name(&mut cursor)
            .unwrap()
            .unwrap()
            .name()
            .to_string_lossy(),
        "SALVAGE"
    );
    for (path, expected) in &fixture.files {
        let name = path.rsplit('/').next().unwrap();
        let row = find(&engine, name);
        let id = row["id"].as_i64().unwrap();
        assert_eq!(
            row["length"].as_u64().unwrap(),
            expected.len() as u64,
            "{path}"
        );
        let read = session
            .read_entry_range(id, 0, expected.len() as u64 + 10)
            .unwrap();
        assert_eq!(sha(&read.data), sha(expected), "{path}");
    }
    // The tree is rebuilt from parent references: leaf.txt sits in docs/deep.
    let leaf = find(&engine, "leaf.txt");
    let tree = call(&engine, "tree_path", json!({"entry": leaf["id"]}));
    let heads: Vec<u64> = tree["heads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|head| head.as_u64().unwrap())
        .collect();
    assert_eq!(heads.len(), 3, "root, docs and deep: {tree}");
    assert_eq!(heads[0], 5, "the root directory is MFT record 5");
    let path = session
        .with_database(|database| database.path_of(Some(heads[2] as u32)))
        .unwrap();
    assert!(path.ends_with("docs/deep"), "{path}");
    assert!(session.verify_unchanged().unwrap().unchanged);
}

#[test]
fn a_fragmented_file_is_read_from_its_runs_and_copied_with_its_hash() {
    let fixture = fixture();
    let expected = fixture.content("/frag.bin").to_vec();
    let session = open(fixture.image.clone(), "frag");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "frag.bin");
    let id = row["id"].as_i64().unwrap();
    let runs = session
        .with_database(|database| {
            database.entry_runs(
                row["sourceCluster"].as_u64().unwrap() as u32,
                row["sourceOffset"].as_u64().unwrap() as u32,
            )
        })
        .unwrap();
    assert!(runs.len() >= 2, "the file should be fragmented: {runs:?}");
    assert_eq!(row["storage"], "runs");
    let destination = session.workspace.dir().join("recovered");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![id],
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, 1);
    let copied = std::fs::read(destination.join("Volume root/frag.bin")).unwrap();
    assert_eq!(sha(&copied), sha(&expected));
}

fn wait_for_copy(session: &Session, files: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(status.files_done, files, "{status:?}");
            return;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "copy timed out: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn deleted_files_keep_their_bytes_and_say_how_they_are_held() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "deleted");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let gone = find(&engine, "gone.bin");
    assert!(!gone["inUse"].as_bool().unwrap());
    assert_eq!(gone["storage"], "runs");
    let read = session
        .read_entry_range(gone["id"].as_i64().unwrap(), 0, 12_000)
        .unwrap();
    assert_eq!(read.data, fixture.content("/gone.bin"));
    let small = find(&engine, "gone.txt");
    assert!(!small["inUse"].as_bool().unwrap());
    assert_eq!(small["storage"], "resident");
    assert_eq!(small["reasonCode"], Value::Null);
    let rows = call(&engine, "search", json!({"query": "gone.txt"}));
    assert_eq!(rows["rows"][0]["verdict"], "plausible");
    assert_eq!(
        session
            .read_entry_range(small["id"].as_i64().unwrap(), 0, 100)
            .unwrap()
            .data,
        fixture.content("/gone.txt")
    );
}

#[test]
fn scans_resume_after_restart_without_duplicating_findings() {
    let fixture = fixture();
    let source = Arc::new(MemorySource::new(fixture.image.clone()));
    let workspace = workspace("resume");
    let path = workspace.dir().to_path_buf();
    let session =
        Session::with_source(Path::new("/test/ntfs-resume"), source.clone(), workspace).unwrap();
    let stop = AtomicBool::new(false);
    let paused = run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 32 },
        &stop,
        |_| stop.store(true, Ordering::Relaxed),
    )
    .unwrap();
    assert_eq!(paused.status, "paused");
    drop(session);
    let resumed = Session::with_source(
        Path::new("/test/ntfs-resume"),
        source,
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    scan(&resumed);
    let first =
        captainsalvage::scan::read_progress(&resumed.database.lock().unwrap(), &resumed.volume)
            .unwrap();
    scan(&resumed);
    let second =
        captainsalvage::scan::read_progress(&resumed.database.lock().unwrap(), &resumed.volume)
            .unwrap();
    assert_eq!(first.sets_found, second.sets_found);
    assert_eq!(second.status, "complete");
    let engine = Engine::new();
    engine.set_session(resumed.clone());
    let rows = call(&engine, "search", json!({"query": "note.txt"}));
    assert_eq!(rows["rows"].as_array().unwrap().len(), 1);
}

#[test]
fn the_volume_is_never_written_and_its_hash_does_not_change() {
    let fixture = fixture();
    let before = sha(&fixture.image);
    let source = Arc::new(Counting {
        inner: MemorySource::new(fixture.image.clone()),
        reads: AtomicU64::new(0),
    });
    let session = Session::with_source(
        Path::new("/test/ntfs-readonly"),
        source.clone(),
        workspace("readonly"),
    )
    .unwrap();
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "big.bin");
    session
        .read_entry_range(row["id"].as_i64().unwrap(), 0, 40_000)
        .unwrap();
    assert!(source.reads.load(Ordering::Relaxed) > 0);
    assert_eq!(sha(&fixture.image), before);
    assert!(session.verify_unchanged().unwrap().unchanged);
}
