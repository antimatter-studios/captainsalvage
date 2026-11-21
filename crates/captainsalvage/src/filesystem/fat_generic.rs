//! FAT12, FAT16 and FAT32 through the generic layers adapter: the default way a FAT volume's entries are found.
//!
//! The FAT driver implements rust-fs-core's layers (`filesystem::generic`), and what it says about a directory entry is
//! what the app uses: the entries found, whether each is live or deleted, and how sure the driver is of it. The old FAT
//! adapter (`filesystem::salvage::find_entries`, which reads every cluster for anything that parses as a directory entry)
//! stays for one release, selected by `SALVAGE_FAT_ADAPTER=legacy`, and is the fallback when the driver does not open a
//! volume.
//!
//! What the driver adds over the old adapter, as the person sees it:
//! - an entry in a cluster that is free in the FAT is deleted (its data may have been overwritten), where the old adapter
//!   read only the entry's own first byte;
//! - an entry the driver found by content only, not through a folder that names it, is a weak candidate: it carries
//!   `unchecked`, and `lone_entry` when it is the only entry in its cluster, so it may be file bytes that look like one.
//!
//! The driver reads the volume through an overlay that gives it the structures the app saved when it first opened the volume
//! (the boot sector, its backup, the FAT copies and a fixed root directory), so a volume read through its saved structures,
//! because the disk's own have been written over, is read the same way through either adapter.

use super::generic::GenericVolume;
use super::{FatWalk, SalvageEntry, VolumeGeometry};
use crate::allocation;
use crate::changes::StructureCopy;
use crate::ranges::RangeList;
use crate::source::BlockSource;
use anyhow::Result;
use fs_core_layers::layers::{CandidateKind, CandidateState, Integrity};
use std::collections::HashMap;
use std::sync::Arc;

/// Which adapter reads FAT12, FAT16 and FAT32 entries.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FatBackend {
    /// The generic layers adapter over the FAT driver. The default.
    Generic,
    /// The old FAT adapter, for one release.
    Legacy,
}

impl FatBackend {
    /// What the environment asks for: `SALVAGE_FAT_ADAPTER=legacy` selects the old adapter, anything else the generic one.
    pub fn requested() -> Self {
        match std::env::var("SALVAGE_FAT_ADAPTER") {
            Ok(value) if value.eq_ignore_ascii_case("legacy") => Self::Legacy,
            _ => Self::Generic,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Legacy => "legacy",
        }
    }
}

/// The volume's bytes, with the structures the app saved in place of the disk's own.
pub struct Overlay {
    source: Arc<dyn BlockSource>,
    length: u64,
    patches: Vec<(u64, Vec<u8>)>,
}

impl Overlay {
    /// The source as the driver sees it: `length` bytes, with `patches` (`(offset, bytes)`) read in place of the disk's.
    pub(crate) fn new(
        source: Arc<dyn BlockSource>,
        length: u64,
        patches: Vec<(u64, Vec<u8>)>,
    ) -> Self {
        Self {
            source,
            length,
            patches,
        }
    }

    fn read(&self, offset: u64, output: &mut [u8]) -> std::io::Result<()> {
        self.source.read_exact_at(offset, output)?;
        let end = offset + output.len() as u64;
        for (at, bytes) in &self.patches {
            let from = offset.max(*at);
            let to = end.min(*at + bytes.len() as u64);
            if from < to {
                output[(from - offset) as usize..(to - offset) as usize]
                    .copy_from_slice(&bytes[(from - at) as usize..(to - at) as usize]);
            }
        }
        Ok(())
    }
}

impl fs_core_layers::BlockRead for Overlay {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        self.read(offset, output).map_err(fs_core_layers::Error::Io)
    }
    fn size_bytes(&self) -> u64 {
        self.length
    }
}

impl fs_core::BlockRead for Overlay {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        self.read(offset, output).map_err(fs_core::Error::Io)
    }
    fn size_bytes(&self) -> u64 {
        self.length
    }
}

pub type GenericFat = GenericVolume<fs_fat32::Volume<Overlay>>;

/// The entries the driver found, placed the way the app stores them: by the cluster they are in and where in it.
#[derive(Default)]
pub struct Sets {
    /// Cluster 0 is the fixed root directory of FAT12 and FAT16.
    by_cluster: HashMap<u32, Vec<SalvageEntry>>,
}

impl Sets {
    pub fn in_cluster(&self, cluster: u32) -> Vec<SalvageEntry> {
        self.by_cluster.get(&cluster).cloned().unwrap_or_default()
    }
}

/// Opens the volume through the driver, reading the saved structures where the app has them.
pub fn open(
    source: &Arc<dyn BlockSource>,
    geometry: &VolumeGeometry,
    structures: &StructureCopy,
) -> Result<GenericFat> {
    let mut patches = vec![(0, structures.main.clone())];
    if !structures.backup.is_empty() {
        patches.push((geometry.bytes_per_sector, structures.backup.clone()));
    }
    if !structures.fat.is_empty() {
        patches.push((geometry.fat_offset, structures.fat.clone()));
        if !structures.root.is_empty() && geometry.fat_bytes > 0 {
            let copies = structures.fat.len() as u64 / geometry.fat_bytes;
            let root = geometry.fat_offset + geometry.fat_bytes * copies;
            // Only the fixed root of FAT12 and FAT16 is saved as bytes; a FAT32 root is a chain in the data area.
            if root < geometry.data_offset {
                patches.push((root, structures.root.clone()));
            }
        }
    }
    GenericFat::open_for_salvage(Overlay {
        source: Arc::clone(source),
        length: geometry.volume_size,
        patches,
    })
}

/// Every entry the driver finds, as the app's entries, keyed by where they are.
pub fn sets(
    generic: &GenericFat,
    geometry: &VolumeGeometry,
    fat: &[u32],
    fat_copies: u64,
) -> Result<Sets> {
    let root_start = geometry.fat_offset + geometry.fat_bytes * fat_copies;
    let mut sets = Sets::default();
    for candidate in generic.candidates()? {
        let view = generic.view(&candidate);
        let (cluster, offset) = if view.id >= geometry.data_offset {
            let into = view.id - geometry.data_offset;
            (
                fs_exfat::primitives::FIRST_CLUSTER + (into / geometry.cluster_size) as u32,
                (into % geometry.cluster_size) as usize,
            )
        } else if view.id >= root_start {
            (0, (view.id - root_start) as usize)
        } else {
            continue;
        };
        let attributes = view.detail["attributes"].as_u64().unwrap_or(0) as u16;
        let first_cluster = view.detail["firstCluster"].as_u64().unwrap_or(0) as u32;
        let size = view.detail["size"].as_u64().unwrap_or(0);
        let is_dir = view.kind == CandidateKind::Directory;
        let name = view.name.clone().unwrap_or_default();
        // What the app has always taken for an entry: not `.` or `..`, and a first cluster inside the volume unless the entry
        // is an empty file.
        if matches!(name.as_str(), "." | "..") {
            continue;
        }
        let cluster_valid = allocation::valid_cluster(geometry, first_cluster);
        if (!cluster_valid && (is_dir || size != 0 || first_cluster != 0))
            || size > geometry.volume_size
        {
            continue;
        }
        let length = if is_dir {
            allocation::clusters_for(
                geometry,
                fat,
                first_cluster,
                u64::from(geometry.cluster_count) * geometry.cluster_size,
                false,
            )
            .clusters
            .len() as u64
                * geometry.cluster_size
        } else {
            size
        };
        let weak = (view.integrity == Integrity::Unchecked).then(|| {
            if view.detail["loneEntry"].as_bool().unwrap_or(false) {
                "lone_entry".to_string()
            } else {
                "unchecked".to_string()
            }
        });
        // A file's clusters are the driver's: its walk of the storage the entry claims. An entry whose chain the FAT cleared (a
        // deleted one) is walked as one run from its first cluster, which is a guess and is said to be.
        let walk = (!is_dir && size > 0)
            .then(|| {
                walk_of(
                    generic,
                    &candidate,
                    geometry,
                    view.state == CandidateState::Deleted,
                )
            })
            .flatten();
        sets.by_cluster
            .entry(cluster)
            .or_default()
            .push(SalvageEntry {
                offset,
                name,
                in_use: view.state != CandidateState::Deleted,
                checksum_ok: view.integrity != Integrity::Failed,
                attributes,
                is_dir,
                first_cluster,
                length,
                valid_length: length,
                no_fat_chain: false,
                created: view.detail["created"].as_str().map(str::to_string),
                modified: view.detail["modified"].as_str().map(str::to_string),
                accessed: view.detail["accessed"].as_str().map(str::to_string),
                slots: 1,
                ntfs: None,
                ranges: RangeList::for_cluster_stream(
                    geometry,
                    fat,
                    first_cluster,
                    length,
                    length,
                    false,
                ),
                name_slot: 0,
                weak,
                walk,
                driver: None,
            });
    }
    for entries in sets.by_cluster.values_mut() {
        entries.sort_by_key(|entry| entry.offset);
    }
    Ok(sets)
}

/// The clusters the driver's walk of a candidate claims, as runs. `None` when the driver could not walk it at all, and then the
/// engine follows the FAT itself.
fn walk_of(
    generic: &GenericFat,
    candidate: &fs_core_layers::layers::Candidate<fs_fat32::layers::FatDetail>,
    geometry: &VolumeGeometry,
    guessed: bool,
) -> Option<FatWalk> {
    let walk = generic.extents(candidate).ok()?;
    let mut extents: Vec<(u32, u32)> = Vec::new();
    for extent in &walk.extents {
        if extent.source.offset < geometry.data_offset || extent.source.length == 0 {
            continue;
        }
        let into = extent.source.offset - geometry.data_offset;
        let first = fs_exfat::primitives::FIRST_CLUSTER + (into / geometry.cluster_size) as u32;
        let count = extent.source.length.div_ceil(geometry.cluster_size) as u32;
        match extents.last_mut() {
            Some((from, run)) if *from + *run == first => *run += count,
            _ => extents.push((first, count)),
        }
    }
    // Only what comes after the first cluster is a guess, so a file of one cluster has nothing guessed.
    let clusters: u32 = extents.iter().map(|(_, count)| count).sum();
    Some(FatWalk {
        guessed: guessed && clusters > 1,
        extents,
    })
}

/// Whether the driver's findings include damage, as sentences.
pub fn damage(generic: &GenericFat) -> Vec<String> {
    let mut sentences = Vec::new();
    if let Ok((found, _)) = generic.damage() {
        for finding in found {
            sentences.push(format!(
                "FAT structure damage found by the driver: {}: {}",
                finding.structure, finding.detail
            ));
        }
    }
    sentences
}
