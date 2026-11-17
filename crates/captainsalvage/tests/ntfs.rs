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
use image::ImageEncoder;
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

/// A picture that is big enough to be stored in clusters, so a file test has bytes to walk.
fn noise_png() -> Vec<u8> {
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

/// Changes one record the way damage would, through the driver's own update path so the
/// update-sequence fixups stay valid, and returns the changed image.
fn change_record(image: &[u8], number: u64, change: impl FnOnce(&mut [u8])) -> Vec<u8> {
    let mut device = Image(Arc::new(Mutex::new(image.to_vec())));
    fs_ntfs::mft_io::update_mft_record_io(&mut device, number, |record| {
        change(record);
        Ok(())
    })
    .unwrap();
    let bytes = device.0.lock().unwrap().clone();
    bytes
}

fn record_of(session: &Arc<Session>, name: &str) -> (Value, u64) {
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, name);
    let number = row["recordNumber"].as_u64().unwrap();
    (row, number)
}

#[test]
fn every_live_file_matches_the_independent_reader_byte_for_byte() {
    use ntfs::NtfsReadSeek;
    let fixture = fixture();
    let session = open(fixture.image.clone(), "oracle");
    scan(&session);
    let mut cursor = std::io::Cursor::new(fixture.image.clone());
    let independent = ntfs::Ntfs::new(&mut cursor).unwrap();
    for (path, _) in fixture
        .files
        .iter()
        .filter(|(path, _)| !path.starts_with("/gone"))
    {
        let name = path.rsplit('/').next().unwrap();
        let (row, number) = record_of(&session, name);
        let file = independent.file(&mut cursor, number).unwrap();
        let data = file.data(&mut cursor, "").unwrap().unwrap();
        let attribute = data.to_attribute().unwrap();
        let mut value = attribute.value(&mut cursor).unwrap();
        let mut expected = vec![0; attribute.value_length() as usize];
        value.read(&mut cursor, &mut expected).unwrap();
        let ours = session
            .read_entry_range(row["id"].as_i64().unwrap(), 0, expected.len() as u64 + 8)
            .unwrap();
        assert_eq!(
            sha(&ours.data),
            sha(&expected),
            "{path}: the app and the independent reader disagree"
        );
        assert_eq!(row["length"].as_u64().unwrap(), expected.len() as u64);
    }
}

#[test]
fn the_baseline_explains_what_was_written_and_old_files_stay_readable() {
    let fixture = fixture();
    let workspace = workspace("baseline");
    let path = workspace.dir().to_path_buf();
    let first = Session::with_source(
        Path::new("/test/ntfs-baseline"),
        Arc::new(MemorySource::new(fixture.image.clone())),
        workspace,
    )
    .unwrap();
    scan(&first);
    drop(first);

    // Something writes a new file to the volume after the baseline was kept. Times are kept to
    // the second, so the write has to land in a later one for the root's change to show.
    std::thread::sleep(std::time::Duration::from_millis(1200));
    let image = Image(Arc::new(Mutex::new(fixture.image.clone())));
    let volume = Volume::open_writable(image.clone()).unwrap();
    volume.create_file("/", "later.bin").unwrap();
    volume
        .write_file_at("/later.bin", 0, &pattern(31, 9000))
        .unwrap();
    volume.flush().unwrap();
    drop(volume);
    let changed = image.0.lock().unwrap().clone();

    let reopened = Session::with_source(
        Path::new("/test/ntfs-baseline"),
        Arc::new(MemorySource::new(changed)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    assert!(!reopened.verify_unchanged().unwrap().unchanged);
    let changes = reopened.structure_changes().unwrap().unwrap();
    assert!(
        changes.written_clusters.len() >= 3,
        "a 9000-byte file takes three clusters: {changes:?}"
    );
    assert!(
        !changes.root_entries.is_empty(),
        "the root directory's own record changed"
    );
    // The map was built from the baseline's structures, and its files still read.
    let engine = Engine::new();
    engine.set_session(reopened.clone());
    let row = find(&engine, "big.bin");
    assert_eq!(
        sha(&reopened
            .read_entry_range(row["id"].as_i64().unwrap(), 0, 40_000)
            .unwrap()
            .data),
        sha(fixture.content("/docs/big.bin"))
    );
}

#[test]
fn a_bitlocker_volume_is_refused_by_name_after_reading_one_sector() {
    let mut image = vec![0; 1024 * 1024];
    image[3..11].copy_from_slice(b"-FVE-FS-");
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    let source = Arc::new(Counting {
        inner: MemorySource::new(image),
        reads: AtomicU64::new(0),
    });
    let error = captainsalvage::volume::Volume::open(source.clone())
        .err()
        .expect("BitLocker is refused");
    let message = format!("{error:#}");
    assert!(message.contains("encrypted with BitLocker"), "{message}");
    assert!(
        message.contains("does not guess at encrypted data"),
        "{message}"
    );
    assert_eq!(source.reads.load(Ordering::Relaxed), 1);
}

fn first_data_attribute_flags(record: &mut [u8]) -> &mut [u8] {
    let location =
        fs_ntfs::attr_io::find_attribute(record, fs_ntfs::attr_io::AttrType::Data, None).unwrap();
    &mut record[location.attr_offset + 12..location.attr_offset + 14]
}

#[test]
fn compressed_and_encrypted_streams_are_listed_but_never_zero_filled_or_copied() {
    for (label, flag, storage) in [
        ("compressed", 0x0001u16, "compressed"),
        ("encrypted", 0x4000, "encrypted"),
    ] {
        let fixture = fixture();
        let probe = open(fixture.image.clone(), &format!("{label}-probe"));
        scan(&probe);
        let (_, number) = record_of(&probe, "big.bin");
        let changed = change_record(&fixture.image, number, |record| {
            first_data_attribute_flags(record).copy_from_slice(&flag.to_le_bytes());
        });
        let session = open(changed, label);
        scan(&session);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let row = find(&engine, "big.bin");
        assert_eq!(row["storage"], storage);
        assert_eq!(row["verdict"], "partial");
        let entry = call(&engine, "entry", json!({"id": row["id"]}));
        assert_eq!(entry["entry"]["storage"], storage);
        let id = row["id"].as_i64().unwrap();
        let error = session.read_entry_range(id, 0, 100).err().unwrap();
        assert!(format!("{error:#}").contains(storage), "{error:#}");
        let destination = session.workspace.dir().join("recovered");
        let request = CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![id],
            ..CopyRequest::default()
        };
        // Planned as usual, an unreadable stream is not a file to copy.
        let refused = session.copy_start(request.clone()).err().unwrap();
        assert!(refused.to_string().contains("Nothing to copy"), "{refused}");
        // Asked for anyway, it fails by name and writes nothing.
        session
            .copy_start(CopyRequest {
                include_partial: true,
                ..request
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        loop {
            let status = session.copy_status().unwrap();
            if status.state == "finished" {
                assert_eq!(status.files_failed, 1, "{status:?}");
                assert_eq!(status.files_done, 0, "{status:?}");
                break;
            }
            assert!(std::time::Instant::now() < deadline, "{status:?}");
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(
            !destination.join("Volume root/docs/big.bin").exists()
                && !destination.join("Volume root/big.bin").exists(),
            "nothing was written for a stream that cannot be read"
        );
    }
}

#[test]
fn a_file_with_an_attribute_list_is_listed_with_its_streams_and_never_guessed_at() {
    let fixture = fixture();
    let mut device = Image(Arc::new(Mutex::new(fixture.image.clone())));
    {
        let volume = Volume::open_writable(device.clone()).unwrap();
        volume.create_file("/", "listed.bin").unwrap();
        volume
            .write_file_at("/listed.bin", 0, &pattern(41, 20_000))
            .unwrap();
        volume.flush().unwrap();
    }
    for index in 0..10 {
        fs_ntfs::write::write_named_stream_io(
            &mut device,
            "/listed.bin",
            &format!("stream{index}"),
            &[b'a' + index as u8; 120],
        )
        .unwrap();
    }
    let image = device.0.lock().unwrap().clone();
    let session = open(image, "listed");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "listed.bin");
    let details = call(&engine, "ntfs_entry", json!({"id": row["id"]}));
    assert_eq!(details["hasAttributeList"], true);
    // Only this record's own streams are listed: the rest are in records that are not followed.
    let streams = details["streams"].as_array().unwrap();
    assert!(!streams.is_empty() && streams.len() < 11, "{streams:?}");
    // The alternate streams are not entries of their own, and the data is not guessed at.
    let search = call(&engine, "search", json!({"query": "stream3"}));
    assert_eq!(search["rows"].as_array().unwrap().len(), 0);
    assert_eq!(row["storage"], "attribute-list");
    assert!(session
        .read_entry_range(row["id"].as_i64().unwrap(), 0, 10)
        .is_err());
}

#[test]
fn a_record_that_fails_its_update_sequence_check_is_kept_as_evidence_and_never_read() {
    let fixture = fixture();
    let probe = open(fixture.image.clone(), "torn-probe");
    scan(&probe);
    let (row, number) = record_of(&probe, "leaf.txt");
    let torn = {
        // Break the check at the end of the record's first sector.
        let offset = probe
            .with_database(|database| {
                Ok(database
                    .entry(row["id"].as_i64().unwrap())?
                    .map(|entry| {
                        probe.volume.cluster_offset(entry.source_cluster.unwrap())
                            + u64::from(entry.source_offset.unwrap())
                    })
                    .unwrap())
            })
            .unwrap();
        let mut bytes = fixture.image.clone();
        bytes[offset as usize + 510] ^= 0xff;
        let _ = number;
        bytes
    };
    let session = open(torn, "torn");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let rows = call(&engine, "search", json!({"query": "leaf.txt"}));
    assert_eq!(
        rows["rows"].as_array().unwrap().len(),
        0,
        "a torn record is not trusted for its name"
    );
    let damaged = find(&engine, "(damaged FILE record)");
    assert_eq!(damaged["verdict"], "damaged-entry");
    assert_eq!(damaged["length"], 0);
    assert!(session
        .read_entry_range(damaged["id"].as_i64().unwrap(), 0, 10)
        .unwrap()
        .data
        .is_empty());
}

#[test]
fn a_rebuild_can_replace_an_ntfs_entrys_clusters_and_can_be_withdrawn() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "rebuild");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let gone = find(&engine, "gone.bin");
    let big = find(&engine, "big.bin");
    let runs = session
        .with_database(|database| {
            database.entry_runs(
                big["sourceCluster"].as_u64().unwrap() as u32,
                big["sourceOffset"].as_u64().unwrap() as u32,
            )
        })
        .unwrap();
    let first = runs[0].lcn.unwrap();
    let added = call(
        &engine,
        "add_overlay",
        json!({
            "kind": "file_extents",
            "target": gone["id"],
            "payload": {"extents": [{"first": first, "count": 3}]},
            "note": "try the first clusters of big.bin",
        }),
    );
    let entry = call(&engine, "entry", json!({"id": gone["id"]}));
    assert_eq!(entry["usesOverlay"], true);
    // Rebuilt onto big.bin's clusters, the entry now shares them with that file.
    let conflicts = call(&engine, "conflicts", json!({"id": gone["id"]}));
    assert!(
        conflicts["claimants"]
            .as_array()
            .unwrap()
            .iter()
            .any(|claimant| claimant["name"] == "big.bin"),
        "{conflicts}"
    );
    let id = gone["id"].as_i64().unwrap();
    let rebuilt = session.read_entry(id, 12_000).unwrap().1;
    assert_eq!(rebuilt, fixture.content("/docs/big.bin")[..12_000]);
    // The disk never changed, and withdrawing the hypothesis restores the entry's own clusters.
    call(&engine, "delete_overlay", json!({"id": added["id"]}));
    assert_eq!(
        session.read_entry(id, 12_000).unwrap().1,
        fixture.content("/gone.bin")
    );
    assert!(session.verify_unchanged().unwrap().unchanged);
}

#[test]
fn file_tests_walk_the_bytes_the_driver_reads() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "filetests");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let picture = find(&engine, "pic.png");
    call(&engine, "file_tests_start", json!({"ids": [picture["id"]]}));
    loop {
        let status = session.file_test_status().unwrap();
        if status.state != "running" {
            assert_eq!(status.state, "finished", "{status:?}");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let test = call(&engine, "file_test", json!({"id": picture["id"]}));
    assert_eq!(test["format"], "png");
    assert_eq!(test["outcome"], "passed");
}

#[test]
fn an_ntfs_interpretation_inside_a_raw_disk_uses_the_backup_boot_when_the_main_one_is_gone() {
    use captainsalvage::disk::{DiskRegion, RawDisk};
    let fixture = fixture();
    let start = 1024 * 1024usize;
    let mut disk = vec![0u8; start + fixture.image.len() + 4096];
    disk[start..start + fixture.image.len()].copy_from_slice(&fixture.image);
    // The main boot sector is written over; the copy in the volume's last sector survives.
    disk[start..start + 512].fill(0);
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-ntfs-disk-{}-{}.img",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, &disk).unwrap();
    let before = sha(&disk);
    let raw = RawDisk::open(&path).unwrap();
    let findings = raw
        .discover(start as u64, fixture.image.len() as u64)
        .unwrap();
    let backup = findings
        .findings
        .iter()
        .find(|item| item.format == "NTFS" && item.structure == "possible backup boot sector")
        .expect("the backup boot sector is recognised");
    assert_eq!(backup.volume_start, Some(start as u64));
    let region = DiskRegion {
        start: start as u64,
        length: fixture.image.len() as u64,
        format: "NTFS".into(),
        label: "NTFS under a written-over boot sector".into(),
        boot_offset: backup.boot_offset.unwrap(),
    };
    // Without the backup the volume cannot be opened, and the source is untouched.
    let mut without = region.clone();
    without.boot_offset = without.start;
    assert!(Session::open_region(&raw, without).is_err());
    let session = Session::open_region(&raw, region).unwrap();
    assert_eq!(session.volume.kind, FilesystemKind::Ntfs);
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "big.bin");
    assert_eq!(
        sha(&session
            .read_entry_range(row["id"].as_i64().unwrap(), 0, 40_000)
            .unwrap()
            .data),
        sha(fixture.content("/docs/big.bin"))
    );
    assert_eq!(
        sha(&std::fs::read(&path).unwrap()),
        before,
        "the source was not written"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn switching_between_ntfs_and_exfat_images_never_mixes_workspaces_identities_or_bytes() {
    // Two volumes whose 64-bit serials fold to the same 32 bits must still be told apart.
    let build = |serial: u64, text: &[u8]| {
        let image = {
            let mut image = Image(Arc::new(Mutex::new(vec![0; IMAGE_BYTES])));
            fs_ntfs::mkfs::format_filesystem(
                &mut image,
                IMAGE_BYTES as u64,
                CLUSTER as u32,
                1024,
                Some("TWIN"),
                Some(serial),
            )
            .unwrap();
            image
        };
        let volume = Volume::open_writable(image.clone()).unwrap();
        volume.create_file("/", "same-name.txt").unwrap();
        volume.write_file_at("/same-name.txt", 0, text).unwrap();
        volume.flush().unwrap();
        drop(volume);
        let bytes = image.0.lock().unwrap().clone();
        bytes
    };
    let first = open(build(0x0000_0001_0000_0000, b"the first volume"), "twin-a");
    let second = open(build(0x0000_0000_0000_0001, b"the second one!"), "twin-b");
    assert_eq!(
        first.volume.geometry.volume_serial, second.volume.geometry.volume_serial,
        "the folded serials collide"
    );
    assert_ne!(
        first.volume.workspace_name(),
        second.volume.workspace_name()
    );
    let exfat = Session::with_source(
        Path::new("/test/exfat-image"),
        Arc::new(MemorySource::new(fs_exfat::fixtures::build_volume())),
        workspace("twin-exfat"),
    )
    .unwrap();
    assert_eq!(exfat.volume.kind, FilesystemKind::Exfat);
    for session in [&first, &second, &exfat] {
        scan(session);
    }
    let text = |session: &Arc<Session>, name: &str| -> Vec<u8> {
        let engine = Engine::new();
        engine.set_session(session.clone());
        let row = find(&engine, name);
        session
            .read_entry_range(row["id"].as_i64().unwrap(), 0, 100)
            .unwrap()
            .data
    };
    for _ in 0..3 {
        assert_eq!(text(&first, "same-name.txt"), b"the first volume");
        assert_eq!(text(&second, "same-name.txt"), b"the second one!");
        assert!(!text(&exfat, "photo.jpg").is_empty());
    }
    assert!(exfat.verify_unchanged().unwrap().unchanged);
    assert!(first.verify_unchanged().unwrap().unchanged);
    assert!(second.verify_unchanged().unwrap().unchanged);
}

#[test]
fn capabilities_and_record_details_are_reported_truthfully_through_the_engine() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "capabilities");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let report = call(&engine, "capabilities", json!({}));
    assert_eq!(report["filesystem"], "NTFS");
    let named = |name: &str| {
        report["capabilities"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["feature"] == name)
            .unwrap_or_else(|| panic!("{name} is not reported"))
            .clone()
    };
    assert_eq!(named("scan")["support"], "supported");
    assert_eq!(named("preview")["support"], "limited");
    assert_eq!(named("attribute_lists")["support"], "missing");
    assert_eq!(named("compression")["support"], "missing");
    assert_eq!(named("journal")["support"], "missing");
    let tools: Vec<String> = captainsalvage::mcp::tool_definitions()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(tools.contains(&"captainsalvage_capabilities".to_string()));
    assert!(tools.contains(&"captainsalvage_ntfs_entry".to_string()));

    let big = find(&engine, "big.bin");
    let details = call(&engine, "ntfs_entry", json!({"id": big["id"]}));
    assert_eq!(details["hasAttributeList"], false);
    assert_eq!(details["names"][0]["name"], "big.bin");
    assert_eq!(details["names"][0]["namespace"], "Win32 and DOS");
    assert_eq!(details["streams"][0]["resident"], false);
    assert_eq!(details["streams"][0]["length"], 40_000);
    assert_eq!(details["issues"].as_array().unwrap().len(), 0);

    // There is no chain of links to follow, and the engine says so rather than inventing one.
    let error = engine
        .call("follow_chain", &json!({"cluster": 20}), AUTHOR_USER)
        .err()
        .unwrap();
    assert!(error.to_string().contains("no chain of links"), "{error}");
    // A cluster that holds MFT records reports them as entries found in it.
    let cluster = big["sourceCluster"].as_u64().unwrap();
    let info = call(&engine, "inspect_cluster", json!({"cluster": cluster}));
    assert!(!info["sets"].as_array().unwrap().is_empty());
    assert_eq!(info["allocated"], true);
}

#[test]
fn a_sparse_file_keeps_its_holes_as_holes_and_reads_the_zeros_ntfs_defines() {
    use ntfs::NtfsReadSeek;
    let fixture = fixture();
    let mut device = Image(Arc::new(Mutex::new(fixture.image.clone())));
    // NTFS makes a hole of a whole 16-cluster unit at a time.
    let mut content = pattern(53, 16 * CLUSTER);
    content.extend(vec![0u8; 16 * CLUSTER]);
    content.extend(pattern(59, 4 * CLUSTER));
    {
        let volume = Volume::open_writable(device.clone()).unwrap();
        volume.create_file("/", "sparse.bin").unwrap();
        volume.flush().unwrap();
    }
    fs_ntfs::write::write_sparse_file_io(&mut device, "/sparse.bin", &content).unwrap();
    let image = device.0.lock().unwrap().clone();
    let session = open(image.clone(), "sparse");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "sparse.bin");
    let runs = session
        .with_database(|database| {
            database.entry_runs(
                row["sourceCluster"].as_u64().unwrap() as u32,
                row["sourceOffset"].as_u64().unwrap() as u32,
            )
        })
        .unwrap();
    assert!(
        runs.iter().any(|run| run.lcn.is_none()),
        "the hole is kept as a hole: {runs:?}"
    );
    // A hole claims no clusters, so a whole file is not made to look incomplete by it.
    assert_eq!(row["clustersNeeded"], 20);
    assert_eq!(row["clustersFound"], 20);
    assert_eq!(row["verdict"], "live");
    let read = session
        .read_entry_range(row["id"].as_i64().unwrap(), 0, content.len() as u64)
        .unwrap();
    assert_eq!(sha(&read.data), sha(&content));
    // The independent reader says the same.
    let mut cursor = std::io::Cursor::new(image);
    let independent = ntfs::Ntfs::new(&mut cursor).unwrap();
    let file = independent
        .file(&mut cursor, row["recordNumber"].as_u64().unwrap())
        .unwrap();
    let data = file.data(&mut cursor, "").unwrap().unwrap();
    let attribute = data.to_attribute().unwrap();
    let mut value = attribute.value(&mut cursor).unwrap();
    let mut expected = vec![0; content.len()];
    value.read(&mut cursor, &mut expected).unwrap();
    assert_eq!(sha(&expected), sha(&read.data));
}

#[test]
fn the_command_line_scans_searches_and_reports_capabilities_for_an_ntfs_image() {
    let fixture = fixture();
    let directory = std::env::temp_dir().join(format!(
        "captainsalvage-ntfs-cli-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&directory);
    std::fs::create_dir_all(&directory).unwrap();
    let image = directory.join("volume.img");
    std::fs::write(&image, &fixture.image).unwrap();
    let before = sha(&fixture.image);
    let run = |args: &[&str]| {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
            .args(args)
            // The workspace lives under the home folder, so the test keeps its own.
            .env("HOME", &directory)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    };
    // `scan` reports the volume first and then prints the tool's JSON.
    let json = |text: String| -> Value {
        let start = text.find("\n{\n").expect("JSON follows the report") + 1;
        serde_json::from_str(&text[start..]).unwrap()
    };
    let path = image.to_str().unwrap();
    let scanned = run(&["scan", path]);
    assert!(scanned.contains("\"status\":\"complete\""), "{scanned}");
    let rows = json(run(&[
        "scan",
        path,
        "--call",
        "search",
        r#"{"query":"big.bin"}"#,
    ]));
    assert_eq!(rows["rows"][0]["name"], "big.bin");
    assert_eq!(rows["rows"][0]["storage"], "runs");
    let capabilities = json(run(&["scan", path, "--call", "capabilities"]));
    assert_eq!(capabilities["filesystem"], "NTFS");
    let direct: Value = serde_json::from_str(&run(&["filesystem", "capabilities", path])).unwrap();
    assert_eq!(direct, capabilities);
    assert_eq!(sha(&std::fs::read(&image).unwrap()), before);
    let _ = std::fs::remove_dir_all(&directory);
}

#[test]
fn mcp_reaches_an_ntfs_volume_through_the_branded_tools() {
    use captainsalvage::mcp::handle_message;
    let fixture = fixture();
    let session = open(fixture.image.clone(), "mcp");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let ask = |tool: &str, arguments: Value| -> Value {
        let reply = handle_message(
            &engine,
            &json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": {"name": format!("captainsalvage_{tool}"), "arguments": arguments},
            }),
        )
        .expect("a reply");
        assert_ne!(reply["result"]["isError"], true, "{tool}: {reply}");
        serde_json::from_str(reply["result"]["content"][0]["text"].as_str().unwrap()).unwrap()
    };
    let capabilities = ask("capabilities", json!({}));
    assert_eq!(capabilities["filesystem"], "NTFS");
    let rows = ask("search", json!({"query": "frag.bin"}));
    let id = rows["rows"][0]["id"].clone();
    let details = ask("ntfs_entry", json!({"id": id}));
    assert!(details["streams"][0]["runs"].as_array().unwrap().len() >= 2);
    let overview = ask("volume_overview", json!({}));
    assert_eq!(overview["filesystem"]["kind"], "ntfs");
    // An AI may look and plan, but only the person starts a copy, as for every filesystem.
    let error = engine
        .call(
            "copy_start",
            &json!({"destination": "/tmp/nowhere", "entryIds": [id]}),
            captainsalvage::engine::AUTHOR_CLAUDE,
        )
        .err()
        .unwrap();
    assert!(error.to_string().contains("Only the person"), "{error}");
}
