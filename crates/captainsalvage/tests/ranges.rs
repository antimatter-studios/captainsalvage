//! The typed ranges (issue #212, first slice) that the FAT, exFAT and NTFS adapters fill for every
//! entry they describe: they must be a valid description of a stream of the entry's size, and agree
//! with the cluster lists the engine reads. Hand-built volumes; nothing reads the ranges but this.

use captainsalvage::filesystem::{FilesystemKind, SalvageEntry, STORAGE_RESIDENT, STORAGE_RUNS};
use captainsalvage::ranges::{Provenance, RangeKind, RangeList};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use fs_exfat::fixtures::{
    bitmap_entry, entry_set, seal_entry_set, upcase_entry, Builder, BITMAP_CLUSTER, CLUSTER,
    ROOT_CLUSTER,
};
use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod fat_support;
mod ntfs_support;

static NEXT: AtomicU64 = AtomicU64::new(0);

fn open(image: Vec<u8>) -> Arc<Session> {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-ranges-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    Session::with_source(
        Path::new("/test/ranges"),
        Arc::new(MemorySource::new(image)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap()
}

/// Every entry the adapter finds anywhere in the volume, as the scan finds them: cluster by
/// cluster, and the fixed root of FAT12 and FAT16.
fn entries(session: &Session) -> Vec<SalvageEntry> {
    let volume = &session.volume;
    let size = volume.geometry.cluster_size();
    let mut found: Vec<SalvageEntry> = volume
        .fixed_root_entries()
        .into_iter()
        .map(|(_, entry)| entry)
        .collect();
    for cluster in 2..volume.geometry.last_cluster_exclusive() {
        let start = volume.cluster_offset(cluster);
        let want = (size + 1024).min(volume.geometry.volume_size.saturating_sub(start)) as usize;
        let Ok(data) = volume.read_bytes(start, want) else {
            continue;
        };
        found.extend(volume.find_sets(&data, size as usize));
    }
    found
}

/// The clusters a list's readable ranges sit on, in stream order.
fn clusters_of(list: &RangeList, session: &Session) -> Vec<u32> {
    let size = session.volume.geometry.cluster_size();
    let heap = session.volume.cluster_offset(2);
    let mut clusters = Vec::new();
    for range in list.ranges() {
        if let (
            RangeKind::Readable | RangeKind::Shared,
            Some(Provenance::Source { offset, length }),
        ) = (range.kind, range.provenance)
        {
            assert_eq!((offset - heap) % size, 0, "a range starts on a cluster");
            for index in 0..length.div_ceil(size) {
                clusters.push(2 + ((offset - heap) / size + index) as u32);
            }
        }
    }
    clusters
}

/// What a FAT or exFAT entry's ranges must be, given the cluster list the engine would use.
fn agrees_with_the_cluster_list(session: &Session, entry: &SalvageEntry) {
    let volume = &session.volume;
    let list = &entry.ranges;
    // A valid description of a stream of this size.
    RangeList::new(list.ranges().to_vec(), entry.length)
        .unwrap_or_else(|error| panic!("{}: {error}", entry.name));
    assert_eq!(
        &RangeList::new(list.ranges().to_vec(), entry.length).unwrap(),
        list
    );
    let claim = volume.clusters_for(entry.first_cluster, entry.length, entry.no_fat_chain);
    let size = volume.geometry.cluster_size();
    // The clusters the readable bytes sit on are the cluster list's, up to the valid length.
    let valid_clusters = entry.valid_length.min(entry.length).div_ceil(size) as usize;
    let wanted: Vec<u32> = claim
        .clusters
        .iter()
        .copied()
        .take(valid_clusters)
        .collect();
    assert_eq!(clusters_of(list, session), wanted, "{}", entry.name);
    // The bytes the chain does not reach are missing, and the bytes past the valid length are zeros.
    let reached = (claim.clusters.len() as u64 * size).min(entry.length);
    let valid = entry.valid_length.min(entry.length);
    assert_eq!(
        list.bytes_of(RangeKind::Missing),
        valid.saturating_sub(reached),
        "{}",
        entry.name
    );
    assert_eq!(
        list.bytes_of(RangeKind::Sparse),
        entry.length - valid,
        "{}",
        entry.name
    );
    assert_eq!(
        list.bytes_of(RangeKind::Readable),
        reached.min(valid),
        "{}",
        entry.name
    );
}

#[test]
fn fat_ranges_add_up_to_the_size_and_agree_with_the_cluster_lists() {
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let session = open(fat_support::Fixture::new(kind).image);
        let found = entries(&session);
        let named = |name: &str| found.iter().find(|entry| entry.name == name).unwrap();
        for entry in &found {
            agrees_with_the_cluster_list(&session, entry);
        }
        // The fragmented file is two ranges that are not side by side.
        let log = named("Captain log.txt");
        assert_eq!(log.length, 700, "{kind}");
        let places: Vec<(u64, u64)> = log
            .ranges
            .ranges()
            .iter()
            .map(|range| (range.start, range.length))
            .collect();
        assert_eq!(places, [(0, 512), (512, 188)], "{kind}");
        assert!(log
            .ranges
            .ranges()
            .iter()
            .all(|range| range.kind == RangeKind::Readable));
        // The deleted file's chain was cleared: its first cluster is readable and the rest missing.
        let gone = named("_OST.TXT");
        assert_eq!(gone.ranges.bytes_of(RangeKind::Readable), 512, "{kind}");
        assert_eq!(gone.ranges.bytes_of(RangeKind::Missing), 8, "{kind}");
        // A folder is clusters, and an empty file has no ranges.
        assert_eq!(named("ARCHIVE").ranges.size(), 512, "{kind}");
    }
}

/// A volume whose root holds, besides the folder `many`, a file in three clusters that are not
/// side by side, a file whose chain ends after one cluster of three, and a file that has been
/// written for only part of its length.
fn exfat_volume() -> Vec<u8> {
    let mut builder = Builder::new();
    for cluster in [
        BITMAP_CLUSTER,
        ROOT_CLUSTER,
        4,
        5,
        6,
        40,
        41,
        60,
        70,
        71,
        72,
        80,
    ] {
        builder.allocate(cluster);
    }
    builder.fat(ROOT_CLUSTER, FAT_END);
    let boot = BootSector::parse(&builder.image).unwrap();
    let location = bitmap_location(&boot, &bitmap_entry(BITMAP_CLUSTER, u64::MAX)).unwrap();
    let mut partial = entry_set("partial.bin", false, 70, 3 * CLUSTER as u64, true, true);
    // Written for 5000 of its 12288 bytes.
    partial[40..48].copy_from_slice(&5000u64.to_le_bytes());
    seal_entry_set(&mut partial);
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(location.first_cluster, location.required_length),
            upcase_entry(4, &[0; 100]),
            entry_set("many", true, 5, 2 * CLUSTER as u64, false, true),
            entry_set("scattered.bin", false, 40, 3 * CLUSTER as u64, false, true),
            entry_set("broken.bin", false, 80, 3 * CLUSTER as u64, false, true),
            entry_set("contiguous.bin", false, 70, 3 * CLUSTER as u64, true, true),
            partial,
            entry_set("empty.bin", false, 0, 0, false, true),
        ],
    );
    builder.fat(5, 6);
    builder.fat(6, FAT_END);
    builder.fat(40, 60);
    builder.fat(60, 41);
    builder.fat(41, FAT_END);
    builder.fat(80, FAT_END);
    builder.image
}

#[test]
fn exfat_ranges_add_up_to_the_size_and_agree_with_the_cluster_lists() {
    let session = open(exfat_volume());
    let found = entries(&session);
    let names: Vec<&str> = found.iter().map(|entry| entry.name.as_str()).collect();
    let named = |name: &str| {
        found
            .iter()
            .find(|entry| entry.name == name)
            .unwrap_or_else(|| panic!("{name} not found in {names:?}"))
    };
    for entry in &found {
        agrees_with_the_cluster_list(&session, entry);
    }
    let heap = |cluster: u32| session.volume.cluster_offset(cluster);
    let scattered = named("scattered.bin");
    assert_eq!(
        scattered
            .ranges
            .ranges()
            .iter()
            .map(|range| range.provenance)
            .collect::<Vec<_>>(),
        [
            Some(Provenance::Source {
                offset: heap(40),
                length: CLUSTER as u64
            }),
            Some(Provenance::Source {
                offset: heap(60),
                length: CLUSTER as u64
            }),
            Some(Provenance::Source {
                offset: heap(41),
                length: CLUSTER as u64
            }),
        ]
    );
    // A run in one piece is one range, wherever it is.
    let contiguous = named("contiguous.bin");
    assert_eq!(contiguous.ranges.ranges().len(), 1);
    assert_eq!(contiguous.ranges.size(), 3 * CLUSTER as u64);
    // A chain that ends early leaves the rest missing.
    let broken = named("broken.bin");
    assert_eq!(broken.ranges.bytes_of(RangeKind::Readable), CLUSTER as u64);
    assert_eq!(
        broken.ranges.bytes_of(RangeKind::Missing),
        2 * CLUSTER as u64
    );
    // What was never written reads as zeros: the same clusters, written for 5000 bytes.
    let partial = named("partial.bin");
    assert_eq!(partial.valid_length, 5000);
    assert_eq!(partial.ranges.bytes_of(RangeKind::Readable), 5000);
    assert_eq!(
        partial.ranges.bytes_of(RangeKind::Sparse),
        3 * CLUSTER as u64 - 5000
    );
    assert_eq!(named("empty.bin").ranges.size(), 0);
}

#[test]
fn ntfs_ranges_add_up_to_the_size_and_agree_with_the_runs() {
    let fixture = ntfs_support::fixture();
    let session = open(fixture.image.clone());
    assert_eq!(session.volume.kind, FilesystemKind::Ntfs);
    let size = session.volume.geometry.cluster_size();
    let found = entries(&session);
    let mut checked = 0;
    for entry in found.iter().filter(|entry| !entry.is_dir) {
        let ntfs = entry.ntfs.as_ref().unwrap();
        RangeList::new(entry.ranges.ranges().to_vec(), entry.length)
            .unwrap_or_else(|error| panic!("{}: {error}", entry.name));
        let valid = entry.valid_length.min(entry.length);
        assert_eq!(
            entry.ranges.bytes_of(RangeKind::Sparse),
            entry.length - valid,
            "{}",
            entry.name
        );
        match ntfs.storage.as_str() {
            STORAGE_RESIDENT => {
                assert_eq!(
                    entry.ranges.bytes_of(RangeKind::Readable),
                    valid,
                    "{}",
                    entry.name
                );
                if entry.length > 0 {
                    assert_eq!(
                        entry.ranges.ranges()[0].provenance,
                        Some(Provenance::InlineInRecord {
                            record: ntfs.record_number
                        }),
                        "{}",
                        entry.name
                    );
                }
            }
            STORAGE_RUNS => {
                // The readable bytes sit where the runs say, and add up to what the runs cover.
                let covered: u64 = ntfs
                    .runs
                    .iter()
                    .filter(|run| run.lcn.is_some())
                    .map(|run| (run.count * size).min(valid.saturating_sub(run.vcn * size)))
                    .sum();
                assert_eq!(
                    entry.ranges.bytes_of(RangeKind::Readable),
                    covered,
                    "{}",
                    entry.name
                );
                for range in entry.ranges.ranges() {
                    if let (RangeKind::Readable, Some(Provenance::Source { offset, .. })) =
                        (range.kind, range.provenance)
                    {
                        let run = ntfs
                            .runs
                            .iter()
                            .find(|run| {
                                run.vcn * size <= range.start
                                    && range.start < (run.vcn + run.count) * size
                            })
                            .expect("a readable range starts inside a run");
                        let lcn = run.lcn.expect("a readable range is in a run with a place");
                        assert_eq!(
                            offset,
                            lcn * size + (range.start - run.vcn * size),
                            "{}",
                            entry.name
                        );
                    }
                }
            }
            // A record with no data stream this app follows: whatever size it says is missing.
            _ => assert_eq!(
                entry.ranges.bytes_of(RangeKind::Missing),
                entry.length,
                "{}",
                entry.name
            ),
        }
        checked += 1;
    }
    assert!(checked >= 6, "the fixture's files were found: {checked}");
    // The file written in two runs is two ranges, side by side on the file and apart on the volume.
    let frag = found
        .iter()
        .find(|entry| entry.name == "frag.bin" && entry.in_use)
        .unwrap();
    assert_eq!(frag.length, 20_000);
    assert!(frag.ranges.ranges().len() >= 2, "{:?}", frag.ranges);
    assert_eq!(frag.ranges.bytes_of(RangeKind::Readable), 20_000);
    // The resident note is inline in its record.
    let note = found
        .iter()
        .find(|entry| entry.name == "note.txt" && entry.in_use)
        .unwrap();
    assert!(matches!(
        note.ranges.ranges()[0].provenance,
        Some(Provenance::InlineInRecord { .. })
    ));
    assert_eq!(note.ranges.size(), note.length);
}

#[test]
fn the_ranges_are_never_serialised_so_what_scripts_and_tools_report_is_as_it_was() {
    let session = open(fat_support::Fixture::new(FilesystemKind::Fat16).image);
    let found = entries(&session);
    let log = found
        .iter()
        .find(|entry| entry.name == "Captain log.txt")
        .unwrap();
    assert!(!log.ranges.is_empty());
    let value = serde_json::to_value(log).unwrap();
    assert!(value.get("ranges").is_none(), "{value}");
    let keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    assert!(
        keys.contains(&"firstCluster") && keys.contains(&"noFatChain"),
        "{keys:?}"
    );
}

// ---------------------------------------------------------------------------------------------
// The preview reads typed ranges (issue #212, second slice). Until then it read the entry's cluster
// list, and `reference_read` below is that algorithm as it was, kept here so the two can be compared
// byte for byte, and in what they call unreadable, on every entry of every volume.

use captainsalvage::browse::ByteRange;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::source::{BlockSource, ReadStats};
use std::sync::atomic::AtomicBool;
use std::sync::Mutex;

/// A source that reads like the image until a stretch of it is told to have gone bad.
struct Flaky {
    source: MemorySource,
    bad: Mutex<Option<std::ops::Range<u64>>>,
}

impl BlockSource for Flaky {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        if let Some(bad) = self.bad.lock().unwrap().clone() {
            if offset < bad.end && offset + buffer.len() as u64 > bad.start {
                return Err(std::io::Error::from_raw_os_error(libc::EIO));
            }
        }
        self.source.read_exact_at(offset, buffer)
    }

    fn read_stats(&self) -> ReadStats {
        self.source.read_stats()
    }
}

fn scanned(image: Vec<u8>) -> (Arc<Session>, Arc<Flaky>) {
    let source = Arc::new(Flaky {
        source: MemorySource::new(image),
        bad: Mutex::new(None),
    });
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-ranges-read-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        Path::new("/test/ranges-read"),
        source.clone(),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 64 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    (session, source)
}

/// What the preview did before it read ranges: the entry's cluster list, a run of clusters that follow
/// each other read together and, when that fails, one cluster at a time, a cluster that cannot be read
/// standing in as zeros and named, and the bytes past the valid length zeros.
fn reference_read(
    session: &Session,
    id: i64,
    start: u64,
    length: u64,
) -> (u64, Vec<u8>, Vec<ByteRange>) {
    let volume = &session.volume;
    let (row, source) = session
        .with_database(|database| {
            let row = database.entry(id)?.unwrap();
            let source = database.entry_source(volume, &row)?;
            Ok((row, source))
        })
        .unwrap();
    let captainsalvage::map::EntrySource::Clusters(clusters) = source else {
        panic!("a cluster entry");
    };
    let size = volume.geometry.cluster_size();
    let total = row.length.min(clusters.len() as u64 * size);
    let valid = row.valid_length.min(row.length);
    let end = start.saturating_add(length).min(total);
    let mut data = Vec::new();
    let mut unreadable: Vec<ByteRange> = Vec::new();
    if start < end {
        let last = ((end - 1) / size) as usize;
        let mut index = (start / size) as usize;
        let mut position = index as u64 * size;
        while index <= last {
            let mut run = 1;
            while index + run <= last && clusters[index + run] == clusters[index + run - 1] + 1 {
                run += 1;
            }
            let run_clusters = &clusters[index..index + run];
            let mut bytes = Vec::new();
            let mut missing = Vec::new();
            let all_valid = run_clusters
                .iter()
                .all(|&cluster| volume.valid_cluster(cluster));
            match all_valid
                .then(|| {
                    volume
                        .read_bytes(volume.cluster_offset(run_clusters[0]), run * size as usize)
                        .ok()
                })
                .flatten()
            {
                Some(whole) => bytes = whole,
                None => {
                    for (place, &cluster) in run_clusters.iter().enumerate() {
                        match volume
                            .valid_cluster(cluster)
                            .then(|| volume.read_cluster(cluster).ok())
                            .flatten()
                        {
                            Some(one) => bytes.extend_from_slice(&one),
                            None => {
                                missing.push(place);
                                bytes.resize(bytes.len() + size as usize, 0);
                            }
                        }
                    }
                }
            }
            for place in missing {
                let first = position + place as u64 * size;
                captainsalvage::browse::push_range(
                    &mut unreadable,
                    first.max(start),
                    (first + size).min(end).min(valid),
                );
            }
            let from = start.saturating_sub(position) as usize;
            let to = (end - position).min(bytes.len() as u64) as usize;
            data.extend_from_slice(&bytes[from..to]);
            position += run as u64 * size;
            index += run;
        }
        if end > valid {
            let keep = valid.saturating_sub(start) as usize;
            data[keep..].fill(0);
        }
    }
    (total, data, unreadable)
}

fn every_row(session: &Session) -> Vec<i64> {
    session
        .query_map("SELECT id FROM sets ORDER BY id")
        .unwrap()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect()
}

/// Reads at these places of a file, whatever its size: from its start, across and inside clusters,
/// and past its end.
fn grid(size: u64) -> Vec<(u64, u64)> {
    let mut reads = Vec::new();
    for start in [
        0,
        1,
        size - 1,
        size,
        size + 1,
        2 * size,
        3 * size - 1,
        40_000,
    ] {
        for length in [1, 100, size, size + 1, 3 * size, 50_000] {
            reads.push((start, length));
        }
    }
    reads
}

fn same_as_before(session: &Session, label: &str) {
    let size = session.volume.geometry.cluster_size();
    let mut compared = 0;
    for id in every_row(session) {
        for (start, length) in grid(size) {
            let (total, data, unreadable) = reference_read(session, id, start, length);
            let Ok(now) = session.read_entry_range(id, start, length) else {
                panic!("{label}: entry {id} at {start}+{length} could not be read");
            };
            assert_eq!(now.total, total, "{label}: entry {id} total");
            assert_eq!(now.data, data, "{label}: entry {id} bytes {start}+{length}");
            assert_eq!(
                now.unreadable, unreadable,
                "{label}: entry {id} unreadable at {start}+{length}"
            );
            compared += 1;
        }
    }
    assert!(compared > 100, "{label}: only {compared} reads compared");
}

/// Where on the source the `index`th cluster of the volume's first file of at least two clusters is.
fn a_cluster_to_spoil(session: &Session, index: usize) -> std::ops::Range<u64> {
    for id in every_row(session) {
        let clusters = session
            .with_database(|database| {
                let row = database.entry(id)?.unwrap();
                database.entry_source(&session.volume, &row)
            })
            .unwrap();
        if let captainsalvage::map::EntrySource::Clusters(clusters) = clusters {
            if clusters.len() >= 2 {
                let at = session.volume.cluster_offset(clusters[index]);
                return at..at + session.volume.geometry.cluster_size();
            }
        }
    }
    panic!("no file of two clusters");
}

#[test]
fn the_preview_reads_the_same_bytes_and_names_the_same_unreadable_stretches_as_before_on_fat_and_exfat(
) {
    let mut volumes: Vec<(String, Vec<u8>)> = [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ]
    .into_iter()
    .map(|kind| (kind.to_string(), fat_support::Fixture::new(kind).image))
    .collect();
    volumes.push(("exFAT".into(), exfat_volume()));
    for (label, image) in volumes {
        let (session, source) = scanned(image);
        same_as_before(&session, &format!("{label}, nothing bad"));
        // A cluster of a file goes bad after the scan: both name it, and read the rest the same.
        for index in [0, 1] {
            *source.bad.lock().unwrap() = Some(a_cluster_to_spoil(&session, index));
            same_as_before(&session, &format!("{label}, bad cluster {index} of a file"));
        }
        let named = every_row(&session)
            .into_iter()
            .filter(|id| {
                session
                    .read_entry_range(*id, 0, 1 << 20)
                    .map(|read| !read.unreadable.is_empty())
                    .unwrap_or(false)
            })
            .count();
        assert!(
            named > 0,
            "{label}: the bad cluster was never named, so the comparison tested nothing"
        );
    }
}
