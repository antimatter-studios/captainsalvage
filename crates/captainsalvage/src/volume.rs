//! A volume's boot regions, FAT and bitmap, loaded into memory for inspection.

use crate::allocation as primitives;
use crate::changes::StructureCopy;
use crate::filesystem::{salvage, FilesystemDetails, FilesystemKind, SalvageEntry, VolumeGeometry};
use crate::source::BlockSource;
use anyhow::{Context, Result};
use fs_exfat::primitives::Limits;
use std::sync::Arc;

pub use crate::allocation::{
    AllocationStats, ChainGroup, Island, StreamClusters, ALLOC_ALLOCATED, ALLOC_FREE_FAT,
    ALLOC_FREE_ZERO, ISLAND_END_BROKEN, ISLAND_END_EOF, ISLAND_END_JOINS_ANOTHER,
};
pub use crate::filesystem::salvage::FilesystemBoot;
pub use fs_exfat::primitives::BootCheck;

pub struct Volume {
    source: Arc<dyn BlockSource>,
    pub boot: FilesystemBoot,
    pub backup_boot: Option<FilesystemBoot>,
    pub boot_check: Option<BootCheck>,
    pub geometry: VolumeGeometry,
    pub kind: FilesystemKind,
    pub filesystem: FilesystemDetails,
    pub dirty_flag: bool,
    pub fat: Vec<u32>,
    pub bitmap: Vec<u8>,
    pub root_clusters: Vec<u32>,
    pub structure_hash: String,
    /// The bytes the structure hash covers, kept so a later change can be explained.
    pub structures: StructureCopy,
    pub stats: AllocationStats,
    predecessors: Vec<u32>,
}

impl Volume {
    pub fn open(source: Arc<dyn BlockSource>) -> Result<Self> {
        let structures = Self::read_structures(source.as_ref())?;
        Self::from_structures(source, structures)
    }

    /// Reads the boot regions, FAT, root directory and allocation bitmap from the disk.
    pub fn read_structures(source: &dyn BlockSource) -> Result<StructureCopy> {
        salvage::read_structures(source)
    }

    /// A volume whose structures come from `structures` rather than from the disk, as a saved
    /// copy does when the disk's own have been written over. File data is still read from
    /// `source`.
    pub fn from_structures(
        source: Arc<dyn BlockSource>,
        structures: StructureCopy,
    ) -> Result<Self> {
        let salvage::Inspected {
            boot,
            backup_boot,
            boot_check,
            geometry,
            kind,
            details,
            dirty_flag,
            fat,
            bitmap,
            root_clusters,
        } = salvage::inspect(&structures)?;
        let structure_hash = structures.hash();
        let mut volume = Self {
            source,
            boot,
            backup_boot,
            boot_check,
            geometry,
            kind,
            filesystem: details,
            dirty_flag,
            fat,
            bitmap,
            root_clusters,
            structure_hash,
            structures,
            stats: AllocationStats::default(),
            predecessors: Vec::new(),
        };
        volume.stats = volume.compute_stats();
        volume.predecessors = volume.compute_predecessors();
        Ok(volume)
    }

    pub fn source(&self) -> &Arc<dyn BlockSource> {
        &self.source
    }

    pub fn find_sets(&self, data: &[u8], start_limit: usize) -> Vec<SalvageEntry> {
        salvage::find_entries(self.kind, &self.geometry, &self.fat, data, start_limit)
    }

    pub fn fixed_root_entries(&self) -> Vec<(u32, SalvageEntry)> {
        if matches!(self.kind, FilesystemKind::Fat12 | FilesystemKind::Fat16) {
            self.find_sets(&self.structures.root, self.structures.root.len())
                .into_iter()
                .map(|entry| (0, entry))
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn limits(&self) -> Limits {
        Limits {
            cluster_count: self.geometry.cluster_count,
            cluster_size: self.geometry.cluster_size(),
        }
    }

    pub fn valid_cluster(&self, cluster: u32) -> bool {
        primitives::valid_cluster(&self.geometry, cluster)
    }

    pub fn is_allocated(&self, cluster: u32) -> bool {
        primitives::is_allocated(&self.bitmap, cluster)
    }

    /// Non-zero FAT entries count here, including values outside the valid range.
    pub fn allocation_class(&self, cluster: u32) -> u8 {
        primitives::allocation_class(&self.fat, &self.bitmap, cluster)
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        primitives::cluster_offset(&self.geometry, cluster)
    }

    pub fn read_bytes(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        self.source
            .read_exact_at(offset, &mut buffer)
            .with_context(|| format!("reading {length} bytes at {offset}"))?;
        Ok(buffer)
    }

    pub fn read_cluster(&self, cluster: u32) -> Result<Vec<u8>> {
        self.read_bytes(
            self.cluster_offset(cluster),
            self.geometry.cluster_size() as usize,
        )
    }

    /// Clusters a stream claims. `clusters` is shorter than `needed` when the
    /// chain ends early or the claim runs off the end of the volume.
    pub fn clusters_for(&self, first: u32, length: u64, no_fat_chain: bool) -> StreamClusters {
        primitives::clusters_for(&self.geometry, &self.fat, first, length, no_fat_chain)
    }

    /// Reads a stream's bytes, up to `max_bytes`, stopping at its valid length.
    pub fn read_stream(
        &self,
        first: u32,
        length: u64,
        valid_length: u64,
        no_fat_chain: bool,
        max_bytes: u64,
    ) -> Result<Vec<u8>> {
        let limit = length.min(valid_length).min(max_bytes);
        let stream = self.clusters_for(first, length, no_fat_chain);
        let cluster_size = self.geometry.cluster_size();
        let mut out = Vec::with_capacity(limit as usize);
        for cluster in stream.clusters {
            if out.len() as u64 >= limit {
                break;
            }
            let data = self.read_cluster(cluster)?;
            let take = (limit - out.len() as u64).min(cluster_size) as usize;
            out.extend_from_slice(&data[..take]);
        }
        Ok(out)
    }

    /// One FAT entry that points at this cluster, if any.
    pub fn predecessor(&self, cluster: u32) -> Option<u32> {
        primitives::predecessor(&self.predecessors, cluster)
    }

    /// Walks the FAT backward from `cluster` to the start of its chain.
    pub fn chain_head(&self, cluster: u32) -> u32 {
        primitives::chain_head(&self.predecessors, cluster)
    }

    fn compute_predecessors(&self) -> Vec<u32> {
        primitives::compute_predecessors(&self.geometry, &self.fat)
    }

    fn compute_stats(&self) -> AllocationStats {
        primitives::compute_stats(&self.geometry, &self.fat, &self.bitmap)
    }

    /// Splits the FAT into islands: runs that start at an entry nothing points
    /// to, and end where the chain stops or meets a cluster already walked.
    pub fn chain_islands(&self) -> Vec<Island> {
        primitives::chain_islands(&self.geometry, &self.fat, &self.bitmap)
    }
}

#[cfg(test)]
mod tests {
    use super::Volume;
    use crate::source::{BlockSource, MemorySource};
    use fs_exfat::fixtures::{build_volume, SECTOR};
    use fs_exfat::primitives::BootSector;
    use std::sync::Arc;

    fn volume() -> Volume {
        Volume::open(Arc::new(MemorySource::new(build_volume()))).expect("fixture volume")
    }

    fn check_boot_errors(backup: bool, context: &str) {
        let volume = volume();
        for bytes in [
            Vec::new(),
            vec![0; SECTOR],
            volume.structures.main[..SECTOR - 1].to_vec(),
        ] {
            let reason = match BootSector::parse(&bytes).expect_err("invalid boot sector") {
                fs_exfat::Error::Invalid(reason) => reason.to_string(),
                error => error.to_string(),
            };
            let mut structures = volume.structures.clone();
            if backup {
                structures.backup = bytes;
            } else {
                structures.main = bytes;
            }
            let error = Volume::from_structures(volume.source().clone(), structures)
                .err()
                .expect("saved boot error");
            assert_eq!(format!("{error:#}"), format!("{context}: {reason}"));
            assert_eq!(error.root_cause().to_string(), reason);
        }
    }

    #[test]
    fn saved_main_boot_errors_keep_app_context_and_driver_reason() {
        check_boot_errors(false, "main boot sector");
    }

    #[test]
    fn saved_backup_boot_errors_keep_app_context_and_driver_reason() {
        check_boot_errors(true, "backup boot sector");
    }

    #[test]
    fn saved_root_copy_must_match_the_driver_root_chain() {
        let volume = volume();
        let mut structures = volume.structures.clone();
        structures.root.pop();
        let error = Volume::from_structures(volume.source().clone(), structures)
            .err()
            .expect("truncated root copy");
        assert_eq!(
            error.to_string(),
            "the root directory copy does not match the FAT's root chain"
        );
    }

    #[test]
    fn structure_read_errors_keep_the_failed_region_context() {
        let volume = volume();
        for (bytes, context) in [
            (Vec::new(), "reading main boot region"),
            (volume.structures.main.clone(), "reading backup boot region"),
        ] {
            let source = MemorySource::new(bytes);
            let error = Volume::read_structures(&source).expect_err("short source");
            assert_eq!(error.to_string(), context);
            assert_eq!(source.read_stats().errors, 1);
        }
    }
}
