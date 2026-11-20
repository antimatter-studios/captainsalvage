//! The person's own rebuild of the folder tree, from the pieces the scan found.
//!
//! A damaged disk leaves pieces: found folders whose parent is gone. A Disk Rebuild records where the
//! person put each one, as if doing a jigsaw: inside another folder, merged into one so their
//! contents become a single folder, or inside a folder they made up because the disk lost it.
//!
//! The rebuilt tree is worked out from those records and the map whenever it is read, so it
//! follows a rebuild of the map. It is never written anywhere but the app's own database, and
//! never to the disk. Where two pieces hold a file of the same name, the copy with the best data
//! quality is shown, unless the person chose another.
//!
//! A Disk Rebuild is about arranging what was found. It does not change how the map reads the disk,
//! which is what hypotheses do.

use crate::map::{file_row, node_row, MapDb, Row, FILE_COLUMNS, NODE_COLUMNS};
use crate::workspace::now_millis;
use anyhow::{bail, Context, Result};
use rusqlite::{params, OptionalExtension};
use serde::Serialize;
use std::cell::RefCell;
use std::collections::{BTreeMap, HashMap, HashSet};

/// A folder the person made up, because the disk lost the real one.
pub const ITEM_FOLDER: &str = "folder";
/// A found folder, placed as a folder of its own.
pub const ITEM_PIECE: &str = "piece";
/// A found folder whose contents join the folder it is placed in.
pub const ITEM_MERGE: &str = "merge";
/// One found file, placed on its own: a small layer of one file.
pub const ITEM_FILE: &str = "file";

/// The id the next item gets: above every id used before, including items since taken off, so an
/// id never names two things and nothing recorded about an old item (what an agent made, a
/// snapshot) can end up on a new one. The high-water mark is kept beside the map's other state.
const NEXT_ITEM_ID: &str = "(SELECT MAX(COALESCE((SELECT MAX(id) FROM disk_rebuild_items), 0), \
     CAST(COALESCE((SELECT value FROM scan_state WHERE key = 'item_high_water'), '0') AS INTEGER)) + 1)";
const NEXT_REBUILD_ID: &str = "(SELECT MAX(COALESCE((SELECT MAX(id) FROM disk_rebuilds), 0), \
     CAST(COALESCE((SELECT value FROM scan_state WHERE key = 'rebuild_high_water'), '0') AS INTEGER)) + 1)";

/// Deeper than any real tree, so a damaged one that loops cannot hang a walk.
const MAX_DEPTH: usize = 256;
const MAX_NAME_CHARS: usize = 255;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskRebuild {
    pub id: i64,
    pub name: String,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
    pub items: u64,
    /// An agent made or renamed it, and the person has not undone that.
    pub made_by_agent: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskRebuildItem {
    pub id: i64,
    pub kind: String,
    /// The found folder, for a piece or a merge.
    pub head: Option<u32>,
    /// The name in the rebuilt tree. For a piece it replaces the folder's own, when set.
    pub name: Option<String>,
    /// The item it sits in. Only the root has none.
    pub parent_item: Option<i64>,
    /// A found folder inside that item's piece, when it was placed deeper than the item itself.
    pub parent_head: Option<u32>,
    /// The found file, for a file placed on its own.
    pub entry: Option<i64>,
    /// An agent made or changed it, and the person has not undone that.
    pub made_by_agent: bool,
}

/// Where something is put: in a Disk Rebuild item, and optionally in a found folder inside it.
/// Nothing at all means the Disk Rebuild itself: the root when there is none yet, inside it after.
#[derive(Clone, Copy, Debug, Default)]
pub struct Place {
    pub item: Option<i64>,
    pub head: Option<u32>,
}

/// One folder of the rebuilt tree, and everything its contents come from.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuiltFolder {
    /// What to put in a path to reach this folder: its name, or `lost:<cluster>` without one.
    pub path_name: String,
    pub name: Option<String>,
    /// The found folder, when the name is missing because the disk lost it.
    pub lost_head: Option<u32>,
    /// `made` when only the person's folders make it up, `found` when only found folders do,
    /// and `placed` when a piece was put here.
    pub origin: String,
    /// More than one folder became this one, by merging or by sharing a name.
    pub merged: bool,
    pub items: Vec<i64>,
    pub heads: Vec<u32>,
    pub files: u64,
    pub bytes: u64,
    pub intact_files: u64,
    pub intact_bytes: u64,
    /// An agent made or changed one of the items this folder is made of.
    pub made_by_agent: bool,
    /// How many of its files (anywhere below it) have bytes in a bad sector of the source, and how many
    /// bytes. Left out when none do, and when the source has no health map. Never a reason to refuse
    /// anything: the files are still placed and copied, their bad bytes as zeros.
    #[serde(skip_serializing_if = "is_zero")]
    pub bad_sector_files: u64,
    #[serde(skip_serializing_if = "is_zero")]
    pub bad_sector_bytes: u64,
}

fn is_zero(value: &u64) -> bool {
    *value == 0
}

/// One file of the rebuilt tree, with every copy of it that the pieces hold.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RebuiltFile {
    pub name: String,
    pub chosen: Row,
    /// The person picked this copy, rather than the best one being picked for them.
    pub by_person: bool,
    /// For a clash, what decided the pick: "person", "quality" (tested, whole, checksum or
    /// size), "date" (the newer copy) or "layer" (the upper layer, when all else ties).
    pub decided_by: Option<String>,
    /// The copies differ, and nothing measured tells them apart, so the person should look.
    pub needs_you: bool,
    /// Every distinct copy, best first. More than one is a clash.
    pub copies: Vec<Row>,
    /// An agent placed this file on its own, or chose its copy.
    pub made_by_agent: bool,
    /// The bytes of the chosen copy that lie in a bad sector of the source (clusters the rebuild or a
    /// reassembly takes from there included). Left out when none do.
    #[serde(skip_serializing_if = "is_zero")]
    pub bad_sector_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Listing {
    pub path: Vec<String>,
    pub folders: Vec<RebuiltFolder>,
    pub files: Vec<RebuiltFile>,
    pub total_files: u64,
    pub clashes: u64,
    /// Clashes the app picked a copy for, from what it measured.
    pub clashes_picked: u64,
    /// Clashes whose copies differ but measure the same, which the person should look at.
    pub clashes_need_you: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnusedPieces {
    pub rows: Vec<Row>,
    pub total: u64,
    pub total_files: u64,
    pub total_bytes: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
enum Source {
    Item(i64),
    Found(u32),
}

/// How good a copy of a file is, lower being better: tested good first, then by verdict.
fn verdict_rank(verdict: &str) -> u8 {
    match verdict {
        "tested-good" => 0,
        "live" => 1,
        "likely-intact" => 2,
        "plausible" => 3,
        "tested-gaps" => 4,
        "partial" => 5,
        "tested-bad" => 6,
        "overwritten" => 7,
        "damaged-entry" => 8,
        "empty" => 9,
        _ => 10,
    }
}

/// Whether a copy's clusters are all there, with none claimed twice.
fn whole(row: &Row) -> bool {
    row.clusters_found >= row.clusters_needed && row.clusters_conflict == 0
}

/// What tells two copies of a file apart, best first: tested or judged good, all its clusters
/// there, a valid entry checksum, and its size as the entry says. Equal here means the
/// measures cannot choose.
fn quality(row: &Row) -> (u8, bool, bool, bool) {
    (
        verdict_rank(&row.verdict),
        !whole(row),
        !row.checksum_ok,
        row.valid_length != row.length,
    )
}

/// The order copies are picked in: by quality, then the newer, then the upper layer.
fn pick_order(a: &Row, b: &Row, layers: &HashMap<i64, i64>) -> std::cmp::Ordering {
    let layer = |row: &Row| row.id.and_then(|id| layers.get(&id)).copied().unwrap_or(0);
    quality(a)
        .cmp(&quality(b))
        .then(b.confidence.total_cmp(&a.confidence))
        .then(b.modified.cmp(&a.modified))
        .then(layer(b).cmp(&layer(a)))
        .then(a.id.cmp(&b.id))
}

fn key(name: &str) -> String {
    // exFAT compares names without case, so a merge does too.
    name.to_lowercase()
}

fn clean_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        bail!("A folder needs a name.");
    }
    if name.contains('/') || name.contains('\0') {
        bail!("A folder name cannot contain a slash.");
    }
    Ok(name.chars().take(MAX_NAME_CHARS).collect())
}

/// The path name of a found folder whose name was lost. exFAT names cannot hold a colon, so
/// this can never be a real folder's name, and two lost folders never merge by accident.
fn lost_key(head: u32) -> String {
    format!("lost:{head}")
}

impl MapDb {
    pub fn disk_rebuilds(&self) -> Result<Vec<DiskRebuild>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT c.id, c.name, c.created_at_millis, c.updated_at_millis, \
             (SELECT COUNT(*) FROM disk_rebuild_items i WHERE i.disk_rebuild = c.id) \
             FROM disk_rebuilds c ORDER BY c.updated_at_millis DESC, c.id DESC",
        )?;
        let by_agent = self.agent_rebuilds()?;
        let rows = statement
            .query_map([], |row| {
                let id: i64 = row.get(0)?;
                Ok(DiskRebuild {
                    id,
                    name: row.get(1)?,
                    created_at_millis: row.get::<_, i64>(2)? as u64,
                    updated_at_millis: row.get::<_, i64>(3)? as u64,
                    items: row.get::<_, i64>(4)? as u64,
                    made_by_agent: by_agent.contains(&id),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn disk_rebuild_create(&self, name: &str) -> Result<i64> {
        let name = clean_name(name)?;
        let now = now_millis() as i64;
        self.connection.execute(
            &format!("INSERT INTO disk_rebuilds (id, name, created_at_millis, updated_at_millis) VALUES ({NEXT_REBUILD_ID}, ?1, ?2, ?2)"),
            params![name, now],
        )?;
        let id = self.connection.last_insert_rowid();
        self.note_high_water("rebuild_high_water", id)?;
        Ok(id)
    }

    /// Keeps the highest id given out, so it is never given out again.
    fn note_high_water(&self, key: &str, id: i64) -> Result<()> {
        self.connection.execute(
            "INSERT OR REPLACE INTO scan_state (key, value) VALUES (?1, ?2)",
            params![key, id.to_string()],
        )?;
        Ok(())
    }

    pub fn disk_rebuild_rename(&self, disk_rebuild: i64, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        self.need_disk_rebuild(disk_rebuild)?;
        self.connection.execute(
            "UPDATE disk_rebuilds SET name = ?2, updated_at_millis = ?3 WHERE id = ?1",
            params![disk_rebuild, name, now_millis() as i64],
        )?;
        Ok(())
    }

    pub fn disk_rebuild_delete(&self, disk_rebuild: i64) -> Result<()> {
        self.need_disk_rebuild(disk_rebuild)?;
        self.atomically(|| {
            let connection = &self.connection;
            connection.execute(
                "DELETE FROM disk_rebuild_items WHERE disk_rebuild = ?1",
                [disk_rebuild],
            )?;
            connection.execute(
                "DELETE FROM disk_rebuild_choices WHERE disk_rebuild = ?1",
                [disk_rebuild],
            )?;
            connection.execute(
                "DELETE FROM disk_rebuild_suggestions WHERE disk_rebuild = ?1",
                [disk_rebuild],
            )?;
            connection.execute("DELETE FROM disk_rebuilds WHERE id = ?1", [disk_rebuild])?;
            Ok(())
        })
    }

    /// Runs several writes as one: all of them happen, or none do. Savepoints nest, so a change
    /// made of smaller changes is still one.
    pub(crate) fn atomically<T>(&self, work: impl FnOnce() -> Result<T>) -> Result<T> {
        self.connection.execute_batch("SAVEPOINT disk_rebuild")?;
        match work() {
            Ok(value) => {
                self.connection.execute_batch("RELEASE disk_rebuild")?;
                Ok(value)
            }
            Err(error) => {
                let _ = self
                    .connection
                    .execute_batch("ROLLBACK TO disk_rebuild; RELEASE disk_rebuild");
                Err(error)
            }
        }
    }

    fn need_disk_rebuild(&self, disk_rebuild: i64) -> Result<()> {
        let found: Option<i64> = self
            .connection
            .query_row(
                "SELECT id FROM disk_rebuilds WHERE id = ?1",
                [disk_rebuild],
                |row| row.get(0),
            )
            .optional()?;
        found
            .map(|_| ())
            .with_context(|| format!("there is no Disk Rebuild {disk_rebuild}"))
    }

    fn touch_disk_rebuild(&self, disk_rebuild: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE disk_rebuilds SET updated_at_millis = ?2 WHERE id = ?1",
            params![disk_rebuild, now_millis() as i64],
        )?;
        Ok(())
    }

    /// Every item on a Disk Rebuild, in the order they were placed.
    pub fn disk_rebuild_items(&self, disk_rebuild: i64) -> Result<Vec<DiskRebuildItem>> {
        self.need_disk_rebuild(disk_rebuild)?;
        let mut statement = self.connection.prepare_cached(
            "SELECT id, kind, head, name, parent_item, parent_head, entry FROM disk_rebuild_items \
             WHERE disk_rebuild = ?1 ORDER BY position, id",
        )?;
        let marks = self.agent_marks(disk_rebuild)?;
        let rows = statement
            .query_map([disk_rebuild], |row| {
                let id: i64 = row.get(0)?;
                Ok(DiskRebuildItem {
                    id,
                    kind: row.get(1)?,
                    head: row.get(2)?,
                    name: row.get(3)?,
                    parent_item: row.get(4)?,
                    parent_head: row.get(5)?,
                    entry: row.get(6)?,
                    made_by_agent: marks.items.contains(&id),
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn disk_rebuild_item(&self, disk_rebuild: i64, item: i64) -> Result<DiskRebuildItem> {
        self.disk_rebuild_items(disk_rebuild)?
            .into_iter()
            .find(|found| found.id == item)
            .with_context(|| format!("there is no item {item} on Disk Rebuild {disk_rebuild}"))
    }

    fn disk_rebuild_root(&self, disk_rebuild: i64) -> Result<Option<DiskRebuildItem>> {
        Ok(self
            .disk_rebuild_items(disk_rebuild)?
            .into_iter()
            .find(|item| item.parent_item.is_none()))
    }

    /// The found folder at the top of the tree a found folder sits in.
    fn top_of(&self, head: u32) -> Result<u32> {
        let mut current = head;
        for _ in 0..MAX_DEPTH {
            let parent: Option<Option<u32>> = self
                .connection
                .query_row(
                    "SELECT parent FROM nodes WHERE head = ?1",
                    [current],
                    |row| row.get(0),
                )
                .optional()?;
            match parent {
                None => bail!("there is no folder at cluster {head}"),
                Some(None) => return Ok(current),
                Some(Some(next)) => current = next,
            }
        }
        bail!("the folder at cluster {head} is in a loop")
    }

    /// Works out where a place really is, refusing one that does not exist on this Disk Rebuild.
    fn resolve_place(&self, disk_rebuild: i64, place: Place) -> Result<Option<(i64, Option<u32>)>> {
        let item = match place.item {
            Some(item) => self.disk_rebuild_item(disk_rebuild, item)?,
            None => {
                match self.disk_rebuild_root(disk_rebuild)? {
                    Some(root) => root,
                    None => {
                        if place.head.is_some() {
                            bail!("A found folder can only be named inside an item of the Disk Rebuild.");
                        }
                        return Ok(None);
                    }
                }
            }
        };
        if let Some(head) = place.head {
            let Some(own) = item.head else {
                bail!("A folder you made holds no found folders to put things in.");
            };
            if self.top_of(head)? != self.top_of(own)? {
                bail!("The folder at cluster {head} is not inside that piece.");
            }
        }
        Ok(Some((item.id, place.head)))
    }

    fn next_position(&self, disk_rebuild: i64) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM disk_rebuild_items WHERE disk_rebuild = ?1",
            [disk_rebuild],
            |row| row.get(0),
        )?)
    }

    fn used_heads(&self, disk_rebuild: i64) -> Result<HashSet<u32>> {
        Ok(self
            .disk_rebuild_items(disk_rebuild)?
            .into_iter()
            .filter_map(|item| item.head)
            .collect())
    }

    /// Puts a piece on the Disk Rebuild, as a folder of its own or merged into the folder it lands
    /// in. The first thing placed becomes the root. A piece can be used once on each Disk Rebuild.
    pub fn disk_rebuild_place(
        &self,
        disk_rebuild: i64,
        head: u32,
        place: Place,
        merge: bool,
    ) -> Result<i64> {
        self.need_disk_rebuild(disk_rebuild)?;
        let top: Option<Option<u32>> = self
            .connection
            .query_row("SELECT parent FROM nodes WHERE head = ?1", [head], |row| {
                row.get(0)
            })
            .optional()?;
        // A folder from inside a piece may be placed too, as a layer of its own. Where its
        // piece is also on the Disk Rebuild, the same files at the same clusters are one copy.
        if top.is_none() {
            bail!("there is no folder at cluster {head}");
        }
        if self.used_heads(disk_rebuild)?.contains(&head) {
            bail!("That piece is already in this Disk Rebuild.");
        }
        let resolved = self.resolve_place(disk_rebuild, place)?;
        let (kind, parent_item, parent_head) = match resolved {
            // The first piece is the root, which nothing can be merged into yet.
            None => (ITEM_PIECE, None, None),
            Some((item, at)) => (if merge { ITEM_MERGE } else { ITEM_PIECE }, Some(item), at),
        };
        self.connection.execute(
            &format!("INSERT INTO disk_rebuild_items (id, disk_rebuild, kind, head, parent_item, parent_head, position, created_at_millis) \
             VALUES ({NEXT_ITEM_ID}, ?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
            params![disk_rebuild, kind, head, parent_item, parent_head, self.next_position(disk_rebuild)?, now_millis() as i64],
        )?;
        let id = self.connection.last_insert_rowid();
        self.note_high_water("item_high_water", id)?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(id)
    }

    /// Places one found file on its own, as a small layer: its file joins the folder it is
    /// placed in, and clashes with a file of the same name there like any other copy.
    pub fn disk_rebuild_place_file(
        &self,
        disk_rebuild: i64,
        entry: i64,
        place: Place,
    ) -> Result<i64> {
        self.need_disk_rebuild(disk_rebuild)?;
        let is_file: Option<bool> = self
            .connection
            .query_row(
                "SELECT COALESCE(o_is_dir, is_dir) = 0 FROM sets WHERE id = ?1",
                [entry],
                |row| row.get(0),
            )
            .optional()?;
        match is_file {
            None => bail!("there is no file {entry}"),
            Some(false) => bail!("That is a folder. Place a folder as a piece instead."),
            Some(true) => {}
        }
        let Some((item, at)) = self.resolve_place(disk_rebuild, place)? else {
            bail!("A file cannot be the top of the tree. Place a piece or make a folder first.");
        };
        self.connection.execute(
            &format!("INSERT INTO disk_rebuild_items (id, disk_rebuild, kind, entry, parent_item, parent_head, position, created_at_millis) \
             VALUES ({NEXT_ITEM_ID}, ?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
            params![disk_rebuild, ITEM_FILE, entry, item, at, self.next_position(disk_rebuild)?, now_millis() as i64],
        )?;
        let id = self.connection.last_insert_rowid();
        self.note_high_water("item_high_water", id)?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(id)
    }

    /// Makes up a folder the disk no longer has, so pieces can be put inside it.
    pub fn disk_rebuild_folder(&self, disk_rebuild: i64, name: &str, place: Place) -> Result<i64> {
        self.need_disk_rebuild(disk_rebuild)?;
        let name = clean_name(name)?;
        let resolved = self.resolve_place(disk_rebuild, place)?;
        let (parent_item, parent_head) = match resolved {
            None => (None, None),
            Some((item, at)) => (Some(item), at),
        };
        self.connection.execute(
            &format!("INSERT INTO disk_rebuild_items (id, disk_rebuild, kind, name, parent_item, parent_head, position, created_at_millis) \
             VALUES ({NEXT_ITEM_ID}, ?1, ?2, ?3, ?4, ?5, ?6, ?7)"),
            params![disk_rebuild, ITEM_FOLDER, name, parent_item, parent_head, self.next_position(disk_rebuild)?, now_millis() as i64],
        )?;
        let id = self.connection.last_insert_rowid();
        self.note_high_water("item_high_water", id)?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(id)
    }

    /// Moves an item, and everything in it, somewhere else on the same Disk Rebuild.
    pub fn disk_rebuild_move(
        &self,
        disk_rebuild: i64,
        item: i64,
        place: Place,
        merge: Option<bool>,
    ) -> Result<()> {
        let moving = self.disk_rebuild_item(disk_rebuild, item)?;
        if moving.parent_item.is_none() {
            bail!("The root cannot move. Put things inside it instead.");
        }
        let Some((target, at)) = self.resolve_place(disk_rebuild, place)? else {
            bail!("The Disk Rebuild has no root to move it into.");
        };
        let items = self.disk_rebuild_items(disk_rebuild)?;
        let parents: HashMap<i64, Option<i64>> = items
            .iter()
            .map(|found| (found.id, found.parent_item))
            .collect();
        let mut current = Some(target);
        for _ in 0..MAX_DEPTH {
            let Some(id) = current else { break };
            if id == item {
                bail!("An item cannot go inside itself.");
            }
            current = parents.get(&id).copied().flatten();
        }
        let kind = match (moving.kind.as_str(), merge) {
            (ITEM_FOLDER, _) => ITEM_FOLDER,
            (_, Some(true)) => ITEM_MERGE,
            (_, Some(false)) => ITEM_PIECE,
            (other, None) => {
                if other == ITEM_MERGE {
                    ITEM_MERGE
                } else {
                    ITEM_PIECE
                }
            }
        };
        self.connection.execute(
            "UPDATE disk_rebuild_items SET parent_item = ?2, parent_head = ?3, kind = ?4 WHERE id = ?1",
            params![item, target, at, kind],
        )?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(())
    }

    /// Swaps the piece behind an item for another unused piece, keeping where it sits and
    /// everything placed in it. For trading a piece for a similar one with better data.
    pub fn disk_rebuild_swap(&self, disk_rebuild: i64, item: i64, head: u32) -> Result<()> {
        let found = self.disk_rebuild_item(disk_rebuild, item)?;
        if found.kind == ITEM_FOLDER {
            bail!("A folder you made has no piece to swap.");
        }
        let top: Option<Option<u32>> = self
            .connection
            .query_row("SELECT parent FROM nodes WHERE head = ?1", [head], |row| {
                row.get(0)
            })
            .optional()?;
        match top {
            None => bail!("there is no folder at cluster {head}"),
            Some(Some(_)) => {
                bail!("That folder is inside another piece. Place the piece it belongs to instead.")
            }
            Some(None) => {}
        }
        if self.used_heads(disk_rebuild)?.contains(&head) {
            bail!("That piece is already in this Disk Rebuild.");
        }
        // Things placed in a found folder of the old piece have nowhere to be in the new one.
        let stranded: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM disk_rebuild_items WHERE disk_rebuild = ?1 AND parent_item = ?2 AND parent_head IS NOT NULL",
            params![disk_rebuild, item],
            |row| row.get(0),
        )?;
        if stranded > 0 {
            bail!("Something is placed inside a folder of this piece. Move it out first.");
        }
        self.connection.execute(
            "UPDATE disk_rebuild_items SET head = ?2 WHERE id = ?1",
            params![item, head],
        )?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(())
    }

    /// Names an item in the rebuilt tree. For a piece, no name puts back the folder's own.
    pub fn disk_rebuild_rename_item(
        &self,
        disk_rebuild: i64,
        item: i64,
        name: Option<&str>,
    ) -> Result<()> {
        let found = self.disk_rebuild_item(disk_rebuild, item)?;
        let name = match name {
            Some(name) => Some(clean_name(name)?),
            None if found.kind == ITEM_FOLDER => bail!("A folder you made needs a name."),
            None => None,
        };
        self.connection.execute(
            "UPDATE disk_rebuild_items SET name = ?2 WHERE id = ?1",
            params![item, name],
        )?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(())
    }

    /// Takes an item off the Disk Rebuild with everything inside it, so its pieces are unused again.
    pub fn disk_rebuild_remove(&self, disk_rebuild: i64, item: i64) -> Result<u64> {
        self.disk_rebuild_item(disk_rebuild, item)?;
        let items = self.disk_rebuild_items(disk_rebuild)?;
        let mut doomed = vec![item];
        let mut index = 0;
        while index < doomed.len() {
            let parent = doomed[index];
            doomed.extend(
                items
                    .iter()
                    .filter(|found| found.parent_item == Some(parent))
                    .map(|found| found.id),
            );
            index += 1;
        }
        self.atomically(|| {
            for id in &doomed {
                self.connection
                    .execute("DELETE FROM disk_rebuild_items WHERE id = ?1", [id])?;
            }
            Ok(())
        })?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(doomed.len() as u64)
    }

    /// Top-level found folders not yet on this Disk Rebuild, largest first.
    pub fn disk_rebuild_unused(
        &self,
        disk_rebuild: i64,
        offset: u64,
        limit: u64,
    ) -> Result<UnusedPieces> {
        self.need_disk_rebuild(disk_rebuild)?;
        let unused = "FROM nodes WHERE parent IS NULL AND head NOT IN \
             (SELECT head FROM disk_rebuild_items WHERE disk_rebuild = ?1 AND head IS NOT NULL)";
        let (total, total_files, total_bytes): (i64, i64, i64) = self.connection.query_row(
            &format!("SELECT COUNT(*), COALESCE(SUM(files), 0), COALESCE(SUM(bytes), 0) {unused}"),
            [disk_rebuild],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let sql = format!(
            "SELECT {NODE_COLUMNS} {unused} ORDER BY kind = 'root' DESC, bytes DESC, head LIMIT ?2 OFFSET ?3"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement
            .query_map(params![disk_rebuild, limit as i64, offset as i64], node_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(UnusedPieces {
            rows,
            total: total as u64,
            total_files: total_files as u64,
            total_bytes: total_bytes as u64,
        })
    }

    /// Puts every unused piece, or only those with at most `max_files` files, into one made-up
    /// folder at the top of the Disk Rebuild, so the small ones need not be placed one by one.
    pub fn disk_rebuild_leftovers(
        &self,
        disk_rebuild: i64,
        name: &str,
        max_files: Option<u64>,
    ) -> Result<(i64, u64)> {
        let Some(root) = self.disk_rebuild_root(disk_rebuild)? else {
            bail!("Place a first piece before putting the rest away.");
        };
        let unused = self.disk_rebuild_unused(disk_rebuild, 0, u64::MAX >> 1)?;
        let chosen: Vec<u32> = unused
            .rows
            .iter()
            .filter(|row| max_files.is_none_or(|most| row.files <= most))
            .filter_map(|row| row.head)
            .collect();
        if chosen.is_empty() {
            bail!("There are no unused pieces left to put away.");
        }
        let folder = self.disk_rebuild_folder(
            disk_rebuild,
            name,
            Place {
                item: Some(root.id),
                head: None,
            },
        )?;
        let first = self.next_position(disk_rebuild)?;
        let now = now_millis() as i64;
        self.atomically(|| {
            for (position, head) in (first..).zip(chosen.iter()) {
                self.connection.execute(
                    &format!("INSERT INTO disk_rebuild_items (id, disk_rebuild, kind, head, parent_item, position, created_at_millis) \
                     VALUES ({NEXT_ITEM_ID}, ?1, ?2, ?3, ?4, ?5, ?6)"),
                    params![disk_rebuild, ITEM_PIECE, head, folder, position, now],
                )?;
                self.note_high_water("item_high_water", self.connection.last_insert_rowid())?;
            }
            Ok(())
        })?;
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok((folder, chosen.len() as u64))
    }

    /// Picks which copy of a clashing file the rebuilt tree shows. None puts back the best one.
    pub fn disk_rebuild_choose(
        &self,
        disk_rebuild: i64,
        path: &[String],
        entry: Option<i64>,
    ) -> Result<()> {
        self.need_disk_rebuild(disk_rebuild)?;
        let Some((name, folder)) = path.split_last() else {
            bail!("A choice needs the path of a file.");
        };
        let listing = self.disk_rebuild_list(disk_rebuild, folder, 0, u64::MAX >> 1)?;
        let file = listing
            .files
            .iter()
            .find(|file| key(&file.name) == key(name))
            .with_context(|| format!("there is no file called {name} there"))?;
        let path_key = choice_key(path);
        match entry {
            Some(entry) => {
                if !file.copies.iter().any(|copy| copy.id == Some(entry)) {
                    bail!("That entry is not a copy of this file.");
                }
                self.connection.execute(
                    "INSERT OR REPLACE INTO disk_rebuild_choices (disk_rebuild, path, entry) VALUES (?1, ?2, ?3)",
                    params![disk_rebuild, path_key, entry],
                )?;
            }
            None => {
                self.connection.execute(
                    "DELETE FROM disk_rebuild_choices WHERE disk_rebuild = ?1 AND path = ?2",
                    params![disk_rebuild, path_key],
                )?;
            }
        }
        self.touch_disk_rebuild(disk_rebuild)?;
        Ok(())
    }

    /// Lists one folder of the rebuilt tree, found by the names on the way down from the root.
    /// The files are paged; the folders are always all there.
    pub fn disk_rebuild_list(
        &self,
        disk_rebuild: i64,
        path: &[String],
        offset: u64,
        limit: u64,
    ) -> Result<Listing> {
        let items = self.disk_rebuild_items(disk_rebuild)?;
        let mut listing = Listing {
            path: path.to_vec(),
            folders: Vec::new(),
            files: Vec::new(),
            total_files: 0,
            clashes: 0,
            clashes_picked: 0,
            clashes_need_you: 0,
        };
        let Some(root) = items.iter().find(|item| item.parent_item.is_none()) else {
            if !path.is_empty() {
                bail!("This Disk Rebuild is empty.");
            }
            return Ok(listing);
        };
        let tree = Tree::new(self, disk_rebuild, &items)?;
        let mut sources = vec![Source::Item(root.id)];
        for name in path {
            let (folders, _) = tree.contents(&sources)?;
            let wanted = key(name);
            sources = folders
                .into_iter()
                .find(|(folder_key, _)| *folder_key == wanted)
                .map(|(_, group)| group.sources)
                .with_context(|| format!("there is no folder called {name} there"))?;
        }
        let (folders, files) = tree.contents(&sources)?;
        listing.folders = folders
            .into_values()
            .map(|group| tree.describe(group))
            .collect::<Result<Vec<_>>>()?;
        listing
            .folders
            .sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.name.cmp(&b.name)));
        let choices = tree.choices()?;
        let hashes = tree.hashes(&files)?;
        let mut rebuilt = rebuild_files(
            files,
            path,
            &choices,
            &tree.file_layers.borrow(),
            &hashes,
            &tree.agent,
        );
        rebuilt.sort_by_key(|file| key(&file.name));
        listing.total_files = rebuilt.len() as u64;
        listing.clashes = rebuilt.iter().filter(|file| file.copies.len() > 1).count() as u64;
        listing.clashes_need_you = rebuilt.iter().filter(|file| file.needs_you).count() as u64;
        listing.clashes_picked = listing.clashes - listing.clashes_need_you;
        listing.files = rebuilt
            .into_iter()
            .skip(offset as usize)
            .take(limit as usize)
            .collect();
        Ok(listing)
    }
}

/// One file of the whole rebuilt tree, with the folders it sits in and the copy that fills it.
#[derive(Clone, Debug)]
pub struct DiskRebuildFile {
    /// Folder names from the top of the tree down, with lost names as `Lost folder <cluster>`.
    pub folders: Vec<String>,
    pub row: Row,
}

/// Turns the copies found under each name into files, picking the person's choice or the best.
/// Copies at the same clusters, or tested and found to hold the same bytes, are one copy.
fn rebuild_files(
    files: BTreeMap<String, Vec<Row>>,
    path: &[String],
    choices: &HashMap<String, i64>,
    layers: &HashMap<i64, i64>,
    hashes: &HashMap<i64, String>,
    agent: &AgentView,
) -> Vec<RebuiltFile> {
    files
        .into_values()
        .map(|mut copies| {
            copies.sort_by(|a, b| pick_order(a, b, layers));
            let mut seen = HashSet::new();
            let mut same_bytes = HashSet::new();
            copies.retain(|copy| {
                let hash = copy.id.and_then(|id| hashes.get(&id));
                seen.insert((copy.first_cluster, copy.length))
                    && hash.is_none_or(|hash| same_bytes.insert(hash.clone()))
            });
            let name = copies[0].name.clone();
            let mut chosen_path = path.to_vec();
            chosen_path.push(name.clone());
            let picked = choices.get(&choice_key(&chosen_path)).copied();
            let chosen = picked
                .and_then(|entry| copies.iter().find(|copy| copy.id == Some(entry)))
                .cloned();
            let by_person = chosen.is_some();
            let chosen = chosen.unwrap_or_else(|| copies[0].clone());
            let (decided_by, needs_you) = match copies.get(1) {
                None => (None, false),
                Some(_) if by_person => (Some("person"), false),
                Some(second) => {
                    let first = &copies[0];
                    if quality(first) != quality(second) || first.confidence != second.confidence {
                        (Some("quality"), false)
                    } else if first.modified != second.modified {
                        (Some("date"), true)
                    } else {
                        (Some("layer"), true)
                    }
                }
            };
            let made_by_agent = (by_person && agent.choices.contains(&choice_key(&chosen_path)))
                || chosen
                    .id
                    .and_then(|id| layers.get(&id))
                    .is_some_and(|layer| agent.file_layers.contains(layer));
            // The name is the chosen copy's, since copies can differ in case.
            RebuiltFile {
                bad_sector_bytes: 0,
                made_by_agent,
                name: chosen.name.clone(),
                by_person,
                decided_by: decided_by.map(str::to_string),
                needs_you,
                chosen,
                copies,
            }
        })
        .collect()
}

/// A file where copies clash, anywhere in the rebuilt tree.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Clash {
    /// The rebuilt folders it sits in, as path names to pass back, then the file's name.
    pub path: Vec<String>,
    /// The same folders as shown, with lost names as `Lost folder <cluster>`.
    pub shown: Vec<String>,
    pub file: RebuiltFile,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClashPage {
    pub clashes: Vec<Clash>,
    pub total: u64,
    pub need_you: u64,
}

impl MapDb {
    /// Visits every file of the rebuilt tree with its folders' path names and shown names.
    pub(crate) fn walk_disk_rebuild(
        &self,
        disk_rebuild: i64,
        mut visit: impl FnMut(&[String], &[String], RebuiltFile),
    ) -> Result<()> {
        self.need_disk_rebuild(disk_rebuild)?;
        let items = self.disk_rebuild_items(disk_rebuild)?;
        let Some(root) = items.iter().find(|item| item.parent_item.is_none()) else {
            bail!("This Disk Rebuild is empty.");
        };
        let tree = Tree::new(self, disk_rebuild, &items)?;
        let choices = tree.choices()?;
        let mut visited: HashSet<Vec<Source>> = HashSet::new();
        // Each folder: what it is made of, its path for choices, and its folder names on disk.
        let mut pending = vec![(vec![Source::Item(root.id)], Vec::new(), Vec::new())];
        while let Some((sources, path, folders)) = pending.pop() {
            if path.len() > MAX_DEPTH || !visited.insert(sources.clone()) {
                continue;
            }
            let (subfolders, files) = tree.contents(&sources)?;
            let hashes = tree.hashes(&files)?;
            let layers = tree.file_layers.borrow().clone();
            for file in rebuild_files(files, &path, &choices, &layers, &hashes, &tree.agent) {
                visit(&path, &folders, file);
            }
            for group in subfolders.into_values() {
                let shown = match (&group.name, group.lost_head) {
                    (Some(name), _) => name.clone(),
                    (None, Some(head)) => format!("Lost folder {head}"),
                    (None, None) => group.path_name.clone(),
                };
                let mut next_path = path.clone();
                next_path.push(group.path_name.clone());
                let mut next_folders = folders.clone();
                next_folders.push(shown);
                pending.push((group.sources, next_path, next_folders));
            }
        }
        Ok(())
    }

    /// Every file of the rebuilt tree, each as the copy that would fill it, for copying out.
    pub fn disk_rebuild_files(&self, disk_rebuild: i64) -> Result<Vec<DiskRebuildFile>> {
        let mut out = Vec::new();
        self.walk_disk_rebuild(disk_rebuild, |_, folders, file| {
            out.push(DiskRebuildFile {
                folders: folders.to_vec(),
                row: file.chosen,
            });
        })?;
        Ok(out)
    }

    /// Every clash in the rebuilt tree, those that need the person first.
    pub fn disk_rebuild_clashes(
        &self,
        disk_rebuild: i64,
        need_you_only: bool,
        offset: u64,
        limit: u64,
    ) -> Result<ClashPage> {
        let mut found = Vec::new();
        self.walk_disk_rebuild(disk_rebuild, |path, folders, file| {
            if file.copies.len() > 1 && (file.needs_you || !need_you_only) {
                let mut full = path.to_vec();
                full.push(file.name.clone());
                found.push(Clash {
                    path: full,
                    shown: folders.to_vec(),
                    file,
                });
            }
        })?;
        found.sort_by(|a, b| {
            b.file
                .needs_you
                .cmp(&a.file.needs_you)
                .then(a.path.cmp(&b.path))
        });
        let need_you = found.iter().filter(|clash| clash.file.needs_you).count() as u64;
        let total = found.len() as u64;
        Ok(ClashPage {
            clashes: found
                .into_iter()
                .skip(offset as usize)
                .take(limit as usize)
                .collect(),
            total,
            need_you,
        })
    }
}

/// The layer a copy of a file came from: the item of the Disk Rebuild that brought it. A later
/// item is an upper layer.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Layer {
    pub item: i64,
    pub kind: String,
    pub name: Option<String>,
    pub head: Option<u32>,
    pub made_by_agent: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolvedCopy {
    pub row: Row,
    pub layer: Option<Layer>,
    pub chosen: bool,
}

/// What one path of the rebuilt tree is, and for a file which copy fills it and why.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Resolved {
    pub path: Vec<String>,
    /// `file` or `folder`.
    pub kind: String,
    pub name: String,
    /// For a file: the copy that fills it.
    pub chosen: Option<Row>,
    /// For a file: every distinct copy, best first, with the layer it came from.
    pub copies: Vec<ResolvedCopy>,
    pub by_person: bool,
    pub decided_by: Option<String>,
    /// In words, what decided which copy won.
    pub why: Option<String>,
    pub needs_you: bool,
    pub made_by_agent: bool,
    /// For a folder: what it is made of.
    pub folder: Option<RebuiltFolder>,
}

/// In words, why the first of two copies of a file won.
fn explain_pick(first: &Row, second: &Row, decided_by: &str) -> String {
    match decided_by {
        "person" => "The copy was chosen on purpose, not picked by the app.".to_string(),
        "quality" => {
            let mut reasons = Vec::new();
            if verdict_rank(&first.verdict) != verdict_rank(&second.verdict) {
                reasons.push(format!(
                    "its verdict is better ({} against {})",
                    first.verdict, second.verdict
                ));
            }
            if whole(first) != whole(second) {
                reasons.push("all its clusters are there".to_string());
            }
            if first.checksum_ok != second.checksum_ok {
                reasons.push("its entry checksum is valid".to_string());
            }
            if (first.valid_length != first.length) != (second.valid_length != second.length) {
                reasons.push("it has the size its entry says".to_string());
            }
            if reasons.is_empty() {
                reasons.push("its confidence is higher".to_string());
            }
            format!("It measured better: {}.", reasons.join(", "))
        }
        "date" => "The copies measure the same, so the newer one won. Look at both if it matters."
            .to_string(),
        _ => "The copies measure the same and have the same date, so the copy from the upper layer (the later item) won. Look at both if it matters."
            .to_string(),
    }
}

impl MapDb {
    /// A copy of a Disk Rebuild under a new name: the same items in the same places, and the same
    /// chosen copies. The two are independent afterwards.
    pub fn disk_rebuild_duplicate(&self, disk_rebuild: i64, name: &str) -> Result<i64> {
        let name = clean_name(name)?;
        let items = self.disk_rebuild_items(disk_rebuild)?;
        let choices: Vec<(String, i64)> = {
            let mut statement = self
                .connection
                .prepare("SELECT path, entry FROM disk_rebuild_choices WHERE disk_rebuild = ?1")?;
            let rows = statement
                .query_map([disk_rebuild], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            rows
        };
        self.atomically(|| {
            let copy = self.disk_rebuild_create(&name)?;
            let now = now_millis() as i64;
            let mut renumbered: HashMap<i64, i64> = HashMap::new();
            for (position, item) in items.iter().enumerate() {
                self.connection.execute(
                    &format!("INSERT INTO disk_rebuild_items (id, disk_rebuild, kind, head, name, parent_head, entry, position, created_at_millis) \
                     VALUES ({NEXT_ITEM_ID}, ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"),
                    params![copy, item.kind, item.head, item.name, item.parent_head, item.entry, position as i64, now],
                )?;
                let new_id = self.connection.last_insert_rowid();
                self.note_high_water("item_high_water", new_id)?;
                renumbered.insert(item.id, new_id);
            }
            for item in &items {
                if let Some(parent) = item.parent_item {
                    self.connection.execute(
                        "UPDATE disk_rebuild_items SET parent_item = ?2 WHERE id = ?1",
                        params![renumbered[&item.id], renumbered.get(&parent)],
                    )?;
                }
            }
            for (path, entry) in &choices {
                self.connection.execute(
                    "INSERT INTO disk_rebuild_choices (disk_rebuild, path, entry) VALUES (?1, ?2, ?3)",
                    params![copy, path, entry],
                )?;
            }
            Ok(copy)
        })
    }

    /// What one path of the rebuilt tree resolves to: for a file, the copy that fills it, every
    /// copy with the layer it came from, which won and why; for a folder, what it is made of.
    pub fn disk_rebuild_resolve(&self, disk_rebuild: i64, path: &[String]) -> Result<Resolved> {
        self.need_disk_rebuild(disk_rebuild)?;
        let items = self.disk_rebuild_items(disk_rebuild)?;
        let Some(root) = items.iter().find(|item| item.parent_item.is_none()) else {
            bail!("This Disk Rebuild is empty.");
        };
        let tree = Tree::new(self, disk_rebuild, &items)?;
        let Some((name, parents)) = path.split_last() else {
            let (shown, lost) = tree.item_name(root)?;
            let group = Group {
                path_name: String::new(),
                name: shown.clone(),
                lost_head: lost,
                sources: vec![Source::Item(root.id)],
            };
            return Ok(Resolved {
                path: Vec::new(),
                kind: "folder".into(),
                name: shown.unwrap_or_default(),
                chosen: None,
                copies: Vec::new(),
                by_person: false,
                decided_by: None,
                why: None,
                needs_you: false,
                made_by_agent: root.made_by_agent,
                folder: Some(tree.describe(group)?),
            });
        };
        let sources = tree.sources_at(root.id, parents)?;
        let (mut folders, mut files) = tree.contents(&sources)?;
        let wanted = key(name);
        if let Some(copies) = files.remove(&wanted) {
            let choices = tree.choices()?;
            let one = BTreeMap::from([(wanted, copies)]);
            let hashes = tree.hashes(&one)?;
            let layers = tree.file_layers.borrow().clone();
            let file = rebuild_files(one, parents, &choices, &layers, &hashes, &tree.agent)
                .into_iter()
                .next()
                .context("that file has no copy")?;
            let chosen_id = file.chosen.id;
            let describe_layer = |row: &Row| {
                let item = row
                    .id
                    .and_then(|id| layers.get(&id))
                    .and_then(|item| tree.items.get(item))?;
                Some(Layer {
                    item: item.id,
                    kind: item.kind.clone(),
                    name: item.name.clone(),
                    head: item.head,
                    made_by_agent: item.made_by_agent,
                })
            };
            let why = match (&file.decided_by, file.copies.get(1)) {
                (Some(decided), Some(second)) => {
                    Some(explain_pick(&file.copies[0], second, decided))
                }
                _ => None,
            };
            let mut full = parents.to_vec();
            full.push(file.name.clone());
            return Ok(Resolved {
                path: full,
                kind: "file".into(),
                name: file.name.clone(),
                chosen: Some(file.chosen.clone()),
                copies: file
                    .copies
                    .iter()
                    .map(|copy| ResolvedCopy {
                        row: copy.clone(),
                        layer: describe_layer(copy),
                        chosen: copy.id == chosen_id,
                    })
                    .collect(),
                by_person: file.by_person,
                decided_by: file.decided_by.clone(),
                why,
                needs_you: file.needs_you,
                made_by_agent: file.made_by_agent,
                folder: None,
            });
        }
        let group = folders
            .remove(&wanted)
            .with_context(|| format!("there is nothing called {name} there"))?;
        let folder = tree.describe(group)?;
        let mut full = parents.to_vec();
        full.push(folder.path_name.clone());
        Ok(Resolved {
            path: full,
            kind: "folder".into(),
            name: folder
                .name
                .clone()
                .unwrap_or_else(|| folder.path_name.clone()),
            chosen: None,
            copies: Vec::new(),
            by_person: false,
            decided_by: None,
            why: None,
            needs_you: false,
            made_by_agent: folder.made_by_agent,
            folder: Some(folder),
        })
    }

    /// Picks the copy of a file that came from one layer (an item of the Disk Rebuild), so a lower
    /// layer can win a path that an upper one would take.
    pub fn disk_rebuild_choose_layer(
        &self,
        disk_rebuild: i64,
        path: &[String],
        layer: i64,
    ) -> Result<()> {
        let resolved = self.disk_rebuild_resolve(disk_rebuild, path)?;
        if resolved.kind != "file" {
            bail!("A layer is chosen for a file. That path is a folder.");
        }
        let copy = resolved
            .copies
            .iter()
            .find(|copy| copy.layer.as_ref().is_some_and(|found| found.item == layer))
            .with_context(|| {
                format!("no copy of that file comes from layer {layer}, or it is the same copy as a better one")
            })?;
        self.disk_rebuild_choose(disk_rebuild, path, copy.row.id)
    }
}

/// A placement someone other than the person proposed, for the person to accept or dismiss.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Suggestion {
    pub id: i64,
    pub head: u32,
    pub place_item: Option<i64>,
    pub place_head: Option<u32>,
    pub merge: bool,
    pub note: String,
    pub author: String,
    pub created_at_millis: u64,
}

impl MapDb {
    /// Records where a piece might go, without placing it. Claude suggests; the person decides.
    pub fn disk_rebuild_suggest(
        &self,
        disk_rebuild: i64,
        head: u32,
        place: Place,
        merge: bool,
        note: &str,
        author: &str,
    ) -> Result<i64> {
        self.need_disk_rebuild(disk_rebuild)?;
        let top: Option<Option<u32>> = self
            .connection
            .query_row("SELECT parent FROM nodes WHERE head = ?1", [head], |row| {
                row.get(0)
            })
            .optional()?;
        match top {
            None => bail!("there is no folder at cluster {head}"),
            Some(Some(_)) => {
                bail!("That folder is inside another piece. Place the piece it belongs to instead.")
            }
            Some(None) => {}
        }
        if self.used_heads(disk_rebuild)?.contains(&head) {
            bail!("That piece is already in this Disk Rebuild.");
        }
        // Checked now, so a suggestion that names a place that does not exist is refused at once.
        let resolved = self.resolve_place(disk_rebuild, place)?;
        let (item, at) = match resolved {
            None => (None, None),
            Some((item, at)) => (Some(item), at),
        };
        if note.trim().is_empty() {
            bail!("A suggestion needs a note saying why.");
        }
        self.connection.execute(
            "INSERT INTO disk_rebuild_suggestions (disk_rebuild, head, place_item, place_head, merge, note, author, state, created_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8)",
            params![disk_rebuild, head, item, at, merge, note.trim(), author, now_millis() as i64],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Suggestions still waiting for the person, oldest first.
    pub fn disk_rebuild_suggestions(&self, disk_rebuild: i64) -> Result<Vec<Suggestion>> {
        self.need_disk_rebuild(disk_rebuild)?;
        // One whose piece has since been placed, or whose place is gone, has nothing left to ask.
        let used = self.used_heads(disk_rebuild)?;
        let items: HashSet<i64> = self
            .disk_rebuild_items(disk_rebuild)?
            .iter()
            .map(|item| item.id)
            .collect();
        let mut statement = self.connection.prepare_cached(
            "SELECT id, head, place_item, place_head, merge, note, author, created_at_millis FROM disk_rebuild_suggestions \
             WHERE disk_rebuild = ?1 AND state = 'open' ORDER BY id",
        )?;
        let rows = statement
            .query_map([disk_rebuild], |row| {
                Ok(Suggestion {
                    id: row.get(0)?,
                    head: row.get(1)?,
                    place_item: row.get(2)?,
                    place_head: row.get(3)?,
                    merge: row.get(4)?,
                    note: row.get(5)?,
                    author: row.get(6)?,
                    created_at_millis: row.get::<_, i64>(7)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows
            .into_iter()
            .filter(|row| {
                !used.contains(&row.head) && row.place_item.is_none_or(|item| items.contains(&item))
            })
            .collect())
    }

    /// Places a suggested piece where it was suggested, and closes the suggestion.
    pub fn disk_rebuild_accept(&self, disk_rebuild: i64, suggestion: i64) -> Result<i64> {
        let found = self
            .disk_rebuild_suggestions(disk_rebuild)?
            .into_iter()
            .find(|row| row.id == suggestion)
            .with_context(|| format!("there is no open suggestion {suggestion}"))?;
        let place = Place {
            item: found.place_item,
            head: found.place_head,
        };
        let item = self.disk_rebuild_place(disk_rebuild, found.head, place, found.merge)?;
        self.connection.execute(
            "UPDATE disk_rebuild_suggestions SET state = 'accepted' WHERE id = ?1",
            [suggestion],
        )?;
        Ok(item)
    }

    pub fn disk_rebuild_dismiss(&self, disk_rebuild: i64, suggestion: i64) -> Result<()> {
        self.need_disk_rebuild(disk_rebuild)?;
        let changed = self.connection.execute(
            "UPDATE disk_rebuild_suggestions SET state = 'dismissed' WHERE id = ?1 AND disk_rebuild = ?2 AND state = 'open'",
            params![suggestion, disk_rebuild],
        )?;
        if changed == 0 {
            bail!("there is no open suggestion {suggestion}");
        }
        Ok(())
    }
}

fn choice_key(path: &[String]) -> String {
    path.iter()
        .map(|part| key(part))
        .collect::<Vec<_>>()
        .join("/")
}

/// The folders that become one in the rebuilt tree, by merging or by sharing a name.
struct Group {
    path_name: String,
    name: Option<String>,
    lost_head: Option<u32>,
    sources: Vec<Source>,
}

/// One Disk Rebuild, read once, for working out its folders.
struct Tree<'a> {
    database: &'a MapDb,
    disk_rebuild: i64,
    items: HashMap<i64, DiskRebuildItem>,
    /// Items in each place, in the order placed.
    inside: HashMap<Source, Vec<i64>>,
    /// The layer each found folder's files came from: the Disk Rebuild item that brought it, whose
    /// id grows with each drop, so a later drop is an upper layer.
    folder_layers: RefCell<HashMap<u32, i64>>,
    /// The same, for each file seen, by entry id.
    file_layers: RefCell<HashMap<i64, i64>>,
    /// What an agent made, so the tree can say so.
    agent: AgentView,
}

/// The parts of a Disk Rebuild an agent made that the rebuilt tree shows: single files it placed,
/// and the copies it chose.
#[derive(Default)]
pub(crate) struct AgentView {
    file_layers: HashSet<i64>,
    choices: HashSet<String>,
}

impl<'a> Tree<'a> {
    fn new(database: &'a MapDb, disk_rebuild: i64, items: &[DiskRebuildItem]) -> Result<Self> {
        let mut inside: HashMap<Source, Vec<i64>> = HashMap::new();
        for item in items {
            let place = match (item.parent_item, item.parent_head) {
                (_, Some(head)) => Source::Found(head),
                (Some(parent), None) => Source::Item(parent),
                (None, None) => continue,
            };
            inside.entry(place).or_default().push(item.id);
        }
        let marks = database.agent_marks(disk_rebuild)?;
        let agent = AgentView {
            file_layers: items
                .iter()
                .filter(|item| item.kind == ITEM_FILE && marks.items.contains(&item.id))
                .map(|item| item.id)
                .collect(),
            choices: marks.choices,
        };
        Ok(Self {
            agent,
            database,
            disk_rebuild,
            items: items.iter().map(|item| (item.id, item.clone())).collect(),
            inside,
            folder_layers: RefCell::new(HashMap::new()),
            file_layers: RefCell::new(HashMap::new()),
        })
    }

    /// What the folder at the end of a path is made of, found by the names down from the root.
    fn sources_at(&self, root: i64, path: &[String]) -> Result<Vec<Source>> {
        let mut sources = vec![Source::Item(root)];
        for name in path {
            let (folders, _) = self.contents(&sources)?;
            let wanted = key(name);
            sources = folders
                .into_iter()
                .find(|(folder_key, _)| *folder_key == wanted)
                .map(|(_, group)| group.sources)
                .with_context(|| format!("there is no folder called {name} there"))?;
        }
        Ok(sources)
    }

    fn choices(&self) -> Result<HashMap<String, i64>> {
        let mut statement = self.database.connection.prepare_cached(
            "SELECT path, entry FROM disk_rebuild_choices WHERE disk_rebuild = ?1",
        )?;
        let rows = statement
            .query_map([self.disk_rebuild], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<String, i64>>>()?;
        Ok(rows)
    }

    /// Adds what a folder's contents come from: a piece's own folder, and every merge into it.
    fn expand(&self, sources: &[Source]) -> Vec<Source> {
        let mut out = Vec::new();
        let mut queue: Vec<Source> = sources.to_vec();
        let mut seen = HashSet::new();
        while let Some(source) = queue.pop() {
            if !seen.insert(source) || out.len() > MAX_DEPTH * 64 {
                continue;
            }
            out.push(source);
            if let Source::Item(id) = source {
                if let Some(head) = self.items.get(&id).and_then(|item| item.head) {
                    self.folder_layers.borrow_mut().insert(head, id);
                    queue.push(Source::Found(head));
                }
            }
            for id in self.inside.get(&source).into_iter().flatten() {
                if self
                    .items
                    .get(id)
                    .is_some_and(|item| item.kind == ITEM_MERGE)
                {
                    queue.push(Source::Item(*id));
                }
            }
        }
        out.sort();
        out
    }

    /// The folders and files of one rebuilt folder, grouped by name.
    #[allow(clippy::type_complexity)]
    fn contents(
        &self,
        sources: &[Source],
    ) -> Result<(BTreeMap<String, Group>, BTreeMap<String, Vec<Row>>)> {
        let mut folders: BTreeMap<String, Group> = BTreeMap::new();
        let mut files: BTreeMap<String, Vec<Row>> = BTreeMap::new();
        let mut add_folder =
            |folder_key: String, name: Option<String>, lost_head: Option<u32>, source: Source| {
                let group = folders.entry(folder_key.clone()).or_insert_with(|| Group {
                    path_name: name.clone().unwrap_or(folder_key),
                    name: name.clone(),
                    lost_head,
                    sources: Vec::new(),
                });
                if group.name.is_none() {
                    group.name = name;
                }
                group.sources.push(source);
            };
        for source in self.expand(sources) {
            for id in self.inside.get(&source).into_iter().flatten() {
                let item = &self.items[id];
                if item.kind == ITEM_MERGE {
                    continue;
                }
                if item.kind == ITEM_FILE {
                    if let Some(entry) = item.entry {
                        let sql = format!("SELECT {FILE_COLUMNS} FROM sets WHERE id = ?1");
                        let found = self
                            .database
                            .connection
                            .query_row(&sql, [entry], file_row)
                            .optional()?;
                        if let Some(row) = found {
                            self.file_layers.borrow_mut().insert(entry, item.id);
                            files.entry(key(&row.name)).or_default().push(row);
                        }
                    }
                    continue;
                }
                let (name, lost) = self.item_name(item)?;
                let folder_key = match (&name, lost) {
                    (Some(name), _) => key(name),
                    (None, Some(head)) => lost_key(head),
                    (None, None) => format!("item:{}", item.id),
                };
                add_folder(folder_key, name, lost, Source::Item(item.id));
            }
            let Source::Found(head) = source else {
                continue;
            };
            let layer = self.folder_layers.borrow().get(&head).copied().unwrap_or(0);
            let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE parent = ?1");
            let mut statement = self.database.connection.prepare_cached(&sql)?;
            let subfolders = statement
                .query_map([head], node_row)?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for folder in subfolders {
                let Some(child) = folder.head else { continue };
                self.folder_layers
                    .borrow_mut()
                    .entry(child)
                    .or_insert(layer);
                if folder.name.is_empty() {
                    add_folder(lost_key(child), None, Some(child), Source::Found(child));
                } else {
                    add_folder(
                        key(&folder.name),
                        Some(folder.name.clone()),
                        None,
                        Source::Found(child),
                    );
                }
            }
            let sql = format!(
                "SELECT {FILE_COLUMNS} FROM sets WHERE node = ?1 AND COALESCE(o_is_dir, is_dir) = 0 \
                 AND verdict != 'rename-residue'"
            );
            let mut statement = self.database.connection.prepare_cached(&sql)?;
            for row in statement.query_map([head], file_row)? {
                let row = row?;
                if let Some(id) = row.id {
                    self.file_layers.borrow_mut().insert(id, layer);
                }
                files.entry(key(&row.name)).or_default().push(row);
            }
        }
        for group in folders.values_mut() {
            group.sources.sort();
            group.sources.dedup();
        }
        Ok((folders, files))
    }

    /// The content hash of each copy that has been tested, to tell identical copies apart
    /// from ones that only share a name.
    fn hashes(&self, files: &BTreeMap<String, Vec<Row>>) -> Result<HashMap<i64, String>> {
        let ids: Vec<String> = files
            .values()
            .filter(|copies| copies.len() > 1)
            .flatten()
            .filter_map(|row| row.id)
            .map(|id| id.to_string())
            .collect();
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let mut statement = self.database.connection.prepare(&format!(
            "SELECT id, sha256 FROM file_tests WHERE sha256 IS NOT NULL AND id IN ({})",
            ids.join(",")
        ))?;
        let found = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<HashMap<i64, String>>>()?;
        Ok(found)
    }

    /// The name an item has in the rebuilt tree, or the found folder whose name was lost.
    fn item_name(&self, item: &DiskRebuildItem) -> Result<(Option<String>, Option<u32>)> {
        if let Some(name) = &item.name {
            return Ok((Some(name.clone()), None));
        }
        let Some(head) = item.head else {
            return Ok((None, None));
        };
        let name: Option<String> = self
            .database
            .connection
            .query_row("SELECT name FROM nodes WHERE head = ?1", [head], |row| {
                row.get(0)
            })
            .optional()?
            .flatten()
            .filter(|name: &String| !name.is_empty());
        Ok(match name {
            Some(name) => (Some(name), None),
            None => (None, Some(head)),
        })
    }

    /// Sizes and origin for a folder, from every found folder and item it is made of.
    fn describe(&self, group: Group) -> Result<RebuiltFolder> {
        let mut folder = RebuiltFolder {
            path_name: group.path_name,
            name: group.name,
            lost_head: if group.sources.len() == 1 {
                group.lost_head
            } else {
                None
            },
            origin: String::new(),
            merged: false,
            items: Vec::new(),
            heads: Vec::new(),
            files: 0,
            bytes: 0,
            intact_files: 0,
            intact_bytes: 0,
            made_by_agent: false,
            bad_sector_files: 0,
            bad_sector_bytes: 0,
        };
        let mut made = 0;
        let mut placed = 0;
        let mut found = 0;
        for source in &group.sources {
            match source {
                Source::Item(id) => {
                    folder.items.push(*id);
                    folder.made_by_agent |= self.items[id].made_by_agent;
                    if self.items[id].kind == ITEM_FOLDER {
                        made += 1;
                    } else {
                        placed += 1;
                    }
                }
                Source::Found(head) => {
                    folder.heads.push(*head);
                    found += 1;
                }
            }
        }
        let mut counted = HashSet::new();
        for source in self.expand(&group.sources) {
            self.add_sizes(source, &mut folder, &mut counted)?;
        }
        folder.merged = made + placed + found > 1
            || self.expand(&group.sources).iter().any(
                |source| matches!(source, Source::Item(id) if self.items[id].kind == ITEM_MERGE),
            );
        folder.origin = if placed > 0 {
            "placed"
        } else if made > 0 && found == 0 {
            "made"
        } else {
            "found"
        }
        .into();
        Ok(folder)
    }

    /// Adds a found folder's totals, and those of everything placed anywhere below it.
    fn add_sizes(
        &self,
        source: Source,
        folder: &mut RebuiltFolder,
        counted: &mut HashSet<Source>,
    ) -> Result<()> {
        if !counted.insert(source) {
            return Ok(());
        }
        if let Source::Found(head) = source {
            let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE head = ?1");
            if let Some(row) = self
                .database
                .connection
                .query_row(&sql, [head], node_row)
                .optional()?
            {
                folder.files += row.files;
                folder.bytes += row.bytes;
                folder.intact_files += row.intact_files;
                folder.intact_bytes += row.intact_bytes;
            }
        }
        if let Source::Item(id) = source {
            if let Some(head) = self.items[&id].head {
                self.add_sizes(Source::Found(head), folder, counted)?;
            }
        }
        // Items placed in this item, or inside any found folder of this item's own piece.
        let placed: Vec<i64> = self
            .items
            .values()
            .filter(|item| match source {
                Source::Item(id) => item.parent_item == Some(id),
                Source::Found(head) => item.parent_head == Some(head),
            })
            .map(|item| item.id)
            .collect();
        for id in placed {
            self.add_sizes(Source::Item(id), folder, counted)?;
        }
        Ok(())
    }
}
