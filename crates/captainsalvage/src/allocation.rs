use crate::filesystem::VolumeGeometry;
use fs_exfat::primitives::{FAT_BAD, FAT_END, FIRST_CLUSTER};
use serde::Serialize;
use std::collections::HashSet;

pub const ALLOC_ALLOCATED: u8 = 1;
pub const ALLOC_FREE_FAT: u8 = 2;
pub const ALLOC_FREE_ZERO: u8 = 3;

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

/// The root directory's clusters, stopping before an invalid or repeated cluster.
pub fn root_chain(boot: &VolumeGeometry, fat: &[u32]) -> Vec<u32> {
    let last = boot.last_cluster_exclusive();
    let mut clusters = Vec::new();
    let mut current = boot.root_cluster;
    let mut seen = HashSet::new();
    while (FIRST_CLUSTER..last).contains(&current) && seen.insert(current) {
        clusters.push(current);
        current = fat[current as usize];
    }
    clusters
}

pub fn valid_cluster(boot: &VolumeGeometry, cluster: u32) -> bool {
    (FIRST_CLUSTER..boot.last_cluster_exclusive()).contains(&cluster)
}

/// Reads the raw allocation bit, without validating the cluster or bitmap length.
pub fn is_allocated(bitmap: &[u8], cluster: u32) -> bool {
    let index = (cluster - FIRST_CLUSTER) as usize;
    bitmap[index / 8] >> (index % 8) & 1 == 1
}

fn fat_in_use(boot: &VolumeGeometry, fat: &[u32], cluster: u32) -> bool {
    let value = fat[cluster as usize];
    value == FAT_END || valid_cluster(boot, value)
}

/// Non-zero FAT entries count here, including values outside the valid range.
pub fn allocation_class(fat: &[u32], bitmap: &[u8], cluster: u32) -> u8 {
    if is_allocated(bitmap, cluster) {
        ALLOC_ALLOCATED
    } else if fat[cluster as usize] != 0 {
        ALLOC_FREE_FAT
    } else {
        ALLOC_FREE_ZERO
    }
}

/// The byte offset in the cluster heap, without validating the cluster number.
pub fn cluster_offset(boot: &VolumeGeometry, cluster: u32) -> u64 {
    boot.heap_offset() + u64::from(cluster - FIRST_CLUSTER) * boot.cluster_size()
}

/// Clusters a stream claims. `clusters` is shorter than `needed` when the
/// chain ends early or the claim runs off the end of the volume.
pub fn clusters_for(
    boot: &VolumeGeometry,
    fat: &[u32],
    first: u32,
    length: u64,
    no_fat_chain: bool,
) -> StreamClusters {
    let needed = length.div_ceil(boot.cluster_size());
    if needed == 0 || !valid_cluster(boot, first) || needed > u64::from(boot.cluster_count) {
        return StreamClusters {
            clusters: Vec::new(),
            needed,
        };
    }
    if no_fat_chain {
        let end = (u64::from(first) + needed).min(u64::from(boot.last_cluster_exclusive()));
        return StreamClusters {
            clusters: (first..end as u32).collect(),
            needed,
        };
    }
    let mut clusters = Vec::with_capacity(needed.min(1024) as usize);
    let mut seen = HashSet::new();
    let mut current = first;
    while valid_cluster(boot, current) && (clusters.len() as u64) < needed && seen.insert(current) {
        clusters.push(current);
        current = fat[current as usize];
    }
    StreamClusters { clusters, needed }
}

/// One FAT entry that points at this cluster, if any, from a precomputed index.
pub fn predecessor(predecessors: &[u32], cluster: u32) -> Option<u32> {
    match predecessors[cluster as usize] {
        0 => None,
        value => Some(value),
    }
}

/// Walks the FAT backward to a head or the first repeated cluster in a cycle.
pub fn chain_head(predecessors: &[u32], cluster: u32) -> u32 {
    let mut seen = HashSet::new();
    let mut current = cluster;
    while seen.insert(current) {
        match predecessor(predecessors, current) {
            Some(previous) => current = previous,
            None => break,
        }
    }
    current
}

/// Builds a predecessor index, choosing the lowest source when links converge.
pub fn compute_predecessors(boot: &VolumeGeometry, fat: &[u32]) -> Vec<u32> {
    let last = boot.last_cluster_exclusive();
    let mut predecessors = vec![0u32; last as usize];
    for cluster in FIRST_CLUSTER..last {
        let value = fat[cluster as usize];
        if valid_cluster(boot, value) && predecessors[value as usize] == 0 {
            predecessors[value as usize] = cluster;
        }
    }
    predecessors
}

/// Counts allocation classes and cross-link targets, grouping only headed islands.
pub fn compute_stats(boot: &VolumeGeometry, fat: &[u32], bitmap: &[u8]) -> AllocationStats {
    let last = boot.last_cluster_exclusive();
    let mut stats = AllocationStats::default();
    let mut targeted = vec![0u8; last as usize];
    for cluster in FIRST_CLUSTER..last {
        match allocation_class(fat, bitmap, cluster) {
            ALLOC_ALLOCATED => stats.allocated += 1,
            ALLOC_FREE_FAT => stats.free_with_fat += 1,
            _ => stats.free_zero_fat += 1,
        }
        let value = fat[cluster as usize];
        if valid_cluster(boot, value) {
            targeted[value as usize] = targeted[value as usize].saturating_add(1);
        }
    }
    stats.cross_links = targeted.iter().filter(|&&count| count > 1).count() as u64;
    for island in walk_islands(boot, fat, bitmap, &targeted) {
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
pub fn chain_islands(boot: &VolumeGeometry, fat: &[u32], bitmap: &[u8]) -> Vec<Island> {
    let last = boot.last_cluster_exclusive();
    let mut targeted = vec![0u8; last as usize];
    for cluster in FIRST_CLUSTER..last {
        let value = fat[cluster as usize];
        if valid_cluster(boot, value) {
            targeted[value as usize] = targeted[value as usize].saturating_add(1);
        }
    }
    walk_islands(boot, fat, bitmap, &targeted)
}

fn walk_islands(boot: &VolumeGeometry, fat: &[u32], bitmap: &[u8], targeted: &[u8]) -> Vec<Island> {
    let last = boot.last_cluster_exclusive();
    let mut visited = vec![false; last as usize];
    let mut islands = Vec::new();
    for head in FIRST_CLUSTER..last {
        if !fat_in_use(boot, fat, head) || targeted[head as usize] != 0 {
            continue;
        }
        let mut island = Island {
            head,
            tail: head,
            length: 0,
            allocated: 0,
            end: ISLAND_END_JOINS_ANOTHER,
        };
        let mut current = head;
        while valid_cluster(boot, current) && !visited[current as usize] {
            visited[current as usize] = true;
            island.length += 1;
            island.tail = current;
            if is_allocated(bitmap, current) {
                island.allocated += 1;
            }
            let value = fat[current as usize];
            if value == FAT_END {
                island.end = ISLAND_END_EOF;
                break;
            }
            if value == 0 || value == FAT_BAD {
                island.end = ISLAND_END_BROKEN;
                break;
            }
            if !valid_cluster(boot, value) {
                island.end = ISLAND_END_BROKEN;
                break;
            }
            current = value;
        }
        islands.push(island);
    }
    islands
}
