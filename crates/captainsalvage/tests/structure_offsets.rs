//! A saved copy of a volume's structures says where on the volume each region was read from (its manifest's `offset`), for FAT,
//! exFAT and NTFS, without changing its hash or what is stored of the bytes.
use captainsalvage::changes::StructureCopy;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::source::MemorySource;
use captainsalvage::volume::Volume;
use fs_exfat::fixtures::build_volume;
use fs_exfat::primitives::BootSector;
use std::sync::Arc;

mod fat_support;
mod ntfs_support;

fn offsets_of(image: Vec<u8>) -> (Volume, Vec<Option<u64>>) {
    let volume = Volume::open(Arc::new(MemorySource::new(image))).expect("opens");
    let offsets = volume
        .structures
        .manifest()
        .regions
        .iter()
        .map(|region| region.offset)
        .collect();
    (volume, offsets)
}

#[test]
fn the_regions_of_a_fat_volume_say_where_they_were_read_from() {
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let fixture = fat_support::Fixture::new(kind);
        let (volume, offsets) = offsets_of(fixture.image.clone());
        let fat = fixture.fat_sector as u64 * 512;
        // The order is the manifest's: main, backup, FAT, bitmap, root.
        let backup = (kind == FilesystemKind::Fat32).then_some(512);
        let root = if kind == FilesystemKind::Fat32 {
            fixture.data_sector as u64 * 512
        } else {
            fixture.root_sector as u64 * 512
        };
        assert_eq!(
            offsets,
            [Some(0), backup, Some(fat), None, Some(root)],
            "{kind}"
        );
        // The bytes of each are the ones at that place on the volume.
        assert_eq!(
            volume.structures.fat,
            fixture.image[fat as usize..fat as usize + volume.structures.fat.len()],
            "{kind}"
        );
        assert_eq!(
            volume.structures.root[..32],
            fixture.image[root as usize..root as usize + 32],
            "{kind}"
        );
        if let Some(at) = backup {
            assert_eq!(
                volume.structures.backup,
                fixture.image[at as usize..at as usize + volume.structures.backup.len()],
                "{kind}"
            );
        }
    }
}

#[test]
fn the_regions_of_an_exfat_volume_say_where_they_were_read_from() {
    let image = build_volume();
    let boot = BootSector::parse(&image[..512]).expect("boot sector");
    let sector = boot.bytes_per_sector();
    let (volume, offsets) = offsets_of(image.clone());
    let root = boot.heap_offset() + u64::from(boot.root_cluster - 2) * boot.cluster_size();
    // The allocation bitmap is found through the root directory, which a copy cannot tell, so it has no place.
    assert_eq!(
        offsets,
        [
            Some(0),
            Some(12 * sector),
            Some(u64::from(boot.fat_offset) * sector),
            None,
            Some(root)
        ]
    );
    assert_eq!(
        volume.structures.backup,
        image[(12 * sector) as usize..(12 * sector) as usize + volume.structures.backup.len()]
    );
    assert_eq!(
        volume.structures.root[..32],
        image[root as usize..root as usize + 32]
    );
}

#[test]
fn the_regions_of_an_ntfs_volume_say_where_they_were_read_from() {
    let fixture = ntfs_support::fixture();
    let image = fixture.image.clone();
    let (volume, offsets) = offsets_of(image.clone());
    let mft =
        u64::from_le_bytes(image[0x30..0x38].try_into().unwrap()) * ntfs_support::CLUSTER as u64;
    // The backup boot sector is the volume's last sector, and the root is the first of three records of `$MFT`.
    let volume_size = volume.geometry.volume_size;
    assert_eq!(offsets, [Some(0), Some(volume_size), None, None, Some(mft)]);
    assert_eq!(
        volume.structures.root[..4],
        image[mft as usize..mft as usize + 4],
        "the root starts with record 0, which is `FILE`"
    );
}

#[test]
fn a_saved_manifest_carries_the_offsets_and_the_hash_and_the_load_are_what_they_were() {
    let fixture = fat_support::Fixture::new(FilesystemKind::Fat16);
    let (volume, _) = offsets_of(fixture.image.clone());
    let directory =
        std::env::temp_dir().join(format!("captainsalvage-offsets-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&directory);
    volume.structures.save(&directory).unwrap();
    let text = std::fs::read_to_string(directory.join("manifest.json")).unwrap();
    let manifest: serde_json::Value = serde_json::from_str(&text).unwrap();
    let saved: Vec<Option<u64>> = manifest["regions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|region| region["offset"].as_u64())
        .collect();
    assert_eq!(
        saved,
        [
            Some(0),
            None,
            Some(512),
            None,
            Some(fixture.root_sector as u64 * 512)
        ]
    );
    // A copy loads as it did, and its structure hash is the same whatever the offsets say.
    let loaded = StructureCopy::load(&directory).expect("loads");
    assert_eq!(loaded, volume.structures);
    assert_eq!(loaded.hash(), volume.structures.hash());
    // A copy saved before offsets were kept (no `offset` in its manifest) still loads: the offset is read from nothing.
    let older = text.replace("\"offset\": 0,", "\"offset\": null,");
    std::fs::write(directory.join("manifest.json"), older).unwrap();
    assert_eq!(
        StructureCopy::load(&directory).expect("loads"),
        volume.structures
    );
    let _ = std::fs::remove_dir_all(directory);
}
