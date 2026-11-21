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
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::{Volume, VolumeAPI};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

mod ntfs_support;
use ntfs_support::*;

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
        // Flagged as compressed with no compression unit: not a layout NTFS makes, so not read.
        ("compressed", 0x0001u16, "compression-unsupported"),
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
fn a_small_file_stored_in_its_record_is_read_whatever_its_compression_flag_says() {
    // Windows sets the compression flag on the data of a small file in a compressed folder, though
    // NTFS keeps that data in the FILE record exactly as written: only clusters are compressed. A
    // volume Windows made has such files (tests/ntfs_windows.rs).
    let fixture = fixture();
    let probe = open(fixture.image.clone(), "resident-compressed-probe");
    scan(&probe);
    let (_, number) = record_of(&probe, "note.txt");
    let changed = change_record(&fixture.image, number, |record| {
        first_data_attribute_flags(record).copy_from_slice(&0x0001u16.to_le_bytes());
    });
    let session = open(changed, "resident-compressed");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "note.txt");
    assert_eq!(row["storage"], "resident");
    assert_eq!(row["verdict"], "live");
    let details = call(&engine, "ntfs_entry", json!({"id": row["id"]}));
    assert_eq!(details["streams"][0]["resident"], true);
    let read = session
        .read_entry_range(row["id"].as_i64().unwrap(), 0, 100)
        .unwrap();
    assert_eq!(read.data, fixture.content("/docs/note.txt"));
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

/// The image with the update-sequence check of one of the first MFT records broken, the way a torn write leaves it.
fn torn_record(image: &[u8], number: usize) -> Vec<u8> {
    let mft = u64::from_le_bytes(image[0x30..0x38].try_into().unwrap()) as usize * CLUSTER;
    let mut bytes = image.to_vec();
    bytes[mft + number * 1024 + 510] ^= 0xff;
    bytes
}

fn damage_of(session: &Session) -> Vec<String> {
    session.overview()["structureDamage"]
        .as_array()
        .map(|list| {
            list.iter()
                .map(|text| text.as_str().unwrap().to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn a_volume_with_an_unreadable_bitmap_opens_with_its_allocation_unknown_and_says_so() {
    let fixture = fixture();
    let whole = open(fixture.image.clone(), "bitmap-whole");
    let session = open(torn_record(&fixture.image, 6), "bitmap-torn");
    assert_eq!(session.volume.kind, FilesystemKind::Ntfs);
    assert!(whole.volume.allocation_known && whole.overview().get("allocationKnown").is_none());
    assert!(
        damage_of(&whole).is_empty(),
        "an undamaged volume says nothing of damage"
    );

    // The window and the tools say it: not refused, and told why.
    assert!(!session.volume.allocation_known);
    let overview = session.overview();
    assert_eq!(overview["allocationKnown"], false);
    assert!(overview["allocationNote"]
        .as_str()
        .unwrap()
        .starts_with("Allocation unknown"));
    let damage = damage_of(&session);
    assert_eq!(damage.len(), 1, "{damage:?}");
    assert!(
        damage[0].starts_with("$Bitmap could not be read ("),
        "{}",
        damage[0]
    );
    assert!(
        damage[0].contains("no cluster is counted as free"),
        "{}",
        damage[0]
    );

    // Every cluster is neither in use nor free.
    let clusters = u64::from(session.volume.geometry.cluster_count);
    assert_eq!(overview["allocation"]["unknown"], clusters);
    assert_eq!(overview["allocation"]["allocated"], 0);
    assert_eq!(overview["allocation"]["freeZeroFat"], 0);
    assert_eq!(
        overview["allocationBytes"]["unknown"],
        clusters * CLUSTER as u64
    );
    for cluster in [
        2,
        7,
        100,
        session.volume.geometry.last_cluster_exclusive() - 1,
    ] {
        assert!(
            !session.volume.is_allocated(cluster) && !session.volume.is_free(cluster),
            "{cluster}"
        );
        assert_eq!(session.volume.allocation_class(cluster), 4, "{cluster}");
    }
    let stats = session.region_stats(2, session.volume.geometry.last_cluster_exclusive());
    assert_eq!(stats["allocation"]["unknown"], clusters);
    assert_eq!(stats["allocation"]["freeZeroFat"], 0);
    // The map draws it as its own state, which is neither of the others.
    let raster = session.map_raster(
        captainsalvage::session::Layer::Allocation,
        2,
        session.volume.geometry.last_cluster_exclusive(),
        16,
        4,
        0.0,
        None,
    );
    assert!(raster.iter().all(|&value| value == 4), "{raster:?}");
    let whole_raster = whole.map_raster(
        captainsalvage::session::Layer::Allocation,
        2,
        whole.volume.geometry.last_cluster_exclusive(),
        16,
        4,
        0.0,
        None,
    );
    assert!(
        whole_raster.iter().all(|&value| value != 4),
        "{whole_raster:?}"
    );

    // Files are found and read as in the undamaged volume, byte for byte, and a copy holds the same bytes.
    scan(&whole);
    scan(&session);
    assert_eq!(
        listed_files(&session).keys().collect::<Vec<_>>(),
        listed_files(&whole).keys().collect::<Vec<_>>()
    );
    for (path, expected) in &fixture.files {
        let name = path.rsplit('/').next().unwrap();
        let engine = Engine::new();
        engine.set_session(session.clone());
        let row = find(&engine, name);
        assert_eq!(
            row["length"].as_u64().unwrap(),
            expected.len() as u64,
            "{path}"
        );
        let read = session
            .read_entry_range(row["id"].as_i64().unwrap(), 0, expected.len() as u64 + 10)
            .unwrap();
        assert_eq!(sha(&read.data), sha(expected), "{path}");
        let undamaged = Engine::new();
        undamaged.set_session(whole.clone());
        let same = find(&undamaged, name);
        assert_eq!(row["inUse"], same["inUse"], "{path}");
        assert_eq!(
            row["verdict"], same["verdict"],
            "{path}: live stays live, and a deleted file's verdict is the one it had"
        );
    }
    let engine = Engine::new();
    engine.set_session(session.clone());
    let gone = find(&engine, "gone.bin");
    assert!(
        gone["reason"]
            .as_str()
            .unwrap_or_default()
            .contains("bitmap could not be read"),
        "a deleted file's verdict says its clusters' use is not known: {gone}"
    );
    let row = find(&engine, "frag.bin");
    let destination = session.workspace.dir().join("recovered");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![row["id"].as_i64().unwrap()],
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, 1);
    let copied = std::fs::read(destination.join("Volume root/frag.bin")).unwrap();
    assert_eq!(sha(&copied), sha(fixture.content("/frag.bin")));
}

#[test]
fn unknown_clusters_are_never_counted_as_free_space() {
    let fixture = fixture();
    let whole = open(fixture.image.clone(), "unknown-whole");
    let session = open(torn_record(&fixture.image, 6), "unknown-torn");
    scan(&whole);
    scan(&session);
    let known = whole.resolve().unwrap();
    assert!(
        known.unexplained_free_clusters > 0,
        "an undamaged volume has free space nothing explains"
    );
    let summary = session.resolve().unwrap();
    assert_eq!(summary.unexplained_free_clusters, 0, "unknown is not free");
    assert_eq!(summary.labelled_free_clusters, 0);
    assert_eq!(summary.bad_sector_free_clusters, 0);
    assert!(
        summary.unexplained_unknown_clusters > 0,
        "but it is not hidden either"
    );
    assert!(
        session.unexplained_runs(0, 10, 1).is_empty(),
        "no run of free space is offered when nothing is known to be free"
    );
    assert_eq!(known.unexplained_unknown_clusters, 0);
}

#[test]
fn a_baseline_of_a_volume_with_an_unreadable_bitmap_says_what_was_not_read() {
    let fixture = fixture();
    let session = open(torn_record(&fixture.image, 6), "baseline-unread");
    let copy = session
        .workspace
        .baseline_copy()
        .expect("the baseline copy was saved");
    assert!(copy.is_unread("bitmap"));
    assert!(copy.bitmap.is_empty());
    let manifest = serde_json::to_string(&copy.manifest()).unwrap();
    assert!(
        manifest.contains("\"unread\":[{\"region\":\"bitmap\""),
        "{manifest}"
    );
    let whole = open(fixture.image.clone(), "baseline-read");
    let whole_copy = whole.workspace.baseline_copy().unwrap();
    assert!(!whole_copy.is_unread("bitmap"));
    assert!(
        !serde_json::to_string(&whole_copy.manifest())
            .unwrap()
            .contains("unread"),
        "a copy of a volume read whole is as it always was"
    );
    // Read again, it is the same volume, and nothing is reported as written.
    assert!(session.verify_unchanged().unwrap().unchanged);
}

#[test]
fn a_torn_mft_record_zero_is_reported_as_damage() {
    let fixture = fixture();
    let session = open(torn_record(&fixture.image, 0), "mft-torn");
    let damage = damage_of(&session);
    assert_eq!(damage.len(), 1, "{damage:?}");
    assert!(
        damage[0].starts_with("$MFT record 0 failed its update-sequence check"),
        "{}",
        damage[0]
    );
    assert!(damage[0].contains("later parts of $MFT"), "{}", damage[0]);
    // The volume still opens and its allocation is known: only the first record is torn.
    assert!(session.volume.allocation_known);
    // Records 3 and 5 are told apart.
    let volume = open(torn_record(&fixture.image, 3), "volume-torn");
    assert!(
        damage_of(&volume)[0].starts_with("$Volume (MFT record 3)"),
        "{:?}",
        damage_of(&volume)
    );
    let root = open(torn_record(&fixture.image, 5), "root-torn");
    assert!(
        damage_of(&root)[0].starts_with("The root directory's record"),
        "{:?}",
        damage_of(&root)
    );
    // Neither is reported of an undamaged volume.
    assert!(damage_of(&open(fixture.image.clone(), "mft-whole")).is_empty());
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
    assert_eq!(named("compression")["support"], "supported");
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
            "export_start",
            &json!({"destination": "/tmp/nowhere", "kind": "file", "target": id}),
            captainsalvage::engine::AUTHOR_CLAUDE,
        )
        .err()
        .unwrap();
    assert!(error.to_string().contains("Only the person"), "{error}");
}

#[test]
fn a_live_resident_file_is_live_and_a_deleted_one_is_not() {
    let fixture = fixture();
    let session = open(fixture.image.clone(), "resident-live");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let live = find(&engine, "note.txt");
    assert_eq!(live["storage"], "resident");
    assert_eq!(live["verdict"], "live", "{live}");
    assert_eq!(
        live["clustersNeeded"], 0,
        "a resident file needs no cluster"
    );
    assert_eq!(find(&engine, "gone.txt")["verdict"], "plausible");
}

/// A volume with live files that hold no bytes, as Windows leaves them: one in the root, one in a
/// folder, one that once held data and was cut to nothing, and a deleted one.
fn volume_with_empty_files() -> Vec<u8> {
    let image = format();
    let volume = Volume::open_writable(image.clone()).unwrap();
    volume.create_directory("/", "docs").unwrap();
    volume.create_directory("/docs", "deep").unwrap();
    for (parent, name) in [
        ("/", "empty.txt"),
        ("/", "gone-empty.txt"),
        ("/docs", "note.txt"),
        ("/docs", ".keep"),
        ("/docs/deep", "zero.bin"),
    ] {
        volume.create_file(parent, name).unwrap();
    }
    volume
        .write_file_at("/docs/note.txt", 0, b"a note with bytes")
        .unwrap();
    volume
        .write_file_at("/docs/deep/zero.bin", 0, &pattern(5, 20_000))
        .unwrap();
    volume.set_len("/docs/deep/zero.bin", 0).unwrap();
    volume.remove_file("/gone-empty.txt").unwrap();
    volume.flush().unwrap();
    drop(volume);
    let bytes = image.0.lock().unwrap().clone();
    bytes
}

#[test]
fn live_files_with_no_bytes_are_copied_empty_and_a_deleted_one_is_not() {
    let session = open(volume_with_empty_files(), "empty-files");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let docs = find(&engine, "note.txt");
    let heads = call(&engine, "tree_path", json!({"entry": docs["id"]}));
    let docs_head = heads["heads"][1].as_u64().unwrap() as u32;
    for name in ["empty.txt", ".keep", "zero.bin"] {
        let row = find(&engine, name);
        assert_eq!(row["verdict"], "empty", "{name}");
        assert_eq!(row["length"], 0, "{name}");
        assert!(row["inUse"].as_bool().unwrap(), "{name}");
    }
    let gone = find(&engine, "gone-empty.txt");
    assert!(!gone["inUse"].as_bool().unwrap());

    // The folder holds three files, and the plan counts the two with no bytes.
    let destination = session.workspace.dir().join("recovered");
    let request = CopyRequest {
        destination: destination.to_string_lossy().into_owned(),
        folders: vec![docs_head],
        ..CopyRequest::default()
    };
    let plan = session.copy_plan(&request).unwrap();
    assert_eq!(plan.files, 3, "{plan:?}");
    assert_eq!(plan.bytes, b"a note with bytes".len() as u64);
    assert_eq!(plan.skipped_partial, 0);
    session.copy_start(request).unwrap();
    wait_for_copy(&session, 3);
    let status = session.copy_status().unwrap();
    assert_eq!(status.files_total, 3);
    assert_eq!((status.files_failed, status.files_partial), (0, 0));
    for path in ["docs/.keep", "docs/deep/zero.bin"] {
        let copied = destination.join("Volume root").join(path);
        assert!(copied.is_file(), "{path} was not copied");
        assert_eq!(std::fs::metadata(&copied).unwrap().len(), 0, "{path}");
    }
    assert_eq!(
        std::fs::read(destination.join("Volume root/docs/note.txt")).unwrap(),
        b"a note with bytes"
    );
    assert!(!destination.join("Volume root/gone-empty.txt").exists());
    // Each is verified: its manifest line is complete and hashes zero bytes.
    let manifest = std::fs::read_to_string(&status.manifest).unwrap();
    let empty_hash = sha(&[]);
    let empties = manifest
        .lines()
        .filter(|line| line.contains("\"length\":0,"))
        .collect::<Vec<_>>();
    assert_eq!(empties.len(), 2, "{manifest}");
    for line in empties {
        assert!(line.contains(&empty_hash) && line.contains("\"status\":\"complete\""));
    }
}

#[test]
fn naming_an_empty_file_or_a_deleted_one_copies_only_the_live_one_and_never_overwrites() {
    let session = open(volume_with_empty_files(), "empty-named");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let live = find(&engine, "empty.txt")["id"].as_i64().unwrap();
    let gone = find(&engine, "gone-empty.txt")["id"].as_i64().unwrap();
    let destination = session.workspace.dir().join("recovered");
    std::fs::create_dir_all(destination.join("Volume root")).unwrap();
    std::fs::write(destination.join("Volume root/empty.txt"), b"already here").unwrap();
    let request = CopyRequest {
        destination: destination.to_string_lossy().into_owned(),
        entry_ids: vec![live, gone],
        ..CopyRequest::default()
    };
    assert_eq!(session.copy_plan(&request).unwrap().files, 1);
    session.copy_start(request).unwrap();
    wait_for_copy(&session, 1);
    assert_eq!(
        std::fs::read(destination.join("Volume root/empty.txt")).unwrap(),
        b"already here"
    );
    let beside = destination.join(format!("Volume root/empty (entry {live}).txt"));
    assert_eq!(std::fs::metadata(beside).unwrap().len(), 0);
    assert!(!destination.join("Volume root/gone-empty.txt").exists());
}

#[test]
fn an_export_of_an_empty_file_or_a_folder_with_one_makes_the_empty_file() {
    let session = open(volume_with_empty_files(), "empty-export");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let live = find(&engine, "empty.txt")["id"].as_i64().unwrap();
    let docs = find(&engine, "note.txt");
    let heads = call(&engine, "tree_path", json!({"entry": docs["id"]}));
    let docs_head = heads["heads"][1].as_u64().unwrap() as u32;
    let destination = session.workspace.dir().join("exported");
    std::fs::create_dir_all(&destination).unwrap();
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![live],
            folders: vec![docs_head],
            direct: true,
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, 4);
    assert_eq!(
        std::fs::metadata(destination.join("empty.txt"))
            .unwrap()
            .len(),
        0
    );
    assert_eq!(
        std::fs::metadata(destination.join("docs/.keep"))
            .unwrap()
            .len(),
        0
    );
}

/// Writes a synthetic NTFS volume to `SALVAGE_NTFS_IMAGE`, for driving the window against it
/// (`tests/window/ntfs_flow_test.py`): folders, a picture, a fragmented file and a deleted one.
/// Run with: SALVAGE_NTFS_IMAGE=/path/ntfs.img cargo test -p captainsalvage --test ntfs write_the_ntfs
#[test]
fn write_the_ntfs_volume_for_window_tests() {
    let fixture = fixture();
    // What is written is a volume this app opens and reads back whole.
    let session = open(fixture.image.clone(), "window-fixture");
    assert_eq!(session.volume.kind, FilesystemKind::Ntfs);
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "frag.bin");
    let read = session
        .read_entry_range(row["id"].as_i64().unwrap(), 0, 30_000)
        .unwrap();
    assert_eq!(read.data, fixture.content("/frag.bin"));
    if let Some(destination) = std::env::var_os("SALVAGE_NTFS_IMAGE") {
        std::fs::write(destination, &fixture.image).expect("exported window fixture");
    }
}

/// One file with three names: `docs/shared.bin` and `docs/second.bin` in one folder, and
/// `other/third.bin` in another; and a file with one name, beside them.
fn hard_link_volume() -> (Vec<u8>, Vec<u8>) {
    let image = format();
    let content = pattern(31, 30_000);
    let volume = Volume::open_writable(image.clone()).unwrap();
    volume.create_directory("/", "docs").unwrap();
    volume.create_directory("/", "other").unwrap();
    volume.create_file("/docs", "shared.bin").unwrap();
    volume
        .write_file_at("/docs/shared.bin", 0, &content)
        .unwrap();
    volume.create_file("/docs", "single.txt").unwrap();
    volume
        .write_file_at("/docs/single.txt", 0, b"one name")
        .unwrap();
    volume.flush().unwrap();
    drop(volume);
    let mut device = image.clone();
    fs_ntfs::write::link_io(&mut device, "/docs/shared.bin", "/docs", "second.bin").unwrap();
    fs_ntfs::write::link_io(&mut device, "/docs/shared.bin", "/other", "third.bin").unwrap();
    let bytes = image.0.lock().unwrap().clone();
    (bytes, content)
}

/// Every live file the app lists, by path, with its length.
fn listed_files(session: &Arc<Session>) -> BTreeMap<String, (i64, u64)> {
    let rows = session
        .query_map("SELECT id FROM sets WHERE is_dir = 0 AND in_use = 1 AND checksum_ok = 1")
        .unwrap();
    let mut files = BTreeMap::new();
    for row in rows["rows"].as_array().unwrap() {
        let id = row["id"].as_i64().unwrap();
        let entry = session
            .with_database(|database| Ok(database.entry(id)?.unwrap()))
            .unwrap();
        let folder = entry.path.clone().unwrap_or_default();
        let path = format!("{}/{}", folder.trim_start_matches('/'), entry.name);
        let path = path.trim_start_matches('/').to_string();
        if !path.starts_with('$') && !path.contains("/$") {
            files.insert(path, (id, entry.length));
        }
    }
    files
}

#[test]
fn a_file_with_several_hard_links_is_listed_under_each_name_in_its_own_folder() {
    let (image, content) = hard_link_volume();
    let session = open(image, "hard-links");
    scan(&session);
    let files = listed_files(&session);
    let names: Vec<&str> = files
        .keys()
        .map(String::as_str)
        .filter(|path| path.starts_with("docs/") || path.starts_with("other/"))
        .collect();
    assert_eq!(
        names,
        [
            "docs/second.bin",
            "docs/shared.bin",
            "docs/single.txt",
            "other/third.bin"
        ]
    );
    // Each name has the file's size and bytes, and its own entry.
    let mut ids = BTreeSet::new();
    for path in ["docs/shared.bin", "docs/second.bin", "other/third.bin"] {
        let (id, length) = files[path];
        assert_eq!(length, content.len() as u64, "{path}");
        let read = session.read_entry_range(id, 0, length + 8).unwrap();
        assert_eq!(read.data, content, "{path}");
        assert!(ids.insert(id), "{path} has its own entry");
    }
    assert_eq!(files["docs/single.txt"].1, 8);
    // A rescan finds the same entries and adds none.
    let before: Vec<i64> = files.values().map(|(id, _)| *id).collect();
    scan(&session);
    let again = listed_files(&session);
    assert_eq!(
        again.keys().collect::<Vec<_>>(),
        files.keys().collect::<Vec<_>>()
    );
    assert_eq!(
        again.values().map(|(id, _)| *id).collect::<Vec<_>>(),
        before
    );
}

#[test]
fn a_copy_holds_a_file_with_several_hard_links_under_every_name() {
    let (image, content) = hard_link_volume();
    let session = open(image, "hard-link-copy");
    scan(&session);
    let files = listed_files(&session);
    let ids: Vec<i64> = [
        "docs/shared.bin",
        "docs/second.bin",
        "other/third.bin",
        "docs/single.txt",
    ]
    .iter()
    .map(|path| files[*path].0)
    .collect();
    let destination = session.workspace.dir().join("copied");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: ids,
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, 4);
    let root = destination.join("Volume root");
    for path in ["docs/shared.bin", "docs/second.bin", "other/third.bin"] {
        assert_eq!(std::fs::read(root.join(path)).unwrap(), content, "{path}");
    }
    assert_eq!(
        std::fs::read(root.join("docs/single.txt")).unwrap(),
        b"one name"
    );
}

/// A source that reads like a memory image until it is told that a stretch of it has gone bad.
struct Flaky {
    source: MemorySource,
    bad: Mutex<Option<std::ops::Range<u64>>>,
}

impl captainsalvage::source::BlockSource for Flaky {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        if let Some(bad) = self.bad.lock().unwrap().clone() {
            if offset < bad.end && offset + buffer.len() as u64 > bad.start {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
        }
        self.source.read_exact_at(offset, buffer)
    }

    fn read_stats(&self) -> captainsalvage::source::ReadStats {
        self.source.read_stats()
    }
}

/// `frag.bin` is 20000 bytes in two runs of 4096-byte clusters. Its fourth cluster (bytes 12288 to
/// 16384) is made unreadable after the scan. Returns the session, the file's bytes and its entry.
fn with_a_bad_cluster(label: &str) -> (Arc<Session>, Vec<u8>, i64) {
    let fixture = fixture();
    let wanted = fixture.content("/frag.bin").to_vec();
    let source = Arc::new(Flaky {
        source: MemorySource::new(fixture.image),
        bad: Mutex::new(None),
    });
    let session = Session::with_source(
        Path::new("/test/ntfs-bad-cluster"),
        source.clone(),
        workspace(label),
    )
    .unwrap();
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "frag.bin");
    let runs = session
        .with_database(|database| {
            database.entry_runs(
                row["sourceCluster"].as_u64().unwrap() as u32,
                row["sourceOffset"].as_u64().unwrap() as u32,
            )
        })
        .unwrap();
    // The cluster that holds byte 12288 of the file.
    let run = runs
        .iter()
        .find(|run| run.vcn <= 3 && 3 < run.vcn + run.count)
        .expect("a run holds the fourth cluster");
    let lcn = run.lcn.expect("it is not a hole") + (3 - run.vcn);
    *source.bad.lock().unwrap() = Some(lcn * 4096..(lcn + 1) * 4096);
    (session, wanted, row["id"].as_i64().unwrap())
}

#[test]
fn a_preview_of_an_ntfs_file_marks_the_stretch_that_could_not_be_read_and_keeps_the_rest() {
    use captainsalvage::browse::ByteRange;
    let (session, wanted, id) = with_a_bad_cluster("ntfs-bad-preview");
    let whole = session.read_entry_range(id, 0, 20_000).unwrap();
    assert_eq!(whole.data.len(), 20_000);
    assert_eq!(whole.data[..12_288], wanted[..12_288]);
    assert_eq!(
        whole.data[12_288..16_384],
        [0u8; 4096],
        "zeros stand in for it"
    );
    assert_eq!(whole.data[16_384..], wanted[16_384..]);
    assert_eq!(
        whole.unreadable,
        [ByteRange {
            start: 12_288,
            end: 16_384
        }]
    );
    assert!(session
        .read_entry_range(id, 0, 8000)
        .unwrap()
        .unreadable
        .is_empty());
    let across = session.read_entry_range(id, 12_000, 1000).unwrap();
    assert_eq!(
        across.unreadable,
        [ByteRange {
            start: 12_288,
            end: 13_000
        }]
    );
}

#[test]
fn a_copy_of_an_ntfs_file_with_an_unreadable_stretch_writes_zeros_there_and_names_it() {
    let (session, wanted, id) = with_a_bad_cluster("ntfs-bad-copy");
    let destination = session.workspace.dir().join("bad-copy");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![id],
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(
                (status.files_done, status.files_partial),
                (1, 1),
                "{status:?}"
            );
            break;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(std::time::Instant::now() < deadline, "copy timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let copied = std::fs::read(destination.join("Volume root/frag.bin")).unwrap();
    assert_eq!(copied.len(), 20_000);
    assert_eq!(copied[..12_288], wanted[..12_288]);
    assert_eq!(copied[12_288..16_384], [0u8; 4096]);
    assert_eq!(copied[16_384..], wanted[16_384..]);
    let manifest =
        std::fs::read_to_string(destination.join("captain-salvage-manifest.jsonl")).unwrap();
    let row: Value = serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
    assert_eq!(
        row["status"],
        "partial: unreadable parts of the stream filled with zeros"
    );
    assert_eq!(row["unreadableClusters"], 1);
    assert_eq!(
        row["unreadableRanges"],
        json!([{"start": 12_288, "end": 16_384}])
    );
    assert_eq!(row["bytesWritten"], 20_000);
}

// ---------------------------------------------------------------------------------------------
// Files NTFS compressed (issue #174).
//
// The driver's formatter cannot compress, so a volume is made with a plain file and then its data
// attribute is rewritten the way NTFS lays a compressed one out: 16-cluster units, each stored as
// its LZNT1 stream in the first clusters with a hole after them, or as it is when compressing did
// not shrink it, or as a hole when it is all zeros. The independent `ntfs` reader grades the result
// first, so the layout is what NTFS does and not only what this app reads.

const UNIT_CLUSTERS: usize = 16;

/// An LZNT1 compressed chunk of at most 4096 bytes, or the chunk stored as it is when that is not
/// smaller. Matches are found by looking back through the chunk for the longest one.
fn lznt1_chunk(data: &[u8]) -> Vec<u8> {
    let mut packed = Vec::new();
    let mut at = 0;
    while at < data.len() {
        let flags_at = packed.len();
        packed.push(0u8);
        let mut flags = 0u8;
        for bit in 0..8 {
            if at >= data.len() {
                break;
            }
            // The split of a token between offset and length depends on how much of the chunk is
            // already behind it: the offset field is as wide as it takes to address any of those
            // bytes, never fewer than four bits.
            let offset_bits =
                (usize::BITS - at.max(1).saturating_sub(1).leading_zeros()).max(4) as usize;
            let length_bits = 16 - offset_bits;
            let longest = (1usize << length_bits) + 2;
            let farthest = 1usize << offset_bits;
            let mut best = (0usize, 0usize);
            for back in 1..=at.min(farthest) {
                let mut length = 0;
                while at + length < data.len()
                    && length < longest
                    && data[at + length] == data[at + length - back]
                {
                    length += 1;
                }
                if length > best.1 {
                    best = (back, length);
                }
            }
            if best.1 >= 3 {
                let token = (((best.0 - 1) << length_bits) | (best.1 - 3)) as u16;
                packed.extend_from_slice(&token.to_le_bytes());
                flags |= 1 << bit;
                at += best.1;
            } else {
                packed.push(data[at]);
                at += 1;
            }
        }
        packed[flags_at] = flags;
    }
    let mut chunk = Vec::new();
    if packed.len() < data.len() {
        chunk.extend_from_slice(&(0xB000u16 | (packed.len() - 1) as u16).to_le_bytes());
        chunk.extend_from_slice(&packed);
    } else {
        chunk.extend_from_slice(&(0x3000u16 | (data.len() - 1) as u16).to_le_bytes());
        chunk.extend_from_slice(data);
    }
    chunk
}

/// The LZNT1 stream of one compression unit, or none when it does not fit in fewer clusters than the
/// unit has.
fn lznt1_unit(unit: &[u8]) -> Option<Vec<u8>> {
    let mut stream: Vec<u8> = unit.chunks(4096).flat_map(lznt1_chunk).collect();
    stream.extend_from_slice(&[0, 0]);
    (stream.len().div_ceil(CLUSTER) < UNIT_CLUSTERS).then_some(stream)
}

/// The bytes of a compressed file: a unit that compresses well, one of noise that does not, a hole of
/// zeros, and a short last unit that compresses.
fn compressible(length: usize, seed: u8) -> Vec<u8> {
    (0..length)
        .map(|index| b"salvage the compressed bytes; "[(index + seed as usize) % 30])
        .collect()
}

fn compressed_content() -> Vec<u8> {
    let unit = UNIT_CLUSTERS * CLUSTER;
    let mut content = compressible(unit, 0);
    // Noise: nothing in it repeats, so no compression makes it smaller.
    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    content.extend((0..unit).map(|_| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state >> 24) as u8
    }));
    content.extend(std::iter::repeat_n(0u8, unit));
    content.extend(compressible(10_000, 7));
    content
}

/// A volume whose file `docs/packed.txt` holds `compressed_content()` in compressed units. Returns the
/// image, the content and, for each unit, the first cluster of its stored clusters and how many.
fn compressed_volume() -> (Vec<u8>, Vec<u8>, Vec<(u64, usize)>) {
    let content = compressed_content();
    let image = format();
    let volume = Volume::open_writable(image.clone()).unwrap();
    volume.create_directory("/", "docs").unwrap();
    volume.create_file("/docs", "packed.txt").unwrap();
    volume
        .write_file_at("/docs/packed.txt", 0, &content)
        .unwrap();
    volume.flush().unwrap();
    drop(volume);
    let mut bytes = image.0.lock().unwrap().clone();
    let probe = open(bytes.clone(), "compressed-probe");
    scan(&probe);
    let (_, number) = record_of(&probe, "packed.txt");
    let mut placed: Vec<(u64, usize)> = Vec::new();
    let mut writes: Vec<(usize, Vec<u8>)> = Vec::new();
    let unit_bytes = UNIT_CLUSTERS * CLUSTER;
    let units = content.len().div_ceil(unit_bytes);
    let mut device = Image(Arc::new(Mutex::new(bytes.clone())));
    fs_ntfs::mft_io::update_mft_record_io(&mut device, number, |record| {
        let found =
            fs_ntfs::attr_io::find_attribute(record, fs_ntfs::attr_io::AttrType::Data, None)
                .unwrap();
        let start = found.attr_offset;
        let old_length =
            u32::from_le_bytes(record[start + 4..start + 8].try_into().unwrap()) as usize;
        let end = start + old_length;
        assert_eq!(
            u32::from_le_bytes(record[end..end + 4].try_into().unwrap()),
            0xFFFF_FFFF,
            "the data is the last attribute"
        );
        let pairs =
            u16::from_le_bytes(record[start + 0x20..start + 0x22].try_into().unwrap()) as usize;
        let old_runs = fs_ntfs::data_runs::decode_runs(&record[start + pairs..end]).unwrap();
        let mut next_lcn = old_runs[0].lcn.unwrap();
        let available: u64 = old_runs.iter().map(|run| run.length).sum();
        let mut runs: Vec<fs_ntfs::data_runs::DataRun> = Vec::new();
        let mut stored_total = 0u64;
        for unit in 0..units {
            let mut piece =
                content[unit * unit_bytes..content.len().min((unit + 1) * unit_bytes)].to_vec();
            piece.resize(unit_bytes, 0);
            let vcn = (unit * UNIT_CLUSTERS) as u64;
            let (data, stored) = if piece.iter().all(|byte| *byte == 0) {
                (Vec::new(), 0)
            } else if let Some(stream) = lznt1_unit(&piece) {
                let stored = stream.len().div_ceil(CLUSTER);
                (stream, stored)
            } else {
                (piece, UNIT_CLUSTERS)
            };
            if stored > 0 {
                runs.push(fs_ntfs::data_runs::DataRun {
                    starting_vcn: vcn,
                    length: stored as u64,
                    lcn: Some(next_lcn),
                });
                let mut padded = data;
                padded.resize(stored * CLUSTER, 0);
                writes.push((next_lcn as usize * CLUSTER, padded));
                placed.push((next_lcn, stored));
                next_lcn += stored as u64;
                stored_total += stored as u64;
            } else {
                placed.push((0, 0));
            }
            if stored < UNIT_CLUSTERS {
                runs.push(fs_ntfs::data_runs::DataRun {
                    starting_vcn: vcn + stored as u64,
                    length: (UNIT_CLUSTERS - stored) as u64,
                    lcn: None,
                });
            }
        }
        assert!(
            stored_total <= available,
            "the compressed units fit in the clusters the file had"
        );
        let mapping = fs_ntfs::data_runs::encode_runs(&runs).unwrap();
        let mut attribute = vec![0u8; 0x48];
        attribute[..0x40].copy_from_slice(&record[start..start + 0x40]);
        let length = (0x48 + mapping.len()).next_multiple_of(8);
        attribute[4..8].copy_from_slice(&(length as u32).to_le_bytes());
        attribute[0x0A..0x0C].copy_from_slice(&0x48u16.to_le_bytes());
        attribute[0x0C..0x0E].copy_from_slice(&0x0001u16.to_le_bytes());
        attribute[0x18..0x20].copy_from_slice(&((units * UNIT_CLUSTERS - 1) as u64).to_le_bytes());
        attribute[0x20..0x22].copy_from_slice(&0x48u16.to_le_bytes());
        attribute[0x22..0x24].copy_from_slice(&4u16.to_le_bytes());
        attribute[0x28..0x30].copy_from_slice(&((units * unit_bytes) as u64).to_le_bytes());
        attribute[0x30..0x38].copy_from_slice(&(content.len() as u64).to_le_bytes());
        attribute[0x38..0x40].copy_from_slice(&(content.len() as u64).to_le_bytes());
        attribute.extend_from_slice(&mapping);
        attribute.resize(length, 0);
        let new_end = start + length;
        assert!(
            new_end + 8 <= record.len(),
            "the compressed attribute fits in the record"
        );
        record[start..new_end].copy_from_slice(&attribute);
        record[new_end..new_end + 4].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        record[new_end + 4..].fill(0);
        record[0x18..0x1C].copy_from_slice(&((new_end + 8) as u32).to_le_bytes());
        Ok(())
    })
    .unwrap();
    bytes = device.0.lock().unwrap().clone();
    for (at, data) in writes {
        bytes[at..at + data.len()].copy_from_slice(&data);
    }
    (bytes, content, placed)
}

/// What the independent `ntfs` reader says of `docs/packed.txt`: whether it sees a compressed
/// non-resident attribute, its length, and the clusters it maps the value to, read as they are (it
/// does not decode LZNT1, so it checks the layout and not the decoding).
fn independent_layout_of_packed(image: &[u8], number: u64) -> (bool, u64, Vec<u8>) {
    use ntfs::NtfsReadSeek;
    let mut cursor = std::io::Cursor::new(image.to_vec());
    let independent = ntfs::Ntfs::new(&mut cursor).unwrap();
    let file = independent.file(&mut cursor, number).unwrap();
    let data = file.data(&mut cursor, "").unwrap().unwrap();
    let attribute = data.to_attribute().unwrap();
    let compressed = !attribute.is_resident()
        && attribute
            .flags()
            .contains(ntfs::NtfsAttributeFlags::COMPRESSED);
    let mut value = attribute.value(&mut cursor).unwrap();
    let mut raw = vec![0; attribute.value_length() as usize];
    value.read(&mut cursor, &mut raw).unwrap();
    (compressed, attribute.value_length(), raw)
}

#[test]
fn a_file_ntfs_compressed_is_read_whole_and_in_pieces_like_the_independent_reader_reads_it() {
    let (image, content, placed) = compressed_volume();
    // The layout is what NTFS does: a compressed unit, a stored one, a hole, a short last unit.
    assert!(placed[0].1 > 0 && placed[0].1 < UNIT_CLUSTERS, "{placed:?}");
    assert_eq!(placed[1].1, UNIT_CLUSTERS, "{placed:?}");
    assert_eq!(placed[2].1, 0, "{placed:?}");
    assert!(placed[3].1 > 0 && placed[3].1 < UNIT_CLUSTERS, "{placed:?}");
    let session = open(image.clone(), "compressed");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "packed.txt");
    let number = row["recordNumber"].as_u64().unwrap();
    // The independent reader sees a compressed attribute of the right length whose clusters are where
    // the units were put: a unit's stored clusters first, a stored unit as it is, a hole as zeros.
    let (flagged, value_length, raw) = independent_layout_of_packed(&image, number);
    assert!(flagged && value_length == content.len() as u64);
    let unit_bytes = UNIT_CLUSTERS * CLUSTER;
    for (index, (lcn, stored)) in placed.iter().enumerate() {
        let seen = &raw[index * unit_bytes..(index * unit_bytes + unit_bytes).min(raw.len())];
        let there = &image[*lcn as usize * CLUSTER..][..stored * CLUSTER];
        assert_eq!(
            &seen[..(stored * CLUSTER).min(seen.len())],
            &there[..(stored * CLUSTER).min(seen.len())],
            "unit {index} is where it was put"
        );
        assert!(
            seen[(stored * CLUSTER).min(seen.len())..]
                .iter()
                .all(|byte| *byte == 0),
            "unit {index} has a hole after its clusters"
        );
    }
    assert_eq!(row["storage"], "compressed");
    assert_eq!(row["verdict"], "live");
    assert_eq!(row["length"].as_u64().unwrap(), content.len() as u64);
    let id = row["id"].as_i64().unwrap();
    let whole = session
        .read_entry_range(id, 0, content.len() as u64 + 100)
        .unwrap();
    assert!(whole.unreadable.is_empty(), "{:?}", whole.unreadable);
    assert_eq!(sha(&whole.data), sha(&content));
    // Across the join of two units, inside the hole, in the short last unit, and past the end.
    let unit = (UNIT_CLUSTERS * CLUSTER) as u64;
    for (start, length) in [
        (unit - 100, 200u64),
        (2 * unit + 10, 500),
        (3 * unit - 4, 20),
        (content.len() as u64 - 50, 500),
    ] {
        let piece = session.read_entry_range(id, start, length).unwrap();
        let end = (start + length).min(content.len() as u64) as usize;
        assert_eq!(
            piece.data,
            &content[start as usize..end],
            "bytes {start}+{length}"
        );
        assert!(piece.unreadable.is_empty());
    }
}

#[test]
fn a_copy_of_a_file_ntfs_compressed_holds_the_file_byte_for_byte() {
    let (image, content, _) = compressed_volume();
    let session = open(image, "compressed-copy");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "packed.txt");
    let destination = session.workspace.dir().join("recovered");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![row["id"].as_i64().unwrap()],
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, 1);
    let copied = std::fs::read(destination.join("Volume root/docs/packed.txt"))
        .or_else(|_| std::fs::read(destination.join("docs/packed.txt")))
        .expect("the copy wrote packed.txt");
    assert_eq!(sha(&copied), sha(&content));
    let status = session.copy_status().unwrap();
    assert_eq!(
        (status.files_done, status.files_failed, status.files_partial),
        (1, 0, 0),
        "{status:?}"
    );
}

#[test]
fn a_compression_unit_that_does_not_decode_is_marked_and_never_zeros() {
    let (mut image, content, placed) = compressed_volume();
    // Spoil the first chunk header of the first unit: its signature bits are wrong.
    let (lcn, _) = placed[0];
    image[lcn as usize * CLUSTER + 1] = 0x00;
    let session = open(image, "compressed-bad-unit");
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let row = find(&engine, "packed.txt");
    let id = row["id"].as_i64().unwrap();
    let read = session
        .read_entry_range(id, 0, content.len() as u64)
        .unwrap();
    let unit = (UNIT_CLUSTERS * CLUSTER) as u64;
    assert_eq!(read.unreadable.len(), 1, "{:?}", read.unreadable);
    assert_eq!(
        (read.unreadable[0].start, read.unreadable[0].end),
        (0, unit)
    );
    // Every other byte is the file's.
    assert_eq!(&read.data[unit as usize..], &content[unit as usize..]);
}
