//! The recovery basket: what to copy, in the order that matters to the person.
//!
//! The basket only records intentions, on this Mac. Nothing is read or written
//! until the person starts it, and removing an item touches no disk.

use crate::export::{canvas_target, CopyPlan, CopyRequest};
use crate::map::{MapDb, VERDICT_FOLDER};
use crate::session::Session;
use crate::workspace::now_millis;
use anyhow::{bail, Context, Result};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::Arc;

pub const KIND_ENTRY: &str = "entry";
pub const KIND_FOLDER: &str = "folder";

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BasketItem {
    pub id: i64,
    pub kind: String,
    pub target: i64,
    pub position: i64,
    pub name: String,
    pub path: Option<String>,
    pub files: u64,
    pub bytes: u64,
    pub likely_good_bytes: u64,
    pub verdict: String,
    pub confidence: f64,
    pub missing: bool,
}

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct BasketOptions {
    pub destination: String,
    pub min_confidence: f64,
    pub include_partial: bool,
}

impl MapDb {
    /// Adds an item at the end. Adding one already there changes nothing.
    pub fn basket_add(&self, kind: &str, target: i64) -> Result<()> {
        if kind != KIND_ENTRY && kind != KIND_FOLDER {
            bail!("kind must be {KIND_ENTRY} or {KIND_FOLDER}, not {kind}");
        }
        let exists = match kind {
            KIND_ENTRY => self.entry(target)?.is_some(),
            _ => self.node(target as u32)?.is_some(),
        };
        if !exists {
            bail!("there is no {kind} {target}");
        }
        self.connection.execute(
            "INSERT OR IGNORE INTO basket (kind, target, position, added_at_millis) \
             VALUES (?1, ?2, (SELECT COALESCE(MAX(position), -1) + 1 FROM basket), ?3)",
            params![kind, target, now_millis() as i64],
        )?;
        Ok(())
    }

    pub fn basket_remove(&self, id: i64) -> Result<()> {
        self.connection
            .execute("DELETE FROM basket WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn basket_clear(&self) -> Result<()> {
        self.connection.execute("DELETE FROM basket", [])?;
        Ok(())
    }

    fn basket_ids(&self) -> Result<Vec<i64>> {
        let mut statement = self
            .connection
            .prepare("SELECT id FROM basket ORDER BY position, id")?;
        let ids = statement.query_map([], |row| row.get(0))?;
        Ok(ids.collect::<rusqlite::Result<Vec<i64>>>()?)
    }

    /// Moves an item to `position`, counted from zero, and closes any gap.
    pub fn basket_move(&mut self, id: i64, position: usize) -> Result<()> {
        let mut ids = self.basket_ids()?;
        let current = ids
            .iter()
            .position(|value| *value == id)
            .context("that item is not in the basket")?;
        ids.remove(current);
        ids.insert(position.min(ids.len()), id);
        let transaction = self.connection.transaction()?;
        for (index, value) in ids.iter().enumerate() {
            transaction.execute(
                "UPDATE basket SET position = ?2 WHERE id = ?1",
                params![value, index as i64],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn basket_rows(&self) -> Result<Vec<(i64, String, i64, i64)>> {
        let mut statement = self
            .connection
            .prepare("SELECT id, kind, target, position FROM basket ORDER BY position, id")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }

    pub fn basket_items(&self) -> Result<Vec<BasketItem>> {
        let mut items = Vec::new();
        for (id, kind, target, position) in self.basket_rows()? {
            let mut item = BasketItem {
                id,
                kind: kind.clone(),
                target,
                position,
                ..BasketItem::default()
            };
            if kind == KIND_ENTRY {
                match self.entry(target)? {
                    Some(row) => {
                        item.name = row.name;
                        item.path = row.path;
                        item.files = 1;
                        item.bytes = row.length;
                        item.verdict = row.verdict;
                        item.confidence = row.confidence;
                        item.likely_good_bytes = row.length;
                    }
                    None => item.missing = true,
                }
            } else {
                match self.node(target as u32)? {
                    Some(row) => {
                        item.name = row.name;
                        item.path = row.path;
                        item.files = row.files;
                        item.bytes = row.bytes;
                        item.likely_good_bytes = row.intact_bytes;
                        item.verdict = VERDICT_FOLDER.to_string();
                        item.confidence = row.link_confidence.unwrap_or(1.0);
                    }
                    None => item.missing = true,
                }
            }
            items.push(item);
        }
        Ok(items)
    }
}

impl Session {
    pub fn basket_items(&self) -> Result<Vec<BasketItem>> {
        self.with_database(|database| database.basket_items())
    }

    /// Reorders on a separate connection, because a move needs a transaction.
    pub fn basket_move(&self, id: i64, position: usize) -> Result<()> {
        MapDb::open(&self.workspace.database())?.basket_move(id, position)
    }

    fn basket_groups(&self, options: &BasketOptions) -> Result<Vec<CopyRequest>> {
        let items = self.basket_items()?;
        Ok(items
            .iter()
            .filter(|item| !item.missing)
            .map(|item| CopyRequest {
                destination: options.destination.clone(),
                entry_ids: if item.kind == KIND_ENTRY {
                    vec![item.target]
                } else {
                    Vec::new()
                },
                folders: if item.kind == KIND_FOLDER {
                    vec![item.target as u32]
                } else {
                    Vec::new()
                },
                min_confidence: options.min_confidence,
                include_partial: options.include_partial,
                canvas: None,
            })
            .collect())
    }

    /// Exact totals for the whole basket, with overlaps removed, without writing.
    pub fn basket_plan(&self, options: &BasketOptions) -> Result<CopyPlan> {
        let groups = self.basket_groups(options)?;
        let combined = CopyRequest {
            destination: options.destination.clone(),
            entry_ids: groups
                .iter()
                .flat_map(|group| group.entry_ids.clone())
                .collect(),
            folders: groups
                .iter()
                .flat_map(|group| group.folders.clone())
                .collect(),
            min_confidence: options.min_confidence,
            include_partial: options.include_partial,
            canvas: None,
        };
        self.copy_plan(&combined)
    }

    /// Copies the basket in its order.
    pub fn basket_start(self: &Arc<Self>, options: &BasketOptions) -> Result<CopyPlan> {
        let groups = self.basket_groups(options)?;
        if groups.is_empty() {
            bail!("The basket is empty. Add files or folders first.");
        }
        self.copy_start_groups(groups)
    }

    /// Where a canvas copies to: a folder named after it, inside the destination.
    fn canvas_request(&self, canvas: i64, options: &BasketOptions) -> Result<CopyRequest> {
        let name = self
            .with_database(|database| database.canvases())?
            .into_iter()
            .find(|found| found.id == canvas)
            .map(|found| found.name)
            .with_context(|| format!("there is no canvas {canvas}"))?;
        let base = if options.destination.trim().is_empty() {
            self.salvage_drive()?.unwrap_or_default()
        } else {
            options.destination.clone()
        };
        // Left empty, the plan refuses it and says to choose a destination.
        let destination = if base.trim().is_empty() {
            String::new()
        } else {
            Path::new(&base)
                .join(canvas_target(&[], &name))
                .display()
                .to_string()
        };
        Ok(CopyRequest {
            destination,
            min_confidence: options.min_confidence,
            include_partial: options.include_partial,
            canvas: Some(canvas),
            ..CopyRequest::default()
        })
    }

    /// What copying a canvas's rebuilt tree would do, without writing.
    pub fn canvas_copy_plan(&self, canvas: i64, options: &BasketOptions) -> Result<CopyPlan> {
        let request = self.canvas_request(canvas, options)?;
        self.copy_plan(&request)
    }

    /// Copies a canvas's rebuilt tree, each file at its rebuilt path.
    pub fn canvas_copy_start(
        self: &Arc<Self>,
        canvas: i64,
        options: &BasketOptions,
    ) -> Result<CopyPlan> {
        let request = self.canvas_request(canvas, options)?;
        self.copy_start(request)
    }
}
