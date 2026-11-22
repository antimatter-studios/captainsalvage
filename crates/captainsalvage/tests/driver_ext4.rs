//! ext2, ext3 and ext4 volumes read through their driver's layers, the second driver of the registry in
//! `filesystem::drivers`: the engine recognises one from its superblock (at byte 1024, past the first sector), opens it
//! from its saved structures, and says what it is. The images are made by the driver's own formatter, empty; the files
//! of kernel-made images are the oracle's (`ext4_oracle.rs`).
use captainsalvage::filesystem::{drivers, FilesystemDetails, FilesystemKind, Support};
use captainsalvage::source::MemorySource;
use captainsalvage::volume::Volume;
use fs_ext4::block_io::BlockDevice;
use fs_ext4::features::FsFlavor;
use fs_ext4::mkfs::format_filesystem_with_flavor;
use std::sync::{Arc, Mutex};

struct Memory(Mutex<Vec<u8>>);

impl BlockDevice for Memory {
    fn read_at(&self, offset: u64, buf: &mut [u8]) -> fs_ext4::Result<()> {
        let bytes = self.0.lock().unwrap();
        let start = offset as usize;
        buf.copy_from_slice(&bytes[start..start + buf.len()]);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.lock().unwrap().len() as u64
    }
    fn is_writable(&self) -> bool {
        true
    }
    fn write_at(&self, offset: u64, buf: &[u8]) -> fs_ext4::Result<()> {
        let mut bytes = self.0.lock().unwrap();
        let start = offset as usize;
        bytes[start..start + buf.len()].copy_from_slice(buf);
        Ok(())
    }
}

fn image(flavor: FsFlavor, block_size: u32) -> Vec<u8> {
    let size = 8 << 20;
    let device = Memory(Mutex::new(vec![0; size]));
    format_filesystem_with_flavor(
        &device,
        Some("Salvage"),
        Some([7; 16]),
        size as u64,
        block_size,
        flavor,
    )
    .unwrap();
    device.0.into_inner().unwrap()
}

fn open(image: Vec<u8>) -> Volume {
    Volume::open(Arc::new(MemorySource::new(image))).expect("the volume opens")
}

#[test]
fn the_registry_tells_ext_by_its_superblock_past_the_first_sector() {
    let image = image(FsFlavor::Ext4, 4096);
    assert!(
        drivers::recognise(&image[..512]).is_none(),
        "the first sector alone does not say"
    );
    let driver = drivers::recognise(&image[..2048]).expect("the registry knows ext4");
    assert_eq!(driver.kind, FilesystemKind::Ext4);
    assert_eq!(driver.format, "ext4");
    assert_eq!(
        FilesystemKind::detect_boot_sector(&image[..2048]).unwrap(),
        FilesystemKind::Ext4
    );
    assert!(drivers::recognise(&[0u8; 2048]).is_none());
    // A boot sector that names another filesystem is that filesystem's, whatever lies at the superblock's magic.
    let mut other = vec![0u8; 2048];
    other[3..11].copy_from_slice(b"EXFAT   ");
    other[1080..1082].copy_from_slice(&[0x53, 0xEF]);
    assert!(drivers::recognise(&other).is_none());
    // The other drivers still tell theirs.
    let mut squashfs = vec![0u8; 2048];
    squashfs[..4].copy_from_slice(b"hsqs");
    assert_eq!(
        drivers::recognise(&squashfs).unwrap().kind,
        FilesystemKind::Squashfs
    );
}

#[test]
fn a_volume_of_each_flavour_opens_from_its_saved_superblock_and_says_what_it_is() {
    for (flavor, block_size) in [
        (FsFlavor::Ext4, 4096),
        (FsFlavor::Ext4, 1024),
        (FsFlavor::Ext3, 1024),
        (FsFlavor::Ext2, 4096),
    ] {
        let volume = open(image(flavor, block_size));
        let name = format!("{flavor:?} at {block_size}");
        assert_eq!(volume.kind, FilesystemKind::Ext4, "{name}");
        assert_eq!(volume.kind.to_string(), "ext2/3/4");
        // What is saved of it: the bytes from the start to the end of its superblock, and no other region.
        assert_eq!(volume.structures.main.len(), 2048, "{name}");
        assert!(volume.structures.backup.is_empty() && volume.structures.fat.is_empty());
        assert!(volume.structures.bitmap.is_empty() && volume.structures.root.is_empty());
        // The map's allocation unit is at least 4096 bytes: a 1024-byte block volume maps four blocks to a unit.
        assert_eq!(
            volume.geometry.cluster_size,
            u64::from(block_size.max(4096)),
            "{name}"
        );
        assert_eq!(volume.geometry.volume_size, 8 << 20, "{name}");
        match &volume.filesystem {
            FilesystemDetails::Driver {
                format,
                label,
                allocation_unit,
                ..
            } => {
                assert_eq!(format, "ext4");
                assert_eq!(*allocation_unit, u64::from(block_size));
                assert_eq!(label.as_deref(), Some("Salvage"), "{name}");
            }
            other => panic!("{name}: {other:?}"),
        }
        assert!(volume.driver().is_some(), "{name}");
        assert!(volume.workspace_name().starts_with("ext4-"), "{name}");
        assert!(volume.damage.is_empty(), "{name}: {:?}", volume.damage);
    }
}

#[test]
fn a_volume_is_read_through_its_saved_superblock_when_the_disks_own_is_written_over() {
    let image = image(FsFlavor::Ext4, 4096);
    let saved = Volume::read_structures(&MemorySource::new(image.clone())).unwrap();
    let mut written_over = image.clone();
    written_over[1024..2048].fill(0);
    let volume = Volume::from_structures(Arc::new(MemorySource::new(written_over)), saved)
        .expect("opens from the saved structures");
    assert_eq!(volume.kind, FilesystemKind::Ext4);
    assert!(volume.driver().is_some());
}

#[test]
fn the_support_table_comes_from_what_the_driver_lacks() {
    let table = FilesystemKind::Ext4.capabilities();
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
    assert_eq!(row("mount_block").support, Support::Missing);
    assert_eq!(
        FilesystemKind::Ext4.claims(),
        captainsalvage::filesystem::Claims::Driver
    );
}
