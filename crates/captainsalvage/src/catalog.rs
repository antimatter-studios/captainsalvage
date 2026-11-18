//! The catalog: sources, interpretations and scans.
//!
//! A scan is the unit of stored work. It is made of a **source** (a device or image, recognised
//! by its markers, see [`crate::raw_scan::Fingerprint`]), an **interpretation** (an extent of the
//! source and the format read there, or none), the scan's options, the scanner's version and the
//! range it covers. Change any of them and it is a new scan; the old one stays and opens again
//! without scanning. Everything a scan stores belongs to it: its filesystem map, tests, islands,
//! read errors and Disk Rebuilds live in the scan's own folder, and raw findings and failures
//! hang off its raw scan.
//!
//! The records are rows in `raw-scans.sqlite`, beside the saved raw scans and the hypotheses they
//! refer to. An interpretation is a saved hypothesis (`hypotheses.rs`), tied to its source by the
//! `interpretations` table. Nothing in this module reads file contents or writes to a source.
//!
//! Where the files are, in the app's folder:
//!
//! ```text
//! raw-scans.sqlite                    the catalog, saved raw scans and hypotheses
//! sources/<source>/blocks/<grid>/     caches that depend only on the source's bytes
//! sources/<source>/scans/<scan>/      one scan's map and everything derived from its format
//! source_extents                      (parent source, extent) -> child source: a partition opened
//!                                     alone and the same extent of its disk are one source
//! vol-<serial>/                       a workspace from before the catalog: registered as a
//!                                     scan, and used where it is, never moved or changed
//! backups/                            copies made before the catalog first changed the database
//! ```

mod extents;
mod migrate;
mod scans;
pub(crate) mod stale;
#[cfg(test)]
mod tests;

pub use extents::{LinkedExtent, ParentLink};
pub use migrate::{migrate, MigrationReport};
pub use scans::{
    FilesystemOpen, InterpretationRecord, InterpretationRequest, OpenedScan, ScanKind, ScanRecord,
    ScanRequest,
};
pub use stale::{MarkerHistory, StaleRegion, StaleSummary};

use crate::disk::RawDisk;
use crate::raw_scan::{Comparison, Fingerprint, MarkerReader, Verification};
use crate::workspace::{now_millis, Workspace};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The version of what a filesystem scan stores. A scan made by another version is another scan.
pub const FILESYSTEM_SCANNER_VERSION: u32 = 1;

/// The tables of the catalog. They refer to `hypotheses` and `raw_scans`, which are created first
/// by whoever opens the database, and they only add: nothing older is changed.
pub(crate) const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS sources (
    id TEXT PRIMARY KEY, label TEXT NOT NULL DEFAULT '', length INTEGER NOT NULL,
    fingerprint TEXT, hardware TEXT,
    first_seen_millis INTEGER NOT NULL, last_seen_millis INTEGER NOT NULL,
    last_path TEXT NOT NULL DEFAULT ''
);
CREATE INDEX IF NOT EXISTS sources_length ON sources(length);
CREATE TABLE IF NOT EXISTS interpretations (
    hypothesis_id TEXT PRIMARY KEY REFERENCES hypotheses(id),
    source_id TEXT NOT NULL REFERENCES sources(id),
    listed INTEGER NOT NULL DEFAULT 0, verified INTEGER NOT NULL DEFAULT 1,
    created_at_millis INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS interpretations_source ON interpretations(source_id);
CREATE TABLE IF NOT EXISTS scans (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL REFERENCES sources(id),
    interpretation_id TEXT REFERENCES hypotheses(id),
    kind TEXT NOT NULL,
    format TEXT NOT NULL, ext_start INTEGER NOT NULL, ext_length INTEGER NOT NULL,
    boot_offset INTEGER, origin TEXT NOT NULL, confidence REAL NOT NULL,
    options TEXT NOT NULL DEFAULT '{}', scanner_version INTEGER NOT NULL,
    range_start INTEGER NOT NULL, range_end INTEGER NOT NULL,
    raw_scan_id TEXT UNIQUE REFERENCES raw_scans(id),
    data_dir TEXT, volume_key TEXT, legacy_key TEXT UNIQUE,
    block_size INTEGER NOT NULL DEFAULT 0, block_count INTEGER NOT NULL DEFAULT 0,
    label TEXT NOT NULL DEFAULT '',
    stored_bytes INTEGER NOT NULL DEFAULT 0, stored_at_millis INTEGER NOT NULL DEFAULT 0,
    created_at_millis INTEGER NOT NULL, updated_at_millis INTEGER NOT NULL,
    opened_at_millis INTEGER NOT NULL DEFAULT 0
);
CREATE INDEX IF NOT EXISTS scans_source ON scans(source_id, updated_at_millis);
CREATE INDEX IF NOT EXISTS scans_volume_key ON scans(volume_key);
CREATE TABLE IF NOT EXISTS source_blocks (
    source_id TEXT NOT NULL REFERENCES sources(id), grid TEXT NOT NULL, dir TEXT NOT NULL,
    PRIMARY KEY(source_id, grid)
);
CREATE TABLE IF NOT EXISTS source_extents (
    child_source_id TEXT NOT NULL REFERENCES sources(id),
    parent_source_id TEXT NOT NULL REFERENCES sources(id),
    ext_start INTEGER NOT NULL, ext_length INTEGER NOT NULL,
    created_at_millis INTEGER NOT NULL,
    PRIMARY KEY (child_source_id, parent_source_id, ext_start)
);
CREATE INDEX IF NOT EXISTS source_extents_parent ON source_extents(parent_source_id);
CREATE TABLE IF NOT EXISTS catalog_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS scan_stale_regions (
    scan_id TEXT NOT NULL REFERENCES scans(id),
    start INTEGER NOT NULL, end INTEGER NOT NULL,
    marked_at_millis INTEGER NOT NULL,
    markers TEXT NOT NULL DEFAULT '[]',
    done_to INTEGER NOT NULL,
    cleared_at_millis INTEGER,
    PRIMARY KEY (scan_id, start, end)
);
CREATE TABLE IF NOT EXISTS source_marker_history (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    source_id TEXT NOT NULL REFERENCES sources(id),
    fingerprint TEXT NOT NULL, replaced_at_millis INTEGER NOT NULL, reason TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS source_marker_history_source ON source_marker_history(source_id);";

/// How the drive's own hardware may identify it. It identifies the physical drive and not a clone
/// of it, so it can only add confidence to a marker match, never decide one.
///
/// Nothing fills this in yet: the macOS tools the app already calls do not report a serial number
/// for every bus. The column is kept as an extension point, so a later change can store what a
/// bus reports and let a block or a known-volume record weigh it, without a migration.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Hardware {
    pub model: Option<String>,
    pub serial: Option<String>,
}

#[derive(Clone, Debug, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SourceRecord {
    pub id: String,
    /// A name the person gave it. Empty until they do.
    pub label: String,
    pub length: u64,
    /// How many markers identify it. Zero for a source that came from an old workspace and has
    /// not been seen since: it is found by its old key until then.
    pub markers: usize,
    pub markers_wanted: u32,
    pub fingerprint_version: u32,
    pub hardware: Option<Hardware>,
    pub first_seen_millis: u64,
    pub last_seen_millis: u64,
    pub last_path: String,
    pub scans: u32,
}

/// How a stored source came to be the one an open device is.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum MatchedBy {
    /// Its markers match.
    Markers,
    /// The volume's old key (its serial number) names it, and its markers agree at least
    /// probably, or it had no markers yet.
    VolumeKey,
    /// Nothing stored matched, so a new source was made.
    New,
}

/// A stored source set beside the device that is open, with how well its markers match.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SourceMatch {
    #[serde(flatten)]
    pub source: SourceRecord,
    pub verification: Verification,
    pub comparison: Comparison,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Identification {
    /// The markers of the device that is open now.
    pub fingerprint: Fingerprint,
    pub matches: Vec<SourceMatch>,
    /// Matches left out for being below 60 percent. `show_all` lists them.
    pub hidden: usize,
}

/// How much a match must say before stored work is applied.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Accept {
    /// Only an exact match, or the old key of a volume whose markers agree at least probably.
    Exact,
    /// A probable match too, because the person acknowledged the changed markers.
    Probable,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolution {
    pub source: SourceRecord,
    pub matched_by: MatchedBy,
    /// How the stored markers compare with the device. None for a new source.
    pub comparison: Option<Comparison>,
    /// A probable match that was not applied, because it was not acknowledged: the stored source
    /// and the comparison, listed so the person can open it knowingly.
    pub declined: Option<SourceMatch>,
}

pub struct Catalog {
    connection: Connection,
    root: PathBuf,
}

/// Runs `step` again, a little later, while the database says it is busy.
fn retry_while_busy(mut step: impl FnMut() -> rusqlite::Result<()>) -> Result<()> {
    let mut tries = 0;
    loop {
        match step() {
            Ok(()) => return Ok(()),
            Err(rusqlite::Error::SqliteFailure(failure, _))
                if tries < 100
                    && matches!(
                        failure.code,
                        rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
                    ) =>
            {
                tries += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(error) => return Err(error.into()),
        }
    }
}

pub(crate) fn identifier() -> Result<String> {
    let mut bytes = [0u8; 8];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

/// A JSON value with its object keys in order, so equal options always read as the same text.
pub(crate) fn canonical(value: &serde_json::Value) -> String {
    use serde_json::Value;
    fn sorted(value: &Value) -> Value {
        match value {
            Value::Object(map) => {
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                Value::Object(
                    keys.into_iter()
                        .map(|key| (key.clone(), sorted(&map[key])))
                        .collect(),
                )
            }
            Value::Array(items) => Value::Array(items.iter().map(sorted).collect()),
            other => other.clone(),
        }
    }
    sorted(value).to_string()
}

impl Catalog {
    /// The catalog in the app's folder. The first time in a process, anything stored before the
    /// catalog existed is registered (see [`migrate`]).
    pub fn for_user() -> Result<Self> {
        Self::open(&Workspace::root()?)
    }

    /// The catalog in `root`, the app's folder.
    pub fn open(root: &Path) -> Result<Self> {
        let catalog = Self::open_unmigrated(root)?;
        migrate::once_per_process(&catalog)?;
        Ok(catalog)
    }

    pub(crate) fn open_unmigrated(root: &Path) -> Result<Self> {
        std::fs::create_dir_all(root).with_context(|| format!("creating {}", root.display()))?;
        // A copy of the database, before the catalog's tables are first created in it.
        let backup = migrate::backup_before_first_change(root)?;
        let connection = Connection::open(root.join("raw-scans.sqlite"))?;
        connection.busy_timeout(Duration::from_secs(30))?;
        // Two apps starting together can both be creating the database. Changing the journal mode
        // and creating tables do not wait for each other's lock, so they are tried again.
        retry_while_busy(|| {
            connection.execute_batch(
                "PRAGMA foreign_keys = ON; PRAGMA journal_mode = WAL; PRAGMA synchronous = FULL;",
            )?;
            connection.execute_batch(crate::hypotheses::SCHEMA)?;
            connection.execute_batch(crate::raw_scan::SCHEMA)?;
            connection.execute_batch(SCHEMA)
        })?;
        if let Some(backup) = backup {
            connection.execute(
                "INSERT OR REPLACE INTO catalog_meta (key, value) VALUES ('backup', ?1)",
                [backup.to_string_lossy()],
            )?;
        }
        Ok(Self {
            connection,
            root: root.to_path_buf(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn connection(&self) -> &Connection {
        &self.connection
    }

    // ---- sources ----

    pub fn sources(&self) -> Result<Vec<SourceRecord>> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {SOURCE_COLUMNS} FROM sources ORDER BY last_seen_millis DESC, id"
        ))?;
        let rows = statement.query_map([], source_row)?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn source(&self, id: &str) -> Result<SourceRecord> {
        self.connection
            .query_row(
                &format!("SELECT {SOURCE_COLUMNS} FROM sources WHERE id = ?1"),
                [id],
                source_row,
            )
            .optional()?
            .with_context(|| format!("no source {id} is stored"))
    }

    pub fn label_source(&self, id: &str, label: &str) -> Result<SourceRecord> {
        ensure!(label.len() <= 128, "a name can be at most 128 bytes");
        let changed = self.connection.execute(
            "UPDATE sources SET label = ?2 WHERE id = ?1",
            params![id, label.trim()],
        )?;
        ensure!(changed == 1, "no source {id} is stored");
        self.source(id)
    }

    /// Stores what the bus reports about the drive, as one weaker marker.
    pub fn set_hardware(&self, id: &str, hardware: &Hardware) -> Result<()> {
        let changed = self.connection.execute(
            "UPDATE sources SET hardware = ?2 WHERE id = ?1",
            params![id, serde_json::to_string(hardware)?],
        )?;
        ensure!(changed == 1, "no source {id} is stored");
        Ok(())
    }

    fn sources_of_length(&self, length: u64) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT count(*) FROM sources WHERE length = ?1",
            [length as i64],
            |row| row.get(0),
        )?)
    }

    pub(crate) fn fingerprint_of(&self, id: &str) -> Result<Option<Fingerprint>> {
        let text: Option<String> = self.connection.query_row(
            "SELECT fingerprint FROM sources WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        text.map(|text| serde_json::from_str(&text).context("reading a stored fingerprint"))
            .transpose()
    }

    /// The stored sources of this length, each compared marker by marker with the open device.
    /// A source below 60 percent is left out and counted, unless `show_all` lists the weak ones.
    /// A different source is never listed.
    pub fn identify(&self, disk: &RawDisk, show_all: bool) -> Result<Identification> {
        let mut reader = MarkerReader::new(disk);
        let fingerprint = Fingerprint::read_with(&mut reader);
        let (matches, hidden) = self.compare_all(disk, &mut reader, show_all)?;
        Ok(Identification {
            fingerprint,
            matches,
            hidden,
        })
    }

    fn compare_all(
        &self,
        disk: &RawDisk,
        reader: &mut MarkerReader,
        show_all: bool,
    ) -> Result<(Vec<SourceMatch>, usize)> {
        let mut statement = self.connection.prepare(&format!(
            "SELECT {SOURCE_COLUMNS}, fingerprint FROM sources
             WHERE length = ?1 AND fingerprint IS NOT NULL ORDER BY last_seen_millis DESC"
        ))?;
        let mut rows = statement.query(params![disk.length as i64])?;
        let mut matches = Vec::new();
        let mut hidden = 0;
        while let Some(row) = rows.next()? {
            let source = source_row(row)?;
            let saved: Fingerprint = serde_json::from_str(&row.get::<_, String>(SOURCE_FIELDS)?)?;
            let comparison = saved.compare(reader);
            match comparison.state {
                Verification::Different | Verification::Incompatible => continue,
                Verification::Weak if !show_all => {
                    hidden += 1;
                    continue;
                }
                _ => {}
            }
            matches.push(SourceMatch {
                source,
                verification: comparison.state,
                comparison: comparison.clone(),
            });
        }
        matches.sort_by_key(|found| {
            (
                std::cmp::Reverse(found.comparison.matched),
                std::cmp::Reverse(found.source.last_seen_millis),
            )
        });
        Ok((matches, hidden))
    }

    /// The stored source this device is, or a new one.
    ///
    /// An exact match is used. A probable one is used only when `accept` says the person
    /// acknowledged the changed markers; otherwise it is not applied and comes back as
    /// `declined`, and a new source is made, so nothing stored is applied silently. The old key
    /// of a volume (its serial number) is the exception that keeps work found after an upgrade:
    /// a stored source with that key is used when its markers match at least probably, or when it
    /// has none yet, in which case it gets this device's markers now. A key whose markers say
    /// weak or different is a different drive with the same serial number, and is not used.
    pub fn resolve_source(
        &self,
        disk: &RawDisk,
        accept: Accept,
        volume_key: Option<&str>,
    ) -> Result<Resolution> {
        self.resolve_attempt(disk, accept, volume_key, 0)
    }

    /// One try at [`Catalog::resolve_source`]. Another app can make the source for this same
    /// device between the comparison and the insert, which would give a drive two sources, so the
    /// insert checks that no source of this length appeared and, if one did, compares again.
    fn resolve_attempt(
        &self,
        disk: &RawDisk,
        accept: Accept,
        volume_key: Option<&str>,
        attempt: u32,
    ) -> Result<Resolution> {
        ensure!(
            disk.length <= i64::MAX as u64,
            "source exceeds the stored length limit"
        );
        let mut reader = MarkerReader::new(disk);
        let current = Fingerprint::read_with(&mut reader);
        ensure!(
            !disk.source.is_gone(),
            "The disk has been disconnected; it was not identified."
        );
        let path = disk.path.to_string_lossy().into_owned();
        let known = self.sources_of_length(disk.length)?;
        let (matches, _) = self.compare_all(disk, &mut reader, false)?;
        let mut declined = None;
        if let Some(best) = matches.iter().find(|found| match found.verification {
            Verification::Exact => true,
            Verification::Probable => accept == Accept::Probable,
            _ => false,
        }) {
            self.see(&best.source.id, &path)?;
            return Ok(Resolution {
                source: self.source(&best.source.id)?,
                matched_by: MatchedBy::Markers,
                comparison: Some(best.comparison.clone()),
                declined: None,
            });
        }
        if let Some(probable) = matches
            .iter()
            .find(|found| found.verification == Verification::Probable)
        {
            declined = Some(probable.clone());
        }
        if let Some(key) = volume_key {
            if let Some(found) = self.source_by_volume_key(key, disk, &current, &mut reader)? {
                self.see(&found.0.id, &path)?;
                return Ok(Resolution {
                    source: self.source(&found.0.id)?,
                    matched_by: MatchedBy::VolumeKey,
                    comparison: found.1,
                    declined: None,
                });
            }
        }
        // A device with no usable markers (all blank, or unreadable) cannot be told from another
        // by them. The same path and length is the only thing left to go by.
        if current.samples.is_empty() {
            let known: Option<String> = self
                .connection
                .query_row(
                    "SELECT id FROM sources WHERE length = ?1 AND last_path = ?2
                     AND (fingerprint IS NULL OR fingerprint LIKE '%\"samples\":[]%')
                     ORDER BY last_seen_millis DESC LIMIT 1",
                    params![disk.length as i64, path],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = known {
                self.see(&id, &path)?;
                return Ok(Resolution {
                    source: self.source(&id)?,
                    matched_by: MatchedBy::Markers,
                    comparison: None,
                    declined,
                });
            }
        }
        let id = identifier()?;
        let now = now_millis() as i64;
        let transaction = rusqlite::Transaction::new_unchecked(
            &self.connection,
            rusqlite::TransactionBehavior::Immediate,
        )?;
        if attempt < 3 && self.sources_of_length(disk.length)? != known {
            drop(transaction);
            return self.resolve_attempt(disk, accept, volume_key, attempt + 1);
        }
        transaction.execute(
            "INSERT INTO sources (id, length, fingerprint, first_seen_millis, last_seen_millis, last_path)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![id, disk.length as i64, serde_json::to_string(&current)?, now, path],
        )?;
        transaction.commit()?;
        Ok(Resolution {
            source: self.source(&id)?,
            matched_by: MatchedBy::New,
            comparison: None,
            declined,
        })
    }

    /// A stored source that holds a scan with this old key and is this device: its markers match
    /// at least probably, or it has none yet and takes this device's now.
    fn source_by_volume_key(
        &self,
        key: &str,
        disk: &RawDisk,
        current: &Fingerprint,
        reader: &mut MarkerReader,
    ) -> Result<Option<(SourceRecord, Option<Comparison>)>> {
        let mut statement = self.connection.prepare(
            "SELECT DISTINCT scans.source_id FROM scans JOIN sources ON sources.id = scans.source_id
             WHERE scans.volume_key = ?1 AND scans.kind = 'filesystem'
             ORDER BY scans.updated_at_millis DESC",
        )?;
        let ids = statement
            .query_map([key], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for id in ids {
            let source = self.source(&id)?;
            match self.fingerprint_of(&id)? {
                None => {
                    // Seen for the first time since the upgrade: it is this device now. Its
                    // length, if it has one, is only what the workspace remembered of the
                    // volume, which need not be the length of the device that holds it.
                    self.connection.execute(
                        "UPDATE sources SET fingerprint = ?2, length = ?3 WHERE id = ?1",
                        params![id, serde_json::to_string(current)?, disk.length as i64],
                    )?;
                    return Ok(Some((source, None)));
                }
                Some(saved) => {
                    let comparison = saved.compare(reader);
                    if matches!(
                        comparison.state,
                        Verification::Exact | Verification::Probable
                    ) {
                        return Ok(Some((source, Some(comparison))));
                    }
                }
            }
        }
        Ok(None)
    }

    pub(crate) fn see(&self, id: &str, path: &str) -> Result<()> {
        self.connection.execute(
            "UPDATE sources SET last_seen_millis = ?2, last_path = ?3 WHERE id = ?1",
            params![id, now_millis() as i64, path],
        )?;
        Ok(())
    }

    /// Makes a source from fingerprints already saved, without the device: for the scans of an
    /// older version, which saved the markers but never had a source.
    pub(crate) fn source_from_saved(
        &self,
        fingerprint: &Fingerprint,
        path: &str,
        when: u64,
    ) -> Result<String> {
        if let Some(hash) = &fingerprint.sampled_sha256 {
            let known: Option<String> = self
                .connection
                .query_row(
                    "SELECT id FROM sources WHERE length = ?1 AND fingerprint LIKE ?2 LIMIT 1",
                    params![
                        fingerprint.length as i64,
                        format!("%\"sampledSha256\":\"{hash}\"%")
                    ],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(id) = known {
                self.connection.execute(
                    "UPDATE sources SET first_seen_millis = min(first_seen_millis, ?2),
                        last_seen_millis = max(last_seen_millis, ?2) WHERE id = ?1",
                    params![id, when as i64],
                )?;
                return Ok(id);
            }
        }
        let id = identifier()?;
        self.connection.execute(
            "INSERT INTO sources (id, length, fingerprint, first_seen_millis, last_seen_millis, last_path)
             VALUES (?1, ?2, ?3, ?4, ?4, ?5)",
            params![
                id,
                fingerprint.length as i64,
                serde_json::to_string(fingerprint)?,
                when as i64,
                path
            ],
        )?;
        Ok(id)
    }

    /// A source from an old workspace that has no markers yet.
    pub(crate) fn source_without_markers(&self, length: u64, when: u64) -> Result<String> {
        let id = identifier()?;
        self.connection.execute(
            "INSERT INTO sources (id, length, fingerprint, first_seen_millis, last_seen_millis, last_path)
             VALUES (?1, ?2, NULL, ?3, ?3, '')",
            params![id, length as i64, when as i64],
        )?;
        Ok(id)
    }
}

/// Where a query that selects the source columns and then the fingerprint finds the latter.
const SOURCE_FIELDS: usize = 11;
const SOURCE_COLUMNS: &str = "id, label, length, \
    coalesce(json_array_length(fingerprint, '$.samples'), 0), \
    coalesce(json_extract(fingerprint, '$.markersWanted'), 0), \
    coalesce(json_extract(fingerprint, '$.version'), 0), \
    hardware, first_seen_millis, last_seen_millis, last_path, \
    (SELECT count(*) FROM scans WHERE scans.source_id = sources.id)";

fn source_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SourceRecord> {
    Ok(SourceRecord {
        id: row.get(0)?,
        label: row.get(1)?,
        length: row.get::<_, i64>(2)? as u64,
        markers: row.get::<_, i64>(3)? as usize,
        markers_wanted: row.get::<_, i64>(4)? as u32,
        fingerprint_version: row.get::<_, i64>(5)? as u32,
        hardware: row
            .get::<_, Option<String>>(6)?
            .and_then(|text| serde_json::from_str(&text).ok()),
        first_seen_millis: row.get::<_, i64>(7)? as u64,
        last_seen_millis: row.get::<_, i64>(8)? as u64,
        last_path: row.get(9)?,
        scans: row.get::<_, i64>(10)? as u32,
    })
}
