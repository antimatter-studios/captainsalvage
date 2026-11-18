//! Rescanning the changed regions of a scan reopened on a probable match.
//!
//! A source whose markers changed a little (a mount wrote metadata) is still recognised. What a
//! scan stored near the changed markers may be out of date, so it is marked stale until read
//! again, and the person's action reads those regions and nothing else. Synthetic images only,
//! kept in a throwaway home folder; the sources are only written by the tests, to play the part
//! of a mount, and never by the app.

use captainsalvage::catalog::Catalog;
use captainsalvage::disk::RawDisk;
use captainsalvage::engine::{Engine, AUTHOR_CLAUDE, AUTHOR_USER};
use captainsalvage::map::MapDb;
use captainsalvage::raw_scan::{Fingerprint, ScanStore, Verification};
use captainsalvage::scan::{rescan_changed, run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::stale::DEFAULT_HEAD_BYTES;
use fs_exfat::fixtures::{self, Builder};
use serde_json::{json, Value};
use std::os::unix::fs::FileExt;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::sync::OnceLock;

const MIB: u64 = 1024 * 1024;

fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("captainsalvage-stale-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("HOME", &path);
        path
    })
}

fn noise(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

/// A sector that raw discovery calls an NTFS boot sector.
fn boot_sector() -> [u8; 512] {
    let mut sector = [0u8; 512];
    sector[3..11].copy_from_slice(b"NTFS    ");
    sector[11..13].copy_from_slice(&512u16.to_le_bytes());
    sector[13] = 8;
    sector[40..48].copy_from_slice(&1024u64.to_le_bytes());
    sector[510..512].copy_from_slice(&[0x55, 0xaa]);
    sector
}

struct Image(PathBuf);

impl Image {
    fn new(label: &str, data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-stale-{}-{label}.img",
            std::process::id()
        ));
        std::fs::write(&path, data).unwrap();
        Self(path)
    }

    /// What a mount does: writes a few bytes where it likes.
    fn write(&self, offset: u64, data: &[u8]) {
        let file = std::fs::OpenOptions::new()
            .write(true)
            .open(&self.0)
            .unwrap();
        file.write_all_at(data, offset).unwrap();
    }

    fn disk(&self) -> RawDisk {
        RawDisk::open(&self.0).unwrap()
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A 36 MiB source of noise with a boot sector inside its first 32 MiB and one beyond.
fn raw_image(label: &str, seed: u64) -> Image {
    let mut data = noise(36 * MIB as usize, seed);
    data[4 * MIB as usize..4 * MIB as usize + 512].copy_from_slice(&boot_sector());
    data[34 * MIB as usize..34 * MIB as usize + 512].copy_from_slice(&boot_sector());
    Image::new(label, &data)
}

struct Root(PathBuf);

impl Root {
    fn new(label: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-stale-root-{}-{label}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn store(&self) -> ScanStore {
        ScanStore::open_in(&self.0).unwrap()
    }

    fn catalog(&self) -> Catalog {
        Catalog::open(&self.0).unwrap()
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Scans the whole source with a saved raw scan, and returns its id and the catalog's id for it.
fn scan_raw(store: &mut ScanStore, catalog: &Catalog, disk: &RawDisk) -> (String, String) {
    let created = store.create(disk, 0, disk.length).unwrap();
    while store.current(disk, &created.scan_id).unwrap().next_offset
        < store.current(disk, &created.scan_id).unwrap().scan_end
    {
        store.step(disk, &created.scan_id).unwrap();
    }
    let scan = catalog.scan_of_raw(&created.scan_id).unwrap().unwrap();
    (created.scan_id, scan.id)
}

fn offsets(snapshot: &captainsalvage::raw_scan::ScanSnapshot, stale: bool) -> Vec<u64> {
    snapshot
        .findings
        .iter()
        .filter(|finding| finding.stale == stale)
        .map(|finding| finding.offset)
        .collect()
}

fn state_of(store: &ScanStore, disk: &RawDisk) -> Verification {
    store.list(disk).unwrap().scans[0].verification
}

#[test]
fn an_exact_match_has_no_stale_regions() {
    let image = raw_image("exact", 1);
    let root = Root::new("exact");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let (raw, scan) = scan_raw(&mut store, &catalog, &disk);
    drop(disk);

    let again = image.disk();
    let loaded = store.resume(&again, &raw, true, false).unwrap();
    assert!(loaded.stale.regions.is_empty());
    assert_eq!(loaded.stale.stale, 0);
    assert!(loaded.findings.iter().all(|finding| !finding.stale));
    assert_eq!(catalog.stale_regions(&scan).unwrap().len(), 0);
    assert_eq!(catalog.scan(&scan).unwrap().stale_regions, 0);

    // Even a person asking to rescan finds nothing to read, and the markers are left alone.
    store.rescan_begin(&again, &raw).unwrap();
    let report = store.rescan_changed(&again, &raw, |_| true).unwrap();
    assert_eq!(report.regions_rescanned, 0);
    assert!(!report.markers_rerecorded);
    let source = catalog.scan(&scan).unwrap().source_id;
    assert!(catalog.marker_history(&source).unwrap().is_empty());
}

#[test]
fn a_changed_marker_marks_the_regions_around_it_and_the_findings_in_them() {
    let image = raw_image("mark", 2);
    let root = Root::new("mark");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let (raw, scan) = scan_raw(&mut store, &catalog, &disk);
    let first = store.current(&disk, &raw).unwrap();
    assert_eq!(offsets(&first, false), [4 * MIB, 34 * MIB]);
    drop(disk);

    // A mount writes near the start of the drive: the first marker's block changes.
    image.write(0, &noise(4096, 999));
    let again = image.disk();
    assert_eq!(state_of(&store, &again), Verification::Probable);
    let refused = store.resume(&again, &raw, true, false);
    assert!(refused.unwrap_err().to_string().contains("probable match"));
    assert_eq!(
        catalog.stale_regions(&scan).unwrap().len(),
        0,
        "nothing is marked until the person accepts the changed markers"
    );

    let loaded = store.resume(&again, &raw, true, true).unwrap();
    assert_eq!(loaded.stale.stale, 1);
    let region = &loaded.stale.regions[0];
    assert_eq!((region.start, region.end), (0, DEFAULT_HEAD_BYTES));
    assert_eq!(region.markers, [0]);
    assert_eq!(region.done_to, 0);
    assert!(region.cleared_at_millis.is_none());
    assert_eq!(
        offsets(&loaded, true),
        [4 * MIB],
        "the finding in the structure area is out of date"
    );
    assert_eq!(
        offsets(&loaded, false),
        [34 * MIB],
        "the one far from the change is not"
    );
    assert_eq!(catalog.scan(&scan).unwrap().stale_regions, 1);

    // Reopening on the same changed source marks the same regions, not more.
    let twice = store.resume(&again, &raw, true, true).unwrap();
    assert_eq!(twice.stale.regions.len(), 1);
    assert_eq!(twice.stale.regions[0].start, 0);
    assert_eq!(twice.stale.regions[0].end, DEFAULT_HEAD_BYTES);
    assert_eq!(twice.changed_source.unwrap().resumes, 2);
}

#[test]
fn a_change_beyond_what_the_scan_read_marks_nothing() {
    // A bigger noise source, so a marker lies well past what the scan covers.
    let image = Image::new("middle", &noise(100 * MIB as usize, 3));
    let root = Root::new("middle");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let created = store.create(&disk, 0, 20 * MIB).unwrap();
    let scan = catalog.scan_of_raw(&created.scan_id).unwrap().unwrap();
    let fingerprint = Fingerprint::read(&disk);
    drop(disk);
    // The scan covers the first 20 MiB only, so a change far past it is outside its results.
    let far = fingerprint
        .samples
        .iter()
        .map(|sample| sample.offset)
        .find(|offset| *offset > 60 * MIB)
        .unwrap();
    image.write(far, &noise(4096, 5));
    let again = image.disk();
    let loaded = store.resume(&again, &created.scan_id, true, true).unwrap();
    assert!(
        loaded.stale.regions.is_empty(),
        "a change beyond what the scan has read leaves nothing of it out of date"
    );
    assert_eq!(catalog.scan(&scan.id).unwrap().stale_regions, 0);
}

#[test]
fn a_rescan_replaces_the_findings_clears_the_marks_and_records_the_markers_again() {
    let image = raw_image("rescan", 4);
    let root = Root::new("rescan");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let (raw, scan) = scan_raw(&mut store, &catalog, &disk);
    let source = catalog.scan(&scan).unwrap().source_id;
    drop(disk);

    // The mount rewrote the first block, took the structure at 4 MiB away and wrote one at 8 MiB.
    image.write(0, &noise(4096, 77));
    image.write(4 * MIB, &noise(512, 78));
    image.write(8 * MIB, &boot_sector());
    let again = image.disk();
    store.resume(&again, &raw, true, true).unwrap();

    store.rescan_begin(&again, &raw).unwrap();
    let report = store.rescan_changed(&again, &raw, |_| true).unwrap();
    assert_eq!(report.regions_rescanned, 1);
    assert_eq!(report.regions_remaining, 0);
    assert_eq!(report.bytes_rescanned, DEFAULT_HEAD_BYTES);
    assert!(report.stopped.is_none());
    assert!(report.markers_rerecorded);

    let snapshot = store.current(&again, &raw).unwrap();
    assert_eq!(offsets(&snapshot, true), Vec::<u64>::new());
    assert_eq!(
        offsets(&snapshot, false),
        [8 * MIB, 34 * MIB],
        "the old structure is gone, the new one is found, the far one stays"
    );
    assert_eq!(snapshot.stale.stale, 0);
    assert!(snapshot.stale.regions[0].cleared_at_millis.is_some());
    assert_eq!(snapshot.stale.regions[0].done_to, DEFAULT_HEAD_BYTES);
    assert_eq!(catalog.scan(&scan).unwrap().stale_regions, 0);

    // The markers were recorded again, so the same drive is exact now, and the earlier markers
    // are in the source's history.
    assert_eq!(state_of(&store, &image.disk()), Verification::Exact);
    let history = catalog.marker_history(&source).unwrap();
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].reason, "rescan of changed regions");
    let kept = history[0].fingerprint.as_ref().unwrap();
    assert_ne!(
        kept.sampled_sha256,
        catalog
            .source(&source)
            .map(|_| Fingerprint::read(&image.disk()).sampled_sha256)
            .unwrap(),
        "the history holds the markers from before"
    );
    let reopened = store.resume(&image.disk(), &raw, true, false).unwrap();
    assert!(
        reopened.changed_source.is_none(),
        "no longer a scan resumed on a changed source"
    );
}

#[test]
fn an_interrupted_rescan_stays_stale_and_resumes_and_the_markers_wait_for_success() {
    let image = raw_image("interrupted", 5);
    let root = Root::new("interrupted");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let (raw, scan) = scan_raw(&mut store, &catalog, &disk);
    let source = catalog.scan(&scan).unwrap().source_id;
    drop(disk);
    image.write(0, &noise(4096, 11));
    image.write(4 * MIB, &noise(512, 13));
    image.write(20 * MIB, &boot_sector());
    let again = image.disk();
    store.resume(&again, &raw, true, true).unwrap();

    // The first batch of the 32 MiB region is read, and then the rescan is stopped.
    store.rescan_begin(&again, &raw).unwrap();
    let mut batches = 0;
    let stopped = store
        .rescan_changed(&again, &raw, |_| {
            batches += 1;
            false
        })
        .unwrap();
    assert_eq!(batches, 1);
    assert_eq!(stopped.regions_remaining, 1);
    assert!(stopped.stopped.is_some());
    assert!(!stopped.markers_rerecorded);
    let region = catalog.stale_regions(&scan).unwrap().remove(0);
    assert_eq!(region.done_to, 16 * MIB, "how far it got is kept");
    assert!(catalog.marker_history(&source).unwrap().is_empty());
    assert_eq!(state_of(&store, &image.disk()), Verification::Probable);
    let partial = store.current(&again, &raw).unwrap();
    assert_eq!(partial.stale.stale, 1, "still stale, so still flagged");
    assert_eq!(
        offsets(&partial, true),
        Vec::<u64>::new(),
        "the batch that was read replaced its findings, the rest of the region has none yet"
    );
    assert_eq!(offsets(&partial, false), [34 * MIB]);

    // Another run, in a new process in effect: it begins again and reads only what is left.
    let later = image.disk();
    store.rescan_begin(&later, &raw).unwrap();
    let mut read_from = Vec::new();
    let done = store
        .rescan_changed(&later, &raw, |step| {
            read_from.push(step.read_from);
            true
        })
        .unwrap();
    assert_eq!(read_from, [16 * MIB]);
    assert_eq!(done.regions_remaining, 0);
    assert!(done.markers_rerecorded);
    assert_eq!(catalog.marker_history(&source).unwrap().len(), 1);
    assert_eq!(state_of(&store, &image.disk()), Verification::Exact);
    let snapshot = store.current(&later, &raw).unwrap();
    assert_eq!(offsets(&snapshot, false), [20 * MIB, 34 * MIB]);
}

#[test]
fn a_rescan_that_loses_the_drive_leaves_the_region_stale() {
    let image = raw_image("failed", 6);
    let root = Root::new("failed");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let (raw, scan) = scan_raw(&mut store, &catalog, &disk);
    let source = catalog.scan(&scan).unwrap().source_id;
    drop(disk);
    image.write(0, &noise(4096, 12));
    // The same bytes, in memory, so the drive can be unplugged part way.
    let memory = Arc::new(MemorySource::new(std::fs::read(&image.0).unwrap()));
    let again = RawDisk::with_source(&image.0, memory.clone(), 36 * MIB).unwrap();
    store.resume(&again, &raw, true, true).unwrap();
    store.rescan_begin(&again, &raw).unwrap();
    memory.disconnect();
    let report = store.rescan_changed(&again, &raw, |_| true).unwrap();
    assert!(report.stopped.is_some(), "{report:?}");
    assert_eq!(report.regions_remaining, 1);
    assert!(!report.markers_rerecorded);
    assert_eq!(catalog.scan(&scan).unwrap().stale_regions, 1);
    assert!(catalog.marker_history(&source).unwrap().is_empty());
}

#[test]
fn marking_and_the_tables_are_idempotent_and_additive() {
    let image = raw_image("idempotent", 7);
    let root = Root::new("idempotent");
    let (mut store, catalog) = (root.store(), root.catalog());
    let disk = image.disk();
    let (_, scan) = scan_raw(&mut store, &catalog, &disk);
    let source = catalog.scan(&scan).unwrap().source_id;
    let options = captainsalvage::stale::StaleOptions::default();
    for _ in 0..3 {
        catalog
            .mark_changed(&source, &[0, 12 * MIB], &options)
            .unwrap();
    }
    let regions = catalog.stale_regions(&scan).unwrap();
    assert_eq!(
        regions.len(),
        1,
        "touching regions join and repeats add nothing"
    );
    assert_eq!(regions[0].markers, [0, 12 * MIB]);

    // Opening the database again, and again, creates the tables only once and changes nothing.
    drop(catalog);
    drop(store);
    for _ in 0..3 {
        let catalog = Catalog::open(&root.0).unwrap();
        let _ = ScanStore::open_in(&root.0).unwrap();
        assert_eq!(catalog.stale_regions(&scan).unwrap(), regions);
    }
    // A wider rule only ever adds to what is stale.
    let catalog = root.catalog();
    catalog
        .mark_changed(
            &source,
            &[40 * MIB],
            &captainsalvage::stale::StaleOptions {
                radius_bytes: 0,
                head_bytes: 0,
            },
        )
        .unwrap();
    assert_eq!(
        catalog.stale_regions(&scan).unwrap().len(),
        1,
        "40 MiB is beyond what the scan read"
    );
}

// ---- a filesystem scan ----

/// An exFAT volume of its own, with noise after its first mebibyte.
fn exfat(seed: u32) -> Vec<u8> {
    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * MIB as usize, 0);
    builder.write_boot();
    let sector = fixtures::SECTOR;
    builder.image[100..104].copy_from_slice(&(0x7000_0000 + seed).to_le_bytes());
    fixtures::seal_boot(&mut builder.image[..sector * 12], sector);
    let main = builder.image[..sector * 12].to_vec();
    builder.image[sector * 12..sector * 24].copy_from_slice(&main);
    builder.image[sector * 12 + 112] = 59;
    let tail = noise(builder.image.len() - MIB as usize, u64::from(seed));
    builder.image[MIB as usize..].copy_from_slice(&tail);
    builder.image
}

fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

fn scan_fully(session: &Session) {
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 16 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
}

/// What the scan stored, as (id, cluster, offset), for comparing before and after.
fn stored_sets(session: &Session) -> Vec<(i64, u32, u32)> {
    let connection = rusqlite::Connection::open(session.workspace.database()).unwrap();
    let mut statement = connection
        .prepare("SELECT id, source_cluster, source_offset FROM sets ORDER BY id")
        .unwrap();
    let rows = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap();
    rows.map(Result::unwrap).collect()
}

/// A mount changes two of the source's markers.
fn change_two_markers(image: &Image) {
    let samples = Fingerprint::read(&image.disk()).samples;
    assert!(samples.len() >= 8, "{} markers", samples.len());
    image.write(samples[3].offset, &noise(4096, 301));
    image.write(samples[6].offset, &noise(4096, 302));
}

fn stale_state(value: &Value) -> u64 {
    value["stale"]["stale"].as_u64().unwrap()
}

#[test]
fn a_filesystem_scan_marks_its_rows_and_a_rescan_replaces_them_and_rebuilds_the_tree() {
    home();
    let image = Image::new("filesystem", &exfat(21));
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    assert_eq!(first["stale"]["stale"], 0, "a new scan has nothing stale");
    let session = engine.session().unwrap();
    scan_fully(&session);
    let scan_id = first["scan"]["id"].as_str().unwrap().to_string();
    let sets = stored_sets(&session);
    assert!(!sets.is_empty());
    // A result from the old state of the drive, that the drive no longer has.
    {
        let connection = rusqlite::Connection::open(session.workspace.database()).unwrap();
        connection
            .execute("INSERT INTO sets (source_cluster, source_offset, name, in_use, checksum_ok, attributes, is_dir, first_cluster, length, valid_length, no_fat_chain) VALUES (3, 480, 'ghost.txt', 1, 1, 0, 0, 5, 100, 100, 0)", [])
            .unwrap();
    }
    let ghost = stored_sets(&session)
        .into_iter()
        .find(|(_, cluster, offset)| (*cluster, *offset) == (3, 480))
        .unwrap();
    let nodes_before = {
        let database = MapDb::open(&session.workspace.database()).unwrap();
        database.roots(0, 1000).unwrap().rows.len()
    };
    drop(session);

    change_two_markers(&image);
    // Without acknowledging the changed markers nothing opens.
    let waiting = call(&engine, "open_source", json!({"path": image.0}));
    assert_eq!(waiting["needsChoice"], true);
    let disk = waiting["disk"]["id"].as_str().unwrap().to_string();

    let opened = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "acceptChangedMarkers": true}),
    );
    assert_eq!(opened["scan"]["id"], scan_id.as_str());
    assert_eq!(stale_state(&opened), 1, "{opened}");
    assert_eq!(opened["scan"]["staleRegions"], 1);
    assert_eq!(
        opened["stale"]["regions"][0]["markers"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let session = engine.session().unwrap();
    assert!(session
        .stale_regions()
        .iter()
        .any(|region| region.is_stale()));

    // The row the old state left is flagged, as is every entry in the stale region.
    let mut row = serde_json::to_value(
        MapDb::open(&session.workspace.database())
            .unwrap()
            .entry(ghost.0)
            .unwrap()
            .unwrap(),
    )
    .unwrap();
    session.flag_stale(&mut row);
    assert_eq!(row["stale"], true, "{row}");

    // An agent may read the regions and may not scan them again.
    let listed = engine
        .call(
            "list_stale_regions",
            &json!({"scanId": scan_id}),
            AUTHOR_CLAUDE,
        )
        .unwrap();
    assert_eq!(listed["stale"]["stale"], 1);
    let refused = engine.call(
        "rescan_changed_regions",
        &json!({"diskId": disk, "scanId": scan_id}),
        AUTHOR_CLAUDE,
    );
    assert!(refused.unwrap_err().to_string().contains("Only the person"));
    assert_eq!(
        stored_sets(&session).len(),
        sets.len() + 1,
        "opening scanned nothing again"
    );

    // The person's action reads the region again.
    let done = call(
        &engine,
        "rescan_changed_regions",
        json!({"diskId": disk, "scanId": scan_id}),
    );
    assert_eq!(done["rescan"]["regionsRemaining"], 0, "{done}");
    assert_eq!(done["rescan"]["resolved"], true);
    assert_eq!(done["markersRerecorded"], true);
    assert_eq!(stale_state(&done), 0);
    let after = stored_sets(&session);
    assert_eq!(
        after, sets,
        "the ghost is gone and every real entry keeps its id"
    );
    let nodes_after = {
        let database = MapDb::open(&session.workspace.database()).unwrap();
        database.roots(0, 1000).unwrap().rows.len()
    };
    assert_eq!(nodes_after, nodes_before);
    let mut row =
        json!({"kind": "file", "sourceCluster": 3, "sourceOffset": 480, "firstCluster": 5});
    session.flag_stale(&mut row);
    assert!(
        row.get("stale").is_none(),
        "rescanned entries are not flagged"
    );
    drop(session);

    // The next open of the same drive is exact, and has nothing stale.
    let exact = call(&engine, "open_source", json!({"path": image.0}));
    assert!(exact.get("needsChoice").is_none());
    assert_eq!(exact["scan"]["id"], scan_id.as_str());
    assert_eq!(stale_state(&exact), 0);
    let catalog = Catalog::for_user().unwrap();
    let history = catalog
        .marker_history(exact["source"]["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(history.len(), 1);
}

#[test]
fn an_interrupted_filesystem_rescan_stays_stale_and_resumes() {
    home();
    let image = Image::new("filesystem-interrupted", &exfat(22));
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    let scan_id = first["scan"]["id"].as_str().unwrap().to_string();
    scan_fully(&engine.session().unwrap());
    change_two_markers(&image);
    let disk = call(&engine, "open_source", json!({"path": image.0}))["disk"]["id"]
        .as_str()
        .unwrap()
        .to_string();
    let opened = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "acceptChangedMarkers": true}),
    );
    assert_eq!(stale_state(&opened), 1);
    let session = engine.session().unwrap();
    let catalog = Catalog::for_user().unwrap();
    let regions = catalog.stale_regions(&scan_id).unwrap();

    let mut chunks = 0;
    let stopped = rescan_changed(
        &session.volume,
        &session.workspace,
        &catalog,
        &scan_id,
        0,
        &regions,
        ScanOptions { chunk_clusters: 2 },
        &AtomicBool::new(false),
        |_, _| {
            chunks += 1;
            chunks < 3
        },
    )
    .unwrap();
    assert_eq!(chunks, 3);
    assert!(stopped.stopped.is_some());
    assert!(!stopped.resolved);
    assert_eq!(stopped.regions_remaining, 1);
    let kept = catalog.stale_regions(&scan_id).unwrap().remove(0);
    assert!(
        kept.done_to > kept.start && kept.done_to < kept.end,
        "{kept:?}"
    );
    assert_eq!(
        catalog
            .marker_history(opened["source"]["id"].as_str().unwrap())
            .unwrap()
            .len(),
        0
    );

    // Resuming reads from where it stopped, and finishes.
    let again = catalog.stale_regions(&scan_id).unwrap();
    let mut first_read = None;
    let done = rescan_changed(
        &session.volume,
        &session.workspace,
        &catalog,
        &scan_id,
        0,
        &again,
        ScanOptions { chunk_clusters: 2 },
        &AtomicBool::new(false),
        |_, read_to| {
            first_read.get_or_insert(read_to);
            true
        },
    )
    .unwrap();
    assert_eq!(done.regions_remaining, 0);
    assert!(done.resolved);
    assert!(first_read.unwrap() > kept.done_to);
    assert!(
        done.clusters_rescanned < 2 * MIB / 512,
        "only what was left was read"
    );
}

#[test]
fn the_command_line_lists_the_stale_regions_and_scans_them_again() {
    home();
    let image = raw_image("cli", 31);
    let mut store = ScanStore::for_user().unwrap();
    let catalog = Catalog::for_user().unwrap();
    let disk = image.disk();
    let (raw, scan) = scan_raw(&mut store, &catalog, &disk);
    drop(disk);
    image.write(0, &noise(4096, 32));
    image.write(4 * MIB, &noise(512, 33));
    store.resume(&image.disk(), &raw, true, true).unwrap();

    let run = |arguments: &[&str]| -> Value {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
            .args(arguments)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{arguments:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let text = String::from_utf8_lossy(&output.stdout);
        serde_json::from_str(text.lines().last().unwrap()).unwrap_or_else(|_| json!(text))
    };
    let path = image.0.to_str().unwrap();
    let listed = run(&["scan", "stale-regions", "--scan", &scan]);
    assert_eq!(listed["stale"]["stale"], 1);
    assert_eq!(listed["stale"]["regions"][0]["start"], 0);
    assert_eq!(listed["markerHistory"].as_array().unwrap().len(), 0);

    let done = run(&["scan", "rescan-changed", path, "--scan", &scan]);
    assert_eq!(done["rescan"]["regionsRescanned"], 1, "{done}");
    assert_eq!(done["rescan"]["regionsRemaining"], 0);
    assert_eq!(done["markersRerecorded"], true);
    let after = run(&["scan", "stale-regions", "--scan", &scan]);
    assert_eq!(after["stale"]["stale"], 0);
    assert_eq!(after["markerHistory"].as_array().unwrap().len(), 1);
    assert_eq!(state_of(&store, &image.disk()), Verification::Exact);

    // Asking to scan a scan with nothing stale reads nothing and leaves the markers.
    let nothing = run(&["scan", "rescan-changed", path, "--scan", &scan]);
    assert_eq!(nothing["rescan"]["regionsRescanned"], 0);
    assert_eq!(nothing["markersRerecorded"], false);
    let bad = std::process::Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
        .args(["scan", "rescan-changed", path])
        .output()
        .unwrap();
    assert!(!bad.status.success());
    assert!(String::from_utf8_lossy(&bad.stderr).contains("--scan"));
}
