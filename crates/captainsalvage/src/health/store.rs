//! What the read journal and the health map keep.
//!
//! The journal is written BEFORE a read starts (`started`) and after it ends (`done`). A device
//! that resets during a read takes the reader with it, or the person quits the app: the row of
//! that read is left without an end, and the next open reads it as the cause.
//!
//! A device read never waits on the catalog. The journal is a file of its own (`read-journal.sqlite`) with
//! its own connection, so a write transaction the catalog's other users hold (a raw scan's, in this same
//! process) cannot stall it, and both the journal and the catalog's connection give up after a moment instead of
//! waiting without end: a journal write that cannot be made is kept in memory and written when the file is free
//! (see [`Journal`]), and the map is kept in memory and written at the next change.
//!
//! Rows are keyed by the source's key (see [`super::source_key`]), which comes from the source's
//! bytes and not from its /dev path, so the same drive is the same after it comes back as another
//! disk. Once the catalog knows which stored source it is, the rows also carry its ID.

use super::map::{HealthMap, HealthRange, RangeState};
use crate::catalog::Catalog;
use crate::workspace::now_millis;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// How long the journal waits for its own file, and the catalog's connection for the catalog's, before giving
/// up. Nothing here may wait without end: a read of a device is waiting behind it.
const JOURNAL_BUSY: Duration = Duration::from_millis(100);
const CATALOG_BUSY: Duration = Duration::from_millis(300);
/// How long a write to the catalog's rows is retried when another connection holds the catalog's write lock. Each
/// try waits `CATALOG_BUSY` at most and lets go of this store's mutex between tries, so a status call is never
/// held up for longer than that, but opening a source and recording what is known of it do not fail on a lock
/// that is held for a moment, as they would under load.
const CATALOG_WRITE_PATIENCE: Duration = Duration::from_secs(30);
/// What the end of a read waits for the journal's file: a little longer, because the read is over.
const JOURNAL_END_BUSY: Duration = Duration::from_millis(500);
const JOURNAL_FILE: &str = "read-journal.sqlite";

/// Runs `work` again while it is refused for a lock, for at most `CATALOG_WRITE_PATIENCE`, with a short wait
/// between tries. Another error, or the same one at the end, is returned.
fn retry_while_busy<R>(mut work: impl FnMut() -> rusqlite::Result<R>) -> rusqlite::Result<R> {
    let deadline = Instant::now() + CATALOG_WRITE_PATIENCE;
    loop {
        match work() {
            Err(rusqlite::Error::SqliteFailure(error, _))
                if matches!(
                    error.code,
                    rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                ) && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(25));
            }
            other => return other,
        }
    }
}

const JOURNAL_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS read_journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_key TEXT NOT NULL, source_id TEXT,
    owner TEXT NOT NULL,
    start INTEGER NOT NULL, length INTEGER NOT NULL,
    started_millis INTEGER NOT NULL,
    finished_millis INTEGER, elapsed_ms INTEGER, outcome TEXT
);
CREATE INDEX IF NOT EXISTS read_journal_key ON read_journal(source_key, finished_millis);";

pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS read_journal (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_key TEXT NOT NULL, source_id TEXT,
    owner TEXT NOT NULL,
    start INTEGER NOT NULL, length INTEGER NOT NULL,
    started_millis INTEGER NOT NULL,
    finished_millis INTEGER, elapsed_ms INTEGER, outcome TEXT
);
CREATE INDEX IF NOT EXISTS read_journal_key ON read_journal(source_key, finished_millis);
CREATE TABLE IF NOT EXISTS health_ranges (
    source_key TEXT NOT NULL, source_id TEXT,
    start INTEGER NOT NULL, end INTEGER NOT NULL,
    state TEXT NOT NULL, latency_ms INTEGER, note TEXT NOT NULL DEFAULT '',
    updated_millis INTEGER NOT NULL,
    PRIMARY KEY (source_key, start)
);
CREATE INDEX IF NOT EXISTS health_ranges_source ON health_ranges(source_id);
CREATE TABLE IF NOT EXISTS health_flags (
    source_key TEXT PRIMARY KEY,
    resets INTEGER NOT NULL DEFAULT 0,
    failing_after INTEGER NOT NULL DEFAULT 2,
    updated_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS health_links (
    child_key TEXT PRIMARY KEY, parent_key TEXT NOT NULL,
    offset INTEGER NOT NULL, length INTEGER NOT NULL,
    updated_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS health_probes (
    source_key TEXT PRIMARY KEY, state TEXT NOT NULL, updated_millis INTEGER NOT NULL
);";

/// How many finished journal rows of reads that went well a source keeps. Rows of reads that did
/// not are kept.
const KEPT_GOOD_ROWS: i64 = 2000;

/// A read that started and has no end.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpenRead {
    pub id: i64,
    pub start: u64,
    pub length: u64,
    pub owner: String,
}

/// A row of the journal as an earlier version kept it in the catalog.
type OldJournalRow = (
    String,
    Option<String>,
    String,
    i64,
    i64,
    i64,
    Option<i64>,
    Option<i64>,
    Option<String>,
);

/// A read the journal could not write when it started, kept until the file is free.
struct PendingRow {
    key: String,
    owner: String,
    start: u64,
    length: u64,
    started_millis: u64,
    /// When it ended, how long it took and how: set when `done` came before the row was written.
    finished: Option<(u64, u64, String)>,
}

/// How a read ended, when the journal's file was busy at that moment.
struct PendingEnd {
    row: i64,
    finished_millis: u64,
    elapsed_ms: u64,
    outcome: String,
}

#[derive(Default)]
struct Pending {
    next: i64,
    /// Ends of reads that were written to the file, not yet written: a row left open would read, next run, as a
    /// read the device went away during.
    ends: Vec<PendingEnd>,
    rows: HashMap<i64, PendingRow>,
    /// The rows that were written after all, by the number `started` gave: their row in the file.
    written: HashMap<i64, i64>,
}

/// The read journal: a file of its own, with its own connection, that gives up after a moment.
struct Journal {
    connection: Mutex<Connection>,
    pending: Mutex<Pending>,
}

pub struct HealthStore {
    catalog: Mutex<Catalog>,
    journal: Journal,
}

impl HealthStore {
    pub fn open(root: &Path) -> Result<Self> {
        Self::with_catalog(Catalog::open(root)?)
    }

    #[cfg(test)]
    pub(crate) fn from_catalog(catalog: Catalog) -> Self {
        Self::with_catalog(catalog).expect("the journal's connection is set up")
    }

    pub fn for_user() -> Result<Self> {
        Self::with_catalog(Catalog::for_user()?)
    }

    /// The journal writes twice for a read, so its own connection does not wait for the disk on each commit:
    /// what it writes survives the app being stopped, which is what it is for. The catalog's connection here
    /// gives up after a moment on a lock someone else holds, instead of the half minute the catalog's own
    /// users wait: what is asked of it is the status of a source, which must always answer.
    fn with_catalog(catalog: Catalog) -> Result<Self> {
        catalog
            .connection()
            .execute_batch("PRAGMA synchronous = NORMAL;")?;
        catalog.connection().busy_timeout(CATALOG_BUSY)?;
        // Setting the journal's file up (its mode, its tables) takes a write lock, and every store of this process
        // that is opened at the same moment does it on the same file: opening a source must wait for that, as for
        // any other lock, and only afterwards does the connection give up quickly, for the reads of a device.
        let connection = Connection::open(catalog.root().join(JOURNAL_FILE))?;
        connection.busy_timeout(CATALOG_WRITE_PATIENCE)?;
        retry_while_busy(|| {
            connection.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")?;
            connection.execute_batch(JOURNAL_SCHEMA)
        })?;
        connection.busy_timeout(JOURNAL_BUSY)?;
        let store = Self {
            catalog: Mutex::new(catalog),
            journal: Journal {
                connection: Mutex::new(connection),
                pending: Mutex::new(Pending::default()),
            },
        };
        store.move_old_journal_rows();
        Ok(store)
    }

    /// Rows an earlier version wrote in the catalog move to the journal's file, once. Not worth waiting
    /// for: when the catalog is busy they stay, and move at the next start.
    fn move_old_journal_rows(&self) {
        let rows: Vec<OldJournalRow> = {
            let catalog = self.lock();
            let Ok(mut statement) = catalog.connection().prepare(
                "SELECT source_key, source_id, owner, start, length, started_millis, finished_millis, elapsed_ms, outcome
                 FROM read_journal ORDER BY id",
            ) else {
                return;
            };
            let Ok(found) = statement.query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                    row.get(8)?,
                ))
            }) else {
                return;
            };
            found.filter_map(Result::ok).collect()
        };
        if rows.is_empty() {
            return;
        }
        let moved = self.with_journal(|connection| {
            let transaction = connection.unchecked_transaction()?;
            for row in &rows {
                transaction.execute(
                    "INSERT INTO read_journal (source_key, source_id, owner, start, length, started_millis, finished_millis, elapsed_ms, outcome)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8],
                )?;
            }
            transaction.commit()
        });
        if moved.is_ok() {
            let _ = self
                .lock()
                .connection()
                .execute("DELETE FROM read_journal", []);
        }
    }

    /// Runs a write on the catalog until it is not refused for a lock: a try that finds the catalog busy gives
    /// the mutex up, waits a moment and tries again, for at most `CATALOG_WRITE_PATIENCE`. A write that is
    /// refused for another reason, or still refused at the end, returns that error.
    fn patiently<R>(
        &self,
        mut work: impl FnMut(&Catalog) -> rusqlite::Result<R>,
    ) -> rusqlite::Result<R> {
        retry_while_busy(|| work(&self.lock()))
    }

    /// [`Self::patiently`] for a write to the journal's file, which only the writes that must not be lost (the
    /// ones that move or close rows, not the one made while a read is in flight) use.
    fn patiently_in_journal<R>(
        &self,
        mut work: impl FnMut(&Connection) -> rusqlite::Result<R>,
    ) -> rusqlite::Result<R> {
        retry_while_busy(|| self.with_journal(&mut work))
    }

    fn lock(&self) -> MutexGuard<'_, Catalog> {
        self.catalog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn with_journal<R>(
        &self,
        work: impl FnOnce(&Connection) -> rusqlite::Result<R>,
    ) -> rusqlite::Result<R> {
        let connection = self
            .journal
            .connection
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        work(&connection)
    }

    fn pending(&self) -> MutexGuard<'_, Pending> {
        self.journal
            .pending
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes the reads that could not be written when they started, if the file is free now.
    fn flush_pending(&self) {
        let mut pending = self.pending();
        let ends = std::mem::take(&mut pending.ends);
        for end in ends {
            let wrote = self.with_journal(|connection| {
                connection.execute(
                    "UPDATE read_journal SET finished_millis = ?2, elapsed_ms = ?3, outcome = ?4 WHERE id = ?1",
                    params![end.row, end.finished_millis as i64, end.elapsed_ms as i64, end.outcome],
                )
            });
            if wrote.is_err() {
                pending.ends.push(end);
            }
        }
        if pending.rows.is_empty() {
            return;
        }
        let numbers: Vec<i64> = pending.rows.keys().copied().collect();
        for number in numbers {
            let Some(row) = pending.rows.get(&number) else {
                continue;
            };
            let wrote = self.with_journal(|connection| {
                connection.execute(
                    "INSERT INTO read_journal (source_key, owner, start, length, started_millis, finished_millis, elapsed_ms, outcome)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        row.key,
                        row.owner,
                        row.start as i64,
                        row.length as i64,
                        row.started_millis as i64,
                        row.finished.as_ref().map(|f| f.0 as i64),
                        row.finished.as_ref().map(|f| f.1 as i64),
                        row.finished.as_ref().map(|f| f.2.clone()),
                    ],
                )?;
                Ok(connection.last_insert_rowid())
            });
            match wrote {
                Ok(real) => {
                    if let Some(row) = pending.rows.remove(&number) {
                        if row.finished.is_none() {
                            pending.written.insert(number, real);
                        }
                    }
                }
                // Busy again: the rest wait too.
                Err(_) => return,
            }
        }
    }

    /// Writes that a read of `start..start + length` is about to start. Returns its row. When the journal's
    /// file is not free within a moment the read is kept in memory, written when it is, and the read goes on.
    pub fn started(&self, key: &str, owner: &str, start: u64, length: u64) -> Result<i64> {
        self.flush_pending();
        let started_millis = now_millis();
        let wrote = self.with_journal(|connection| {
            connection.execute(
                "INSERT INTO read_journal (source_key, owner, start, length, started_millis)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    key,
                    owner,
                    start as i64,
                    length as i64,
                    started_millis as i64
                ],
            )?;
            Ok(connection.last_insert_rowid())
        });
        match wrote {
            Ok(row) => Ok(row),
            Err(_) => {
                let mut pending = self.pending();
                pending.next -= 1;
                let number = pending.next;
                pending.rows.insert(
                    number,
                    PendingRow {
                        key: key.into(),
                        owner: owner.into(),
                        start,
                        length,
                        started_millis,
                        finished: None,
                    },
                );
                Ok(number)
            }
        }
    }

    /// Writes that the read ended: how long it took and how it ended (`ok`, `error` or `gone`).
    pub fn done(&self, id: i64, elapsed_ms: u64, outcome: &str) -> Result<()> {
        let finished_millis = now_millis();
        let id = if id < 0 {
            let mut pending = self.pending();
            if let Some(row) = pending.rows.get_mut(&id) {
                // Never written: it is written now, once, with its end.
                row.finished = Some((finished_millis, elapsed_ms, outcome.to_string()));
                drop(pending);
                self.flush_pending();
                return Ok(());
            }
            match pending.written.remove(&id) {
                Some(real) => real,
                None => return Ok(()),
            }
        } else {
            id
        };
        // The read is over, so the end is worth a little more waiting than the start was, and when the file
        // is still busy it is kept and written later: a row left open is read, next run, as the cause of a reset.
        let wrote = self.with_journal(|connection| {
            connection.busy_timeout(JOURNAL_END_BUSY)?;
            let wrote = connection.execute(
                "UPDATE read_journal SET finished_millis = ?2, elapsed_ms = ?3, outcome = ?4 WHERE id = ?1",
                params![id, finished_millis as i64, elapsed_ms as i64, outcome],
            );
            let _ = connection.busy_timeout(JOURNAL_BUSY);
            wrote
        });
        if wrote.is_err() {
            self.pending().ends.push(PendingEnd {
                row: id,
                finished_millis,
                elapsed_ms,
                outcome: outcome.to_string(),
            });
        }
        Ok(())
    }

    /// The reads of `key` that started and never ended, and are not in flight now.
    pub fn abandoned(&self, key: &str) -> Result<Vec<OpenRead>> {
        self.flush_pending();
        let all = self.with_journal(|connection| {
            let mut statement = connection.prepare(
                "SELECT id, start, length, owner FROM read_journal
                 WHERE source_key = ?1 AND finished_millis IS NULL ORDER BY id",
            )?;
            let rows = statement.query_map([key], |row| {
                Ok(OpenRead {
                    id: row.get(0)?,
                    start: row.get::<_, i64>(1)? as u64,
                    length: row.get::<_, i64>(2)? as u64,
                    owner: row.get(3)?,
                })
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?;
        Ok(all
            .into_iter()
            .filter(|read| !super::owner_is_alive(&read.owner))
            .collect())
    }

    /// Closes the rows of reads that never ended, so each is counted once.
    pub fn settle(&self, ids: &[i64], outcome: &str) -> Result<()> {
        self.patiently_in_journal(|connection| {
            for id in ids {
                connection.execute(
                    "UPDATE read_journal SET finished_millis = ?2, outcome = ?3 WHERE id = ?1",
                    params![id, now_millis() as i64, outcome],
                )?;
            }
            Ok(())
        })?;
        Ok(())
    }

    /// The most recent rows of a source, newest first.
    pub fn recent(&self, key: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        self.flush_pending();
        Ok(self.with_journal(|connection| {
            let mut statement = connection.prepare(
                "SELECT start, length, started_millis, finished_millis, elapsed_ms, outcome
                 FROM read_journal WHERE source_key = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = statement.query_map(params![key, limit as i64], |row| {
                Ok(serde_json::json!({
                    "start": row.get::<_, i64>(0)?,
                    "length": row.get::<_, i64>(1)?,
                    "startedMillis": row.get::<_, i64>(2)?,
                    "finishedMillis": row.get::<_, Option<i64>>(3)?,
                    "elapsedMs": row.get::<_, Option<i64>>(4)?,
                    "outcome": row.get::<_, Option<String>>(5)?,
                }))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }

    /// Drops old rows of reads that went well.
    pub fn prune(&self, key: &str) -> Result<()> {
        self.with_journal(|connection| {
            connection.execute(
                "DELETE FROM read_journal WHERE source_key = ?1 AND outcome = 'ok'
                 AND id <= (SELECT coalesce(max(id), 0) FROM read_journal WHERE source_key = ?1) - ?2",
                params![key, KEPT_GOOD_ROWS],
            )
        })?;
        Ok(())
    }

    /// How many reads the journal holds in memory because its file was busy.
    pub fn journal_pending(&self) -> usize {
        self.pending().rows.len()
    }

    /// The bad zones stored for a source key, as extents. They are the same whichever run of the app, and
    /// whichever name the device has, wrote them.
    pub fn bad_extents(&self, key: &str) -> Result<Vec<super::BadExtent>> {
        Ok(super::bad_extents(self.load_map(key)?.ranges()))
    }

    pub fn load_map(&self, key: &str) -> Result<HealthMap> {
        let catalog = self.lock();
        let mut statement = catalog.connection().prepare(
            "SELECT start, end, state, latency_ms, note, updated_millis FROM health_ranges WHERE source_key = ?1 ORDER BY start",
        )?;
        let rows = statement.query_map([key], range_row)?;
        Ok(HealthMap::from_ranges(
            rows.collect::<rusqlite::Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect(),
        ))
    }

    /// The ranges other keys hold for the same stored source.
    pub fn load_map_of_source(
        &self,
        source_id: &str,
        except_key: &str,
    ) -> Result<Vec<HealthRange>> {
        let catalog = self.lock();
        let mut statement = catalog.connection().prepare(
            "SELECT start, end, state, latency_ms, note, updated_millis FROM health_ranges
             WHERE source_id = ?1 AND source_key != ?2 ORDER BY start",
        )?;
        let rows = statement.query_map(params![source_id, except_key], range_row)?;
        Ok(rows
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .collect())
    }

    /// Stores the map of `key` as it is now.
    pub fn save_map(&self, key: &str, source_id: Option<&str>, map: &HealthMap) -> Result<()> {
        self.patiently(|catalog| {
            let transaction = catalog.connection().unchecked_transaction()?;
            transaction.execute("DELETE FROM health_ranges WHERE source_key = ?1", [key])?;
            for range in map.ranges() {
                transaction.execute(
                    "INSERT INTO health_ranges (source_key, source_id, start, end, state, latency_ms, note, updated_millis)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                    params![
                        key,
                        source_id,
                        range.start as i64,
                        range.end as i64,
                        range.state.name(),
                        range.latency_ms.map(|ms| ms as i64),
                        range.note,
                        range.found_millis as i64
                    ],
                )?;
            }
            transaction.commit()
        })?;
        Ok(())
    }

    /// Tells the rows of `key` which stored source they are about.
    pub fn adopt(&self, key: &str, source_id: &str) -> Result<()> {
        self.patiently(|catalog| {
            catalog.connection().execute(
                "UPDATE health_ranges SET source_id = ?2 WHERE source_key = ?1",
                params![key, source_id],
            )
        })?;
        self.patiently_in_journal(|connection| {
            connection.execute(
                "UPDATE read_journal SET source_id = ?2 WHERE source_key = ?1",
                params![key, source_id],
            )
        })?;
        Ok(())
    }

    /// How many times the device went away while this source was being read, and after how many the
    /// drive is called failing.
    pub fn load_flags(&self, key: &str) -> Result<(u64, u64)> {
        Ok(self
            .lock()
            .connection()
            .query_row(
                "SELECT resets, failing_after FROM health_flags WHERE source_key = ?1",
                [key],
                |row| Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64)),
            )
            .optional()?
            .unwrap_or((0, super::DEFAULT_FAILING_AFTER)))
    }

    /// The physical disk a partition's key belongs to: the whole disk's key and where the partition starts
    /// in it.
    pub fn link_of(&self, child_key: &str) -> Result<Option<(String, u64)>> {
        Ok(self
            .lock()
            .connection()
            .query_row(
                "SELECT parent_key, offset FROM health_links WHERE child_key = ?1",
                [child_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64)),
            )
            .optional()?)
    }

    /// Records that the source of `child_key` is the stretch `offset..offset + length` of the physical disk
    /// `parent_key`, and brings what is stored under the child's key (its bad zones, its journal rows, its
    /// reset count) into the disk's, in the disk's own offsets. Done once: the child's rows are gone after.
    pub fn link(&self, child_key: &str, parent_key: &str, offset: u64, length: u64) -> Result<()> {
        if child_key == parent_key {
            return Ok(());
        }
        self.patiently(|catalog| {
        let connection = catalog.connection();
        let transaction = connection.unchecked_transaction()?;
        transaction.execute(
            "INSERT INTO health_links (child_key, parent_key, offset, length, updated_millis) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(child_key) DO UPDATE SET parent_key = excluded.parent_key, offset = excluded.offset,
             length = excluded.length, updated_millis = excluded.updated_millis",
            params![child_key, parent_key, offset as i64, length as i64, now_millis() as i64],
        )?;
        // Its zones, moved to the disk's offsets. A zone the disk already knows stays as it is, and the
        // earlier of the two times is kept by `record`.
        let mut statement = transaction.prepare(
            "SELECT start, end, state, latency_ms, note, updated_millis FROM health_ranges WHERE source_key = ?1 ORDER BY start",
        )?;
        let moved: Vec<HealthRange> = statement
            .query_map([child_key], range_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .flatten()
            .map(|mut range| {
                range.start += offset;
                range.end += offset;
                range
            })
            .collect();
        drop(statement);
        if !moved.is_empty() {
            let mut statement = transaction.prepare(
                "SELECT start, end, state, latency_ms, note, updated_millis FROM health_ranges WHERE source_key = ?1 ORDER BY start",
            )?;
            let mut map = HealthMap::from_ranges(
                statement
                    .query_map([parent_key], range_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?
                    .into_iter()
                    .flatten()
                    .collect(),
            );
            drop(statement);
            for range in moved {
                map.record(range);
            }
            transaction.execute(
                "DELETE FROM health_ranges WHERE source_key = ?1",
                [parent_key],
            )?;
            for range in map.ranges() {
                transaction.execute(
                    "INSERT INTO health_ranges (source_key, source_id, start, end, state, latency_ms, note, updated_millis)
                     VALUES (?1, NULL, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![
                        parent_key,
                        range.start as i64,
                        range.end as i64,
                        range.state.name(),
                        range.latency_ms.map(|ms| ms as i64),
                        range.note,
                        range.found_millis as i64
                    ],
                )?;
            }
            transaction.execute(
                "DELETE FROM health_ranges WHERE source_key = ?1",
                [child_key],
            )?;
        }
        // Its reset count: the drive is one drive.
        let child_resets: i64 = transaction
            .query_row(
                "SELECT resets FROM health_flags WHERE source_key = ?1",
                [child_key],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(0);
        if child_resets > 0 {
            transaction.execute(
                "INSERT INTO health_flags (source_key, resets, updated_millis) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_key) DO UPDATE SET resets = resets + excluded.resets, updated_millis = excluded.updated_millis",
                params![parent_key, child_resets, now_millis() as i64],
            )?;
        }
        transaction.execute(
            "DELETE FROM health_flags WHERE source_key = ?1",
            [child_key],
        )?;
        transaction.commit()
        })?;
        // Its journal rows (an open one is a culprit for the disk too), in the disk's offsets: in the journal's
        // own file, after the catalog's commit, so neither waits for the other.
        self.patiently_in_journal(|connection| {
            connection.execute(
                "UPDATE read_journal SET source_key = ?2, start = start + ?3 WHERE source_key = ?1",
                params![child_key, parent_key, offset as i64],
            )
        })?;
        Ok(())
    }

    /// Moves everything stored under `old` to `new`: a source whose key gained its GUID. Does nothing when
    /// something is already stored under `new`, so a second disk of the same model never takes the first's.
    pub fn rekey(&self, old: &str, new: &str) -> Result<bool> {
        if old == new {
            return Ok(false);
        }
        let in_journal = |key: &str| -> Result<i64> {
            Ok(self.with_journal(|connection| {
                connection.query_row(
                    "SELECT count(*) FROM read_journal WHERE source_key = ?1",
                    [key],
                    |row| row.get(0),
                )
            })?)
        };
        let (journal_new, journal_old) = (in_journal(new)?, in_journal(old)?);
        let moved = self.patiently(|catalog| {
            let transaction = catalog.connection().unchecked_transaction()?;
            let taken: i64 = transaction.query_row(
                "SELECT (SELECT count(*) FROM health_ranges WHERE source_key = ?1)
                  + (SELECT count(*) FROM health_flags WHERE source_key = ?1)",
                [new],
                |row| row.get(0),
            )?;
            let has: i64 = transaction.query_row(
                "SELECT (SELECT count(*) FROM health_ranges WHERE source_key = ?1)
                  + (SELECT count(*) FROM health_flags WHERE source_key = ?1)
                  + (SELECT count(*) FROM health_links WHERE parent_key = ?1)",
                [old],
                |row| row.get(0),
            )?;
            if taken + journal_new > 0 || has + journal_old == 0 {
                return Ok(false);
            }
            for table in ["health_ranges", "health_flags", "health_probes"] {
                transaction.execute(
                    &format!("UPDATE {table} SET source_key = ?2 WHERE source_key = ?1"),
                    params![old, new],
                )?;
            }
            transaction.execute(
                "UPDATE health_links SET parent_key = ?2 WHERE parent_key = ?1",
                params![old, new],
            )?;
            transaction.commit()?;
            Ok(true)
        })?;
        if !moved {
            return Ok(false);
        }
        self.patiently_in_journal(|connection| {
            connection.execute(
                "UPDATE read_journal SET source_key = ?2 WHERE source_key = ?1",
                params![old, new],
            )
        })?;
        Ok(true)
    }

    /// The reads of `key` that ended well, most recent first: ranges known to read, from earlier opens.
    pub fn good_reads(&self, key: &str, limit: usize) -> Result<Vec<(u64, u64)>> {
        Ok(self.with_journal(|connection| {
            let mut statement = connection.prepare(
                "SELECT start, length FROM read_journal WHERE source_key = ?1 AND outcome = 'ok' ORDER BY id DESC LIMIT ?2",
            )?;
            let rows = statement.query_map(params![key, limit as i64], |row| {
                Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?)
    }

    /// Removes the reset-culprit ranges that were recorded for a read made after the device had already gone:
    /// the read came back at once (under `min_ms`), so it cannot have been the one in flight when the
    /// device went away. Only the ranges the guard recorded for such a read, by their note, are removed:
    /// ones the person marked, and ones found from a read that never finished, are never touched.
    pub fn drop_instant_culprits(&self, key: &str, min_ms: u64) -> Result<usize> {
        Ok(self.patiently(|catalog| {
            catalog.connection().execute(
                "DELETE FROM health_ranges WHERE source_key = ?1 AND state = 'reset-culprit'
                 AND latency_ms IS NOT NULL AND latency_ms < ?2 AND note LIKE 'the device went away during a read of this range%'",
                params![key, min_ms as i64],
            )
        })?)
    }

    /// Adds `resets` to the count and returns the new count.
    pub fn add_resets(&self, key: &str, resets: u64) -> Result<u64> {
        Ok(self.patiently(|catalog| {
            let transaction = catalog.connection().unchecked_transaction()?;
            transaction.execute(
                "INSERT INTO health_flags (source_key, resets, updated_millis) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_key) DO UPDATE SET resets = resets + excluded.resets, updated_millis = excluded.updated_millis",
                params![key, resets as i64, now_millis() as i64],
            )?;
            let total = transaction.query_row(
                "SELECT resets FROM health_flags WHERE source_key = ?1",
                [key],
                |row| row.get::<_, i64>(0),
            )?;
            transaction.commit()?;
            Ok(total)
        })? as u64)
    }

    pub fn set_resets(&self, key: &str, resets: u64) -> Result<()> {
        self.patiently(|catalog| {
            catalog.connection().execute(
                "INSERT INTO health_flags (source_key, resets, updated_millis) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_key) DO UPDATE SET resets = excluded.resets, updated_millis = excluded.updated_millis",
                params![key, resets as i64, now_millis() as i64],
            )
        })?;
        Ok(())
    }

    pub fn set_failing_after(&self, key: &str, after: u64) -> Result<()> {
        self.patiently(|catalog| {
            catalog.connection().execute(
                "INSERT INTO health_flags (source_key, failing_after, updated_millis) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_key) DO UPDATE SET failing_after = excluded.failing_after, updated_millis = excluded.updated_millis",
                params![key, after as i64, now_millis() as i64],
            )
        })?;
        Ok(())
    }

    pub fn load_probe(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .lock()
            .connection()
            .query_row(
                "SELECT state FROM health_probes WHERE source_key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn save_probe(&self, key: &str, state: &str) -> Result<()> {
        self.patiently(|catalog| {
            catalog.connection().execute(
                "INSERT INTO health_probes (source_key, state, updated_millis) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source_key) DO UPDATE SET state = excluded.state, updated_millis = excluded.updated_millis",
                params![key, state, now_millis() as i64],
            )
        })?;
        Ok(())
    }

    pub fn clear_probe(&self, key: &str) -> Result<()> {
        self.patiently(|catalog| {
            catalog
                .connection()
                .execute("DELETE FROM health_probes WHERE source_key = ?1", [key])
        })?;
        Ok(())
    }

    /// Every source key with a map or an open row, for a listing.
    pub fn keys(&self) -> Result<HashSet<String>> {
        let mut keys: HashSet<String> = {
            let catalog = self.lock();
            let mut statement = catalog
                .connection()
                .prepare("SELECT DISTINCT source_key FROM health_ranges")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<HashSet<_>>>()?
        };
        keys.extend(self.with_journal(|connection| {
            let mut statement =
                connection.prepare("SELECT DISTINCT source_key FROM read_journal")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<rusqlite::Result<Vec<_>>>()
        })?);
        Ok(keys)
    }
}

fn range_row(row: &rusqlite::Row) -> rusqlite::Result<Option<HealthRange>> {
    let state: String = row.get(2)?;
    Ok(RangeState::parse(&state).map(|state| HealthRange {
        start: row.get::<_, i64>(0).unwrap_or(0) as u64,
        end: row.get::<_, i64>(1).unwrap_or(0) as u64,
        state,
        latency_ms: row
            .get::<_, Option<i64>>(3)
            .ok()
            .flatten()
            .map(|ms| ms as u64),
        note: row.get(4).unwrap_or_default(),
        found_millis: row.get::<_, i64>(5).unwrap_or(0) as u64,
    }))
}
