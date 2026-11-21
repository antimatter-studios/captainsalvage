//! A filesystem read through its driver (the registry in `filesystem::drivers`), scanned, browsed, previewed and copied by
//! the engine: the entries are the driver's candidates, placed in folders by the driver's parent claims, read through the
//! driver. SquashFS is the one registered driver; nothing here is specific to it but the image.
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

fn fixture() -> Vec<u8> {
    std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/squashfs-basic.sqfs"))
        .unwrap()
}

/// The driver's pattern for `/sub/deep/big.bin`: an LCG, as its build script writes it.
fn big() -> Vec<u8> {
    let mut state: u32 = 0x1234_5678;
    (0..20000)
        .map(|_| {
            state = state.wrapping_mul(1_103_515_245).wrapping_add(12_345);
            (state >> 16) as u8
        })
        .collect()
}

fn scanned(label: &str, chunk_clusters: u32) -> Arc<Session> {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-driver-scan-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        Path::new("/test/squashfs-image"),
        Arc::new(MemorySource::new(fixture())),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    session
}

/// What a person browsing from the root sees: every folder and file under it, by path, with its kind, length and verdict.
fn browsed_ids(session: &Session) -> BTreeMap<String, (String, u64, String, Option<i64>)> {
    let root = session.volume.geometry.root_cluster;
    let mut found = BTreeMap::new();
    let mut queue = vec![(root, String::new())];
    while let Some((node, folder)) = queue.pop() {
        let page = session
            .with_database(|database| database.children(node, 0, 1000, true, 0.0))
            .unwrap();
        for row in page.rows {
            let path = if folder.is_empty() {
                row.name.clone()
            } else {
                format!("{folder}/{}", row.name)
            };
            if row.kind == "file" {
                found.insert(path, (row.kind, row.length, row.verdict, row.id));
            } else {
                found.insert(path.clone(), (row.kind, 0, row.verdict, None));
                queue.push((row.head.expect("a folder has a head"), path));
            }
        }
    }
    found
}

fn browsed(session: &Session) -> BTreeMap<String, (String, u64, String)> {
    browsed_ids(session)
        .into_iter()
        .map(|(path, (kind, length, verdict, _))| (path, (kind, length, verdict)))
        .collect()
}

#[test]
fn the_entries_are_the_drivers_candidates_in_the_folders_its_parent_claims_give() {
    let session = scanned("entries", 16);
    let found = browsed(&session);
    let expect = |path: &str, kind: &str, length: u64| {
        let entry = found
            .get(path)
            .unwrap_or_else(|| panic!("{path}: {found:?}"));
        assert_eq!((entry.0.as_str(), entry.1), (kind, length), "{path}");
    };
    expect("hello.txt", "file", 3);
    expect("empty.txt", "file", 0);
    expect("sub/note.md", "file", 23);
    expect("sub/deep/big.bin", "file", 20000);
}

#[test]
fn what_is_found_does_not_depend_on_how_the_scan_was_chunked() {
    let whole = browsed(&scanned("chunk-16", 16));
    assert_eq!(browsed(&scanned("chunk-1", 1)), whole);
    assert_eq!(browsed(&scanned("chunk-2", 2)), whole);
    assert!(whole.len() >= 7, "{whole:?}");
}

/// What the image holds, by path.
fn contents() -> BTreeMap<&'static str, Vec<u8>> {
    BTreeMap::from([
        ("hello.txt", b"hi\n".to_vec()),
        ("empty.txt", Vec::new()),
        ("link", b"hello.txt".to_vec()),
        ("sub/note.md", b"# note\nsome words here\n".to_vec()),
        ("sub/deep/big.bin", big()),
    ])
}

#[test]
fn a_file_is_previewed_through_the_driver_from_any_place_in_it() {
    let session = scanned("preview", 16);
    let found = browsed_ids(&session);
    for (path, wanted) in contents() {
        let id = found[path].3.unwrap_or_else(|| panic!("{path}"));
        let whole = session
            .read_entry_range(id, 0, wanted.len() as u64 + 100)
            .unwrap();
        assert_eq!(whole.total, wanted.len() as u64, "{path}");
        assert_eq!(whole.data, wanted, "{path}");
        assert!(whole.unreadable.is_empty(), "{path}");
        if wanted.len() > 10 {
            let part = session.read_entry_range(id, 7, 5).unwrap();
            assert_eq!(part.data, wanted[7..12], "{path}");
        }
    }
}

#[test]
fn a_copy_writes_the_bytes_the_driver_reads_and_says_it_is_complete() {
    use captainsalvage::export::CopyRequest;
    let session = scanned("copy", 16);
    let found = browsed_ids(&session);
    let ids: Vec<i64> = contents()
        .keys()
        .map(|path| found[*path].3.unwrap())
        .collect();
    let destination = session.workspace.dir().join("copy");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: ids,
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(
                (status.files_done, status.files_failed, status.files_partial),
                (5, 0, 0),
                "{status:?}"
            );
            break;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(std::time::Instant::now() < deadline, "{status:?}");
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let manifest =
        std::fs::read_to_string(destination.join("captain-salvage-manifest.jsonl")).unwrap();
    for line in manifest.lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["status"], "complete", "{row}");
        assert_eq!(row["unreadableRanges"], serde_json::json!([]), "{row}");
    }
    for (path, wanted) in contents() {
        let copied = std::fs::read(destination.join("Volume root").join(path)).unwrap();
        assert_eq!(copied, wanted, "{path}");
    }
}

#[test]
fn an_entrys_typed_ranges_are_its_drivers_walk() {
    use captainsalvage::ranges::RangeKind;
    let session = scanned("ranges", 16);
    let mut by_name = BTreeMap::new();
    for cluster in 2..session.volume.geometry.last_cluster_exclusive() {
        for entry in session.volume.sets_in_cluster(cluster, &[], 4096) {
            by_name.insert(entry.name.clone(), entry.ranges);
        }
    }
    // A file of several blocks and a tail is stored as it is here (it does not compress): readable, in a place on the image.
    let big = &by_name["big.bin"];
    assert_eq!(big.size(), 20000);
    assert_eq!(big.bytes_of(RangeKind::Readable), 20000);
    assert!(big.ranges()[0].provenance.is_some());
    assert_eq!(by_name["hello.txt"].size(), 3);
    assert!(by_name["empty.txt"].is_empty());
}
