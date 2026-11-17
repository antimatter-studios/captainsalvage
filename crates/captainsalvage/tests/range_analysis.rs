//! Analyzing a selected byte range: every driver is tried at every start offset, on synthetic
//! images only. The structures sit at odd offsets, with their signatures damaged or their boot
//! sectors gone, to show the slide finds what a signature search cannot. The source never
//! changes.

use captainsalvage::disk::RawDisk;
use captainsalvage::range_analysis::{analyze, Options, Report, Status};
use captainsalvage::source::MemorySource;
use fs_exfat::fixtures::{self, Builder};
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::{Volume, VolumeAPI};
use sha2::{Digest, Sha256};
use std::path::Path;
use std::sync::{Arc, Mutex};

mod ntfs_support;
use ntfs_support::{pattern, Image};

fn disk(data: Vec<u8>) -> RawDisk {
    let length = data.len() as u64;
    RawDisk::with_source(
        Path::new("synthetic.img"),
        Arc::new(MemorySource::new(data)),
        length,
    )
    .unwrap()
}

fn digest(disk: &RawDisk) -> String {
    let mut hash = Sha256::new();
    let mut offset = 0;
    while offset < disk.length {
        let length = (disk.length - offset).min(1 << 20);
        hash.update(disk.read(offset, length).unwrap());
        offset += length;
    }
    hash.finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn run(disk: &RawDisk, start: u64, length: u64, options: &Options) -> Report {
    let before = digest(disk);
    let report = analyze(disk, start, length, options, &mut |_, _| true).unwrap();
    assert_eq!(digest(disk), before, "the source must not change");
    report
}

/// Deterministic bytes that look like nothing in particular.
fn noise(length: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as u8
        })
        .collect()
}

fn exfat() -> Vec<u8> {
    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * 1024 * 1024, 0);
    builder.write_boot();
    builder.image
}

const NTFS_BYTES: usize = 8 * 1024 * 1024;

fn ntfs() -> Vec<u8> {
    let image = Image(Arc::new(Mutex::new(vec![0; NTFS_BYTES])));
    let mut device = image.clone();
    fs_ntfs::mkfs::format_filesystem(
        &mut device,
        NTFS_BYTES as u64,
        4096,
        1024,
        Some("SALVAGE"),
        Some(0xCA55_0000_0000_2024),
    )
    .unwrap();
    let volume = Volume::open_writable(image.clone()).unwrap();
    volume.create_directory("/", "docs").unwrap();
    for (index, name) in ["a.bin", "b.bin", "c.bin"].iter().enumerate() {
        let path = format!("/docs/{name}");
        volume.create_file("/docs", name).unwrap();
        volume
            .write_file_at(&path, 0, &pattern(index as u8, 9000))
            .unwrap();
    }
    volume.flush().unwrap();
    drop(volume);
    let bytes = image.0.lock().unwrap().clone();
    bytes
}

fn embed(inner: &[u8], at: usize, total: usize, seed: u32) -> Vec<u8> {
    let mut image = noise(total, seed);
    image[at..at + inner.len()].copy_from_slice(inner);
    image
}

fn short_entry(name: &[u8; 11], attributes: u8, cluster: u16, size: u32) -> Vec<u8> {
    let mut entry = vec![0; 32];
    entry[..11].copy_from_slice(name);
    entry[11] = attributes;
    // 2026-11-17 in FAT's date encoding.
    let date: u16 = ((2026 - 1980) << 9) | (11 << 5) | 17;
    entry[16..18].copy_from_slice(&date.to_le_bytes());
    entry[24..26].copy_from_slice(&date.to_le_bytes());
    entry[26..28].copy_from_slice(&cluster.to_le_bytes());
    entry[28..32].copy_from_slice(&size.to_le_bytes());
    entry
}

fn fat_directory() -> Vec<u8> {
    let mut directory = Vec::new();
    directory.extend(short_entry(b".          ", 0x10, 3, 0));
    directory.extend(short_entry(b"..         ", 0x10, 0, 0));
    directory.extend(short_entry(b"TREASURE   ", 0x10, 5, 0));
    directory.extend(short_entry(b"MAP     TXT", 0x20, 7, 1200));
    directory.extend(short_entry(b"LOG     BIN", 0x20, 9, 70_000));
    directory.extend(short_entry(b"\xE5EAD    TXT", 0x20, 11, 50));
    directory.extend([0; 64]);
    directory
}

fn offsets(report: &Report, format: &str) -> Vec<u64> {
    report
        .findings
        .iter()
        .filter(|finding| finding.format == format)
        .map(|finding| finding.offset)
        .collect()
}

#[test]
fn an_ntfs_volume_at_an_odd_offset_is_found_by_its_boot_sector() {
    let inner = ntfs();
    let start = 5 * 512 + 3;
    let disk = disk(embed(&inner, start, NTFS_BYTES + 20_000, 1));
    let report = run(&disk, 0, disk.length, &Options::default());
    let volume = report
        .findings
        .iter()
        .find(|finding| finding.format == "NTFS" && finding.status == Status::Volume)
        .expect("an NTFS volume");
    assert_eq!(volume.offset, start as u64);
    assert_eq!(volume.volume_start, Some(start as u64));
    assert!(volume.signature);
    assert!(volume.confidence >= 0.9, "{}", volume.confidence);
    assert_eq!(report.step, 1);
    let ntfs = report
        .drivers
        .iter()
        .find(|driver| driver.driver == "NTFS")
        .unwrap();
    assert!(ntfs.best_offset.is_some());
    // The MFT records of that volume are its own metadata, not islands.
    assert!(report
        .findings
        .iter()
        .filter(|finding| finding.structure.starts_with("FILE records"))
        .all(|finding| finding.status != Status::Island));
}

#[test]
fn every_driver_is_listed_even_when_nothing_decodes() {
    let disk = disk(noise(1 << 20, 7));
    let report = run(&disk, 0, disk.length, &Options::default());
    assert!(report.findings.is_empty(), "{:?}", report.findings);
    let names: Vec<_> = report.drivers.iter().map(|d| d.driver.as_str()).collect();
    assert_eq!(names, ["exFAT", "FAT", "NTFS", "BitLocker"]);
    assert!(report
        .drivers
        .iter()
        .all(|driver| driver.best_offset.is_none()));
    assert_eq!(report.offsets_tried, 1 << 20);
}

#[test]
fn an_ntfs_volume_with_no_boot_sector_is_placed_from_its_mft_and_opens_in_memory() {
    let mut inner = ntfs();
    // The boot sector and its backup are gone; the records are intact.
    inner[..512].fill(0);
    let last = inner.len() - 512;
    inner[last..].fill(0);
    let start = 2 * 4096 + 7;
    let disk = disk(embed(&inner, start, NTFS_BYTES + 30_000, 2));
    let report = run(&disk, 0, disk.length, &Options::default());
    assert!(!report
        .findings
        .iter()
        .any(|finding| finding.structure.contains("boot sector")));
    let found = report
        .findings
        .iter()
        .find(|finding| finding.structure == "FILE records")
        .unwrap_or_else(|| panic!("records: {:#?}", report.findings));
    assert!(found.signature);
    let geometry = found.geometry.as_ref().unwrap();
    assert_eq!(geometry.cluster_size, 4096);
    assert_eq!(geometry.volume_start, start as u64);
    assert!(geometry.opens, "{}", found.detail);
    assert_eq!(found.status, Status::Inferred);
    assert_eq!(found.volume_start, Some(start as u64));
    assert!(found.confidence >= 0.9);
    assert!(found.missing[0].contains("original boot sector"));
}

#[test]
fn ntfs_records_with_no_signature_left_still_decode_and_propose_their_geometry() {
    let mut inner = ntfs();
    // Overwrite the boot sector, its backup and every FILE magic: no signature survives.
    inner[..512].fill(0);
    let last = inner.len() - 512;
    inner[last..].fill(0);
    for record in (0..inner.len()).step_by(512) {
        if &inner[record..record + 4] == b"FILE" {
            inner[record..record + 4].copy_from_slice(&[0, 0, 0, 0]);
        }
    }
    let start = 2 * 4096 + 7;
    let disk = disk(embed(&inner, start, NTFS_BYTES + 30_000, 2));
    for signatures in [true, false] {
        let report = run(
            &disk,
            0,
            disk.length,
            &Options {
                signatures,
                ..Options::default()
            },
        );
        let found = report
            .findings
            .iter()
            .find(|finding| finding.structure == "FILE records with damaged magic")
            .unwrap_or_else(|| panic!("records: {:#?}", report.findings));
        assert!(!found.signature);
        assert!(found.units >= 5, "{}", found.units);
        // The geometry still follows from the $MFT record's runs and allocated size. The volume
        // cannot be opened, because the driver rightly refuses records without their magic.
        let geometry = found.geometry.as_ref().unwrap();
        assert_eq!(geometry.cluster_size, 4096);
        assert_eq!(geometry.volume_start, start as u64);
        assert!(!geometry.opens);
        assert_eq!(found.status, Status::Island);
        assert_eq!(found.volume_start, Some(start as u64));
        assert!(found.confidence <= 0.6);
    }
}

#[test]
fn fat_directory_entries_decode_by_shape_alone_at_any_offset() {
    let directory = fat_directory();
    for start in [1000usize, 4099, 65_537] {
        let disk = disk(embed(&directory, start, 200_000, 3));
        let report = run(&disk, 0, disk.length, &Options::default());
        let found = report
            .findings
            .iter()
            .find(|finding| finding.format == "FAT")
            .unwrap_or_else(|| panic!("start {start}: {:#?}", report.findings));
        assert_eq!(found.offset, start as u64);
        assert_eq!(found.units, 6);
        assert_eq!(found.decoded_bytes, 6 * 32);
        // Identified, but nothing places it: an island with an offer to look for its metadata.
        assert_eq!(found.status, Status::Island);
        assert!(!found.missing.is_empty());
        let offer = found.offer.as_ref().expect("an offer");
        assert_eq!(offer.formats, ["FAT"]);
        assert!(offer.search_start <= found.offset);
        assert!(offer.search_start + offer.search_length >= found.offset + found.length);
    }
}

#[test]
fn a_larger_step_misses_odd_offsets_and_says_so() {
    let disk = disk(embed(&fat_directory(), 4099, 200_000, 3));
    let report = run(
        &disk,
        0,
        disk.length,
        &Options {
            step: 512,
            ..Options::default()
        },
    );
    assert!(report.findings.is_empty());
    assert!(report
        .notes
        .iter()
        .any(|note| note.contains("multiple of the step")));
}

#[test]
fn an_exfat_volume_is_found_with_its_checksummed_boot_region() {
    let inner = exfat();
    let start = 3 * 512;
    let disk = disk(embed(&inner, start, inner.len() + 10_000, 4));
    let report = run(&disk, 0, disk.length, &Options::default());
    let volume = report
        .findings
        .iter()
        .find(|finding| finding.format == "exFAT" && finding.status == Status::Volume)
        .unwrap_or_else(|| panic!("{:#?}", report.findings));
    assert_eq!(volume.volume_start, Some(start as u64));
    assert!(volume.confidence >= 0.95);
}

#[test]
fn exfat_entry_sets_without_a_boot_sector_stay_an_island() {
    let mut inner = exfat();
    inner[..24 * 512].fill(0);
    let disk = disk(embed(&inner, 777, inner.len() + 2000, 5));
    let report = run(&disk, 0, disk.length, &Options::default());
    let sets = report
        .findings
        .iter()
        .find(|finding| finding.structure == "checksummed directory entry sets")
        .unwrap_or_else(|| panic!("{:#?}", report.findings));
    assert_eq!(sets.status, Status::Island);
    assert!(sets.offer.is_some());
    assert!(!report
        .findings
        .iter()
        .any(|finding| finding.status == Status::Volume));
}

#[test]
fn the_formats_option_limits_the_drivers_tried() {
    let disk = disk(embed(&fat_directory(), 4099, 100_000, 3));
    let report = run(
        &disk,
        0,
        disk.length,
        &Options {
            formats: vec!["NTFS".into()],
            ..Options::default()
        },
    );
    assert_eq!(report.drivers.len(), 1);
    assert!(report.findings.is_empty());
    let both = run(
        &disk,
        0,
        disk.length,
        &Options {
            formats: vec!["fat32".into()],
            ..Options::default()
        },
    );
    assert_eq!(offsets(&both, "FAT"), [4099]);
}

#[test]
fn a_range_inside_the_source_only_examines_that_range() {
    let disk = disk(embed(&fat_directory(), 90_001, 200_000, 3));
    let outside = run(&disk, 0, 50_000, &Options::default());
    assert!(outside.findings.is_empty());
    let inside = run(&disk, 80_000, 20_000, &Options::default());
    assert_eq!(offsets(&inside, "FAT"), [90_001]);
}

#[test]
fn bad_ranges_and_steps_are_refused() {
    let disk = disk(noise(10_000, 1));
    let options = Options::default();
    let attempt =
        |start, length, options: &Options| analyze(&disk, start, length, options, &mut |_, _| true);
    assert!(attempt(0, 0, &options).is_err());
    assert!(attempt(5000, 6000, &options).is_err());
    assert!(attempt(u64::MAX, 2, &options).is_err());
    assert!(attempt(
        0,
        100,
        &Options {
            step: 1 << 30,
            ..Options::default()
        }
    )
    .is_err());
    assert!(attempt(
        0,
        100,
        &Options {
            formats: vec!["zfs".into()],
            ..Options::default()
        }
    )
    .is_err());
}

#[test]
fn a_progress_callback_can_stop_the_analysis() {
    let disk = disk(noise(10 * 1024 * 1024, 9));
    let mut calls = 0;
    let report = analyze(
        &disk,
        0,
        disk.length,
        &Options {
            step: 4096,
            ..Options::default()
        },
        &mut |_, _| {
            calls += 1;
            false
        },
    )
    .unwrap();
    assert_eq!(calls, 1);
    assert!(report.notes.iter().any(|note| note.contains("stopped")));
}

/// A file on disk, removed when dropped.
struct ImageFile(std::path::PathBuf);

impl ImageFile {
    fn new(label: &str, data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-range-{}-{label}.img",
            std::process::id()
        ));
        std::fs::write(&path, data).unwrap();
        Self(path)
    }
}

impl Drop for ImageFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

#[test]
fn the_mcp_tool_and_the_command_line_give_the_same_answer() {
    use captainsalvage::engine::{Engine, AUTHOR_USER};
    use serde_json::{json, Value};
    let data = embed(&fat_directory(), 4099, 100_000, 3);
    let file = ImageFile::new("parity", &data);
    let before = std::fs::read(&file.0).unwrap();
    let engine = Engine::default();
    let opened = engine
        .call("open_raw_disk", &json!({"path": file.0}), AUTHOR_USER)
        .unwrap();
    let arguments = json!({"diskId": opened["id"], "offset": 0, "length": data.len()});
    let tool = engine
        .call("analyze_range", &arguments, AUTHOR_USER)
        .unwrap();
    assert_eq!(tool["findings"][0]["format"], "FAT");
    assert_eq!(tool["findings"][0]["offset"], 4099);
    assert_eq!(tool["findings"][0]["names"][0], "TREASURE");

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
        .args(["range", "analyze"])
        .arg(&file.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let command: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(command, tool);

    let tool_names: Vec<String> = captainsalvage::mcp::tool_definitions()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap().to_string())
        .collect();
    assert!(tool_names.contains(&"captainsalvage_analyze_range".to_string()));
    assert_eq!(std::fs::read(&file.0).unwrap(), before);
}
