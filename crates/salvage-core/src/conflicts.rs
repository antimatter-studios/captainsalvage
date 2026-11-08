//! Who else claims a file's clusters, who holds them, and what they hold.
//!
//! A file marked partial because its clusters are shared needs the other side of the story:
//! which entries claim the same clusters, whether they were deleted and when they were made,
//! and what those clusters contain now. This puts that in one answer, worked out the way the
//! map works out ownership.

use crate::content::CLASS_NAMES;
use crate::exfat::FIRST_CLUSTER;
use crate::map::{file_row, to_extents, Extent, Row, FILE_COLUMNS, FILE_NODE_INDEX};
use crate::session::Session;
use anyhow::{Context, Result};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};

/// How far back a contiguous entry can start and still reach a cluster: the same reach as owners.
const LOOKBACK_BYTES: u64 = 16 * 1024 * 1024 * 1024;
/// Possible first clusters asked about per query.
const START_BATCH: usize = 500;
/// Claimants looked at, at most. A file shared with more than this is described in part.
const MAX_CANDIDATES: usize = 5000;
/// Runs of shared clusters listed per claimant.
const MAX_RUNS: usize = 20;

/// Who holds clusters two entries claim, by the rule the map uses: the one made after the
/// other was deleted.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Holder {
    ThisFile,
    TheOther,
    Unsettled,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Claimant {
    pub id: i64,
    pub name: String,
    pub path: Option<String>,
    pub verdict: String,
    pub in_use: bool,
    pub created: Option<String>,
    pub length: u64,
    pub is_dir: bool,
    pub shared_clusters: u64,
    pub shared: Vec<Extent>,
    pub holds: Holder,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContentCount {
    pub content: String,
    pub clusters: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Conflicts {
    pub id: i64,
    pub clusters: u64,
    /// Clusters any other entry or a directory also claims.
    pub shared_clusters: u64,
    /// Clusters holding directory entries the scan found, which no file can also hold.
    pub directory_clusters: u64,
    pub claimants: Vec<Claimant>,
    /// What the shared clusters hold now.
    pub shared_content: Vec<ContentCount>,
    pub shared_mean_entropy: f64,
    /// Whether more possible claimants existed than were looked at.
    pub cut_short: bool,
}

fn wall_clock(time: &Option<String>) -> Option<&str> {
    time.as_deref().map(|text| text.get(..19).unwrap_or(text))
}

/// Whether `newer` was made after `older` was deleted, so their shared clusters hold `newer`.
fn replaced(older: &Row, newer: &Row) -> bool {
    match (wall_clock(&older.created), wall_clock(&newer.created)) {
        (Some(old), Some(new)) => !older.in_use && old < new,
        _ => false,
    }
}

impl Session {
    pub fn conflicts(&self, id: i64) -> Result<Conflicts> {
        let volume = &self.volume;
        let (file, clusters, candidates, directories, cut_short) =
            self.with_database(|database| {
                let file = database
                    .entry(id)?
                    .with_context(|| format!("no entry with id {id}"))?;
                let clusters = database.entry_clusters(volume, &file)?;
                let Some((&low, &high)) = clusters.iter().min().zip(clusters.iter().max()) else {
                    return Ok((file, clusters, Vec::new(), HashSet::new(), false));
                };
                // Chains can be claimed from any of their clusters, or from before the head.
                let mut starts: Vec<u32> = clusters.clone();
                let mut seen: HashSet<u32> = clusters.iter().copied().collect();
                let mut current = clusters[0];
                while let Some(previous) = volume.predecessor(current) {
                    if !seen.insert(previous) {
                        break;
                    }
                    starts.push(previous);
                    current = previous;
                }
                let lookback = (LOOKBACK_BYTES / volume.boot.cluster_size()) as u32;
                let mut candidates: Vec<(Row, Option<u32>)> = Vec::new();
                let read = |row: &rusqlite::Row<'_>| Ok((file_row(row)?, row.get::<_, Option<u32>>(FILE_NODE_INDEX)?));
                let contiguous = format!(
                    "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_ok, checksum_ok) = 1 AND length > 0 \
                     AND id != ?3 AND COALESCE(o_no_fat, no_fat_chain) = 1 AND COALESCE(o_first, first_cluster) BETWEEN ?1 AND ?2"
                );
                candidates.extend(
                    database
                        .connection
                        .prepare(&contiguous)?
                        .query_map(rusqlite::params![low.saturating_sub(lookback), high, id], read)?
                        .collect::<rusqlite::Result<Vec<_>>>()?,
                );
                // The starts are cluster numbers this code produced, so listing them inline is safe.
                for batch in starts.chunks(START_BATCH) {
                    let list = batch.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                    let chained = format!(
                        "SELECT {FILE_COLUMNS}, node FROM sets WHERE COALESCE(o_ok, checksum_ok) = 1 AND length > 0 \
                         AND id != {id} AND COALESCE(o_no_fat, no_fat_chain) = 0 AND COALESCE(o_first, first_cluster) IN ({list})"
                    );
                    candidates.extend(
                        database
                            .connection
                            .prepare(&chained)?
                            .query_map([], read)?
                            .collect::<rusqlite::Result<Vec<_>>>()?,
                    );
                }
                let cut_short = candidates.len() > MAX_CANDIDATES;
                candidates.truncate(MAX_CANDIDATES);
                let mut directories = HashSet::new();
                for batch in clusters.chunks(START_BATCH) {
                    let list = batch.iter().map(u32::to_string).collect::<Vec<_>>().join(",");
                    let mut statement = database.connection.prepare(&format!(
                        "SELECT DISTINCT source_cluster FROM sets WHERE checksum_ok = 1 AND source_cluster IN ({list})"
                    ))?;
                    for cluster in statement.query_map([], |row| row.get::<_, u32>(0))? {
                        directories.insert(cluster?);
                    }
                }
                let mut with_clusters = Vec::new();
                for (row, node) in candidates {
                    let theirs = database.entry_clusters(volume, &row)?;
                    with_clusters.push((row, node, theirs));
                }
                Ok((file, clusters, with_clusters, directories, cut_short))
            })?;

        let mine: HashSet<u32> = clusters.iter().copied().collect();
        let mut shared: HashSet<u32> = directories.clone();
        let mut claimants = Vec::new();
        for (row, node, theirs) in candidates {
            let mut overlap: Vec<u32> = theirs
                .into_iter()
                .filter(|cluster| mine.contains(cluster))
                .collect();
            if overlap.is_empty() {
                continue;
            }
            overlap.sort_unstable();
            overlap.dedup();
            shared.extend(&overlap);
            let holds = if replaced(&row, &file) {
                Holder::ThisFile
            } else if replaced(&file, &row) {
                Holder::TheOther
            } else {
                Holder::Unsettled
            };
            let path = self.with_database(|database| database.path_of(node)).ok();
            let mut runs = to_extents(&overlap);
            runs.truncate(MAX_RUNS);
            claimants.push(Claimant {
                id: row.id.unwrap_or_default(),
                name: row.name.clone(),
                path,
                verdict: row.verdict.clone(),
                in_use: row.in_use,
                created: row.created.clone(),
                length: row.length,
                is_dir: row.kind != "file",
                shared_clusters: overlap.len() as u64,
                shared: runs,
                holds,
            });
        }
        claimants.sort_by_key(|claimant| std::cmp::Reverse(claimant.shared_clusters));

        let classes = self
            .workspace
            .read_cluster_map(&self.workspace.classes(), volume.boot.cluster_count);
        let entropy = self
            .workspace
            .read_cluster_map(&self.workspace.entropy(), volume.boot.cluster_count);
        let mut content: BTreeMap<&str, u64> = BTreeMap::new();
        let mut entropy_sum = 0.0;
        for &cluster in &shared {
            if !volume.valid_cluster(cluster) {
                continue;
            }
            let index = (cluster - FIRST_CLUSTER) as usize;
            let class = classes.get(index).copied().unwrap_or(0);
            *content
                .entry(
                    CLASS_NAMES
                        .get(usize::from(class))
                        .copied()
                        .unwrap_or("unknown"),
                )
                .or_default() += 1;
            entropy_sum += f64::from(entropy.get(index).copied().unwrap_or(0)) * 8.0 / 255.0;
        }
        let mut shared_content: Vec<ContentCount> = content
            .into_iter()
            .map(|(name, clusters)| ContentCount {
                content: name.to_string(),
                clusters,
            })
            .collect();
        shared_content.sort_by_key(|item| std::cmp::Reverse(item.clusters));
        Ok(Conflicts {
            id,
            clusters: clusters.len() as u64,
            shared_clusters: shared.len() as u64,
            directory_clusters: directories.len() as u64,
            claimants,
            shared_content,
            shared_mean_entropy: if shared.is_empty() {
                0.0
            } else {
                entropy_sum / shared.len() as f64
            },
            cut_short,
        })
    }
}
