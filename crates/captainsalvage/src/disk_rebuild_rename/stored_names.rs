//! Renames the one stored name that carries a retired word.
//!
//! Version 0.4.0 turned an old treasure list into a Disk Rebuild called "Imported treasure list", with
//! a folder of the same name at its root, and noted in the map (`scan_state`, key
//! `treasure_list_imported`) that it had. The import code is gone, but the names it wrote are data,
//! and the window shows them. When a map is opened, that Disk Rebuild and that folder are renamed,
//! once, to "Files picked before 0.4" (with " (2)", " (3)" ... if another Disk Rebuild already has
//! the name), under the same rules as the rename of the Disk Rebuild tables:
//!
//! - nothing is changed in a map that did not have the import run in it, and a name is changed only
//!   when it is exactly the one the import wrote: a name a person chose is never touched;
//! - a copy of the map is made first, next to it, named `<map>.before-retired-name`, by an APFS clone
//!   or a plain copy, and an existing copy is never overwritten;
//! - the change runs in one transaction that starts with `BEGIN IMMEDIATE`, is checked with
//!   `PRAGMA quick_check` and is rolled back if the check or anything else fails;
//! - the map says (`scan_state`, key `retired_names_renamed`) that it has run, so a name a person
//!   later gives back to the Disk Rebuild is left alone;
//! - a map that cannot be changed is left exactly as it is.
//!
//! Other places the app keeps text in a map were looked at for retired words: the tables and columns
//! (renamed by the module above), `scan_state` keys, interpretation and finding kinds, and the
//! catalog's saved scans hold none. The old `basket` table, kept by the import when a file it listed
//! could not be placed, and the key above are not shown anywhere, and stay as they are.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use std::path::Path;

/// What the import called the Disk Rebuild and the folder at its root.
pub const OLD_NAME: &str = "Imported treasure list";
/// What they are called now.
pub const NEW_NAME: &str = "Files picked before 0.4";
/// Added to a map's file name to name the copy made before the change.
pub const BACKUP_SUFFIX: &str = ".before-retired-name";
/// Set in `scan_state` by the import, which no longer exists.
const IMPORTED_KEY: &str = "treasure_list_imported";
/// Set in `scan_state` when this has run.
const DONE_KEY: &str = "retired_names_renamed";

/// What `run` did.
#[derive(Debug, PartialEq, Eq)]
pub enum Renamed {
    /// The map had no name to change, or it was done before.
    Nothing,
    /// The Disk Rebuild (and its folder) was renamed. Where the copy went.
    Done {
        new_name: String,
        backup: std::path::PathBuf,
    },
}

/// Renames the stored names, if the map at `path` (open as `connection`) needs it. A failure leaves
/// the map as it was, and says why.
pub fn run(path: &Path, connection: &Connection) -> Result<Renamed> {
    if state(connection, DONE_KEY)?.is_some() || state(connection, IMPORTED_KEY)?.is_none() {
        return Ok(Renamed::Nothing);
    }
    if !needs_renaming(connection)? {
        set_state(connection, DONE_KEY)?;
        return Ok(Renamed::Nothing);
    }
    // Fold the write-ahead log into the file when nobody else is reading, so the copy is one file.
    let _ = connection.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |_| Ok(()));
    connection.execute_batch("BEGIN IMMEDIATE")?;
    let changed = change(path, connection);
    match changed {
        Ok(renamed) => match connection.execute_batch("COMMIT") {
            Ok(()) => Ok(renamed),
            Err(error) => {
                let _ = connection.execute_batch("ROLLBACK");
                Err(error.into())
            }
        },
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(error)
        }
    }
}

fn state(connection: &Connection, key: &str) -> Result<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT value FROM scan_state WHERE key = ?1",
            [key],
            |row| row.get(0),
        )
        .optional()?)
}

fn set_state(connection: &Connection, key: &str) -> Result<()> {
    connection.execute(
        "INSERT INTO scan_state (key, value) VALUES (?1, '1') ON CONFLICT (key) DO UPDATE SET value = '1'",
        [key],
    )?;
    Ok(())
}

/// Whether a Disk Rebuild or a root folder has exactly the name the import wrote.
fn needs_renaming(connection: &Connection) -> Result<bool> {
    let one = |sql: &str| -> Result<bool> {
        Ok(connection.query_row(sql, [OLD_NAME], |row| row.get::<_, i64>(0))? > 0)
    };
    Ok(one("SELECT count(*) FROM disk_rebuilds WHERE name = ?1")?
        || one("SELECT count(*) FROM disk_rebuild_items WHERE name = ?1 AND parent_item IS NULL")?)
}

/// The change, inside the caller's transaction: the copy first, then the names, then the check.
fn change(path: &Path, connection: &Connection) -> Result<Renamed> {
    // Another opener may have done it while this one waited for the lock.
    if state(connection, DONE_KEY)?.is_some() || !needs_renaming(connection)? {
        set_state(connection, DONE_KEY)?;
        return Ok(Renamed::Nothing);
    }
    let backup = super::back_up_with_suffix(path, BACKUP_SUFFIX)
        .context("the copy made before the change failed")?;
    let taken = |name: &str| -> Result<bool> {
        Ok(connection.query_row(
            "SELECT count(*) FROM disk_rebuilds WHERE name = ?1",
            [name],
            |row| row.get::<_, i64>(0),
        )? > 0)
    };
    let mut new_name = NEW_NAME.to_string();
    let mut number = 2;
    while taken(&new_name)? {
        new_name = format!("{NEW_NAME} ({number})");
        number += 1;
    }
    connection.execute(
        "UPDATE disk_rebuilds SET name = ?1 WHERE name = ?2",
        [&new_name, OLD_NAME],
    )?;
    connection.execute(
        "UPDATE disk_rebuild_items SET name = ?1 WHERE name = ?2 AND parent_item IS NULL",
        [NEW_NAME, OLD_NAME],
    )?;
    set_state(connection, DONE_KEY)?;
    super::quick_check(connection)?;
    Ok(Renamed::Done { new_name, backup })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::map::MapDb;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct Folder(std::path::PathBuf);
    impl Folder {
        fn new() -> Self {
            static NEXT: AtomicU32 = AtomicU32::new(0);
            let path = std::env::temp_dir().join(format!(
                "captainsalvage-stored-names-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::SeqCst)
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn map(&self) -> std::path::PathBuf {
            self.0.join("map.sqlite")
        }
    }
    impl Drop for Folder {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A made-up map as 0.4.x left it: the import ran, and made a Disk Rebuild and a root folder with the
    /// old name, beside a Disk Rebuild a person made.
    fn imported_map(folder: &Folder, key: bool) {
        let database = MapDb::open(&folder.map()).unwrap();
        let rebuild = database.disk_rebuild_create(OLD_NAME).unwrap();
        database
            .disk_rebuild_folder(rebuild, OLD_NAME, Default::default())
            .unwrap();
        database.disk_rebuild_create("Holiday photos").unwrap();
        if key {
            database.set_state(IMPORTED_KEY, "1").unwrap();
        }
    }

    fn names(path: &Path, sql: &str) -> Vec<String> {
        let connection = Connection::open(path).unwrap();
        let mut statement = connection.prepare(sql).unwrap();
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    }

    fn rebuild_names(path: &Path) -> Vec<String> {
        names(path, "SELECT name FROM disk_rebuilds ORDER BY id")
    }

    fn item_names(path: &Path) -> Vec<String> {
        names(path, "SELECT name FROM disk_rebuild_items ORDER BY id")
    }

    fn backups(folder: &Folder) -> Vec<std::path::PathBuf> {
        let mut found: Vec<_> = std::fs::read_dir(&folder.0)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.to_string_lossy().contains(BACKUP_SUFFIX))
            .collect();
        found.sort();
        found
    }

    #[test]
    fn the_name_the_import_wrote_is_renamed_once_when_the_map_is_opened_and_a_copy_is_kept() {
        let folder = Folder::new();
        imported_map(&folder, true);
        drop(MapDb::open(&folder.map()).unwrap());
        assert_eq!(rebuild_names(&folder.map()), [NEW_NAME, "Holiday photos"]);
        assert_eq!(
            item_names(&folder.map()),
            [NEW_NAME],
            "the folder at its root too"
        );
        let copies = backups(&folder);
        assert_eq!(copies.len(), 1, "{copies:?}");
        assert_eq!(
            rebuild_names(&copies[0]),
            [OLD_NAME, "Holiday photos"],
            "the copy is as it was"
        );
        // A later open changes nothing and makes no second copy, even when the name is given back.
        Connection::open(folder.map())
            .unwrap()
            .execute(
                "UPDATE disk_rebuilds SET name = ?1 WHERE id = 1",
                [OLD_NAME],
            )
            .unwrap();
        drop(MapDb::open(&folder.map()).unwrap());
        assert_eq!(rebuild_names(&folder.map())[0], OLD_NAME, "it runs once");
        assert_eq!(backups(&folder).len(), 1);
    }

    #[test]
    fn a_name_already_taken_gets_a_number() {
        let folder = Folder::new();
        imported_map(&folder, true);
        {
            let database = MapDb::open_without_renaming(&folder.map()).unwrap();
            database.disk_rebuild_create(NEW_NAME).unwrap();
            database
                .disk_rebuild_create(&format!("{NEW_NAME} (2)"))
                .unwrap();
        }
        drop(MapDb::open(&folder.map()).unwrap());
        assert_eq!(
            rebuild_names(&folder.map())[0],
            format!("{NEW_NAME} (3)"),
            "{:?}",
            rebuild_names(&folder.map())
        );
    }

    #[test]
    fn a_name_a_person_chose_is_never_touched() {
        let folder = Folder::new();
        {
            let database = MapDb::open_without_renaming(&folder.map()).unwrap();
            database
                .disk_rebuild_create("imported treasure list")
                .unwrap();
            database
                .disk_rebuild_create("Imported treasure list 2")
                .unwrap();
            database.disk_rebuild_create("Mine").unwrap();
            database.set_state(IMPORTED_KEY, "1").unwrap();
        }
        drop(MapDb::open(&folder.map()).unwrap());
        assert_eq!(
            rebuild_names(&folder.map()),
            ["imported treasure list", "Imported treasure list 2", "Mine"],
            "only the exact name is changed"
        );
        assert!(backups(&folder).is_empty(), "nothing changed, so no copy");
    }

    #[test]
    fn a_map_the_import_never_ran_in_is_left_alone_whatever_a_person_called_a_disk_rebuild() {
        let folder = Folder::new();
        imported_map(&folder, false);
        drop(MapDb::open(&folder.map()).unwrap());
        assert_eq!(rebuild_names(&folder.map()), [OLD_NAME, "Holiday photos"]);
        assert!(backups(&folder).is_empty());
    }

    #[test]
    fn a_map_with_nothing_to_rename_says_it_is_done_without_a_copy() {
        let folder = Folder::new();
        {
            let database = MapDb::open_without_renaming(&folder.map()).unwrap();
            database.disk_rebuild_create("Holiday photos").unwrap();
            database.set_state(IMPORTED_KEY, "1").unwrap();
        }
        drop(MapDb::open(&folder.map()).unwrap());
        assert!(backups(&folder).is_empty());
        let database = MapDb::open(&folder.map()).unwrap();
        assert_eq!(database.state(DONE_KEY).unwrap().as_deref(), Some("1"));
    }

    #[test]
    fn a_map_that_cannot_be_changed_is_left_exactly_as_it_is() {
        use std::os::unix::fs::PermissionsExt;
        let folder = Folder::new();
        imported_map(&folder, true);
        let before = std::fs::read(folder.map()).unwrap();
        let mut permissions = std::fs::metadata(&folder.0).unwrap().permissions();
        // A folder nothing can be added to: the copy cannot be made.
        permissions.set_mode(0o555);
        std::fs::set_permissions(&folder.0, permissions.clone()).unwrap();
        let connection = Connection::open(folder.map()).unwrap();
        let result = run(&folder.map(), &connection);
        permissions.set_mode(0o755);
        std::fs::set_permissions(&folder.0, permissions).unwrap();
        drop(connection);
        assert!(result.is_err(), "{result:?}");
        assert_eq!(rebuild_names(&folder.map()), [OLD_NAME, "Holiday photos"]);
        assert_eq!(std::fs::read(folder.map()).unwrap().len(), before.len());
    }
}
