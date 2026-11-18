//! The old treasure list, turned into a Disk Rebuild.
//!
//! Before Disk Rebuilds replaced it, a scan could hold a flat list of files and folders to
//! copy, in the `basket` table. A list with items becomes one Disk Rebuild, "Imported treasure
//! list", for its scan, once. The table is dropped only when every item was placed.

use crate::canvas::Place;
use crate::map::MapDb;
use anyhow::Result;
use rusqlite::OptionalExtension;

pub const IMPORTED_LIST_NAME: &str = "Imported treasure list";
const STATE_IMPORTED: &str = "treasure_list_imported";

struct OldItem {
    kind: String,
    target: i64,
}

impl MapDb {
    fn old_list_exists(&self) -> Result<bool> {
        Ok(self
            .connection
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'basket'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    }

    fn old_list_items(&self) -> Result<Vec<OldItem>> {
        let mut statement = self
            .connection
            .prepare("SELECT kind, target FROM basket ORDER BY position, id")?;
        let rows = statement.query_map([], |row| {
            Ok(OldItem {
                kind: row.get(0)?,
                target: row.get(1)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    /// Makes the old list a Disk Rebuild. Safe to call on every open: it does nothing once
    /// it has run, and nothing without an old list. Returns the Disk Rebuild it made.
    pub fn import_old_treasure_list(&self) -> Result<Option<i64>> {
        if !self.old_list_exists()? || self.state(STATE_IMPORTED)?.is_some() {
            return Ok(None);
        }
        let items = self.old_list_items()?;
        let transaction = self.connection.unchecked_transaction()?;
        let mut made = None;
        let mut left_out = 0;
        if !items.is_empty() {
            let rebuild = self.canvas_create(IMPORTED_LIST_NAME)?;
            let root = self.canvas_folder(rebuild, IMPORTED_LIST_NAME, Place::default())?;
            let inside = Place {
                item: Some(root),
                head: None,
            };
            for item in &items {
                let placed = if item.kind == "folder" {
                    u32::try_from(item.target)
                        .map_err(anyhow::Error::from)
                        .and_then(|head| self.canvas_place(rebuild, head, inside, false))
                } else {
                    self.canvas_place_file(rebuild, item.target, inside)
                };
                if placed.is_err() {
                    left_out += 1;
                }
            }
            made = Some(rebuild);
        }
        self.set_state(STATE_IMPORTED, "1")?;
        // Items that could not be placed, because the disk no longer has them, stay in the
        // old table, so nothing is lost.
        if left_out == 0 {
            self.connection.execute_batch("DROP TABLE basket;")?;
        }
        transaction.commit()?;
        Ok(made)
    }
}
