use super::*;
use crate::map::MapDb;
use rusqlite::types::ValueRef;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Barrier};

/// The map's schema as an older build made it. It is a fixture, so it may use the old names.
const OLD_SCHEMA: &str = include_str!("../../tests/fixtures/map_schema_before_disk_rebuild.sql");

/// Only the statements that make the tables and indexes, as an older build ran at every start.
fn old_creates() -> &'static str {
    OLD_SCHEMA.split("\nALTER TABLE").next().unwrap()
}

/// A folder of one test, removed when it ends.
struct Folder(PathBuf);

impl Folder {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-rename-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn map(&self) -> PathBuf {
        self.0.join("map.sqlite")
    }
}

impl Drop for Folder {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Every table of the old schema, with the row count each is filled to.
fn old_tables(connection: &Connection) -> Vec<String> {
    connection
        .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

/// Fills a table with three rows of whatever its columns hold, different in every row.
fn fill(connection: &Connection, table: &str) {
    let columns: Vec<(String, String)> = connection
        .prepare(&format!(
            "SELECT name, type FROM pragma_table_info('{table}') ORDER BY cid"
        ))
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    for row in 1..=3 {
        let values: Vec<String> = columns
            .iter()
            .map(|(name, kind)| match kind.as_str() {
                "INTEGER" => (row * 10 + name.len()).to_string(),
                "REAL" => format!("{row}.25"),
                _ => format!("'{table}-{name}-{row}'"),
            })
            .collect();
        connection
            .execute_batch(&format!(
                "INSERT INTO {table} VALUES ({})",
                values.join(", ")
            ))
            .unwrap();
    }
}

/// A database the way an older build left it: every table filled, in WAL mode, closed.
fn old_database(folder: &Folder) -> PathBuf {
    let path = folder.map();
    let connection = Connection::open(&path).unwrap();
    connection
        .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .unwrap();
    connection.execute_batch(OLD_SCHEMA).unwrap();
    for table in old_tables(&connection) {
        fill(&connection, &table);
    }
    drop(connection);
    assert!(!wal_of(&path).exists(), "a closed database leaves no log");
    path
}

/// The agent change journal and the FAT weak marks and walks, which a database from before them gets empty when it is opened.
const ADDED_AFTER_THE_RENAME: &[&str] = &[
    "fat_weak",
    "fat_walks",
    "agent_sessions",
    "disk_rebuild_changes",
    "disk_rebuild_agent_marks",
    "disk_rebuild_snapshots",
];

/// Every row of every table, as text, by the table's current name.
fn rows(connection: &Connection) -> Vec<(String, Vec<Vec<String>>)> {
    let mut all = Vec::new();
    for table in old_tables(connection) {
        // Tables made empty by the schema after the rename have no old rows to compare.
        if table.starts_with("sqlite_") || ADDED_AFTER_THE_RENAME.contains(&table.as_str()) {
            continue;
        }
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let width = statement.column_count();
        let found = statement
            .query_map([], |row| {
                (0..width)
                    .map(|index| {
                        Ok(match row.get_ref(index)? {
                            ValueRef::Null => "NULL".to_string(),
                            ValueRef::Integer(number) => number.to_string(),
                            ValueRef::Real(number) => number.to_string(),
                            ValueRef::Text(text) => String::from_utf8_lossy(text).into_owned(),
                            ValueRef::Blob(blob) => format!("{blob:?}"),
                        })
                    })
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        all.push((new_table_name(&table).to_string(), found));
    }
    all.sort();
    all
}

fn names(connection: &Connection) -> Vec<String> {
    connection
        .prepare("SELECT name || ' ' || coalesce(sql, '') FROM sqlite_master")
        .unwrap()
        .query_map([], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap()
}

fn has_no_old_name(connection: &Connection) {
    for text in names(connection) {
        assert!(!text.contains("canvas"), "an old name is left: {text}");
    }
}

fn backups(folder: &Folder) -> Vec<String> {
    let mut found: Vec<String> = std::fs::read_dir(&folder.0)
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.contains(BACKUP_SUFFIX))
        .collect();
    found.sort();
    found
}

fn check_ok(connection: &Connection) {
    let answer: String = connection
        .query_row("PRAGMA integrity_check", [], |row| row.get(0))
        .unwrap();
    assert_eq!(answer, "ok");
}

#[test]
fn the_bundled_sqlite_can_rename_columns() {
    assert!(rusqlite::version_number() >= 3_025_000);
}

#[test]
fn an_old_database_is_renamed_with_every_row_kept_and_a_copy_made() {
    let folder = Folder::new();
    let path = old_database(&folder);
    let original = std::fs::read(&path).unwrap();
    let before = rows(&Connection::open(&path).unwrap());
    assert!(before
        .iter()
        .any(|(name, found)| name == "disk_rebuild_items" && found.len() == 3));
    // The original is untouched by reading it.
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let Prepared::Migrated { backup } = prepare(&path).unwrap() else {
        panic!("an old database is migrated");
    };
    let backup = backup.expect("a copy is made");
    assert_eq!(
        backup,
        folder.0.join("map.sqlite.before-disk-rebuild-rename")
    );
    assert_eq!(
        std::fs::read(&backup).unwrap(),
        original,
        "the copy is the original"
    );

    let connection = Connection::open(&path).unwrap();
    assert_eq!(
        rows(&connection),
        before,
        "every row is where it was, under the new names"
    );
    has_no_old_name(&connection);
    check_ok(&connection);
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let index: i64 = connection
        .query_row(
            "SELECT count(*) FROM sqlite_master WHERE type = 'index' AND name = 'disk_rebuild_items_disk_rebuild'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index, 1);
}

#[test]
fn running_it_again_changes_nothing() {
    let folder = Folder::new();
    let path = old_database(&folder);
    prepare(&path).unwrap();
    let after = std::fs::read(&path).unwrap();
    assert_eq!(prepare(&path).unwrap(), Prepared::Ready);
    assert_eq!(std::fs::read(&path).unwrap(), after);
    assert_eq!(backups(&folder), ["map.sqlite.before-disk-rebuild-rename"]);
}

#[test]
fn opening_the_map_renames_it_and_it_then_works() {
    let folder = Folder::new();
    let path = old_database(&folder);
    let database = MapDb::open(&path).unwrap();
    assert!(database.read_only_reason().is_none());
    let count: i64 = database
        .connection
        .query_row(
            "SELECT count(*) FROM disk_rebuild_items WHERE disk_rebuild = 16",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    database
        .connection
        .execute_batch("INSERT INTO disk_rebuilds (name, created_at_millis, updated_at_millis) VALUES ('new', 1, 1)")
        .unwrap();
    drop(database);
    // Opening again does not copy again.
    MapDb::open(&path).unwrap();
    assert_eq!(backups(&folder).len(), 1);
}

#[test]
fn a_database_with_some_names_already_new_is_finished() {
    let folder = Folder::new();
    let path = old_database(&folder);
    {
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch(
                "ALTER TABLE canvases RENAME TO disk_rebuilds; \
                 ALTER TABLE canvas_choices RENAME TO disk_rebuild_choices; \
                 ALTER TABLE disk_rebuild_choices RENAME COLUMN canvas TO disk_rebuild;",
            )
            .unwrap();
    }
    let before = rows(&Connection::open(&path).unwrap());
    assert!(matches!(prepare(&path).unwrap(), Prepared::Migrated { .. }));
    let connection = Connection::open(&path).unwrap();
    assert_eq!(rows(&connection), before);
    has_no_old_name(&connection);
    check_ok(&connection);
}

#[test]
fn an_older_build_that_made_its_empty_tables_again_loses_nothing() {
    let folder = Folder::new();
    let path = old_database(&folder);
    prepare(&path).unwrap();
    let migrated = rows(&Connection::open(&path).unwrap());
    // An older build opens the migrated file and makes the tables it knows, empty.
    Connection::open(&path)
        .unwrap()
        .execute_batch(old_creates())
        .unwrap();
    assert!(matches!(prepare(&path).unwrap(), Prepared::Migrated { .. }));
    let connection = Connection::open(&path).unwrap();
    assert_eq!(rows(&connection), migrated);
    has_no_old_name(&connection);
    // The first copy is not replaced: the second change gets its own.
    assert_eq!(
        backups(&folder),
        [
            "map.sqlite.before-disk-rebuild-rename",
            "map.sqlite.before-disk-rebuild-rename.2"
        ]
    );
}

#[test]
fn rows_an_older_build_added_are_kept_when_they_cannot_be_merged() {
    let folder = Folder::new();
    let path = old_database(&folder);
    prepare(&path).unwrap();
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch(old_creates()).unwrap();
    connection
        .execute_batch("INSERT INTO canvases (name, created_at_millis, updated_at_millis) VALUES ('older build', 1, 1)")
        .unwrap();
    drop(connection);
    let kept: i64 = {
        let connection = Connection::open(&path).unwrap();
        connection
            .query_row("SELECT count(*) FROM disk_rebuilds", [], |row| row.get(0))
            .unwrap()
    };
    assert!(matches!(prepare(&path).unwrap(), Prepared::Migrated { .. }));
    let connection = Connection::open(&path).unwrap();
    let new: i64 = connection
        .query_row("SELECT count(*) FROM disk_rebuilds", [], |row| row.get(0))
        .unwrap();
    assert_eq!(new, kept);
    let aside: String = connection
        .query_row("SELECT name FROM unmerged_canvases", [], |row| row.get(0))
        .unwrap();
    assert_eq!(aside, "older build");
}

#[test]
fn openers_at_the_same_moment_wait_for_each_other_and_see_the_finished_rename() {
    let folder = Folder::new();
    let path = old_database(&folder);
    let before = rows(&Connection::open(&path).unwrap());
    let barrier = Arc::new(Barrier::new(6));
    let workers: Vec<_> = (0..6)
        .map(|_| {
            let (path, barrier) = (path.clone(), barrier.clone());
            std::thread::spawn(move || {
                barrier.wait();
                let database = MapDb::open(&path).unwrap();
                assert!(database.read_only_reason().is_none());
                database
                    .connection
                    .query_row("SELECT count(*) FROM disk_rebuild_items", [], |row| {
                        row.get::<_, i64>(0)
                    })
                    .unwrap()
            })
        })
        .collect();
    for worker in workers {
        assert_eq!(worker.join().unwrap(), 3);
    }
    assert_eq!(
        backups(&folder).len(),
        1,
        "one copy, made by the one that did the work"
    );
    let connection = Connection::open(&path).unwrap();
    assert_eq!(rows(&connection), before);
    check_ok(&connection);
}

#[test]
fn a_read_only_file_is_left_unchanged_and_opens_with_the_new_names_and_a_message() {
    let folder = Folder::new();
    let path = old_database(&folder);
    let original = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o444)).unwrap();
    let Prepared::ReadOnly(message) = prepare(&path).unwrap() else {
        panic!("a read-only file cannot be renamed");
    };
    assert!(message.contains("read-only"), "{message}");
    assert!(backups(&folder).is_empty());
    let database = MapDb::open(&path).unwrap();
    assert!(database.read_only_reason().unwrap().contains("read-only"));
    let found: Vec<(i64, i64)> = database
        .connection
        .prepare("SELECT id, disk_rebuild FROM disk_rebuild_items ORDER BY id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<rusqlite::Result<_>>()
        .unwrap();
    assert_eq!(found.len(), 3);
    assert!(database
        .connection
        .execute_batch("INSERT INTO disk_rebuilds (name, created_at_millis, updated_at_millis) VALUES ('x', 1, 1)")
        .is_err());
    drop(database);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        original,
        "not one byte changed"
    );
}

#[test]
fn a_read_only_folder_is_left_unchanged_and_still_opens_for_reading() {
    let folder = Folder::new();
    let path = old_database(&folder);
    let original = std::fs::read(&path).unwrap();
    std::fs::set_permissions(&folder.0, std::fs::Permissions::from_mode(0o555)).unwrap();
    let database = MapDb::open(&path).unwrap();
    assert!(database.read_only_reason().is_some());
    let count: i64 = database
        .connection
        .query_row("SELECT count(*) FROM disk_rebuild_suggestions", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(count, 3);
    drop(database);
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(backups(&folder).is_empty());
}

#[test]
fn a_rename_that_fails_rolls_back_and_changes_nothing() {
    let folder = Folder::new();
    let path = old_database(&folder);
    // A view that no longer works makes SQLite refuse to rename the table it reads.
    Connection::open(&path)
        .unwrap()
        .execute_batch("CREATE VIEW broken AS SELECT nothing_here FROM canvases")
        .unwrap();
    let original = std::fs::read(&path).unwrap();
    let Prepared::ReadOnly(message) = prepare(&path).unwrap() else {
        panic!("a failed rename leaves the database as it was");
    };
    assert!(message.contains("unchanged"), "{message}");
    assert_eq!(std::fs::read(&path).unwrap(), original);
    let connection = Connection::open(&path).unwrap();
    assert!(names(&connection)
        .iter()
        .any(|text| text.starts_with("canvases ")));
    check_ok(&connection);
}

#[test]
fn a_new_database_has_only_the_new_names() {
    let folder = Folder::new();
    let path = folder.map();
    let database = MapDb::open(&path).unwrap();
    has_no_old_name(&database.connection);
    let version: i64 = database
        .connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, SCHEMA_VERSION);
    let tables = old_tables(&database.connection);
    for table in [
        "disk_rebuilds",
        "disk_rebuild_items",
        "disk_rebuild_suggestions",
        "disk_rebuild_choices",
    ] {
        assert!(tables.iter().any(|name| name == table), "{table}");
    }
    drop(database);
    assert!(backups(&folder).is_empty());
}

#[test]
fn a_log_that_holds_changes_is_copied_with_the_database() {
    let folder = Folder::new();
    let path = old_database(&folder);
    // A reader holds the log open, so closing the writer cannot fold it into the file.
    let writer = Connection::open(&path).unwrap();
    writer
        .query_row("PRAGMA journal_mode = WAL", [], |_| Ok(()))
        .unwrap();
    let reader = Connection::open(&path).unwrap();
    reader
        .execute_batch("BEGIN; SELECT count(*) FROM canvases;")
        .unwrap();
    writer
        .execute_batch("INSERT INTO canvases (name, created_at_millis, updated_at_millis) VALUES ('late', 9, 9)")
        .unwrap();
    drop(writer);
    drop(reader);
    // Whatever the log held, the copy that is made carries it.
    let target = back_up(&path).unwrap();
    let connection = Connection::open(&target).unwrap();
    let found: i64 = connection
        .query_row("SELECT count(*) FROM canvases", [], |row| row.get(0))
        .unwrap();
    assert_eq!(found, 4);
}

#[test]
fn a_renamed_database_also_has_the_agent_change_journal() {
    let folder = Folder::new();
    let path = old_database(&folder);
    let database = MapDb::open(&path).unwrap();
    for table in ADDED_AFTER_THE_RENAME {
        let rows: i64 = database
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 0, "{table}");
    }
    // An agent can change a Disk Rebuild that came through the rename, and the person can undo it.
    let existing: i64 = database
        .connection
        .query_row("SELECT id FROM disk_rebuilds LIMIT 1", [], |row| row.get(0))
        .unwrap();
    let who = crate::disk_rebuild_agent::AgentConnection::default();
    database
        .agent_change(
            &who,
            &Default::default(),
            "disk_rebuild_rename",
            "Renamed",
            Some(existing),
            |database| {
                database.disk_rebuild_rename(existing, "By an agent")?;
                Ok((serde_json::json!({"ok": true}), existing))
            },
        )
        .unwrap();
    assert!(database.disk_rebuilds().unwrap()[0].made_by_agent);
    database.agent_undo_last(None).unwrap();
    assert!(!database.disk_rebuilds().unwrap()[0].made_by_agent);
}
