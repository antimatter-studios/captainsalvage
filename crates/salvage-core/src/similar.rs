//! Pieces that are near copies of one another.
//!
//! A damaged disk often leaves the same folder behind more than once: an older copy, a backup,
//! the leftovers of a move. Two pieces are similar when many of their files have the same name
//! and size. Knowing that lets the person place one copy instead of sorting through several, and
//! swap a placed piece for a similar one whose data came through better.

use crate::map::{node_row, MapDb, Row, NODE_COLUMNS};
use anyhow::{bail, Result};
use rusqlite::OptionalExtension;
use serde::Serialize;
use std::collections::HashMap;

/// Below this share of the piece's files in common, another piece is not called similar.
const MIN_SHARED_FRACTION: f64 = 0.2;
const MAX_DEPTH: usize = 256;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SimilarPiece {
    pub piece: Row,
    /// Files the two pieces have in common, by name and size.
    pub shared: u64,
    /// The share of this piece's files that the other piece also holds.
    pub covers: f64,
    /// The share of the other piece's files that this piece also holds.
    pub covered_by: f64,
    /// The share of each piece's bytes that is likely intact. Higher is better data.
    pub quality: f64,
    pub own_quality: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Similar {
    pub piece: Row,
    pub files: u64,
    pub similar: Vec<SimilarPiece>,
}

fn quality(row: &Row) -> f64 {
    if row.bytes == 0 {
        0.0
    } else {
        row.intact_bytes as f64 / row.bytes as f64
    }
}

impl MapDb {
    /// The top-level found folder each folder sits in, for every folder at once.
    fn tops(&self) -> Result<HashMap<u32, u32>> {
        let mut statement = self
            .connection
            .prepare_cached("SELECT head, parent FROM nodes")?;
        let parents: HashMap<u32, Option<u32>> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut tops = HashMap::with_capacity(parents.len());
        for &head in parents.keys() {
            let mut current = head;
            let mut path = Vec::new();
            for _ in 0..MAX_DEPTH {
                if let Some(&top) = tops.get(&current) {
                    current = top;
                    break;
                }
                path.push(current);
                match parents.get(&current).copied().flatten() {
                    Some(parent) => current = parent,
                    None => break,
                }
            }
            for node in path {
                tops.insert(node, current);
            }
        }
        Ok(tops)
    }

    /// Other pieces holding many of the same files as this one, most in common first.
    pub fn similar_pieces(&self, head: u32, limit: usize) -> Result<Similar> {
        let sql = format!("SELECT {NODE_COLUMNS}, parent FROM nodes WHERE head = ?1");
        let found = self
            .connection
            .query_row(&sql, [head], |row| {
                Ok((node_row(row)?, row.get::<_, Option<u32>>(9)?))
            })
            .optional()?;
        let Some((piece, parent)) = found else {
            bail!("there is no folder at cluster {head}");
        };
        if parent.is_some() {
            bail!("That folder is inside another piece. Place the piece it belongs to instead.");
        }
        let tops = self.tops()?;
        // Every file key once per piece, so a piece is not counted twice for duplicates.
        let mut keys: HashMap<(String, u64), Vec<u32>> = HashMap::new();
        let mut statement = self.connection.prepare_cached(
            "SELECT node, LOWER(COALESCE(o_name, name)), COALESCE(o_length, length) FROM sets \
             WHERE COALESCE(o_is_dir, is_dir) = 0 AND node IS NOT NULL AND COALESCE(o_length, length) > 0 \
             AND verdict != 'rename-residue'",
        )?;
        let mut files_in: HashMap<u32, u64> = HashMap::new();
        for row in statement.query_map([], |row| {
            Ok((
                row.get::<_, u32>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)? as u64,
            ))
        })? {
            let (node, name, length) = row?;
            let Some(&top) = tops.get(&node) else {
                continue;
            };
            let owners = keys.entry((name, length)).or_default();
            if !owners.contains(&top) {
                owners.push(top);
                *files_in.entry(top).or_default() += 1;
            }
        }
        let own = files_in.get(&head).copied().unwrap_or(0);
        let mut shared: HashMap<u32, u64> = HashMap::new();
        for owners in keys.values() {
            if owners.contains(&head) {
                for &other in owners.iter().filter(|&&other| other != head) {
                    *shared.entry(other).or_default() += 1;
                }
            }
        }
        let mut similar = Vec::new();
        for (other, count) in shared {
            let covers = if own == 0 {
                0.0
            } else {
                count as f64 / own as f64
            };
            let theirs = files_in.get(&other).copied().unwrap_or(0);
            let covered_by = if theirs == 0 {
                0.0
            } else {
                count as f64 / theirs as f64
            };
            if covers.max(covered_by) < MIN_SHARED_FRACTION {
                continue;
            }
            let sql = format!("SELECT {NODE_COLUMNS} FROM nodes WHERE head = ?1");
            let Some(row) = self
                .connection
                .query_row(&sql, [other], node_row)
                .optional()?
            else {
                continue;
            };
            similar.push(SimilarPiece {
                quality: quality(&row),
                own_quality: quality(&piece),
                piece: row,
                shared: count,
                covers,
                covered_by,
            });
        }
        similar.sort_by(|a, b| {
            b.shared
                .cmp(&a.shared)
                .then(b.quality.total_cmp(&a.quality))
        });
        similar.truncate(limit);
        Ok(Similar {
            piece,
            files: own,
            similar,
        })
    }
}
