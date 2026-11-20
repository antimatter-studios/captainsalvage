//! FAT12, FAT16 and FAT32 through the generic layers adapter (the default) and through the old FAT adapter (one flag away):
//! what each stores for the same image, and exactly what differs.
use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::filesystem::fat_generic::FatBackend;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::map::MapDb;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::volume::Volume;
use captainsalvage::workspace::Workspace;
use serde_json::json;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

mod fat_support;
use fat_support::*;

const KINDS: [FilesystemKind; 3] = [
    FilesystemKind::Fat12,
    FilesystemKind::Fat16,
    FilesystemKind::Fat32,
];

fn workspace(label: &str) -> Workspace {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-fatbackends-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    Workspace::at(&path).unwrap()
}

/// What the map stores for each entry, and what it says of it after the map is built.
#[derive(Debug, Clone, PartialEq)]
struct Stored {
    at: (u32, u32),
    name: String,
    in_use: bool,
    checksum_ok: bool,
    attributes: i64,
    is_dir: bool,
    first_cluster: u32,
    length: i64,
    valid_length: i64,
    no_fat_chain: bool,
    created: Option<String>,
    modified: Option<String>,
    accessed: Option<String>,
    weak: Option<String>,
    clusters_found: i64,
    verdict: String,
    confidence: f64,
    reason: String,
}

fn stored(image: &[u8], backend: FatBackend, label: &str) -> (Vec<Stored>, Volume) {
    let volume = Volume::open_with(Arc::new(MemorySource::new(image.to_vec())), backend).unwrap();
    assert_eq!(volume.fat_adapter(), backend);
    let workspace = workspace(label);
    run_scan(
        &volume,
        &workspace,
        ScanOptions {
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    MapDb::open(&workspace.database())
        .unwrap()
        .resolve(&volume, &workspace)
        .unwrap();
    let connection = rusqlite::Connection::open(workspace.database()).unwrap();
    let mut statement = connection
        .prepare(
            "SELECT source_cluster, source_offset, name, in_use, checksum_ok, attributes, is_dir, first_cluster, length, \
             valid_length, no_fat_chain, created, modified, accessed, (SELECT mark FROM fat_weak w WHERE w.source_cluster = sets.source_cluster AND w.source_offset = sets.source_offset), COALESCE(clusters_found, 0), COALESCE(verdict, ''), COALESCE(confidence, 0), \
             COALESCE(reason, '') FROM sets ORDER BY source_cluster, source_offset",
        )
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok(Stored {
                at: (row.get(0)?, row.get(1)?),
                name: row.get(2)?,
                in_use: row.get(3)?,
                checksum_ok: row.get(4)?,
                attributes: row.get(5)?,
                is_dir: row.get(6)?,
                first_cluster: row.get(7)?,
                length: row.get(8)?,
                valid_length: row.get(9)?,
                no_fat_chain: row.get(10)?,
                created: row.get(11)?,
                modified: row.get(12)?,
                accessed: row.get(13)?,
                weak: row.get(14)?,
                clusters_found: row.get(15)?,
                verdict: row.get(16)?,
                confidence: row.get(17)?,
                reason: row.get(18)?,
            })
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    (rows, volume)
}

/// The fixture, with a dated entry and a second orphaned folder cluster of two entries.
fn fixture(kind: FilesystemKind) -> Fixture {
    let mut fixture = Fixture::new(kind);
    let mut orphans = short_entry(b"TWO     TXT", 0x20, 18, 5);
    orphans.extend(short_entry(b"THREE   TXT", 0x20, 19, 5));
    fixture.put_cluster(16, &orphans);
    fixture.put_cluster(18, b"two..");
    fixture.put_cluster(19, b"three");
    // A date and time on the long-named file's short entry: 2021-06-15 13:45:20, created, modified and accessed.
    let date = ((2021 - 1980) << 9 | 6 << 5 | 15) as u16;
    let time = (13 << 11 | 45 << 5 | 10) as u16;
    let at = fixture
        .image
        .windows(11)
        .position(|window| window == b"CAPTAI~1TXT")
        .expect("the long-named file's short entry");
    fixture.image[at + 14..at + 16].copy_from_slice(&time.to_le_bytes());
    fixture.image[at + 16..at + 18].copy_from_slice(&date.to_le_bytes());
    fixture.image[at + 18..at + 20].copy_from_slice(&date.to_le_bytes());
    fixture.image[at + 22..at + 24].copy_from_slice(&time.to_le_bytes());
    fixture.image[at + 24..at + 26].copy_from_slice(&date.to_le_bytes());
    fixture
}

#[test]
fn what_changes_with_the_generic_adapter_is_the_state_and_the_marks_of_entries_no_folder_names() {
    for kind in KINDS {
        let fixture = fixture(kind);
        let (before, legacy) = stored(&fixture.image, FatBackend::Legacy, &format!("old-{kind}"));
        let (after, generic) = stored(&fixture.image, FatBackend::Generic, &format!("new-{kind}"));
        assert_eq!(
            before.len(),
            after.len(),
            "{kind}: the same entries are found"
        );
        assert_eq!(before.len(), 8, "{kind}");
        let mut changed: Vec<String> = Vec::new();
        for (old, new) in before.iter().zip(&after) {
            assert_eq!(old.at, new.at, "{kind}");
            if old == new {
                continue;
            }
            // Say what differs. Everything else of the entry is the same: the name, size, clusters, dates and attributes.
            let mut same = new.clone();
            same.in_use = old.in_use;
            same.weak = old.weak.clone();
            same.clusters_found = old.clusters_found;
            same.verdict = old.verdict.clone();
            same.confidence = old.confidence;
            same.reason = old.reason.clone();
            assert_eq!(
                old, &same,
                "{kind}: {} differs in more than its state and marks",
                old.name
            );
            changed.push(old.name.clone());
        }
        // The dates are the same, which the fixture sets so that the comparison covers them.
        let dated = after
            .iter()
            .find(|row| row.name == "Captain log.txt")
            .unwrap();
        assert!(
            dated.created.is_some() && dated.modified.is_some() && dated.accessed.is_some(),
            "{kind}"
        );
        assert_eq!(
            dated.modified,
            before
                .iter()
                .find(|row| row.name == "Captain log.txt")
                .unwrap()
                .modified,
            "{kind}"
        );
        assert_eq!(
            changed,
            [
                "_OST.TXT",
                "child by.tes",
                "ORPHAN.TXT",
                "TWO.TXT",
                "THREE.TXT"
            ],
            "{kind}"
        );
        let row =
            |rows: &[Stored], name: &str| rows.iter().find(|row| row.name == name).unwrap().clone();
        // The deleted file the folder names: its chain was cleared, so the old adapter found one cluster of it and the driver's
        // walk is the whole claim, as one run from its first cluster, marked a guess in its reason (its first cluster is known, the
        // rest is where the driver put it). Its entry, name, state and dates are the same.
        let (old, new) = (row(&before, "_OST.TXT"), row(&after, "_OST.TXT"));
        assert_eq!((old.clusters_found, new.clusters_found), (1, 2), "{kind}");
        assert_eq!(
            (old.verdict.as_str(), new.verdict.as_str()),
            ("partial", "likely-intact"),
            "{kind}"
        );
        assert_eq!((old.confidence, new.confidence), (0.5, 0.75), "{kind}");
        assert!(!old.reason.contains("a guess"), "{kind}: {}", old.reason);
        assert!(
            new.reason.contains("the last 1 clusters are a guess"),
            "{kind}: {}",
            new.reason
        );
        assert_eq!(
            (old.in_use, new.in_use, &old.weak, &new.weak),
            (false, false, &None, &None),
            "{kind}"
        );
        // The entries of a cluster nothing points to: deleted for the driver, which reads the FAT's state of their cluster,
        // and weak, which the old adapter had no way to say.
        for name in ["ORPHAN.TXT", "TWO.TXT", "THREE.TXT"] {
            let (old, new) = (row(&before, name), row(&after, name));
            assert!(old.in_use && !new.in_use, "{kind}: {name}");
            assert_eq!(old.weak, None, "{kind}: {name}");
            assert_eq!(
                new.weak.as_deref(),
                Some(if name == "ORPHAN.TXT" {
                    "lone_entry"
                } else {
                    "unchecked"
                }),
                "{kind}: {name}"
            );
            assert!(
                new.confidence <= old.confidence * 0.5 + 1e-9,
                "{kind}: {name}: {} against {}",
                new.confidence,
                old.confidence
            );
            assert!(
                new.reason.contains("found this entry by its content"),
                "{kind}: {name}: {}",
                new.reason
            );
        }
        // The file bytes the old adapter took for an entry are still found, now marked: they are in a cluster of a file, which
        // the FAT says is in use, so the entry is in use and weak.
        let (old, new) = (row(&before, "child by.tes"), row(&after, "child by.tes"));
        assert_eq!((old.in_use, new.in_use), (true, true), "{kind}");
        assert_eq!(new.weak.as_deref(), Some("lone_entry"), "{kind}");
        assert!(
            new.confidence <= old.confidence * 0.5 + 1e-9,
            "{kind}: {old:?} {new:?}"
        );
        // The generic adapter reads the structures of the FAT driver and tells of what it found wrong in them.
        assert!(legacy.damage.is_empty(), "{kind}");
        assert!(
            generic
                .damage
                .iter()
                .all(|text| text.starts_with("FAT structure damage found by the driver: ")),
            "{kind}"
        );
        assert!(
            !generic.damage.is_empty(),
            "{kind}: the fixture's first FAT entry carries no media descriptor"
        );
    }
}

#[test]
fn a_weak_candidate_is_marked_in_what_a_search_and_a_tool_say() {
    let fixture = fixture(FilesystemKind::Fat16);
    let session = Session::with_source(
        Path::new("/test/fat-image"),
        Arc::new(MemorySource::new(fixture.image.clone())),
        workspace("weak"),
    )
    .unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions {
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    session.resolve().unwrap();
    assert_eq!(
        session.overview()["fatAdapter"],
        "generic",
        "the generic adapter is the default"
    );
    let engine = Engine::new();
    engine.set_session(session.clone());
    let search = |query: &str| {
        engine
            .call("search", &json!({"query": query}), AUTHOR_USER)
            .unwrap()["rows"][0]
            .clone()
    };
    let orphan = search("ORPHAN.TXT");
    assert_eq!(orphan["weak"], "lone_entry", "{orphan}");
    assert_eq!(orphan["inUse"], false);
    assert_eq!(search("TWO.TXT")["weak"], "unchecked");
    // An entry a folder names carries no mark, so its answer is as it was.
    let named = search("CHILD.TXT");
    assert!(named.get("weak").is_none(), "{named}");
    assert_eq!(named["inUse"], true);
    // The structure the driver found damaged is in the overview, in words.
    assert!(session.overview()["structureDamage"][0]
        .as_str()
        .unwrap()
        .starts_with("FAT structure damage found by the driver: "));
}

#[test]
fn the_environment_chooses_the_old_adapter_for_one_release() {
    // The only test here that reads the environment, so nothing else in this process sees it.
    std::env::set_var("SALVAGE_FAT_ADAPTER", "legacy");
    assert_eq!(FatBackend::requested(), FatBackend::Legacy);
    std::env::set_var("SALVAGE_FAT_ADAPTER", "Generic");
    assert_eq!(FatBackend::requested(), FatBackend::Generic);
    std::env::remove_var("SALVAGE_FAT_ADAPTER");
    assert_eq!(FatBackend::requested(), FatBackend::Generic);
}

/// The volume the window test of weak candidates opens (tests/window/fat_weak_candidates_test.py): a FAT16 image with
/// entries no folder names. Run with:
/// SALVAGE_FAT_IMAGE=$PWD/tests/window/fat.img cargo test -p captainsalvage --test fat_backends write_the_fat
#[test]
fn write_the_fat_volume_for_window_tests() {
    let fixture = fixture(FilesystemKind::Fat16);
    let volume = Volume::open_with(
        Arc::new(MemorySource::new(fixture.image.clone())),
        FatBackend::Generic,
    )
    .unwrap();
    assert_eq!(volume.kind, FilesystemKind::Fat16);
    if let Some(destination) = std::env::var_os("SALVAGE_FAT_IMAGE") {
        std::fs::write(destination, &fixture.image).expect("exported window fixture");
    }
}
