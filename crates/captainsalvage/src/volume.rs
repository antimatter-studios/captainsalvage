//! A volume's boot regions, FAT and bitmap, loaded into memory for inspection.

use crate::allocation as primitives;
use crate::changes::StructureCopy;
use crate::filesystem::ntfs_salvage;
use crate::filesystem::{
    salvage, Claims, FilesystemDetails, FilesystemKind, NtfsRun, SalvageEntry, VolumeGeometry,
};
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
    /// Reads NTFS streams through the driver. None for every other filesystem.
    pub(crate) ntfs: Option<ntfs_salvage::Access>,
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
        let ntfs = (kind == FilesystemKind::Ntfs).then(|| {
            ntfs_salvage::Access::new(
                source.clone(),
                &geometry,
                ntfs_salvage::mft_runs(&structures, &geometry),
            )
        });
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
            ntfs,
        };
        volume.stats = volume.compute_stats();
        volume.predecessors = volume.compute_predecessors();
        Ok(volume)
    }

    /// The name of the folder this volume's workspace lives in. A 32-bit serial would let two
    /// NTFS volumes share a workspace by chance, so NTFS uses its whole 64-bit serial.
    pub fn workspace_name(&self) -> String {
        match &self.filesystem {
            FilesystemDetails::Ntfs { serial, .. } => format!("ntfs-{serial}"),
            _ => format!("vol-{:08x}", self.geometry.volume_serial),
        }
    }

    /// What a content class is called on this volume.
    pub fn class_name(&self, class: u8) -> &'static str {
        crate::content::class_names(self.kind.classes_name_file_records())[usize::from(class)]
    }

    /// The volume's serial number as text: eight hex digits, or sixteen for NTFS.
    pub fn serial_text(&self) -> String {
        match &self.filesystem {
            FilesystemDetails::Ntfs { serial, .. } => serial.clone(),
            _ => format!("{:08x}", self.geometry.volume_serial),
        }
    }

    /// How this volume's entries say which clusters they hold.
    pub fn claims(&self) -> Claims {
        self.kind.claims()
    }

    pub fn source(&self) -> &Arc<dyn BlockSource> {
        &self.source
    }

    pub fn find_sets(&self, data: &[u8], start_limit: usize) -> Vec<SalvageEntry> {
        salvage::find_entries(self.kind, &self.geometry, &self.fat, data, start_limit)
    }

    pub fn fixed_root_entries(&self) -> Vec<(u32, SalvageEntry)> {
        if self.kind.has_fixed_root() {
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
        if !self.claims().has_links() {
            // There are no links to be left behind by a cleared allocation, only the bitmap.
            return if primitives::is_allocated(&self.bitmap, cluster) {
                primitives::ALLOC_ALLOCATED
            } else {
                primitives::ALLOC_FREE_ZERO
            };
        }
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
        // NTFS has no chain to follow: a guess from a first cluster is a contiguous one.
        let contiguous = no_fat_chain || !self.claims().has_links();
        primitives::clusters_for(&self.geometry, &self.fat, first, length, contiguous)
    }

    /// Clusters an NTFS stream's run list claims, in order. A sparse hole claims nothing, so
    /// it is not counted as needed; a run list that stops early leaves `needed` larger than
    /// the clusters returned, as a broken FAT chain does.
    pub fn clusters_from_runs(&self, runs: &[NtfsRun], length: u64) -> StreamClusters {
        let size = self.geometry.cluster_size();
        let total = length.div_ceil(size);
        let mut clusters = Vec::new();
        let mut sparse = 0u64;
        for run in runs {
            if run.vcn >= total {
                break;
            }
            let count = run.count.min(total - run.vcn);
            match run.lcn {
                None => sparse += count,
                Some(lcn) => {
                    for index in 0..count {
                        match lcn
                            .checked_add(index)
                            .and_then(|cluster| u32::try_from(cluster).ok())
                            .filter(|&cluster| self.valid_cluster(cluster))
                        {
                            Some(cluster) => clusters.push(cluster),
                            None => break,
                        }
                    }
                }
            }
        }
        StreamClusters {
            clusters,
            needed: total - sparse.min(total),
        }
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

    /// The allocation-table value for a cluster. NTFS keeps no table of links, so it is zero.
    pub fn fat_value(&self, cluster: u32) -> u32 {
        self.fat.get(cluster as usize).copied().unwrap_or(0)
    }

    /// One FAT entry that points at this cluster, if any.
    pub fn predecessor(&self, cluster: u32) -> Option<u32> {
        if !self.claims().has_links() {
            return None;
        }
        primitives::predecessor(&self.predecessors, cluster)
    }

    /// Walks the FAT backward from `cluster` to the start of its chain.
    pub fn chain_head(&self, cluster: u32) -> u32 {
        if !self.claims().has_links() {
            return cluster;
        }
        primitives::chain_head(&self.predecessors, cluster)
    }

    fn compute_predecessors(&self) -> Vec<u32> {
        if !self.claims().has_links() {
            return Vec::new();
        }
        primitives::compute_predecessors(&self.geometry, &self.fat)
    }

    fn compute_stats(&self) -> AllocationStats {
        if !self.claims().has_links() {
            let mut stats = AllocationStats::default();
            for cluster in 2..self.geometry.last_cluster_exclusive() {
                if primitives::is_allocated(&self.bitmap, cluster) {
                    stats.allocated += 1;
                } else {
                    stats.free_zero_fat += 1;
                }
            }
            return stats;
        }
        primitives::compute_stats(&self.geometry, &self.fat, &self.bitmap)
    }

    /// Splits the FAT into islands: runs that start at an entry nothing points
    /// to, and end where the chain stops or meets a cluster already walked.
    pub fn chain_islands(&self) -> Vec<Island> {
        if !self.claims().has_links() {
            // Chains of links exist only where there is a table of links.
            return Vec::new();
        }
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
