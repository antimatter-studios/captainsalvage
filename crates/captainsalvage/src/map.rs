//! The SQLite map of everything the scan found: storage and queries.
//!
//! exFAT entries have no parent pointers. A folder's parent is recoverable only
//! because some other directory holds an entry whose first cluster is this
//! folder's first cluster. When that parent is gone, the folder is an orphan.
//! `resolve.rs` holds the logic that infers that structure.

use crate::content::{is_signature_class, CLASS_DIRECTORY, CLASS_FILL, CLASS_NAMES, CLASS_ZEROS};
use crate::disk_rebuild_rename::{self, Prepared};
use crate::filesystem::SalvageEntry as EntrySet;
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
/// Failed its test, but plays: a player skips the damaged stretches.
pub const VERDICT_TESTED_GAPS: &str = "tested-gaps";
/// A file that passed its test is trusted, and one that failed is not, whatever else says.
pub const TESTED_GOOD_CONFIDENCE: f64 = 0.99;
pub const TESTED_BAD_CONFIDENCE: f64 = 0.1;
pub const TESTED_GAPS_CONFIDENCE: f64 = 0.6;
/// Said after the test's own note when a failed file still plays.
pub const PLAYS_WITH_GAPS: &str = "It plays, with a gap where each break is.";
/// Deeper than this, a walk up the folders is taken to be a loop.
const MAX_TREE_DEPTH: usize = 256;

/// A file waiting to be tested.
pub struct FileTestCandidate {
    pub id: i64,
    pub name: String,
    pub first_cluster: u32,
    pub length: u64,
    pub verdict: Option<String>,
    pub reason_code: Option<String>,
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
CREATE TABLE IF NOT EXISTS records (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    record_number INTEGER,
    sequence INTEGER NOT NULL,
    parent_record INTEGER,
    parent_sequence INTEGER,
    storage TEXT NOT NULL,
    PRIMARY KEY (source_cluster, source_offset)
);
CREATE TABLE IF NOT EXISTS driver_records (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    candidate_id INTEGER NOT NULL,
    candidate TEXT NOT NULL,
    walk TEXT NOT NULL,
    PRIMARY KEY (source_cluster, source_offset)
);
CREATE INDEX IF NOT EXISTS driver_records_candidate ON driver_records (candidate_id);
CREATE TABLE IF NOT EXISTS driver_numbers (
    candidate_id INTEGER PRIMARY KEY,
    record_number INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS fat_weak (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    mark TEXT NOT NULL,
    PRIMARY KEY (source_cluster, source_offset)
);
CREATE TABLE IF NOT EXISTS fat_walks (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    guessed INTEGER NOT NULL,
    extents TEXT NOT NULL,
    PRIMARY KEY (source_cluster, source_offset)
);
CREATE TABLE IF NOT EXISTS ntfs_extents (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    vcn INTEGER NOT NULL,
    lcn INTEGER,
    count INTEGER NOT NULL,
    PRIMARY KEY (source_cluster, source_offset, vcn)
);
CREATE INDEX IF NOT EXISTS ntfs_extents_lcn ON ntfs_extents (lcn);
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
CREATE INDEX IF NOT EXISTS file_tests_outcome ON file_tests (outcome);
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
CREATE TABLE IF NOT EXISTS disk_rebuilds (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    created_at_millis INTEGER NOT NULL,
    updated_at_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS disk_rebuild_items (
    id INTEGER PRIMARY KEY,
    disk_rebuild INTEGER NOT NULL,
    kind TEXT NOT NULL,
    head INTEGER,
    name TEXT,
    parent_item INTEGER,
    parent_head INTEGER,
    position INTEGER NOT NULL,
    created_at_millis INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS disk_rebuild_items_disk_rebuild ON disk_rebuild_items (disk_rebuild, parent_item);
CREATE TABLE IF NOT EXISTS disk_rebuild_suggestions (
    id INTEGER PRIMARY KEY,
    disk_rebuild INTEGER NOT NULL,
    head INTEGER NOT NULL,
    place_item INTEGER,
    place_head INTEGER,
    merge INTEGER NOT NULL,
    note TEXT NOT NULL,
    author TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS disk_rebuild_choices (
    disk_rebuild INTEGER NOT NULL,
    path TEXT NOT NULL,
    entry INTEGER NOT NULL,
    PRIMARY KEY (disk_rebuild, path)
);
CREATE TABLE IF NOT EXISTS agent_sessions (
    id TEXT PRIMARY KEY,
    client TEXT,
    started_at_millis INTEGER NOT NULL,
    last_at_millis INTEGER NOT NULL,
    changes INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS disk_rebuild_changes (
    id INTEGER PRIMARY KEY,
    disk_rebuild INTEGER NOT NULL,
    session TEXT NOT NULL,
    tool TEXT NOT NULL,
    summary TEXT NOT NULL,
    diff TEXT NOT NULL,
    size INTEGER NOT NULL,
    at_millis INTEGER NOT NULL,
    undone INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS disk_rebuild_changes_rebuild ON disk_rebuild_changes (disk_rebuild, undone);
CREATE INDEX IF NOT EXISTS disk_rebuild_changes_session ON disk_rebuild_changes (session, undone);
CREATE TABLE IF NOT EXISTS disk_rebuild_agent_marks (
    change INTEGER NOT NULL,
    disk_rebuild INTEGER NOT NULL,
    kind TEXT NOT NULL,
    key TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS disk_rebuild_agent_marks_rebuild ON disk_rebuild_agent_marks (disk_rebuild);
CREATE TABLE IF NOT EXISTS disk_rebuild_snapshots (
    disk_rebuild INTEGER NOT NULL,
    session TEXT NOT NULL,
    existed INTEGER NOT NULL,
    state TEXT NOT NULL,
    journal_before INTEGER NOT NULL,
    taken_at_millis INTEGER NOT NULL,
    PRIMARY KEY (disk_rebuild, session)
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
    ("disk_rebuild_items", "entry", "INTEGER"),
    ("sets", "o_name", "TEXT"),
    ("sets", "o_first", "INTEGER"),
    ("sets", "o_length", "INTEGER"),
    ("sets", "o_no_fat", "INTEGER"),
    ("sets", "o_is_dir", "INTEGER"),
    ("sets", "o_ok", "INTEGER"),
    ("sets", "confidence", "REAL"),
    ("nodes", "link_confidence", "REAL"),
    ("sets", "reason_code", "TEXT"),
    ("sets", "type_group", "TEXT"),
    ("sets", "extension", "TEXT"),
];

/// The map's tables and their columns, read from the schema itself so the description given
/// to agents can never fall behind it.
pub fn schema_description() -> String {
    let mut tables: Vec<(String, Vec<String>)> = Vec::new();
    for statement in SCHEMA.split(';') {
        let Some(rest) = statement.trim().strip_prefix("CREATE TABLE IF NOT EXISTS ") else {
            continue;
        };
        let Some((name, body)) = rest.split_once('(') else {
            continue;
        };
        let columns = body
            .split(',')
            .filter_map(|part| part.split_whitespace().next())
            .filter(|word| !matches!(*word, "UNIQUE" | "PRIMARY" | ")"))
            .map(|word| word.trim_end_matches(')').to_string())
            .filter(|word| !word.is_empty() && !word.chars().next().is_some_and(char::is_numeric))
            .fold(Vec::<String>::new(), |mut unique, word| {
                if !unique.contains(&word) {
                    unique.push(word);
                }
                unique
            });
        tables.push((name.trim().to_string(), columns));
    }
    for (table, column, _) in ADDED_COLUMNS {
        if let Some((_, columns)) = tables.iter_mut().find(|(name, _)| name == table) {
            columns.push((*column).to_string());
        }
    }
    tables
        .iter()
        .map(|(name, columns)| format!("{name}({})", columns.join(", ")))
        .collect::<Vec<_>>()
        .join("; ")
}

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
    /// Count every shared cluster as a conflict. By default a cluster claimed by a
    /// deleted entry and by a file made after it was deleted belongs to the later file.
    pub ignore_reuse_order: bool,
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

/// Where one entry's bytes are read from.
pub enum EntrySource {
    /// Clusters in file order, as a FAT chain, a contiguous run or a rebuild names them.
    Clusters(Vec<u32>),
    /// An NTFS stream, read through the driver so sparse ranges, initialised length and
    /// unreadable streams are what NTFS says they are.
    Ntfs(crate::filesystem::ntfs_salvage::StreamReference),
    /// A candidate of a driver (`filesystem::drivers`), read through the driver: its bytes are what its walk rebuilds, which
    /// decodes what the driver stored compressed.
    Driver(DriverStream),
}

/// What the engine keeps to read a driver's candidate: the storage its walk reached, and the size the record claims.
#[derive(Clone, Debug)]
pub struct DriverStream {
    pub walk: fs_core_layers::layers::ExtentWalk,
    pub length: u64,
    /// For a candidate that is not a stream of bytes the driver walks (a symbolic link, a special file): its path from the root,
    /// by which the driver's file layer reads it.
    pub node: Option<String>,
}

/// Largest contiguous file assumed when looking for owners of a cluster: 16 GiB.
const OWNER_LOOKBACK_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Possible first clusters asked about per query when looking for a cluster's owners.
const OWNER_START_BATCH: usize = 500;

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
    /// Free clusters that nothing explains and that lie in a bad sector of the source: nobody can read
    /// them, so they are not counted as unexplained. Left out of the answer when there are none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub bad_sector_free_clusters: u64,
    /// Clusters that nothing explains on a volume whose allocation is unknown: they are not free space, and they are not
    /// counted in `unexplained_free_clusters`. Left out of the answer when there are none.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub unexplained_unknown_clusters: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
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
    /// For a folder that lost its name: what its own files mostly are, to suggest a label.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contents: Option<FolderContents>,
    pub confidence: f64,
    pub link_confidence: Option<f64>,
    /// NTFS only: the FILE record's own number, and the sequence it had.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record_number: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sequence: Option<u16>,
    /// NTFS only: how the bytes are held (`resident`, `runs`) or why they cannot be read
    /// (`compressed`, `encrypted`, `attribute-list`, `incomplete`, `none`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    /// A weak candidate: a FAT entry the driver found by its content only (`unchecked`), or as the only entry of its cluster
    /// (`lone_entry`), so it may be file bytes that look like an entry.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weak: Option<String>,
    /// A FAT file whose clusters the driver guessed (its chain was cleared when it was deleted, so its first cluster is known and
    /// the rest is where the driver's walk put it).
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub guessed: bool,
    /// The runs of `(first cluster, count)` the driver walked for a FAT file. The engine reads the file from these.
    #[serde(skip)]
    pub walk: Option<Vec<(u32, u32)>>,
}

/// What a lost folder's own files mostly are, by type, so the window can suggest a label such
/// as "Mostly videos". It is a guess from file names, never the folder's real name.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FolderContents {
    /// photos, videos, music, documents, archives or other.
    pub group: String,
    /// The share of the folder's bytes that group holds, 0 to 1.
    pub share: f64,
    /// Files directly in the folder.
    pub files: u64,
}

/// Longer than this after the last dot, a name has no extension worth grouping by.
const MAX_EXTENSION_CHARS: usize = 8;

/// The file tests' results for one file type.
#[derive(Clone, Debug, Default, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeTestTotals {
    /// The extension, lower case, or empty for files without one.
    pub extension: String,
    /// photos, videos, music, documents, archives or other.
    pub group: String,
    pub files: u64,
    pub bytes: u64,
    pub passed: u64,
    pub damaged: u64,
    /// Tested, but no test exists for the type, or the file is too big to test whole.
    pub no_test: u64,
    pub not_tested_yet: u64,
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
    /// Why this map is open read-only: it still has the names from before Disk Rebuild and could
    /// not be renamed. None for a map that can be written.
    read_only: Option<String>,
}

pub(crate) const FILE_COLUMNS: &str = "id, COALESCE(o_name, name), COALESCE(o_length, length), \
    COALESCE(o_length, valid_length), in_use, COALESCE(o_ok, checksum_ok), COALESCE(verdict, ''), \
    COALESCE(reason, ''), COALESCE(o_first, first_cluster), COALESCE(o_no_fat, no_fat_chain), created, modified, \
    source_cluster, source_offset, COALESCE(clusters_needed, 0), COALESCE(clusters_found, 0), \
    COALESCE(clusters_allocated, 0), COALESCE(clusters_conflict, 0), COALESCE(o_is_dir, is_dir), \
    COALESCE(confidence, 0), \
    (SELECT record_number FROM records r WHERE r.source_cluster = sets.source_cluster AND r.source_offset = sets.source_offset), \
    (SELECT sequence FROM records r WHERE r.source_cluster = sets.source_cluster AND r.source_offset = sets.source_offset), \
    (SELECT storage FROM records r WHERE r.source_cluster = sets.source_cluster AND r.source_offset = sets.source_offset), \
    (SELECT mark FROM fat_weak w WHERE w.source_cluster = sets.source_cluster AND w.source_offset = sets.source_offset), \
    (SELECT guessed FROM fat_walks k WHERE k.source_cluster = sets.source_cluster AND k.source_offset = sets.source_offset), \
    (SELECT extents FROM fat_walks k WHERE k.source_cluster = sets.source_cluster AND k.source_offset = sets.source_offset)";

/// Position of `node` when a query appends it to `FILE_COLUMNS`.
pub(crate) const FILE_NODE_INDEX: usize = 26;

pub(crate) fn file_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
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
        record_number: row.get::<_, Option<i64>>(20)?.map(|value| value as u64),
        sequence: row.get::<_, Option<i64>>(21)?.map(|value| value as u16),
        storage: row.get(22)?,
        weak: row.get(23)?,
        guessed: row.get::<_, Option<bool>>(24)?.unwrap_or(false),
        walk: row
            .get::<_, Option<String>>(25)?
            .and_then(|text| serde_json::from_str(&text).ok()),
        ..Row::default()
    })
}

pub(crate) const NODE_COLUMNS: &str =
    "head, COALESCE(name, ''), kind, modified, files, bytes, intact_files, intact_bytes, link_confidence";

pub(crate) fn node_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Row> {
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

/// Writes a chunk's entries, NTFS records, read errors and state inside `transaction`. With
/// `replace`, an entry already stored at the same place is updated in place and keeps its id;
/// without it, the stored one is left as it is.
/// Renames the table of records a map made before it was driver-neutral had (`ntfs_records`), when it has not been renamed.
fn rename_record_table(connection: &Connection) -> Result<()> {
    let has = |name: &str| -> Result<bool> {
        Ok(connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )? > 0)
    };
    if !has("ntfs_records")? || has("records")? {
        return Ok(());
    }
    // Several openers may find the old name at the same moment (the app, a scan, a tool): the one that gets the write lock
    // renames it, and the others, which wait for the lock, look again and find nothing to do.
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let renamed = (|| -> Result<()> {
        if has("ntfs_records")? && !has("records")? {
            connection.execute_batch("ALTER TABLE ntfs_records RENAME TO records")?;
        }
        Ok(())
    })();
    match renamed {
        Ok(()) => connection.execute_batch("COMMIT")?,
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            return Err(error);
        }
    }
    Ok(())
}

fn write_chunk(
    transaction: &rusqlite::Transaction,
    found: &[(u32, EntrySet)],
    errors: &[(u32, String)],
    state: &[(&str, String)],
    replace: bool,
) -> Result<()> {
    let insert_sql = if replace {
        "INSERT INTO sets (source_cluster, source_offset, name, in_use, checksum_ok, attributes, \
         is_dir, first_cluster, length, valid_length, no_fat_chain, created, modified, accessed) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14) \
         ON CONFLICT (source_cluster, source_offset) DO UPDATE SET name = excluded.name, \
         in_use = excluded.in_use, checksum_ok = excluded.checksum_ok, \
         attributes = excluded.attributes, is_dir = excluded.is_dir, \
         first_cluster = excluded.first_cluster, length = excluded.length, \
         valid_length = excluded.valid_length, no_fat_chain = excluded.no_fat_chain, \
         created = excluded.created, modified = excluded.modified, accessed = excluded.accessed"
    } else {
        "INSERT OR IGNORE INTO sets (source_cluster, source_offset, name, in_use, checksum_ok, attributes, \
         is_dir, first_cluster, length, valid_length, no_fat_chain, created, modified, accessed) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
    };
    let mut insert = transaction.prepare_cached(insert_sql)?;
    let mut record_insert = transaction.prepare_cached(
        "INSERT OR IGNORE INTO records (source_cluster, source_offset, record_number, sequence, \
         parent_record, parent_sequence, storage) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
    )?;
    let mut weak_insert = transaction.prepare_cached(
        "INSERT OR REPLACE INTO fat_weak (source_cluster, source_offset, mark) VALUES (?1, ?2, ?3)",
    )?;
    let mut walk_insert = transaction.prepare_cached(
        "INSERT OR REPLACE INTO fat_walks (source_cluster, source_offset, guessed, extents) VALUES (?1, ?2, ?3, ?4)",
    )?;
    let mut extent_insert = transaction.prepare_cached(
        "INSERT OR IGNORE INTO ntfs_extents (source_cluster, source_offset, vcn, lcn, count) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut number_of = transaction
        .prepare_cached("SELECT record_number FROM driver_numbers WHERE candidate_id = ?1")?;
    let mut number_insert = transaction.prepare_cached(
        "INSERT INTO driver_numbers (candidate_id, record_number) VALUES (?1, ?2)",
    )?;
    let mut driver_insert = transaction.prepare_cached(
        "INSERT OR IGNORE INTO driver_records (source_cluster, source_offset, candidate_id, candidate, walk) \
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    // A driver's candidates are numbered as they are stored, from sixteen, as the records of a volume with records are: the
    // map's tree is made of those numbers. The count goes on across chunks and across a resumed scan.
    let mut next_record: i64 = transaction
        .query_row(
            "SELECT value FROM scan_state WHERE key = 'driver_next_record'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.parse().ok())
        .unwrap_or(crate::filesystem::drivers::FIRST_RECORD);
    let first_record = next_record;
    for (cluster, set) in found {
        let ntfs = set.record.as_ref();
        // A further name of the same NTFS file is told apart by its slot.
        let key = crate::filesystem::source_key(set.offset, set.name_slot);
        insert.execute(params![
            cluster,
            key,
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
        // A weak candidate is told apart in a table of its own, so a map of any other filesystem is as it was.
        if let Some(mark) = &set.weak {
            weak_insert.execute(params![cluster, key, mark])?;
        }
        if let Some(walk) = &set.walk {
            walk_insert.execute(params![
                cluster,
                key,
                walk.guessed,
                serde_json::to_string(&walk.extents)?
            ])?;
        }
        let mut record_number =
            ntfs.and_then(|entry| entry.record_number.map(|value| value as i64));
        if let Some(facts) = &set.driver {
            // A candidate keeps the number it was first given, whatever happens to its row: a rescan of the cluster it is in
            // stores it again, and what a Disk Rebuild or an overlay says of the folder with that number must still be true.
            let known: Option<i64> = number_of
                .query_row([facts.record.id as i64], |row| row.get(0))
                .optional()?;
            record_number = Some(match known {
                Some(number) => number,
                None => {
                    let number = next_record;
                    next_record += 1;
                    number_insert.execute(params![facts.record.id as i64, number])?;
                    number
                }
            });
            driver_insert.execute(params![
                cluster,
                key,
                facts.record.id as i64,
                facts.record.candidate,
                serde_json::to_string(&facts.walk)?
            ])?;
        }
        if let Some(entry) = ntfs {
            record_insert.execute(params![
                cluster,
                key,
                record_number,
                i64::from(entry.sequence),
                entry.parent_record.map(|value| value as i64),
                entry.parent_sequence.map(i64::from),
                entry.storage,
            ])?;
        }
        for run in ntfs.map(|entry| entry.runs.as_slice()).unwrap_or_default() {
            extent_insert.execute(params![
                cluster,
                key,
                run.vcn as i64,
                run.lcn.map(|value| value as i64),
                run.count as i64,
            ])?;
        }
    }
    if next_record != first_record {
        transaction.execute(
            "INSERT INTO scan_state (key, value) VALUES ('driver_next_record', ?1) \
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [next_record.to_string()],
        )?;
    }
    let mut error_insert = transaction
        .prepare_cached("INSERT OR REPLACE INTO read_errors (cluster, message) VALUES (?1, ?2)")?;
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
    Ok(())
}

/// How many file tests ended each way. It reads the `outcome` index alone, so SQLite counts the entries in
/// order and neither sorts nor builds a temporary b-tree, however many tests the map holds.
pub const FILE_TEST_COUNTS_SQL: &str = "SELECT outcome, COUNT(*) FROM file_tests GROUP BY outcome";

impl MapDb {
    /// Opens the map, first renaming a database that still has the names from before Disk Rebuild
    /// (see `disk_rebuild_rename`). One that cannot be renamed opens read-only and unchanged;
    /// `read_only_reason` says why.
    pub fn open(path: &Path) -> Result<Self> {
        let database = Self::open_without_renaming(path)?;
        if database.read_only.is_none() {
            // A stored name with a retired word is renamed once (see `stored_names`). If it cannot be,
            // the map is as it was and opens all the same.
            if let Err(error) = disk_rebuild_rename::stored_names::run(path, &database.connection) {
                eprintln!(
                    "{} still has a stored name from before 0.4 that could not be renamed: {error:#}",
                    path.display()
                );
            }
        }
        Ok(database)
    }

    /// As `open`, without the rename of stored names: for the tests that make a map as an older build
    /// left it.
    #[cfg(test)]
    pub(crate) fn open_without_renaming(path: &Path) -> Result<Self> {
        Self::open_prepared(path)
    }

    #[cfg(not(test))]
    fn open_without_renaming(path: &Path) -> Result<Self> {
        Self::open_prepared(path)
    }

    fn open_prepared(path: &Path) -> Result<Self> {
        if let Prepared::ReadOnly(reason) = disk_rebuild_rename::prepare(path)? {
            eprintln!("{reason}");
            return Ok(Self {
                connection: disk_rebuild_rename::open_read_only(path)?,
                read_only: Some(reason),
            });
        }
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(300))?;
        connection.query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))?;
        connection.execute_batch("PRAGMA synchronous = NORMAL;")?;
        // The table of records (the numbers a map's tree is made of) was made for NTFS and called `ntfs_records`. It holds a
        // driver's candidates too, and is named for what it holds. A map made before is renamed, once, with its rows.
        rename_record_table(&connection)?;
        connection.execute_batch(SCHEMA)?;
        let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        if version < disk_rebuild_rename::SCHEMA_VERSION {
            connection.execute_batch(&format!(
                "PRAGMA user_version = {}",
                disk_rebuild_rename::SCHEMA_VERSION
            ))?;
        }
        let database = Self {
            connection,
            read_only: None,
        };
        database.add_missing_columns()?;
        Ok(database)
    }

    /// Why the map was opened read-only, when it was.
    pub fn read_only_reason(&self) -> Option<&str> {
        self.read_only.as_deref()
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
    /// Gives each of a driver's candidates the record it is in, from the parent claims the driver finds. The records are
    /// numbered when they are stored; the folder every claim names the root of takes the volume's root number (`geometry.root_cluster`, which the driver's registry entry gave),
    /// and every other candidate's parent is the number of the folder it names. The folders are held, with their candidates
    /// (a volume has far fewer of them than files), and the rest is read and written a batch at a time. Done again when
    /// candidates were stored since it last ran.
    pub fn link_driver_records(&mut self, volume: &Volume) -> Result<()> {
        use crate::filesystem::drivers::{DriverRecord, ParentRef};
        let Some(driver) = volume.driver() else {
            return Ok(());
        };
        let total: i64 =
            self.connection
                .query_row("SELECT count(*) FROM driver_records", [], |row| row.get(0))?;
        if total == 0 || self.state("driver_linked")?.as_deref() == Some(&total.to_string()) {
            return Ok(());
        }
        // The root folder's number is the volume's own (`geometry.root_cluster`), which the driver's registry entry gave it.
        let root_record = i64::from(volume.geometry.root_cluster);
        struct Stored {
            cluster: i64,
            offset: i64,
            id: u64,
            candidate: String,
        }
        let load = |connection: &Connection,
                    folders: bool,
                    after: Option<(i64, i64)>,
                    limit: i64|
         -> Result<Vec<Stored>> {
            let mut statement = connection.prepare(
                "SELECT d.source_cluster, d.source_offset, d.candidate_id, d.candidate FROM driver_records d \
                 JOIN sets s ON s.source_cluster = d.source_cluster AND s.source_offset = d.source_offset \
                 WHERE s.is_dir = ?1 AND (d.source_cluster, d.source_offset) > (?2, ?3) \
                 ORDER BY d.source_cluster, d.source_offset LIMIT ?4",
            )?;
            let (cluster, offset) = after.unwrap_or((-1, -1));
            let rows = statement.query_map(params![folders, cluster, offset, limit], |row| {
                Ok(Stored {
                    cluster: row.get(0)?,
                    offset: row.get(1)?,
                    id: row.get::<_, i64>(2)? as u64,
                    candidate: row.get(3)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        };
        let folders = load(&self.connection, true, None, i64::MAX)?;
        let folder_records: Vec<DriverRecord> = folders
            .iter()
            .map(|folder| DriverRecord::stored(folder.id, folder.candidate.clone()))
            .collect();
        let mut number_of: std::collections::HashMap<u64, i64> = std::collections::HashMap::new();
        for folder in &folders {
            let number: i64 = self.connection.query_row(
                "SELECT record_number FROM records WHERE source_cluster = ?1 AND source_offset = ?2",
                params![folder.cluster, folder.offset],
                |row| row.get(0),
            )?;
            number_of.insert(folder.id, number);
        }
        let link = |this: &mut Self,
                    batch: &[Stored],
                    number_of: &mut std::collections::HashMap<u64, i64>|
         -> Result<()> {
            let records: Vec<DriverRecord> = batch
                .iter()
                .map(|stored| DriverRecord::stored(stored.id, stored.candidate.clone()))
                .collect();
            let named = driver.claims(&folder_records, &records)?;
            let claims: Vec<Vec<ParentRef>> = named
                .iter()
                .map(|claims| claims.iter().map(|(parent, _)| *parent).collect())
                .collect();
            let transaction = this.connection.transaction()?;
            // The root first, so a folder of the same batch that names it finds the root's number.
            for (stored, claims) in batch.iter().zip(&claims) {
                if claims.first() == Some(&ParentRef::Root) {
                    // The root has the volume's root number, and no parent.
                    transaction.execute(
                        "UPDATE records SET record_number = ?3, parent_record = NULL WHERE source_cluster = ?1 AND source_offset = ?2",
                        params![stored.cluster, stored.offset, root_record],
                    )?;
                    // And the number a rescan will give it again.
                    transaction.execute(
                        "UPDATE driver_numbers SET record_number = ?2 WHERE candidate_id = ?1",
                        params![stored.id as i64, root_record],
                    )?;
                    number_of.insert(stored.id, root_record);
                }
            }
            // A candidate the driver gave no name takes the one its first claim gives it: that is where a format whose names
            // are in its directory entries keeps them.
            for (stored, claims) in batch.iter().zip(&named) {
                if let Some((_, Some(name))) = claims.first() {
                    transaction.execute(
                        "UPDATE sets SET name = ?3 WHERE source_cluster = ?1 AND source_offset = ?2 AND name = ''",
                        params![stored.cluster, stored.offset, String::from_utf8_lossy(name)],
                    )?;
                }
            }
            for (stored, claims) in batch.iter().zip(&claims) {
                let parent = match claims.first() {
                    Some(ParentRef::Root) => continue,
                    Some(ParentRef::Record(id)) => number_of.get(id).copied(),
                    None => None,
                };
                transaction.execute(
                    "UPDATE records SET parent_record = ?3 WHERE source_cluster = ?1 AND source_offset = ?2",
                    params![stored.cluster, stored.offset, parent],
                )?;
            }
            transaction.commit()?;
            Ok(())
        };
        // The folders first, so the root is known before any file is placed under it.
        link(self, &folders, &mut number_of)?;
        let mut after = None;
        loop {
            let batch = load(&self.connection, false, after, 5000)?;
            let Some(last) = batch.last() else { break };
            after = Some((last.cluster, last.offset));
            link(self, &batch, &mut number_of)?;
        }
        self.set_state("driver_linked", &total.to_string())?;
        Ok(())
    }

    pub fn commit_chunk(
        &mut self,
        found: &[(u32, EntrySet)],
        errors: &[(u32, String)],
        state: &[(&str, String)],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        write_chunk(&transaction, found, errors, state, false)?;
        transaction.commit()?;
        Ok(())
    }

    /// Replaces what the scan stored for clusters `first..end` with what reading them again
    /// found, atomically: the entries that are no longer there are removed, the ones still there
    /// keep their ids (what a person decided about them stays attached), and new ones are added.
    /// Read errors in the range are replaced too, and the totals that depend on them recounted.
    pub fn replace_chunk(
        &mut self,
        first: u32,
        end: u32,
        found: &[(u32, EntrySet)],
        errors: &[(u32, String)],
    ) -> Result<()> {
        let transaction = self.connection.transaction()?;
        let kept: std::collections::HashSet<(u32, u32)> = found
            .iter()
            .map(|(cluster, set)| {
                // The key is the stored one: a further name or stream of the same record has its own slot, so its row keeps its id too.
                (
                    *cluster,
                    crate::filesystem::source_key(set.offset, set.name_slot) as u32,
                )
            })
            .collect();
        let existing: Vec<(u32, u32)> = {
            let mut statement = transaction.prepare(
                "SELECT source_cluster, source_offset FROM sets
                 WHERE source_cluster >= ?1 AND source_cluster < ?2",
            )?;
            let rows = statement.query_map(params![first, end], |row| {
                Ok((row.get::<_, u32>(0)?, row.get::<_, u32>(1)?))
            })?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        for (cluster, offset) in existing.iter().filter(|entry| !kept.contains(entry)) {
            transaction.execute(
                "DELETE FROM sets WHERE source_cluster = ?1 AND source_offset = ?2",
                params![cluster, offset],
            )?;
        }
        transaction.execute(
            "DELETE FROM ntfs_extents WHERE source_cluster >= ?1 AND source_cluster < ?2",
            params![first, end],
        )?;
        transaction.execute(
            "DELETE FROM records WHERE source_cluster >= ?1 AND source_cluster < ?2",
            params![first, end],
        )?;
        transaction.execute(
            "DELETE FROM fat_weak WHERE source_cluster >= ?1 AND source_cluster < ?2",
            params![first, end],
        )?;
        transaction.execute(
            "DELETE FROM fat_walks WHERE source_cluster >= ?1 AND source_cluster < ?2",
            params![first, end],
        )?;
        transaction.execute(
            "DELETE FROM driver_records WHERE source_cluster >= ?1 AND source_cluster < ?2",
            params![first, end],
        )?;
        // The records stored again have no parent until the folders are linked again (`link_driver_records`, first in a resolve).
        transaction.execute("DELETE FROM scan_state WHERE key = 'driver_linked'", [])?;
        transaction.execute(
            "DELETE FROM read_errors WHERE cluster >= ?1 AND cluster < ?2",
            params![first, end],
        )?;
        write_chunk(&transaction, found, errors, &[], true)?;
        transaction.execute(
            "INSERT INTO scan_state (key, value) VALUES ('sets_found', (SELECT count(*) FROM sets))
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        transaction.execute(
            "INSERT INTO scan_state (key, value) VALUES ('read_errors', (SELECT count(*) FROM read_errors))
             ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            [],
        )?;
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
            bad_sector_free_clusters: 0,
            unexplained_unknown_clusters: 0,
        })
    }

    pub fn summary(&self) -> Result<Option<MapSummary>> {
        Ok(self
            .state("summary")?
            .and_then(|text| serde_json::from_str(&text).ok()))
    }

    /// Top-level folders: the volume root, then orphans with the most data first.
    /// Found files of each kind, as (group, files, bytes), for files with one of these verdicts.
    pub fn kind_totals(&self, verdicts: &[&str]) -> Result<Vec<(&'static str, u64, u64)>> {
        let list = verdicts
            .iter()
            .map(|verdict| format!("'{}'", verdict.replace('\'', "")))
            .collect::<Vec<_>>()
            .join(",");
        let mut statement = self.connection.prepare(&format!(
            "SELECT COALESCE(o_name, name), COALESCE(o_length, length) FROM sets \
             WHERE is_dir = 0 AND verdict IN ({list})"
        ))?;
        let mut totals: Vec<(&'static str, u64, u64)> = Vec::new();
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (name, length) = row?;
            let group = crate::browse::group_id_of(&name);
            match totals.iter_mut().find(|total| total.0 == group) {
                Some(total) => {
                    total.1 += 1;
                    total.2 += length.max(0) as u64;
                }
                None => totals.push((group, 1, length.max(0) as u64)),
            }
        }
        totals.sort_by_key(|total| std::cmp::Reverse(total.2));
        Ok(totals)
    }

    /// Found folders whose parent is gone: the pieces left to fit together.
    pub fn lost_folder_count(&self) -> Result<u64> {
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM nodes WHERE parent IS NULL AND kind != 'root'",
            [],
            |row| row.get(0),
        )?;
        Ok(count as u64)
    }

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
                row.contents = self.lost_contents(row, head)?;
            }
        }
        Ok(Page {
            rows,
            total: total as u64,
        })
    }

    /// What a nameless folder's own files mostly are. Nothing for a named folder or the root.
    fn lost_contents(&self, row: &Row, head: u32) -> Result<Option<FolderContents>> {
        if !row.name.is_empty() || row.node_kind.as_deref() == Some("root") {
            return Ok(None);
        }
        self.folder_contents(head)
    }

    /// The type that holds most of a folder's own bytes, by file name.
    pub fn folder_contents(&self, node: u32) -> Result<Option<FolderContents>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT COALESCE(o_name, name), COALESCE(o_length, length) FROM sets \
             WHERE node = ?1 AND is_dir = 0 AND verdict != 'rename-residue'",
        )?;
        let mut totals: Vec<(&'static str, u64)> = Vec::new();
        let mut files = 0u64;
        let rows = statement.query_map([node], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (name, length) = row?;
            files += 1;
            // An empty file still counts, so a folder of empty files is not left unnamed.
            let weight = (length.max(0) as u64).max(1);
            let group = crate::browse::group_id_of(&name);
            match totals.iter_mut().find(|total| total.0 == group) {
                Some(total) => total.1 += weight,
                None => totals.push((group, weight)),
            }
        }
        let all: u64 = totals.iter().map(|total| total.1).sum();
        Ok(totals
            .into_iter()
            .max_by_key(|total| total.1)
            .map(|(group, bytes)| FolderContents {
                group: group.to_string(),
                share: bytes as f64 / all.max(1) as f64,
                files,
            }))
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
            let mut folders = statement
                .query_map([node], node_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            drop(statement);
            for folder in &mut folders {
                if let Some(head) = folder.head {
                    folder.contents = self.lost_contents(folder, head)?;
                }
            }
            rows.extend(folders);
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

    /// The folders from a top-level folder down to `head`, top first, so the sidebar can open
    /// down to a selection. A loop in the parents stops the walk.
    pub fn folder_heads(&self, head: u32) -> Result<Vec<u32>> {
        let mut heads = Vec::new();
        let mut current = Some(head);
        while let Some(found) = current {
            if heads.contains(&found) || heads.len() > MAX_TREE_DEPTH {
                break;
            }
            heads.push(found);
            current = self
                .connection
                .query_row("SELECT parent FROM nodes WHERE head = ?1", [found], |row| {
                    row.get::<_, Option<u32>>(0)
                })
                .optional()?
                .flatten();
        }
        heads.reverse();
        Ok(heads)
    }

    /// The folder in the tree that holds an entry, if the map placed it in one.
    pub fn folder_of_entry(&self, id: i64) -> Result<Option<u32>> {
        Ok(self
            .connection
            .query_row("SELECT node FROM sets WHERE id = ?1", [id], |row| {
                row.get::<_, Option<u32>>(0)
            })
            .optional()?
            .flatten())
    }

    /// Readable path of a node. Orphan roots are shown as `[orphan @cluster]`.
    pub fn path_of(&self, node: Option<u32>) -> Result<String> {
        let mut parts = Vec::new();
        let mut current = node;
        let mut guard = 0;
        while let Some(head) = current {
            guard += 1;
            if guard > MAX_TREE_DEPTH {
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

    /// Entries whose claimed clusters include `cluster`. A chain can be claimed from any of its
    /// clusters, not only its head: a deleted entry whose clusters were reused can start in
    /// the middle of another file's chain. So every cluster on the way back to the head is a
    /// possible first cluster.
    pub fn owners(&self, volume: &Volume, cluster: u32) -> Result<Vec<Row>> {
        if !volume.claims().has_links() {
            return self.ntfs_owners(volume, cluster);
        }
        let lookback = (OWNER_LOOKBACK_BYTES / volume.geometry.cluster_size()) as u32;
        let mut starts = vec![cluster];
        let mut seen: HashSet<u32> = HashSet::from([cluster]);
        let mut current = cluster;
        while let Some(previous) = volume.predecessor(current) {
            if !seen.insert(previous) {
                break;
            }
            starts.push(previous);
            current = previous;
        }
        let read = |row: &rusqlite::Row<'_>| {
            Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?))
        };
        let contiguous = format!(
            "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_ok, checksum_ok) = 1 AND length > 0 AND \
             no_fat_chain = 1 AND first_cluster BETWEEN ?1 AND ?2"
        );
        let mut found = self
            .connection
            .prepare_cached(&contiguous)?
            .query_map(params![cluster.saturating_sub(lookback), cluster], read)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        // The starts are cluster numbers this code produced, so listing them inline is safe.
        for batch in starts.chunks(OWNER_START_BATCH) {
            let list = batch
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let chained = format!(
                "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_ok, checksum_ok) = 1 AND length > 0 AND \
                 no_fat_chain = 0 AND (first_cluster IN ({list}) OR o_first IN ({list}))"
            );
            let mut statement = self.connection.prepare(&chained)?;
            found.extend(
                statement
                    .query_map([], read)?
                    .collect::<rusqlite::Result<Vec<_>>>()?,
            );
        }
        let cluster_size = volume.geometry.cluster_size();
        let mut rows = Vec::new();
        for (mut row, node) in found {
            let contains = if let Some(walk) = &row.walk {
                volume
                    .clusters_from_walk(walk, row.length)
                    .clusters
                    .contains(&cluster)
            } else if row.no_fat_chain {
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
        rows.sort_by_key(|row| std::cmp::Reverse(row.first_cluster));
        rows.truncate(2000);
        Ok(rows)
    }

    /// NTFS entries whose run lists include `cluster`. The runs are looked up by their first
    /// cluster, so a run that began far before `cluster` is found only within the lookback.
    fn ntfs_owners(&self, volume: &Volume, cluster: u32) -> Result<Vec<Row>> {
        let lookback = OWNER_LOOKBACK_BYTES / volume.geometry.cluster_size();
        let sql = format!(
            "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_ok, checksum_ok) = 1 AND length > 0 AND \
             (source_cluster, source_offset) IN (SELECT source_cluster, source_offset FROM ntfs_extents \
             WHERE lcn BETWEEN ?1 AND ?2 AND lcn + count > ?2)"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let found = statement
            .query_map(
                params![i64::from(cluster).saturating_sub(lookback as i64), cluster],
                |row| Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?)),
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut rows = Vec::new();
        for (mut row, node) in found {
            row.path = Some(self.path_of(node)?);
            rows.push(row);
        }
        rows.sort_by_key(|row| std::cmp::Reverse(row.first_cluster));
        rows.truncate(2000);
        Ok(rows)
    }

    /// The runs an NTFS entry was found with, in virtual-cluster order.
    pub fn entry_runs(
        &self,
        source_cluster: u32,
        source_offset: u32,
    ) -> Result<Vec<crate::filesystem::NtfsRun>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT vcn, lcn, count FROM ntfs_extents WHERE source_cluster = ?1 AND source_offset = ?2 ORDER BY vcn",
        )?;
        let runs = statement
            .query_map(params![source_cluster, source_offset], |row| {
                Ok(crate::filesystem::NtfsRun {
                    vcn: row.get::<_, i64>(0)? as u64,
                    lcn: row.get::<_, Option<i64>>(1)?.map(|value| value as u64),
                    count: row.get::<_, i64>(2)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(runs)
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
        let needed = entry.length.div_ceil(volume.geometry.cluster_size());
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
                    entropy.get((cluster - fs_exfat::primitives::FIRST_CLUSTER) as usize)
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
        if entry.storage.is_some() {
            // NTFS: the clusters its run list names. Holes claim none.
            let runs = self.entry_runs(
                entry.source_cluster.unwrap_or(0),
                entry.source_offset.unwrap_or(0),
            )?;
            return Ok(volume.clusters_from_runs(&runs, entry.length).clusters);
        }
        // A FAT file the driver walked is read from its walk, which is the clusters it claims: as the chain, and for a deleted file
        // (whose chain the FAT cleared) as the one run the driver guessed.
        let mut stream = match &entry.walk {
            Some(walk) => volume.clusters_from_walk(walk, entry.length),
            None => volume.clusters_for(entry.first_cluster, entry.length, entry.no_fat_chain),
        };
        if self.params()?.chain_fallback_contiguous && !stream.clusters.is_empty() {
            let mut next = stream.clusters[stream.clusters.len() - 1] + 1;
            while (stream.clusters.len() as u64) < stream.needed && volume.valid_cluster(next) {
                stream.clusters.push(next);
                next += 1;
            }
        }
        Ok(stream.clusters)
    }

    /// Where an entry's bytes come from: an overlay's clusters when a rebuild replaced them,
    /// the driver for NTFS, and the cluster list for the other filesystems.
    pub fn entry_source(&self, volume: &Volume, entry: &Row) -> Result<EntrySource> {
        let overlaid = match entry.id {
            Some(id) => self.overlay_clusters(id)?.is_some(),
            None => false,
        };
        if !overlaid {
            if let (Some(storage), Some(cluster), Some(offset)) = (
                entry.storage.as_ref(),
                entry.source_cluster,
                entry.source_offset,
            ) {
                if storage.starts_with(crate::filesystem::STORAGE_DRIVER) {
                    let walk: Option<String> = self
                        .connection
                        .query_row(
                            "SELECT walk FROM driver_records WHERE source_cluster = ?1 AND source_offset = ?2",
                            params![cluster, offset],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if let Some(walk) = walk {
                        let node = (storage == crate::filesystem::STORAGE_DRIVER_NODE).then(|| {
                            format!(
                                "{}/{}",
                                entry.path.as_deref().unwrap_or_default(),
                                entry.name
                            )
                        });
                        return Ok(EntrySource::Driver(DriverStream {
                            walk: serde_json::from_str(&walk)?,
                            length: entry.length,
                            node,
                        }));
                    }
                }
                return Ok(EntrySource::Ntfs(
                    crate::filesystem::ntfs_salvage::StreamReference {
                        record_offset: volume.cluster_offset(cluster)
                            + u64::from(crate::filesystem::record_offset(offset)),
                        record_number: entry.record_number,
                        sequence: entry.sequence.unwrap_or(0),
                        attribute_index: entry.source_offset.and_then(|source_offset| {
                            let slot = source_offset >> crate::filesystem::NAME_SLOT_SHIFT;
                            (crate::filesystem::ntfs_salvage::STREAM_SLOT
                                ..crate::filesystem::ntfs_salvage::EXTENSION_STREAM_SLOT)
                                .contains(&slot)
                                .then(|| {
                                    (slot - crate::filesystem::ntfs_salvage::STREAM_SLOT) as usize
                                })
                        }),
                        extension_stream: entry.source_offset.and_then(|source_offset| {
                            let slot = source_offset >> crate::filesystem::NAME_SLOT_SHIFT;
                            (slot >= crate::filesystem::ntfs_salvage::EXTENSION_STREAM_SLOT).then(
                                || {
                                    (slot - crate::filesystem::ntfs_salvage::EXTENSION_STREAM_SLOT)
                                        as usize
                                },
                            )
                        }),
                        storage: storage.clone(),
                        length: entry.length,
                        valid_length: entry.valid_length.min(entry.length),
                    },
                ));
            }
        }
        Ok(EntrySource::Clusters(self.entry_clusters(volume, entry)?))
    }

    /// Every file in a folder and the folders beneath it, with its path.
    /// Files with all their clusters, not yet tested, in disk order so reading stays fast.
    pub fn file_test_candidates(&self, max_bytes: u64) -> Result<Vec<FileTestCandidate>> {
        let mut statement = self.connection.prepare(
            "SELECT s.id, COALESCE(s.o_name, s.name), COALESCE(s.o_first, s.first_cluster), COALESCE(s.o_length, s.length), \
             s.verdict, s.reason_code FROM sets s LEFT JOIN file_tests t ON t.id = s.id \
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
                    verdict: row.get(4)?,
                    reason_code: row.get(5)?,
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
                } else if crate::filetest::plays_with_gaps(
                    &test.outcome,
                    &test.format,
                    test.valid_fraction,
                ) {
                    bad.execute(params![
                        test.id,
                        VERDICT_TESTED_GAPS,
                        TESTED_GAPS_CONFIDENCE,
                        format!("{} {PLAYS_WITH_GAPS}", test.note)
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

    /// Groups by day everything in the folders macOS keeps at the top of a volume. Copies of
    /// those folders deeper down came from other disks, so they say nothing about this one.
    pub fn mount_dates(&self, root: u32) -> Result<Vec<crate::mount::MountDate>> {
        let mut statement = self.connection.prepare(
            "WITH RECURSIVE under(head) AS ( \
                 SELECT head FROM nodes WHERE parent = ?1 AND name IN ('.Spotlight-V100', '.fseventsd') \
                 UNION SELECT nodes.head FROM nodes JOIN under ON nodes.parent = under.head \
             ) \
             SELECT substr(modified, 1, 10), COUNT(*), SUM(length), SUM(COALESCE(clusters_needed, 0)) \
             FROM sets \
             WHERE modified IS NOT NULL AND ( \
                 node IN (SELECT head FROM under) \
                 OR (is_dir = 1 AND first_cluster IN (SELECT head FROM under)) \
             ) \
             GROUP BY 1 ORDER BY 1",
        )?;
        let dates = statement
            .query_map([root], |row| {
                Ok(crate::mount::MountDate {
                    date: row.get(0)?,
                    entries: row.get::<_, i64>(1)? as u64,
                    bytes: row.get::<_, i64>(2)? as u64,
                    clusters: row.get::<_, i64>(3)? as u64,
                })
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(dates)
    }

    /// The entries in the volume's `.Spotlight-V100` and `.fseventsd` folders, with the date each
    /// was last changed: what macOS wrote while the volume was mounted.
    pub fn macos_system_entries(&self, root: u32) -> Result<Vec<(String, Row)>> {
        let sql = format!(
            "WITH RECURSIVE under(head) AS ( \
                 SELECT head FROM nodes WHERE parent = ?1 AND name IN ('.Spotlight-V100', '.fseventsd') \
                 UNION SELECT nodes.head FROM nodes JOIN under ON nodes.parent = under.head \
             ) \
             SELECT {FILE_COLUMNS}, substr(modified, 1, 10) FROM sets \
             WHERE modified IS NOT NULL AND ( \
                 node IN (SELECT head FROM under) \
                 OR (is_dir = 1 AND first_cluster IN (SELECT head FROM under)) \
             )"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map([root], |row| {
                // The date comes after the file's own columns.
                let date: String = row.get(FILE_NODE_INDEX)?;
                Ok((date, file_row(row)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        Ok(rows)
    }

    /// Removes stored results, all of them or those with one outcome.
    /// Forgets test results with this outcome and of these formats; an empty list is every format.
    pub fn forget_file_tests(&self, outcome: Option<&str>, formats: &[String]) -> Result<u64> {
        let mut conditions = Vec::new();
        let mut values: Vec<&str> = Vec::new();
        if let Some(outcome) = outcome {
            conditions.push("outcome = ?".to_string());
            values.push(outcome);
        }
        if !formats.is_empty() {
            conditions.push(format!(
                "format IN ({})",
                vec!["?"; formats.len()].join(", ")
            ));
            values.extend(formats.iter().map(String::as_str));
        }
        let filter = if conditions.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", conditions.join(" AND "))
        };
        let removed = self.connection.execute(
            &format!("DELETE FROM file_tests{filter}"),
            rusqlite::params_from_iter(values),
        )?;
        Ok(removed as u64)
    }

    /// Files passed, damaged and with no test yet.
    pub fn file_test_counts(&self) -> Result<(u64, u64, u64)> {
        let mut statement = self.connection.prepare(FILE_TEST_COUNTS_SQL)?;
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

    /// For each file type, by extension, how many found files passed their test, came out
    /// damaged, or have not been tested or have no test. Only files, not folders.
    pub fn file_tests_by_type(&self) -> Result<Vec<TypeTestTotals>> {
        let mut statement = self.connection.prepare(
            "SELECT COALESCE(s.o_name, s.name), COALESCE(s.o_length, s.length), f.outcome \
             FROM sets s LEFT JOIN file_tests f ON f.id = s.id \
             WHERE COALESCE(s.o_is_dir, s.is_dir) = 0 AND COALESCE(s.verdict, '') != 'rename-residue'",
        )?;
        let mut totals: Vec<TypeTestTotals> = Vec::new();
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?;
        for row in rows {
            let (name, length, outcome) = row?;
            let extension = name
                .rsplit_once('.')
                .map(|(_, extension)| extension.to_ascii_lowercase())
                .filter(|extension| !extension.is_empty() && extension.len() <= MAX_EXTENSION_CHARS)
                .unwrap_or_default();
            let index = match totals.iter().position(|total| total.extension == extension) {
                Some(index) => index,
                None => {
                    totals.push(TypeTestTotals {
                        group: crate::browse::group_id_of(&name).to_string(),
                        extension: extension.clone(),
                        ..TypeTestTotals::default()
                    });
                    totals.len() - 1
                }
            };
            let total = &mut totals[index];
            total.files += 1;
            total.bytes += length.max(0) as u64;
            match outcome.as_deref() {
                Some(crate::filetest::OUTCOME_PASSED) => total.passed += 1,
                Some(crate::filetest::OUTCOME_DAMAGED) => total.damaged += 1,
                Some(_) => total.no_test += 1,
                None => total.not_tested_yet += 1,
            }
        }
        totals.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.extension.cmp(&b.extension)));
        Ok(totals)
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
        self.files_in_folder(folder, false)
    }

    /// Like `files_under`, and also the live files that have no bytes, which a copy still makes.
    pub fn files_to_copy_under(&self, folder: u32) -> Result<Vec<Row>> {
        self.files_in_folder(folder, true)
    }

    fn files_in_folder(&self, folder: u32, live_empty: bool) -> Result<Vec<Row>> {
        let has_bytes = if live_empty {
            "(COALESCE(o_length, length) > 0 OR (in_use = 1 AND COALESCE(o_ok, checksum_ok) = 1 \
             AND COALESCE(verdict, '') = 'empty'))"
        } else {
            "COALESCE(o_length, length) > 0"
        };
        let sql = format!(
            "WITH RECURSIVE tree(head) AS ( \
                SELECT ?1 UNION SELECT nodes.head FROM nodes JOIN tree ON nodes.parent = tree.head \
             ) \
             SELECT {FILE_COLUMNS}, node FROM sets WHERE node IN tree AND COALESCE(o_is_dir, is_dir) = 0 \
             AND {has_bytes} AND COALESCE(verdict, '') != 'rename-residue'"
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
