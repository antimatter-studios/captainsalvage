use super::*;
use crate::source::{BlockSource, MemorySource};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

struct Database(PathBuf);

impl Database {
    fn new() -> Self {
        let dir = std::env::temp_dir().join(format!(
            "captainsalvage-checkpoint-{}",
            identifier().unwrap()
        ));
        std::fs::create_dir(&dir).unwrap();
        Self(dir.join("scans.sqlite"))
    }

    fn open(&self) -> ScanStore {
        ScanStore::open(&self.0).unwrap()
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(self.0.parent().unwrap());
    }
}

fn disk(path: &str, data: Vec<u8>) -> RawDisk {
    let length = data.len() as u64;
    RawDisk::with_source(Path::new(path), Arc::new(MemorySource::new(data)), length).unwrap()
}

fn evidence_bytes() -> Vec<u8> {
    let mut data = vec![0; 512 * 1024];
    for sector in data.chunks_exact_mut(512) {
        sector[3..11].copy_from_slice(b"NTFS    ");
        sector[11..13].copy_from_slice(&512u16.to_le_bytes());
        sector[13] = 8;
        sector[40..48].copy_from_slice(&1024u64.to_le_bytes());
        sector[510..512].copy_from_slice(&[0x55, 0xaa]);
    }
    data
}

struct BadRegion {
    source: MemorySource,
    fail: AtomicBool,
}

impl BlockSource for BadRegion {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        if self.fail.load(Ordering::Relaxed)
            && buffer.len() > SAMPLE_BYTES as usize
            && offset == 65536
        {
            return Err(io::Error::from_raw_os_error(libc::EIO));
        }
        self.source.read_exact_at(offset, buffer)
    }

    fn read_stats(&self) -> crate::source::ReadStats {
        self.source.read_stats()
    }
}

#[test]
fn fingerprint_matches_clones_and_images_independent_of_path() {
    let first = disk("/dev/rdisk4", vec![31; 128 * 1024]);
    let clone = disk("copy.img", vec![31; 128 * 1024]);
    let saved = Fingerprint::read(&first);
    assert_eq!(
        saved.verify(&Fingerprint::read(&clone)),
        Verification::SampledMatch
    );
    assert_ne!(first.id, clone.id);
    assert_eq!(saved.samples.len(), SAMPLE_COUNT as usize);
    assert_eq!(
        saved.samples.last().unwrap().offset + SAMPLE_BYTES,
        first.length
    );
}

#[test]
fn tiny_and_unaligned_sources_have_bounded_unique_samples() {
    for size in [1, 511, 4096, 4097, 65539] {
        let source = disk("small.img", vec![19; size]);
        let fingerprint = Fingerprint::read(&source);
        assert!(fingerprint.sampled_sha256.is_some());
        assert!(fingerprint
            .samples
            .iter()
            .all(|sample| sample.offset + sample.length <= size as u64));
        assert!(fingerprint
            .samples
            .windows(2)
            .all(|pair| pair[0].offset < pair[1].offset));
        assert_eq!(
            fingerprint.samples.last().unwrap().offset + fingerprint.samples.last().unwrap().length,
            size as u64
        );
    }
}

#[test]
fn changed_bytes_sizes_and_fingerprint_versions_do_not_match() {
    let original = Fingerprint::read(&disk("disk", vec![1; 8192]));
    let mut changed = vec![1; 8192];
    changed[8191] = 2;
    assert_eq!(
        original.verify(&Fingerprint::read(&disk("disk", changed))),
        Verification::Mismatch
    );
    assert_eq!(
        original.verify(&Fingerprint::read(&disk("disk", vec![1; 8193]))),
        Verification::Mismatch
    );
    let mut future = original.clone();
    future.version += 1;
    assert_eq!(original.verify(&future), Verification::Incompatible);
    let mut unverified = original.clone();
    unverified.sampled_sha256 = None;
    assert_eq!(original.verify(&unverified), Verification::Unverifiable);
    assert_eq!(unverified.verify(&original), Verification::Unverifiable);
}

#[test]
fn a_sampled_match_does_not_claim_unsampled_bytes_are_identical() {
    let first = Fingerprint::read(&disk("disk", vec![0; 1024 * 1024]));
    let mut changed = vec![0; 1024 * 1024];
    changed[8192] = 1;
    assert_eq!(
        first.verify(&Fingerprint::read(&disk("disk", changed))),
        Verification::SampledMatch
    );
}

#[test]
fn checkpoints_findings_and_ids_survive_database_reopening() {
    let database = Database::new();
    let source = disk("/dev/rdisk4", evidence_bytes());
    let mut store = database.open();
    let scan = store.create(&source, 0, source.length).unwrap();
    let step = store.step(&source, &scan.scan_id).unwrap();
    assert!(step.batch.evidence_limit_reached);
    assert!(step.batch.next_offset < source.length);
    let count = step.batch.findings.len();
    drop(store);
    let mut reopened = database.open();
    let clone = disk("renamed.img", evidence_bytes());
    assert!(reopened.step(&clone, &scan.scan_id).is_err());
    assert!(reopened.current(&clone, &scan.scan_id).is_err());
    let loaded = reopened.resume(&clone, &scan.scan_id, true).unwrap();
    assert_eq!(loaded.scan_id, scan.scan_id);
    assert_eq!(loaded.next_offset, step.batch.next_offset);
    assert_eq!(loaded.findings.len(), count);
    assert_eq!(
        reopened.current(&clone, &scan.scan_id).unwrap().next_offset,
        loaded.next_offset
    );
    let listed = reopened.list(&clone).unwrap();
    assert_eq!(listed.scans[0].verification, Verification::SampledMatch);
    assert_ne!(scan.scan_id, listed.fingerprint.sampled_sha256.unwrap());
    assert!(reopened.step(&source, &scan.scan_id).is_err());
    assert!(reopened.current(&source, &scan.scan_id).is_err());
    while reopened.snapshot(&scan.scan_id).unwrap().next_offset < source.length {
        reopened.step(&clone, &scan.scan_id).unwrap();
    }
    assert_eq!(
        reopened.snapshot(&scan.scan_id).unwrap().findings.len(),
        1024
    );
    assert!(reopened.step(&clone, &scan.scan_id).is_err());
}

#[test]
fn one_fingerprint_can_have_multiple_independent_scan_ranges() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", vec![3; 128 * 1024]);
    let first = store.create(&source, 0, 65536).unwrap();
    let second = store.create(&source, 65536, 32768).unwrap();
    assert_ne!(first.scan_id, second.scan_id);
    store.step(&source, &second.scan_id).unwrap();
    assert_eq!(store.snapshot(&first.scan_id).unwrap().next_offset, 0);
    let scans = store.list(&source).unwrap().scans;
    assert_eq!(scans.len(), 2);
    assert!(scans
        .iter()
        .all(|scan| scan.verification == Verification::SampledMatch));
}

#[test]
fn invalid_ranges_and_unknown_scan_ids_are_rejected() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", vec![0; 4096]);
    for (start, length) in [(0, 0), (4096, 1), (4095, 2), (u64::MAX, 2)] {
        assert!(store.create(&source, start, length).is_err());
    }
    assert!(store.resume(&source, "absent", true).is_err());
    assert!(store.step(&source, "absent").is_err());
    assert!(store.list(&source).unwrap().scans.is_empty());
}

#[test]
fn resume_requires_acknowledgement_and_matching_current_content() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("/dev/rdisk4", vec![1; 8192]);
    let scan = store.create(&source, 0, source.length).unwrap();
    assert!(store.resume(&source, &scan.scan_id, false).is_err());
    for size in [8192, 16384] {
        let replacement = disk("/dev/rdisk4", vec![2; size]);
        assert!(store.resume(&replacement, &scan.scan_id, true).is_err());
        assert!(store.step(&replacement, &scan.scan_id).is_err());
        assert_eq!(
            store.list(&replacement).unwrap().scans[0].verification,
            Verification::Mismatch
        );
    }
    let unrelated = disk("other.img", vec![2; 8192]);
    assert!(store.list(&unrelated).unwrap().scans.is_empty());
    assert_eq!(store.snapshot(&scan.scan_id).unwrap().next_offset, 0);
}

#[test]
fn unreadable_fingerprint_samples_never_authorise_resume() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", vec![0; 8192]);
    let scan = store.create(&source, 0, source.length).unwrap();
    let memory = Arc::new(MemorySource::new(vec![0; 8192]));
    memory.disconnect();
    let gone = RawDisk::with_source(Path::new("image"), memory, 8192).unwrap();
    assert!(Fingerprint::read(&gone).sampled_sha256.is_none());
    assert_eq!(
        store.list(&gone).unwrap().scans[0].verification,
        Verification::Unverifiable
    );
    assert!(store.resume(&gone, &scan.scan_id, true).is_err());
    assert!(store.create(&gone, 0, 8192).is_err());
}

#[test]
fn incompatible_scanner_versions_are_listed_but_cannot_resume() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", vec![1; 4096]);
    let scan = store.create(&source, 0, 4096).unwrap();
    store
        .connection
        .execute("UPDATE raw_scans SET scanner_version = 99", [])
        .unwrap();
    assert_eq!(
        store.list(&source).unwrap().scans[0].verification,
        Verification::Incompatible
    );
    assert!(store.resume(&source, &scan.scan_id, true).is_err());
}

#[test]
fn read_errors_belong_to_the_source_and_are_retried_on_a_clone() {
    let database = Database::new();
    let mut store = database.open();
    let bytes = vec![0; 256 * 1024];
    let failing = Arc::new(BadRegion {
        source: MemorySource::new(bytes.clone()),
        fail: AtomicBool::new(true),
    });
    let source = RawDisk::with_source(Path::new("broken"), failing, bytes.len() as u64).unwrap();
    let scan = store.create(&source, 0, source.length).unwrap();
    let step = store.step(&source, &scan.scan_id).unwrap();
    assert_eq!(step.batch.errors.len(), 1);
    assert_eq!(store.snapshot(&scan.scan_id).unwrap().errors.len(), 1);
    drop(store);
    let clone = disk("healthy.img", bytes);
    let mut store = database.open();
    let loaded = store.resume(&clone, &scan.scan_id, true).unwrap();
    assert_eq!(loaded.next_offset, 65536);
    assert!(loaded.errors.is_empty());
    assert!(store
        .step(&clone, &scan.scan_id)
        .unwrap()
        .batch
        .errors
        .is_empty());
    let loaded = store.resume(&clone, &scan.scan_id, true).unwrap();
    assert_eq!(loaded.next_offset, source.length);
    assert!(loaded.errors.is_empty());
}

#[test]
fn a_failed_save_rolls_back_findings_and_checkpoint_together() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", evidence_bytes());
    let scan = store.create(&source, 0, source.length).unwrap();
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER refuse_checkpoint BEFORE UPDATE ON raw_scans
        BEGIN SELECT RAISE(ABORT, 'simulated disk full'); END;",
        )
        .unwrap();
    assert!(store
        .step(&source, &scan.scan_id)
        .err()
        .unwrap()
        .to_string()
        .contains("simulated disk full"));
    let loaded = store.snapshot(&scan.scan_id).unwrap();
    assert_eq!(loaded.next_offset, 0);
    assert!(loaded.findings.is_empty());
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_checkpoint")
        .unwrap();
    assert!(store.step(&source, &scan.scan_id).is_ok());
}

#[test]
fn disconnect_during_a_batch_keeps_the_previous_checkpoint() {
    let database = Database::new();
    let mut store = database.open();
    let memory = Arc::new(MemorySource::new(vec![0; 256 * 1024]));
    let source = RawDisk::with_source(Path::new("image"), memory.clone(), 256 * 1024).unwrap();
    let scan = store.create(&source, 0, source.length).unwrap();
    memory.disconnect_after_reads(1);
    assert!(store.step(&source, &scan.scan_id).is_err());
    assert_eq!(store.snapshot(&scan.scan_id).unwrap().next_offset, 0);
}

#[test]
fn hashes_are_sha256_and_identifiers_are_unique() {
    assert_eq!(
        hash(b"abc"),
        "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
    );
    let first = identifier().unwrap();
    assert_eq!(first.len(), 32);
    assert!(first.chars().all(|character| character.is_ascii_hexdigit()));
    assert_ne!(first, identifier().unwrap());
}

#[test]
fn interrupted_replies_can_be_reconciled_without_rescanning_or_changing_identity() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", evidence_bytes());
    let scan = store.create(&source, 0, source.length).unwrap();
    let first = store.step(&source, &scan.scan_id).unwrap();
    let status = store.current(&source, &scan.scan_id).unwrap();
    assert_eq!(status.next_offset, first.batch.next_offset);
    assert_eq!(status.findings.len(), first.batch.findings.len());
    assert_eq!(status.scan_id, scan.scan_id);
    let next = store.step(&source, &scan.scan_id).unwrap();
    assert!(next.batch.next_offset > status.next_offset);
    assert_eq!(
        store
            .current(&source, &scan.scan_id)
            .unwrap()
            .findings
            .len(),
        first.batch.findings.len() + next.batch.findings.len()
    );
}

#[test]
fn retrying_failures_does_not_duplicate_previously_found_structures() {
    let database = Database::new();
    let mut store = database.open();
    let bytes = evidence_bytes();
    let failing = Arc::new(BadRegion {
        source: MemorySource::new(bytes.clone()),
        fail: AtomicBool::new(true),
    });
    let source = RawDisk::with_source(Path::new("broken"), failing, bytes.len() as u64).unwrap();
    let scan = store.create(&source, 0, source.length).unwrap();
    while store.current(&source, &scan.scan_id).unwrap().next_offset < source.length {
        store.step(&source, &scan.scan_id).unwrap();
    }
    assert_eq!(
        store.current(&source, &scan.scan_id).unwrap().errors.len(),
        1
    );
    let clone = disk("clone.img", bytes);
    store.resume(&clone, &scan.scan_id, true).unwrap();
    while store.current(&clone, &scan.scan_id).unwrap().next_offset < clone.length {
        store.step(&clone, &scan.scan_id).unwrap();
    }
    let status = store.current(&clone, &scan.scan_id).unwrap();
    assert_eq!(status.findings.len(), 1024);
    assert!(status.errors.is_empty());
    assert_eq!(
        store
            .resume(&clone, &scan.scan_id, true)
            .unwrap()
            .next_offset,
        clone.length
    );
}
