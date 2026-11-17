use super::*;
use crate::engine::{Engine, AUTHOR_CLAUDE, AUTHOR_USER};
use crate::filesystem::FilesystemKind;
use crate::source::MemorySource;
use crate::volume::Volume;
use fs_exfat::fixtures::{self, entry_set, Builder, SECTOR};
use serde_json::json;
use std::sync::atomic::AtomicUsize;

static NEXT_TEST: AtomicUsize = AtomicUsize::new(0);

struct TestFile(PathBuf);

impl TestFile {
    fn new(data: &[u8]) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "captainsalvage-raw-{}-{}",
            std::process::id(),
            NEXT_TEST.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("disk.img");
        std::fs::write(&path, data).unwrap();
        Self(path)
    }
}

impl Drop for TestFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

fn disk(data: Vec<u8>) -> RawDisk {
    let length = data.len() as u64;
    RawDisk::with_source(
        Path::new("fixture.img"),
        Arc::new(MemorySource::new(data)),
        length,
    )
    .unwrap()
}

fn exfat() -> Vec<u8> {
    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * 1024 * 1024, 0);
    builder.write_boot();
    builder.image
}

fn ntfs_boot(length: u64) -> Vec<u8> {
    let mut data = vec![0; SECTOR];
    data[3..11].copy_from_slice(b"NTFS    ");
    data[11..13].copy_from_slice(&512u16.to_le_bytes());
    data[13] = 8;
    data[40..48].copy_from_slice(&(length / 512).to_le_bytes());
    data[510..512].copy_from_slice(&[0x55, 0xaa]);
    data
}

fn region(start: u64, length: u64) -> DiskRegion {
    DiskRegion {
        start,
        length,
        format: "exFAT".into(),
        label: "Possible old exFAT volume".into(),
        boot_offset: start,
    }
}

#[test]
fn unknown_sources_open_and_exact_tail_bytes_remain_accessible() {
    let file = TestFile::new(&[7; 1025]);
    let disk = RawDisk::open(&file.0).unwrap();
    assert_eq!(disk.length, 1025);
    assert_eq!(disk.read(1024, 1).unwrap(), [7]);
    assert!(disk.overview().partitions.is_empty());
    assert!(disk.discover(0, 1025).unwrap().findings.is_empty());
    assert_eq!(disk.source.read_stats().errors, 0);
    assert!(disk.source.read_stats().bytes > 0);
    assert_eq!(std::fs::read(&file.0).unwrap(), vec![7; 1025]);
}

#[test]
fn empty_sources_and_invalid_raw_ranges_are_rejected() {
    let empty = TestFile::new(&[]);
    assert!(RawDisk::open(&empty.0).is_err());
    let disk = disk(vec![0; 512]);
    assert!(disk.read(512, 1).is_err());
    assert!(disk.read(u64::MAX, 1).is_err());
    assert!(disk.read(0, DISCOVERY_BATCH_BYTES + 1).is_err());
    assert!(disk.discover(0, 0).is_err());
    assert!(disk.discover(500, 13).is_err());
    assert!(disk.discover(0, DISCOVERY_BATCH_BYTES + 1).is_err());
    assert_eq!(disk.read(512, 0).unwrap(), Vec::<u8>::new());
}

#[test]
fn bounded_views_never_read_the_neighbouring_region() {
    let source = Arc::new(MemorySource::new((0..=255).collect()));
    let bounded = RegionSource::new(source.clone(), 256, 20, 10).unwrap();
    let mut bytes = [0; 10];
    bounded.read_exact_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [20, 21, 22, 23, 24, 25, 26, 27, 28, 29]);
    assert!(bounded.read_exact_at(9, &mut [0; 2]).is_err());
    assert!(bounded.read_exact_at(u64::MAX, &mut [0; 1]).is_err());
    assert!(RegionSource::new(source.clone(), 256, 0, 0).is_err());
    assert!(RegionSource::new(source.clone(), 256, u64::MAX, 2).is_err());
    assert!(RegionSource::new(source.clone(), 256, 250, 7).is_err());
    assert_eq!(bounded.read_stats().operations, 1);
    source.disconnect();
    assert!(bounded.is_gone());
    assert!(bounded.read_exact_at(0, &mut [0]).is_err());
}

#[test]
fn finding_ntfs_does_not_stop_discovery_of_exfat_at_the_end() {
    let old = exfat();
    let start = 1024 * 1024;
    let mut image = vec![0; start + old.len()];
    let length = image.len() as u64;
    image[..512].copy_from_slice(&ntfs_boot(length));
    image[start..].copy_from_slice(&old);
    let disk = disk(image);
    let findings = disk.discover(0, disk.length).unwrap().findings;
    assert!(findings
        .iter()
        .any(|item| item.format == "NTFS" && item.offset == 0 && !item.supported));
    assert!(findings
        .iter()
        .any(|item| item.format == "exFAT" && item.volume_start == Some(start as u64)));
    let volume = Volume::open(
        disk.interpretation(&region(start as u64, old.len() as u64))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(volume.kind, FilesystemKind::Exfat);
    assert_eq!(volume.read_bytes(0, 11).unwrap()[3..], *b"EXFAT   ");
    assert!(disk.overview().warnings.is_empty());
}

#[test]
fn overwritten_main_boot_can_use_surviving_exfat_backup_without_writing_ntfs() {
    let mut image = exfat();
    let length = image.len() as u64;
    image[..512].copy_from_slice(&ntfs_boot(length));
    let disk = disk(image);
    let discoveries = disk.discover(0, disk.length).unwrap();
    let candidate = discoveries
        .findings
        .iter()
        .find(|item| {
            item.format == "exFAT" && item.volume_start == Some(0) && item.offset == 12 * 512
        })
        .unwrap();
    let mut interpretation = region(0, length);
    interpretation.boot_offset = candidate.boot_offset.unwrap();
    let view = disk.interpretation(&interpretation).unwrap();
    let volume = Volume::open(view.clone()).unwrap();
    assert_eq!(volume.kind, FilesystemKind::Exfat);
    let offset = Builder::cluster_position(10) as u64;
    assert_eq!(volume.read_bytes(offset, 4).unwrap(), b"ID3\x04");
    assert_eq!(&disk.read(0, 11).unwrap()[3..], b"NTFS    ");
    let mut across = vec![0; 16];
    view.read_exact_at(12 * 512 - 8, &mut across).unwrap();
    assert_eq!(&across[8..], &disk.read(12 * 512, 8).unwrap());
}

#[test]
fn directory_fragments_are_evidence_not_invented_volume_geometry() {
    let mut image = vec![0; 4096];
    let entry = entry_set("lost.mp3", false, 123, 4096, true, true);
    image[1024..1024 + entry.len()].copy_from_slice(&entry);
    let result = disk(image).discover(0, 4096).unwrap();
    let entry = result
        .findings
        .iter()
        .find(|item| item.structure.contains("directory"))
        .unwrap();
    assert_eq!(entry.offset, 1024);
    assert!(entry.volume_start.is_none());
    assert!(entry.volume_length.is_none());
    assert_eq!(entry.format, "exFAT");
}

#[test]
fn discovery_handles_unaligned_ranges_and_structures_crossing_batches() {
    let entry = entry_set("boundary.mp3", false, 123, 4096, true, true);
    let offset = SCAN_CHUNK_BYTES as usize - 32;
    let mut image = vec![0; offset + 4096];
    image[offset..offset + entry.len()].copy_from_slice(&entry);
    let disk = disk(image);
    let first = disk.discover(1, offset as u64).unwrap();
    assert_eq!(first.findings.len(), 1);
    assert_eq!(first.findings[0].offset, offset as u64);
    let second = disk
        .discover(first.next_offset, disk.length - first.next_offset)
        .unwrap();
    assert!(second.findings.is_empty());
}

#[test]
fn corrupt_signatures_do_not_create_a_confirmed_filesystem() {
    let mut image = vec![0; 4096];
    image[3..11].copy_from_slice(b"EXFAT   ");
    image[515..523].copy_from_slice(b"NTFS    ");
    let mut entry = entry_set("bad.mp3", false, 10, 100, true, true);
    entry[2] ^= 1;
    image[1024..1024 + entry.len()].copy_from_slice(&entry);
    assert!(disk(image).discover(0, 4096).unwrap().findings.is_empty());
}

#[test]
fn evidence_batches_have_an_exact_continuation_without_losing_hits() {
    let entry = entry_set("many.mp3", false, 10, 100, true, true);
    let mut image = vec![0; 512 * 400];
    for offset in (0..image.len()).step_by(512) {
        image[offset..offset + entry.len()].copy_from_slice(&entry);
    }
    let disk = disk(image);
    let first = disk.discover(0, disk.length).unwrap();
    assert!(first.evidence_limit_reached);
    assert_eq!(first.findings.len(), MAX_EVIDENCE);
    let second = disk
        .discover(first.next_offset, disk.length - first.next_offset)
        .unwrap();
    let offsets: std::collections::BTreeSet<_> = first
        .findings
        .iter()
        .chain(&second.findings)
        .map(|item| item.offset)
        .collect();
    assert_eq!(offsets.len(), 400);
}

struct BadSpot;

impl BlockSource for BadSpot {
    fn read_exact_at(&self, offset: u64, bytes: &mut [u8]) -> io::Result<()> {
        if offset == 0 {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        bytes.fill(0);
        Ok(())
    }
    fn read_stats(&self) -> ReadStats {
        ReadStats::default()
    }
}

#[test]
fn unreadable_regions_are_reported_and_not_classified_as_empty() {
    let disk = RawDisk::with_source(
        Path::new("bad.img"),
        Arc::new(BadSpot),
        SCAN_CHUNK_BYTES * 2,
    )
    .unwrap();
    let scan = disk.discover(0, disk.length).unwrap();
    assert_eq!(scan.errors.len(), 1);
    assert_eq!(scan.errors[0].length, SCAN_CHUNK_BYTES);
    assert_eq!(scan.next_offset, disk.length);
    assert!(!disk.overview().warnings.is_empty());
}

#[test]
fn disconnect_stops_discovery_instead_of_reporting_zeroes() {
    let source = Arc::new(MemorySource::new(vec![0; 4096]));
    let disk = RawDisk::with_source(Path::new("gone.img"), source.clone(), 4096).unwrap();
    source.disconnect_after_reads(0);
    assert!(disk
        .discover(0, 4096)
        .err()
        .unwrap()
        .to_string()
        .contains("disconnected"));
}

#[test]
fn bad_interpretations_are_refused_without_changing_the_source() {
    let disk = disk(exfat());
    let mut request = region(0, disk.length);
    request.format = "FAT32".into();
    assert!(disk.interpretation(&request).is_err());
    request.boot_offset = 512;
    assert!(disk.interpretation(&request).is_err());
    request.format = "exFAT".into();
    assert!(disk.interpretation(&request).is_err());
    request.boot_offset = 12 * 512;
    request.label = "x".repeat(513);
    assert!(disk.interpretation(&request).is_err());
    assert_eq!(&disk.read(3, 8).unwrap(), b"EXFAT   ");
}

#[test]
fn ntfs_can_be_inspected_without_claiming_ntfs_recovery_support() {
    let mut image = vec![0; 4096];
    image[..512].copy_from_slice(&ntfs_boot(4096));
    let disk = disk(image);
    let mut request = region(0, disk.length);
    request.format = "NTFS".into();
    assert!(disk
        .interpretation(&request)
        .err()
        .unwrap()
        .to_string()
        .contains("not integrated"));
    assert_eq!(&disk.read(3, 8).unwrap(), b"NTFS    ");
}

#[test]
fn mbr_boundaries_do_not_restrict_discovery_or_hide_overlaps() {
    let mut image = vec![0; 4096];
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    for (index, start) in [1u32, 2].into_iter().enumerate() {
        let base = 446 + index * 16;
        image[base + 4] = 7;
        image[base + 8..base + 12].copy_from_slice(&start.to_le_bytes());
        image[base + 12..base + 16].copy_from_slice(&4u32.to_le_bytes());
    }
    let disk = disk(image);
    let overview = disk.overview();
    assert_eq!(overview.partitions.len(), 2);
    assert_eq!(overview.partitions[0].start, 512);
    assert_eq!(disk.read(3500, 1).unwrap(), [0]);
}

#[test]
fn corrupt_and_extended_partition_entries_remain_warnings() {
    let mut image = vec![0; 1024];
    image[510..512].copy_from_slice(&[0x55, 0xaa]);
    image[450] = 0x0f;
    image[466] = 7;
    image[470..474].copy_from_slice(&10u32.to_le_bytes());
    image[474..478].copy_from_slice(&10u32.to_le_bytes());
    let overview = disk(image).overview();
    assert!(overview.partitions.is_empty());
    assert_eq!(overview.warnings.len(), 2);
    assert!(disk(vec![0; 100]).overview().partitions.is_empty());
}

fn gpt(sector: usize) -> Vec<u8> {
    let mut image = vec![0; sector * 12];
    let entry = &mut image[sector * 2..sector * 2 + 128];
    entry[0] = 1;
    entry[32..40].copy_from_slice(&4u64.to_le_bytes());
    entry[40..48].copy_from_slice(&7u64.to_le_bytes());
    entry[56..58].copy_from_slice(&(b'X' as u16).to_le_bytes());
    let entries_crc = crc32(entry);
    let header = &mut image[sector..sector * 2];
    header[..8].copy_from_slice(b"EFI PART");
    header[12..16].copy_from_slice(&92u32.to_le_bytes());
    header[72..80].copy_from_slice(&2u64.to_le_bytes());
    header[80..84].copy_from_slice(&1u32.to_le_bytes());
    header[84..88].copy_from_slice(&128u32.to_le_bytes());
    header[88..92].copy_from_slice(&entries_crc.to_le_bytes());
    let checksum = crc32(&header[..92]);
    header[16..20].copy_from_slice(&checksum.to_le_bytes());
    image
}

#[test]
fn gpt_partitions_require_checksums_and_support_512_and_4096_byte_sectors() {
    assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
    for sector in [512, 4096] {
        let image = gpt(sector);
        let overview = disk(image.clone()).overview();
        assert!(overview.warnings.is_empty());
        assert_eq!(overview.partitions.len(), 1);
        assert_eq!(overview.partitions[0].start, 4 * sector as u64);
        assert_eq!(overview.partitions[0].length, 4 * sector as u64);
        assert_eq!(overview.partitions[0].label, "X");
        let mut corrupt = image.clone();
        corrupt[sector + 16] ^= 1;
        assert!(!disk(corrupt).overview().warnings.is_empty());
        let mut corrupt = image;
        corrupt[sector * 2] ^= 1;
        assert!(!disk(corrupt).overview().warnings.is_empty());
    }
}

#[test]
fn engine_raw_tools_work_without_a_filesystem_and_reject_stale_offsets() {
    let file = TestFile::new(&[9; 4096]);
    let engine = Engine::default();
    let opened = engine
        .call("open_raw_disk", &json!({"path": file.0}), AUTHOR_USER)
        .unwrap();
    let args = json!({"diskId": opened["id"], "offset": 0, "length": 10});
    assert_eq!(
        engine.call("read_raw_disk", &args, AUTHOR_USER).unwrap()["hex"],
        "09".repeat(10)
    );
    assert_eq!(
        engine
            .call("discover_raw_disk", &args, AUTHOR_USER)
            .unwrap()["nextOffset"],
        10
    );
    assert!(engine.session().is_err());
    engine.set_names_hidden(true);
    assert!(engine.call("read_raw_disk", &args, AUTHOR_CLAUDE).is_err());
    engine
        .call("open_raw_disk", &json!({"path": file.0}), AUTHOR_USER)
        .unwrap();
    assert!(engine
        .call("read_raw_disk", &args, AUTHOR_USER)
        .unwrap_err()
        .to_string()
        .contains("changed"));
    assert_eq!(std::fs::read(&file.0).unwrap(), vec![9; 4096]);
}

#[test]
fn all_fat_variants_are_probed_through_the_driver_adapter() {
    for (format, sectors, fat_sectors, roots) in [
        ("FAT12", 100u32, 1u32, 16u16),
        ("FAT16", 10_000, 40, 16),
        ("FAT32", 100_000, 800, 0),
    ] {
        let mut boot = vec![0; 512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&1u16.to_le_bytes());
        boot[16] = 1;
        boot[17..19].copy_from_slice(&roots.to_le_bytes());
        boot[32..36].copy_from_slice(&sectors.to_le_bytes());
        if format == "FAT32" {
            boot[36..40].copy_from_slice(&fat_sectors.to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        } else {
            boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
        }
        boot[510..512].copy_from_slice(&[0x55, 0xaa]);
        let findings = probe::inspect(&boot, 0);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].format, format);
        assert_eq!(findings[0].volume_length, Some(u64::from(sectors) * 512));
    }
}

#[test]
fn different_region_views_keep_their_offsets_and_reads_independent() {
    let source = Arc::new(MemorySource::new(vec![1, 2, 3, 4, 5, 6]));
    let first = RegionSource::new(source.clone(), 6, 0, 4).unwrap();
    let overlapping = RegionSource::new(source, 6, 2, 4).unwrap();
    let mut bytes = [0; 4];
    first.read_exact_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [1, 2, 3, 4]);
    overlapping.read_exact_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [3, 4, 5, 6]);
    first.read_exact_at(0, &mut bytes).unwrap();
    assert_eq!(bytes, [1, 2, 3, 4]);
}

#[test]
fn interpreted_exfat_uses_the_normal_scan_and_file_read_workflow() {
    let mut image = exfat();
    let length = image.len() as u64;
    image[..512].copy_from_slice(&ntfs_boot(length));
    let file = TestFile::new(&image);
    let engine = Engine::default();
    let raw = engine
        .call("open_raw_disk", &json!({"path": file.0}), AUTHOR_USER)
        .unwrap();
    let opened = engine
        .call(
            "open_disk_region",
            &json!({
                "diskId": raw["id"], "start": 0, "length": length,
                "format": "exFAT", "bootOffset": 12 * 512,
                "label": "Old exFAT under NTFS",
            }),
            AUTHOR_USER,
        )
        .unwrap();
    assert_eq!(opened["interpretation"]["label"], "Old exFAT under NTFS");
    let workspace = PathBuf::from(opened["workspace"].as_str().unwrap());
    assert!(workspace.join("interpretation.json").is_file());
    let session = engine.session().unwrap();
    crate::scan::run_scan(
        &session.volume,
        &session.workspace,
        crate::scan::ScanOptions::for_volume(&session.volume),
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let found = engine
        .call("search", &json!({"query": "keep.mp3"}), AUTHOR_USER)
        .unwrap();
    let entry = &found["rows"].as_array().unwrap()[0];
    let (_, bytes) = session
        .read_entry(entry["id"].as_i64().unwrap(), 1024 * 1024)
        .unwrap();
    assert_eq!(&bytes[..4], b"ID3\x04");
    assert!(engine.open(file.0.to_str().unwrap()).is_err());
    assert_eq!(std::fs::read(&file.0).unwrap(), image);
    drop(session);
    drop(engine);
    std::fs::remove_dir_all(workspace).unwrap();
}

#[test]
fn invalid_open_region_does_not_replace_the_current_session_or_raw_source() {
    let file = TestFile::new(&[0; 4096]);
    let engine = Engine::default();
    let raw = engine
        .call("open_raw_disk", &json!({"path": file.0}), AUTHOR_USER)
        .unwrap();
    assert!(engine
        .call(
            "open_disk_region",
            &json!({
                "diskId": raw["id"], "start": 0, "length": 4096, "format": "exFAT"
            }),
            AUTHOR_USER
        )
        .is_err());
    assert!(engine.session().is_err());
    assert_eq!(
        engine
            .call(
                "read_raw_disk",
                &json!({"diskId": raw["id"], "offset": 0, "length": 1}),
                AUTHOR_USER
            )
            .unwrap()["hex"],
        "00"
    );
    assert!(engine
        .call(
            "read_raw_disk",
            &json!({"diskId": raw["id"], "offset": 0, "length": 65537}),
            AUTHOR_USER
        )
        .is_err());
    assert!(engine
        .call(
            "read_raw_disk",
            &json!({"offset": 0, "length": 1}),
            AUTHOR_USER
        )
        .is_err());
    assert!(Engine::default()
        .call(
            "read_raw_disk",
            &json!({"diskId": "1", "offset": 0, "length": 1}),
            AUTHOR_USER
        )
        .is_err());
}

#[test]
fn backup_metadata_must_fit_the_selected_region_before_it_can_be_read() {
    let disk = disk(exfat());
    let mut request = region(0, 12 * 512);
    request.boot_offset = 12 * 512;
    let before = disk.source.read_stats().operations;
    assert!(disk
        .interpretation(&request)
        .err()
        .unwrap()
        .to_string()
        .contains("outside selected"));
    assert_eq!(disk.source.read_stats().operations, before);
    request.length = 13 * 512;
    assert!(disk
        .interpretation(&request)
        .err()
        .unwrap()
        .to_string()
        .contains("exceeds interpretation"));
    request.start = 12 * 512;
    request.boot_offset = 0;
    assert!(disk.interpretation(&request).is_err());
}
