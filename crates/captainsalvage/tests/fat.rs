use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::export::CopyRequest;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

mod fat_support;
use fat_support::*;

fn workspace(label: &str) -> Workspace {
    let path =
        std::env::temp_dir().join(format!("captainsalvage-fat-{}-{label}", std::process::id()));
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
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
}

#[test]
fn fat12_fat16_and_fat32_sessions_scan_browse_and_read_fragmented_files() {
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let fixture = Fixture::new(kind);
        let expected = fixture.contents.clone();
        let session = Session::with_source(
            Path::new("/test/fat-image"),
            Arc::new(MemorySource::new(fixture.image)),
            workspace(&format!("session-{kind}")),
        )
        .unwrap();
        assert_eq!(session.volume.kind, kind);
        assert_eq!(
            session.overview()["filesystem"]["variant"],
            kind.to_string()
        );
        let explanation = session.explain_region(3, 4, 1).unwrap();
        assert_eq!(explanation["directoryEntriesRecognized"], 1);
        assert_eq!(explanation["directoryNames"][0], "CHILD.TXT");
        assert!(explanation.get("exfatNames").is_none());
        assert_eq!(session.overview()["bootCheck"], Value::Null);
        assert_eq!(session.volume.stats.allocated, 5);
        scan(&session);
        let engine = Engine::new();
        engine.set_session(session.clone());
        let rows = call(&engine, "search", json!({"query":"Captain log.txt"}));
        let id = rows["rows"][0]["id"]
            .as_i64()
            .expect("long-name file found");
        let read = session.read_entry_range(id, 0, 700).unwrap();
        assert_eq!(read.data, expected);
        assert_eq!(
            session.read_entry_range(id, 500, 30).unwrap().data,
            expected[500..530]
        );
        let child = call(&engine, "search", json!({"query":"CHILD.TXT"}));
        assert_eq!(child["rows"][0]["name"], "CHILD.TXT");
        let orphan = call(&engine, "search", json!({"query":"ORPHAN.TXT"}));
        assert_eq!(orphan["rows"][0]["name"], "ORPHAN.TXT");
        let deleted = call(&engine, "search", json!({"query":"_OST.TXT"}));
        let deleted_id = deleted["rows"][0]["id"]
            .as_i64()
            .expect("deleted entry found beyond end marker");
        assert!(!deleted["rows"][0]["inUse"].as_bool().unwrap());
        assert_eq!(
            session.read_entry_range(deleted_id, 0, 520).unwrap().data,
            vec![b'D'; 512]
        );
        call(
            &engine,
            "set_parameters",
            json!({"chainFallbackContiguous": true}),
        );
        assert_eq!(
            session.read_entry_range(deleted_id, 0, 520).unwrap().data,
            [vec![b'D'; 512], vec![b'E'; 8]].concat()
        );
        assert!(session.verify_unchanged().unwrap().unchanged);
        let destination = session.workspace.dir().join("recovered");
        session
            .copy_start(CopyRequest {
                destination: destination.to_string_lossy().into_owned(),
                entry_ids: vec![id, deleted_id],
                ..CopyRequest::default()
            })
            .unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            let status = session.copy_status().unwrap();
            if status.state == "finished" {
                assert_eq!(status.files_done, 2, "{status:?}");
                break;
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
        assert_eq!(
            std::fs::read(destination.join("Volume root/Captain log.txt")).unwrap(),
            expected
        );
        assert_eq!(
            std::fs::read(destination.join("Volume root/_OST.TXT")).unwrap(),
            [vec![b'D'; 512], vec![b'E'; 8]].concat()
        );
    }
}

#[test]
fn fat_scans_resume_after_restart_without_duplicating_fixed_roots() {
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let fixture = Fixture::new(kind);
        let source = Arc::new(MemorySource::new(fixture.image));
        let workspace = workspace(&format!("resume-{kind}"));
        let path = workspace.dir().to_path_buf();
        let session =
            Session::with_source(Path::new("/test/resume"), source.clone(), workspace).unwrap();
        let stop = AtomicBool::new(false);
        let paused = run_scan(
            &session.volume,
            &session.workspace,
            ScanOptions { chunk_clusters: 32 },
            &stop,
            |_| {
                stop.store(true, Ordering::Relaxed);
            },
        )
        .unwrap();
        assert_eq!(paused.status, "paused");
        drop(session);
        let resumed = Session::with_source(
            Path::new("/test/resume"),
            source,
            Workspace::at(&path).unwrap(),
        )
        .unwrap();
        scan(&resumed);
        let roots = resumed.with_database(|db| db.roots(0, 20)).unwrap();
        let root = roots
            .rows
            .iter()
            .find(|row| row.node_kind.as_deref() == Some("root"))
            .unwrap();
        assert_eq!(
            root.head,
            Some(if kind == FilesystemKind::Fat32 { 2 } else { 0 })
        );
        let rows = resumed
            .with_database(|db| db.children(root.head.unwrap(), 0, 100, true, 0.0))
            .unwrap();
        assert_eq!(
            rows.rows
                .iter()
                .filter(|row| row.name == "Captain log.txt")
                .count(),
            1
        );
        let first =
            captainsalvage::scan::read_progress(&resumed.database.lock().unwrap(), &resumed.volume)
                .unwrap();
        scan(&resumed);
        let second =
            captainsalvage::scan::read_progress(&resumed.database.lock().unwrap(), &resumed.volume)
                .unwrap();
        assert_eq!(first.sets_found, second.sets_found);
        assert!(first.sets_found >= 5);
        assert!(resumed.verify_unchanged().unwrap().unchanged);
    }
}

#[test]
fn fat_changed_structures_are_explained_and_saved_baselines_remain_usable() {
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let mut fixture = Fixture::new(kind);
        let workspace = workspace(&format!("baseline-{kind}"));
        let path = workspace.dir().to_path_buf();
        let session = Session::with_source(
            Path::new("/test/baseline"),
            Arc::new(MemorySource::new(fixture.image.clone())),
            workspace,
        )
        .unwrap();
        scan(&session);
        drop(session);
        fixture.fat(11, 12);
        fixture.fat(12, u32::MAX);
        let short = fixture.root_sector * 512 + 64;
        fixture.image[short..short + 11].copy_from_slice(b"CHANGED TXT");
        let expected = fixture.contents.clone();
        let reopened = Session::with_source(
            Path::new("/test/baseline"),
            Arc::new(MemorySource::new(fixture.image)),
            Workspace::at(&path).unwrap(),
        )
        .unwrap();
        assert_eq!(reopened.volume.fat[11], 0);
        assert!(!reopened.verify_unchanged().unwrap().unchanged);
        let changes = reopened.structure_changes().unwrap().unwrap();
        assert_eq!(changes.written_clusters, vec![11, 12]);
        assert!(changes.fat_entries >= 2);
        assert!(changes
            .root_entries
            .iter()
            .any(|entry| entry.name == "CHANGED.TXT"));
        let engine = Engine::new();
        engine.set_session(reopened.clone());
        let rows = call(&engine, "search", json!({"query":"Captain log.txt"}));
        let id = rows["rows"][0]["id"].as_i64().unwrap();
        assert_eq!(
            reopened.read_entry_range(id, 0, 700).unwrap().data,
            expected
        );
    }
}

#[test]
fn fat_disk_discovery_marks_msdos_variants_and_ntfs_as_openable() {
    for format in [
        "MS-DOS FAT12",
        "MS-DOS FAT16",
        "MS-DOS FAT32",
        "MS-DOS (FAT32)",
        "FAT16",
        "FAT32",
    ] {
        let info = format!("Device Identifier: disk99s1\nDevice Node: /dev/disk99s1\nFile System Personality: {format}\nVolume Name: Test\n");
        assert!(
            captainsalvage::startup::parse_volume(&info)
                .unwrap()
                .can_open,
            "{format}"
        );
    }
    let info =
        "Device Identifier: disk99s1\nDevice Node: /dev/disk99s1\nFile System Personality: NTFS\n";
    assert!(
        captainsalvage::startup::parse_volume(info)
            .unwrap()
            .can_open
    );
}

#[test]
fn fat32_uses_the_active_table_and_detects_changes_in_other_copies() {
    use captainsalvage::volume::Volume;
    let mut fixture = Fixture::new(FilesystemKind::Fat32);
    fixture.image[40..42].copy_from_slice(&0x81u16.to_le_bytes());
    let first_link = fixture.fat_sector * 512 + 5 * 4;
    fixture.image[first_link..first_link + 4].copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
    let source = Arc::new(MemorySource::new(fixture.image.clone()));
    let volume = Volume::open(source).unwrap();
    assert_eq!(
        volume.geometry.fat_offset,
        ((fixture.fat_sector + fixture.fat_sectors) * 512) as u64
    );
    assert_eq!(volume.fat[5], 9);
    assert_eq!(
        volume.read_stream(5, 700, 700, false, 700).unwrap(),
        fixture.contents
    );
    let workspace = workspace("mirror");
    let path = workspace.dir().to_path_buf();
    let session = Session::with_source(
        Path::new("/test/mirror"),
        Arc::new(MemorySource::new(fixture.image.clone())),
        workspace,
    )
    .unwrap();
    drop(session);
    fixture.image[first_link..first_link + 4].copy_from_slice(&0u32.to_le_bytes());
    let session = Session::with_source(
        Path::new("/test/mirror"),
        Arc::new(MemorySource::new(fixture.image)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    assert!(!session.verify_unchanged().unwrap().unchanged);
    let changes = session.structure_changes().unwrap().unwrap();
    assert_eq!(changes.fat_entries, 1);
    assert!(changes.written_clusters.is_empty());
}

#[test]
fn fat_saved_snapshots_reject_truncated_regions() {
    use captainsalvage::volume::Volume;
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let source = Arc::new(MemorySource::new(Fixture::new(kind).image));
        let copy = Volume::read_structures(source.as_ref()).unwrap();
        for field in ["main", "backup", "fat", "root"] {
            if field == "backup" && copy.backup.is_empty() {
                continue;
            }
            let mut malformed = copy.clone();
            let bytes = match field {
                "main" => &mut malformed.main,
                "backup" => &mut malformed.backup,
                "fat" => &mut malformed.fat,
                _ => &mut malformed.root,
            };
            bytes.pop();
            assert!(
                Volume::from_structures(source.clone(), malformed).is_err(),
                "{kind}: {field}"
            );
        }
    }
}

#[test]
fn fat_damaged_links_stop_without_reading_invalid_clusters() {
    use captainsalvage::volume::Volume;
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        for value in [5, 1, 0, 0x0FFF_FFF7, 0x0FFF_FFF0] {
            let mut fixture = Fixture::new(kind);
            fixture.fat(5, value);
            let volume = Volume::open(Arc::new(MemorySource::new(fixture.image))).unwrap();
            assert_eq!(volume.clusters_for(5, 700, false).clusters, vec![5]);
            assert_eq!(
                volume.read_stream(5, 700, 700, false, 700).unwrap(),
                fixture.contents[..512]
            );
            assert!(volume
                .clusters_for(u32::MAX, 700, false)
                .clusters
                .is_empty());
            assert!(volume.clusters_for(0, 700, false).clusters.is_empty());
            assert!(volume.clusters_for(5, 0, false).clusters.is_empty());
        }
    }
}

#[test]
fn branded_cli_scans_fat_images_with_an_isolated_home() {
    use std::process::Command;
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let fixture = Fixture::new(kind);
        let workspace = workspace(&format!("cli-{kind}"));
        let image = workspace.dir().join("source.img");
        std::fs::write(&image, &fixture.image).unwrap();
        let execute = |arguments: &[&str]| {
            let output = Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
                .arg("scan")
                .arg(&image)
                .args(arguments)
                .env("HOME", workspace.dir())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "{kind}: {}\n{}",
                String::from_utf8_lossy(&output.stderr),
                String::from_utf8_lossy(&output.stdout)
            );
            String::from_utf8(output.stdout).unwrap()
        };
        let scanned = execute(&[]);
        assert!(scanned.contains("stats:"));
        let searched = execute(&["--call", "search", r#"{"query":"Captain log.txt"}"#]);
        assert!(searched.contains("Captain log.txt"));
        let overview = execute(&["--call", "volume_overview"]);
        assert!(overview.contains(&kind.to_string()));
        assert_eq!(std::fs::read(&image).unwrap(), fixture.image);
    }
}

#[test]
fn one_engine_switches_between_exfat_and_fat_sessions() {
    use fs_exfat::fixtures::{
        bitmap_entry, entry_set, upcase_entry, Builder, BITMAP_CLUSTER, CLUSTER, ROOT_CLUSTER,
    };
    let mut builder = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 5] {
        builder.allocate(cluster);
    }
    builder.fat(ROOT_CLUSTER, fs_exfat::primitives::FAT_END);
    let boot = fs_exfat::primitives::BootSector::parse(&builder.image).unwrap();
    let bitmap_length = u64::from(boot.cluster_count).div_ceil(8);
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(BITMAP_CLUSTER, bitmap_length),
            upcase_entry(4, &[0; 100]),
            entry_set("exfat-only.txt", false, 5, 11, true, true),
        ],
    );
    builder.put(5, 0, b"exfat bytes");
    let exfat = Session::with_source(
        Path::new("/test/exfat"),
        Arc::new(MemorySource::new(builder.image)),
        workspace("mixed-exfat"),
    )
    .unwrap();
    let fixture = Fixture::new(FilesystemKind::Fat16);
    let expected = fixture.contents.clone();
    let fat = Session::with_source(
        Path::new("/test/fat"),
        Arc::new(MemorySource::new(fixture.image)),
        workspace("mixed-fat"),
    )
    .unwrap();
    scan(&exfat);
    scan(&fat);
    let engine = Engine::new();
    for session in [&exfat, &fat, &exfat, &fat] {
        engine.set_session((*session).clone());
        let is_exfat = session.volume.kind == FilesystemKind::Exfat;
        let name = if is_exfat {
            "exfat-only.txt"
        } else {
            "Captain log.txt"
        };
        let absent = if is_exfat {
            "Captain log.txt"
        } else {
            "exfat-only.txt"
        };
        let result = call(&engine, "search", json!({"query":name}));
        let id = result["rows"][0]["id"].as_i64().unwrap();
        let expected = if is_exfat {
            b"exfat bytes".as_slice()
        } else {
            expected.as_slice()
        };
        assert_eq!(
            engine
                .session()
                .unwrap()
                .read_entry_range(id, 0, CLUSTER as u64)
                .unwrap()
                .data,
            expected
        );
        assert_eq!(
            call(&engine, "search", json!({"query":absent}))["rows"]
                .as_array()
                .unwrap()
                .len(),
            0
        );
    }
}
