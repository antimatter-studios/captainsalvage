//! What changed on a volume since its baseline.
//!
//! The first time a volume is opened, its boot regions, FAT, allocation bitmap and root
//! directory are hashed, and a copy of them is kept on the Mac beside the hash. When the hash
//! later differs, comparing that copy with the disk says what was written: which clusters were
//! free then and are in use now, which FAT entries and root entries changed, and so which found
//! files lost data. Nothing here reads or writes anything but the Mac's own copy.

use crate::exfat::{find_sets, EntrySet, Limits, FIRST_CLUSTER};
use anyhow::{Context, Result};
use fs_exfat::primitives;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// The copy's files, in the order the structure hash covers them.
const COPY_FILES: [&str; 5] = [
    "boot_main.bin",
    "boot_backup.bin",
    "fat.bin",
    "bitmap.bin",
    "root.bin",
];

/// The volume's structures, byte for byte, as the structure hash covers them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructureCopy {
    pub main: Vec<u8>,
    pub backup: Vec<u8>,
    pub fat: Vec<u8>,
    pub bitmap: Vec<u8>,
    pub root: Vec<u8>,
}

impl StructureCopy {
    fn parts(&self) -> [&Vec<u8>; 5] {
        [
            &self.main,
            &self.backup,
            &self.fat,
            &self.bitmap,
            &self.root,
        ]
    }

    pub fn hash(&self) -> String {
        let mut hasher = Sha256::new();
        for part in self.parts() {
            hasher.update(part);
        }
        hasher
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        for (name, part) in COPY_FILES.iter().zip(self.parts()) {
            fs::write(dir.join(name), part).with_context(|| format!("writing {name}"))?;
        }
        Ok(())
    }

    /// The copy in `dir`, if every file is there.
    pub fn load(dir: &Path) -> Option<Self> {
        let mut parts = COPY_FILES.iter().map(|name| fs::read(dir.join(name)).ok());
        Some(Self {
            main: parts.next()??,
            backup: parts.next()??,
            fat: parts.next()??,
            bitmap: parts.next()??,
            root: parts.next()??,
        })
    }
}

/// One root directory entry that differs between the baseline and now.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RootChange {
    pub name: String,
    pub before: Option<EntryState>,
    pub after: Option<EntryState>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EntryState {
    pub in_use: bool,
    pub is_dir: bool,
    pub first_cluster: u32,
    pub length: u64,
    pub created: Option<String>,
    pub modified: Option<String>,
}

impl From<&EntrySet> for EntryState {
    fn from(set: &EntrySet) -> Self {
        Self {
            in_use: set.in_use,
            is_dir: set.is_dir,
            first_cluster: set.first_cluster,
            length: set.length,
            created: set.created.clone(),
            modified: set.modified.clone(),
        }
    }
}

/// A found file or folder that claimed a cluster written since the baseline.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AffectedEntry {
    pub id: i64,
    pub name: String,
    pub path: Option<String>,
    pub length: u64,
    pub is_dir: bool,
    pub clusters_written: u64,
}

/// What differs between the baseline copy and the disk now.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureChanges {
    pub baseline_recorded_at_millis: u64,
    /// When this difference was first seen. A later, different change resets it.
    pub detected_at_millis: u64,
    pub current_hash: String,
    pub boot_main_bytes: u64,
    pub boot_backup_bytes: u64,
    pub fat_entries: u64,
    /// Clusters free at the baseline and in use now, so written since.
    pub written_clusters: Vec<u32>,
    /// Clusters in use at the baseline and free now.
    pub freed_clusters: u64,
    pub root_entries: Vec<RootChange>,
    pub affected: Vec<AffectedEntry>,
}

/// A short account of the changes, for the status every screen polls.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChangeSummary {
    pub detected_at_millis: u64,
    pub written_clusters: u64,
    pub fat_entries: u64,
    pub root_entries: Vec<String>,
    pub boot_changed: bool,
    pub affected_files: u64,
    pub affected_bytes: u64,
}

impl StructureChanges {
    pub fn summary(&self) -> ChangeSummary {
        let files = self.affected.iter().filter(|entry| !entry.is_dir);
        ChangeSummary {
            detected_at_millis: self.detected_at_millis,
            written_clusters: self.written_clusters.len() as u64,
            fat_entries: self.fat_entries,
            root_entries: self
                .root_entries
                .iter()
                .map(|entry| entry.name.clone())
                .collect(),
            boot_changed: self.boot_main_bytes + self.boot_backup_bytes > 0,
            affected_files: files.clone().count() as u64,
            affected_bytes: files.map(|entry| entry.length).sum(),
        }
    }
}

fn differing_bytes(before: &[u8], after: &[u8]) -> u64 {
    let common = before
        .iter()
        .zip(after)
        .filter(|(left, right)| left != right)
        .count();
    (common + before.len().abs_diff(after.len())) as u64
}

fn fat_entry(fat: &[u8], cluster: usize) -> Option<u32> {
    let entry_size = std::mem::size_of::<u32>();
    fat.get(cluster * entry_size..(cluster + 1) * entry_size)
        .map(|bytes| primitives::u32_at(bytes, 0))
}

fn bit(bitmap: &[u8], cluster: u32) -> bool {
    let index = (cluster - FIRST_CLUSTER) as usize;
    index < bitmap.len().saturating_mul(u8::BITS as usize)
        && primitives::is_allocated(bitmap, cluster)
}

fn root_changes(before: &[u8], after: &[u8], limits: Limits) -> Vec<RootChange> {
    let sets = |data: &[u8]| -> Vec<(usize, EntrySet)> {
        find_sets(data, data.len(), limits)
            .into_iter()
            .filter(|set| set.checksum_ok)
            .map(|set| (set.offset, set))
            .collect()
    };
    let old = sets(before);
    let new = sets(after);
    let mut offsets: Vec<usize> = old.iter().chain(&new).map(|(offset, _)| *offset).collect();
    offsets.sort_unstable();
    offsets.dedup();
    let find = |list: &[(usize, EntrySet)], offset: usize| {
        list.iter()
            .find(|(at, _)| *at == offset)
            .map(|(_, set)| set.clone())
    };
    offsets
        .into_iter()
        .filter_map(|offset| {
            let was = find(&old, offset);
            let now = find(&new, offset);
            let before = was.as_ref().map(EntryState::from);
            let after = now.as_ref().map(EntryState::from);
            if before == after
                && was.as_ref().map(|set| &set.name) == now.as_ref().map(|set| &set.name)
            {
                return None;
            }
            let name = now.or(was).map(|set| set.name).unwrap_or_default();
            Some(RootChange {
                name,
                before,
                after,
            })
        })
        .collect()
}

/// Compares the baseline copy with the structures on disk now.
pub fn compare(
    baseline: &StructureCopy,
    current: &StructureCopy,
    limits: Limits,
    last_cluster_exclusive: u32,
) -> StructureChanges {
    let mut written = Vec::new();
    let mut freed = 0;
    let mut fat_entries = 0;
    for cluster in FIRST_CLUSTER..last_cluster_exclusive {
        match (
            bit(&baseline.bitmap, cluster),
            bit(&current.bitmap, cluster),
        ) {
            (false, true) => written.push(cluster),
            (true, false) => freed += 1,
            _ => {}
        }
    }
    let entries = baseline.fat.len().max(current.fat.len()) / std::mem::size_of::<u32>();
    for cluster in 0..entries {
        if fat_entry(&baseline.fat, cluster) != fat_entry(&current.fat, cluster) {
            fat_entries += 1;
        }
    }
    StructureChanges {
        current_hash: current.hash(),
        boot_main_bytes: differing_bytes(&baseline.main, &current.main),
        boot_backup_bytes: differing_bytes(&baseline.backup, &current.backup),
        fat_entries,
        written_clusters: written,
        freed_clusters: freed,
        root_entries: root_changes(&baseline.root, &current.root, limits),
        ..StructureChanges::default()
    }
}
