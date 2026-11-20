//! A copy of an exFAT file whose valid length is shorter than its length. The bytes past the valid
//! length are zeros by definition, so no read of the disk is made for them: a cluster there that
//! cannot be read costs nothing, and the copy is complete. Hand-built images from the driver's
//! fixtures. The source is made to fail after the scan, so the volume itself is never changed.

use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::export::CopyRequest;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::{BlockSource, MemorySource, ReadStats};
use captainsalvage::workspace::Workspace;
use fs_exfat::fixtures::{
    bitmap_entry, entry_set, noise, seal_entry_set, upcase_entry, Builder, BITMAP_CLUSTER, CLUSTER,
    ROOT_CLUSTER,
};
use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};
use serde_json::{json, Value};
use std::ops::Range;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};

const NAME: &str = "valid tail.bin";
/// The file is three clusters, 10 to 12, in one run.
const FIRST: u32 = 10;
const LENGTH: u64 = 3 * CLUSTER as u64;
/// It has written one cluster and 100 bytes: cluster 12 lies wholly past that.
const VALID: u64 = CLUSTER as u64 + 100;

/// A source that reads like a memory image until it is told that a stretch of it has gone bad.
struct Flaky {
    source: MemorySource,
    bad: Mutex<Option<Range<u64>>>,
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

/// The file's three clusters hold noise all the way, so a copy that wrongly read past the valid
/// length would show it. Its entry says only `VALID` bytes were ever written.
fn volume() -> Vec<u8> {
    let mut builder = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, FIRST, FIRST + 1, FIRST + 2] {
        builder.allocate(cluster);
    }
    builder.fat(ROOT_CLUSTER, FAT_END);
    let boot = BootSector::parse(&builder.image).unwrap();
    let location = bitmap_location(&boot, &bitmap_entry(BITMAP_CLUSTER, u64::MAX)).unwrap();
    let mut file = entry_set(NAME, false, FIRST, LENGTH, true, true);
    // The stream extension holds the valid data length first, then the first cluster, then the data length.
    file[40..48].copy_from_slice(&VALID.to_le_bytes());
    seal_entry_set(&mut file);
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(location.first_cluster, location.required_length),
            upcase_entry(4, &[0; 100]),
            file,
        ],
    );
    for index in 0..3 {
        builder.put(FIRST + index, 0, &noise(FIRST + index, CLUSTER));
    }
    builder.image
}

/// The bytes the file holds on disk: noise, cluster after cluster.
fn on_disk() -> Vec<u8> {
    (0..3).flat_map(|n| noise(FIRST + n, CLUSTER)).collect()
}

struct Copied {
    bytes: Vec<u8>,
    row: Value,
    partial: u64,
}

/// Scans the volume, makes `cluster` of the file unreadable (when given), copies the file and returns what was written.
fn copy_with_a_bad_cluster(label: &str, bad_cluster: Option<u32>) -> Copied {
    let image = volume();
    let bad = bad_cluster.map(|cluster| {
        let at = Builder::cluster_position(cluster) as u64;
        at..at + CLUSTER as u64
    });
    let source = Arc::new(Flaky {
        source: MemorySource::new(image),
        bad: Mutex::new(None),
    });
    let workspace = Workspace::at(std::env::temp_dir().join(format!(
        "captainsalvage-exfat-valid-{}-{label}",
        std::process::id()
    )))
    .unwrap();
    let session =
        Session::with_source(Path::new("/test/exfat-valid"), source.clone(), workspace).unwrap();
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions {
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let engine = Engine::new();
    engine.set_session(session.clone());
    let found = engine
        .call("search", &json!({"query": NAME}), AUTHOR_USER)
        .unwrap();
    let id = found["rows"][0]["id"].as_i64().unwrap();
    *source.bad.lock().unwrap() = bad;

    let destination = session.workspace.dir().join("copy");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: vec![id],
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    let partial = loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(
                (status.files_done, status.files_failed),
                (1, 0),
                "{status:?}"
            );
            break status.files_partial;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "copy timed out: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    let bytes = std::fs::read(destination.join("Volume root").join(NAME)).unwrap();
    let manifest =
        std::fs::read_to_string(destination.join("captain-salvage-manifest.jsonl")).unwrap();
    let row = serde_json::from_str(manifest.lines().next().unwrap()).unwrap();
    Copied {
        bytes,
        row,
        partial,
    }
}

#[test]
fn a_file_with_a_short_valid_length_is_copied_with_zeros_past_it() {
    let copied = copy_with_a_bad_cluster("readable", None);
    let disk = on_disk();
    assert_eq!(copied.bytes.len() as u64, LENGTH);
    assert_eq!(copied.bytes[..VALID as usize], disk[..VALID as usize]);
    assert!(
        copied.bytes[VALID as usize..].iter().all(|byte| *byte == 0),
        "the bytes past the valid length are zeros, not the noise the clusters hold"
    );
    assert_eq!(copied.partial, 0);
    assert_eq!(copied.row["unreadableRanges"], json!([]), "{}", copied.row);
}

#[test]
fn a_cluster_wholly_past_the_valid_length_that_cannot_be_read_does_not_make_the_copy_partial() {
    let copied = copy_with_a_bad_cluster("tail-bad", Some(FIRST + 2));
    let disk = on_disk();
    assert_eq!(copied.bytes.len() as u64, LENGTH);
    assert_eq!(copied.bytes[..VALID as usize], disk[..VALID as usize]);
    assert!(copied.bytes[VALID as usize..].iter().all(|byte| *byte == 0));
    assert_eq!(copied.partial, 0, "{}", copied.row);
    assert!(
        !copied.row["status"]
            .as_str()
            .unwrap()
            .starts_with("partial"),
        "{}",
        copied.row
    );
    assert_eq!(copied.row["unreadableRanges"], json!([]), "{}", copied.row);
    assert_eq!(copied.row["unreadableClusters"], 0, "{}", copied.row);
}

#[test]
fn an_unreadable_cluster_before_the_valid_length_is_still_named() {
    let copied = copy_with_a_bad_cluster("head-bad", Some(FIRST));
    let disk = on_disk();
    assert_eq!(copied.bytes.len() as u64, LENGTH);
    assert!(copied.bytes[..CLUSTER].iter().all(|byte| *byte == 0));
    assert_eq!(
        copied.bytes[CLUSTER..VALID as usize],
        disk[CLUSTER..VALID as usize]
    );
    assert!(copied.bytes[VALID as usize..].iter().all(|byte| *byte == 0));
    assert_eq!(copied.partial, 1, "{}", copied.row);
    assert_eq!(copied.row["unreadableClusters"], 1, "{}", copied.row);
    assert_eq!(
        copied.row["unreadableRanges"],
        json!([{"start": 0, "end": CLUSTER}]),
        "{}",
        copied.row
    );
}

#[test]
fn an_unreadable_cluster_that_holds_the_valid_length_names_only_the_bytes_written() {
    let copied = copy_with_a_bad_cluster("edge-bad", Some(FIRST + 1));
    let disk = on_disk();
    assert_eq!(copied.bytes.len() as u64, LENGTH);
    assert_eq!(copied.bytes[..CLUSTER], disk[..CLUSTER]);
    assert!(copied.bytes[CLUSTER..].iter().all(|byte| *byte == 0));
    assert_eq!(copied.partial, 1, "{}", copied.row);
    assert_eq!(
        copied.row["unreadableRanges"],
        json!([{"start": CLUSTER, "end": VALID}]),
        "{}",
        copied.row
    );
}
