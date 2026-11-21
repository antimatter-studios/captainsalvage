//! A volume's boot regions, FAT and bitmap, loaded into memory for inspection.

use crate::allocation as primitives;
use crate::changes::StructureCopy;
use crate::filesystem::drivers;
use crate::filesystem::fat_generic::{self, FatBackend, GenericFat};
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
    ALLOC_FREE_ZERO, ALLOC_UNKNOWN, ISLAND_END_BROKEN, ISLAND_END_EOF, ISLAND_END_JOINS_ANOTHER,
};
pub use crate::filesystem::salvage::FilesystemBoot;
pub use fs_exfat::primitives::BootCheck;

/// Where a volume's knowledge of which clusters are free comes from. The tools that look for what is left in free space
/// (carving, the unexplained regions) ask this, and say which it was, so unknown is never presented as free.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AllocationSource {
    /// Read from the volume's own bitmap or table.
    Known,
    /// Rebuilt from the file records, because the bitmap could not be read. It is as complete as the records are, and its
    /// limitations are said whenever it is used.
    DerivedFromRecords { limitations: Vec<String> },
    /// Not known, and not rebuilt, or rebuilt incompletely: the whole volume is searched, with a warning.
    Unknown { why_not_derived: Option<String> },
}

/// Allocation rebuilt from the file records of a volume whose bitmap could not be read. It is never used to call a cluster
/// free anywhere but in the search of free space, which it narrows from the whole volume.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DerivedAllocation {
    /// One bit for each cluster, in the numbering of `Volume::bitmap` (bit 0 is cluster 2): set where a record says the
    /// cluster is in use.
    pub allocated: Vec<u8>,
    /// Whether every record could be read. An incomplete rebuild is not used: the whole volume is searched instead.
    pub complete: bool,
    /// What the rebuild cannot see, in words (an extension record that was not read, an attribute list), shown with it.
    pub limitations: Vec<String>,
}

pub struct Volume {
    source: Arc<dyn BlockSource>,
    /// The FAT driver's volume, through the generic layers adapter, when that is the one in use.
    generic_fat: Option<GenericFat>,
    /// The driver's volume, through the registry's generic access, when this is a filesystem that is read that way.
    driver: Option<Box<dyn drivers::DriverAccess>>,
    /// What the generic adapter finds, worked out once, the first time the scan asks. `None` once it has failed.
    generic_sets: std::sync::OnceLock<Option<fat_generic::Sets>>,
    pub boot: FilesystemBoot,
    pub backup_boot: Option<FilesystemBoot>,
    pub boot_check: Option<BootCheck>,
    pub geometry: VolumeGeometry,
    pub kind: FilesystemKind,
    pub filesystem: FilesystemDetails,
    pub dirty_flag: bool,
    /// False when the volume's allocation bitmap could not be read (an NTFS volume with a damaged `$Bitmap`). Then no cluster
    /// is known to be free or in use, `bitmap` says nothing, and nothing may call a cluster free.
    pub allocation_known: bool,
    /// What is wrong with the volume's own structures, in words, for the person to be told.
    pub damage: Vec<String>,
    /// Allocation rebuilt from the file records, when the volume's own is unknown and a rebuild was made. Nothing makes one
    /// yet: it plugs in here, from the NTFS driver's `allocation_from_records`, through `with_derived_allocation`.
    derived_allocation: Option<DerivedAllocation>,
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

    /// As `open`, with the adapter of FAT entries named instead of asked of the environment.
    pub fn open_with(source: Arc<dyn BlockSource>, fat_backend: FatBackend) -> Result<Self> {
        let structures = Self::read_structures(source.as_ref())?;
        Self::from_structures_with(source, structures, fat_backend)
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
        Self::from_structures_with(source, structures, FatBackend::requested())
    }

    pub fn from_structures_with(
        source: Arc<dyn BlockSource>,
        structures: StructureCopy,
        fat_backend: FatBackend,
    ) -> Result<Self> {
        let salvage::Inspected {
            boot,
            backup_boot,
            boot_check,
            geometry,
            kind,
            details,
            dirty_flag,
            allocation_known,
            damage,
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
        let mut damage = damage;
        // A FAT volume is read through the generic layers adapter unless the old one is asked for. When the driver does not open
        // the volume, the old adapter reads it, and the person is told.
        let mut generic_fat = None;
        if kind.is_fat_family() {
            match fat_backend {
                FatBackend::Legacy => {}
                FatBackend::Generic => match fat_generic::open(&source, &geometry, &structures) {
                    Ok(opened) => {
                        damage.extend(fat_generic::damage(&opened));
                        generic_fat = Some(opened);
                    }
                    Err(error) => {
                        damage.push(format!(
                            "The FAT driver did not open this volume ({error:#}), so the old FAT adapter reads it."
                        ));
                    }
                },
            }
        }
        // A driver's volume is read through the registry's access, over the saved structures. When the driver does not open
        // it, the volume is still opened (it says what it is and what is wrong), and nothing is listed.
        let mut driver = None;
        if let Some(registered) = drivers::of_kind(kind) {
            match drivers::open(&source, &geometry, &structures, registered) {
                Ok(opened) => {
                    damage.extend(drivers::findings(opened.as_ref()));
                    driver = Some(opened);
                }
                Err(error) => damage.push(format!(
                    "The {} driver did not open this volume ({error:#}), so nothing in it is listed.",
                    registered.format
                )),
            }
        }
        let mut volume = Self {
            source,
            driver,
            generic_fat,
            generic_sets: std::sync::OnceLock::new(),
            boot,
            backup_boot,
            boot_check,
            geometry,
            kind,
            filesystem: details,
            dirty_flag,
            allocation_known,
            damage,
            derived_allocation: None,
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
            // A driver's volume has no serial of ours: it is known by what its saved structures hash to.
            FilesystemDetails::Driver { format, .. } => {
                format!("{format}-{}", &self.structure_hash[..16])
            }
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
            FilesystemDetails::Driver { serial, .. } => serial
                .clone()
                .unwrap_or_else(|| self.structure_hash[..8].to_string()),
            _ => format!("{:08x}", self.geometry.volume_serial),
        }
    }

    /// How this volume's entries say which clusters they hold.
    pub fn claims(&self) -> Claims {
        self.kind.claims()
    }

    /// Reads this volume through `driver` instead of the one its saved structures open: for the tests of the engine that bring a
    /// driver of their own, which the registry has no entry for.
    pub fn with_driver(mut self, driver: Box<dyn drivers::DriverAccess>) -> Self {
        self.driver = Some(driver);
        self
    }

    /// The driver's volume, for a filesystem that is read through the registry (`filesystem::drivers`), when it opened.
    pub fn driver(&self) -> Option<&dyn drivers::DriverAccess> {
        self.driver.as_deref()
    }

    pub fn source(&self) -> &Arc<dyn BlockSource> {
        &self.source
    }

    /// The ranges of this volume (`(start, end)` in its own bytes) that the health map of its source
    /// says must not be read: suspect, unreadable and reset-culprit. Empty for a source with no health
    /// map, which is every source in the command-line tools and the tests that do not make one.
    pub fn bad_sectors(&self) -> Vec<(u64, u64)> {
        self.source
            .bad_ranges()
            .into_iter()
            .filter(|range| range.state.blocks())
            .map(|range| (range.start, range.end))
            .collect()
    }

    /// The entries found in `data`, with the bytes of each that lie in a bad sector of the source marked
    /// as such in its typed ranges.
    pub fn find_sets(&self, data: &[u8], start_limit: usize) -> Vec<SalvageEntry> {
        let mut entries =
            salvage::find_entries(self.kind, &self.geometry, &self.fat, data, start_limit);
        let bad = self.bad_sectors();
        if !bad.is_empty() {
            for entry in &mut entries {
                entry.ranges.mark_bad(&bad);
            }
        }
        entries
    }

    /// Which adapter reads this volume's FAT entries: `generic`, or `legacy` (always for exFAT and NTFS).
    pub fn fat_adapter(&self) -> FatBackend {
        if self.generic_fat.is_some() {
            FatBackend::Generic
        } else {
            FatBackend::Legacy
        }
    }

    /// What the generic adapter finds, worked out once. `None` when it is not the adapter in use, or has failed (the old one
    /// then reads the volume).
    fn generic_sets(&self) -> Option<&fat_generic::Sets> {
        let generic = self.generic_fat.as_ref()?;
        self.generic_sets
            .get_or_init(|| {
                let copies = self.structures.fat.len() as u64 / self.geometry.fat_bytes.max(1);
                fat_generic::sets(generic, &self.geometry, &self.fat, copies).ok()
            })
            .as_ref()
    }

    /// The entries that start in this cluster of the scan, `data` being its bytes: what the generic adapter found there, or, for
    /// every other filesystem and when the old adapter is asked for, what `find_sets` finds in `data`.
    pub fn sets_in_cluster(
        &self,
        cluster: u32,
        data: &[u8],
        start_limit: usize,
    ) -> Vec<SalvageEntry> {
        if let Some(driver) = self.driver.as_deref() {
            return self.marked_bad(drivers::entries_in_cluster(driver, &self.geometry, cluster));
        }
        match self.generic_sets() {
            Some(sets) => self.marked_bad(sets.in_cluster(cluster)),
            None => self.find_sets(data, start_limit),
        }
    }

    fn marked_bad(&self, mut entries: Vec<SalvageEntry>) -> Vec<SalvageEntry> {
        let bad = self.bad_sectors();
        if !bad.is_empty() {
            for entry in &mut entries {
                entry.ranges.mark_bad(&bad);
            }
        }
        entries
    }

    pub fn fixed_root_entries(&self) -> Vec<(u32, SalvageEntry)> {
        if self.kind.has_fixed_root() {
            if let Some(sets) = self.generic_sets() {
                return self
                    .marked_bad(sets.in_cluster(0))
                    .into_iter()
                    .map(|entry| (0, entry))
                    .collect();
            }
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

    /// Whether the bitmap says this cluster is in use. False for a cluster of a volume whose allocation is unknown, which is
    /// not known to be in use.
    pub fn is_allocated(&self, cluster: u32) -> bool {
        self.allocation_known && primitives::is_allocated(&self.bitmap, cluster)
    }

    /// Gives a volume whose allocation is unknown the allocation rebuilt from its file records. A volume whose own allocation is
    /// known keeps it.
    pub fn with_derived_allocation(mut self, derived: DerivedAllocation) -> Self {
        if !self.allocation_known {
            self.derived_allocation = Some(derived);
        }
        self
    }

    /// Where the knowledge of free space comes from now.
    pub fn allocation_source(&self) -> AllocationSource {
        if self.allocation_known {
            return AllocationSource::Known;
        }
        match &self.derived_allocation {
            Some(derived) if derived.complete => AllocationSource::DerivedFromRecords {
                limitations: derived.limitations.clone(),
            },
            Some(derived) => AllocationSource::Unknown {
                why_not_derived: Some(if derived.limitations.is_empty() {
                    "not every file record could be read".to_string()
                } else {
                    derived.limitations.join("; ")
                }),
            },
            None => AllocationSource::Unknown {
                why_not_derived: None,
            },
        }
    }

    /// Whether the tools that look for what is left in free space may look in this cluster: where the allocation is known,
    /// where it is free; where it was rebuilt from the file records, where no record says it is in use; and where it is
    /// unknown, everywhere. This is not `is_free`, which is false for every cluster of such a volume.
    pub fn is_searched_as_free(&self, cluster: u32) -> bool {
        match self.allocation_source() {
            AllocationSource::Known => self.is_free(cluster),
            AllocationSource::DerivedFromRecords { .. } => self
                .derived_allocation
                .as_ref()
                .is_some_and(|derived| !primitives::is_allocated(&derived.allocated, cluster)),
            AllocationSource::Unknown { .. } => true,
        }
    }

    /// What those tools say of where their free space came from, when it is not the volume's own bitmap. Nothing for a
    /// volume whose allocation is known, so their answers are what they were.
    pub fn free_space_warning(&self) -> Option<(&'static str, String)> {
        match self.allocation_source() {
            AllocationSource::Known => None,
            AllocationSource::DerivedFromRecords { limitations } => Some((
                "derived-from-records",
                format!(
                    "Allocation here was rebuilt from the file records, not read from a bitmap, so it is only as complete as they are: {}",
                    if limitations.is_empty() {
                        "no limitation was found".to_string()
                    } else {
                        limitations.join("; ")
                    }
                ),
            )),
            AllocationSource::Unknown { why_not_derived } => Some((
                "whole-volume",
                match why_not_derived {
                    None => "Allocation is unknown for this volume, so the whole volume is searched as if it were free space. Nothing here is known to be free: clusters that current files use may be listed.".to_string(),
                    Some(why) => format!(
                        "Allocation could not be rebuilt completely from the file records ({why}), so the whole volume is searched as if it were free space. Nothing here is known to be free: clusters that current files use may be listed."
                    ),
                },
            )),
        }
    }

    /// Whether the bitmap says this cluster is free. False for a cluster of a volume whose allocation is unknown: unknown is
    /// never free, so nothing is called free space, unclaimed or overwritten by it.
    pub fn is_free(&self, cluster: u32) -> bool {
        self.allocation_known && !primitives::is_allocated(&self.bitmap, cluster)
    }

    /// Non-zero FAT entries count here, including values outside the valid range. `ALLOC_UNKNOWN` for every cluster of a
    /// volume whose allocation is unknown.
    pub fn allocation_class(&self, cluster: u32) -> u8 {
        if !self.allocation_known {
            return primitives::ALLOC_UNKNOWN;
        }
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

    /// The clusters of a file as the FAT driver walked them, as runs of `(first cluster, count)`, cut to what its length needs.
    pub fn clusters_from_walk(&self, walk: &[(u32, u32)], length: u64) -> StreamClusters {
        let needed = length.div_ceil(self.geometry.cluster_size());
        let mut clusters = Vec::new();
        for &(first, count) in walk {
            for cluster in first..first.saturating_add(count) {
                if clusters.len() as u64 >= needed {
                    break;
                }
                if self.valid_cluster(cluster) {
                    clusters.push(cluster);
                }
            }
        }
        StreamClusters { clusters, needed }
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
        if !self.allocation_known {
            return AllocationStats {
                unknown: u64::from(self.geometry.cluster_count),
                ..AllocationStats::default()
            };
        }
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
