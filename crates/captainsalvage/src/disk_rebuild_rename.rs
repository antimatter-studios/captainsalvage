//! Moves a map database from the names it had before "Disk Rebuild" to the names it has now.
//!
//! Older builds kept a Disk Rebuild in `canvases`, `canvas_items`, `canvas_suggestions` and
//! `canvas_choices`, with a column `canvas` pointing at the first. Those names are gone from the
//! schema, so a database that still has them is renamed the first time a build opens it. The work
//! is weeks of arrangement and hours of scanning, so this is built to lose none of it:
//!
//! - A copy of the database is made next to it first, named `<map>.before-disk-rebuild-rename`,
//!   by an APFS clone (instant, and no space until the two differ) or else a plain copy. An
//!   existing copy is never overwritten: a later one gets `.2`, `.3` and so on. The copy is safe
//!   to delete after a while.
//! - The renames run in ONE transaction that starts with `BEGIN IMMEDIATE`, so a second app
//!   opening the same file waits, then finds the work done. The need is checked again inside the
//!   transaction, so a second opener changes nothing.
//! - Renaming a table or a column does not touch a row, so what is stored is what was stored.
//! - `PRAGMA quick_check` runs before the commit. Anything but "ok" rolls the whole thing back.
//! - A database that cannot be changed (a read-only file or folder, a lock that never frees, a
//!   failed copy, a damaged file) is left exactly as it is and opened read-only, with temporary
//!   views that give it the new names, and a message saying why.
//! - It is safe to run again, and safe when some names are already new. An older build that opens
//!   a migrated database makes its empty old tables again: those are dropped, or when the person
//!   made something in them, kept under an `unmerged_` name rather than lost.
//!
//! An older build (v0.3.0 and the release candidates before this one) does not find the new names
//! in a migrated database. The copy is how to go back.

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OpenFlags, TransactionBehavior};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The version stored in `PRAGMA user_version` once a database has the names of this schema.
pub const SCHEMA_VERSION: i64 = 1;
/// Added to a map's file name to name the copy made before the rename.
pub const BACKUP_SUFFIX: &str = ".before-disk-rebuild-rename";
/// How long a second opener waits for the first to finish the rename.
const WAIT: Duration = Duration::from_secs(300);

/// Old table name, new table name.
const TABLES: [(&str, &str); 4] = [
    ("canvases", "disk_rebuilds"),
    ("canvas_items", "disk_rebuild_items"),
    ("canvas_suggestions", "disk_rebuild_suggestions"),
    ("canvas_choices", "disk_rebuild_choices"),
];
/// The tables, by their new names, that have a column pointing at a Disk Rebuild.
const POINTING_TABLES: [&str; 3] = [
    "disk_rebuild_items",
    "disk_rebuild_suggestions",
    "disk_rebuild_choices",
];
const OLD_COLUMN: &str = "canvas";
const NEW_COLUMN: &str = "disk_rebuild";
const OLD_INDEX: &str = "canvas_items_canvas";
const NEW_INDEX_SQL: &str = "CREATE INDEX IF NOT EXISTS disk_rebuild_items_disk_rebuild \
     ON disk_rebuild_items (disk_rebuild, parent_item)";
/// What an old table is called when it could not be merged into a new table that holds rows too.
const UNMERGED_PREFIX: &str = "unmerged_";

/// What opening a map database found, once it is ready to be used.
#[derive(Debug, PartialEq, Eq)]
pub enum Prepared {
    /// Already has the new names, or is new.
    Ready,
    /// Was renamed by this call. Where the copy went, if one was made.
    Migrated { backup: Option<PathBuf> },
    /// Could not be changed, and has been left as it was. Open it read-only, with this message.
    ReadOnly(String),
}

/// Makes the database at `path` ready for the current schema, or says why it cannot be.
pub fn prepare(path: &Path) -> Result<Prepared> {
    if std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0) == 0 {
        return Ok(Prepared::Ready);
    }
    let connection = match Connection::open(path) {
        Ok(connection) => connection,
        Err(error) => {
            return Ok(Prepared::ReadOnly(format!(
                "{} cannot be opened for writing: {error}",
                path.display()
            )))
        }
    };
    connection.busy_timeout(WAIT)?;
    match has_old_names(&connection) {
        Ok(false) => return Ok(Prepared::Ready),
        Ok(true) => {}
        Err(error) => {
            return Ok(Prepared::ReadOnly(format!(
                "{} cannot be read: {error:#}",
                path.display()
            )))
        }
    }
    match migrate(connection, path) {
        Ok(prepared) => Ok(prepared),
        Err(error) => Ok(Prepared::ReadOnly(format!(
            "{} still has the names from before Disk Rebuild and could not be renamed, so it is \
             open read-only and unchanged: {error:#}",
            path.display()
        ))),
    }
}

fn migrate(mut connection: Connection, path: &Path) -> Result<Prepared> {
    if connection.is_readonly(rusqlite::MAIN_DB)? {
        return Err(anyhow!("the file is read-only"));
    }
    // Fold the write-ahead log into the file when nobody else is reading, so the copy below is
    // one file. A log that stays is copied with it.
    let _ = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    let transaction = connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .context("another program is using the database")?;
    // The first opener may have finished while this one waited.
    if !has_old_names(&transaction)? {
        transaction.commit()?;
        return Ok(Prepared::Ready);
    }
    let backup = back_up(path).context("the copy made before the change failed")?;
    let result = rename_all(&transaction).and_then(|()| quick_check(&transaction));
    match result {
        Ok(()) => {
            transaction.commit()?;
            Ok(Prepared::Migrated {
                backup: Some(backup),
            })
        }
        Err(error) => {
            // Dropping the transaction rolls it back: nothing has changed.
            drop(transaction);
            Err(error)
        }
    }
}

/// Whether any table, column or index still has an old name.
fn has_old_names(connection: &Connection) -> Result<bool> {
    let names: HashSet<String> = connection
        .prepare("SELECT name FROM sqlite_master WHERE type IN ('table', 'index')")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    if names.contains(OLD_INDEX) || TABLES.iter().any(|(old, _)| names.contains(*old)) {
        return Ok(true);
    }
    for table in POINTING_TABLES {
        if names.contains(table) && columns(connection, table)?.contains(OLD_COLUMN) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn columns(connection: &Connection, table: &str) -> Result<HashSet<String>> {
    Ok(connection
        .prepare(&format!("SELECT name FROM pragma_table_info('{table}')"))?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

fn table_exists(connection: &Connection, table: &str) -> Result<bool> {
    Ok(connection.query_row(
        "SELECT count(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [table],
        |row| row.get::<_, i64>(0),
    )? > 0)
}

fn row_count(connection: &Connection, table: &str) -> Result<i64> {
    Ok(
        connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
            row.get(0)
        })?,
    )
}

/// The renames themselves, inside the caller's transaction.
fn rename_all(connection: &Connection) -> Result<()> {
    for (old, new) in TABLES {
        if !table_exists(connection, old)? {
            continue;
        }
        if table_exists(connection, new)? {
            // Both exist: an older build opened a migrated database and made the old table again.
            if row_count(connection, old)? == 0 {
                connection.execute_batch(&format!("DROP TABLE {old}"))?;
                continue;
            }
            if row_count(connection, new)? == 0 {
                connection.execute_batch(&format!("DROP TABLE {new}"))?;
            } else {
                // Both hold rows and their ids would clash. Keep the old rows, set aside under
                // a name that is not taken, so earlier rows set aside are never replaced.
                let mut aside = format!("{UNMERGED_PREFIX}{old}");
                let mut number = 2;
                while table_exists(connection, &aside)? {
                    aside = format!("{UNMERGED_PREFIX}{old}_{number}");
                    number += 1;
                }
                connection.execute_batch(&format!("ALTER TABLE {old} RENAME TO {aside}"))?;
                continue;
            }
        }
        connection.execute_batch(&format!("ALTER TABLE {old} RENAME TO {new}"))?;
    }
    for table in POINTING_TABLES {
        if !table_exists(connection, table)? {
            continue;
        }
        let present = columns(connection, table)?;
        if present.contains(OLD_COLUMN) && !present.contains(NEW_COLUMN) {
            connection.execute_batch(&format!(
                "ALTER TABLE {table} RENAME COLUMN {OLD_COLUMN} TO {NEW_COLUMN}"
            ))?;
        }
    }
    connection.execute_batch(&format!("DROP INDEX IF EXISTS {OLD_INDEX}"))?;
    if table_exists(connection, "disk_rebuild_items")?
        && columns(connection, "disk_rebuild_items")?.contains(NEW_COLUMN)
    {
        connection.execute_batch(NEW_INDEX_SQL)?;
    }
    connection.execute_batch(&format!("PRAGMA user_version = {SCHEMA_VERSION}"))?;
    Ok(())
}

fn quick_check(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare("PRAGMA quick_check")?;
    let answers: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?;
    if answers.len() == 1 && answers[0] == "ok" {
        Ok(())
    } else {
        Err(anyhow!(
            "the check after the rename failed: {}",
            answers.into_iter().take(3).collect::<Vec<_>>().join("; ")
        ))
    }
}

/// Where the next copy of `path` goes: the first name not taken.
fn backup_path(path: &Path, suffix: &str) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let name = name.unwrap_or_else(|| "map.sqlite".into());
    let mut candidate = path.with_file_name(format!("{name}{suffix}"));
    let mut number = 2;
    while candidate.exists() || wal_of(&candidate).exists() {
        candidate = path.with_file_name(format!("{name}{suffix}.{number}"));
        number += 1;
    }
    candidate
}

fn wal_of(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_os_string();
    name.push("-wal");
    PathBuf::from(name)
}

/// Copies the database, and its write-ahead log if one holds changes, never over an existing
/// file. The caller holds the write lock, so nothing is added to the log meanwhile.
fn back_up(path: &Path) -> Result<PathBuf> {
    back_up_with_suffix(path, BACKUP_SUFFIX)
}

/// As `back_up`, naming the copy with `suffix` instead of the Disk Rebuild rename's.
pub(crate) fn back_up_with_suffix(path: &Path, suffix: &str) -> Result<PathBuf> {
    let target = backup_path(path, suffix);
    let log = wal_of(path);
    let log_size = std::fs::metadata(&log).map(|meta| meta.len()).unwrap_or(0);
    let result = copy_one(path, &target).and_then(|()| {
        if log_size > 0 {
            copy_one(&log, &wal_of(&target))?;
        }
        Ok(())
    });
    if let Err(error) = result {
        let _ = std::fs::remove_file(&target);
        let _ = std::fs::remove_file(wal_of(&target));
        return Err(error);
    }
    Ok(target)
}

/// A clone where the volume allows one, which takes no time and no space, else a copy. Checks the
/// size afterwards.
fn copy_one(from: &Path, to: &Path) -> Result<()> {
    let cloned = cfg!(target_os = "macos")
        && std::process::Command::new("/bin/cp")
            .arg("-c")
            .arg(from)
            .arg(to)
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|status| status.success());
    if !cloned {
        let _ = std::fs::remove_file(to);
        std::fs::copy(from, to)
            .with_context(|| format!("copying {} to {}", from.display(), to.display()))?;
    }
    let (want, got) = (std::fs::metadata(from)?.len(), std::fs::metadata(to)?.len());
    if want != got {
        return Err(anyhow!(
            "the copy of {} is {got} bytes and should be {want}",
            from.display()
        ));
    }
    Ok(())
}

/// The current name of a table, given a current or an old one. A read-only view over an old table
/// reports the old table to a privacy check, which holds the new names.
pub fn new_table_name(name: &str) -> &str {
    TABLES
        .iter()
        .find(|(old, _)| *old == name)
        .map_or(name, |(_, new)| new)
}

/// Opens a database that could not be renamed, read-only, and gives it the new names with
/// temporary views, so what is in it can still be read. Nothing in the file changes.
pub fn open_read_only(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let normal = Connection::open_with_flags(path, flags).and_then(|connection| {
        // Opening does not read the file: a first read shows whether the log can be shared.
        connection.query_row("SELECT count(*) FROM sqlite_master", [], |_| Ok(()))?;
        Ok(connection)
    });
    let connection = match normal {
        Ok(connection) => connection,
        Err(_) => {
            // A database in write-ahead mode in a folder that cannot be written has nowhere to
            // put the file readers share. Read it as it stands in the file, which changes nothing.
            let escaped = path
                .to_string_lossy()
                .replace('%', "%25")
                .replace('?', "%3f")
                .replace('#', "%23");
            Connection::open_with_flags(
                format!("file:{escaped}?immutable=1"),
                flags | OpenFlags::SQLITE_OPEN_URI,
            )?
        }
    };
    connection.busy_timeout(Duration::from_secs(30))?;
    for (old, new) in TABLES {
        if !table_exists(&connection, old)? || table_exists(&connection, new)? {
            continue;
        }
        let list = columns_in_order(&connection, old)?
            .into_iter()
            .map(|column| {
                if column == OLD_COLUMN {
                    format!("{OLD_COLUMN} AS {NEW_COLUMN}")
                } else {
                    column
                }
            })
            .collect::<Vec<_>>()
            .join(", ");
        connection.execute_batch(&format!(
            "CREATE TEMP VIEW {new} AS SELECT {list} FROM main.{old}"
        ))?;
    }
    // A map made before the table of records was named for what it holds is read through its old name.
    if table_exists(&connection, "ntfs_records")? && !table_exists(&connection, "records")? {
        connection.execute_batch("CREATE TEMP VIEW records AS SELECT * FROM main.ntfs_records")?;
    }
    connection.execute_batch("PRAGMA query_only = ON;")?;
    Ok(connection)
}

fn columns_in_order(connection: &Connection, table: &str) -> Result<Vec<String>> {
    Ok(connection
        .prepare(&format!(
            "SELECT name FROM pragma_table_info('{table}') ORDER BY cid"
        ))?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<_>>()?)
}

pub mod stored_names;

#[cfg(test)]
mod tests;
