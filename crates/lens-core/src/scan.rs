//! The full read-only pass over a volume.
//!
//! Reads every cluster once, in disk order. For each it records a content class
//! and an entropy value, and stores any exFAT entry sets it finds. Progress is
//! committed chunk by chunk, so a scan can stop at any moment and resume later.

use crate::content::{classify, expected_class, text_head_value, CLASS_READ_ERROR, CLASS_TEXT, TEXT_HEAD_MAX_BYTES};
use crate::exfat::{find_sets, EntrySet, FIRST_CLUSTER};
use crate::map::MapDb;
use crate::volume::Volume;
use crate::workspace::{now_millis, Workspace};
use anyhow::{bail, Context, Result};
use serde::Serialize;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

/// Enough to finish a maximum-size entry set (19 slots) that starts in the last
/// slot of a chunk and continues into the next cluster.
const LOOKAHEAD_BYTES: u64 = 1024;
const DEFAULT_CHUNK_BYTES: u64 = 64 * 1024 * 1024;

const STATE_NEXT: &str = "next_cluster";
const STATE_STATUS: &str = "status";
const STATE_DIRECTORIES: &str = "directory_clusters";
const STATE_SETS: &str = "sets_found";
const STATE_ERRORS: &str = "read_errors";
const STATE_STARTED: &str = "started_at_millis";
const STATE_UPDATED: &str = "updated_at_millis";
const STATE_TEXT_HEADS_FROM: &str = "text_heads_from_cluster";
/// A rebuild or other writer can hold the database for a while. The scan waits.
const BUSY_RETRY_DELAY: Duration = Duration::from_secs(2);
const BUSY_RETRY_LIMIT: u32 = 3600;

#[derive(Clone, Copy, Debug)]
pub struct ScanOptions {
    pub chunk_clusters: u32,
}

impl ScanOptions {
    pub fn for_volume(volume: &Volume) -> Self {
        Self { chunk_clusters: (DEFAULT_CHUNK_BYTES / volume.boot.cluster_size()).max(1) as u32 }
    }
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanProgress {
    pub status: String,
    pub next_cluster: u32,
    pub total_clusters: u32,
    pub clusters_scanned: u32,
    pub directory_clusters: u64,
    pub sets_found: u64,
    pub read_errors: u64,
    pub started_at_millis: u64,
    pub updated_at_millis: u64,
    pub bytes_per_second: f64,
}

/// Reads scan progress as recorded in the database.
pub fn read_progress(database: &MapDb, volume: &Volume) -> Result<ScanProgress> {
    let next = database.state_u64(STATE_NEXT)?.max(u64::from(FIRST_CLUSTER)) as u32;
    Ok(ScanProgress {
        status: database.state(STATE_STATUS)?.unwrap_or_else(|| "not-started".into()),
        next_cluster: next,
        total_clusters: volume.boot.cluster_count,
        clusters_scanned: next - FIRST_CLUSTER,
        directory_clusters: database.state_u64(STATE_DIRECTORIES)?,
        sets_found: database.state_u64(STATE_SETS)?,
        read_errors: database.state_u64(STATE_ERRORS)?,
        started_at_millis: database.state_u64(STATE_STARTED)?,
        updated_at_millis: database.state_u64(STATE_UPDATED)?,
        bytes_per_second: 0.0,
    })
}

fn open_map_file(path: &std::path::Path, length: u64) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    file.set_len(length)?;
    Ok(file)
}

fn is_database_busy(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| match cause.downcast_ref::<rusqlite::Error>() {
        Some(rusqlite::Error::SqliteFailure(failure, _)) => {
            matches!(failure.code, rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        }
        _ => false,
    })
}

/// Measures text heads for text files found before this map existed. Reads only
/// the start of each file's first cluster, in disk order.
fn backfill_text_heads(volume: &Volume, database: &MapDb, text_heads: &File, up_to: u32) -> Result<()> {
    let mut clusters: Vec<u32> = database
        .file_names_before(up_to)?
        .into_iter()
        .filter(|(name, _)| expected_class(name) == Some(CLASS_TEXT))
        .map(|(_, cluster)| cluster)
        .filter(|&cluster| volume.valid_cluster(cluster))
        .collect();
    clusters.sort_unstable();
    clusters.dedup();
    for cluster in clusters {
        // Rounded up to a sector, because raw devices reject smaller reads.
        let length = TEXT_HEAD_MAX_BYTES.next_multiple_of(512);
        if let Ok(data) = volume.read_bytes(volume.cluster_offset(cluster), length) {
            text_heads.write_all_at(&[text_head_value(&data)], u64::from(cluster - FIRST_CLUSTER))?;
        }
    }
    Ok(())
}

/// Runs or resumes the scan, then rebuilds the tree when the pass completes.
/// Returns an error if another process already holds the workspace's scan lock.
pub fn run_scan(
    volume: &Volume,
    workspace: &Workspace,
    options: ScanOptions,
    stop: &AtomicBool,
    mut on_progress: impl FnMut(&ScanProgress),
) -> Result<ScanProgress> {
    let lock = OpenOptions::new().create(true).write(true).truncate(false).open(workspace.lock())?;
    match lock.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => bail!("another scan is already running on this workspace"),
        Err(TryLockError::Error(error)) => return Err(error.into()),
    }

    let mut database = MapDb::open(&workspace.database())?;
    let cluster_count = u64::from(volume.boot.cluster_count);
    let classes = open_map_file(&workspace.classes(), cluster_count)?;
    let entropy = open_map_file(&workspace.entropy(), cluster_count)?;
    let text_heads = open_map_file(&workspace.text_heads(), cluster_count)?;

    let cluster_size = volume.boot.cluster_size();
    let last = volume.boot.last_cluster_exclusive();
    let limits = volume.limits();
    let mut next = database.state_u64(STATE_NEXT)?.max(u64::from(FIRST_CLUSTER)) as u32;
    let mut directories = database.state_u64(STATE_DIRECTORIES)?;
    let mut sets_total = database.state_u64(STATE_SETS)?;
    let mut errors_total = database.state_u64(STATE_ERRORS)?;
    if database.state(STATE_STARTED)?.is_none() {
        database.set_state(STATE_STARTED, &now_millis().to_string())?;
    }
    if database.state(STATE_TEXT_HEADS_FROM)?.is_none() {
        if next > FIRST_CLUSTER {
            database.set_state(STATE_STATUS, "measuring-text")?;
            backfill_text_heads(volume, &database, &text_heads, next)?;
        }
        database.set_state(STATE_TEXT_HEADS_FROM, &next.to_string())?;
    }

    let mut buffer = Vec::new();
    let session_start = Instant::now();
    let mut session_bytes = 0u64;

    while next < last {
        if stop.load(Ordering::Relaxed) {
            database.set_state(STATE_STATUS, "paused")?;
            let mut progress = read_progress(&database, volume)?;
            progress.status = "paused".into();
            on_progress(&progress);
            return Ok(progress);
        }

        let count = options.chunk_clusters.min(last - next);
        let chunk_bytes = u64::from(count) * cluster_size;
        let offset = volume.cluster_offset(next);
        let volume_end = volume.cluster_offset(last - 1) + cluster_size;
        let lookahead = LOOKAHEAD_BYTES.min(volume_end - (offset + chunk_bytes));
        buffer.resize((chunk_bytes + lookahead) as usize, 0);

        let mut failed = vec![false; count as usize];
        let mut chunk_errors: Vec<(u32, String)> = Vec::new();
        if volume.source().read_exact_at(offset, &mut buffer).is_err() {
            // Retry one cluster at a time, so one bad spot loses only that cluster.
            buffer.fill(0);
            for index in 0..count as usize {
                let start = index * cluster_size as usize;
                let slice = &mut buffer[start..start + cluster_size as usize];
                if let Err(error) = volume.source().read_exact_at(offset + start as u64, slice) {
                    failed[index] = true;
                    chunk_errors.push((next + index as u32, error.to_string()));
                }
            }
        }

        let mut found: Vec<(u32, EntrySet)> = Vec::new();
        let mut class_bytes = vec![0u8; count as usize];
        let mut entropy_bytes = vec![0u8; count as usize];
        let mut text_head_bytes = vec![0u8; count as usize];
        for index in 0..count as usize {
            if failed[index] {
                class_bytes[index] = CLASS_READ_ERROR;
                continue;
            }
            let cluster = next + index as u32;
            let start = index * cluster_size as usize;
            let end = start + cluster_size as usize;
            let sets = find_sets(&buffer[start..buffer.len().min(end + LOOKAHEAD_BYTES as usize)], cluster_size as usize, limits);
            let holds_directory = sets.iter().any(|set| set.checksum_ok);
            let (class, scaled) = classify(&buffer[start..end], holds_directory);
            class_bytes[index] = class;
            entropy_bytes[index] = scaled;
            text_head_bytes[index] = text_head_value(&buffer[start..end]);
            if holds_directory {
                directories += 1;
            }
            sets_total += sets.len() as u64;
            found.extend(sets.into_iter().map(|set| (cluster, set)));
        }

        let map_position = u64::from(next - FIRST_CLUSTER);
        classes.write_all_at(&class_bytes, map_position)?;
        entropy.write_all_at(&entropy_bytes, map_position)?;
        text_heads.write_all_at(&text_head_bytes, map_position)?;

        next += count;
        errors_total += chunk_errors.len() as u64;
        let chunk_state = [
            (STATE_NEXT, next.to_string()),
            (STATE_STATUS, "running".to_string()),
            (STATE_DIRECTORIES, directories.to_string()),
            (STATE_SETS, sets_total.to_string()),
            (STATE_ERRORS, errors_total.to_string()),
            (STATE_UPDATED, now_millis().to_string()),
        ];
        let mut attempts = 0;
        loop {
            match database.commit_chunk(&found, &chunk_errors, &chunk_state) {
                Ok(()) => break,
                Err(error) if is_database_busy(&error) && attempts < BUSY_RETRY_LIMIT => {
                    attempts += 1;
                    std::thread::sleep(BUSY_RETRY_DELAY);
                }
                Err(error) => return Err(error),
            }
        }

        session_bytes += chunk_bytes;
        let mut progress = read_progress(&database, volume)?;
        progress.bytes_per_second = session_bytes as f64 / session_start.elapsed().as_secs_f64().max(0.001);
        on_progress(&progress);
    }

    database.set_state(STATE_STATUS, "resolving")?;
    database.resolve(volume, workspace)?;
    database.set_state(STATE_STATUS, "complete")?;
    database.set_state(STATE_UPDATED, &now_millis().to_string())?;
    let progress = read_progress(&database, volume)?;
    on_progress(&progress);
    Ok(progress)
}
