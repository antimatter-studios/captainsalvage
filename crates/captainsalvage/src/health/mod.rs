//! Physical damage: where a failing drive reads well, slowly, not at all, or resets.
//!
//! A drive with a bad area can retry inside for seconds. Behind a USB bridge that is long enough
//! for the host to give up and reset the whole device, which then comes back under another name,
//! and every reset hurts a damaged drive again. So the app learns where the bad areas are, once,
//! and from then on never reads them:
//!
//! - the **read journal** ([`store`]) writes "started" before a read and "done" after it. A read
//!   that started and never ended is what the device went away during, and its range is marked
//!   `reset-culprit` the next time the source is open;
//! - the **guard** ([`GuardedSource`]) sits between the device and everything that reads it. It
//!   times the reads it journals, records what it sees (slow, unreadable, culprit), and answers a
//!   read of a range known to be bad with an error, without touching the device. The readers
//!   already know what to do with an error: a scan reports the bytes as unreadable, a preview
//!   marks them, a copy writes zeros there and names them;
//! - the **probe** ([`probe`]) finds the bad zones on purpose, gently: a sparse grid of single
//!   4 KiB reads, then each bad point is approached from the good side in halving steps, so the
//!   edges of a zone are found without reading into it. Each call is bounded and resumable.
//!
//! Everything here only reads. The key of a source ([`source_key`]) is made from its length and
//! the start of its first sector, so it is known before any marker is read (reading markers is
//! what can reset the device) and is the same whatever the drive is called this time.

pub mod map;
pub mod probe;
pub mod store;
#[cfg(test)]
mod tests;

pub use map::{HealthMap, HealthRange, RangeState};
pub use store::HealthStore;

use crate::raw_scan::Fingerprint;
use crate::source::{means_device_gone, BlockSource, ReadStats};
use anyhow::Result;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

/// Ranges are recorded in whole blocks of this size.
pub const BLOCK: u64 = 4096;
/// A read slower than this is `slow`, unless a probe says otherwise.
pub const DEFAULT_SLOW_MS: u64 = 500;
/// After this many resets a source is a failing drive (see [`Health::failing`]), unless said otherwise.
pub const DEFAULT_FAILING_AFTER: u64 = 2;
/// On a failing drive no read is longer than this: a long read into a bad area keeps the drive
/// retrying for longer, and its reset costs more.
const CAREFUL_PIECE_BYTES: u64 = 64 * 1024;
/// A read that fails because the device is gone is the culprit of the reset only if it was in flight for at
/// least this long. The reads the first real run found were 108 and 140 ms in; one that comes back at once
/// was issued after the device had already gone, and says nothing about where the drive is bad.
pub const CULPRIT_MIN_MS: u64 = 20;
/// The first this many bytes of a source are known to read: the partition table and the boot sectors were
/// read to open it.
const HEAD_BYTES: u64 = 1024 * 1024;
/// How many of the earlier good reads of a source are kept in mind to tell which ranges are safe to read again.
const GOOD_READS_KEPT: usize = 2000;
/// A read this long is always journaled, whoever asks: nothing small enough to be one bad sector
/// is journaled unless it is a probe or the source is being opened.
const JOURNAL_FROM_BYTES: u64 = 1024 * 1024;
/// A failed read up to this long says which bytes failed. A longer one is retried in pieces by
/// whoever asked, and the pieces say.
const LOCALISES_UP_TO: u64 = 64 * 1024;
/// A slow read up to this long says which bytes were slow.
const SLOW_LOCALISES_UP_TO: u64 = 1024 * 1024;

static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);
static LIVE: Mutex<Option<HashSet<u64>>> = Mutex::new(None);

fn live_set<R>(use_it: impl FnOnce(&mut HashSet<u64>) -> R) -> R {
    let mut guard = LIVE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    use_it(guard.get_or_insert_with(HashSet::new))
}

/// Whether the process that wrote a journal row can still be finishing it: this process, with
/// that guard alive, or another process that is running.
pub(crate) fn owner_is_alive(owner: &str) -> bool {
    let Some((pid, serial)) = owner.split_once(':') else {
        return false;
    };
    let (Ok(pid), Ok(serial)) = (pid.parse::<u32>(), serial.parse::<u64>()) else {
        return false;
    };
    if pid == std::process::id() {
        return live_set(|live| live.contains(&serial));
    }
    // SAFETY: signal 0 only asks whether the process exists.
    let found = unsafe { libc::kill(pid as libc::pid_t, 0) };
    found == 0 || io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
}

/// The kind a bad zone has as an extent of its source, beside the other kinds of extent.
pub const BAD_SECTOR_KIND: &str = "bad_sector";

/// A bad zone as an extent of its source: a range the health map says must not be read. They are kept in
/// the catalog under the source's key, so they are there when the source is opened again, and when the
/// drive comes back under another name.
#[derive(Clone, Debug, serde::Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BadExtent {
    /// Always `bad_sector`.
    pub kind: &'static str,
    pub start: u64,
    pub length: u64,
    /// How it was found: `reset-culprit`, `unreadable` or `suspect`.
    pub state: RangeState,
    pub latency_ms: Option<u64>,
    pub found_millis: u64,
    pub note: String,
}

/// The bad-sector extents among `ranges`: the ones that block reading.
pub fn bad_extents(ranges: &[HealthRange]) -> Vec<BadExtent> {
    ranges
        .iter()
        .filter(|range| range.state.blocks())
        .map(|range| BadExtent {
            kind: BAD_SECTOR_KIND,
            start: range.start,
            length: range.end - range.start,
            state: range.state,
            latency_ms: range.latency_ms,
            found_millis: range.found_millis,
            note: range.note.clone(),
        })
        .collect()
}

/// What a read of a range known to be bad is answered with, without reading the device.
#[derive(Debug)]
pub struct KnownBadRange {
    pub start: u64,
    pub end: u64,
    pub state: RangeState,
}

impl std::fmt::Display for KnownBadRange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "not read: bytes {} to {} are marked {} in this source's health map, and reading them can reset the device",
            self.start,
            self.end,
            self.state.name()
        )
    }
}

impl std::error::Error for KnownBadRange {}

/// The range a read was refused for, if `error` is that refusal.
pub fn known_bad(error: &io::Error) -> Option<&KnownBadRange> {
    error.get_ref()?.downcast_ref::<KnownBadRange>()
}

/// The key of a source: its length and the start of its first sector, with the bytes a mount
/// rewrites left out (exFAT keeps its volume flags and percent in use there). It names the drive
/// whatever it is called this time and needs one read, of the sector that always reads.
pub fn source_key(device: &dyn BlockSource, length: u64) -> String {
    let base = base_key(device, length);
    // A GPT disk's first sector is the same protective MBR on every disk of one model, which has the same
    // length: its own GUID, in the header after it, tells two such disks apart.
    let mut header = [0u8; 72];
    if length >= 1024
        && device.read_exact_at(512, &mut header).is_ok()
        && &header[..8] == b"EFI PART"
    {
        let guid: String = header[56..64]
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        return format!("{base}-g{guid}");
    }
    base
}

/// The key a source had before a GPT disk's own GUID was part of it: its length and the start of its first
/// sector. Rows stored under it belong to the one disk that had it, and are moved to the new key.
pub fn legacy_source_key(device: &dyn BlockSource, length: u64) -> String {
    base_key(device, length)
}

fn base_key(device: &dyn BlockSource, length: u64) -> String {
    let mut head = vec![0u8; length.min(512) as usize];
    let digest = match device.read_exact_at(0, &mut head) {
        Ok(()) => {
            for byte in head.iter_mut().take(120).skip(104) {
                *byte = 0;
            }
            let hash = Sha256::digest(&head);
            hash[..8]
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        }
        Err(_) => "unreadable-head".into(),
    };
    format!("{length}-{digest}")
}

struct State {
    map: HealthMap,
    source_id: Option<String>,
}

/// The health of one source, shared by the guard that reads it and the tools that show it.
pub struct Health {
    /// The key the map, the journal and the reset count are kept under: the physical disk's. A whole disk
    /// and its partitions share it, so a bad zone found by one is avoided by the other.
    key: String,
    /// This device's own key (its length and first sector), which a probe's progress is kept under, in
    /// this device's own offsets.
    own_key: String,
    /// Where this device starts in the physical disk: 0 for the whole disk.
    base: u64,
    /// The ranges that earlier reads of the disk found good, in the disk's offsets.
    good_reads: RwLock<Vec<(u64, u64)>>,
    owner: String,
    serial: u64,
    length: u64,
    store: Arc<HealthStore>,
    inner: RwLock<Arc<dyn BlockSource>>,
    state: RwLock<State>,
    journaling: AtomicU32,
    slow_ms: AtomicU64,
    found_at_open: Mutex<Vec<HealthRange>>,
    identity: Mutex<Option<Fingerprint>>,
    /// How many times the device went away while this source was read, over all runs.
    resets: AtomicU64,
    failing_after: AtomicU64,
    /// The current disappearance of the device has been counted.
    counted_gone: AtomicBool,
    /// The last read that found the device gone had been in flight long enough to be the one it went away during.
    gone_in_flight: AtomicBool,
}

impl Drop for Health {
    fn drop(&mut self) {
        let serial = self.serial;
        live_set(|live| {
            live.remove(&serial);
        });
    }
}

/// While this lives, every read through the guard is journaled.
pub struct JournalScope<'a>(&'a Health);

impl Drop for JournalScope<'_> {
    fn drop(&mut self) {
        self.0.journaling.fetch_sub(1, Ordering::Relaxed);
    }
}

/// What one timed read of a probe found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Probed {
    pub state: RangeState,
    pub latency_ms: u64,
    /// The device is gone: nothing more can be probed until it comes back.
    pub gone: bool,
    /// The read was refused because the range is already known to be bad.
    pub refused: bool,
    /// With `gone`: the read was in flight long enough to be the one the device went away during. When it
    /// is not, the device had already gone, and nothing is learned about the range.
    pub in_flight: bool,
}

fn aligned(offset: u64, end: u64, length: u64) -> (u64, u64) {
    (
        offset / BLOCK * BLOCK,
        end.div_ceil(BLOCK).saturating_mul(BLOCK).min(length),
    )
}

/// Sorted spans joined where they touch or overlap.
fn coalesce_spans(spans: Vec<(u64, u64)>) -> Vec<(u64, u64)> {
    let mut joined: Vec<(u64, u64)> = Vec::with_capacity(spans.len());
    for (start, length) in spans {
        let end = start.saturating_add(length);
        match joined.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.max(end),
            _ => joined.push((start, end)),
        }
    }
    joined
}

impl Health {
    /// A range of the physical disk as this device sees it, cut to the device: None when it is elsewhere.
    fn down(&self, range: &HealthRange) -> Option<HealthRange> {
        let end = self.base.saturating_add(self.length);
        if range.end <= self.base || range.start >= end {
            return None;
        }
        Some(HealthRange {
            start: range.start.max(self.base) - self.base,
            end: range.end.min(end) - self.base,
            ..range.clone()
        })
    }

    /// A range of this device in the physical disk's offsets.
    fn up(&self, range: HealthRange) -> HealthRange {
        HealthRange {
            start: range.start + self.base,
            end: range.end + self.base,
            ..range
        }
    }

    /// Puts the health of `inner` under `key`. Reads that started and never ended in an earlier
    /// run become `reset-culprit` ranges now, before anything reads the source.
    pub fn attach(
        inner: Arc<dyn BlockSource>,
        length: u64,
        store: Arc<HealthStore>,
        key: String,
    ) -> Result<Arc<Self>> {
        Self::attach_in(inner, length, store, key, None)
    }

    /// [`Health::attach`] for a device that is a stretch of a physical disk: `link` is that disk's key and
    /// where the device starts in it. What is known is kept under the disk's key, in the disk's offsets,
    /// and seen from here in this device's own.
    pub fn attach_in(
        inner: Arc<dyn BlockSource>,
        length: u64,
        store: Arc<HealthStore>,
        own_key: String,
        link: Option<(String, u64)>,
    ) -> Result<Arc<Self>> {
        let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
        live_set(|live| {
            live.insert(serial);
        });
        let (key, base) = match link {
            Some((disk_key, offset)) if disk_key != own_key => {
                store.link(&own_key, &disk_key, offset, length)?;
                (disk_key, offset)
            }
            _ => (own_key.clone(), 0),
        };
        // A culprit recorded for a read made after the device had gone is not one.
        let _ = store.drop_instant_culprits(&key, CULPRIT_MIN_MS);
        let mut map = store.load_map(&key)?;
        let abandoned = store.abandoned(&key)?;
        let mut found = Vec::new();
        for read in &abandoned {
            let (start, end) = aligned(read.start, read.start + read.length, u64::MAX);
            let range = HealthRange::new(
                start,
                end,
                RangeState::ResetCulprit,
                None,
                if read.length > LOCALISES_UP_TO {
                    "a read of this range started and never finished, so the device probably reset during it; the bad spot is somewhere inside"
                } else {
                    "a read of this range started and never finished, so the device probably reset during it"
                },
            );
            map.record(range.clone());
            found.push(range);
        }
        let (mut resets, failing_after) = store.load_flags(&key)?;
        if !abandoned.is_empty() {
            // Each read that was open when its run ended is a reset that run could not count.
            resets = store.add_resets(&key, abandoned.len() as u64)?;
        }
        let mut good = store.good_reads(&key, GOOD_READS_KEPT).unwrap_or_default();
        good.sort_unstable();
        let health = Arc::new(Self {
            key,
            own_key,
            base,
            good_reads: RwLock::new(coalesce_spans(good)),
            owner: format!("{}:{serial}", std::process::id()),
            serial,
            length,
            store,
            inner: RwLock::new(inner),
            state: RwLock::new(State {
                map,
                source_id: None,
            }),
            journaling: AtomicU32::new(0),
            slow_ms: AtomicU64::new(DEFAULT_SLOW_MS),
            found_at_open: Mutex::new(Vec::new()),
            identity: Mutex::new(None),
            resets: AtomicU64::new(resets),
            failing_after: AtomicU64::new(failing_after.max(1)),
            counted_gone: AtomicBool::new(false),
            gone_in_flight: AtomicBool::new(false),
        });
        let mine: Vec<HealthRange> = found
            .iter()
            .filter_map(|range| health.down(range))
            .collect();
        if let Ok(mut slot) = health.found_at_open.lock() {
            *slot = mine;
        }
        if !abandoned.is_empty() {
            health.persist();
            let ids: Vec<i64> = abandoned.iter().map(|read| read.id).collect();
            health.store.settle(&ids, "never-finished")?;
        }
        let _ = health.store.prune(&health.key);
        Ok(health)
    }

    /// Reads again what is stored for the physical disk, after something else (the link of a partition to
    /// the disk) changed it: the zones, the reset count, and reads that started and never ended.
    pub fn reload(&self) {
        let _ = self.store.drop_instant_culprits(&self.key, CULPRIT_MIN_MS);
        let Ok(mut map) = self.store.load_map(&self.key) else {
            return;
        };
        let abandoned = self.store.abandoned(&self.key).unwrap_or_default();
        for read in &abandoned {
            let (start, end) = aligned(read.start, read.start + read.length, u64::MAX);
            map.record(HealthRange::new(
                start,
                end,
                RangeState::ResetCulprit,
                None,
                "a read of this range started and never finished, so the device probably reset during it",
            ));
        }
        if let Ok(mut state) = self.state.write() {
            // What this run learned and has not stored yet stays.
            for range in state.map.ranges().to_vec() {
                map.record(range);
            }
            state.map = map;
        }
        if !abandoned.is_empty() {
            let _ = self.store.add_resets(&self.key, abandoned.len() as u64);
            let ids: Vec<i64> = abandoned.iter().map(|read| read.id).collect();
            let _ = self.store.settle(&ids, "never-finished");
        }
        if let Ok((resets, failing_after)) = self.store.load_flags(&self.key) {
            self.resets.store(resets, Ordering::Relaxed);
            self.failing_after
                .store(failing_after.max(1), Ordering::Relaxed);
        }
        self.persist();
    }

    /// The guard to give to everything that reads the source.
    pub fn guard(self: &Arc<Self>) -> Arc<dyn BlockSource> {
        Arc::new(GuardedSource {
            health: self.clone(),
        })
    }

    /// The key of the physical disk this is, whole or in part: what the map, the journal and the reset count
    /// are kept under.
    pub fn key(&self) -> &str {
        &self.key
    }

    /// This device's own key, which its probe's progress is kept under.
    pub fn own_key(&self) -> &str {
        &self.own_key
    }

    /// Where this device starts in the physical disk.
    pub fn base(&self) -> u64 {
        self.base
    }

    pub fn length(&self) -> u64 {
        self.length
    }

    pub fn store(&self) -> &Arc<HealthStore> {
        &self.store
    }

    pub fn slow_ms(&self) -> u64 {
        self.slow_ms.load(Ordering::Relaxed)
    }

    /// How many times the device went away while this source was being read.
    pub fn resets(&self) -> u64 {
        self.resets.load(Ordering::Relaxed)
    }

    pub fn failing_after(&self) -> u64 {
        self.failing_after.load(Ordering::Relaxed)
    }

    pub fn set_failing_after(&self, after: u64) {
        let after = after.max(1);
        self.failing_after.store(after, Ordering::Relaxed);
        let _ = self.store.set_failing_after(&self.key, after);
    }

    /// The drive has reset often enough to be called failing. Reading then goes carefully: every
    /// read is journaled, none is longer than 64 KiB, and a suspect band is not read.
    pub fn failing(&self) -> bool {
        self.resets() >= self.failing_after()
    }

    /// The person says the count is wrong, or the drive was repaired.
    pub fn clear_resets(&self) {
        self.resets.store(0, Ordering::Relaxed);
        let _ = self.store.set_resets(&self.key, 0);
    }

    /// The device went away during a read: counted once however many reads were in flight.
    fn count_reset(&self) {
        if !self.counted_gone.swap(true, Ordering::Relaxed) {
            if let Ok(total) = self.store.add_resets(&self.key, 1) {
                self.resets.store(total, Ordering::Relaxed);
            } else {
                self.resets.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    pub fn set_slow_ms(&self, ms: u64) {
        self.slow_ms.store(ms.max(1), Ordering::Relaxed);
    }

    /// Every read through the guard is journaled while the scope lives. Opening a source and
    /// probing use it: those are the reads that reach an unknown bad spot first.
    pub fn journal_scope(&self) -> JournalScope<'_> {
        self.journaling.fetch_add(1, Ordering::Relaxed);
        JournalScope(self)
    }

    /// The reads that started and never ended before this run, found when the source was opened.
    pub fn found_at_open(&self) -> Vec<HealthRange> {
        self.found_at_open
            .lock()
            .map(|found| found.clone())
            .unwrap_or_default()
    }

    /// Which stored source this is, once the catalog says. Ranges the same source has under
    /// another key (its first sector changed) come in.
    pub fn adopt_source(&self, source_id: &str) {
        // A stored source is a device's, and the rows are the disk's: only the whole disk names them.
        if self.base != 0 || self.key != self.own_key {
            return;
        }
        let _ = self.store.adopt(&self.key, source_id);
        let others = self
            .store
            .load_map_of_source(source_id, &self.key)
            .unwrap_or_default();
        let changed = !others.is_empty();
        if let Ok(mut state) = self.state.write() {
            state.source_id = Some(source_id.to_string());
            for range in others {
                state.map.record(range);
            }
        }
        if changed {
            self.persist();
        }
    }

    pub fn source_id(&self) -> Option<String> {
        self.state
            .read()
            .ok()
            .and_then(|state| state.source_id.clone())
    }

    /// The markers the open source was recognised by, kept so the same drive can be found again
    /// after it comes back under another name.
    pub fn set_identity(&self, fingerprint: Fingerprint) {
        if let Ok(mut identity) = self.identity.lock() {
            *identity = Some(fingerprint);
        }
    }

    pub fn identity(&self) -> Option<Fingerprint> {
        self.identity
            .lock()
            .ok()
            .and_then(|identity| identity.clone())
    }

    /// The bad zones of this source as extents, in order.
    pub fn bad_extents(&self) -> Vec<BadExtent> {
        bad_extents(self.snapshot().ranges())
    }

    /// The map as this device sees it, in its own offsets.
    pub fn snapshot(&self) -> HealthMap {
        self.state
            .read()
            .map(|state| {
                if self.base == 0
                    && state
                        .map
                        .ranges()
                        .last()
                        .is_none_or(|last| last.end <= self.length)
                {
                    state.map.clone()
                } else {
                    HealthMap::from_ranges(
                        state
                            .map
                            .ranges()
                            .iter()
                            .filter_map(|range| self.down(range))
                            .collect(),
                    )
                }
            })
            .unwrap_or_default()
    }

    /// The whole physical disk's map, in the disk's offsets.
    pub fn disk_snapshot(&self) -> HealthMap {
        self.state
            .read()
            .map(|state| state.map.clone())
            .unwrap_or_default()
    }

    /// The disk has a zone where the drive reset or that a probe jumped over, or has reset often: opening and
    /// reading it should spare it (see `MarkerReader`).
    pub fn careful_open(&self) -> bool {
        self.failing()
            || self.state.read().is_ok_and(|state| {
                state.map.ranges().iter().any(|range| {
                    matches!(range.state, RangeState::ResetCulprit | RangeState::Suspect)
                })
            })
    }

    /// Whether `offset..offset + length` of this device is known to read: in its first mebibyte, which was
    /// read to open it; or a range a probe found good; or one an earlier read of it ended well.
    pub fn known_good(&self, offset: u64, length: u64) -> bool {
        let end = offset.saturating_add(length);
        if end <= HEAD_BYTES {
            return true;
        }
        let (from, to) = (offset + self.base, end + self.base);
        if self.state.read().is_ok_and(|state| {
            state.map.ranges().iter().any(|range| {
                range.state == RangeState::Good && range.start <= from && range.end >= to
            })
        }) {
            return true;
        }
        self.good_reads.read().is_ok_and(|good| {
            good.iter()
                .any(|(start, stop)| *start <= from && *stop >= to)
        })
    }

    /// The first range that blocks reading and overlaps `start..end`.
    pub fn blocked(&self, start: u64, end: u64) -> Option<KnownBadRange> {
        self.blocked_with(start, end, true)
    }

    fn blocked_with(&self, start: u64, end: u64, suspect_blocks: bool) -> Option<KnownBadRange> {
        let state = self.state.read().ok()?;
        if state.map.is_empty() {
            return None;
        }
        state
            .map
            .blocked_with(start + self.base, end + self.base, suspect_blocks)
            .map(|range| KnownBadRange {
                start: range.start.saturating_sub(self.base),
                end: range.end.saturating_sub(self.base),
                state: range.state,
            })
    }

    pub fn blocked_within(&self, start: u64, end: u64) -> Vec<(u64, u64)> {
        self.state
            .read()
            .map(|state| {
                state
                    .map
                    .blocked_within(start + self.base, end + self.base)
                    .into_iter()
                    .map(|(from, to)| (from - self.base, to - self.base))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Records an observation and stores the map.
    pub fn record(&self, range: HealthRange) {
        self.record_quiet(range);
        self.persist();
    }

    /// Records an observation, and leaves storing the map to [`Health::persist`].
    pub fn record_quiet(&self, range: HealthRange) {
        let range = self.up(range);
        if let Ok(mut state) = self.state.write() {
            state.map.record(range);
        }
    }

    pub fn replace_zone(&self, range: HealthRange) {
        let range = self.up(range);
        if let Ok(mut state) = self.state.write() {
            state.map.replace(range);
        }
    }

    /// A suspect band whose edges were mapped: what was suspect between `band_start` and `band_end`
    /// is forgotten, and only `zone_start..zone_end` is suspect (or worse, as `state` says). The
    /// reads that were good on the way, which the band had covered, are good again.
    pub fn resolve_band(
        &self,
        band_start: u64,
        band_end: u64,
        zone: HealthRange,
        good_reads: &[u64],
    ) {
        let zone = self.up(zone);
        if let Ok(mut state) = self.state.write() {
            state
                .map
                .clear_suspect(band_start + self.base, band_end + self.base);
            state.map.record(zone);
            for at in good_reads {
                state.map.record(HealthRange::new(
                    *at + self.base,
                    *at + self.base + BLOCK,
                    RangeState::Good,
                    None,
                    "a probe read this range well",
                ));
            }
        }
    }

    /// Records a bad zone found outside the probe, such as by reading the drive with `dd`: `start..end` of this
    /// device, in the state given (a zone the drive reset on, one that failed, or one to keep out of), with who
    /// says so and why. It is read by nothing from then on, like any other, until it is forgiven.
    pub fn mark_bad(
        &self,
        start: u64,
        end: u64,
        state: RangeState,
        who: &str,
        why: &str,
    ) -> Result<HealthRange> {
        anyhow::ensure!(
            matches!(
                state,
                RangeState::ResetCulprit | RangeState::Unreadable | RangeState::Suspect
            ),
            "a zone is marked reset-culprit, unreadable or suspect, not {}",
            state.name()
        );
        anyhow::ensure!(start < end, "start must be below end");
        anyhow::ensure!(
            end <= self.length,
            "the zone ends at {end}, past the end of the source at {}",
            self.length
        );
        let why = why.trim();
        anyhow::ensure!(
            !why.is_empty(),
            "say why: where the zone was found, as a note that will make sense later"
        );
        anyhow::ensure!(
            why.chars().count() <= 500,
            "the reason is at most 500 characters"
        );
        let range = HealthRange::new(start, end, state, None, &format!("marked by {who}: {why}"));
        self.record(range.clone());
        Ok(range)
    }

    /// Forgets what is known about a range: the person says it is fine, or fixed.
    pub fn forgive(&self, start: u64, end: u64) {
        if let Ok(mut state) = self.state.write() {
            state.map.clear(start + self.base, end + self.base);
        }
        self.persist();
    }

    pub fn persist(&self) {
        if let Ok(state) = self.state.read() {
            let _ = self
                .store
                .save_map(&self.key, state.source_id.as_deref(), &state.map);
        }
    }

    /// Points the guard at a device that came back. The source it replaces is dropped, and with
    /// it the descriptor it held on a node that no longer exists.
    pub fn rebind(&self, device: Arc<dyn BlockSource>) {
        if let Ok(mut inner) = self.inner.write() {
            *inner = device;
        }
        self.counted_gone.store(false, Ordering::Relaxed);
    }

    pub fn is_gone(&self) -> bool {
        self.device().is_gone()
    }

    /// The device the guard reads now.
    pub fn device_handle(&self) -> Arc<dyn BlockSource> {
        self.device()
    }

    fn device(&self) -> Arc<dyn BlockSource> {
        self.inner
            .read()
            .map(|inner| inner.clone())
            .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
    }

    /// One read through the guard. See the module documentation for what it records.
    fn read(
        &self,
        offset: u64,
        buffer: &mut [u8],
        force: bool,
        through_suspect: bool,
    ) -> (io::Result<()>, u64, bool) {
        let device = self.device();
        let length = buffer.len() as u64;
        let end = offset.saturating_add(length);
        if device.is_gone() {
            // Issued after the device had gone: not a culprit, and not counted again.
            self.gone_in_flight.store(false, Ordering::Relaxed);
            return (device.read_exact_at(offset, buffer), 0, false);
        }
        if let Some(bad) = self.blocked_with(offset, end, !through_suspect) {
            return (Err(io::Error::other(bad)), 0, true);
        }
        let careful = self.failing();
        let journal = force
            || careful
            || self.journaling.load(Ordering::Relaxed) > 0
            || length >= JOURNAL_FROM_BYTES;
        if careful && length > CAREFUL_PIECE_BYTES {
            // Small reads, each journaled and timed, the first failure ending the read: whoever
            // asked retries in smaller pieces, which are refused or read one by one.
            let mut total = 0;
            for (index, piece) in buffer.chunks_mut(CAREFUL_PIECE_BYTES as usize).enumerate() {
                let at = offset + index as u64 * CAREFUL_PIECE_BYTES;
                let (result, elapsed, refused) = self.read_once(&device, at, piece, journal);
                total += elapsed;
                if result.is_err() {
                    return (result, total, refused);
                }
            }
            return (Ok(()), total, false);
        }
        self.read_once(&device, offset, buffer, journal)
    }

    /// One read of the device, journaled when asked and recorded in the map by how it went.
    fn read_once(
        &self,
        device: &Arc<dyn BlockSource>,
        offset: u64,
        buffer: &mut [u8],
        journal: bool,
    ) -> (io::Result<()>, u64, bool) {
        let length = buffer.len() as u64;
        let end = offset.saturating_add(length);
        let row = journal
            .then(|| {
                self.store
                    .started(&self.key, &self.owner, offset + self.base, length)
                    .ok()
            })
            .flatten();
        let started = Instant::now();
        let result = device.read_exact_at(offset, buffer);
        let elapsed = started.elapsed().as_millis() as u64;
        let gone = result.as_ref().err().is_some_and(means_device_gone) || device.is_gone();
        if let Some(row) = row {
            let outcome = match (&result, gone) {
                (Ok(()), _) => "ok",
                (Err(_), true) if elapsed >= CULPRIT_MIN_MS => "gone",
                // Issued after the device had gone: the evidence says nothing about this range.
                (Err(_), true) => "gone-at-once",
                (Err(_), false) => "error",
            };
            let _ = self.store.done(row, elapsed, outcome);
        }
        let (from, to) = aligned(offset, end, self.length);
        if gone {
            self.count_reset();
            // Only the read in flight when the device went away can be the cause. One that comes back at once
            // (a failure because the node had already vanished) was issued after it, whatever it asked for.
            let in_flight = elapsed >= CULPRIT_MIN_MS;
            self.gone_in_flight.store(in_flight, Ordering::Relaxed);
            if in_flight {
                self.record(HealthRange::new(
                    from,
                    to,
                    RangeState::ResetCulprit,
                    Some(elapsed),
                    if length > LOCALISES_UP_TO {
                        "the device went away during a read of this range; the bad spot is somewhere inside"
                    } else {
                        "the device went away during a read of this range"
                    },
                ));
            }
        } else if result.is_err() {
            if length <= LOCALISES_UP_TO {
                self.record(HealthRange::new(
                    from,
                    to,
                    RangeState::Unreadable,
                    Some(elapsed),
                    "a read of this range failed",
                ));
            }
        } else if elapsed >= self.slow_ms() && length <= SLOW_LOCALISES_UP_TO {
            self.record(HealthRange::new(
                from,
                to,
                RangeState::Slow,
                Some(elapsed),
                "a read of this range was slow",
            ));
        }
        (result, elapsed, false)
    }

    /// One timed, journaled read of `len` bytes at `offset`, recorded as good, slow, unreadable
    /// or culprit. The map is not stored: the probe does that when it ends a step.
    pub fn probe_read(&self, offset: u64, len: u64, through_suspect: bool) -> Probed {
        let mut buffer = vec![0u8; len as usize];
        let before = self
            .snapshot()
            .blocked_with(offset, offset + len, !through_suspect)
            .cloned();
        let (result, latency_ms, refused) = self.read(offset, &mut buffer, true, through_suspect);
        let gone = self.is_gone();
        let (from, to) = aligned(offset, offset + len, self.length);
        let state = if refused {
            before.map_or(RangeState::Unreadable, |range| range.state)
        } else if gone {
            RangeState::ResetCulprit
        } else if result.is_err() {
            RangeState::Unreadable
        } else if latency_ms >= self.slow_ms() {
            RangeState::Slow
        } else {
            RangeState::Good
        };
        if !refused && state == RangeState::Good {
            self.record_quiet(HealthRange::new(
                from,
                to,
                RangeState::Good,
                Some(latency_ms),
                "a probe read this range well",
            ));
        }
        Probed {
            state,
            latency_ms,
            gone,
            refused,
            in_flight: gone && self.gone_in_flight.load(Ordering::Relaxed),
        }
    }

    pub fn report(&self) -> Value {
        let map = self.snapshot();
        let counts =
            |state: RangeState| json!({"ranges": map.count(state), "bytes": map.bytes(state)});
        json!({
            "key": self.key,
            "deviceKey": self.own_key,
            "offsetInDisk": self.base,
            "sourceId": self.source_id(),
            "length": self.length,
            "slowThresholdMs": self.slow_ms(),
            "resets": self.resets(),
            "failingAfterResets": self.failing_after(),
            "failing": self.failing(),
            "deviceGone": self.is_gone(),
            "good": counts(RangeState::Good),
            "slow": counts(RangeState::Slow),
            "suspect": counts(RangeState::Suspect),
            "unreadable": counts(RangeState::Unreadable),
            "resetCulprit": counts(RangeState::ResetCulprit),
            "foundAtOpen": self.found_at_open(),
            "ranges": map.ranges(),
        })
    }
}

/// What every reader of a source reads through.
pub struct GuardedSource {
    health: Arc<Health>,
}

impl GuardedSource {
    pub fn health(&self) -> &Arc<Health> {
        &self.health
    }
}

impl BlockSource for GuardedSource {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        self.health.read(offset, buffer, false, false).0
    }

    fn read_stats(&self) -> ReadStats {
        self.health.device().read_stats()
    }

    fn is_gone(&self) -> bool {
        self.health.device().is_gone()
    }

    fn bad_ranges(&self) -> Vec<HealthRange> {
        self.health
            .snapshot()
            .ranges()
            .iter()
            .filter(|range| range.state > RangeState::Good)
            .cloned()
            .collect()
    }

    #[cfg(feature = "test-hooks")]
    fn simulate_disconnect(&self) {
        self.health.device().simulate_disconnect();
    }
}
