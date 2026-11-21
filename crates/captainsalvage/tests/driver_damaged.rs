//! A damaged image of a driver's filesystem opens, says what is wrong, and lists what is still there, instead of failing (#290).
//! The images are the SquashFS fixture with one structure damaged each, the damage the driver's own conformance tests make.
use captainsalvage::filesystem::FilesystemKind;
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

/// Opens, scans and browses an image, as a person does, and returns what the volume says is wrong and the names it lists.
fn opened(label: &str, image: Vec<u8>) -> (Vec<String>, Vec<String>) {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-driver-damaged-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        Path::new("/test/squashfs-damaged"),
        Arc::new(MemorySource::new(image)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap_or_else(|error| panic!("{label}: a damaged image did not open: {error:#}"));
    assert_eq!(session.volume.kind, FilesystemKind::Squashfs, "{label}");
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 16 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap_or_else(|error| panic!("{label}: the scan failed: {error:#}"));
    let mut names = Vec::new();
    let mut queue = vec![session.volume.geometry.root_cluster];
    while let Some(node) = queue.pop() {
        let page = session
            .with_database(|database| database.children(node, 0, 1000, true, 0.0))
            .unwrap();
        for row in page.rows {
            if row.kind == "file" {
                names.push(row.name);
            } else if let Some(head) = row.head {
                queue.push(head);
            }
        }
    }
    names.sort();
    (session.volume.damage.clone(), names)
}

#[test]
fn an_image_that_claims_more_bytes_than_it_has_opens_says_so_and_still_lists_its_files() {
    let mut image = fixture();
    let claimed = image.len() as u64 + 4096;
    image[0x28..0x30].copy_from_slice(&claimed.to_le_bytes());
    let (damage, names) = opened("bytes-used", image);
    assert!(!damage.is_empty(), "the volume says nothing is wrong");
    assert_eq!(
        names,
        ["big.bin", "empty.txt", "hello.txt", "link", "note.md"],
        "{damage:?}"
    );
}

#[test]
fn an_image_whose_inode_table_header_is_zeroed_opens_and_reports_without_failing() {
    let mut image = fixture();
    let inode_table = word(&image, 0x40) as usize;
    image[inode_table..inode_table + 2].fill(0);
    let (damage, names) = opened("inode-table", image);
    assert!(
        !damage.is_empty(),
        "the volume says nothing is wrong: {names:?}"
    );
    // What can still be read is listed; what cannot is not invented.
    assert!(names.len() <= 5, "{names:?}");
}

#[test]
fn an_image_whose_directory_table_header_is_zeroed_opens_and_reports_without_failing() {
    let mut image = fixture();
    let directory_table = word(&image, 0x48) as usize;
    image[directory_table..directory_table + 2].fill(0);
    let (damage, names) = opened("directory-table", image);
    assert!(
        !damage.is_empty(),
        "the volume says nothing is wrong: {names:?}"
    );
    assert!(names.len() <= 5, "{names:?}");
}
