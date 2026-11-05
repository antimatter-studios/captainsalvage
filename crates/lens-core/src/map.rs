//! The SQLite map of everything the scan found: storage and queries.
//!
//! exFAT entries have no parent pointers. A folder's parent is recoverable only
//! because some other directory holds an entry whose first cluster is this
//! folder's first cluster. When that parent is gone, the folder is an orphan.
//! `resolve.rs` holds the logic that infers that structure.

use crate::content::{is_signature_class, CLASS_DIRECTORY, CLASS_FILL, CLASS_NAMES, CLASS_ZEROS};
use crate::exfat::EntrySet;
use crate::resolve::parse_extents_payload;
use crate::volume::Volume;
use crate::workspace::now_millis;
use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

pub const VERDICT_LIVE: &str = "live";
pub const VERDICT_INTACT: &str = "likely-intact";
pub const VERDICT_PLAUSIBLE: &str = "plausible";
pub const VERDICT_PARTIAL: &str = "partial";
pub const VERDICT_TESTED_GOOD: &str = "tested-good";
pub const VERDICT_TESTED_BAD: &str = "tested-bad";
/// A file that passed its test is trusted, and one that failed is not, whatever else says.
pub const TESTED_GOOD_CONFIDENCE: f64 = 0.99;
pub const TESTED_BAD_CONFIDENCE: f64 = 0.1;

/// A file waiting to be tested.
pub struct FileTestCandidate {
    pub id: i64,
    pub name: String,
    pub first_cluster: u32,
    pub length: u64,
}
pub const VERDICT_OVERWRITTEN: &str = "overwritten";
pub const VERDICT_EMPTY: &str = "empty";
pub const VERDICT_RESIDUE: &str = "rename-residue";
pub const VERDICT_DAMAGED: &str = "damaged-entry";
pub const VERDICT_FOLDER: &str = "folder";
pub const VERDICT_FOLDER_EMPTY: &str = "folder-empty";
pub const VERDICT_FOLDER_MISSING: &str = "folder-missing";

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS scan_state (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS sets (
    id INTEGER PRIMARY KEY,
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    name TEXT NOT NULL,
    in_use INTEGER NOT NULL,
    checksum_ok INTEGER NOT NULL,
    attributes INTEGER NOT NULL,
    is_dir INTEGER NOT NULL,
    first_cluster INTEGER NOT NULL,
    length INTEGER NOT NULL,
    valid_length INTEGER NOT NULL,
    no_fat_chain INTEGER NOT NULL,
    created TEXT,
    modified TEXT,
    accessed TEXT,
    node INTEGER,
    verdict TEXT,
    reason TEXT,
    clusters_needed INTEGER,
    clusters_found INTEGER,
    clusters_allocated INTEGER,
    clusters_conflict INTEGER,
    UNIQUE (source_cluster, source_offset)
);
CREATE INDEX IF NOT EXISTS sets_first ON sets (first_cluster);
CREATE INDEX IF NOT EXISTS sets_node ON sets (node);
CREATE TABLE IF NOT EXISTS file_tests (
    id INTEGER PRIMARY KEY,
    first_cluster INTEGER NOT NULL,
    length INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    format TEXT NOT NULL,
    valid_fraction REAL,
    first_break INTEGER,
    decoded INTEGER,
    note TEXT NOT NULL,
    sha256 TEXT,
    tested_at_millis INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS file_tests_sha256 ON file_tests (sha256);
CREATE TABLE IF NOT EXISTS read_errors (cluster INTEGER PRIMARY KEY, message TEXT);
CREATE TABLE IF NOT EXISTS nodes (
    head INTEGER PRIMARY KEY,
    parent INTEGER,
    name TEXT,
    kind TEXT NOT NULL,
    modified TEXT,
    files INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    intact_files INTEGER NOT NULL,
    intact_bytes INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes (parent);
CREATE TABLE IF NOT EXISTS overlays (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    target INTEGER,
    payload TEXT NOT NULL,
    note TEXT NOT NULL,
    author TEXT NOT NULL,
    active INTEGER NOT NULL,
    created_at_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS islands (
    head INTEGER PRIMARY KEY,
    tail INTEGER NOT NULL,
    length INTEGER NOT NULL,
    allocated INTEGER NOT NULL,
    end_kind TEXT NOT NULL,
    owners INTEGER NOT NULL,
    head_class INTEGER NOT NULL,
    mean_entropy INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS islands_length ON islands (length);
CREATE TABLE IF NOT EXISTS basket (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    target INTEGER NOT NULL,
    position INTEGER NOT NULL,
    added_at_millis INTEGER NOT NULL,
    UNIQUE (kind, target)
);
CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY,
    at_millis INTEGER NOT NULL,
    author TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL
);
";

/// Columns added after the first schema. The `o_` columns hold overlay
/// corrections to an entry: raw values from the disk stay untouched beside them,
/// and every resolve recalculates them.
const ADDED_COLUMNS: &[(&str, &str, &str)] = &[
    ("sets", "o_name", "TEXT"),
    ("sets", "o_first", "INTEGER"),
    ("sets", "o_length", "INTEGER"),
    ("sets", "o_no_fat", "INTEGER"),
    ("sets", "o_is_dir", "INTEGER"),
    ("sets", "o_ok", "INTEGER"),
    ("sets", "confidence", "REAL"),
    ("nodes", "link_confidence", "REAL"),
];

pub const OVERLAY_ACCEPT_ENTRY: &str = "accept_entry";
pub const OVERLAY_FILE_EXTENTS: &str = "file_extents";
pub const OVERLAY_FOLDER_PARENT: &str = "folder_parent";
pub const OVERLAY_DIRECTORY_CLUSTER: &str = "directory_cluster";
pub const OVERLAY_NOTE: &str = "note";
pub const OVERLAY_STRUCT: &str = "struct";
pub const OVERLAY_LENS: &str = "lens";
pub const OVERLAY_REGION_LABEL: &str = "region_label";

/// A hypothesis about the volume. Overlays change how the map interprets the
/// disk. They never change the disk.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Overlay {
    pub id: i64,
    pub kind: String,
    pub target: Option<i64>,
    pub payload: serde_json::Value,
    pub note: String,
    pub author: String,
    pub active: bool,
    pub created_at_millis: u64,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ResolveParams {
    /// When a FAT chain ends early, assume the rest of the file is contiguous.
    pub chain_fallback_contiguous: bool,
    /// Clusters a file may share with other entries and still count as whole.
    pub conflict_tolerance_clusters: u64,
    /// Ignore overlaps between found entries. Clusters in current use still count.
    pub ignore_entry_conflicts: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ActivityItem {
    pub id: i64,
    pub at_millis: u64,
    pub author: String,
    pub action: String,
    pub detail: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Extent {
    pub first: u32,
    pub count: u32,
}

/// Run-length encodes a cluster list for display and storage.
pub fn to_extents(clusters: &[u32]) -> Vec<Extent> {
    let mut extents: Vec<Extent> = Vec::new();
    for &cluster in clusters {
        match extents.last_mut() {
            Some(last) if last.first + last.count == cluster => last.count += 1,
            _ => extents.push(Extent {
                first: cluster,
                count: 1,
            }),
        }
    }
    extents
}

pub fn from_extents(extents: &[Extent]) -> Vec<u32> {
    extents
        .iter()
        .flat_map(|extent| extent.first..extent.first.saturating_add(extent.count))
        .collect()
}

/// Largest contiguous file assumed when looking for owners of a cluster: 16 GiB.
const OWNER_LOOKBACK_BYTES: u64 = 16 * 1024 * 1024 * 1024;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VerdictTotal {
    pub verdict: String,
    pub count: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MapSummary {
    pub verdicts: Vec<VerdictTotal>,
    pub nodes_by_kind: Vec<(String, u64)>,
    pub resolved_at_millis: u64,
    pub nameless_chains: u64,
    pub nameless_clusters: u64,
    pub broken_nameless_chains: u64,
    pub unexplained_free_clusters: u64,
    pub labelled_free_clusters: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Row {
    pub kind: String,
    pub id: Option<i64>,
    pub head: Option<u32>,
    pub name: String,
    pub path: Option<String>,
    pub length: u64,
    pub valid_length: u64,
    pub in_use: bool,
    pub checksum_ok: bool,
    pub verdict: String,
    pub reason: String,
    pub first_cluster: u32,
    pub no_fat_chain: bool,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub source_cluster: Option<u32>,
    pub source_offset: Option<u32>,
    pub clusters_needed: u64,
    pub clusters_found: u64,
    pub clusters_allocated: u64,
    pub clusters_conflict: u64,
    pub files: u64,
    pub bytes: u64,
    pub intact_files: u64,
    pub intact_bytes: u64,
    pub node_kind: Option<String>,
    pub hint: Option<String>,
    pub confidence: f64,
    pub link_confidence: Option<f64>,
}

/// A FAT chain, with what is known about who owns it and what it holds.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct IslandRow {
    pub head: u32,
    pub tail: u32,
    pub length: u64,
    pub allocated: u64,
    pub end_kind: String,
    pub owners: u64,
    pub head_class: u8,
    pub head_class_name: String,
    pub mean_entropy: u8,
}

/// A chain that might continue a file whose own chain ends early.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BridgeCandidate {
    pub island: IslandRow,
    pub score: f64,
    pub size_fit: f64,
    pub proximity: f64,
    pub entropy_fit: f64,
    pub reason: String,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub rows: Vec<Row>,
    pub total: u64,
}

pub struct MapDb {
    pub(crate) connection: Connection,
}

const FILE_COLUMNS: &str = "id, COALESCE(o_name, name), COALESCE(o_length, length), \
    COALESCE(o_length, valid_length), in_use, COALESCE(o_ok, checksum_ok), COALESCE(verdict, ''), \
    COALESCE(reason, ''), COALESCE(o_first, first_cluster), COALESCE(o_no_fat, no_fat_chain), created, modified, \
    source_cluster, source_offset, COALESCE(clusters_needed, 0), COALESCE(clusters_found, 0), \
    COALESCE(clusters_allocated, 0), COALESCE(clusters_conflict, 0), COALESCE(o_is_dir, is_dir), \
    COALESCE(confidence, 0)";

/// Position of `node` when a query appends it to `FILE_COLUMNS`.
const FILE_NODE_INDEX: usize = 20;

fn file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let is_dir: bool = row.get(18)?;
    Ok(Row {
        kind: if is_dir {
            "missing-folder".into()
        } else {
            "file".into()
        },
        id: Some(row.get(0)?),
        name: row.get(1)?,
        length: row.get::<_, i64>(2)? as u64,
        valid_length: row.get::<_, i64>(3)? as u64,
        in_use: row.get(4)?,
        checksum_ok: row.get(5)?,
        verdict: row.get(6)?,
        reason: row.get(7)?,
        first_cluster: row.get(8)?,
        no_fat_chain: row.get(9)?,
        created: row.get(10)?,
        modified: row.get(11)?,
        source_cluster: Some(row.get(12)?),
        source_offset: Some(row.get(13)?),
        clusters_needed: row.get::<_, i64>(14)? as u64,
        clusters_found: row.get::<_, i64>(15)? as u64,
        clusters_allocated: row.get::<_, i64>(16)? as u64,
        clusters_conflict: row.get::<_, i64>(17)? as u64,
        confidence: row.get(19)?,
        ..Row::default()
    })
}

const NODE_COLUMNS: &str =
    "head, COALESCE(name, ''), kind, modified, files, bytes, intact_files, intact_bytes, link_confidence";

fn node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
    let head: u32 = row.get(0)?;
    Ok(Row {
        kind: "folder".into(),
        head: Some(head),
        name: row.get(1)?,
        node_kind: Some(row.get(2)?),
        modified: row.get(3)?,
        files: row.get::<_, i64>(4)? as u64,
        bytes: row.get::<_, i64>(5)? as u64,
        intact_files: row.get::<_, i64>(6)? as u64,
        intact_bytes: row.get::<_, i64>(7)? as u64,
        link_confidence: row.get(8)?,
        first_cluster: head,
        verdict: VERDICT_FOLDER.into(),
        ..Row::default()
    })
}

impl MapDb {
    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(300))?;
        connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        connection.execute_batch("PRAGMA synchronous = NORMAL;")?;
        connection.execute_batch(SCHEMA)?;
        let database = Self { connection };
        database.add_missing_columns()?;
        Ok(database)
    }

    fn add_missing_columns(&self) -> Result<()> {
        for (table, column, kind) in ADDED_COLUMNS {
            let mut statement = self
                .connection
                .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?;
            let existing = statement
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<HashSet<String>>>()?;
            if !existing.contains(*column) {
                self.connection
                    .execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {kind};"))?;
            }
        }
        Ok(())
    }

    pub fn add_overlay(
        &self,
        kind: &str,
        target: Option<i64>,
        payload: &serde_json::Value,
        note: &str,
        author: &str,
    ) -> Result<i64> {
        const KINDS: &[&str] = &[
            OVERLAY_ACCEPT_ENTRY,
            OVERLAY_FILE_EXTENTS,
            OVERLAY_FOLDER_PARENT,
            OVERLAY_DIRECTORY_CLUSTER,
            OVERLAY_NOTE,
            OVERLAY_STRUCT,
            OVERLAY_LENS,
            OVERLAY_REGION_LABEL,
        ];
        if !KINDS.contains(&kind) {
            anyhow::bail!(
                "unknown overlay kind {kind}. Valid kinds: {}",
                KINDS.join(", ")
            );
        }
        self.connection.execute(
            "INSERT INTO overlays (kind, target, payload, note, author, active, created_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, 1, ?6)",
            params![kind, target, payload.to_string(), note, author, now_millis() as i64],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn overlays(&self, only_active: bool) -> Result<Vec<Overlay>> {
        let filter = if only_active { "WHERE active = 1" } else { "" };
        let sql = format!(
            "SELECT id, kind, target, payload, note, author, active, created_at_millis FROM overlays {filter} ORDER BY id DESC"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement.query_map([], |row| {
            let payload: String = row.get(3)?;
            Ok(Overlay {
                id: row.get(0)?,
                kind: row.get(1)?,
                target: row.get(2)?,
                payload: serde_json::from_str(&payload).unwrap_or(serde_json::Value::Null),
                note: row.get(4)?,
                author: row.get(5)?,
                active: row.get(6)?,
                created_at_millis: row.get::<_, i64>(7)? as u64,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn set_overlay_active(&self, id: i64, active: bool) -> Result<bool> {
        Ok(self.connection.execute(
            "UPDATE overlays SET active = ?2 WHERE id = ?1",
            params![id, active],
        )? > 0)
    }

    pub fn delete_overlay(&self, id: i64) -> Result<bool> {
        Ok(self
            .connection
            .execute("DELETE FROM overlays WHERE id = ?1", [id])?
            > 0)
    }

    pub fn params(&self) -> Result<ResolveParams> {
        Ok(self
            .state("resolve_params")?
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default())
    }

    pub fn set_params(&self, params: &ResolveParams) -> Result<()> {
        self.set_state("resolve_params", &serde_json::to_string(params)?)
    }

    pub fn log_activity(&self, author: &str, action: &str, detail: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO activity (at_millis, author, action, detail) VALUES (?1, ?2, ?3, ?4)",
            params![now_millis() as i64, author, action, detail],
        )?;
        Ok(())
    }

    pub fn activity(&self, after_id: i64, limit: u64) -> Result<Vec<ActivityItem>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT id, at_millis, author, action, detail FROM activity WHERE id > ?1 ORDER BY id DESC LIMIT ?2",
        )?;
        let rows = statement.query_map(params![after_id, limit as i64], |row| {
            Ok(ActivityItem {
                id: row.get(0)?,
                at_millis: row.get::<_, i64>(1)? as u64,
                author: row.get(2)?,
                action: row.get(3)?,
                detail: row.get(4)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// The corrected cluster list an active overlay assigns to an entry, if any.
    pub fn overlay_clusters(&self, entry_id: i64) -> Result<Option<Vec<u32>>> {
        let payload: Option<String> = self
            .connection
            .query_row(
                "SELECT payload FROM overlays WHERE kind = ?1 AND target = ?2 AND active = 1 ORDER BY id DESC LIMIT 1",
                params![OVERLAY_FILE_EXTENTS, entry_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(payload
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| parse_extents_payload(&value)))
    }

    pub fn state(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT value FROM scan_state WHERE key = ?1",
                [key],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn state_u64(&self, key: &str) -> Result<u64> {
        Ok(self
            .state(key)?
            .and_then(|value| value.parse().ok())
            .unwrap_or(0))
    }

    pub fn set_state(&self, key: &str, value: &str) -> Result<()> {
        self.connection.execute(
            "INSERT INTO scan_state (key, value) VALUES (?1, ?2) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    /// Stores one chunk's results atomically with the scan position, so a resume
    /// from any point repeats at most one chunk and creates no duplicates.
    pub fn commit_chunk(
        &mut self,
        found: &[(u32, EntrySet)],
        errors: &[(u32, String)],
        state: &[(&str, String)],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT OR IGNORE INTO sets (source_cluster, source_offset, name, in_use, checksum_ok, attributes, \
                 is_dir, first_cluster, length, valid_length, no_fat_chain, created, modified, accessed) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            )?;
            for (cluster, set) in found {
                insert.execute(params![
                    cluster,
                    set.offset as i64,
                    set.name,
                    set.in_use,
                    set.checksum_ok,
                    set.attributes,
                    set.is_dir,
                    set.first_cluster,
                    set.length as i64,
                    set.valid_length as i64,
                    set.no_fat_chain,
                    set.created,
                    set.modified,
                    set.accessed,
                ])?;
            }
            let mut error_insert = transaction.prepare_cached(
                "INSERT OR REPLACE INTO read_errors (cluster, message) VALUES (?1, ?2)",
            )?;
            for (cluster, message) in errors {
                error_insert.execute(params![cluster, message])?;
            }
            let mut state_insert = transaction.prepare_cached(
                "INSERT INTO scan_state (key, value) VALUES (?1, ?2) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )?;
            for (key, value) in state {
                state_insert.execute(params![key, value])?;
            }
        }
        transaction.commit()?;
        Ok(())
    }

    pub(crate) fn compute_summary(&self) -> Result<MapSummary> {
        let mut statement = self.connection.prepare(
            "SELECT COALESCE(verdict, 'unresolved'), COUNT(*), COALESCE(SUM(length), 0) FROM sets GROUP BY 1 ORDER BY 3 DESC",
        )?;
        let verdicts = statement
            .query_map([], |row| {
                Ok(VerdictTotal {
                    verdict: row.get(0)?,
                    count: row.get::<_, i64>(1)? as u64,
                    bytes: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut statement = self
            .connection
            .prepare("SELECT kind, COUNT(*) FROM nodes GROUP BY 1")?;
        let nodes_by_kind = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let (nameless_chains, nameless_clusters, broken_nameless_chains): (i64, i64, i64) = self.connection.query_row(
            "SELECT COUNT(*), COALESCE(SUM(length), 0), COALESCE(SUM(end_kind != 'eof'), 0) FROM islands \
             WHERE owners = 0 AND allocated = 0",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        Ok(MapSummary {
            verdicts,
            nodes_by_kind,
            resolved_at_millis: now_millis(),
            nameless_chains: nameless_chains as u64,
            nameless_clusters: nameless_clusters as u64,
            broken_nameless_chains: broken_nameless_chains as u64,
            unexplained_free_clusters: 0,
            labelled_free_clusters: 0,
        })
    }

    pub fn summary(&self) -> Result<Option<MapSummary>> {
        Ok(self
            .state("summary")?
            .and_then(|text| serde_json::from_str(&text).ok()))
    }

    /// Top-level folders: the volume root, then orphans with the most data first.
    pub fn roots(&self, offset: u64, limit: u64) -> Result<Page> {
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent IS NULL",
            [],
            |r| r.get(0),
        )?;
        let sql = format!(
            "SELECT {NODE_COLUMNS} FROM nodes WHERE parent IS NULL \
             ORDER BY kind = 'root' DESC, bytes DESC, head LIMIT ?1 OFFSET ?2"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let mut rows = statement
            .query_map(params![limit as i64, offset as i64], node_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(statement);
        for row in &mut rows {
            if let Some(head) = row.head {
                row.hint = self.hint(head)?;
            }
        }
        Ok(Page {
            rows,
            total: total as u64,
        })
    }

    fn hint(&self, node: u32) -> Result<Option<String>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT name FROM sets WHERE node = ?1 AND checksum_ok = 1 AND verdict != 'rename-residue' LIMIT 3",
        )?;
        let names = statement
            .query_map([node], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((!names.is_empty()).then(|| names.join(", ")))
    }

    /// A folder's children: subfolders and lost folders, then a page of files
    /// at or above the confidence floor.
    pub fn children(
        &self,
        node: u32,
        offset: u64,
        limit: u64,
        include_hidden: bool,
        min_confidence: f64,
    ) -> Result<Page> {
        let residue = if include_hidden {
            ""
        } else {
            "AND verdict != 'rename-residue'"
        };
        let floor = format!("AND COALESCE(confidence, 0) >= {}", min_confidence.max(0.0));
        let hidden = format!("{residue} {floor}");
        let mut rows = Vec::new();
        if offset == 0 {
            let sql = format!(
                "SELECT {NODE_COLUMNS} FROM nodes WHERE parent = ?1 ORDER BY bytes DESC, name"
            );
            let mut statement = self.connection.prepare_cached(&sql)?;
            rows.extend(
                statement
                    .query_map([node], node_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
            let sql = format!(
                "SELECT {FILE_COLUMNS} FROM sets WHERE node = ?1 AND is_dir = 1 \
                 AND verdict IN ('folder-missing', 'folder-empty') ORDER BY name"
            );
            let mut statement = self.connection.prepare_cached(&sql)?;
            rows.extend(
                statement
                    .query_map([node], file_row)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        let count_sql =
            format!("SELECT COUNT(*) FROM sets WHERE node = ?1 AND is_dir = 0 {hidden}");
        let total: i64 = self
            .connection
            .query_row(&count_sql, [node], |row| row.get(0))?;
        let sql = format!(
            "SELECT {FILE_COLUMNS} FROM sets WHERE node = ?1 AND is_dir = 0 {hidden} \
             ORDER BY name LIMIT ?2 OFFSET ?3"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        rows.extend(
            statement
                .query_map(params![node, limit as i64, offset as i64], file_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
        Ok(Page {
            rows,
            total: total as u64,
        })
    }

    pub fn entry(&self, id: i64) -> Result<Option<Row>> {
        let sql = format!("SELECT {FILE_COLUMNS}, node FROM sets WHERE id = ?1");
        let found = self
            .connection
            .query_row(&sql, [id], |row| {
                Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?))
            })
            .optional()?;
        let Some((mut row, node)) = found else {
            return Ok(None);
        };
        row.path = Some(self.path_of(node)?);
        Ok(Some(row))
    }

    pub fn node(&self, head: u32) -> Result<Option<Row>> {
        let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE head = ?1");
        let found = self
            .connection
            .query_row(&sql, [head], node_row)
            .optional()?;
        let Some(mut row) = found else {
            return Ok(None);
        };
        row.path = Some(self.path_of(Some(head))?);
        row.hint = self.hint(head)?;
        Ok(Some(row))
    }

    /// Readable path of a node. Orphan roots are shown as `[orphan @cluster]`.
    pub fn path_of(&self, node: Option<u32>) -> Result<String> {
        let mut parts = Vec::new();
        let mut current = node;
        let mut guard = 0;
        while let Some(head) = current {
            guard += 1;
            if guard > 256 {
                break;
            }
            let found: Option<(Option<String>, Option<u32>, String)> = self
                .connection
                .query_row(
                    "SELECT name, parent, kind FROM nodes WHERE head = ?1",
                    [head],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            let Some((name, parent, kind)) = found else {
                parts.push(format!("[unresolved @{head}]"));
                break;
            };
            parts.push(match (name, kind.as_str()) {
                (_, "root") => String::new(),
                (Some(name), _) => name,
                (None, _) => format!("[orphan @{head}]"),
            });
            current = parent;
        }
        parts.reverse();
        Ok(parts.join("/"))
    }

    pub fn search(&self, query: &str, limit: u64, min_confidence: f64) -> Result<Page> {
        let escaped = query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let pattern = format!("%{escaped}%");
        let sql = format!(
            "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_name, name) LIKE ?1 ESCAPE '\\' \
             AND COALESCE(verdict, '') != 'rename-residue' AND COALESCE(confidence, 0) >= ?3 \
             ORDER BY confidence DESC, length DESC LIMIT ?2"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let found = statement
            .query_map(params![pattern, limit as i64, min_confidence], |row| {
                Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(found.len());
        for (mut row, node) in found {
            row.path = Some(self.path_of(node)?);
            if row.kind == "missing-folder" && self.node_exists(row.first_cluster)? {
                row.kind = "folder".into();
                row.head = Some(row.first_cluster);
            }
            rows.push(row);
        }
        Ok(Page {
            total: rows.len() as u64,
            rows,
        })
    }

    fn node_exists(&self, head: u32) -> Result<bool> {
        Ok(self
            .connection
            .query_row("SELECT 1 FROM nodes WHERE head = ?1", [head], |_| Ok(()))
            .optional()?
            .is_some())
    }

    /// Entries whose claimed clusters include `cluster`.
    pub fn owners(&self, volume: &Volume, cluster: u32) -> Result<Vec<Row>> {
        let chain_head = volume.chain_head(cluster);
        let lookback = (OWNER_LOOKBACK_BYTES / volume.boot.cluster_size()) as u32;
        let sql = format!(
            "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_ok, checksum_ok) = 1 AND length > 0 AND \
             (first_cluster = ?1 OR o_first = ?1 OR (no_fat_chain = 1 AND first_cluster BETWEEN ?2 AND ?3)) \
             ORDER BY first_cluster = ?1 DESC, first_cluster DESC LIMIT 2000"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let found = statement
            .query_map(
                params![chain_head, cluster.saturating_sub(lookback), cluster],
                |row| Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let cluster_size = volume.boot.cluster_size();
        let mut rows = Vec::new();
        for (mut row, node) in found {
            let contains = if row.no_fat_chain {
                u64::from(cluster)
                    < u64::from(row.first_cluster) + row.length.div_ceil(cluster_size)
            } else {
                volume
                    .clusters_for(row.first_cluster, row.length, false)
                    .clusters
                    .contains(&cluster)
            };
            if contains {
                row.path = Some(self.path_of(node)?);
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Valid and damaged entry sets found in a specific cluster.
    /// Names and first clusters of files whose first cluster precedes `up_to`.
    pub fn file_names_before(&self, up_to: u32) -> Result<Vec<(String, u32)>> {
        let mut statement = self.connection.prepare(
            "SELECT name, first_cluster FROM sets WHERE is_dir = 0 AND checksum_ok = 1 AND length > 0 \
             AND first_cluster BETWEEN 2 AND ?1",
        )?;
        let rows = statement.query_map([up_to], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn sets_in_cluster(&self, cluster: u32) -> Result<u64> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM sets WHERE source_cluster = ?1",
            [cluster],
            |r| r.get(0),
        )?;
        Ok(count as u64)
    }

    fn island_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<IslandRow> {
        let head_class: u8 = row.get(6)?;
        Ok(IslandRow {
            head: row.get(0)?,
            tail: row.get(1)?,
            length: row.get::<_, i64>(2)? as u64,
            allocated: row.get::<_, i64>(3)? as u64,
            end_kind: row.get(4)?,
            owners: row.get::<_, i64>(5)? as u64,
            head_class,
            head_class_name: CLASS_NAMES
                .get(usize::from(head_class))
                .copied()
                .unwrap_or("unknown")
                .to_string(),
            mean_entropy: row.get(7)?,
        })
    }

    /// Free FAT chains no entry claims, longest first: possibly files whose
    /// entries are gone. `head_class` narrows the list to one content class.
    pub fn nameless_chains(
        &self,
        offset: u64,
        limit: u64,
        head_class: Option<u8>,
    ) -> Result<(Vec<IslandRow>, u64)> {
        let filter = head_class
            .map(|class| format!("AND head_class = {class}"))
            .unwrap_or_default();
        let total: i64 = self.connection.query_row(
            &format!("SELECT COUNT(*) FROM islands WHERE owners = 0 AND allocated = 0 {filter}"),
            [],
            |row| row.get(0),
        )?;
        let mut statement = self.connection.prepare_cached(&format!(
            "SELECT head, tail, length, allocated, end_kind, owners, head_class, mean_entropy FROM islands \
             WHERE owners = 0 AND allocated = 0 {filter} ORDER BY length DESC, head LIMIT ?1 OFFSET ?2"
        ))?;
        let rows = statement
            .query_map(params![limit as i64, offset as i64], Self::island_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok((rows, total as u64))
    }

    pub fn island(&self, head: u32) -> Result<Option<IslandRow>> {
        Ok(self
            .connection
            .query_row(
                "SELECT head, tail, length, allocated, end_kind, owners, head_class, mean_entropy FROM islands WHERE head = ?1",
                [head],
                Self::island_from_row,
            )
            .optional()?)
    }

    /// Files whose chains end early, most missing data first. These are the
    /// places where a trail stops and a route around might exist.
    pub fn broken_files(&self, offset: u64, limit: u64) -> Result<Vec<Row>> {
        let sql = format!(
            "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_is_dir, is_dir) = 0 \
             AND clusters_found < clusters_needed AND clusters_found > 0 \
             ORDER BY clusters_needed - clusters_found DESC LIMIT ?1 OFFSET ?2"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let found = statement
            .query_map(params![limit as i64, offset as i64], |row| {
                Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::with_capacity(found.len());
        for (mut row, node) in found {
            row.path = Some(self.path_of(node)?);
            rows.push(row);
        }
        Ok(rows)
    }

    /// Ranks nameless chains that could continue a file past the end of its
    /// chain. Scores are a guide for investigation, not evidence of a match.
    pub fn bridge_candidates(
        &self,
        volume: &Volume,
        id: i64,
        entropy: &[u8],
        limit: usize,
    ) -> Result<Vec<BridgeCandidate>> {
        let Some(entry) = self.entry(id)? else {
            anyhow::bail!("no entry with id {id}");
        };
        let clusters = self.entry_clusters(volume, &entry)?;
        let needed = entry.length.div_ceil(volume.boot.cluster_size());
        let remainder = needed.saturating_sub(clusters.len() as u64);
        if remainder == 0 {
            return Ok(Vec::new());
        }
        let tail = clusters.last().copied().unwrap_or(entry.first_cluster);
        let own_entropy = if clusters.is_empty() {
            None
        } else {
            let sum: u64 = clusters
                .iter()
                .filter_map(|&cluster| {
                    entropy.get((cluster - crate::exfat::FIRST_CLUSTER) as usize)
                })
                .map(|&value| u64::from(value))
                .sum();
            Some(sum as f64 / clusters.len() as f64)
        };

        let (islands, _) = self.nameless_chains(0, u64::MAX >> 1, None)?;
        let mut candidates: Vec<BridgeCandidate> = islands
            .into_iter()
            .filter(|island| island.head != tail && !clusters.contains(&island.head))
            .map(|island| score_bridge(island, tail, remainder, own_entropy))
            .collect();
        candidates.sort_by(|a, b| b.score.total_cmp(&a.score));
        candidates.truncate(limit);
        Ok(candidates)
    }

    /// The clusters this entry uses, after overlays. A chain may be shorter than
    /// the entry's length says it should be.
    pub fn entry_clusters(&self, volume: &Volume, entry: &Row) -> Result<Vec<u32>> {
        if let Some(id) = entry.id {
            if let Some(clusters) = self.overlay_clusters(id)? {
                return Ok(clusters);
            }
        }
        let mut stream = volume.clusters_for(entry.first_cluster, entry.length, entry.no_fat_chain);
        if self.params()?.chain_fallback_contiguous && !stream.clusters.is_empty() {
            let mut next = stream.clusters[stream.clusters.len() - 1] + 1;
            while (stream.clusters.len() as u64) < stream.needed && volume.valid_cluster(next) {
                stream.clusters.push(next);
                next += 1;
            }
        }
        Ok(stream.clusters)
    }

    /// Every file in a folder and the folders beneath it, with its path.
    /// Files with all their clusters, not yet tested, in disk order so reading stays fast.
    pub fn file_test_candidates(&self, max_bytes: u64) -> Result<Vec<FileTestCandidate>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, COALESCE(s.o_name, s.name), COALESCE(s.o_first, s.first_cluster), COALESCE(s.o_length, s.length) \
             FROM sets s LEFT JOIN file_tests t ON t.id = s.id \
             AND t.first_cluster = COALESCE(s.o_first, s.first_cluster) AND t.length = COALESCE(s.o_length, s.length) \
             WHERE t.id IS NULL AND COALESCE(s.o_is_dir, s.is_dir) = 0 \
             AND COALESCE(s.o_length, s.length) BETWEEN 1 AND ?1 \
             AND COALESCE(s.clusters_found, 0) >= COALESCE(s.clusters_needed, 0) \
             AND COALESCE(s.verdict, '') NOT IN ('rename-residue', 'overwritten', 'empty') \
             AND COALESCE(s.o_name, s.name) NOT LIKE '.\\_%' ESCAPE '\\' \
             ORDER BY 3",
        )?;
        let found = statement
            .query_map([max_bytes as i64], |row| {
                Ok(FileTestCandidate {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    first_cluster: row.get(2)?,
                    length: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    /// Stores results and applies them at once, so lists show them while a run continues.
    pub fn save_file_tests(&self, tests: &[crate::filetest::FileTest]) -> Result<()> {
        if tests.is_empty() {
            return Ok(());
        }
        let transaction = self.connection.unchecked_transaction()?;
        {
            let mut insert = transaction.prepare_cached(
                "INSERT OR REPLACE INTO file_tests (id, first_cluster, length, outcome, format, valid_fraction, first_break, decoded, note, sha256, tested_at_millis) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            )?;
            let mut good = transaction.prepare_cached(
                "UPDATE sets SET verdict = CASE WHEN verdict = ?2 THEN verdict ELSE ?3 END, confidence = MAX(COALESCE(confidence, 0), ?4), reason = ?5 || ' ' || COALESCE(reason, '') WHERE id = ?1",
            )?;
            let mut bad = transaction.prepare_cached(
                "UPDATE sets SET verdict = ?2, confidence = MIN(COALESCE(confidence, 0), ?3), reason = ?4 || ' ' || COALESCE(reason, '') WHERE id = ?1",
            )?;
            for test in tests {
                insert.execute(params![
                    test.id,
                    test.first_cluster,
                    test.length as i64,
                    test.outcome,
                    test.format,
                    test.valid_fraction,
                    test.first_break.map(|offset| offset as i64),
                    test.decoded,
                    test.note,
                    test.sha256,
                    test.tested_at_millis as i64,
                ])?;
                if test.outcome == crate::filetest::OUTCOME_PASSED {
                    good.execute(params![
                        test.id,
                        VERDICT_LIVE,
                        VERDICT_TESTED_GOOD,
                        TESTED_GOOD_CONFIDENCE,
                        test.note
                    ])?;
                } else if test.outcome == crate::filetest::OUTCOME_DAMAGED {
                    bad.execute(params![
                        test.id,
                        VERDICT_TESTED_BAD,
                        TESTED_BAD_CONFIDENCE,
                        test.note
                    ])?;
                }
            }
        }
        transaction.commit()?;
        Ok(())
    }

    /// Each tested file's content hash, shortened to a number that is quick to compare.
    pub fn file_hashes(&self) -> Result<std::collections::HashMap<i64, u128>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, SUBSTR(sha256, 1, 32) FROM file_tests WHERE sha256 IS NOT NULL")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut found = std::collections::HashMap::new();
        for row in rows {
            let (id, text) = row?;
            if let Ok(hash) = u128::from_str_radix(&text, 16) {
                found.insert(id, hash);
            }
        }
        Ok(found)
    }

    /// Other found files holding exactly the same bytes as this one.
    pub fn file_copies(&self, id: i64, limit: u64) -> Result<Vec<Row>> {
        let mut statement = self.connection.prepare(
            "SELECT id FROM file_tests WHERE id != ?1 AND sha256 = (SELECT sha256 FROM file_tests WHERE id = ?1) LIMIT ?2",
        )?;
        let ids = statement
            .query_map(params![id, limit as i64], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::new();
        for other in ids {
            if let Some(row) = self.entry(other)? {
                rows.push(row);
            }
        }
        Ok(rows)
    }

    /// Removes stored results, all of them or those with one outcome.
    pub fn forget_file_tests(&self, outcome: Option<&str>) -> Result<u64> {
        let removed = match outcome {
            Some(outcome) => self
                .connection
                .execute("DELETE FROM file_tests WHERE outcome = ?1", [outcome])?,
            None => self.connection.execute("DELETE FROM file_tests", [])?,
        };
        Ok(removed as u64)
    }

    /// Files passed, damaged and with no test yet.
    pub fn file_test_counts(&self) -> Result<(u64, u64, u64)> {
        let mut statement = self
            .connection
            .prepare("SELECT outcome, COUNT(*) FROM file_tests GROUP BY outcome")?;
        let mut counts = (0, 0, 0);
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as u64))
        })?;
        for row in rows {
            let (outcome, count) = row?;
            match outcome.as_str() {
                crate::filetest::OUTCOME_PASSED => counts.0 = count,
                crate::filetest::OUTCOME_DAMAGED => counts.1 = count,
                _ => counts.2 = count,
            }
        }
        Ok(counts)
    }

    pub fn file_test(&self, id: i64) -> Result<Option<crate::filetest::FileTest>> {
        Ok(self.file_tests_where("WHERE id = ?1", [id])?.pop())
    }

    /// Every stored result, for applying when the map is rebuilt.
    pub fn all_file_tests(&self) -> Result<Vec<crate::filetest::FileTest>> {
        self.file_tests_where("", [])
    }

    fn file_tests_where<P: rusqlite::Params>(
        &self,
        filter: &str,
        values: P,
    ) -> Result<Vec<crate::filetest::FileTest>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT id, first_cluster, length, outcome, format, valid_fraction, first_break, decoded, note, sha256, tested_at_millis FROM file_tests {filter}"
        ))?;
        let found = statement
            .query_map(values, |row| {
                Ok(crate::filetest::FileTest {
                    id: row.get(0)?,
                    first_cluster: row.get(1)?,
                    length: row.get::<_, i64>(2)? as u64,
                    outcome: row.get(3)?,
                    format: row.get(4)?,
                    valid_fraction: row.get(5)?,
                    first_break: row.get::<_, Option<i64>>(6)?.map(|offset| offset as u64),
                    decoded: row.get(7)?,
                    note: row.get(8)?,
                    sha256: row.get(9)?,
                    tested_at_millis: row.get::<_, i64>(10)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    /// Every file worth browsing, in the smallest form that can sort and group them.
    pub fn browse_scan(&self) -> Result<Vec<crate::browse::BrowseScanRow>> {
        let mut statement = self.connection.prepare(
            "SELECT id, COALESCE(o_name, name), COALESCE(o_length, length), COALESCE(confidence, 0), modified \
             FROM sets WHERE COALESCE(o_is_dir, is_dir) = 0 AND COALESCE(o_length, length) > 0 \
             AND COALESCE(verdict, '') != 'rename-residue'",
        )?;
        let found = statement
            .query_map([], |row| {
                Ok(crate::browse::BrowseScanRow {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    length: row.get::<_, i64>(2)? as u64,
                    confidence: row.get(3)?,
                    modified: row.get(4)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(found)
    }

    /// Ids of entries whose name contains this text.
    pub fn browse_ids_named(&self, text: &str) -> Result<std::collections::HashSet<i64>> {
        let escaped = text
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let mut statement = self
            .connection
            .prepare("SELECT id FROM sets WHERE COALESCE(o_name, name) LIKE ?1 ESCAPE '\\'")?;
        let found = statement
            .query_map([format!("%{escaped}%")], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    /// The rows for one page of a browse list, by id.
    pub fn browse_rows(
        &self,
        ids: &[i64],
    ) -> Result<std::collections::HashMap<i64, crate::browse::BrowseRow>> {
        if ids.is_empty() {
            return Ok(std::collections::HashMap::new());
        }
        let list = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
        let mut statement = self.connection.prepare(&format!(
            "SELECT id, COALESCE(o_name, name), COALESCE(o_length, length), COALESCE(verdict, ''), \
             COALESCE(confidence, 0), COALESCE(o_first, first_cluster), modified, in_use \
             FROM sets WHERE id IN ({list})"
        ))?;
        let found = statement
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                Ok((
                    id,
                    crate::browse::BrowseRow {
                        id,
                        name: row.get(1)?,
                        length: row.get::<_, i64>(2)? as u64,
                        verdict: row.get(3)?,
                        confidence: row.get(4)?,
                        first_cluster: row.get(5)?,
                        modified: row.get(6)?,
                        in_use: row.get(7)?,
                        copies: 1,
                    },
                ))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(found)
    }

    pub fn files_under(&self, folder: u32) -> Result<Vec<Row>> {
        let sql = format!(
            "WITH RECURSIVE tree(head) AS ( \
                SELECT ?1 UNION SELECT nodes.head FROM nodes JOIN tree ON nodes.parent = tree.head \
             ) \
             SELECT {FILE_COLUMNS}, node FROM sets WHERE node IN tree AND COALESCE(o_is_dir, is_dir) = 0 \
             AND COALESCE(o_length, length) > 0 AND COALESCE(verdict, '') != 'rename-residue'"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let found = statement
            .query_map([folder], |row| {
                Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut paths: std::collections::HashMap<Option<u32>, String> =
            std::collections::HashMap::new();
        let mut rows = Vec::with_capacity(found.len());
        for (mut row, node) in found {
            if let std::collections::hash_map::Entry::Vacant(slot) = paths.entry(node) {
                slot.insert(self.path_of(node)?);
            }
            row.path = paths.get(&node).cloned();
            rows.push(row);
        }
        Ok(rows)
    }

    pub fn read_error_count(&self) -> Result<u64> {
        let count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM read_errors", [], |r| r.get(0))?;
        Ok(count as u64)
    }
}

const SIZE_WEIGHT: f64 = 0.45;
const PROXIMITY_WEIGHT: f64 = 0.25;
const ENTROPY_WEIGHT: f64 = 0.30;
/// A chain that starts with a file signature is probably its own file.
const STARTS_ANOTHER_FILE_PENALTY: f64 = 0.4;
const NOT_FILE_DATA_PENALTY: f64 = 0.05;
/// Distance, in clusters, at which proximity starts to fall away.
const PROXIMITY_SCALE: f64 = 16.0;

fn score_bridge(
    island: IslandRow,
    tail: u32,
    remainder: u64,
    own_entropy: Option<f64>,
) -> BridgeCandidate {
    let length = island.length as f64;
    let wanted = remainder as f64;
    let size_fit = if island.length == remainder {
        1.0
    } else if island.length < remainder {
        // It may be one fragment of several.
        0.8 * length / wanted
    } else {
        0.5 * wanted / length
    };
    let distance = f64::from(island.head.abs_diff(tail));
    let proximity = 1.0 / (1.0 + (1.0 + distance / PROXIMITY_SCALE).log2());
    let entropy_fit = own_entropy.map_or(0.5, |own| {
        1.0 - (own - f64::from(island.mean_entropy)).abs() / 255.0
    });
    let penalty = if matches!(
        island.head_class,
        CLASS_DIRECTORY | CLASS_ZEROS | CLASS_FILL
    ) {
        NOT_FILE_DATA_PENALTY
    } else if is_signature_class(island.head_class) {
        STARTS_ANOTHER_FILE_PENALTY
    } else {
        1.0
    };
    let score =
        (SIZE_WEIGHT * size_fit + PROXIMITY_WEIGHT * proximity + ENTROPY_WEIGHT * entropy_fit)
            * penalty;
    let reason = format!(
        "{} clusters for a gap of {remainder}. Head is {} clusters from the file's last cluster. Head holds {}.",
        island.length, distance as u64, island.head_class_name
    );
    BridgeCandidate {
        island,
        score,
        size_fit,
        proximity,
        entropy_fit,
        reason,
    }
}
