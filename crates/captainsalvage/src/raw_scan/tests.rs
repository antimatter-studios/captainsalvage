use super::markers::{hash, identifies};
use super::*;
use crate::source::{BlockSource, MemorySource};
use std::collections::{BTreeSet, HashMap};
use std::io;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

const SAMPLE_BYTES: u64 = MARKER_BYTES;

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

/// Bytes that tell one drive from another: every block is different, and a different seed gives
/// a different drive of the same size.
fn patterned(size: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

fn evidence_bytes() -> Vec<u8> {
    let mut data = vec![0; 512 * 1024];
    for (index, sector) in data.chunks_exact_mut(512).enumerate() {
        sector[100..104].copy_from_slice(&(index as u32).to_le_bytes());
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

/// Fails marker-sized reads at chosen offsets, and counts every marker-sized read.
struct FailingBlocks {
    source: MemorySource,
    failing: Mutex<BTreeSet<u64>>,
    reads: Mutex<HashMap<u64, u32>>,
}

impl FailingBlocks {
    fn new(data: Vec<u8>, failing: impl IntoIterator<Item = u64>) -> Arc<Self> {
        Arc::new(Self {
            source: MemorySource::new(data),
            failing: Mutex::new(failing.into_iter().collect()),
            reads: Mutex::new(HashMap::new()),
        })
    }
}

impl BlockSource for FailingBlocks {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        if buffer.len() == MARKER_BYTES as usize {
            *self.reads.lock().unwrap().entry(offset).or_default() += 1;
            if self.failing.lock().unwrap().contains(&offset) {
                return Err(io::Error::from_raw_os_error(libc::EIO));
            }
        }
        self.source.read_exact_at(offset, buffer)
    }

    fn read_stats(&self) -> crate::source::ReadStats {
        self.source.read_stats()
    }
}

fn failing_disk(
    data: Vec<u8>,
    failing: impl IntoIterator<Item = u64>,
) -> (RawDisk, Arc<FailingBlocks>) {
    let length = data.len() as u64;
    let source = FailingBlocks::new(data, failing);
    (
        RawDisk::with_source(Path::new("degraded"), source.clone(), length).unwrap(),
        source,
    )
}

fn offsets(fingerprint: &Fingerprint) -> Vec<u64> {
    fingerprint
        .samples
        .iter()
        .map(|sample| sample.offset)
        .collect()
}

fn compare(saved: &Fingerprint, current: &RawDisk) -> Comparison {
    saved.compare(&mut MarkerReader::new(current))
}

/// Overwrites the whole 4 KiB block at `offset`, as a mount rewriting metadata would.
fn overwrite(data: &mut [u8], offset: u64) {
    data[offset as usize..offset as usize + 4096].fill(0xA5);
    data[offset as usize + 7] = 0x11;
    data[offset as usize + 4000] = 0x22;
}

const MEBIBYTE: usize = 1024 * 1024;

#[test]
fn fingerprint_matches_clones_and_images_independent_of_path() {
    let data = patterned(4 * MEBIBYTE, 1);
    let first = disk("/dev/rdisk4", data.clone());
    let clone = disk("copy.img", data);
    let saved = Fingerprint::read(&first);
    let result = compare(&saved, &clone);
    assert_eq!(result.state, Verification::Exact);
    assert_eq!(
        (
            result.total,
            result.matched,
            result.changed,
            result.unreadable
        ),
        (20, 20, 0, 0)
    );
    assert_eq!(result.confidence_percent, 100);
    assert_ne!(first.id, clone.id);
    assert_eq!(saved.version, 2);
    assert_eq!(saved.samples.len(), MARKER_COUNT as usize);
    assert!(!saved.is_short());
}

#[test]
fn markers_are_spread_over_the_source_with_the_first_and_last_blocks_among_them() {
    let source = disk("image", patterned(40 * MEBIBYTE, 2));
    let saved = Fingerprint::read(&source);
    let found = offsets(&saved);
    assert_eq!(found.len(), 20);
    assert_eq!(found[0], 0);
    assert_eq!(*found.last().unwrap(), source.length - MARKER_BYTES);
    assert!(found
        .windows(2)
        .all(|pair| pair[1] - pair[0] >= MARKER_BYTES));
    // Not concentrated at the start: a mount rewriting the first 1 MiB cannot reach most markers.
    assert!(
        found
            .iter()
            .filter(|offset| **offset < MEBIBYTE as u64)
            .count()
            <= 1
    );
    assert!(
        found
            .iter()
            .filter(|offset| **offset > source.length / 2)
            .count()
            >= 9
    );
    assert!(saved
        .samples
        .iter()
        .all(|sample| sample.length == MARKER_BYTES && sample.sha256.is_some()));
    // The same drive always gives the same offsets.
    let again = Fingerprint::read(&disk("again.img", patterned(40 * MEBIBYTE, 2)));
    assert_eq!(offsets(&again), found);
    assert_eq!(again.sampled_sha256, saved.sampled_sha256);
}

#[test]
fn tiny_and_unaligned_sources_have_bounded_unique_markers_and_say_so() {
    for size in [16, 511, 4096, 4097, 5000, 65539, 100_000] {
        let source = disk("small.img", patterned(size, 3));
        let fingerprint = Fingerprint::read(&source);
        assert!(!fingerprint.samples.is_empty(), "size {size}");
        assert!(fingerprint.sampled_sha256.is_some());
        assert!(fingerprint
            .samples
            .iter()
            .all(|sample| sample.offset + sample.length <= size as u64));
        assert!(fingerprint
            .samples
            .windows(2)
            .all(|pair| pair[0].offset + pair[0].length <= pair[1].offset));
        assert_eq!(fingerprint.markers_wanted, 20);
        assert_eq!(
            fingerprint.is_short(),
            fingerprint.samples.len() < 20,
            "size {size}"
        );
    }
    let small = Fingerprint::read(&disk("small.img", patterned(4097, 3)));
    assert!(small.is_short());
    let result = compare(&small, &disk("same.img", patterned(4097, 3)));
    assert_eq!(result.state, Verification::Exact);
    assert!(result
        .summary
        .contains("Markers used: 1 instead of 20, because the source is small or mostly blank."));
}

#[test]
fn blank_and_repeating_blocks_are_not_markers() {
    assert!(!identifies(&[0; 4096]));
    assert!(!identifies(&[0xFF; 4096]));
    assert!(!identifies(&[0x5A; 4096]));
    assert!(!identifies(&b"ab".repeat(2048)));
    assert!(!identifies(
        &(0..4096).map(|index| (index % 7) as u8).collect::<Vec<_>>()
    ));
    assert!(!identifies(
        &(0..4096)
            .map(|index| (index % 64) as u8)
            .collect::<Vec<_>>()
    ));
    assert!(!identifies(&[1, 2, 3]));
    assert!(identifies(&patterned(4096, 4)));
    assert!(identifies(
        &(0..4096)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>()
    ));
    // Almost blank, but carrying something: still identifies.
    let mut nearly = vec![0; 4096];
    nearly[2000..2008].copy_from_slice(b"SALVAGE!");
    assert!(identifies(&nearly));

    // Blank spans are skipped, a nearby block is used instead, and the choice is repeatable.
    let mut data = patterned(8 * MEBIBYTE, 5);
    let plain = offsets(&Fingerprint::read(&disk("plain", data.clone())));
    for offset in [0usize, 2 * MEBIBYTE, 6 * MEBIBYTE] {
        data[offset..offset + 3 * 4096].fill(0);
    }
    let blank_hash = hash(&[0; 4096]);
    let source = disk("holes", data.clone());
    let saved = Fingerprint::read(&source);
    let chosen = offsets(&saved);
    assert_eq!(chosen.len(), 20);
    assert_ne!(chosen, plain);
    assert!(saved
        .samples
        .iter()
        .all(|sample| sample.sha256.as_deref() != Some(&blank_hash)));
    for offset in [0usize, 2 * MEBIBYTE, 6 * MEBIBYTE] {
        assert!(!chosen.contains(&(offset as u64)));
    }
    assert_eq!(chosen, offsets(&Fingerprint::read(&disk("again", data))));
    // Substitutes stay near: the replacement for the first block is within the first 64 KiB.
    assert!(chosen[0] > 0 && chosen[0] <= 64 * 1024);
}

#[test]
fn two_unrelated_blank_drives_do_not_match() {
    let database = Database::new();
    let mut store = database.open();
    let first = disk("blank-a.img", vec![0; 2 * MEBIBYTE]);
    let second = disk("blank-b.img", vec![0; 2 * MEBIBYTE]);
    let saved = Fingerprint::read(&first);
    assert!(saved.samples.is_empty());
    assert!(saved.sampled_sha256.is_none());
    let result = compare(&saved, &second);
    assert_eq!(result.state, Verification::Different);
    assert_eq!(result.confidence_percent, 0);
    let scan = store.create(&first, 0, first.length).unwrap();
    assert!(store.list_matching(&second, true).unwrap().scans.is_empty());
    assert!(store.resume(&second, &scan.scan_id, true, true).is_err());
}

#[test]
fn a_different_drive_of_the_same_size_does_not_match() {
    let database = Database::new();
    let mut store = database.open();
    let drive = disk("/dev/rdisk4", patterned(4 * MEBIBYTE, 6));
    let other = disk("/dev/rdisk5", patterned(4 * MEBIBYTE, 7));
    let scan = store.create(&drive, 0, drive.length).unwrap();
    let result = compare(&Fingerprint::read(&drive), &other);
    assert_eq!(result.state, Verification::Different);
    assert_eq!(result.matched, 0);
    assert_eq!(result.changed, 20);
    assert!(store.list_matching(&other, true).unwrap().scans.is_empty());
    assert!(store.resume(&other, &scan.scan_id, true, true).is_err());
}

#[test]
fn a_mount_rewriting_metadata_near_the_start_is_still_the_same_drive() {
    let data = patterned(8 * MEBIBYTE, 8);
    let saved = Fingerprint::read(&disk("/dev/rdisk4", data.clone()));
    let found = offsets(&saved);

    let mut mounted = data.clone();
    overwrite(&mut mounted, found[0]);
    let result = compare(&saved, &disk("/dev/rdisk4", mounted.clone()));
    assert_eq!(result.state, Verification::Probable);
    assert_eq!(
        (
            result.total,
            result.matched,
            result.changed,
            result.unreadable
        ),
        (20, 19, 1, 0)
    );
    assert_eq!(result.confidence_percent, 95);
    assert_eq!(result.changed_offsets, vec![found[0]]);
    assert!(result.summary.starts_with("1 of 20 markers changed."));
    assert!(result.summary.contains("within the first 1 MiB"));

    // Finder data in the first 1 MiB, in two places: two markers, or one if only one is a marker.
    overwrite(&mut mounted, found[1].min(8 * MEBIBYTE as u64 - 4096));
    let result = compare(&saved, &disk("/dev/rdisk4", mounted));
    assert_eq!(result.state, Verification::Probable);
    assert_eq!((result.matched, result.changed), (18, 2));
    assert_eq!(result.confidence_percent, 90);
    assert_eq!(result.changed_offsets, vec![found[0], found[1]]);
}

#[test]
fn changes_beyond_the_start_are_described_as_such_and_mixed_changes_are_split() {
    let data = patterned(8 * MEBIBYTE, 9);
    let saved = Fingerprint::read(&disk("a", data.clone()));
    let found = offsets(&saved);
    let mut later = data.clone();
    overwrite(&mut later, found[10]);
    let result = compare(&saved, &disk("b", later.clone()));
    assert_eq!(result.state, Verification::Probable);
    assert!(result.summary.contains("further into the drive"));
    overwrite(&mut later, found[0]);
    let result = compare(&saved, &disk("c", later));
    assert!(result.summary.starts_with("2 of 20 markers changed."));
    assert!(result
        .summary
        .contains("Of the changed markers, 1 are within the first 1 MiB"));
}

#[test]
fn an_unreadable_marker_is_unreadable_not_changed() {
    let data = patterned(8 * MEBIBYTE, 10);
    let saved = Fingerprint::read(&disk("a", data.clone()));
    let found = offsets(&saved);
    let (degraded, _) = failing_disk(data, [found[3]]);
    let result = compare(&saved, &degraded);
    assert_eq!(
        (
            result.total,
            result.matched,
            result.changed,
            result.unreadable
        ),
        (20, 19, 0, 1)
    );
    assert_eq!(result.unreadable_offsets, vec![found[3]]);
    assert!(result.changed_offsets.is_empty());
    assert_eq!(result.confidence_percent, 95);
    // Not every marker is known to match, so this is not exact.
    assert_eq!(result.state, Verification::Probable);
    assert!(result
        .summary
        .contains("Of the 20 markers, 1 could not be read."));
    assert!(!result.summary.contains("changed"));
}

#[test]
fn too_few_readable_markers_cannot_match() {
    let data = patterned(8 * MEBIBYTE, 11);
    let saved = Fingerprint::read(&disk("a", data.clone()));
    let found = offsets(&saved);
    // 13 of 20 unreadable leaves 7 matches: under the minimum of 8. They are not all in a row,
    // so reading goes on past each failure.
    let unreadable = [0, 1, 2, 4, 5, 6, 8, 9, 10, 12, 14, 16, 18];
    let (degraded, _) = failing_disk(data.clone(), unreadable.map(|index| found[index]));
    let result = compare(&saved, &degraded);
    assert_eq!((result.matched, result.unreadable), (7, 13));
    assert_eq!(result.state, Verification::Different);
    assert!(result.summary.contains("Only 7 of 20 markers match"));
    // 12 unreadable leaves exactly 8: weak, but reported.
    let (degraded, _) = failing_disk(data, unreadable[..12].iter().map(|index| found[*index]));
    let result = compare(&saved, &degraded);
    assert_eq!((result.matched, result.unreadable), (8, 12));
    assert_eq!(result.state, Verification::Weak);
}

#[test]
fn a_gone_drive_matches_nothing_and_is_not_read() {
    let data = patterned(MEBIBYTE, 12);
    let saved = Fingerprint::read(&disk("a", data.clone()));
    let memory = Arc::new(MemorySource::new(data));
    memory.disconnect();
    let gone = RawDisk::with_source(Path::new("a"), memory, MEBIBYTE as u64).unwrap();
    let result = compare(&saved, &gone);
    assert_eq!(result.state, Verification::Different);
    assert_eq!(result.unreadable, 20);
    assert_eq!(result.changed, 0);
    assert!(Fingerprint::read(&gone).samples.is_empty());
}

#[test]
fn each_marker_block_is_read_once_per_listing_however_many_scans_are_saved() {
    let database = Database::new();
    let mut store = database.open();
    let data = patterned(4 * MEBIBYTE, 13);
    let (source, counter) = failing_disk(data, []);
    for start in [0, 4096, 8192] {
        store.create(&source, start, 65536).unwrap();
    }
    counter.reads.lock().unwrap().clear();
    let listed = store.list_matching(&source, false).unwrap();
    assert_eq!(listed.scans.len(), 3);
    assert!(counter
        .reads
        .lock()
        .unwrap()
        .values()
        .all(|count| *count == 1));
    assert!(counter.reads.lock().unwrap().len() <= 20);
}

#[test]
fn marker_reads_stop_after_repeated_failures_instead_of_hammering_the_drive() {
    let data = patterned(8 * MEBIBYTE, 14);
    let saved = Fingerprint::read(&disk("a", data.clone()));
    let (degraded, counter) = failing_disk(
        data,
        (0..8 * MEBIBYTE as u64 / 4096).map(|block| block * 4096),
    );
    counter.reads.lock().unwrap().clear();
    let result = compare(&saved, &degraded);
    assert_eq!(result.unreadable, 20);
    assert_eq!(result.state, Verification::Different);
    let attempted: u32 = counter.reads.lock().unwrap().values().sum();
    assert!(attempted <= 5, "{attempted} reads on a failing drive");
    // Choosing markers on a failing drive is just as bounded, and says it found none.
    counter.reads.lock().unwrap().clear();
    let fresh = Fingerprint::read(&degraded);
    assert!(fresh.samples.is_empty());
    assert!(counter.reads.lock().unwrap().values().sum::<u32>() <= 5);
}

#[test]
fn length_mismatch_is_a_different_scope_not_a_changed_drive() {
    let data = patterned(4 * MEBIBYTE, 15);
    let saved = Fingerprint::read(&disk("a", data.clone()));
    let mut longer = data.clone();
    longer.extend_from_slice(&patterned(4096, 16));
    let result = compare(&saved, &disk("b", longer));
    assert_eq!(result.state, Verification::Different);
    assert_eq!(
        (result.matched, result.changed, result.unreadable),
        (0, 0, 0)
    );
    assert_eq!(result.summary, "The source has a different size.");
    let database = Database::new();
    let mut store = database.open();
    let scan = store
        .create(&disk("a", data), 0, 4 * MEBIBYTE as u64)
        .unwrap();
    let longer = disk("b", patterned(4 * MEBIBYTE + 4096, 15));
    assert!(store.list_matching(&longer, true).unwrap().scans.is_empty());
    assert!(store.resume(&longer, &scan.scan_id, true, true).is_err());
}

/// What the previous version stored: sixteen samples, a combined hash and no `markersWanted`.
fn version_one(data: &[u8]) -> Fingerprint {
    let length = data.len() as u64;
    let last = length - 4096;
    let samples: Vec<Sample> = (0..16u64)
        .map(|index| {
            let offset = if index == 15 {
                last
            } else {
                (u128::from(last) * u128::from(index) / 15) as u64 / 4096 * 4096
            };
            Sample {
                offset,
                length: 4096,
                sha256: Some(hash(&data[offset as usize..offset as usize + 4096])),
            }
        })
        .collect();
    let json = serde_json::json!({
        "version": 1, "length": length, "samples": samples, "sampledSha256": "ab",
    });
    serde_json::from_value(json).unwrap()
}

#[test]
fn version_one_fingerprints_compare_marker_by_marker_over_their_own_count() {
    let data = patterned(4 * MEBIBYTE, 17);
    let saved = version_one(&data);
    assert_eq!(saved.version, 1);
    assert_eq!(saved.samples.len(), 16);
    assert_eq!(saved.markers_wanted, 0);
    assert!(!saved.is_short());
    let result = compare(&saved, &disk("clone", data.clone()));
    assert_eq!(
        (result.state, result.total, result.matched),
        (Verification::Exact, 16, 16)
    );

    let mut changed = data.clone();
    overwrite(&mut changed, saved.samples[0].offset);
    let result = compare(&saved, &disk("mounted", changed.clone()));
    assert_eq!(result.state, Verification::Probable);
    assert_eq!((result.total, result.matched, result.changed), (16, 15, 1));
    assert_eq!(result.confidence_percent, 93);

    // 9 of 16 is 56 percent: weak. 10 of 16 is 62 percent: probable.
    for (altered, state) in [(7usize, Verification::Weak), (6, Verification::Probable)] {
        let mut data = data.clone();
        for sample in &saved.samples[..altered] {
            overwrite(&mut data, sample.offset);
        }
        assert_eq!(
            compare(&saved, &disk("x", data)).state,
            state,
            "{altered} changed"
        );
    }
    // A fingerprint with an unreadable saved sample counts it as unreadable.
    let mut partial = saved.clone();
    partial.samples[2].sha256 = None;
    let result = compare(&partial, &disk("clone", data));
    assert_eq!((result.matched, result.unreadable), (15, 1));
}

#[test]
fn a_saved_version_one_scan_is_listed_and_resumes() {
    let database = Database::new();
    let mut store = database.open();
    let data = patterned(2 * MEBIBYTE, 18);
    let source = disk("/dev/rdisk4", data.clone());
    let scan = store.create(&source, 0, source.length).unwrap();
    store
        .connection
        .execute(
            "UPDATE raw_scans SET fingerprint = ?1",
            [serde_json::to_string(&version_one(&data)).unwrap()],
        )
        .unwrap();
    let clone = disk("clone.img", data);
    let listed = store.list(&clone).unwrap();
    assert_eq!(listed.scans[0].verification, Verification::Exact);
    assert_eq!(listed.scans[0].comparison.total, 16);
    assert_eq!(
        store
            .resume(&clone, &scan.scan_id, true, false)
            .unwrap()
            .scan_id,
        scan.scan_id
    );
}

#[test]
fn unknown_fingerprint_versions_are_incompatible() {
    let data = patterned(MEBIBYTE, 19);
    let mut saved = Fingerprint::read(&disk("a", data.clone()));
    saved.version = 3;
    let result = compare(&saved, &disk("b", data));
    assert_eq!(result.state, Verification::Incompatible);
}

#[test]
fn a_sampled_match_does_not_claim_unsampled_bytes_are_identical() {
    let data = patterned(MEBIBYTE, 20);
    let first = Fingerprint::read(&disk("disk", data.clone()));
    let unsampled = (0..MEBIBYTE as u64 / 4096)
        .map(|block| block * 4096 + 1)
        .find(|offset| {
            !first
                .samples
                .iter()
                .any(|sample| (sample.offset..sample.offset + 4096).contains(offset))
        })
        .unwrap();
    let mut changed = data;
    changed[unsampled as usize] ^= 1;
    assert_eq!(
        compare(&first, &disk("disk", changed)).state,
        Verification::Exact
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
    let loaded = reopened.resume(&clone, &scan.scan_id, true, false).unwrap();
    assert_eq!(loaded.scan_id, scan.scan_id);
    assert_eq!(loaded.next_offset, step.batch.next_offset);
    assert_eq!(loaded.findings.len(), count);
    assert_eq!(
        reopened.current(&clone, &scan.scan_id).unwrap().next_offset,
        loaded.next_offset
    );
    let listed = reopened.list(&clone).unwrap();
    assert_eq!(listed.scans[0].verification, Verification::Exact);
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
    let source = disk("image", patterned(128 * 1024, 21));
    let first = store.create(&source, 0, 65536).unwrap();
    let second = store.create(&source, 65536, 32768).unwrap();
    assert_ne!(first.scan_id, second.scan_id);
    store.step(&source, &second.scan_id).unwrap();
    assert_eq!(store.snapshot(&first.scan_id).unwrap().next_offset, 0);
    let scans = store.list(&source).unwrap().scans;
    assert_eq!(scans.len(), 2);
    assert!(scans
        .iter()
        .all(|scan| scan.verification == Verification::Exact));
}

#[test]
fn invalid_ranges_and_unknown_scan_ids_are_rejected() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", vec![0; 4096]);
    for (start, length) in [(0, 0), (4096, 1), (4095, 2), (u64::MAX, 2)] {
        assert!(store.create(&source, start, length).is_err());
    }
    assert!(store.resume(&source, "absent", true, true).is_err());
    assert!(store.step(&source, "absent").is_err());
    assert!(store.list(&source).unwrap().scans.is_empty());
}

#[test]
fn resume_requires_acknowledgement_and_matching_current_content() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("/dev/rdisk4", patterned(8192 * 4, 22));
    let scan = store.create(&source, 0, source.length).unwrap();
    assert!(store.resume(&source, &scan.scan_id, false, false).is_err());
    assert!(store.resume(&source, &scan.scan_id, false, true).is_err());
    for (size, seed) in [(8192 * 4, 23), (8192 * 8, 22)] {
        let replacement = disk("/dev/rdisk4", patterned(size, seed));
        assert!(store
            .resume(&replacement, &scan.scan_id, true, true)
            .is_err());
        assert!(store.step(&replacement, &scan.scan_id).is_err());
        assert!(store.list(&replacement).unwrap().scans.is_empty());
    }
    let unrelated = disk("other.img", patterned(8192 * 4, 24));
    assert!(store
        .list_matching(&unrelated, true)
        .unwrap()
        .scans
        .is_empty());
    assert_eq!(store.snapshot(&scan.scan_id).unwrap().next_offset, 0);
    // An exact match resumes without acknowledging changed markers: there are none.
    assert!(store.resume(&source, &scan.scan_id, true, false).is_ok());
    assert!(store
        .snapshot(&scan.scan_id)
        .unwrap()
        .changed_source
        .is_none());
}

/// A saved scan of a 4 MiB drive, and a function that gives the drive with some markers rewritten.
fn saved_drive(store: &mut ScanStore) -> (Vec<u8>, Vec<u64>, String) {
    let data = patterned(4 * MEBIBYTE, 25);
    let source = disk("/dev/rdisk4", data.clone());
    let scan = store.create(&source, 0, source.length).unwrap();
    let found = offsets(&Fingerprint::read(&source));
    (data, found, scan.scan_id)
}

fn with_rewritten(data: &[u8], found: &[u64], count: usize) -> RawDisk {
    let mut changed = data.to_vec();
    for offset in &found[..count] {
        overwrite(&mut changed, *offset);
    }
    disk("/dev/rdisk4", changed)
}

#[test]
fn a_probable_match_needs_its_own_acknowledgement_and_records_the_change() {
    let database = Database::new();
    let mut store = database.open();
    let (data, found, id) = saved_drive(&mut store);
    let mounted = with_rewritten(&data, &found, 2);

    let listed = store.list(&mounted).unwrap();
    assert_eq!(listed.scans.len(), 1);
    assert_eq!(listed.scans[0].verification, Verification::Probable);
    assert_eq!(listed.scans[0].comparison.confidence_percent, 90);
    assert!(listed.scans[0].changed_source.is_none());

    let refused = store
        .resume(&mounted, &id, true, false)
        .unwrap_err()
        .to_string();
    assert!(refused.contains("probable match") && refused.contains("2 of 20 markers changed"));
    assert!(store.current(&mounted, &id).is_err());
    assert!(store.snapshot(&id).unwrap().changed_source.is_none());

    let resumed = store.resume(&mounted, &id, true, true).unwrap();
    let changed = resumed
        .changed_source
        .expect("the scan records the changed source");
    assert_eq!(changed.resumes, 1);
    assert_eq!(changed.changed_offsets, vec![found[0], found[1]]);
    assert_eq!(
        (
            changed.matched,
            changed.changed,
            changed.unreadable,
            changed.total
        ),
        (18, 2, 0, 20)
    );
    assert_eq!(changed.confidence_percent, 90);
    assert!(changed.summary.contains("within the first 1 MiB"));
    assert!(store.current(&mounted, &id).is_ok());
    let listed = store.list(&mounted).unwrap();
    assert_eq!(listed.scans[0].changed_source.as_ref().unwrap().resumes, 1);

    // A later resume on a drive that changed elsewhere adds to the record.
    let later = with_rewritten(&data, &found[10..], 1);
    let resumed = store.resume(&later, &id, true, true).unwrap();
    let changed = resumed.changed_source.unwrap();
    assert_eq!(changed.resumes, 2);
    assert_eq!(changed.changed_offsets, vec![found[0], found[1], found[10]]);
    assert_eq!(changed.changed, 1);
}

#[test]
fn weak_and_different_sources_never_resume_and_show_all_reveals_only_weak_ones() {
    let database = Database::new();
    let mut store = database.open();
    let (data, found, id) = saved_drive(&mut store);

    // 9 changed: 11 of 20 match (55 percent), enough to show, not enough to resume.
    let weak = with_rewritten(&data, &found, 9);
    let listed = store.list(&weak).unwrap();
    assert!(listed.scans.is_empty());
    assert_eq!(listed.hidden, 1);
    let listed = store.list_matching(&weak, true).unwrap();
    assert_eq!(listed.hidden, 0);
    assert_eq!(listed.scans.len(), 1);
    assert_eq!(listed.scans[0].verification, Verification::Weak);
    assert_eq!(listed.scans[0].comparison.confidence_percent, 55);
    assert!(store.resume(&weak, &id, true, true).is_err());
    assert!(store.step(&weak, &id).is_err());

    // 8 changed: 12 of 20 (60 percent) is probable and shown without show_all.
    let edge = with_rewritten(&data, &found, 8);
    let listed = store.list(&edge).unwrap();
    assert_eq!(listed.scans[0].verification, Verification::Probable);
    assert_eq!(listed.scans[0].comparison.confidence_percent, 60);

    // 13 changed: 7 of 20 match, under the minimum: a different drive, never listed.
    let different = with_rewritten(&data, &found, 13);
    for show_all in [false, true] {
        let listed = store.list_matching(&different, show_all).unwrap();
        assert!(listed.scans.is_empty());
        assert_eq!(listed.hidden, 0);
    }
    assert!(store.resume(&different, &id, true, true).is_err());
    assert_eq!(store.snapshot(&id).unwrap().next_offset, 0);
}

#[test]
fn unreadable_markers_never_authorise_resume_without_enough_readable_ones() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", patterned(8192 * 4, 26));
    let scan = store.create(&source, 0, source.length).unwrap();
    let memory = Arc::new(MemorySource::new(patterned(8192 * 4, 26)));
    memory.disconnect();
    let gone = RawDisk::with_source(Path::new("image"), memory, 8192 * 4).unwrap();
    assert!(Fingerprint::read(&gone).sampled_sha256.is_none());
    assert!(store.list_matching(&gone, true).unwrap().scans.is_empty());
    assert!(store.resume(&gone, &scan.scan_id, true, true).is_err());
    assert!(store.create(&gone, 0, 8192).is_err());
}

#[test]
fn incompatible_scanner_versions_are_listed_but_cannot_resume() {
    let database = Database::new();
    let mut store = database.open();
    let source = disk("image", patterned(64 * 1024, 27));
    let scan = store.create(&source, 0, 4096).unwrap();
    store
        .connection
        .execute("UPDATE raw_scans SET scanner_version = 99", [])
        .unwrap();
    assert_eq!(
        store.list(&source).unwrap().scans[0].verification,
        Verification::Incompatible
    );
    assert!(store.resume(&source, &scan.scan_id, true, true).is_err());
}

#[test]
fn read_errors_belong_to_the_source_and_are_retried_on_a_clone() {
    let database = Database::new();
    let mut store = database.open();
    let bytes = patterned(256 * 1024, 28);
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
    let loaded = store.resume(&clone, &scan.scan_id, true, false).unwrap();
    assert_eq!(loaded.next_offset, 65536);
    assert!(loaded.errors.is_empty());
    assert!(store
        .step(&clone, &scan.scan_id)
        .unwrap()
        .batch
        .errors
        .is_empty());
    let loaded = store.resume(&clone, &scan.scan_id, true, false).unwrap();
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
    store.resume(&clone, &scan.scan_id, true, false).unwrap();
    while store.current(&clone, &scan.scan_id).unwrap().next_offset < clone.length {
        store.step(&clone, &scan.scan_id).unwrap();
    }
    let status = store.current(&clone, &scan.scan_id).unwrap();
    assert_eq!(status.findings.len(), 1024);
    assert!(status.errors.is_empty());
    assert_eq!(
        store
            .resume(&clone, &scan.scan_id, true, false)
            .unwrap()
            .next_offset,
        clone.length
    );
}

#[test]
fn resuming_on_the_same_damaged_drive_continues_from_the_checkpoint_not_from_its_first_failure() {
    let database = Database::new();
    let mut store = database.open();
    let bytes = patterned(48 * 1024 * 1024, 31);
    let open = || {
        let failing = Arc::new(BadRegion {
            source: MemorySource::new(bytes.clone()),
            fail: AtomicBool::new(true),
        });
        RawDisk::with_source(Path::new("damaged"), failing, bytes.len() as u64).unwrap()
    };
    let first = open();
    let scan = store.create(&first, 0, first.length).unwrap();
    store.step(&first, &scan.scan_id).unwrap();
    let reached = store.step(&first, &scan.scan_id).unwrap().batch.next_offset;
    assert!(reached > 65536 * 2);
    drop(store);
    // The drive is opened again and still cannot read the same block: it is the same drive.
    let second = open();
    let mut store = database.open();
    let loaded = store.resume(&second, &scan.scan_id, true, false).unwrap();
    assert_eq!(loaded.next_offset, reached);
    assert_eq!(loaded.errors.len(), 1, "the unreadable block stays listed");
}
