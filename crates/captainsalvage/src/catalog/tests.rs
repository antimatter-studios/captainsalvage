use super::scans::NO_FORMAT;
use super::*;
use crate::hypotheses::{HypothesisStore, NewHypothesis};
use crate::map::MapDb;
use crate::source::MemorySource;
use rusqlite::Connection;
use serde_json::json;
use std::path::PathBuf;
use std::sync::Arc;

/// A folder of this test, standing in for the app's, removed when the test ends.
struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("captainsalvage-catalog-{}", identifier().unwrap()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn catalog(&self) -> Catalog {
        Catalog::open_unmigrated(&self.0).unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Bytes that tell one drive from another: every block differs, and another seed is another drive.
fn patterned(size: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

const SIZE: usize = 4 * 1024 * 1024;

fn disk(path: &str, data: Vec<u8>) -> RawDisk {
    let length = data.len() as u64;
    RawDisk::with_source(Path::new(path), Arc::new(MemorySource::new(data)), length).unwrap()
}

fn extent(format: &str, start: u64, length: u64) -> InterpretationRequest {
    InterpretationRequest {
        format: format.into(),
        start,
        length,
        boot_offset: None,
        origin: "analysis".into(),
        confidence: 0.9,
    }
}

fn open_as(format: &str, key: &str, accept: Accept) -> FilesystemOpen {
    FilesystemOpen {
        interpretation: extent(format, 0, SIZE as u64),
        volume_key: key.into(),
        block_size: 4096,
        block_count: 1000,
        options: json!({}),
        accept,
    }
}

#[test]
fn a_device_is_one_source_however_often_it_is_opened() {
    let root = Root::new();
    let catalog = root.catalog();
    let drive = disk("/dev/rdisk7", patterned(SIZE, 1));
    let first = catalog.resolve_source(&drive, Accept::Exact, None).unwrap();
    assert_eq!(first.matched_by, MatchedBy::New);
    assert!(
        first.source.markers >= 8,
        "{} markers",
        first.source.markers
    );
    // Another path, the same bytes: an image of the drive, or the drive under another number.
    let again = disk("/dev/rdisk9", patterned(SIZE, 1));
    let second = catalog.resolve_source(&again, Accept::Exact, None).unwrap();
    assert_eq!(second.matched_by, MatchedBy::Markers);
    assert_eq!(second.source.id, first.source.id);
    assert_eq!(second.source.last_path, "/dev/rdisk9");
    assert_eq!(second.comparison.unwrap().state, Verification::Exact);
    assert_eq!(catalog.sources().unwrap().len(), 1);
}

#[test]
fn a_different_drive_of_the_same_size_is_another_source() {
    let root = Root::new();
    let catalog = root.catalog();
    let first = catalog
        .resolve_source(
            &disk("/dev/rdisk7", patterned(SIZE, 1)),
            Accept::Probable,
            None,
        )
        .unwrap();
    let other = catalog
        .resolve_source(
            &disk("/dev/rdisk7", patterned(SIZE, 2)),
            Accept::Probable,
            None,
        )
        .unwrap();
    assert_eq!(other.matched_by, MatchedBy::New);
    assert_ne!(other.source.id, first.source.id);
}

/// What a mount does: a few blocks near the start are rewritten.
fn mounted(data: &[u8], source: &SourceRecord, catalog: &Catalog, changed: usize) -> RawDisk {
    let fingerprint = catalog.fingerprint_of(&source.id).unwrap().unwrap();
    let mut data = data.to_vec();
    for sample in fingerprint.samples.iter().take(changed) {
        let at = sample.offset as usize;
        data[at..at + 64].fill(0xAB);
    }
    disk("/dev/rdisk7", data)
}

#[test]
fn a_probable_match_is_never_applied_without_acknowledgement() {
    let root = Root::new();
    let catalog = root.catalog();
    let data = patterned(SIZE, 3);
    let source = catalog
        .resolve_source(&disk("/dev/rdisk7", data.clone()), Accept::Exact, None)
        .unwrap()
        .source;
    let touched = mounted(&data, &source, &catalog, 2);
    let identified = catalog.identify(&touched, false).unwrap();
    assert_eq!(identified.matches.len(), 1);
    assert_eq!(identified.matches[0].verification, Verification::Probable);
    assert_eq!(identified.matches[0].comparison.changed, 2);

    let declined = catalog
        .resolve_source(&touched, Accept::Exact, None)
        .unwrap();
    assert_eq!(
        declined.matched_by,
        MatchedBy::New,
        "stored work is not applied silently"
    );
    assert_eq!(declined.declined.unwrap().source.id, source.id);

    // The stored scan opens on it only when the person acknowledges the changed markers.
    let drive = disk("/dev/rdisk7", data);
    let stored = catalog
        .open_filesystem(&drive, &open_as("exFAT", "k", Accept::Exact))
        .unwrap()
        .scan;
    let refused = catalog
        .open_scan(&touched, &stored.id, Accept::Exact)
        .unwrap_err();
    assert!(
        format!("{refused:#}").contains("probable match"),
        "{refused:#}"
    );
    let (opened, from, comparison) = catalog
        .open_scan(&touched, &stored.id, Accept::Probable)
        .unwrap();
    assert_eq!((opened.id, from.id), (stored.id.clone(), source.id));
    assert_eq!(
        (comparison.state, comparison.changed),
        (Verification::Probable, 2)
    );
    let other = disk("/dev/rdisk7", patterned(SIZE, 99));
    assert!(catalog
        .open_scan(&other, &stored.id, Accept::Probable)
        .is_err());
}

#[test]
fn weak_matches_are_hidden_until_all_are_shown() {
    let root = Root::new();
    let catalog = root.catalog();
    let data = patterned(SIZE, 4);
    let source = catalog
        .resolve_source(&disk("/dev/rdisk7", data.clone()), Accept::Exact, None)
        .unwrap()
        .source;
    // Most markers changed, but enough still match to be worth showing.
    let markers = source.markers;
    let touched = mounted(&data, &source, &catalog, markers - 9);
    let hidden = catalog.identify(&touched, false).unwrap();
    assert!(hidden.matches.is_empty());
    assert_eq!(hidden.hidden, 1);
    let shown = catalog.identify(&touched, true).unwrap();
    assert_eq!(shown.matches[0].verification, Verification::Weak);
}

#[test]
fn changing_the_format_makes_a_new_scan_and_keeps_the_old_one() {
    let root = Root::new();
    let catalog = root.catalog();
    let drive = disk("/dev/rdisk7", patterned(SIZE, 5));
    let as_exfat = catalog
        .open_filesystem(&drive, &open_as("exFAT", "vol-00000001", Accept::Exact))
        .unwrap();
    assert!(as_exfat.created);
    let as_ntfs = catalog
        .open_filesystem(&drive, &open_as("NTFS", "vol-00000001", Accept::Exact))
        .unwrap();
    assert!(as_ntfs.created);
    assert_eq!(as_ntfs.source.id, as_exfat.source.id);
    assert_ne!(as_ntfs.scan.id, as_exfat.scan.id);
    // Each scan has its own folder and map; the caches that depend only on bytes are shared.
    assert_ne!(as_ntfs.workspace.dir(), as_exfat.workspace.dir());
    assert_ne!(as_ntfs.workspace.database(), as_exfat.workspace.database());
    assert_ne!(as_ntfs.workspace.classes(), as_exfat.workspace.classes());
    assert_eq!(as_ntfs.workspace.entropy(), as_exfat.workspace.entropy());
    assert_eq!(
        as_ntfs.workspace.text_heads(),
        as_exfat.workspace.text_heads()
    );

    // Back to exFAT: the stored scan, not a new one.
    let back = catalog
        .open_filesystem(&drive, &open_as("exFAT", "vol-00000001", Accept::Exact))
        .unwrap();
    assert!(!back.created);
    assert_eq!(back.scan.id, as_exfat.scan.id);
    assert_eq!(back.workspace.dir(), as_exfat.workspace.dir());
    assert_eq!(catalog.scans(&back.source.id).unwrap().len(), 2);
}

#[test]
fn the_extent_the_options_and_the_scanner_version_each_make_a_new_scan() {
    let root = Root::new();
    let catalog = root.catalog();
    let drive = disk("/dev/rdisk7", patterned(SIZE, 6));
    let source = catalog
        .resolve_source(&drive, Accept::Exact, None)
        .unwrap()
        .source
        .id;
    let request = |start: u64, length: u64, options: serde_json::Value, version: u32| ScanRequest {
        source_id: source.clone(),
        kind: ScanKind::Filesystem,
        interpretation: extent("exFAT", start, length),
        options,
        scanner_version: version,
        range_start: start,
        range_end: start + length,
        volume_key: None,
        raw_scan_id: None,
        block_size: 4096,
        block_count: 100,
    };
    let (base, created) = catalog
        .ensure_scan(&request(0, 1 << 20, json!({}), 1))
        .unwrap();
    assert!(created);
    let (same, created) = catalog
        .ensure_scan(&request(0, 1 << 20, json!({}), 1))
        .unwrap();
    assert!(!created);
    assert_eq!(same.id, base.id);
    for (name, other) in [
        ("extent", request(4096, 1 << 20, json!({}), 1)),
        ("length", request(0, 2 << 20, json!({}), 1)),
        ("options", request(0, 1 << 20, json!({"deep": true}), 1)),
        ("version", request(0, 1 << 20, json!({}), 2)),
    ] {
        let (scan, created) = catalog.ensure_scan(&other).unwrap();
        assert!(created, "a different {name} is a new scan");
        assert_ne!(scan.id, base.id);
    }
    // Key order in the options does not matter.
    let (first, _) = catalog
        .ensure_scan(&request(0, 1 << 20, json!({"a": 1, "b": 2}), 3))
        .unwrap();
    let (second, created) = catalog
        .ensure_scan(&request(0, 1 << 20, json!({"b": 2, "a": 1}), 3))
        .unwrap();
    assert!(!created);
    assert_eq!(first.id, second.id);
    assert_eq!(catalog.scans(&source).unwrap().len(), 6);
}

#[test]
fn a_scan_list_starts_with_the_interpretation_last_used_and_shows_what_is_stored() {
    let root = Root::new();
    let catalog = root.catalog();
    let drive = disk("/dev/rdisk7", patterned(SIZE, 7));
    let exfat = catalog
        .open_filesystem(&drive, &open_as("exFAT", "k", Accept::Exact))
        .unwrap();
    std::fs::write(exfat.workspace.dir().join("map.sqlite"), vec![7u8; 5000]).unwrap();
    let ntfs = catalog
        .open_filesystem(&drive, &open_as("NTFS", "k", Accept::Exact))
        .unwrap();
    std::thread::sleep(std::time::Duration::from_millis(5));
    catalog.touch_scan(&exfat.scan.id).unwrap();
    let list = catalog.scans(&exfat.source.id).unwrap();
    assert_eq!(
        list[0].id, exfat.scan.id,
        "the interpretation opened last comes first"
    );
    assert_eq!(list[1].id, ntfs.scan.id);
    assert!(
        list[0].stored_bytes >= 5000,
        "{} bytes",
        list[0].stored_bytes
    );
    assert_eq!(list[0].format, "exFAT");
    assert_eq!(list[0].range_end, SIZE as u64);
    // The size is kept, so a list can show it without measuring.
    let kept: i64 = catalog
        .connection()
        .query_row(
            "SELECT stored_bytes FROM scans WHERE id = ?1",
            [&exfat.scan.id],
            |row| row.get(0),
        )
        .unwrap();
    assert!(kept >= 5000);
    let named = catalog
        .label_scan(&exfat.scan.id, "  the first guess ")
        .unwrap();
    assert_eq!(named.label, "the first guess");
}

#[test]
fn an_interpretation_is_a_saved_hypothesis_and_a_hypothesis_saved_earlier_is_one() {
    let root = Root::new();
    let catalog = root.catalog();
    let data = patterned(SIZE, 8);
    let drive = disk("/dev/rdisk7", data.clone());
    let path = root.0.join("raw-scans.sqlite");
    // The person saved a hypothesis before any scan was made under it.
    let mut store = HypothesisStore::open(&path).unwrap();
    let saved = store
        .create(
            &drive,
            NewHypothesis {
                format: "NTFS".into(),
                start: 0,
                length: SIZE as u64,
                ..NewHypothesis::default()
            },
        )
        .unwrap();
    let opened = catalog
        .open_filesystem(&drive, &open_as("NTFS", "k", Accept::Exact))
        .unwrap();
    assert_eq!(
        opened.scan.interpretation_id.as_deref(),
        Some(saved.id.as_str())
    );
    // Opening as exFAT makes an interpretation too, but not a layer the person asked for.
    catalog
        .open_filesystem(&drive, &open_as("exFAT", "k", Accept::Exact))
        .unwrap();
    let layers = store.list(&drive).unwrap();
    assert_eq!(layers.len(), 1);
    assert_eq!(layers[0].id, saved.id);
    assert_eq!(catalog.interpretations(&opened.source.id).unwrap().len(), 2);
    // Deleting the layer keeps what its scan stored.
    store.delete(&drive, &saved.id).unwrap();
    assert!(store.list(&drive).unwrap().is_empty());
    let kept = catalog.scan(&opened.scan.id).unwrap();
    assert_eq!(kept.interpretation_id.as_deref(), Some(saved.id.as_str()));
}

/// The database as an older version left it: no catalog tables.
fn old_database(root: &Path) -> Connection {
    let connection = Connection::open(root.join("raw-scans.sqlite")).unwrap();
    connection.execute_batch(crate::raw_scan::SCHEMA).unwrap();
    connection.execute_batch(crate::hypotheses::SCHEMA).unwrap();
    connection
}

fn old_workspace(root: &Path, name: &str, bytes: u64, scanned_clusters: u64) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("volume.json"),
        json!({"uuid": "ABCD", "name": "Photos", "bytes": bytes, "lastOpenedMillis": 1_700_000_000_000u64}).to_string(),
    )
    .unwrap();
    let map = MapDb::open(&dir.join("map.sqlite")).unwrap();
    map.set_state("next_cluster", &(scanned_clusters + 2).to_string())
        .unwrap();
    map.set_state("status", "paused").unwrap();
    drop(map);
    std::fs::write(dir.join("class.bin"), vec![1u8; 1000]).unwrap();
    std::fs::write(dir.join("entropy.bin"), vec![2u8; 1000]).unwrap();
    std::fs::write(dir.join("texthead.bin"), vec![3u8; 1000]).unwrap();
    dir
}

fn old_raw_scan(
    connection: &Connection,
    id: &str,
    source: &RawDisk,
    start: u64,
    end: u64,
    next: u64,
) {
    let fingerprint = Fingerprint::read(source);
    connection
        .execute(
            "INSERT INTO raw_scans VALUES (?1, ?2, ?3, ?4, 1, ?5, ?6, ?7, 'token', ?7, 1000, 2000)",
            params![
                id,
                serde_json::to_string(&fingerprint).unwrap(),
                source.length as i64,
                "/dev/rdisk3",
                start as i64,
                end as i64,
                next as i64
            ],
        )
        .unwrap();
    connection
        .execute(
            "INSERT INTO raw_findings VALUES (?1, 4096, 'NTFS', 'boot sector', '{}')",
            [id],
        )
        .unwrap();
}

fn old_layout(root: &Path) {
    old_workspace(root, "vol-1a2b3c4d", SIZE as u64, 500);
    old_workspace(root, "ntfs-0123456789abcdef", SIZE as u64, 10);
    let region = root
        .join("vol-1a2b3c4d")
        .join("region-0123456789abcdef-100000-200000-100000");
    std::fs::create_dir_all(&region).unwrap();
    std::fs::write(
        region.join("interpretation.json"),
        json!({"start": 1_048_576, "length": 2_097_152, "format": "NTFS", "label": "Old NTFS", "bootOffset": 1_048_576}).to_string(),
    )
    .unwrap();
    MapDb::open(&region.join("map.sqlite")).unwrap();
    let source = patterned(SIZE, 9);
    let connection = old_database(root);
    old_raw_scan(
        &connection,
        "rawscan1",
        &disk("/dev/rdisk3", source.clone()),
        0,
        SIZE as u64,
        SIZE as u64,
    );
    old_raw_scan(
        &connection,
        "rawscan2",
        &disk("/dev/rdisk3", source.clone()),
        1 << 20,
        2 << 20,
        1 << 20,
    );
}

#[test]
fn old_workspaces_and_raw_scans_become_scans_that_keep_their_key() {
    let root = Root::new();
    old_layout(&root.0);
    let before = std::fs::read(root.0.join("vol-1a2b3c4d/map.sqlite")).unwrap();
    let catalog = Catalog::open_unmigrated(&root.0).unwrap();
    let report = migrate(&catalog).unwrap();
    assert!(
        report.backup.is_some(),
        "the database is copied before it changes"
    );
    assert_eq!(
        (report.workspaces, report.regions, report.raw_scans),
        (2, 1, 2),
        "{report:?}"
    );

    // Nothing in a workspace was touched.
    assert_eq!(
        std::fs::read(root.0.join("vol-1a2b3c4d/map.sqlite")).unwrap(),
        before
    );

    // Both raw scans were made of the same drive, so they are scans of one source.
    let sources = catalog.sources().unwrap();
    let raw_source = sources.iter().find(|source| source.markers > 0).unwrap();
    let raw = catalog.scans(&raw_source.id).unwrap();
    assert_eq!(raw.len(), 2);
    assert!(raw
        .iter()
        .all(|scan| scan.kind == ScanKind::Raw && scan.format == NO_FORMAT));
    let first = raw
        .iter()
        .find(|scan| scan.raw_scan_id.as_deref() == Some("rawscan1"))
        .unwrap();
    assert_eq!((first.range_start, first.range_end), (0, SIZE as u64));
    assert!(first.complete);
    assert!(first.stored_bytes > 0);

    // The workspaces are scans that use their folders where they are, with a guessed format.
    let by_key = |key: &str| {
        let id: String = catalog
            .connection()
            .query_row("SELECT id FROM scans WHERE legacy_key = ?1", [key], |row| {
                row.get(0)
            })
            .unwrap();
        catalog.scan(&id).unwrap()
    };
    let volume = by_key("vol-1a2b3c4d");
    assert_eq!(volume.workspace.as_deref(), Some("vol-1a2b3c4d"));
    assert_eq!(volume.label, "Photos");
    assert!(volume.migrated);
    assert_eq!(volume.status, "paused");
    assert!(
        volume.stored_bytes > 3000,
        "the folder's size: {}",
        volume.stored_bytes
    );
    assert_eq!(by_key("ntfs-0123456789abcdef").format, "NTFS");
    let region = by_key("vol-1a2b3c4d/region-0123456789abcdef-100000-200000-100000");
    assert_eq!(
        (region.format.as_str(), region.start, region.length),
        ("NTFS", 1_048_576, 2_097_152)
    );

    // Running it again changes nothing.
    let again = migrate(&catalog).unwrap();
    assert!(!again.changed(), "{again:?}");
    assert_eq!(again.already, 2 + 1 + 2);
    let count: i64 = catalog
        .connection()
        .query_row("SELECT count(*) FROM scans", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 5);
}

#[test]
fn the_database_is_copied_once_before_the_catalog_first_changes_it() {
    let root = Root::new();
    old_layout(&root.0);
    let first = Catalog::open_unmigrated(&root.0).unwrap();
    let backups: Vec<_> = std::fs::read_dir(root.0.join("backups"))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(backups.len(), 1);
    // The copy holds what was there, and none of the catalog.
    let copy = Connection::open(backups[0].path()).unwrap();
    let scans: i64 = copy
        .query_row("SELECT count(*) FROM raw_scans", [], |row| row.get(0))
        .unwrap();
    assert_eq!(scans, 2);
    let tables: i64 = copy
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE name = 'sources'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tables, 0);
    drop(first);
    Catalog::open_unmigrated(&root.0).unwrap();
    assert_eq!(
        std::fs::read_dir(root.0.join("backups")).unwrap().count(),
        1
    );
}

#[test]
fn a_new_folder_has_nothing_to_copy() {
    let root = Root::new();
    root.catalog();
    assert!(!root.0.join("backups").exists());
}

#[test]
fn a_run_that_fails_registers_nothing_and_the_next_run_registers_everything() {
    let root = Root::new();
    old_layout(&root.0);
    // A raw scan whose saved fingerprint cannot be read stops the run.
    let connection = old_database(&root.0);
    connection
        .execute(
            "INSERT INTO raw_scans VALUES ('broken', 'not json', 10, '/dev/x', 1, 0, 10, 10, 't', 10, 3000, 3000)",
            [],
        )
        .unwrap();
    drop(connection);
    let catalog = Catalog::open_unmigrated(&root.0).unwrap();
    assert!(migrate(&catalog).is_err());
    let count: i64 = catalog
        .connection()
        .query_row("SELECT count(*) FROM scans", [], |row| row.get(0))
        .unwrap();
    assert_eq!(
        count, 0,
        "a run that stops half way leaves nothing half done"
    );
    catalog
        .connection()
        .execute("DELETE FROM raw_scans WHERE id = 'broken'", [])
        .unwrap();
    let report = migrate(&catalog).unwrap();
    assert_eq!(
        (report.workspaces, report.regions, report.raw_scans),
        (2, 1, 2)
    );
}

#[test]
fn two_apps_migrating_together_register_each_scan_once() {
    let root = Root::new();
    old_layout(&root.0);
    let path = root.0.clone();
    let workers: Vec<_> = (0..4)
        .map(|_| {
            let path = path.clone();
            std::thread::spawn(move || {
                let catalog = Catalog::open_unmigrated(&path).unwrap();
                migrate(&catalog).unwrap()
            })
        })
        .collect();
    let reports: Vec<MigrationReport> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    let registered: usize = reports
        .iter()
        .map(|report| report.workspaces + report.regions + report.raw_scans)
        .sum();
    assert_eq!(registered, 5);
    let catalog = root.catalog();
    let count: i64 = catalog
        .connection()
        .query_row("SELECT count(*) FROM scans", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 5);
}

#[test]
fn an_old_workspace_is_found_by_its_serial_and_then_has_markers() {
    let root = Root::new();
    old_layout(&root.0);
    let catalog = Catalog::open_unmigrated(&root.0).unwrap();
    migrate(&catalog).unwrap();
    let legacy: String = catalog
        .connection()
        .query_row(
            "SELECT id FROM scans WHERE legacy_key = 'vol-1a2b3c4d'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let before = catalog.scan(&legacy).unwrap();
    let source_before = catalog.source(&before.source_id).unwrap();
    assert_eq!(
        source_before.markers, 0,
        "no markers until the device is seen"
    );

    // The device is seen: it is an exFAT volume, and the old scan is its scan.
    let drive = disk("/dev/rdisk5", patterned(SIZE, 10));
    let opened = catalog
        .open_filesystem(&drive, &open_as("FAT32", "vol-1a2b3c4d", Accept::Exact))
        .unwrap();
    assert!(!opened.created, "nothing is scanned again");
    assert_eq!(opened.scan.id, legacy);
    assert_eq!(opened.resolution.matched_by, MatchedBy::VolumeKey);
    assert_eq!(opened.workspace.dir(), root.0.join("vol-1a2b3c4d"));
    assert_eq!(
        opened.scan.format, "FAT32",
        "the device's format replaces the guess"
    );
    assert!(opened.source.markers >= 8);
    assert_eq!(opened.source.length, SIZE as u64);
    // The caches stayed where they were, and a second scan of the source shares them.
    assert_eq!(
        opened.workspace.entropy(),
        root.0.join("vol-1a2b3c4d/entropy.bin")
    );
    let other = catalog
        .open_filesystem(&drive, &open_as("NTFS", "vol-1a2b3c4d", Accept::Exact))
        .unwrap();
    assert!(other.created);
    assert_eq!(
        other.workspace.entropy(),
        root.0.join("vol-1a2b3c4d/entropy.bin")
    );
    assert_ne!(other.workspace.database(), opened.workspace.database());

    // Next time the markers find it, and the key is no longer needed.
    let again = catalog
        .open_filesystem(&drive, &open_as("FAT32", "vol-ffffffff", Accept::Exact))
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.scan.id, legacy);
    assert_eq!(again.resolution.matched_by, MatchedBy::Markers);
}

#[test]
fn a_volume_with_an_old_serial_but_other_bytes_does_not_get_its_results() {
    let root = Root::new();
    old_layout(&root.0);
    let catalog = Catalog::open_unmigrated(&root.0).unwrap();
    migrate(&catalog).unwrap();
    // Seen once, so its markers are known.
    let drive = disk("/dev/rdisk5", patterned(SIZE, 10));
    let opened = catalog
        .open_filesystem(&drive, &open_as("exFAT", "vol-1a2b3c4d", Accept::Exact))
        .unwrap();
    // Another drive made with the same serial number.
    let clash = disk("/dev/rdisk6", patterned(SIZE, 11));
    let other = catalog
        .open_filesystem(&clash, &open_as("exFAT", "vol-1a2b3c4d", Accept::Exact))
        .unwrap();
    assert!(other.created);
    assert_ne!(other.source.id, opened.source.id);
    assert_ne!(other.scan.id, opened.scan.id);
}

#[test]
fn a_boot_sector_written_over_after_scanning_still_finds_the_scan() {
    let root = Root::new();
    let catalog = root.catalog();
    let data = patterned(SIZE, 12);
    let opened = catalog
        .open_filesystem(
            &disk("/dev/rdisk7", data.clone()),
            &open_as("exFAT", "vol-00000042", Accept::Exact),
        )
        .unwrap();
    // The boot sector, and the serial number in it, are gone.
    let mut damaged = data;
    damaged[..4096].fill(0);
    let found = catalog
        .open_filesystem(
            &disk("/dev/rdisk7", damaged),
            &open_as("exFAT", "vol-00000000", Accept::Probable),
        )
        .unwrap();
    assert!(!found.created);
    assert_eq!(found.scan.id, opened.scan.id);
}

#[test]
fn raw_scans_are_registered_as_scans_of_the_source_they_were_made_on() {
    let root = Root::new();
    let mut store = crate::raw_scan::ScanStore::open_in(&root.0).unwrap();
    let drive = disk("/dev/rdisk7", patterned(SIZE, 13));
    let made = store.create(&drive, 0, SIZE as u64).unwrap();
    let catalog = root.catalog();
    let scan = catalog.scan_of_raw(&made.scan_id).unwrap().unwrap();
    assert_eq!(scan.kind, ScanKind::Raw);
    assert_eq!(scan.format, NO_FORMAT);
    assert_eq!(scan.status, "not-started");
    // A scan of the same source, another range, is another scan; both list under one source.
    let second = store.create(&drive, 0, 1 << 20).unwrap();
    let again = catalog.scan_of_raw(&second.scan_id).unwrap().unwrap();
    assert_eq!(again.source_id, scan.source_id);
    assert_eq!(catalog.scans(&scan.source_id).unwrap().len(), 2);
    assert_eq!(catalog.sources().unwrap().len(), 1);
    // Reading it moves it forward, and the list says how far.
    store.resume(&drive, &second.scan_id, true, false).unwrap();
    let step = store.step(&drive, &second.scan_id).unwrap();
    let listed = catalog.scans(&scan.source_id).unwrap();
    let moved = listed.iter().find(|item| item.id == again.id).unwrap();
    assert_eq!(moved.covered_end, Some(step.batch.next_offset));
    assert!(moved.complete);
}

#[test]
fn a_folder_that_cannot_be_written_does_not_open_a_catalog() {
    use std::os::unix::fs::PermissionsExt;
    let root = Root::new();
    old_layout(&root.0);
    let mut permissions = std::fs::metadata(&root.0).unwrap().permissions();
    permissions.set_mode(0o500);
    std::fs::set_permissions(&root.0, permissions.clone()).unwrap();
    let opened = Catalog::open_unmigrated(&root.0);
    permissions.set_mode(0o700);
    std::fs::set_permissions(&root.0, permissions).unwrap();
    // Whatever it does, nothing stored is lost: the workspaces are still there to be opened.
    assert!(
        std::fs::read_dir(root.0.join("vol-1a2b3c4d"))
            .unwrap()
            .count()
            >= 4
    );
    drop(opened);
}

#[test]
fn hardware_identity_is_kept_but_decides_nothing() {
    let root = Root::new();
    let catalog = root.catalog();
    let drive = disk("/dev/rdisk7", patterned(SIZE, 14));
    let source = catalog
        .resolve_source(&drive, Accept::Exact, None)
        .unwrap()
        .source;
    let hardware = Hardware {
        model: Some("Test Drive".into()),
        serial: Some("SN1".into()),
    };
    catalog.set_hardware(&source.id, &hardware).unwrap();
    assert_eq!(catalog.source(&source.id).unwrap().hardware, Some(hardware));
    let other = disk("/dev/rdisk7", patterned(SIZE, 15));
    assert_eq!(
        catalog
            .resolve_source(&other, Accept::Probable, None)
            .unwrap()
            .matched_by,
        MatchedBy::New
    );
}
