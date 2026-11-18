//! Moving what older versions stored into the catalog, without losing or repeating any of it.
//!
//! Before the catalog, work was kept under three keys: a folder per volume named from its serial
//! number (`vol-<serial>`, or `ntfs-<serial>`), holding the map and caches of a filesystem scan;
//! a saved raw scan with a fingerprint of its source; and hypotheses keyed by the sampled hash of
//! their source. This registers each as a source, an interpretation and a scan that reproduces
//! its old key, so everything already scanned is found again and nothing is scanned a second
//! time.
//!
//! It only adds rows, in one transaction. Nothing in a workspace folder is moved, copied or
//! changed: a registered workspace is used where it is, so a window or command-line session of
//! the older version that is still open keeps working on the same files. The database gets a copy
//! first, in `backups/`, before the catalog's tables are first created in it. Running it again
//! changes nothing, because every scan it registers carries the key it came from, which can be
//! registered once. If it stops half way nothing is registered, and the next run does all of it;
//! two apps running it together wait for each other, and the second finds it done.

use super::scans::{adopt_saved_hypotheses, ensure_interpretation, NO_FORMAT};
use super::{identifier, Catalog, InterpretationRequest, FILESYSTEM_SCANNER_VERSION};
use crate::raw_scan::Fingerprint;
use crate::startup::{VolumeRecord, VOLUME_FILE};
use crate::workspace::now_millis;
use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::Serialize;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// What one run registered.
#[derive(Debug, Default, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct MigrationReport {
    /// Where the database was copied before the catalog first changed it, if it was.
    pub backup: Option<PathBuf>,
    /// `vol-<serial>` and `ntfs-<serial>` workspaces registered as scans.
    pub workspaces: usize,
    /// Filesystem interpretations of a raw disk inside a workspace, registered as scans.
    pub regions: usize,
    pub raw_scans: usize,
    /// Hypotheses saved before the catalog that became interpretations of a source.
    pub hypotheses: usize,
    /// Registered by an earlier run, and left as they are.
    pub already: usize,
}

impl MigrationReport {
    pub fn changed(&self) -> bool {
        self.workspaces + self.regions + self.raw_scans + self.hypotheses > 0
    }
}

/// Runs the sweep the first time a catalog is opened in this process, for this folder. A failure
/// is reported and does not stop the catalog being used: the next run of the app tries again.
pub(super) fn once_per_process(catalog: &Catalog) -> Result<()> {
    static DONE: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    let done = DONE.get_or_init(Mutex::default);
    let Ok(mut done) = done.lock() else {
        return Ok(());
    };
    if done.contains(catalog.root()) {
        return Ok(());
    }
    match migrate(catalog) {
        Ok(_) => {
            done.insert(catalog.root().to_path_buf());
        }
        Err(error) => eprintln!("could not register stored scans in the catalog: {error:#}"),
    }
    Ok(())
}

/// Copies the database before the catalog's tables are first created in it. Returns where, or
/// None when there is nothing to copy (a new database, or one the catalog already changed).
pub(super) fn backup_before_first_change(root: &Path) -> Result<Option<PathBuf>> {
    let database = root.join("raw-scans.sqlite");
    if !database.exists() || std::fs::metadata(&database)?.len() == 0 {
        return Ok(None);
    }
    let connection = Connection::open(&database)?;
    connection.busy_timeout(std::time::Duration::from_secs(30))?;
    let changed: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE name = 'catalog_meta'",
        [],
        |row| row.get(0),
    )?;
    if changed > 0 {
        return Ok(None);
    }
    let folder = root.join("backups");
    std::fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    let copy = folder.join(format!(
        "raw-scans-before-catalog-{}-{}.sqlite",
        now_millis(),
        identifier()?
    ));
    // A copy that is consistent whatever the write-ahead log holds.
    connection
        .execute("VACUUM INTO ?1", [copy.to_string_lossy()])
        .with_context(|| format!("copying the database to {}", copy.display()))?;
    Ok(Some(copy))
}

/// Registers everything older versions stored that is not registered yet.
pub fn migrate(catalog: &Catalog) -> Result<MigrationReport> {
    let mut report = MigrationReport::default();
    let transaction =
        Transaction::new_unchecked(catalog.connection(), TransactionBehavior::Immediate)?;
    let registered: HashSet<String> = {
        let mut statement =
            transaction.prepare("SELECT legacy_key FROM scans WHERE legacy_key IS NOT NULL")?;
        let keys = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<HashSet<_>>>()?;
        keys
    };
    let mut region_sources: HashMap<String, String> = HashMap::new();
    for entry in sorted_entries(catalog.root()) {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("vol-") || name.starts_with("ntfs-")) || !entry.path().is_dir() {
            continue;
        }
        if registered.contains(&name) {
            report.already += 1;
        } else {
            register_workspace(catalog, &transaction, &name, &entry.path())?;
            report.workspaces += 1;
        }
        for region in sorted_entries(&entry.path()) {
            let region_name = region.file_name().to_string_lossy().into_owned();
            let Some(disk) = region_name
                .strip_prefix("region-")
                .and_then(|rest| rest.split('-').next())
            else {
                continue;
            };
            let key = format!("{name}/{region_name}");
            if registered.contains(&key) {
                report.already += 1;
            } else if register_region(
                catalog,
                &transaction,
                &key,
                &region.path(),
                disk,
                &mut region_sources,
            )? {
                report.regions += 1;
            }
        }
    }
    report.raw_scans = register_raw_scans(catalog, &transaction, &mut report.already)?;
    let sources: Vec<String> = {
        let mut statement = transaction.prepare("SELECT id FROM sources")?;
        let ids = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        ids
    };
    let count = |transaction: &Transaction| -> Result<i64> {
        Ok(transaction.query_row("SELECT count(*) FROM interpretations", [], |row| row.get(0))?)
    };
    let before = count(&transaction)?;
    for source in &sources {
        adopt_saved_hypotheses(&transaction, source)?;
    }
    report.hypotheses = (count(&transaction)? - before) as usize;
    transaction.execute(
        "INSERT OR IGNORE INTO catalog_meta (key, value) VALUES ('migrated_at_millis', ?1)",
        [now_millis().to_string()],
    )?;
    transaction.commit()?;
    report.backup = catalog
        .connection()
        .query_row(
            "SELECT value FROM catalog_meta WHERE key = 'backup'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .map(PathBuf::from);
    Ok(report)
}

fn sorted_entries(folder: &Path) -> Vec<std::fs::DirEntry> {
    let Ok(entries) = std::fs::read_dir(folder) else {
        return Vec::new();
    };
    let mut entries: Vec<_> = entries.flatten().collect();
    entries.sort_by_key(|entry| entry.file_name());
    entries
}

fn modified_millis(path: &Path) -> Option<u64> {
    std::fs::metadata(path)
        .ok()?
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .map(|elapsed| elapsed.as_millis() as u64)
}

/// What an old workspace is, as far as its folder tells.
struct Legacy<'a> {
    source: &'a str,
    key: &'a str,
    format: &'a str,
    start: u64,
    length: u64,
    boot_offset: Option<u64>,
    label: String,
    updated: u64,
}

/// A volume workspace: one source with no markers yet, read as the format its name suggests until
/// the device is seen, and one scan that uses the folder as it is.
fn register_workspace(
    catalog: &Catalog,
    transaction: &Transaction,
    name: &str,
    folder: &Path,
) -> Result<()> {
    let record: Option<VolumeRecord> = std::fs::read_to_string(folder.join(VOLUME_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok());
    let bytes = record.as_ref().map_or(0, |record| record.bytes);
    let updated = modified_millis(&folder.join("map.sqlite"))
        .or_else(|| record.as_ref().map(|record| record.last_opened_millis))
        .or_else(|| modified_millis(folder))
        .unwrap_or_else(now_millis);
    // The name says NTFS or not; a FAT volume is told from exFAT only by opening it.
    let format = if name.starts_with("ntfs-") {
        "NTFS"
    } else {
        "exFAT"
    };
    let source = catalog.source_without_markers(bytes, updated)?;
    insert_legacy(
        transaction,
        &Legacy {
            source: &source,
            key: name,
            format,
            start: 0,
            length: bytes.max(1),
            boot_offset: None,
            label: record.map(|record| record.name).unwrap_or_default(),
            updated,
        },
    )
}

/// A raw disk read as a filesystem inside a workspace. The folder name holds a hash of the
/// disk's path, which is all that told two disks apart, so regions with the same hash share one
/// source.
fn register_region(
    catalog: &Catalog,
    transaction: &Transaction,
    key: &str,
    folder: &Path,
    disk: &str,
    sources: &mut HashMap<String, String>,
) -> Result<bool> {
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Region {
        start: u64,
        length: u64,
        format: String,
        boot_offset: u64,
        #[serde(default)]
        label: String,
    }
    let Some(region) = std::fs::read_to_string(folder.join("interpretation.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<Region>(&text).ok())
    else {
        return Ok(false);
    };
    let updated = modified_millis(&folder.join("map.sqlite"))
        .or_else(|| modified_millis(folder))
        .unwrap_or_else(now_millis);
    let source = match sources.get(disk) {
        Some(source) => source.clone(),
        None => {
            let known: Option<String> = transaction
                .query_row(
                    "SELECT source_id FROM scans WHERE legacy_key LIKE ?1 LIMIT 1",
                    [format!("%/region-{disk}-%")],
                    |row| row.get(0),
                )
                .optional()?;
            let source = match known {
                Some(source) => source,
                None => catalog.source_without_markers(0, updated)?,
            };
            sources.insert(disk.to_string(), source.clone());
            source
        }
    };
    insert_legacy(
        transaction,
        &Legacy {
            source: &source,
            key,
            format: &region.format,
            start: region.start,
            length: region.length,
            boot_offset: (region.boot_offset != region.start).then_some(region.boot_offset),
            label: region.label,
            updated,
        },
    )?;
    Ok(true)
}

fn insert_legacy(transaction: &Transaction, legacy: &Legacy) -> Result<()> {
    // The interpretation is a guess until the device is seen again, and says so.
    let found = ensure_interpretation(
        transaction,
        legacy.source,
        &InterpretationRequest {
            format: legacy.format.into(),
            start: legacy.start,
            length: legacy.length,
            boot_offset: legacy.boot_offset,
            origin: "analysis".into(),
            confidence: 0.0,
        },
    )?;
    transaction.execute(
        "UPDATE interpretations SET verified = 0 WHERE hypothesis_id = ?1 AND listed = 0",
        [&found.id],
    )?;
    transaction.execute(
        "INSERT INTO scans (id, source_id, interpretation_id, kind, format, ext_start, ext_length,
            boot_offset, origin, confidence, options, scanner_version, range_start, range_end,
            data_dir, volume_key, legacy_key, label, created_at_millis, updated_at_millis)
         VALUES (?1, ?2, ?3, 'filesystem', ?4, ?5, ?6, ?7, 'analysis', 0, '{}', ?8, ?5, ?9,
                 ?10, ?10, ?10, ?11, ?12, ?12)",
        params![
            identifier()?,
            legacy.source,
            found.id,
            legacy.format,
            legacy.start as i64,
            legacy.length as i64,
            legacy.boot_offset.map(|value| value as i64),
            FILESYSTEM_SCANNER_VERSION,
            (legacy.start + legacy.length) as i64,
            legacy.key,
            legacy.label,
            legacy.updated as i64
        ],
    )?;
    Ok(())
}

/// Saved raw scans: the source is their fingerprint, the interpretation the scanned range as raw
/// bytes, and the scan points at the saved scan by its ID, so resuming by that ID still works.
fn register_raw_scans(
    catalog: &Catalog,
    transaction: &Transaction,
    already: &mut usize,
) -> Result<usize> {
    type Row = (String, String, String, u32, i64, i64, i64, i64, bool);
    let rows: Vec<Row> = {
        let mut statement = transaction.prepare(
            "SELECT id, fingerprint, source_path, scanner_version, scan_start, scan_end,
                    created_at_millis, updated_at_millis,
                    id IN (SELECT raw_scan_id FROM scans WHERE raw_scan_id IS NOT NULL)
             FROM raw_scans ORDER BY created_at_millis, id",
        )?;
        let rows = statement
            .query_map([], |row| {
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
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut registered = 0;
    for (id, fingerprint, path, version, start, end, created, updated, has_scan) in rows {
        if has_scan {
            *already += 1;
            continue;
        }
        let fingerprint: Fingerprint =
            serde_json::from_str(&fingerprint).context("reading a saved scan's fingerprint")?;
        let source = catalog.source_from_saved(&fingerprint, &path, created as u64)?;
        let length = (end - start).max(1);
        let interpretation = ensure_interpretation(
            transaction,
            &source,
            &InterpretationRequest {
                format: NO_FORMAT.into(),
                start: start as u64,
                length: length as u64,
                boot_offset: None,
                origin: "manual".into(),
                confidence: 0.0,
            },
        )?;
        transaction.execute(
            "INSERT INTO scans (id, source_id, interpretation_id, kind, format, ext_start, ext_length,
                origin, confidence, options, scanner_version, range_start, range_end, raw_scan_id,
                legacy_key, created_at_millis, updated_at_millis)
             VALUES (?1, ?2, ?3, 'raw', ?4, ?5, ?6, 'manual', 0, '{}', ?7, ?5, ?8, ?9, ?10, ?11, ?12)",
            params![
                identifier()?,
                source,
                interpretation.id,
                NO_FORMAT,
                start,
                length,
                version,
                end,
                id,
                format!("raw:{id}"),
                created,
                updated
            ],
        )?;
        registered += 1;
    }
    Ok(registered)
}
