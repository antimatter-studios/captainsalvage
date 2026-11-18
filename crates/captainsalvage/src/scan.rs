//! The full read-only pass over a volume.
//!
//! Reads every cluster once, in disk order. For each it records a content class
//! and an entropy value, and stores any exFAT entry sets it finds. Progress is
//! committed chunk by chunk, so a scan can stop at any moment and resume later.

use crate::content::{
    classify, expected_class, text_head_value, CLASS_READ_ERROR, CLASS_TEXT, TEXT_HEAD_MAX_BYTES,
};
use crate::filesystem::{FilesystemKind, SalvageEntry as EntrySet, STORAGE_DAMAGED};
use crate::map::MapDb;
use crate::session::DISK_GONE;
use crate::volume::Volume;
use crate::workspace::{now_millis, Workspace};
use anyhow::{bail, Context, Result};
use fs_exfat::primitives::FIRST_CLUSTER;
use serde::Serialize;
use std::fs::{File, OpenOptions, TryLockError};
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

mod rescan;
pub use rescan::{rescan_changed, FilesystemRescan};

/// Enough to finish a maximum-size entry set (19 slots) that starts in the last
/// slot of a chunk and continues into the next cluster.
const LOCK_ATTEMPTS: u32 = 20;
const LOCK_RETRY_WAIT: std::time::Duration = std::time::Duration::from_millis(50);
const LOOKAHEAD_BYTES: u64 = 1024;
/// A FAT file's long name is at most twenty entries of thirty-two bytes before its short entry. A
/// short entry nearer the start of a cluster than this may have the rest of its name in the cluster
/// before it in the folder.
const LONG_NAME_REACH: usize = 20 * 32;
/// An exFAT file's entry set is at most nineteen entries of thirty-two bytes (the File entry, the
/// Stream Extension and seventeen name entries). A File entry nearer the end of a cluster than this
/// may have the rest of its set in the next cluster of its folder.
const EXFAT_SET_REACH: usize = 19 * 32;
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
        Self {
            chunk_clusters: (DEFAULT_CHUNK_BYTES / volume.geometry.cluster_size()).max(1) as u32,
        }
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
    let next = database
        .state_u64(STATE_NEXT)?
        .max(u64::from(FIRST_CLUSTER)) as u32;
    Ok(ScanProgress {
        status: database
            .state(STATE_STATUS)?
            .unwrap_or_else(|| "not-started".into()),
        next_cluster: next,
        total_clusters: volume.geometry.cluster_count,
        clusters_scanned: next - FIRST_CLUSTER,
        directory_clusters: database.state_u64(STATE_DIRECTORIES)?,
        sets_found: database.state_u64(STATE_SETS)?,
        read_errors: database.state_u64(STATE_ERRORS)?,
        started_at_millis: database.state_u64(STATE_STARTED)?,
        updated_at_millis: database.state_u64(STATE_UPDATED)?,
        bytes_per_second: 0.0,
    })
}

pub(crate) fn open_map_file(path: &std::path::Path, length: u64) -> Result<File> {
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

pub(crate) fn is_database_busy(error: &anyhow::Error) -> bool {
    error
        .chain()
        .any(|cause| match cause.downcast_ref::<rusqlite::Error>() {
            Some(rusqlite::Error::SqliteFailure(failure, _)) => {
                matches!(
                    failure.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                )
            }
            _ => false,
        })
}

/// Measures text heads for text files found before this map existed. Reads only
/// the start of each file's first cluster, in disk order.
fn backfill_text_heads(
    volume: &Volume,
    database: &MapDb,
    text_heads: &File,
    up_to: u32,
) -> Result<()> {
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
            text_heads.write_all_at(
                &[text_head_value(&data)],
                u64::from(cluster - FIRST_CLUSTER),
            )?;
        }
    }
    Ok(())
}

/// What one chunk of clusters holds.
struct Chunk {
    found: Vec<(u32, EntrySet)>,
    errors: Vec<(u32, String)>,
    classes: Vec<u8>,
    entropy: Vec<u8>,
    text_heads: Vec<u8>,
    /// Clusters that hold a directory.
    directories: u64,
    /// Entry sets found.
    sets: u64,
}

/// Reads `count` clusters from `first`, and works out what each is and which entry sets start in
/// it. A cluster that cannot be read is classed as a read error and costs only itself. None when
/// the drive has gone: nothing read then says anything about the clusters.
fn read_chunk(
    volume: &Volume,
    buffer: &mut Vec<u8>,
    first: u32,
    count: u32,
    lookahead_max: u64,
    last: u32,
) -> Option<Chunk> {
    let cluster_size = volume.geometry.cluster_size();
    let chunk_bytes = u64::from(count) * cluster_size;
    let offset = volume.cluster_offset(first);
    let volume_end = volume.cluster_offset(last - 1) + cluster_size;
    let lookahead = lookahead_max.min(volume_end - (offset + chunk_bytes));
    buffer.resize((chunk_bytes + lookahead) as usize, 0);

    let mut failed = vec![false; count as usize];
    let mut chunk_errors: Vec<(u32, String)> = Vec::new();
    if volume.source().read_exact_at(offset, buffer).is_err() {
        // Retry one cluster at a time, so one bad spot loses only that cluster.
        buffer.fill(0);
        for (index, cluster_failed) in failed.iter_mut().enumerate() {
            let start = index * cluster_size as usize;
            let slice = &mut buffer[start..start + cluster_size as usize];
            if let Err(error) = volume.source().read_exact_at(offset + start as u64, slice) {
                *cluster_failed = true;
                chunk_errors.push((first + index as u32, error.to_string()));
            }
        }
    }
    // Nothing here is a bad spot. Marking these clusters would spoil a good map, and a
    // later resume could not undo it.
    if volume.source().is_gone() {
        return None;
    }

    let mut found: Vec<(u32, EntrySet)> = Vec::new();
    let mut class_bytes = vec![0u8; count as usize];
    let mut entropy_bytes = vec![0u8; count as usize];
    let mut text_head_bytes = vec![0u8; count as usize];
    let mut directories = 0;
    let mut sets_total = 0;
    for index in 0..count as usize {
        if failed[index] {
            class_bytes[index] = CLASS_READ_ERROR;
            continue;
        }
        let cluster = first + index as u32;
        let start = index * cluster_size as usize;
        let end = start + cluster_size as usize;
        let data = &buffer[start..buffer.len().min(end + lookahead_max as usize)];
        let mut sets = volume.find_sets(data, cluster_size as usize);
        // A FAT entry is placed by its short entry, so a long name that began in the cluster before
        // it in the folder is whole only with that cluster's end in front.
        if is_fat(volume.kind) && sets.iter().any(|set| set.offset < LONG_NAME_REACH) {
            if let Some(lead) = long_name_lead(volume, cluster) {
                let mut joined = lead.clone();
                joined.extend_from_slice(data);
                sets = volume
                    .find_sets(&joined, lead.len() + cluster_size as usize)
                    .into_iter()
                    .filter(|set| set.offset >= lead.len())
                    .map(|mut set| {
                        set.offset -= lead.len();
                        set
                    })
                    .collect();
            }
        }
        // An exFAT entry set is placed by its first entry, and the driver is given the next
        // `lookahead` bytes of the disk for the rest of it. When the folder goes on in a cluster
        // that is not the next on the disk, the rest is there, so it is read by the FAT chain.
        if volume.kind == FilesystemKind::Exfat
            && starts_set_near_end(&data[..cluster_size as usize])
        {
            if let Some(next) = chain_continuation(volume, cluster, lookahead_max as usize) {
                let mut joined = data[..cluster_size as usize].to_vec();
                joined.extend(next);
                sets = volume.find_sets(&joined, cluster_size as usize);
            }
        }
        let holds_directory = sets.iter().any(|set| set.checksum_ok);
        let (class, scaled) = classify(&buffer[start..end], holds_directory);
        class_bytes[index] = class;
        entropy_bytes[index] = scaled;
        text_head_bytes[index] = text_head_value(&buffer[start..end]);
        if holds_directory {
            directories += 1;
        }
        sets_total += sets.len() as u64;
        found.extend(sets.into_iter().map(|mut set| {
            // A FILE record in a cluster the bitmap calls free is an old copy of the MFT, whatever
            // its own flag says, so it is recorded as not in use.
            if let Some(ntfs) = set.ntfs.as_mut() {
                set.in_use &= volume.is_allocated(cluster);
                // Records the kernel's ntfs3 driver wrote have no number in their header, so
                // it is where `$MFT` puts them.
                if ntfs.record_number.is_none() && ntfs.storage != STORAGE_DAMAGED {
                    ntfs.record_number = volume
                        .ntfs
                        .as_ref()
                        .and_then(|access| access.record_number_at(cluster, set.offset));
                }
            }
            (cluster, set)
        }));
    }
    Some(Chunk {
        found,
        errors: chunk_errors,
        classes: class_bytes,
        entropy: entropy_bytes,
        text_heads: text_head_bytes,
        directories,
        sets: sets_total,
    })
}

/// True when a File entry (in use or deleted) starts in the last nineteen entries of a cluster.
fn starts_set_near_end(cluster: &[u8]) -> bool {
    let from = cluster.len().saturating_sub(EXFAT_SET_REACH) / 32 * 32;
    cluster[from..]
        .chunks_exact(32)
        .any(|entry| matches!(entry[0], 0x85 | 0x05))
}

/// The start of the cluster that follows `cluster` in its FAT chain, up to `length` bytes, when it
/// is not the one beside it on the disk. None when the chain ends, leaves the volume, or goes to
/// the next cluster, which the scan already has.
fn chain_continuation(volume: &Volume, cluster: u32, length: usize) -> Option<Vec<u8>> {
    let next = volume.fat_value(cluster);
    if next == cluster + 1 || !volume.valid_cluster(next) {
        return None;
    }
    let mut bytes = volume.read_cluster(next).ok()?;
    bytes.truncate(length);
    Some(bytes)
}

fn is_fat(kind: FilesystemKind) -> bool {
    matches!(
        kind,
        FilesystemKind::Fat12 | FilesystemKind::Fat16 | FilesystemKind::Fat32
    )
}

/// The end of the folder cluster that comes before `cluster` in its chain, enough of it to hold the
/// longest long name, or None when nothing in the FAT leads to this cluster or it cannot be read.
fn long_name_lead(volume: &Volume, cluster: u32) -> Option<Vec<u8>> {
    let size = volume.geometry.cluster_size() as usize;
    let mut before = Vec::new();
    let mut current = cluster;
    while before.len() * size < LONG_NAME_REACH {
        let Some(previous) = volume.predecessor(current) else {
            break;
        };
        if previous == cluster || before.contains(&previous) {
            break;
        }
        before.push(previous);
        current = previous;
    }
    if before.is_empty() {
        return None;
    }
    let mut lead = Vec::new();
    for previous in before.into_iter().rev() {
        lead.extend(volume.read_cluster(previous).ok()?);
    }
    Some(lead)
}

/// Takes the lock that lets one scan at a time work on a workspace, held until the file drops.
/// Fails when another process holds it.
fn lock_scan(workspace: &Workspace) -> Result<File> {
    let lock = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(workspace.lock())?;
    // A child process being started can hold a copy of this lock for a moment, until it
    // replaces itself with its program. The app starts ffmpeg, so that alone must not
    // read as another scan running.
    let mut attempts = 0;
    loop {
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) if attempts < LOCK_ATTEMPTS => {
                attempts += 1;
                std::thread::sleep(LOCK_RETRY_WAIT);
            }
            Err(TryLockError::WouldBlock) => {
                bail!("another scan is already running on this workspace")
            }
            Err(TryLockError::Error(error)) => return Err(error.into()),
        }
    }
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
    let _lock = lock_scan(workspace)?;

    let mut database = MapDb::open(&workspace.database())?;
    let cluster_count = u64::from(volume.geometry.cluster_count);
    let classes = open_map_file(&workspace.classes(), cluster_count)?;
    let entropy = open_map_file(&workspace.entropy(), cluster_count)?;
    let text_heads = open_map_file(&workspace.text_heads(), cluster_count)?;

    let cluster_size = volume.geometry.cluster_size();
    // A FILE record that starts in the last sector of a cluster ends in the next one.
    let lookahead_max = LOOKAHEAD_BYTES.max(volume.geometry.record_size);
    let last = volume.geometry.last_cluster_exclusive();
    let mut next = database
        .state_u64(STATE_NEXT)?
        .max(u64::from(FIRST_CLUSTER)) as u32;
    if database.state("fixed_root_indexed")?.is_none() {
        let entries = volume.fixed_root_entries();
        let total = database.state_u64(STATE_SETS)? + entries.len() as u64;
        database.commit_chunk(
            &entries,
            &[],
            &[
                ("fixed_root_indexed", "true".into()),
                (STATE_SETS, total.to_string()),
            ],
        )?;
    }
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
        let Some(read) = read_chunk(volume, &mut buffer, next, count, lookahead_max, last) else {
            database.set_state(STATE_STATUS, "paused")?;
            bail!(DISK_GONE);
        };
        directories += read.directories;
        sets_total += read.sets;
        let Chunk {
            found,
            errors: chunk_errors,
            classes: class_bytes,
            entropy: entropy_bytes,
            text_heads: text_head_bytes,
            ..
        } = read;

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
        progress.bytes_per_second =
            session_bytes as f64 / session_start.elapsed().as_secs_f64().max(0.001);
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
