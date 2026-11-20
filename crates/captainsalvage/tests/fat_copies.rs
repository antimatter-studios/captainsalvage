//! FAT file extents and copies come from the driver's walk: what a deleted file's copy holds, and how it is marked.
use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::export::CopyRequest;
use captainsalvage::filesystem::generic::GenericVolume;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use serde_json::{json, Value};
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
        "captainsalvage-fatcopies-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    Workspace::at(&path).unwrap()
}

/// A scanned, mapped session, reading FAT entries through the old adapter when `legacy`. The adapter is chosen when the volume
/// is opened, from the environment, so opening takes turns.
fn session(fixture: &Fixture, label: &str, legacy: bool) -> Arc<Session> {
    static TURN: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let session = {
        let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        if legacy {
            std::env::set_var("SALVAGE_FAT_ADAPTER", "legacy");
        } else {
            std::env::remove_var("SALVAGE_FAT_ADAPTER");
        }
        let opened = Session::with_source(
            Path::new("/test/fat-image"),
            Arc::new(MemorySource::new(fixture.image.clone())),
            workspace(label),
        );
        std::env::remove_var("SALVAGE_FAT_ADAPTER");
        opened.unwrap()
    };
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
    session
}

fn find(engine: &Engine, name: &str) -> Value {
    let rows = engine
        .call("search", &json!({"query": name}), AUTHOR_USER)
        .unwrap();
    rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["name"] == name)
        .unwrap_or_else(|| panic!("{name} not found"))
        .clone()
}

/// Copies the entries, waits, and returns what the copy made and its manifest lines. A file with a chain that ends early is
/// partial, and is copied only when partial files are asked for.
fn copy(session: &Arc<Session>, ids: Vec<i64>, label: &str) -> (std::path::PathBuf, Vec<Value>) {
    let destination = session.workspace.dir().join(format!("recovered-{label}"));
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: ids.clone(),
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(status.files_done, ids.len() as u64, "{status:?}");
            break;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(std::time::Instant::now() < deadline, "copy timed out");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let manifest = std::fs::read_to_string(destination.join(captainsalvage::export::MANIFEST_NAME))
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    (destination, manifest)
}

#[test]
fn a_deleted_files_copy_is_the_whole_claim_by_default_and_says_it_is_a_guess() {
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let session = session(&fixture, &format!("new-{kind}"), false);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let whole = [vec![b'D'; 512], vec![b'E'; 8]].concat();
        let deleted = find(&engine, "_OST.TXT");
        // The FAT cleared its chain; the driver follows the claim as one run from its first cluster.
        assert_eq!(deleted["inUse"], false, "{kind}");
        assert_eq!(deleted["guessed"], true, "{kind}");
        assert_eq!(
            (
                deleted["clustersFound"].as_u64(),
                deleted["clustersNeeded"].as_u64()
            ),
            (Some(2), Some(2)),
            "{kind}"
        );
        assert!(
            deleted["reason"]
                .as_str()
                .unwrap()
                .contains("the last 1 clusters are a guess"),
            "{kind}: {deleted}"
        );
        let live = find(&engine, "Captain log.txt");
        assert!(
            live.get("guessed").is_none(),
            "{kind}: a file the FAT chains is not a guess"
        );
        let ids = vec![
            deleted["id"].as_i64().unwrap(),
            live["id"].as_i64().unwrap(),
        ];
        let (destination, manifest) = copy(&session, ids, &format!("new-{kind}"));
        assert_eq!(
            std::fs::read(destination.join("Volume root/_OST.TXT")).unwrap(),
            whole,
            "{kind}"
        );
        assert_eq!(
            std::fs::read(destination.join("Volume root/Captain log.txt")).unwrap(),
            fixture.contents,
            "{kind}"
        );
        // The manifest marks the one copied from a guess, and only that one; and it is not partial.
        let line = |name: &str| manifest.iter().find(|row| row["name"] == name).unwrap();
        assert_eq!(line("_OST.TXT")["extentsGuessed"], true, "{kind}");
        assert_eq!(line("_OST.TXT")["status"], "complete", "{kind}");
        assert!(
            line("Captain log.txt").get("extentsGuessed").is_none(),
            "{kind}"
        );
        let status = session.copy_status().unwrap();
        assert_eq!(
            (status.files_guessed, status.files_partial),
            (1, 0),
            "{kind}"
        );
    }
}

#[test]
fn the_old_adapter_still_copies_one_cluster_of_a_deleted_file_and_says_nothing_of_a_guess() {
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let session = session(&fixture, &format!("old-{kind}"), true);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let deleted = find(&engine, "_OST.TXT");
        assert!(deleted.get("guessed").is_none(), "{kind}");
        let (destination, manifest) = copy(
            &session,
            vec![deleted["id"].as_i64().unwrap()],
            &format!("old-{kind}"),
        );
        assert_eq!(
            std::fs::read(destination.join("Volume root/_OST.TXT")).unwrap(),
            vec![b'D'; 512],
            "{kind}: the chain was cleared, so one cluster"
        );
        assert!(manifest[0].get("extentsGuessed").is_none(), "{kind}");
        assert_eq!(session.copy_status().unwrap().files_guessed, 0, "{kind}");
    }
}

#[test]
fn what_the_engine_reads_of_every_entry_is_what_the_drivers_own_copy_makes() {
    use fs_core_layers::layers::{CandidateKind, CandidateState};
    struct Memory(Vec<u8>);
    impl fs_core_layers::BlockRead for Memory {
        fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
            let size = self.0.len() as u64;
            let start = offset as usize;
            let bytes = self.0.get(start..start + output.len()).ok_or(
                fs_core_layers::Error::OutOfBounds {
                    offset,
                    len: output.len() as u64,
                    size,
                },
            )?;
            output.copy_from_slice(bytes);
            Ok(())
        }
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let session = session(&fixture, &format!("drv-{kind}"), false);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let generic = GenericVolume::<fs_fat32::Volume<Memory>>::open_for_salvage(Memory(
            fixture.image.clone(),
        ))
        .unwrap();
        let mut compared = 0;
        for candidate in generic.candidates().unwrap() {
            if candidate.kind != CandidateKind::File || candidate.size.unwrap_or(0) == 0 {
                continue;
            }
            let name = candidate.name.clone().unwrap();
            let row = find(&engine, &name);
            let mut made = Vec::new();
            generic.copy(&candidate, &mut made).unwrap();
            let read = session
                .read_entry_range(row["id"].as_i64().unwrap(), 0, made.len() as u64 + 10)
                .unwrap();
            assert_eq!(read.data, made, "{kind}: {name}");
            assert_eq!(
                candidate.state == CandidateState::Deleted,
                row["inUse"] == false,
                "{kind}: {name}"
            );
            compared += 1;
        }
        assert_eq!(
            compared, 4,
            "{kind}: the long-named file, the child, the deleted entry and the lone entry that has bytes"
        );
    }
}

#[test]
fn the_file_tests_run_on_a_deleted_file_copied_from_a_guess_as_on_any_other() {
    for kind in KINDS {
        let fixture = Fixture::new(kind);
        let session = session(&fixture, &format!("tests-{kind}"), false);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let deleted = find(&engine, "_OST.TXT");
        let id = deleted["id"].as_i64().unwrap();
        // Its two clusters are found, so it is a candidate for a test: before, with one of two found, it was not.
        let started = engine
            .call("file_tests_start", &json!({"ids": [id]}), AUTHOR_USER)
            .unwrap();
        assert_eq!(started["total"], 1, "{kind}: {started}");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while session.file_test_status().unwrap().state != "finished" {
            assert!(std::time::Instant::now() < deadline, "the tests timed out");
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let tested = engine
            .call("file_test", &json!({"id": id}), AUTHOR_USER)
            .unwrap();
        assert!(!tested.is_null(), "{kind}");
        // The test read the whole claim: the hash is of its 520 bytes.
        let whole = [vec![b'D'; 512], vec![b'E'; 8]].concat();
        assert_eq!(
            tested["sha256"].as_str().unwrap_or_default(),
            captainsalvage_sha(&whole),
            "{kind}: {tested}"
        );
        // And its verdict still says its extents were guessed.
        assert!(find(&engine, "_OST.TXT")["reason"]
            .as_str()
            .unwrap()
            .contains("are a guess"));
    }
}

fn captainsalvage_sha(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The shared fixture with a deleted file written around a live one: `GONE.TXT` (1500 bytes) starts at cluster 21, and cluster 22,
/// which would be its next, holds the live file `LIVE.TXT`, so the rest of it is in clusters 23 and 24. The FAT cleared its chain.
fn written_around_a_live_file(kind: FilesystemKind) -> (Fixture, Vec<u8>) {
    let mut fixture = Fixture::new(kind);
    // The root's entries so far: the long-named file, ARCHIVE, the end marker and the deleted `_OST.TXT`.
    let used = long_entry("Captain log.txt", b"CAPTAI~1TXT", 5, 700).len() + 32 + 32 + 32;
    let mut entries = short_entry(b"\xE5ONE    TXT", 0x20, 21, 1500);
    entries.extend(short_entry(b"LIVE    TXT", 0x20, 22, 512));
    let at = fixture.root_sector * 512 + used;
    fixture.image[at..at + entries.len()].copy_from_slice(&entries);
    fixture.fat(22, u32::MAX);
    fixture.put_cluster(21, &[b'A'; 512]);
    fixture.put_cluster(22, &[b'L'; 512]);
    fixture.put_cluster(23, &[b'B'; 512]);
    fixture.put_cluster(24, &[b'C'; 476]);
    let whole = [vec![b'A'; 512], vec![b'B'; 512], vec![b'C'; 476]].concat();
    (fixture, whole)
}

#[test]
fn a_deleted_file_written_around_a_live_file_is_copied_byte_for_byte_by_skipping_the_live_one() {
    for kind in KINDS {
        let (fixture, whole) = written_around_a_live_file(kind);
        let session = session(&fixture, &format!("around-{kind}"), false);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let gone = find(&engine, "_ONE.TXT");
        let live = find(&engine, "LIVE.TXT");
        assert_eq!(
            (gone["inUse"].as_bool(), live["inUse"].as_bool()),
            (Some(false), Some(true)),
            "{kind}"
        );
        // Its three clusters are the next free ones: 21, 23 and 24, and the live file's 22 is left out.
        assert_eq!(gone["guessed"], true, "{kind}");
        assert_eq!(gone["clustersFound"], 3, "{kind}: {gone}");
        let (destination, manifest) = copy(
            &session,
            vec![gone["id"].as_i64().unwrap(), live["id"].as_i64().unwrap()],
            &format!("around-{kind}"),
        );
        assert_eq!(
            std::fs::read(destination.join("Volume root/_ONE.TXT")).unwrap(),
            whole,
            "{kind}: the deleted file, byte for byte"
        );
        assert_eq!(
            std::fs::read(destination.join("Volume root/LIVE.TXT")).unwrap(),
            vec![b'L'; 512],
            "{kind}: the live file is not touched"
        );
        let line = |name: &str| manifest.iter().find(|row| row["name"] == name).unwrap();
        assert_eq!(line("_ONE.TXT")["extentsGuessed"], true, "{kind}");
        assert!(line("LIVE.TXT").get("extentsGuessed").is_none(), "{kind}");
        // What the engine reads, and what the driver's own copy makes, are the same bytes.
        assert_eq!(
            session
                .read_entry_range(gone["id"].as_i64().unwrap(), 0, 1600)
                .unwrap()
                .data,
            whole,
            "{kind}"
        );

        // The one-run guess of the earlier pin is the old adapter with its fallback on: it runs into the live file.
        let old = self::session(&fixture, &format!("around-old-{kind}"), true);
        let old_engine = Engine::new();
        old_engine.set_session(old.clone());
        old_engine
            .call(
                "set_parameters",
                &json!({"chainFallbackContiguous": true}),
                AUTHOR_USER,
            )
            .unwrap();
        let old_gone = find(&old_engine, "_ONE.TXT");
        let contiguous = old
            .read_entry_range(old_gone["id"].as_i64().unwrap(), 0, 1600)
            .unwrap()
            .data;
        assert_ne!(contiguous, whole, "{kind}");
        assert!(
            contiguous.contains(&b'L'),
            "{kind}: a contiguous run takes the live file's bytes"
        );
    }
}
