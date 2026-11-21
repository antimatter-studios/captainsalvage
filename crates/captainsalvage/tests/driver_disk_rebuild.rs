//! A Disk Rebuild over a driver's volume (#290): a damaged SquashFS image whose directory table is lost leaves files with no
//! name and no folder; the person gathers them in a folder of their own, names them, and copies the rebuilt tree out.
use captainsalvage::disk_rebuild::Place;
use captainsalvage::export::CopyRequest;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn fixture() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/squashfs-basic.sqfs"))
        .unwrap()
}

fn word(image: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(image[at..at + 8].try_into().unwrap())
}

/// The fixture with the first header of its directory table zeroed: the files are still there, their names and folders are not.
fn without_its_directory_table() -> Vec<u8> {
    let mut image = fixture();
    let start = word(&image, 0x48) as usize;
    image[start..start + 2].fill(0);
    image
}

fn scanned(label: &str, image: Vec<u8>) -> Arc<Session> {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-driver-rebuild-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        Path::new("/test/squashfs-rebuild"),
        Arc::new(MemorySource::new(image)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 16 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    session
}

fn big() -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    (0..20000)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        })
        .collect()
}

fn wait_for_copy(session: &Session, files: u64) {
    wait_for_copy_with_failed(session, files, 0)
}

fn wait_for_copy_with_failed(session: &Session, files: u64, failed: u64) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(
                (status.files_done, status.files_failed, status.files_partial),
                (files, failed, 0),
                "{status:?}"
            );
            return;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(std::time::Instant::now() < deadline, "{status:?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn files_that_lost_their_names_and_folder_are_gathered_named_and_copied_out_at_their_rebuilt_paths()
{
    let session = scanned("lost-names", without_its_directory_table());
    // The scan found the files, with no name and no folder: each is a file of the map with a length and bytes.
    let rows = session
        .query_map("SELECT id, length FROM sets WHERE is_dir = 0 AND length > 0 ORDER BY length")
        .unwrap();
    let found: Vec<(i64, u64)> = rows["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| (row["id"].as_i64().unwrap(), row["length"].as_u64().unwrap()))
        .collect();
    // hello.txt (3), the link (9), note.md (23) and big.bin (20000).
    assert_eq!(
        found.iter().map(|(_, length)| *length).collect::<Vec<_>>(),
        [3, 9, 23, 20000]
    );
    let names = [
        (3u64, "hello.txt"),
        (9, "link"),
        (23, "note.md"),
        (20000, "big.bin"),
    ];
    // A file whose name the disk lost is given one by the person: an accepted entry with a name, which is what the map then reads.
    for (id, length) in &found {
        let name = names.iter().find(|(l, _)| l == length).unwrap().1;
        session
            .with_database(|database| {
                database.add_overlay(
                    captainsalvage::map::OVERLAY_ACCEPT_ENTRY,
                    Some(*id),
                    &serde_json::json!({ "name": name }),
                    "named by the person",
                    "user",
                )
            })
            .unwrap();
    }
    session.resolve().unwrap();
    let rebuild = session
        .with_database(|database| {
            let rebuild = database.disk_rebuild_create("rebuilt")?;
            // The first folder placed is the root of the rebuilt tree, and the files go inside it.
            let root = database.disk_rebuild_folder(rebuild, "recovered", Place::default())?;
            for (id, _) in &found {
                database.disk_rebuild_place_file(
                    rebuild,
                    *id,
                    Place {
                        item: Some(root),
                        head: None,
                    },
                )?;
            }
            Ok(rebuild)
        })
        .unwrap();
    let files = session
        .with_database(|database| database.disk_rebuild_files(rebuild))
        .unwrap();
    let mut shown: Vec<String> = files.iter().map(|file| file.row.name.clone()).collect();
    shown.sort();
    assert_eq!(
        shown,
        ["big.bin", "hello.txt", "link", "note.md"],
        "{files:?}"
    );

    let destination = session.workspace.dir().join("copy");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            disk_rebuild: Some(rebuild),
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    // The three files whose bytes the driver walks are copied. The link is read by its path through the driver's file layer,
    // and with the directory table lost that path leads nowhere: it fails, named, and is not written as zeros.
    wait_for_copy_with_failed(&session, 3, 1);
    let copied = |name: &str| {
        std::fs::read(destination.join(name)).unwrap_or_else(|error| panic!("{name}: {error}"))
    };
    assert_eq!(copied("hello.txt"), b"hi\n");
    assert!(!destination.join("link").exists());
    assert_eq!(copied("note.md"), b"# note\nsome words here\n");
    assert_eq!(copied("big.bin"), big());
}

/// The fixture's "sub" folder: its head in the map, and the files under it.
fn sub_head(session: &Session) -> u32 {
    let page = session
        .with_database(|database| {
            database.children(session.volume.geometry.root_cluster, 0, 100, true, 0.0)
        })
        .unwrap();
    page.rows
        .iter()
        .find(|row| row.name == "sub")
        .and_then(|row| row.head)
        .expect("the root holds sub")
}

#[test]
fn a_folder_of_an_intact_image_is_placed_as_a_piece_and_copied_out_with_what_it_holds() {
    let session = scanned("piece", fixture());
    let head = sub_head(&session);
    let rebuild = session
        .with_database(|database| {
            let rebuild = database.disk_rebuild_create("sub only")?;
            database.disk_rebuild_place(rebuild, head, Place::default(), false)?;
            Ok(rebuild)
        })
        .unwrap();
    let files = session
        .with_database(|database| database.disk_rebuild_files(rebuild))
        .unwrap();
    let mut shown: Vec<String> = files
        .iter()
        .map(|file| format!("{}/{}", file.folders.join("/"), file.row.name))
        .collect();
    shown.sort();
    assert_eq!(shown, ["/note.md", "deep/big.bin"]);
    let destination = session.workspace.dir().join("copy");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            disk_rebuild: Some(rebuild),
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, 2);
    assert_eq!(
        std::fs::read(destination.join("note.md")).unwrap(),
        b"# note\nsome words here\n"
    );
    assert_eq!(
        std::fs::read(destination.join("deep/big.bin")).unwrap(),
        big()
    );
}

#[test]
fn a_rescan_of_the_clusters_keeps_every_folders_number_so_a_disk_rebuild_still_holds() {
    let session = scanned("rescan", fixture());
    let head = sub_head(&session);
    let rebuild = session
        .with_database(|database| {
            let rebuild = database.disk_rebuild_create("sub only")?;
            database.disk_rebuild_place(rebuild, head, Place::default(), false)?;
            Ok(rebuild)
        })
        .unwrap();
    let numbers = |session: &Session| {
        session
            .query_map("SELECT source_cluster, source_offset, record_number, parent_record FROM records ORDER BY record_number")
            .unwrap()
    };
    let before = numbers(&session);
    // The scan's results for every cluster are stored again, as a rescan of the stale regions does.
    let last = session.volume.geometry.last_cluster_exclusive();
    let mut found = Vec::new();
    for cluster in 2..last {
        for entry in session.volume.sets_in_cluster(cluster, &[], 4096) {
            found.push((cluster, entry));
        }
    }
    captainsalvage::map::MapDb::open(&session.workspace.database())
        .unwrap()
        .replace_chunk(2, last, &found, &[])
        .unwrap();
    session.resolve().unwrap();
    assert_eq!(
        numbers(&session),
        before,
        "a record changed its number or lost its parent"
    );
    assert_eq!(sub_head(&session), head);
    let files = session
        .with_database(|database| database.disk_rebuild_files(rebuild))
        .unwrap();
    assert_eq!(files.len(), 2, "{files:?}");
}
