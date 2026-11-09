//! A volume's boot regions, FAT and bitmap, loaded into memory for inspection.

use crate::changes::StructureCopy;
use crate::exfat::{parse_boot_sector, BootSector, Limits, BOOT_REGION_SECTORS};
use crate::source::BlockSource;
use anyhow::{bail, Context, Result};
use fs_exfat::primitives;
use std::sync::Arc;

pub use fs_exfat::primitives::{
    AllocationStats, BootCheck, ChainGroup, Island, StreamClusters, ALLOC_ALLOCATED,
    ALLOC_FREE_FAT, ALLOC_FREE_ZERO, ISLAND_END_BROKEN, ISLAND_END_EOF, ISLAND_END_JOINS_ANOTHER,
};

const BOOT_SECTOR_ASSUMED: u64 = 512;

pub struct Volume {
    source: Arc<dyn BlockSource>,
    pub boot: BootSector,
    pub backup_boot: BootSector,
    pub boot_check: BootCheck,
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
        let region_bytes = (BOOT_REGION_SECTORS * BOOT_SECTOR_ASSUMED) as usize;
        let mut main = vec![0u8; region_bytes];
        let mut backup = vec![0u8; region_bytes];
        source
            .read_exact_at(0, &mut main)
            .context("reading main boot region")?;
        source
            .read_exact_at(region_bytes as u64, &mut backup)
            .context("reading backup boot region")?;
        let boot = parse_boot_sector(&main).context("main boot sector")?;
        if boot.bytes_per_sector() != BOOT_SECTOR_ASSUMED {
            bail!("unsupported sector size {}", boot.bytes_per_sector());
        }

        let fat_bytes = u64::from(boot.fat_length) * boot.bytes_per_sector();
        let mut fat = vec![0u8; fat_bytes as usize];
        source
            .read_exact_at(
                u64::from(boot.fat_offset) * boot.bytes_per_sector(),
                &mut fat,
            )
            .context("reading FAT")?;

        let root_clusters = primitives::root_chain(&boot, &primitives::decode_fat(&fat, &boot));
        let cluster_size = boot.cluster_size();
        let mut root = vec![0u8; root_clusters.len() * cluster_size as usize];
        for (index, &cluster) in root_clusters.iter().enumerate() {
            let start = index * cluster_size as usize;
            let offset = primitives::cluster_offset(&boot, cluster);
            source
                .read_exact_at(offset, &mut root[start..start + cluster_size as usize])
                .context("reading root directory")?;
        }

        let (bitmap_cluster, bitmap_length) = primitives::find_bitmap_entry(&root)
            .context("root directory has no allocation bitmap entry")?;
        let needed_bitmap = u64::from(boot.cluster_count).div_ceil(8);
        if bitmap_length < needed_bitmap {
            bail!("allocation bitmap holds {bitmap_length} bytes, volume needs {needed_bitmap}");
        }
        let mut bitmap = vec![0u8; bitmap_length as usize];
        source
            .read_exact_at(
                primitives::cluster_offset(&boot, bitmap_cluster),
                &mut bitmap,
            )
            .context("reading allocation bitmap")?;
        Ok(StructureCopy {
            main,
            backup,
            fat,
            bitmap,
            root,
        })
    }

    /// A volume whose structures come from `structures` rather than from the disk, as a saved
    /// copy does when the disk's own have been written over. File data is still read from
    /// `source`.
    pub fn from_structures(
        source: Arc<dyn BlockSource>,
        structures: StructureCopy,
    ) -> Result<Self> {
        let boot = parse_boot_sector(&structures.main).context("main boot sector")?;
        let backup_boot = parse_boot_sector(&structures.backup).context("backup boot sector")?;
        if boot.bytes_per_sector() != BOOT_SECTOR_ASSUMED {
            bail!("unsupported sector size {}", boot.bytes_per_sector());
        }
        let sector = boot.bytes_per_sector() as usize;
        let boot_check = primitives::boot_check(&structures.main, &structures.backup, sector);
        let fat = primitives::decode_fat(&structures.fat, &boot);
        let root_clusters = primitives::root_chain(&boot, &fat);
        if structures.root.len() as u64 != root_clusters.len() as u64 * boot.cluster_size() {
            bail!("the root directory copy does not match the FAT's root chain");
        }
        let structure_hash = structures.hash();
        let mut volume = Self {
            source,
            boot,
            backup_boot,
            boot_check,
            fat,
            bitmap: structures.bitmap.clone(),
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

    pub fn limits(&self) -> Limits {
        Limits {
            cluster_count: self.boot.cluster_count,
            cluster_size: self.boot.cluster_size(),
        }
    }

    pub fn valid_cluster(&self, cluster: u32) -> bool {
        primitives::valid_cluster(&self.boot, cluster)
    }

    pub fn is_allocated(&self, cluster: u32) -> bool {
        primitives::is_allocated(&self.bitmap, cluster)
    }

    /// Non-zero FAT entries count here, including values outside the valid range.
    pub fn allocation_class(&self, cluster: u32) -> u8 {
        primitives::allocation_class(&self.fat, &self.bitmap, cluster)
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        primitives::cluster_offset(&self.boot, cluster)
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
            self.boot.cluster_size() as usize,
        )
    }

    /// Clusters a stream claims. `clusters` is shorter than `needed` when the
    /// chain ends early or the claim runs off the end of the volume.
    pub fn clusters_for(&self, first: u32, length: u64, no_fat_chain: bool) -> StreamClusters {
        primitives::clusters_for(&self.boot, &self.fat, first, length, no_fat_chain)
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
        let cluster_size = self.boot.cluster_size();
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
        primitives::compute_predecessors(&self.boot, &self.fat)
    }

    fn compute_stats(&self) -> AllocationStats {
        primitives::compute_stats(&self.boot, &self.fat, &self.bitmap)
    }

    /// Splits the FAT into islands: runs that start at an entry nothing points
    /// to, and end where the chain stops or meets a cluster already walked.
    pub fn chain_islands(&self) -> Vec<Island> {
        primitives::chain_islands(&self.boot, &self.fat, &self.bitmap)
    }
}
