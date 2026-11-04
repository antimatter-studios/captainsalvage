//! A volume's boot regions, FAT and bitmap, loaded into memory for inspection.

use crate::exfat::{
    boot_checksum, find_bitmap_entry, stored_boot_checksum, BootSector, Limits, BOOT_REGION_SECTORS, FAT_BAD,
    FAT_END, FIRST_CLUSTER,
};
use crate::source::BlockSource;
use anyhow::{bail, Context, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::sync::Arc;

pub const ALLOC_ALLOCATED: u8 = 1;
pub const ALLOC_FREE_FAT: u8 = 2;
pub const ALLOC_FREE_ZERO: u8 = 3;

const BOOT_SECTOR_ASSUMED: u64 = 512;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChainGroup {
    pub chains: u64,
    pub clusters: u64,
    pub longest: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AllocationStats {
    pub allocated: u64,
    pub free_with_fat: u64,
    pub free_zero_fat: u64,
    pub cross_links: u64,
    pub free_chains: ChainGroup,
    pub allocated_chains: ChainGroup,
    pub mixed_chains: ChainGroup,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BootCheck {
    pub main_stored: u32,
    pub main_computed: u32,
    pub backup_stored: u32,
    pub backup_computed: u32,
    pub regions_identical: bool,
}

pub struct StreamClusters {
    pub clusters: Vec<u32>,
    pub needed: u64,
}

/// The chain ends with the FAT's end marker.
pub const ISLAND_END_EOF: &str = "eof";
/// The chain stops at a zero, bad or out-of-range FAT entry.
pub const ISLAND_END_BROKEN: &str = "broken";
/// The chain runs into a cluster already reached from another head.
pub const ISLAND_END_JOINS_ANOTHER: &str = "joins-another";

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Island {
    pub head: u32,
    pub tail: u32,
    pub length: u64,
    pub allocated: u64,
    pub end: &'static str,
}

pub struct Volume {
    source: Arc<dyn BlockSource>,
    pub boot: BootSector,
    pub backup_boot: BootSector,
    pub boot_check: BootCheck,
    pub fat: Vec<u32>,
    pub bitmap: Vec<u8>,
    pub root_clusters: Vec<u32>,
    pub structure_hash: String,
    pub stats: AllocationStats,
    predecessors: Vec<u32>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

impl Volume {
    pub fn open(source: Arc<dyn BlockSource>) -> Result<Self> {
        let region_bytes = (BOOT_REGION_SECTORS * BOOT_SECTOR_ASSUMED) as usize;
        let mut main_region = vec![0u8; region_bytes];
        let mut backup_region = vec![0u8; region_bytes];
        source.read_exact_at(0, &mut main_region).context("reading main boot region")?;
        source
            .read_exact_at(region_bytes as u64, &mut backup_region)
            .context("reading backup boot region")?;
        let boot = BootSector::parse(&main_region).context("main boot sector")?;
        let backup_boot = BootSector::parse(&backup_region).context("backup boot sector")?;
        if boot.bytes_per_sector() != BOOT_SECTOR_ASSUMED {
            bail!("unsupported sector size {}", boot.bytes_per_sector());
        }
        let sector = boot.bytes_per_sector() as usize;
        let boot_check = BootCheck {
            main_stored: stored_boot_checksum(&main_region, sector),
            main_computed: boot_checksum(&main_region, sector),
            backup_stored: stored_boot_checksum(&backup_region, sector),
            backup_computed: boot_checksum(&backup_region, sector),
            regions_identical: main_region == backup_region,
        };

        let fat_bytes = u64::from(boot.fat_length) * boot.bytes_per_sector();
        let mut raw_fat = vec![0u8; fat_bytes as usize];
        source
            .read_exact_at(u64::from(boot.fat_offset) * boot.bytes_per_sector(), &mut raw_fat)
            .context("reading FAT")?;
        let mut fat: Vec<u32> = raw_fat
            .chunks_exact(4)
            .map(|entry| u32::from_le_bytes(entry.try_into().expect("four bytes")))
            .collect();
        let last = boot.last_cluster_exclusive();
        if fat.len() < last as usize {
            fat.resize(last as usize, 0);
        }

        let mut root_clusters = Vec::new();
        let mut current = boot.root_cluster;
        let mut seen = HashSet::new();
        while (FIRST_CLUSTER..last).contains(&current) && seen.insert(current) {
            root_clusters.push(current);
            current = fat[current as usize];
        }
        let cluster_size = boot.cluster_size();
        let mut root = vec![0u8; root_clusters.len() * cluster_size as usize];
        for (index, &cluster) in root_clusters.iter().enumerate() {
            let start = index * cluster_size as usize;
            let offset = boot.heap_offset() + u64::from(cluster - FIRST_CLUSTER) * cluster_size;
            source
                .read_exact_at(offset, &mut root[start..start + cluster_size as usize])
                .context("reading root directory")?;
        }

        let (bitmap_cluster, bitmap_length) =
            find_bitmap_entry(&root).context("root directory has no allocation bitmap entry")?;
        let needed_bitmap = u64::from(boot.cluster_count).div_ceil(8);
        if bitmap_length < needed_bitmap {
            bail!("allocation bitmap holds {bitmap_length} bytes, volume needs {needed_bitmap}");
        }
        let mut bitmap = vec![0u8; bitmap_length as usize];
        source
            .read_exact_at(
                boot.heap_offset() + u64::from(bitmap_cluster - FIRST_CLUSTER) * cluster_size,
                &mut bitmap,
            )
            .context("reading allocation bitmap")?;

        let mut hasher = Sha256::new();
        hasher.update(&main_region);
        hasher.update(&backup_region);
        hasher.update(&raw_fat);
        hasher.update(&bitmap);
        hasher.update(&root);
        let structure_hash = hex(hasher.finalize().as_slice());

        let mut volume = Self {
            source,
            boot,
            backup_boot,
            boot_check,
            fat,
            bitmap,
            root_clusters,
            structure_hash,
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
        Limits { cluster_count: self.boot.cluster_count, cluster_size: self.boot.cluster_size() }
    }

    pub fn valid_cluster(&self, cluster: u32) -> bool {
        (FIRST_CLUSTER..self.boot.last_cluster_exclusive()).contains(&cluster)
    }

    pub fn is_allocated(&self, cluster: u32) -> bool {
        let index = (cluster - FIRST_CLUSTER) as usize;
        self.bitmap[index / 8] >> (index % 8) & 1 == 1
    }

    fn fat_in_use(&self, cluster: u32) -> bool {
        let value = self.fat[cluster as usize];
        value == FAT_END || self.valid_cluster(value)
    }

    /// Non-zero FAT entries count here, including values outside the valid range.
    pub fn allocation_class(&self, cluster: u32) -> u8 {
        if self.is_allocated(cluster) {
            ALLOC_ALLOCATED
        } else if self.fat[cluster as usize] != 0 {
            ALLOC_FREE_FAT
        } else {
            ALLOC_FREE_ZERO
        }
    }

    pub fn cluster_offset(&self, cluster: u32) -> u64 {
        self.boot.heap_offset() + u64::from(cluster - FIRST_CLUSTER) * self.boot.cluster_size()
    }

    pub fn read_bytes(&self, offset: u64, length: usize) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; length];
        self.source.read_exact_at(offset, &mut buffer).with_context(|| format!("reading {length} bytes at {offset}"))?;
        Ok(buffer)
    }

    pub fn read_cluster(&self, cluster: u32) -> Result<Vec<u8>> {
        self.read_bytes(self.cluster_offset(cluster), self.boot.cluster_size() as usize)
    }

    /// Clusters a stream claims. `clusters` is shorter than `needed` when the
    /// chain ends early or the claim runs off the end of the volume.
    pub fn clusters_for(&self, first: u32, length: u64, no_fat_chain: bool) -> StreamClusters {
        let needed = length.div_ceil(self.boot.cluster_size());
        if needed == 0 || !self.valid_cluster(first) || needed > u64::from(self.boot.cluster_count) {
            return StreamClusters { clusters: Vec::new(), needed };
        }
        if no_fat_chain {
            let end = (u64::from(first) + needed).min(u64::from(self.boot.last_cluster_exclusive()));
            return StreamClusters { clusters: (first..end as u32).collect(), needed };
        }
        let mut clusters = Vec::with_capacity(needed.min(1024) as usize);
        let mut seen = HashSet::new();
        let mut current = first;
        while self.valid_cluster(current) && (clusters.len() as u64) < needed && seen.insert(current) {
            clusters.push(current);
            current = self.fat[current as usize];
        }
        StreamClusters { clusters, needed }
    }

    /// Reads a stream's bytes, up to `max_bytes`, stopping at its valid length.
    pub fn read_stream(&self, first: u32, length: u64, valid_length: u64, no_fat_chain: bool, max_bytes: u64) -> Result<Vec<u8>> {
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
        match self.predecessors[cluster as usize] {
            0 => None,
            value => Some(value),
        }
    }

    /// Walks the FAT backward from `cluster` to the start of its chain.
    pub fn chain_head(&self, cluster: u32) -> u32 {
        let mut seen = HashSet::new();
        let mut current = cluster;
        while seen.insert(current) {
            match self.predecessor(current) {
                Some(previous) => current = previous,
                None => break,
            }
        }
        current
    }

    fn compute_predecessors(&self) -> Vec<u32> {
        let last = self.boot.last_cluster_exclusive();
        let mut predecessors = vec![0u32; last as usize];
        for cluster in FIRST_CLUSTER..last {
            let value = self.fat[cluster as usize];
            if self.valid_cluster(value) && predecessors[value as usize] == 0 {
                predecessors[value as usize] = cluster;
            }
        }
        predecessors
    }

    fn compute_stats(&self) -> AllocationStats {
        let last = self.boot.last_cluster_exclusive();
        let mut stats = AllocationStats::default();
        let mut targeted = vec![0u8; last as usize];
        for cluster in FIRST_CLUSTER..last {
            match self.allocation_class(cluster) {
                ALLOC_ALLOCATED => stats.allocated += 1,
                ALLOC_FREE_FAT => stats.free_with_fat += 1,
                _ => stats.free_zero_fat += 1,
            }
            let value = self.fat[cluster as usize];
            if self.valid_cluster(value) {
                targeted[value as usize] = targeted[value as usize].saturating_add(1);
            }
        }
        stats.cross_links = targeted.iter().filter(|&&count| count > 1).count() as u64;
        for island in self.walk_islands(&targeted) {
            let group = if island.allocated == 0 {
                &mut stats.free_chains
            } else if island.allocated == island.length {
                &mut stats.allocated_chains
            } else {
                &mut stats.mixed_chains
            };
            group.chains += 1;
            group.clusters += island.length;
            group.longest = group.longest.max(island.length);
        }
        stats
    }

    /// Splits the FAT into islands: runs that start at an entry nothing points
    /// to, and end where the chain stops or meets a cluster already walked.
    pub fn chain_islands(&self) -> Vec<Island> {
        let last = self.boot.last_cluster_exclusive();
        let mut targeted = vec![0u8; last as usize];
        for cluster in FIRST_CLUSTER..last {
            let value = self.fat[cluster as usize];
            if self.valid_cluster(value) {
                targeted[value as usize] = targeted[value as usize].saturating_add(1);
            }
        }
        self.walk_islands(&targeted)
    }

    fn walk_islands(&self, targeted: &[u8]) -> Vec<Island> {
        let last = self.boot.last_cluster_exclusive();
        let mut visited = vec![false; last as usize];
        let mut islands = Vec::new();
        for head in FIRST_CLUSTER..last {
            if !self.fat_in_use(head) || targeted[head as usize] != 0 {
                continue;
            }
            let mut island = Island { head, tail: head, length: 0, allocated: 0, end: ISLAND_END_JOINS_ANOTHER };
            let mut current = head;
            while self.valid_cluster(current) && !visited[current as usize] {
                visited[current as usize] = true;
                island.length += 1;
                island.tail = current;
                if self.is_allocated(current) {
                    island.allocated += 1;
                }
                let value = self.fat[current as usize];
                if value == FAT_END {
                    island.end = ISLAND_END_EOF;
                    break;
                }
                if value == 0 || value == FAT_BAD {
                    island.end = ISLAND_END_BROKEN;
                    break;
                }
                if !self.valid_cluster(value) {
                    island.end = ISLAND_END_BROKEN;
                    break;
                }
                current = value;
            }
            islands.push(island);
        }
        islands
    }
}
