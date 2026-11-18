//! The single door, `open_source`: one way to open a device, a partition, a disk image or one
//! extent of a disk, with an interpretation applied and nothing scanned. Synthetic images only;
//! the sources never change, and everything is kept in a throwaway home folder.

use captainsalvage::catalog::Catalog;
use captainsalvage::engine::{Engine, AUTHOR_CLAUDE, AUTHOR_USER};
use captainsalvage::map::MapDb;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use fs_exfat::fixtures::{self, Builder};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::OnceLock;

fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-opening-home-{}",
            std::process::id()
        ));
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

/// An exFAT volume of its own: its own serial number and its own bytes after the volume.
fn exfat(seed: u32) -> Vec<u8> {
    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * 1024 * 1024, 0);
    builder.write_boot();
    let sector = fixtures::SECTOR;
    builder.image[100..104].copy_from_slice(&(0x6000_0000 + seed).to_le_bytes());
    fixtures::seal_boot(&mut builder.image[..sector * 12], sector);
    let main = builder.image[..sector * 12].to_vec();
    builder.image[sector * 12..sector * 24].copy_from_slice(&main);
    builder.image[sector * 12 + 112] = 59;
    let tail = noise(builder.image.len() - 1024 * 1024, u64::from(seed));
    builder.image[1024 * 1024..].copy_from_slice(&tail);
    builder.image
}

/// A FAT16 volume with a fixed number of sectors.
fn fat16(serial: u32) -> Vec<u8> {
    let sectors = 5000usize;
    let mut image = vec![0; sectors * 512];
    image[11..13].copy_from_slice(&512u16.to_le_bytes());
    image[13] = 1;
    image[14..16].copy_from_slice(&1u16.to_le_bytes());
    image[16] = 2;
    image[17..19].copy_from_slice(&512u16.to_le_bytes());
    image[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
    image[22..24].copy_from_slice(&20u16.to_le_bytes());
    image[39..43].copy_from_slice(&serial.to_le_bytes());
    image[510..512].copy_from_slice(&[0x55, 0xAA]);
    for table in 0..2 {
        let base = (1 + table * 20) * 512;
        image[base..base + 4].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF]);
    }
    image
}

/// A disk with an MBR, `volume` as its one partition from 1 MiB, and noise round it.
fn disk_with(volume: &[u8], seed: u64) -> (Vec<u8>, u64) {
    let start = 1024 * 1024u64;
    let mut data = noise(start as usize + volume.len() + 64 * 1024, seed);
    data[..512].fill(0);
    let entry = &mut data[446..462];
    entry[4] = 0x07;
    entry[8..12].copy_from_slice(&((start / 512) as u32).to_le_bytes());
    entry[12..16].copy_from_slice(&((volume.len() / 512) as u32).to_le_bytes());
    data[510..512].copy_from_slice(&[0x55, 0xAA]);
    data[start as usize..start as usize + volume.len()].copy_from_slice(volume);
    (data, start)
}

struct ImageFile(PathBuf);

impl ImageFile {
    fn new(label: &str, data: &[u8]) -> Self {
        home();
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-opening-{}-{label}.img",
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

fn stored_nodes(session: &Session) -> usize {
    MapDb::open(&session.workspace.database())
        .unwrap()
        .roots(0, 1000)
        .unwrap()
        .rows
        .len()
}

#[test]
fn opening_a_partition_applies_the_detected_filesystem_and_starts_no_scan() {
    let image = ImageFile::new("partition", &exfat(1));
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    assert_eq!(first["scope"], "partition");
    assert_eq!(first["interpretation"]["format"], "exFAT");
    assert_eq!(first["created"], true);
    assert_eq!(
        first["scan"]["status"], "not-started",
        "opening starts nothing"
    );
    assert!(first["overview"].is_object());
    let detected = first["detected"]["confidence"].as_f64().unwrap();
    assert!(detected > 0.5, "{detected}");
    let choices = first["choices"].as_array().unwrap();
    let ids: Vec<&str> = choices
        .iter()
        .map(|choice| choice["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["exFAT", "FAT", "NTFS", "none"]);
    assert_eq!(choices[0]["detected"], true);
    assert_eq!(choices[0]["current"], true);
    assert_eq!(choices[1]["available"], false);
    assert_eq!(choices[2]["available"], false);

    let session = engine.session().unwrap();
    scan_fully(&session);
    let nodes = stored_nodes(&session);
    assert!(nodes > 0);
    drop(session);

    // Reopening finds the stored scan and its results at once, and runs nothing.
    let again = call(&engine, "open_source", json!({"path": image.0}));
    assert_eq!(again["created"], false);
    assert_eq!(again["scan"]["id"], first["scan"]["id"]);
    assert_eq!(again["source"]["id"], first["source"]["id"]);
    assert_eq!(stored_nodes(&engine.session().unwrap()), nodes);
    let status = call(&engine, "status", json!({}));
    assert_eq!(status["scanInThisApp"], false);
    assert_eq!(again["scans"].as_array().unwrap().len(), 1);
}

#[test]
fn switching_the_interpretation_keeps_the_old_scan_and_switching_back_scans_nothing() {
    // An exFAT volume at the start, and a FAT16 volume after it that a person saved as a hypothesis.
    let mut data = exfat(2);
    let fat_start = data.len() as u64;
    let fat = fat16(0xCA00_0016);
    data.extend(&fat);
    let image = ImageFile::new("switch", &data);
    let before = std::fs::read(&image.0).unwrap();
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    let disk = first["disk"]["id"].as_str().unwrap().to_string();
    assert_eq!(first["interpretation"]["format"], "exFAT");
    scan_fully(&engine.session().unwrap());
    let exfat_scan = first["scan"]["id"].as_str().unwrap().to_string();
    let exfat_nodes = stored_nodes(&engine.session().unwrap());

    // The FAT choice is not available until something says where a FAT volume is.
    assert_eq!(first["choices"][1]["available"], false);
    let refused = engine.call(
        "open_source",
        &json!({"diskId": disk, "format": "FAT"}),
        AUTHOR_USER,
    );
    assert!(refused
        .unwrap_err()
        .to_string()
        .contains("No FAT boot evidence"));

    call(
        &engine,
        "save_hypothesis",
        json!({"diskId": disk, "format": "FAT16", "start": fat_start, "length": fat.len()}),
    );
    let switched = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "format": "FAT"}),
    );
    assert_eq!(switched["interpretation"]["format"], "FAT16");
    assert_eq!(switched["created"], true, "a new scan, not the old one");
    assert_ne!(switched["scan"]["id"].as_str().unwrap(), exfat_scan);
    assert_eq!(
        switched["scans"].as_array().unwrap().len(),
        2,
        "the exFAT scan stays"
    );
    assert_eq!(switched["scan"]["status"], "not-started");

    // Back to exFAT: the old scan, with its results, and nothing scanned.
    let back = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "format": "exFAT"}),
    );
    assert_eq!(back["created"], false);
    assert_eq!(back["scan"]["id"], exfat_scan.as_str());
    assert_eq!(stored_nodes(&engine.session().unwrap()), exfat_nodes);
    assert_eq!(back["choices"][1]["stored"], true);
    assert_eq!(back["choices"][0]["stored"], true);

    // Raw bytes is a reading too, and the exFAT scan survives it.
    let raw = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "format": "none"}),
    );
    assert!(raw["interpretation"].is_null());
    assert!(raw["overview"].is_null());
    assert_eq!(raw["choices"][3]["current"], true);
    assert!(raw["scans"]
        .as_array()
        .unwrap()
        .iter()
        .any(|scan| scan["id"] == exfat_scan.as_str()));
    let again = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "format": "exFAT"}),
    );
    assert_eq!(again["scan"]["id"], exfat_scan.as_str());
    assert_eq!(
        std::fs::read(&image.0).unwrap(),
        before,
        "the source is never written"
    );

    // An agent suggests; only the person switches.
    let agent = engine.call(
        "open_source",
        &json!({"diskId": disk, "format": "none"}),
        AUTHOR_CLAUDE,
    );
    assert!(agent.unwrap_err().to_string().contains("Only the person"));
}

#[test]
fn a_rescan_is_a_new_scan_and_keeps_the_old_one() {
    let image = ImageFile::new("rescan", &exfat(3));
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    scan_fully(&engine.session().unwrap());
    let disk = first["disk"]["id"].as_str().unwrap();
    let again = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "rescan": true}),
    );
    assert_eq!(again["created"], true);
    assert_ne!(again["scan"]["id"], first["scan"]["id"]);
    assert_eq!(again["scans"].as_array().unwrap().len(), 2);
    // Opening plainly now finds the latest of the last-used interpretation, and scans nothing.
    let plain = call(&engine, "open_source", json!({"diskId": disk}));
    assert_eq!(plain["created"], false);
    assert_eq!(plain["scan"]["id"], again["scan"]["id"]);
}

#[test]
fn a_whole_disk_opens_as_raw_bytes_and_each_extent_opens_as_a_source_of_its_own() {
    let volume = exfat(4);
    let (data, start) = disk_with(&volume, 40);
    let image = ImageFile::new("whole", &data);
    let engine = Engine::default();
    let whole = call(&engine, "open_source", json!({"path": image.0}));
    assert_eq!(whole["scope"], "disk");
    assert!(whole["interpretation"].is_null());
    assert!(whole["overview"].is_null());
    assert_eq!(whole["disk"]["partitions"].as_array().unwrap().len(), 1);
    assert!(whole["choices"].as_array().unwrap().is_empty());
    let disk = whole["disk"]["id"].as_str().unwrap();

    let extent = json!({"start": start, "length": volume.len()});
    let opened = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "extent": extent}),
    );
    assert_eq!(opened["scope"], "partition");
    assert_eq!(opened["interpretation"]["format"], "exFAT");
    assert_eq!(
        opened["interpretation"]["start"], 0,
        "relative to the extent"
    );
    assert_eq!(opened["created"], true);
    assert_eq!(opened["parents"][0]["parent"]["id"], whole["source"]["id"]);
    assert_ne!(opened["source"]["id"], whole["source"]["id"]);
    assert_eq!(opened["extents"].as_array().unwrap().len(), 1);
    let catalog = Catalog::for_user().unwrap();
    let linked = catalog
        .extents_of(whole["source"]["id"].as_str().unwrap())
        .unwrap();
    assert_eq!(linked.len(), 1);
    assert_eq!(
        (linked[0].start, linked[0].length),
        (start, volume.len() as u64)
    );
    assert_eq!(linked[0].scans.len(), 1);

    // The same extent opened again from the disk is the same scan.
    let again = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "extent": extent}),
    );
    assert_eq!(again["created"], false);
    assert_eq!(again["scan"]["id"], opened["scan"]["id"]);
}

#[test]
fn a_partition_opened_alone_and_its_extent_seen_from_the_disk_share_one_interpretation() {
    let volume = exfat(5);
    let (data, start) = disk_with(&volume, 50);
    let disk_image = ImageFile::new("link-disk", &data);
    let partition_image = ImageFile::new("link-partition", &volume);
    let engine = Engine::default();

    // Alone first: scanned there.
    let alone = call(&engine, "open_source", json!({"path": partition_image.0}));
    assert_eq!(alone["scope"], "partition");
    scan_fully(&engine.session().unwrap());
    let nodes = stored_nodes(&engine.session().unwrap());
    assert!(nodes > 0);

    // Then from the whole disk: the same source, scan and results, and nothing scanned.
    let whole = call(&engine, "open_source", json!({"path": disk_image.0}));
    let disk = whole["disk"]["id"].as_str().unwrap();
    let from_disk = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "extent": {"start": start, "length": volume.len()}}),
    );
    assert_eq!(from_disk["source"]["id"], alone["source"]["id"]);
    assert_eq!(from_disk["scan"]["id"], alone["scan"]["id"]);
    assert_eq!(from_disk["created"], false);
    assert_eq!(stored_nodes(&engine.session().unwrap()), nodes);
    assert_eq!(from_disk["parents"][0]["start"], start);

    // And the partition alone says which disk it was seen in.
    let alone_again = call(&engine, "open_source", json!({"path": partition_image.0}));
    assert_eq!(alone_again["scan"]["id"], alone["scan"]["id"]);
    assert_eq!(alone_again["parents"].as_array().unwrap().len(), 1);
    assert_eq!(
        alone_again["parents"][0]["parent"]["id"],
        whole["source"]["id"]
    );
    assert_eq!(
        Catalog::for_user()
            .unwrap()
            .scans(alone["source"]["id"].as_str().unwrap())
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn an_extent_scanned_from_the_disk_is_found_by_the_partition_opened_alone() {
    let volume = exfat(6);
    let (data, start) = disk_with(&volume, 60);
    let disk_image = ImageFile::new("link2-disk", &data);
    let partition_image = ImageFile::new("link2-partition", &volume);
    let engine = Engine::default();
    let whole = call(&engine, "open_source", json!({"path": disk_image.0}));
    let disk = whole["disk"]["id"].as_str().unwrap();
    let from_disk = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "extent": {"start": start, "length": volume.len()}}),
    );
    scan_fully(&engine.session().unwrap());
    let nodes = stored_nodes(&engine.session().unwrap());

    let alone = call(&engine, "open_source", json!({"path": partition_image.0}));
    assert_eq!(alone["source"]["id"], from_disk["source"]["id"]);
    assert_eq!(alone["scan"]["id"], from_disk["scan"]["id"]);
    assert_eq!(alone["created"], false);
    assert_eq!(stored_nodes(&engine.session().unwrap()), nodes);
}

/// Overwrites the blocks of these markers, as a mount rewriting metadata would, and nothing else.
fn changed_at(mut data: Vec<u8>, fingerprint: &Value, markers: &[usize]) -> Vec<u8> {
    let samples = fingerprint["samples"].as_array().unwrap();
    for index in markers {
        let at = samples[*index]["offset"].as_u64().unwrap() as usize;
        for byte in &mut data[at..at + 4096] {
            *byte ^= 0x5A;
        }
    }
    data
}

#[test]
fn a_probable_match_is_listed_and_never_applied_without_the_persons_say() {
    let original = exfat(7);
    let image = ImageFile::new("probable", &original);
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    scan_fully(&engine.session().unwrap());
    let scan = first["scan"]["id"].as_str().unwrap().to_string();
    let source = first["source"]["id"].as_str().unwrap().to_string();

    // The same drive after a mount wrote near the start: only a probable match.
    let changed = ImageFile::new(
        "probable-changed",
        &changed_at(original.clone(), &first["fingerprint"], &[3, 4, 5]),
    );
    let known = Catalog::for_user().unwrap().sources().unwrap().len();
    let choice = call(&engine, "open_source", json!({"path": changed.0}));
    assert_eq!(choice["needsChoice"], true, "{choice}");
    assert!(choice["overview"].is_null());
    let matches = choice["matches"].as_array().unwrap();
    assert_eq!(matches.len(), 1);
    assert_eq!(matches[0]["verification"], "probable");
    assert_eq!(matches[0]["id"], source.as_str());
    assert!(matches[0]["comparison"]["changed"].as_u64().unwrap() >= 1);
    assert_eq!(matches[0]["scans"][0]["id"], scan.as_str());
    assert_eq!(
        Catalog::for_user().unwrap().sources().unwrap().len(),
        known,
        "nothing was made by asking"
    );

    // Opening its scan without acknowledging the changed markers is refused.
    let disk = choice["disk"]["id"].as_str().unwrap();
    let refused = engine.call(
        "open_source",
        &json!({"diskId": disk, "scanId": scan}),
        AUTHOR_USER,
    );
    assert!(refused.unwrap_err().to_string().contains("probable match"));

    // Acknowledged, it opens the stored scan: same source, same scan, nothing new.
    let accepted = call(
        &engine,
        "open_source",
        json!({"diskId": disk, "scanId": scan, "acceptChangedMarkers": true}),
    );
    assert_eq!(accepted["scan"]["id"], scan.as_str());
    assert_eq!(accepted["created"], false);
    assert_eq!(accepted["source"]["id"], source.as_str());

    // Or the person opens it as a new source, which leaves the stored one as it is.
    let other = ImageFile::new(
        "probable-fresh",
        &changed_at(original, &first["fingerprint"], &[3, 4, 5]),
    );
    let fresh = call(
        &engine,
        "open_source",
        json!({"path": other.0, "fresh": true}),
    );
    assert_eq!(fresh["needsChoice"], Value::Null);
    assert_ne!(fresh["source"]["id"], source.as_str());
}

#[test]
fn weak_matches_are_hidden_until_all_are_shown_and_are_never_offered() {
    let original = exfat(8);
    let image = ImageFile::new("weak", &original);
    let engine = Engine::default();
    let first = call(&engine, "open_source", json!({"path": image.0}));
    let total = first["fingerprint"]["samples"].as_array().unwrap().len();
    // Six markers changed: 8 of 14 still match, which is under 60% but enough to show.
    let last: Vec<usize> = (total - 6..total).collect();
    let other = ImageFile::new(
        "weak-changed",
        &changed_at(original, &first["fingerprint"], &last),
    );
    let hidden = call(&engine, "open_source", json!({"path": other.0}));
    assert_ne!(hidden["needsChoice"], true, "a weak match is never offered");
    assert_eq!(hidden["hidden"], 1);
    assert!(hidden["matches"].as_array().unwrap().is_empty());
    assert_ne!(
        hidden["source"]["id"], first["source"]["id"],
        "nothing stored was applied"
    );
    let shown = call(
        &engine,
        "open_source",
        json!({"path": other.0, "showAll": true}),
    );
    let listed = shown["matches"].as_array().unwrap();
    assert!(
        listed
            .iter()
            .any(|found| found["verification"] == "weak" && found["id"] == first["source"]["id"]),
        "{listed:?}"
    );
}

#[test]
fn the_command_line_opens_through_the_same_door() {
    let volume = exfat(9);
    let (data, start) = disk_with(&volume, 90);
    let whole = ImageFile::new("cli-whole", &data);
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
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let path = whole.0.to_str().unwrap();
    let opened = run(&["source", "open", path]);
    assert_eq!(opened["scope"], "disk");
    assert!(opened["interpretation"].is_null());
    let extent = run(&[
        "source",
        "open",
        path,
        "--extent-start",
        &start.to_string(),
        "--extent-length",
        &volume.len().to_string(),
    ]);
    assert_eq!(extent["scope"], "partition");
    assert_eq!(extent["interpretation"]["format"], "exFAT");
    assert_eq!(extent["scan"]["status"], "not-started");
    // The CLI makes its own engine each time, and the second open finds what the first stored.
    let again = run(&[
        "source",
        "open",
        path,
        "--extent-start",
        &start.to_string(),
        "--extent-length",
        &volume.len().to_string(),
    ]);
    assert_eq!(again["created"], false);
    assert_eq!(again["scan"]["id"], extent["scan"]["id"]);
}
