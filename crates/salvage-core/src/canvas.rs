//! The person's own rebuild of the folder tree, from the pieces the scan found.
//!
//! A damaged disk leaves pieces: found folders whose parent is gone. A canvas records where the
//! person put each one, as if doing a jigsaw: inside another folder, merged into one so their
//! contents become a single folder, or inside a folder they made up because the disk lost it.
//!
//! The rebuilt tree is worked out from those records and the map whenever it is read, so it
//! follows a rebuild of the map. It is never written anywhere but the app's own database, and
//! never to the disk. Where two pieces hold a file of the same name, the copy with the best data
//! quality is shown, unless the person chose another.
//!
//! A canvas is about arranging what was found. It does not change how the map reads the disk,
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

/// Deeper than any real tree, so a damaged one that loops cannot hang a walk.
const MAX_DEPTH: usize = 256;
const MAX_NAME_CHARS: usize = 255;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Canvas {
    pub id: i64,
    pub name: String,
    pub created_at_millis: u64,
    pub updated_at_millis: u64,
    pub items: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CanvasItem {
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
}

/// Where something is put: in a canvas item, and optionally in a found folder inside it.
/// Nothing at all means the canvas itself: the root when there is none yet, inside it after.
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
        "partial" => 4,
        "tested-bad" => 5,
        "overwritten" => 6,
        "damaged-entry" => 7,
        "empty" => 8,
        _ => 9,
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
    pub fn canvases(&self) -> Result<Vec<Canvas>> {
        let mut statement = self.connection.prepare_cached(
            "SELECT c.id, c.name, c.created_at_millis, c.updated_at_millis, \
             (SELECT COUNT(*) FROM canvas_items i WHERE i.canvas = c.id) \
             FROM canvases c ORDER BY c.updated_at_millis DESC, c.id DESC",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok(Canvas {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    created_at_millis: row.get::<_, i64>(2)? as u64,
                    updated_at_millis: row.get::<_, i64>(3)? as u64,
                    items: row.get::<_, i64>(4)? as u64,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    pub fn canvas_create(&self, name: &str) -> Result<i64> {
        let name = clean_name(name)?;
        let now = now_millis() as i64;
        self.connection.execute(
            "INSERT INTO canvases (name, created_at_millis, updated_at_millis) VALUES (?1, ?2, ?2)",
            params![name, now],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    pub fn canvas_rename(&self, canvas: i64, name: &str) -> Result<()> {
        let name = clean_name(name)?;
        self.need_canvas(canvas)?;
        self.connection.execute(
            "UPDATE canvases SET name = ?2, updated_at_millis = ?3 WHERE id = ?1",
            params![canvas, name, now_millis() as i64],
        )?;
        Ok(())
    }

    pub fn canvas_delete(&self, canvas: i64) -> Result<()> {
        self.need_canvas(canvas)?;
        let transaction = self.connection.unchecked_transaction()?;
        transaction.execute("DELETE FROM canvas_items WHERE canvas = ?1", [canvas])?;
        transaction.execute("DELETE FROM canvas_choices WHERE canvas = ?1", [canvas])?;
        transaction.execute("DELETE FROM canvas_suggestions WHERE canvas = ?1", [canvas])?;
        transaction.execute("DELETE FROM canvases WHERE id = ?1", [canvas])?;
        transaction.commit()?;
        Ok(())
    }

    fn need_canvas(&self, canvas: i64) -> Result<()> {
        let found: Option<i64> = self
            .connection
            .query_row("SELECT id FROM canvases WHERE id = ?1", [canvas], |row| {
                row.get(0)
            })
            .optional()?;
        found
            .map(|_| ())
            .with_context(|| format!("there is no canvas {canvas}"))
    }

    fn touch_canvas(&self, canvas: i64) -> Result<()> {
        self.connection.execute(
            "UPDATE canvases SET updated_at_millis = ?2 WHERE id = ?1",
            params![canvas, now_millis() as i64],
        )?;
        Ok(())
    }

    /// Every item on a canvas, in the order they were placed.
    pub fn canvas_items(&self, canvas: i64) -> Result<Vec<CanvasItem>> {
        self.need_canvas(canvas)?;
        let mut statement = self.connection.prepare_cached(
            "SELECT id, kind, head, name, parent_item, parent_head FROM canvas_items \
             WHERE canvas = ?1 ORDER BY position, id",
        )?;
        let rows = statement
            .query_map([canvas], |row| {
                Ok(CanvasItem {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    head: row.get(2)?,
                    name: row.get(3)?,
                    parent_item: row.get(4)?,
                    parent_head: row.get(5)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(rows)
    }

    fn canvas_item(&self, canvas: i64, item: i64) -> Result<CanvasItem> {
        self.canvas_items(canvas)?
            .into_iter()
            .find(|found| found.id == item)
            .with_context(|| format!("there is no item {item} on canvas {canvas}"))
    }

    fn canvas_root(&self, canvas: i64) -> Result<Option<CanvasItem>> {
        Ok(self
            .canvas_items(canvas)?
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

    /// Works out where a place really is, refusing one that does not exist on this canvas.
    fn resolve_place(&self, canvas: i64, place: Place) -> Result<Option<(i64, Option<u32>)>> {
        let item = match place.item {
            Some(item) => self.canvas_item(canvas, item)?,
            None => match self.canvas_root(canvas)? {
                Some(root) => root,
                None => {
                    if place.head.is_some() {
                        bail!("A found folder can only be named inside an item of the canvas.");
                    }
                    return Ok(None);
                }
            },
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

    fn next_position(&self, canvas: i64) -> Result<i64> {
        Ok(self.connection.query_row(
            "SELECT COALESCE(MAX(position), -1) + 1 FROM canvas_items WHERE canvas = ?1",
            [canvas],
            |row| row.get(0),
        )?)
    }

    fn used_heads(&self, canvas: i64) -> Result<HashSet<u32>> {
        Ok(self
            .canvas_items(canvas)?
            .into_iter()
            .filter_map(|item| item.head)
            .collect())
    }

    /// Puts a piece on the canvas, as a folder of its own or merged into the folder it lands
    /// in. The first thing placed becomes the root. A piece can be used once on each canvas.
    pub fn canvas_place(&self, canvas: i64, head: u32, place: Place, merge: bool) -> Result<i64> {
        self.need_canvas(canvas)?;
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
        if self.used_heads(canvas)?.contains(&head) {
            bail!("That piece is already on this canvas.");
        }
        let resolved = self.resolve_place(canvas, place)?;
        let (kind, parent_item, parent_head) = match resolved {
            // The first piece is the root, which nothing can be merged into yet.
            None => (ITEM_PIECE, None, None),
            Some((item, at)) => (if merge { ITEM_MERGE } else { ITEM_PIECE }, Some(item), at),
        };
        self.connection.execute(
            "INSERT INTO canvas_items (canvas, kind, head, parent_item, parent_head, position, created_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![canvas, kind, head, parent_item, parent_head, self.next_position(canvas)?, now_millis() as i64],
        )?;
        let id = self.connection.last_insert_rowid();
        self.touch_canvas(canvas)?;
        Ok(id)
    }

    /// Makes up a folder the disk no longer has, so pieces can be put inside it.
    pub fn canvas_folder(&self, canvas: i64, name: &str, place: Place) -> Result<i64> {
        self.need_canvas(canvas)?;
        let name = clean_name(name)?;
        let resolved = self.resolve_place(canvas, place)?;
        let (parent_item, parent_head) = match resolved {
            None => (None, None),
            Some((item, at)) => (Some(item), at),
        };
        self.connection.execute(
            "INSERT INTO canvas_items (canvas, kind, name, parent_item, parent_head, position, created_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![canvas, ITEM_FOLDER, name, parent_item, parent_head, self.next_position(canvas)?, now_millis() as i64],
        )?;
        let id = self.connection.last_insert_rowid();
        self.touch_canvas(canvas)?;
        Ok(id)
    }

    /// Moves an item, and everything in it, somewhere else on the same canvas.
    pub fn canvas_move(
        &self,
        canvas: i64,
        item: i64,
        place: Place,
        merge: Option<bool>,
    ) -> Result<()> {
        let moving = self.canvas_item(canvas, item)?;
        if moving.parent_item.is_none() {
            bail!("The root cannot move. Put things inside it instead.");
        }
        let Some((target, at)) = self.resolve_place(canvas, place)? else {
            bail!("The canvas has no root to move it into.");
        };
        let items = self.canvas_items(canvas)?;
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
            "UPDATE canvas_items SET parent_item = ?2, parent_head = ?3, kind = ?4 WHERE id = ?1",
            params![item, target, at, kind],
        )?;
        self.touch_canvas(canvas)?;
        Ok(())
    }

    /// Swaps the piece behind an item for another unused piece, keeping where it sits and
    /// everything placed in it. For trading a piece for a similar one with better data.
    pub fn canvas_swap(&self, canvas: i64, item: i64, head: u32) -> Result<()> {
        let found = self.canvas_item(canvas, item)?;
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
        if self.used_heads(canvas)?.contains(&head) {
            bail!("That piece is already on this canvas.");
        }
        // Things placed in a found folder of the old piece have nowhere to be in the new one.
        let stranded: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM canvas_items WHERE canvas = ?1 AND parent_item = ?2 AND parent_head IS NOT NULL",
            params![canvas, item],
            |row| row.get(0),
        )?;
        if stranded > 0 {
            bail!("Something is placed inside a folder of this piece. Move it out first.");
        }
        self.connection.execute(
            "UPDATE canvas_items SET head = ?2 WHERE id = ?1",
            params![item, head],
        )?;
        self.touch_canvas(canvas)?;
        Ok(())
    }

    /// Names an item in the rebuilt tree. For a piece, no name puts back the folder's own.
    pub fn canvas_rename_item(&self, canvas: i64, item: i64, name: Option<&str>) -> Result<()> {
        let found = self.canvas_item(canvas, item)?;
        let name = match name {
            Some(name) => Some(clean_name(name)?),
            None if found.kind == ITEM_FOLDER => bail!("A folder you made needs a name."),
            None => None,
        };
        self.connection.execute(
            "UPDATE canvas_items SET name = ?2 WHERE id = ?1",
            params![item, name],
        )?;
        self.touch_canvas(canvas)?;
        Ok(())
    }

    /// Takes an item off the canvas with everything inside it, so its pieces are unused again.
    pub fn canvas_remove(&self, canvas: i64, item: i64) -> Result<u64> {
        self.canvas_item(canvas, item)?;
        let items = self.canvas_items(canvas)?;
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
        let transaction = self.connection.unchecked_transaction()?;
        for id in &doomed {
            transaction.execute("DELETE FROM canvas_items WHERE id = ?1", [id])?;
        }
        transaction.commit()?;
        self.touch_canvas(canvas)?;
        Ok(doomed.len() as u64)
    }

    /// Top-level found folders not yet on this canvas, largest first.
    pub fn canvas_unused(&self, canvas: i64, offset: u64, limit: u64) -> Result<UnusedPieces> {
        self.need_canvas(canvas)?;
        let unused = "FROM nodes WHERE parent IS NULL AND head NOT IN \
             (SELECT head FROM canvas_items WHERE canvas = ?1 AND head IS NOT NULL)";
        let (total, total_files, total_bytes): (i64, i64, i64) = self.connection.query_row(
            &format!("SELECT COUNT(*), COALESCE(SUM(files), 0), COALESCE(SUM(bytes), 0) {unused}"),
            [canvas],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let sql = format!(
            "SELECT {NODE_COLUMNS} {unused} ORDER BY kind = 'root' DESC, bytes DESC, head LIMIT ?2 OFFSET ?3"
        );
        let mut statement = self.connection.prepare_cached(&sql)?;
        let rows = statement
            .query_map(params![canvas, limit as i64, offset as i64], node_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(UnusedPieces {
            rows,
            total: total as u64,
            total_files: total_files as u64,
            total_bytes: total_bytes as u64,
        })
    }

    /// Puts every unused piece, or only those with at most `max_files` files, into one made-up
    /// folder at the top of the canvas, so the small ones need not be placed one by one.
    pub fn canvas_leftovers(
        &self,
        canvas: i64,
        name: &str,
        max_files: Option<u64>,
    ) -> Result<(i64, u64)> {
        let Some(root) = self.canvas_root(canvas)? else {
            bail!("Place a first piece before putting the rest away.");
        };
        let unused = self.canvas_unused(canvas, 0, u64::MAX >> 1)?;
        let chosen: Vec<u32> = unused
            .rows
            .iter()
            .filter(|row| max_files.is_none_or(|most| row.files <= most))
            .filter_map(|row| row.head)
            .collect();
        if chosen.is_empty() {
            bail!("There are no unused pieces left to put away.");
        }
        let folder = self.canvas_folder(
            canvas,
            name,
            Place {
                item: Some(root.id),
                head: None,
            },
        )?;
        let transaction = self.connection.unchecked_transaction()?;
        let first = self.next_position(canvas)?;
        let now = now_millis() as i64;
        for (position, head) in (first..).zip(chosen.iter()) {
            transaction.execute(
                "INSERT INTO canvas_items (canvas, kind, head, parent_item, position, created_at_millis) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![canvas, ITEM_PIECE, head, folder, position, now],
            )?;
        }
        transaction.commit()?;
        self.touch_canvas(canvas)?;
        Ok((folder, chosen.len() as u64))
    }

    /// Picks which copy of a clashing file the rebuilt tree shows. None puts back the best one.
    pub fn canvas_choose(&self, canvas: i64, path: &[String], entry: Option<i64>) -> Result<()> {
        self.need_canvas(canvas)?;
        let Some((name, folder)) = path.split_last() else {
            bail!("A choice needs the path of a file.");
        };
        let listing = self.canvas_list(canvas, folder, 0, u64::MAX >> 1)?;
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
                    "INSERT OR REPLACE INTO canvas_choices (canvas, path, entry) VALUES (?1, ?2, ?3)",
                    params![canvas, path_key, entry],
                )?;
            }
            None => {
                self.connection.execute(
                    "DELETE FROM canvas_choices WHERE canvas = ?1 AND path = ?2",
                    params![canvas, path_key],
                )?;
            }
        }
        self.touch_canvas(canvas)?;
        Ok(())
    }

    /// Lists one folder of the rebuilt tree, found by the names on the way down from the root.
    /// The files are paged; the folders are always all there.
    pub fn canvas_list(
        &self,
        canvas: i64,
        path: &[String],
        offset: u64,
        limit: u64,
    ) -> Result<Listing> {
        let items = self.canvas_items(canvas)?;
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
                bail!("This canvas is empty.");
            }
            return Ok(listing);
        };
        let tree = Tree::new(self, canvas, &items)?;
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
        let mut rebuilt = rebuild_files(files, path, &choices, &tree.file_layers.borrow(), &hashes);
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
pub struct CanvasFile {
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
            // The name is the chosen copy's, since copies can differ in case.
            RebuiltFile {
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
    fn walk_canvas(
        &self,
        canvas: i64,
        mut visit: impl FnMut(&[String], &[String], RebuiltFile),
    ) -> Result<()> {
        self.need_canvas(canvas)?;
        let items = self.canvas_items(canvas)?;
        let Some(root) = items.iter().find(|item| item.parent_item.is_none()) else {
            bail!("This canvas is empty.");
        };
        let tree = Tree::new(self, canvas, &items)?;
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
            for file in rebuild_files(files, &path, &choices, &layers, &hashes) {
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
    pub fn canvas_files(&self, canvas: i64) -> Result<Vec<CanvasFile>> {
        let mut out = Vec::new();
        self.walk_canvas(canvas, |_, folders, file| {
            out.push(CanvasFile {
                folders: folders.to_vec(),
                row: file.chosen,
            });
        })?;
        Ok(out)
    }

    /// Every clash in the rebuilt tree, those that need the person first.
    pub fn canvas_clashes(
        &self,
        canvas: i64,
        need_you_only: bool,
        offset: u64,
        limit: u64,
    ) -> Result<ClashPage> {
        let mut found = Vec::new();
        self.walk_canvas(canvas, |path, folders, file| {
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
    pub fn canvas_suggest(
        &self,
        canvas: i64,
        head: u32,
        place: Place,
        merge: bool,
        note: &str,
        author: &str,
    ) -> Result<i64> {
        self.need_canvas(canvas)?;
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
        if self.used_heads(canvas)?.contains(&head) {
            bail!("That piece is already on this canvas.");
        }
        // Checked now, so a suggestion that names a place that does not exist is refused at once.
        let resolved = self.resolve_place(canvas, place)?;
        let (item, at) = match resolved {
            None => (None, None),
            Some((item, at)) => (Some(item), at),
        };
        if note.trim().is_empty() {
            bail!("A suggestion needs a note saying why.");
        }
        self.connection.execute(
            "INSERT INTO canvas_suggestions (canvas, head, place_item, place_head, merge, note, author, state, created_at_millis) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'open', ?8)",
            params![canvas, head, item, at, merge, note.trim(), author, now_millis() as i64],
        )?;
        Ok(self.connection.last_insert_rowid())
    }

    /// Suggestions still waiting for the person, oldest first.
    pub fn canvas_suggestions(&self, canvas: i64) -> Result<Vec<Suggestion>> {
        self.need_canvas(canvas)?;
        // One whose piece has since been placed, or whose place is gone, has nothing left to ask.
        let used = self.used_heads(canvas)?;
        let items: HashSet<i64> = self
            .canvas_items(canvas)?
            .iter()
            .map(|item| item.id)
            .collect();
        let mut statement = self.connection.prepare_cached(
            "SELECT id, head, place_item, place_head, merge, note, author, created_at_millis FROM canvas_suggestions \
             WHERE canvas = ?1 AND state = 'open' ORDER BY id",
        )?;
        let rows = statement
            .query_map([canvas], |row| {
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
    pub fn canvas_accept(&self, canvas: i64, suggestion: i64) -> Result<i64> {
        let found = self
            .canvas_suggestions(canvas)?
            .into_iter()
            .find(|row| row.id == suggestion)
            .with_context(|| format!("there is no open suggestion {suggestion}"))?;
        let place = Place {
            item: found.place_item,
            head: found.place_head,
        };
        let item = self.canvas_place(canvas, found.head, place, found.merge)?;
        self.connection.execute(
            "UPDATE canvas_suggestions SET state = 'accepted' WHERE id = ?1",
            [suggestion],
        )?;
        Ok(item)
    }

    pub fn canvas_dismiss(&self, canvas: i64, suggestion: i64) -> Result<()> {
        self.need_canvas(canvas)?;
        let changed = self.connection.execute(
            "UPDATE canvas_suggestions SET state = 'dismissed' WHERE id = ?1 AND canvas = ?2 AND state = 'open'",
            params![suggestion, canvas],
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

/// One canvas, read once, for working out its folders.
struct Tree<'a> {
    database: &'a MapDb,
    canvas: i64,
    items: HashMap<i64, CanvasItem>,
    /// Items in each place, in the order placed.
    inside: HashMap<Source, Vec<i64>>,
    /// The layer each found folder's files came from: the canvas item that brought it, whose
    /// id grows with each drop, so a later drop is an upper layer.
    folder_layers: RefCell<HashMap<u32, i64>>,
    /// The same, for each file seen, by entry id.
    file_layers: RefCell<HashMap<i64, i64>>,
}

impl<'a> Tree<'a> {
    fn new(database: &'a MapDb, canvas: i64, items: &[CanvasItem]) -> Result<Self> {
        let mut inside: HashMap<Source, Vec<i64>> = HashMap::new();
        for item in items {
            let place = match (item.parent_item, item.parent_head) {
                (_, Some(head)) => Source::Found(head),
                (Some(parent), None) => Source::Item(parent),
                (None, None) => continue,
            };
            inside.entry(place).or_default().push(item.id);
        }
        Ok(Self {
            database,
            canvas,
            items: items.iter().map(|item| (item.id, item.clone())).collect(),
            inside,
            folder_layers: RefCell::new(HashMap::new()),
            file_layers: RefCell::new(HashMap::new()),
        })
    }

    fn choices(&self) -> Result<HashMap<String, i64>> {
        let mut statement = self
            .database
            .connection
            .prepare_cached("SELECT path, entry FROM canvas_choices WHERE canvas = ?1")?;
        let rows = statement
            .query_map([self.canvas], |row| Ok((row.get(0)?, row.get(1)?)))?
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
    fn item_name(&self, item: &CanvasItem) -> Result<(Option<String>, Option<u32>)> {
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
        };
        let mut made = 0;
        let mut placed = 0;
        let mut found = 0;
        for source in &group.sources {
            match source {
                Source::Item(id) => {
                    folder.items.push(*id);
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
