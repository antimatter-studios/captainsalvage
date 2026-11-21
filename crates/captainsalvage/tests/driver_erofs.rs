//! EROFS read through its driver's layers, the third driver of the registry in `filesystem::drivers`: the engine recognises
//! an image by its superblock (at byte 1024), opens it from its saved copy, scans and browses it, and says what it is. The
//! images are made by the driver's own builder; the files of `mkfs.erofs` images are the oracle's (`erofs_oracle.rs`).
use captainsalvage::filesystem::{drivers, FilesystemDetails, FilesystemKind, Support};
use captainsalvage::source::MemorySource;
use captainsalvage::volume::Volume;
use fs_erofs::mkfs::{build_image, Node, NodeMeta};
use std::collections::BTreeMap;
use std::sync::Arc;

fn file(data: &[u8]) -> Node<'static> {
    Node::File {
        mode: 0o100644,
        data: data.to_vec(),
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

fn folder(entries: Vec<(&str, Node<'static>)>) -> Node<'static> {
    Node::Dir {
        mode: 0o040755,
        entries: entries
            .into_iter()
            .map(|(name, node)| (name.to_string(), node))
            .collect::<BTreeMap<_, _>>(),
        meta: NodeMeta::default(),
        xattrs: Vec::new(),
    }
}

fn image(block_bits: u8) -> Vec<u8> {
    let root = folder(vec![
        ("hello.txt", file(b"hello")),
        ("big.bin", file(&vec![7u8; 20_000])),
        ("docs", folder(vec![("guide.txt", file(b"a guide"))])),
        (
            "link-to-file",
            Node::Symlink {
                mode: 0o120777,
                target: "hello.txt".to_string(),
                meta: NodeMeta::default(),
                xattrs: Vec::new(),
            },
        ),
    ]);
    build_image(root, block_bits).expect("the builder makes an image")
}

fn open(image: Vec<u8>) -> Volume {
    Volume::open(Arc::new(MemorySource::new(image))).expect("the volume opens")
}

#[test]
fn the_registry_tells_erofs_by_its_superblock_past_the_first_sector() {
    let image = image(12);
    assert!(drivers::recognise(&image[..512]).is_none());
    let driver = drivers::recognise(&image[..2048]).expect("the registry knows EROFS");
    assert_eq!(driver.kind, FilesystemKind::Erofs);
    assert_eq!(driver.format, "erofs");
    assert_eq!(
        FilesystemKind::detect_boot_sector(&image[..2048]).unwrap(),
        FilesystemKind::Erofs
    );
    assert!(drivers::recognise(&[0u8; 2048]).is_none());
}

#[test]
fn an_image_opens_from_its_saved_superblock_and_says_what_it_is() {
    for bits in [9u8, 12] {
        let image = image(bits);
        let volume = open(image.clone());
        assert_eq!(volume.kind, FilesystemKind::Erofs, "{bits}");
        assert_eq!(volume.kind.to_string(), "EROFS");
        assert!(volume.structures.backup.is_empty() && volume.structures.fat.is_empty());
        assert!(volume.structures.bitmap.is_empty() && volume.structures.root.is_empty());
        assert!(volume.structures.main.len() >= 1024 + 128, "{bits}");
        match &volume.filesystem {
            FilesystemDetails::Driver {
                format,
                allocation_unit,
                ..
            } => {
                assert_eq!(format, "erofs");
                assert_eq!(*allocation_unit, 1u64 << bits, "{bits}");
            }
            other => panic!("{bits}: {other:?}"),
        }
        assert!(volume.driver().is_some(), "{bits}");
        assert!(volume.damage.is_empty(), "{bits}: {:?}", volume.damage);
        // Opened again from the saved copy, over an image whose own superblock is written over.
        let saved = Volume::read_structures(&MemorySource::new(image.clone())).unwrap();
        let mut written_over = image;
        written_over[1024..1024 + 128].fill(0);
        let again = Volume::from_structures(Arc::new(MemorySource::new(written_over)), saved)
            .expect("opens from the saved structures");
        assert_eq!(again.kind, FilesystemKind::Erofs, "{bits}");
    }
}

#[test]
fn the_support_table_comes_from_what_the_driver_lacks() {
    let table = FilesystemKind::Erofs.capabilities();
    let row = |feature: &str| table.iter().find(|row| row.feature == feature).unwrap();
    for feature in [
        "open",
        "scan",
        "browse",
        "export",
        "rebuilds",
        "mount_block",
    ] {
        assert!(!row(feature).note.is_empty(), "{feature}");
    }
    assert_eq!(row("open").support, Support::Supported);
    assert_eq!(row("browse").support, Support::Supported);
    // The driver lacks deleted candidates and deleted extents: a read-only image keeps nothing deleted.
    assert_eq!(row("scan").support, Support::Limited);
    assert_eq!(row("mount_block").support, Support::Missing);
    assert_eq!(
        FilesystemKind::Erofs.claims(),
        captainsalvage::filesystem::Claims::Driver
    );
}

#[test]
fn a_session_over_an_image_scans_and_lists_its_files() {
    use captainsalvage::scan::{run_scan, ScanOptions};
    use captainsalvage::session::Session;
    use captainsalvage::workspace::Workspace;
    use std::sync::atomic::AtomicBool;
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-driver-erofs-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        std::path::Path::new("/test/erofs-image"),
        Arc::new(MemorySource::new(image(12))),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    assert_eq!(session.volume.kind, FilesystemKind::Erofs);
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 2 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let overview = session.overview();
    assert_eq!(overview["filesystem"]["kind"], "driver");
    assert_eq!(overview["filesystem"]["format"], "erofs");
    // The root, a folder, and three files: the sets the scan keeps. The names are the oracle's to check.
    let sets = session.query_map("SELECT count(*) AS n FROM sets").unwrap();
    assert!(sets["rows"][0]["n"].as_i64().unwrap() >= 5, "{sets}");
}

/// EROFS keeps names in its directory entries, so the candidates carry none and the parent claims give them: every entry of the
/// root is named, and a symbolic link reads as the file that holds its target.
#[test]
fn a_symbolic_link_is_listed_by_its_name_as_a_file_that_holds_its_target() {
    use captainsalvage::scan::{run_scan, ScanOptions};
    use captainsalvage::session::Session;
    use captainsalvage::workspace::Workspace;
    use std::sync::atomic::AtomicBool;
    let path =
        std::env::temp_dir().join(format!("captainsalvage-erofs-link-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        std::path::Path::new("/test/erofs-link"),
        Arc::new(MemorySource::new(image(12))),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 2 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let page = session
        .with_database(|database| {
            database.children(session.volume.geometry.root_cluster, 0, 100, true, 0.0)
        })
        .unwrap();
    let mut found = BTreeMap::new();
    for row in page.rows {
        let bytes = match (row.kind.as_str(), row.id) {
            ("file", Some(id)) => {
                session
                    .read_entry_range(id, 0, row.length + 1)
                    .unwrap()
                    .data
            }
            _ => Vec::new(),
        };
        found.insert(row.name.clone(), (row.kind.clone(), row.length, bytes));
    }
    assert_eq!(
        found["link-to-file"],
        ("file".to_string(), 9, b"hello.txt".to_vec()),
        "{found:?}"
    );
    assert_eq!(found["hello.txt"].2, b"hello");
    assert!(found.keys().all(|name| !name.is_empty()), "{found:?}");
}
