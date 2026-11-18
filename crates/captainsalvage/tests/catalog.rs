//! Scans in the catalog, through the engine, the session and the command line. Synthetic images
//! only; the source never changes, and everything is kept in a throwaway home folder.

use captainsalvage::catalog::{Accept, Catalog, ScanKind};
use captainsalvage::disk::RawDisk;
use captainsalvage::engine::{Engine, AUTHOR_CLAUDE, AUTHOR_USER};
use captainsalvage::map::MapDb;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::volume::Volume;
use captainsalvage::workspace::Workspace;
use fs_exfat::fixtures::{self, Builder};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, OnceLock};

/// Points HOME at a folder of this test run, once, before anything opens a store.
fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-catalog-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("HOME", &path);
        path
    })
}

fn app_folder() -> PathBuf {
    home().join("Library/Application Support/Captain Salvage")
}

/// A volume that no other test has: its own serial number and its own bytes after the volume, so
/// it is its own drive.
fn exfat(seed: u32) -> Vec<u8> {
    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * 1024 * 1024, 0);
    builder.write_boot();
    let sector = fixtures::SECTOR;
    builder.image[100..104].copy_from_slice(&(0x5000_0000 + seed).to_le_bytes());
    fixtures::seal_boot(&mut builder.image[..sector * 12], sector);
    let main = builder.image[..sector * 12].to_vec();
    builder.image[sector * 12..sector * 24].copy_from_slice(&main);
    builder.image[sector * 12 + 112] = 59;
    let mut state = u64::from(seed).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    for byte in &mut builder.image[1024 * 1024..] {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        *byte = (state >> 24) as u8;
    }
    builder.image
}

struct ImageFile(PathBuf);

impl ImageFile {
    fn new(label: &str, data: &[u8]) -> Self {
        home();
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-catalog-{}-{label}.img",
            std::process::id()
        ));
        std::fs::write(&path, data).unwrap();
        Self(path)
    }
}

impl Drop for ImageFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
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

fn stored_nodes(session: &Session) -> usize {
    let database = MapDb::open(&session.workspace.database()).unwrap();
    database.roots(0, 1000).unwrap().rows.len()
}

fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

fn open_raw(engine: &Engine, file: &ImageFile) -> String {
    call(engine, "open_raw_disk", json!({"path": file.0}))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn reopening_a_scanned_volume_finds_its_stored_results_and_starts_nothing() {
    let image = ImageFile::new("reopen", &exfat(1));
    let before = std::fs::read(&image.0).unwrap();
    let first = Session::open(&image.0).unwrap();
    let binding = first.scan.clone().expect("the scan is in the catalog");
    assert!(binding.created);
    assert!(first
        .workspace
        .dir()
        .starts_with(app_folder().join("sources")));
    scan_fully(&first);
    let nodes = stored_nodes(&first);
    assert!(nodes > 0);
    drop(first);

    let second = Session::open(&image.0).unwrap();
    let again = second.scan.clone().unwrap();
    assert!(!again.created, "nothing was stored under it? it was");
    assert_eq!(again.scan_id, binding.scan_id);
    assert_eq!(again.source_id, binding.source_id);
    assert_eq!(
        stored_nodes(&second),
        nodes,
        "the stored results are there at once"
    );

    // What is stored is listed, with its size and how far it got.
    let catalog = Catalog::for_user().unwrap();
    let scans = catalog.scans(&binding.source_id).unwrap();
    assert_eq!(scans.len(), 1);
    assert_eq!(scans[0].format, "exFAT");
    assert_eq!(scans[0].kind, ScanKind::Filesystem);
    assert!(scans[0].stored_bytes > 0);
    assert!(scans[0].complete, "{:?}", scans[0].status);
    assert_eq!(
        std::fs::read(&image.0).unwrap(),
        before,
        "the source is never written"
    );
}

#[test]
fn a_volume_a_mount_touched_is_still_found_by_its_serial_number() {
    let image = ImageFile::new("mounted", &exfat(2));
    let first = Session::open(&image.0).unwrap();
    let id = first.scan.clone().unwrap().scan_id;
    drop(first);
    // A mount writes near the start of the volume, and the volume is no longer an exact match.
    let mut touched = std::fs::read(&image.0).unwrap();
    let fingerprint =
        captainsalvage::raw_scan::Fingerprint::read(&RawDisk::open(&image.0).unwrap());
    for sample in fingerprint.samples.iter().skip(1).take(2) {
        let at = sample.offset as usize;
        touched[at + 600..at + 700].fill(0x5A);
    }
    std::fs::write(&image.0, &touched).unwrap();
    let second = Session::open(&image.0).unwrap();
    assert_eq!(second.scan.as_ref().unwrap().scan_id, id);
}

#[test]
fn an_old_workspace_is_found_and_not_scanned_again() {
    let image = ImageFile::new("old-layout", &exfat(3));
    let volume = Volume::open(Arc::new(MemorySource::new(exfat(3)))).unwrap();
    let old_dir = app_folder().join(volume.workspace_name());
    // What an earlier version left: a folder named from the serial number, with a finished scan.
    let old = Workspace::at(&old_dir).unwrap();
    run_scan(
        &volume,
        &old,
        ScanOptions { chunk_clusters: 16 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let nodes = MapDb::open(&old.database())
        .unwrap()
        .roots(0, 1000)
        .unwrap()
        .rows
        .len();
    let entropy = std::fs::read(old.entropy()).unwrap();
    assert!(nodes > 0 && !entropy.is_empty());

    let session = Session::open(&image.0).unwrap();
    let binding = session.scan.clone().unwrap();
    assert!(
        !binding.created,
        "the earlier scan is the scan: it is not made again"
    );
    assert_eq!(session.workspace.dir(), old_dir);
    assert_eq!(stored_nodes(&session), nodes);
    assert_eq!(std::fs::read(session.workspace.entropy()).unwrap(), entropy);
    let catalog = Catalog::for_user().unwrap();
    let scans = catalog.scans(&binding.source_id).unwrap();
    assert_eq!(scans.len(), 1);
    assert!(scans[0].migrated);
    assert!(scans[0].complete);
    let source = catalog.source(&binding.source_id).unwrap();
    assert!(
        source.markers > 0,
        "the source got its markers when the device was seen"
    );
    drop(session);
    // The next open does not need the serial number.
    let again = Session::open(&image.0).unwrap();
    assert_eq!(again.scan.as_ref().unwrap().scan_id, binding.scan_id);
}

#[test]
fn an_interpretation_of_a_raw_disk_is_a_scan_that_opens_again_without_scanning() {
    use captainsalvage::disk::DiskRegion;
    let volume = exfat(4);
    let start = 1024 * 1024usize;
    let mut data = vec![0u8; start + volume.len() + 4096];
    let noise: Vec<u8> = (0..data.len() as u32)
        .map(|index| (index.wrapping_mul(2654435761) >> 13) as u8)
        .collect();
    data.copy_from_slice(&noise);
    data[start..start + volume.len()].copy_from_slice(&volume);
    let image = ImageFile::new("region", &data);
    let region = DiskRegion {
        start: start as u64,
        length: volume.len() as u64,
        format: "exFAT".into(),
        label: "an exFAT volume".into(),
        boot_offset: start as u64,
    };
    let raw = RawDisk::open(&image.0).unwrap();
    let first = Session::open_region(&raw, region.clone()).unwrap();
    let binding = first.scan.clone().unwrap();
    assert!(binding.created);
    scan_fully(&first);
    let nodes = stored_nodes(&first);
    drop(first);

    let second = Session::open_region(&RawDisk::open(&image.0).unwrap(), region).unwrap();
    assert!(!second.scan.as_ref().unwrap().created);
    assert_eq!(second.scan.as_ref().unwrap().scan_id, binding.scan_id);
    assert_eq!(stored_nodes(&second), nodes);
    drop(second);

    // Opened by the scan's own ID, as a list offers it.
    let by_id = Session::open_scan(
        &RawDisk::open(&image.0).unwrap(),
        &binding.scan_id,
        Accept::Exact,
    )
    .unwrap();
    assert_eq!(by_id.scan.as_ref().unwrap().scan_id, binding.scan_id);
    assert_eq!(stored_nodes(&by_id), nodes);
}

#[test]
fn the_tools_list_sources_and_scans_and_only_the_person_changes_them() {
    let image = ImageFile::new("tools", &exfat(5));
    let session = Session::open(&image.0).unwrap();
    let binding = session.scan.clone().unwrap();
    scan_fully(&session);
    drop(session);

    let engine = Engine::new();
    let disk = open_raw(&engine, &image);
    let sources = call(&engine, "list_sources", json!({}));
    assert!(sources["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|source| source["id"] == binding.source_id));
    let identified = call(&engine, "identify_source", json!({"diskId": disk}));
    assert_eq!(identified["matches"][0]["verification"], "exact");
    assert_eq!(identified["matches"][0]["id"], binding.source_id);

    let listed = call(&engine, "list_scans", json!({"diskId": disk}));
    let scans = listed["matches"][0]["scans"].as_array().unwrap();
    assert_eq!(scans.len(), 1);
    assert_eq!(scans[0]["id"], binding.scan_id);
    assert_eq!(scans[0]["format"], "exFAT");
    assert!(scans[0]["storedBytes"].as_u64().unwrap() > 0);
    assert_eq!(scans[0]["complete"], true);

    // Switching the format makes a second scan and keeps the first.
    let length = std::fs::metadata(&image.0).unwrap().len();
    let switched = call(
        &engine,
        "switch_interpretation",
        json!({"diskId": disk, "format": "FAT32", "start": 0, "length": length}),
    );
    assert_eq!(switched["created"], true);
    assert_eq!(switched["scan"]["format"], "FAT32");
    assert_eq!(switched["source"]["id"], binding.source_id);
    let again = call(
        &engine,
        "switch_interpretation",
        json!({"diskId": disk, "format": "FAT32", "start": 0, "length": length}),
    );
    assert_eq!(again["created"], false);
    assert_eq!(again["scan"]["id"], switched["scan"]["id"]);
    let both = call(
        &engine,
        "list_scans",
        json!({"sourceId": binding.source_id}),
    );
    assert_eq!(both["scans"].as_array().unwrap().len(), 2);
    let formats: Vec<&str> = both["scans"]
        .as_array()
        .unwrap()
        .iter()
        .map(|scan| scan["format"].as_str().unwrap())
        .collect();
    assert!(formats.contains(&"exFAT") && formats.contains(&"FAT32"));

    // Opening the stored scan loads its results and scans nothing.
    let overview = call(
        &engine,
        "open_stored_scan",
        json!({"diskId": disk, "scanId": binding.scan_id}),
    );
    assert_eq!(overview["scan"]["scanId"], binding.scan_id);
    assert_eq!(overview["scan"]["created"], false);

    // An agent reads, and cannot change what is stored.
    for (tool, args) in [
        (
            "switch_interpretation",
            json!({"diskId": disk, "format": "NTFS", "start": 0, "length": length}),
        ),
        (
            "label_scan",
            json!({"scanId": binding.scan_id, "label": "mine"}),
        ),
        (
            "label_source",
            json!({"sourceId": binding.source_id, "label": "mine"}),
        ),
        ("migrate_catalog", json!({})),
    ] {
        let refused = engine.call(tool, &args, AUTHOR_CLAUDE).unwrap_err();
        assert!(
            format!("{refused:#}").contains("Only the person"),
            "{tool}: {refused:#}"
        );
    }
    for (tool, args) in [
        ("list_sources", json!({})),
        ("identify_source", json!({"diskId": disk})),
        ("list_scans", json!({"diskId": disk})),
    ] {
        engine
            .call(tool, &args, AUTHOR_CLAUDE)
            .unwrap_or_else(|error| panic!("{tool}: {error:#}"));
    }
    let named = call(
        &engine,
        "label_scan",
        json!({"scanId": binding.scan_id, "label": "first pass"}),
    );
    assert_eq!(named["scan"]["label"], "first pass");
    let migrated = call(&engine, "migrate_catalog", json!({}));
    assert_eq!(migrated["workspaces"], 0);
}

#[test]
fn the_command_line_lists_and_switches_the_same_scans() {
    let image = ImageFile::new("cli", &exfat(6));
    let run = |arguments: &[&str]| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
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
    let session = Session::open(&image.0).unwrap();
    let source = session.scan.clone().unwrap().source_id;
    scan_fully(&session);
    drop(session);

    let identified = run(&["source", "identify", path]);
    assert_eq!(identified["matches"][0]["verification"], "exact");
    let scans = run(&["source", "scans", path]);
    assert_eq!(scans["matches"][0]["scans"].as_array().unwrap().len(), 1);
    let length = std::fs::metadata(&image.0).unwrap().len().to_string();
    let switched = run(&[
        "source", "switch", path, "--format", "NTFS", "--start", "0", "--length", &length,
    ]);
    assert_eq!(switched["created"], true);
    let listed = run(&["source", "scans", "--source", &source]);
    assert_eq!(listed["scans"].as_array().unwrap().len(), 2);
    assert!(run(&["source", "list"])["sources"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["id"] == source));
    let scan = listed["scans"][0]["id"].as_str().unwrap();
    let named = run(&["source", "label", "--scan", scan, "--label", "kept"]);
    assert_eq!(named["scan"]["label"], "kept");
}
