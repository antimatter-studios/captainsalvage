//! What the read journal and the health map keep in the catalog.
//!
//! The journal is written BEFORE a read starts (`started`) and after it ends (`done`). A device
//! that resets during a read takes the reader with it, or the person quits the app: the row of
//! that read is left without an end, and the next open reads it as the cause.
//!
//! Rows are keyed by the source's key (see [`super::source_key`]), which comes from the source's
//! bytes and not from its /dev path, so the same drive is the same after it comes back as another
//! disk. Once the catalog knows which stored source it is, the rows also carry its ID.

use super::map::{HealthMap, HealthRange, RangeState};
use crate::catalog::Catalog;
use crate::workspace::now_millis;
use anyhow::Result;
use rusqlite::{params, OptionalExtension};
use std::collections::HashSet;
use std::path::Path;
use std::sync::{Mutex, MutexGuard};

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

pub struct HealthStore {
    catalog: Mutex<Catalog>,
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

    /// The journal writes twice for a read, so its own connection does not wait for the disk on each
    /// commit: what it writes survives the app being stopped, which is what it is for. (The catalog's
    /// other connections keep full synchronous commits.)
    fn with_catalog(catalog: Catalog) -> Result<Self> {
        catalog
            .connection()
            .execute_batch("PRAGMA synchronous = NORMAL;")?;
        Ok(Self {
            catalog: Mutex::new(catalog),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Catalog> {
        self.catalog
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Writes that a read of `start..start + length` is about to start. Returns its row.
    pub fn started(&self, key: &str, owner: &str, start: u64, length: u64) -> Result<i64> {
        let catalog = self.lock();
        catalog.connection().execute(
            "INSERT INTO read_journal (source_key, owner, start, length, started_millis)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![key, owner, start as i64, length as i64, now_millis() as i64],
        )?;
        Ok(catalog.connection().last_insert_rowid())
    }

    /// Writes that the read ended: how long it took and how it ended (`ok`, `error` or `gone`).
    pub fn done(&self, id: i64, elapsed_ms: u64, outcome: &str) -> Result<()> {
        self.lock().connection().execute(
            "UPDATE read_journal SET finished_millis = ?2, elapsed_ms = ?3, outcome = ?4 WHERE id = ?1",
            params![id, now_millis() as i64, elapsed_ms as i64, outcome],
        )?;
        Ok(())
    }

    /// The reads of `key` that started and never ended, and are not in flight now.
    pub fn abandoned(&self, key: &str) -> Result<Vec<OpenRead>> {
        let catalog = self.lock();
        let mut statement = catalog.connection().prepare(
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
        let all = rows.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(all
            .into_iter()
            .filter(|read| !super::owner_is_alive(&read.owner))
            .collect())
    }

    /// Closes the rows of reads that never ended, so each is counted once.
    pub fn settle(&self, ids: &[i64], outcome: &str) -> Result<()> {
        let catalog = self.lock();
        for id in ids {
            catalog.connection().execute(
                "UPDATE read_journal SET finished_millis = ?2, outcome = ?3 WHERE id = ?1",
                params![id, now_millis() as i64, outcome],
            )?;
        }
        Ok(())
    }

    /// The most recent rows of a source, newest first.
    pub fn recent(&self, key: &str, limit: usize) -> Result<Vec<serde_json::Value>> {
        let catalog = self.lock();
        let mut statement = catalog.connection().prepare(
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
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Drops old rows of reads that went well.
    pub fn prune(&self, key: &str) -> Result<()> {
        self.lock().connection().execute(
            "DELETE FROM read_journal WHERE source_key = ?1 AND outcome = 'ok'
             AND id <= (SELECT coalesce(max(id), 0) FROM read_journal WHERE source_key = ?1) - ?2",
            params![key, KEPT_GOOD_ROWS],
        )?;
        Ok(())
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
        let catalog = self.lock();
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
        transaction.commit()?;
        Ok(())
    }

    /// Tells the rows of `key` which stored source they are about.
    pub fn adopt(&self, key: &str, source_id: &str) -> Result<()> {
        let catalog = self.lock();
        for table in ["health_ranges", "read_journal"] {
            catalog.connection().execute(
                &format!("UPDATE {table} SET source_id = ?2 WHERE source_key = ?1"),
                params![key, source_id],
            )?;
        }
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
        let catalog = self.lock();
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
        // Its journal rows (an open one is a culprit for the disk too), in the disk's offsets.
        transaction.execute(
            "UPDATE read_journal SET source_key = ?2, start = start + ?3 WHERE source_key = ?1",
            params![child_key, parent_key, offset as i64],
        )?;
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
        transaction.commit()?;
        Ok(())
    }

    /// Moves everything stored under `old` to `new`: a source whose key gained its GUID. Does nothing when
    /// something is already stored under `new`, so a second disk of the same model never takes the first's.
    pub fn rekey(&self, old: &str, new: &str) -> Result<bool> {
        if old == new {
            return Ok(false);
        }
        let catalog = self.lock();
        let transaction = catalog.connection().unchecked_transaction()?;
        let taken: i64 = transaction.query_row(
            "SELECT (SELECT count(*) FROM health_ranges WHERE source_key = ?1)
                  + (SELECT count(*) FROM health_flags WHERE source_key = ?1)
                  + (SELECT count(*) FROM read_journal WHERE source_key = ?1)",
            [new],
            |row| row.get(0),
        )?;
        let has: i64 = transaction.query_row(
            "SELECT (SELECT count(*) FROM health_ranges WHERE source_key = ?1)
                  + (SELECT count(*) FROM health_flags WHERE source_key = ?1)
                  + (SELECT count(*) FROM read_journal WHERE source_key = ?1)
                  + (SELECT count(*) FROM health_links WHERE parent_key = ?1)",
            [old],
            |row| row.get(0),
        )?;
        if taken > 0 || has == 0 {
            return Ok(false);
        }
        for table in [
            "health_ranges",
            "health_flags",
            "read_journal",
            "health_probes",
        ] {
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
    }

    /// The reads of `key` that ended well, most recent first: ranges known to read, from earlier opens.
    pub fn good_reads(&self, key: &str, limit: usize) -> Result<Vec<(u64, u64)>> {
        let catalog = self.lock();
        let mut statement = catalog.connection().prepare(
            "SELECT start, length FROM read_journal WHERE source_key = ?1 AND outcome = 'ok' ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![key, limit as i64], |row| {
            Ok((row.get::<_, i64>(0)? as u64, row.get::<_, i64>(1)? as u64))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Removes the reset-culprit ranges that were recorded for a read made after the device had already gone:
    /// the read came back at once (under `min_ms`), so it cannot have been the one in flight when the
    /// device went away. Only the ranges the guard recorded for such a read, by their note, are removed:
    /// ones the person marked, and ones found from a read that never finished, are never touched.
    pub fn drop_instant_culprits(&self, key: &str, min_ms: u64) -> Result<usize> {
        Ok(self.lock().connection().execute(
            "DELETE FROM health_ranges WHERE source_key = ?1 AND state = 'reset-culprit'
             AND latency_ms IS NOT NULL AND latency_ms < ?2 AND note LIKE 'the device went away during a read of this range%'",
            params![key, min_ms as i64],
        )?)
    }

    /// Adds `resets` to the count and returns the new count.
    pub fn add_resets(&self, key: &str, resets: u64) -> Result<u64> {
        let catalog = self.lock();
        catalog.connection().execute(
            "INSERT INTO health_flags (source_key, resets, updated_millis) VALUES (?1, ?2, ?3)
             ON CONFLICT(source_key) DO UPDATE SET resets = resets + excluded.resets, updated_millis = excluded.updated_millis",
            params![key, resets as i64, now_millis() as i64],
        )?;
        Ok(catalog.connection().query_row(
            "SELECT resets FROM health_flags WHERE source_key = ?1",
            [key],
            |row| row.get::<_, i64>(0),
        )? as u64)
    }

    pub fn set_resets(&self, key: &str, resets: u64) -> Result<()> {
        self.lock().connection().execute(
            "INSERT INTO health_flags (source_key, resets, updated_millis) VALUES (?1, ?2, ?3)
             ON CONFLICT(source_key) DO UPDATE SET resets = excluded.resets, updated_millis = excluded.updated_millis",
            params![key, resets as i64, now_millis() as i64],
        )?;
        Ok(())
    }

    pub fn set_failing_after(&self, key: &str, after: u64) -> Result<()> {
        self.lock().connection().execute(
            "INSERT INTO health_flags (source_key, failing_after, updated_millis) VALUES (?1, ?2, ?3)
             ON CONFLICT(source_key) DO UPDATE SET failing_after = excluded.failing_after, updated_millis = excluded.updated_millis",
            params![key, after as i64, now_millis() as i64],
        )?;
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
        self.lock().connection().execute(
            "INSERT INTO health_probes (source_key, state, updated_millis) VALUES (?1, ?2, ?3)
             ON CONFLICT(source_key) DO UPDATE SET state = excluded.state, updated_millis = excluded.updated_millis",
            params![key, state, now_millis() as i64],
        )?;
        Ok(())
    }

    pub fn clear_probe(&self, key: &str) -> Result<()> {
        self.lock()
            .connection()
            .execute("DELETE FROM health_probes WHERE source_key = ?1", [key])?;
        Ok(())
    }

    /// Every source key with a map or an open row, for a listing.
    pub fn keys(&self) -> Result<HashSet<String>> {
        let catalog = self.lock();
        let mut statement = catalog
            .connection()
            .prepare("SELECT DISTINCT source_key FROM health_ranges UNION SELECT DISTINCT source_key FROM read_journal")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        Ok(rows.collect::<rusqlite::Result<HashSet<_>>>()?)
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
