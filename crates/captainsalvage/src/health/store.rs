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

    pub fn load_map(&self, key: &str) -> Result<HealthMap> {
        let catalog = self.lock();
        let mut statement = catalog.connection().prepare(
            "SELECT start, end, state, latency_ms, note FROM health_ranges WHERE source_key = ?1 ORDER BY start",
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
            "SELECT start, end, state, latency_ms, note FROM health_ranges
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
        let now = now_millis() as i64;
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
                    now
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
    }))
}
