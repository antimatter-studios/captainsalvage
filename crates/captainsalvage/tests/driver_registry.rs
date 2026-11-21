//! A filesystem read through its driver's layers (the registry in `filesystem::drivers`): the engine opens it from its
//! saved structures, says what it is, and lists nothing yet. SquashFS is the one registered driver; nothing here, or in
//! the engine, is specific to it but the label and the image.
use captainsalvage::filesystem::{drivers, FilesystemDetails, FilesystemKind, Support};
use captainsalvage::source::MemorySource;
use captainsalvage::volume::Volume;
use std::sync::Arc;

fn fixture() -> Vec<u8> {
    std::fs::read(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/squashfs-basic.sqfs"),
    )
    .unwrap()
}

fn open(image: Vec<u8>) -> Volume {
    Volume::open(Arc::new(MemorySource::new(image))).expect("the volume opens")
}

#[test]
fn the_registry_tells_a_driver_by_the_bytes_its_volumes_start_with() {
    let image = fixture();
    let driver = drivers::recognise(&image[..512]).expect("the registry knows SquashFS");
    assert_eq!(driver.kind, FilesystemKind::Squashfs);
    assert_eq!(driver.format, "squashfs");
    assert!(drivers::recognise(&[0u8; 512]).is_none());
    assert_eq!(
        FilesystemKind::detect_boot_sector(&image[..512]).unwrap(),
        FilesystemKind::Squashfs
    );
}

#[test]
fn an_image_opens_from_its_saved_superblock_and_says_what_it_is() {
    let volume = open(fixture());
    assert_eq!(volume.kind, FilesystemKind::Squashfs);
    assert_eq!(volume.kind.to_string(), "SquashFS");
    // What is saved of it: the bytes of its superblock, from the start, and no other region.
    assert_eq!(volume.structures.main.len(), 96);
    assert!(volume.structures.backup.is_empty() && volume.structures.fat.is_empty());
    assert!(volume.structures.bitmap.is_empty() && volume.structures.root.is_empty());
    // Its geometry is the driver's: 4096-byte blocks, the allocation unit of the map, from the start of the image.
    // The volume is the bytes it says it uses (`bytes_used`, at 0x28), not the padding after them in the file.
    let image = fixture();
    let declared = u64::from_le_bytes(image[0x28..0x30].try_into().unwrap());
    assert!(declared < image.len() as u64);
    assert_eq!(volume.geometry.cluster_size, 4096);
    assert_eq!(volume.geometry.data_offset, 0);
    assert_eq!(volume.geometry.volume_size, declared);
    assert_eq!(
        u64::from(volume.geometry.cluster_count),
        declared.div_ceil(4096)
    );
    match &volume.filesystem {
        FilesystemDetails::Driver {
            format,
            allocation_unit,
            ..
        } => {
            assert_eq!(format, "squashfs");
            assert_eq!(*allocation_unit, 4096);
        }
        other => panic!("{other:?}"),
    }
    // The driver gives no allocation, so none is known and no cluster is called free.
    assert!(!volume.allocation_known);
    assert_eq!(
        volume.stats.unknown,
        u64::from(volume.geometry.cluster_count)
    );
    assert!(volume.driver().is_some());
    assert_eq!(volume.driver().unwrap().format(), "squashfs");
    assert!(volume.workspace_name().starts_with("squashfs-"));
    assert!(volume.damage.is_empty(), "{:?}", volume.damage);
}

#[test]
fn an_image_is_read_through_its_saved_superblock_when_the_disks_own_is_written_over() {
    let image = fixture();
    let saved = Volume::read_structures(&MemorySource::new(image.clone())).unwrap();
    let mut written_over = image.clone();
    written_over[..96].fill(0);
    let volume = Volume::from_structures(Arc::new(MemorySource::new(written_over)), saved)
        .expect("opens from the saved structures");
    assert_eq!(volume.kind, FilesystemKind::Squashfs);
    let driver = volume
        .driver()
        .expect("the driver opens over the saved bytes");
    assert!(driver.health().unwrap().is_clean());
}

#[test]
fn the_support_table_of_a_driver_comes_from_what_it_lacks() {
    let table = FilesystemKind::Squashfs.capabilities();
    let row = |feature: &str| table.iter().find(|row| row.feature == feature).unwrap();
    for feature in [
        "open",
        "scan",
        "map",
        "browse",
        "preview",
        "resume",
        "baseline",
        "file_tests",
        "rebuilds",
        "export",
        "mount_block",
    ] {
        assert!(!row(feature).note.is_empty(), "{feature}");
    }
    // The volume opens and its structures are kept; nothing that needs its entries is offered yet.
    assert_eq!(row("open").support, Support::Supported);
    assert_eq!(row("baseline").support, Support::Supported);
    for feature in [
        "scan",
        "map",
        "browse",
        "preview",
        "resume",
        "file_tests",
        "rebuilds",
        "export",
    ] {
        assert_eq!(row(feature).support, Support::Missing, "{feature}");
    }
    assert_eq!(row("mount_block").support, Support::Missing);
    assert_eq!(
        FilesystemKind::Squashfs.claims(),
        captainsalvage::filesystem::Claims::Driver
    );
    assert!(!FilesystemKind::Squashfs.claims().has_links());
}

#[test]
fn an_image_that_is_not_a_volume_of_its_driver_is_refused_by_name() {
    let mut image = fixture();
    // The magic is there, and the block size is not a power of two.
    image[12..16].copy_from_slice(&3000u32.to_le_bytes());
    let opened = Volume::open(Arc::new(MemorySource::new(image)));
    // The driver says what is wrong with it; the engine opens it and tells the person, or refuses it by name.
    match opened {
        Ok(volume) => assert!(!volume.damage.is_empty() || volume.driver().is_none()),
        Err(error) => assert!(format!("{error:#}").to_lowercase().contains("squashfs")),
    }
}

#[test]
fn a_session_over_an_image_scans_and_lists_nothing_yet() {
    use captainsalvage::scan::{run_scan, ScanOptions};
    use captainsalvage::session::Session;
    use captainsalvage::workspace::Workspace;
    use std::sync::atomic::AtomicBool;
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-driver-registry-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        std::path::Path::new("/test/squashfs-image"),
        Arc::new(MemorySource::new(fixture())),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    assert_eq!(session.volume.kind, FilesystemKind::Squashfs);
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
    assert_eq!(overview["filesystem"]["format"], "squashfs");
    let sets = session.query_map("SELECT count(*) AS n FROM sets").unwrap();
    assert_eq!(sets["rows"][0]["n"], 0, "{sets}");
}
