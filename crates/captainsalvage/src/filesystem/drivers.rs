//! The registry of drivers the engine reads through rust-fs-core's layers, and the one object they are all read as.
//!
//! A driver that implements `OpenVolume`, `PrimitiveAPI` (with a `SalvageAdapter`) and `FileAPI` is added here with one
//! entry: the bytes its volumes begin with (to tell it from the others), its `probe`, how to `open` it, and what it
//! lacks (`Capabilities`). Nothing else in the engine names the format. It gets a `FilesystemKind` label, which is only a
//! label: the shared engine asks what a volume can do (`Claims::Driver`, `FilesystemDetails::Driver`, the derived
//! support rows), and never which driver it is.
//!
//! Every registered driver is read as a [`DriverAccess`], an object-safe view of `generic::GenericVolume` over the
//! driver, so the engine holds one type for any of them.

use super::fat_generic::Overlay;
use super::generic::{capability_rows, GenericVolume};
use super::salvage::{FilesystemBoot, Inspected};
use super::{
    Capability, DriverFacts, FilesystemDetails, FilesystemKind, NtfsEntry, SalvageEntry, Support,
    VolumeGeometry, STORAGE_DRIVER, STORAGE_DRIVER_PARTIAL,
};
use crate::changes::StructureCopy;
use crate::source::BlockSource;
use anyhow::{anyhow, bail, Result};
use fs_core_layers::layers::{
    Candidate, CandidateKind, CandidateState, Capabilities, Damage, Extent, ExtentWalk, FileAPI,
    Geometry, Integrity, OpenVolume, Parent, PrimitiveAPI, Probe, Recognition, SalvageAdapter,
    Structure, StructureRole, VolumeHealth, VolumeSupport,
};
use fs_core_layers::BlockRead;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::ops::ControlFlow;
use std::sync::Arc;

/// How a driver opens a volume over a source it is lent.
pub type OpenDriver = fn(Arc<dyn BlockRead>) -> Result<Box<dyn DriverAccess>>;

/// One driver in the registry.
pub struct RegisteredDriver {
    pub kind: FilesystemKind,
    /// The name the driver gives its format (`VolumeAPI::format`).
    pub format: &'static str,
    /// Bytes every volume of the format has, as `(offset, bytes)` in its first 512: enough to tell it from the others
    /// without opening it. The driver's own `probe` then says whether it is intact.
    pub magic: &'static [(usize, &'static [u8])],
    pub probe: fn(&dyn BlockRead) -> Result<Probe>,
    pub open: OpenDriver,
    /// What the driver's salvage adapter lacks, which the support rows are derived from.
    pub capabilities: fn() -> Capabilities,
}

/// The volumes of a driver, as the engine reads them: the geometry, what the driver says it supports, how healthy its
/// own structures are, where they are, and what is damaged.
/// A driver's candidate as the engine keeps it: the fields every format has, and the whole candidate as JSON, which the
/// driver is handed back to walk it, read it and place it. The engine never looks inside it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverRecord {
    /// The driver's `candidate_id`: what trees and parent claims name, unique among the candidates of one source.
    pub id: u64,
    /// The record bytes that describe the candidate.
    pub location: Extent,
    pub name: Option<String>,
    pub kind: CandidateKind,
    pub state: CandidateState,
    pub integrity: Integrity,
    /// The logical size the record claims.
    pub size: Option<u64>,
    pub candidate: String,
}

impl DriverRecord {
    /// A record rebuilt from what the map stored of it: the driver is given back the candidate, and reads nothing else.
    pub fn stored(id: u64, candidate: String) -> Self {
        Self {
            id,
            location: Extent::new(0, 0),
            name: None,
            kind: CandidateKind::Other,
            state: CandidateState::Live,
            integrity: Integrity::Unchecked,
            size: None,
            candidate,
        }
    }
}

/// What a parent claim names.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ParentRef {
    Root,
    Record(u64),
}

/// One bounded read of a candidate's bytes: the bytes, and the ranges of them (from the start of the read) that could not
/// be read, which are zeros in the bytes.
pub struct DriverRead {
    pub data: Vec<u8>,
    pub unreadable: Vec<Extent>,
}

pub trait DriverAccess: Send + Sync {
    fn format(&self) -> &'static str;
    fn geometry(&self) -> Result<Geometry>;
    fn support(&self) -> Result<VolumeSupport>;
    fn health(&self) -> Result<VolumeHealth>;
    fn structures(&self) -> Result<Vec<Structure>>;
    fn damage(&self) -> Result<Vec<Damage>>;
    /// Hands every candidate whose record starts inside `range` to `sink`, as the scan reads the source a cluster at a
    /// time. Nothing is held: the engine keeps a cluster's records, never the volume's.
    fn find(
        &self,
        range: Extent,
        sink: &mut dyn FnMut(DriverRecord) -> ControlFlow<()>,
    ) -> Result<()>;
    /// The storage a candidate claims, as far as its map could be followed.
    fn walk(&self, record: &DriverRecord) -> Result<ExtentWalk>;
    /// The parents each of `files` claims, found with `folders` (every folder of the volume) beside them, in the order of
    /// `files`. A candidate whose folder is not among the `folders` has no claim.
    fn parents(
        &self,
        folders: &[DriverRecord],
        files: &[DriverRecord],
    ) -> Result<Vec<Vec<ParentRef>>>;
    /// `length` bytes of what the walk rebuilds, from `start`, decoding what the driver stored compressed. A stretch whose
    /// source bytes cannot be read is zeros, and named.
    fn read(&self, walk: &ExtentWalk, start: u64, length: u64) -> Result<DriverRead>;
}

fn record_of<D: Serialize>(id: u64, candidate: &Candidate<D>) -> Result<DriverRecord> {
    Ok(DriverRecord {
        id,
        location: candidate.location,
        name: candidate.name.clone(),
        kind: candidate.kind,
        state: candidate.state,
        integrity: candidate.integrity,
        size: candidate.size,
        candidate: serde_json::to_string(candidate)?,
    })
}

fn candidate_of<D: DeserializeOwned>(record: &DriverRecord) -> Result<Candidate<D>> {
    Ok(serde_json::from_str(&record.candidate)?)
}

impl<V> DriverAccess for GenericVolume<V>
where
    V: OpenVolume + PrimitiveAPI + FileAPI + Send + Sync,
    <V as PrimitiveAPI>::Adapter: SalvageAdapter + Sized,
{
    fn format(&self) -> &'static str {
        GenericVolume::format(self)
    }
    fn geometry(&self) -> Result<Geometry> {
        GenericVolume::geometry(self)
    }
    fn support(&self) -> Result<VolumeSupport> {
        GenericVolume::support(self)
    }
    fn health(&self) -> Result<VolumeHealth> {
        GenericVolume::health(self)
    }
    fn structures(&self) -> Result<Vec<Structure>> {
        GenericVolume::structures(self)
    }
    fn damage(&self) -> Result<Vec<Damage>> {
        Ok(GenericVolume::damage(self)?.0)
    }
    fn find(
        &self,
        range: Extent,
        sink: &mut dyn FnMut(DriverRecord) -> ControlFlow<()>,
    ) -> Result<()> {
        let adapter = self.volume().adapter()?;
        let mut found: Vec<Candidate<<V::Adapter as SalvageAdapter>::Detail>> = Vec::new();
        self.volume()
            .salvage()?
            .find_candidates(range, &mut found)?;
        for candidate in &found {
            let record = record_of(adapter.candidate_id(candidate), candidate)?;
            if sink(record).is_break() {
                break;
            }
        }
        Ok(())
    }
    fn walk(&self, record: &DriverRecord) -> Result<ExtentWalk> {
        let candidate = candidate_of(record)?;
        GenericVolume::extents(self, &candidate)
    }
    fn parents(
        &self,
        folders: &[DriverRecord],
        files: &[DriverRecord],
    ) -> Result<Vec<Vec<ParentRef>>> {
        let adapter = self.volume().adapter()?;
        let mine: std::collections::HashSet<u64> = files.iter().map(|file| file.id).collect();
        let candidates: Vec<Candidate<<V::Adapter as SalvageAdapter>::Detail>> = files
            .iter()
            .chain(folders.iter().filter(|folder| !mine.contains(&folder.id)))
            .map(candidate_of)
            .collect::<Result<_>>()?;
        let tree = self.volume().salvage()?.rebuild_tree(&candidates)?;
        let claims: std::collections::HashMap<u64, Vec<ParentRef>> = tree
            .nodes
            .iter()
            .map(|node| {
                (
                    node.candidate,
                    node.parents
                        .iter()
                        .map(|claim| match claim.parent {
                            Parent::Root => ParentRef::Root,
                            Parent::Candidate(id) => ParentRef::Record(id),
                        })
                        .collect(),
                )
            })
            .collect();
        let _ = adapter;
        Ok(files
            .iter()
            .map(|file| claims.get(&file.id).cloned().unwrap_or_default())
            .collect())
    }
    fn read(&self, walk: &ExtentWalk, start: u64, length: u64) -> Result<DriverRead> {
        let adapter = self.volume().adapter()?;
        let plan = walk.plan();
        let reader = adapter.plan_reader(self.volume().source(), &plan)?;
        let end = start.saturating_add(length).min(reader.len());
        if start >= end {
            return Ok(DriverRead {
                data: Vec::new(),
                unreadable: Vec::new(),
            });
        }
        let mut data = vec![0u8; usize::try_from(end - start)?];
        let read = reader.read_at(start, &mut data)?;
        data.truncate(usize::try_from(read.length)?);
        Ok(DriverRead {
            data,
            unreadable: read
                .unreadable()
                .map(|range| Extent::new(range.offset - start, range.length))
                .collect(),
        })
    }
}

fn probe_as<V: OpenVolume>(source: &dyn BlockRead) -> Result<Probe> {
    V::probe(source).map_err(|error| anyhow!("{error}"))
}

fn open_as<V>(source: Arc<dyn BlockRead>) -> Result<Box<dyn DriverAccess>>
where
    V: OpenVolume<Source = Arc<dyn BlockRead>> + PrimitiveAPI + FileAPI + Send + Sync + 'static,
    <V as PrimitiveAPI>::Adapter: SalvageAdapter + Sized,
{
    Ok(Box::new(GenericVolume::<V>::open_for_salvage(source)?))
}

const SQUASHFS: RegisteredDriver = RegisteredDriver {
    kind: FilesystemKind::Squashfs,
    format: "squashfs",
    magic: &[(0, b"hsqs")],
    probe: probe_as::<fs_squashfs::layers::SquashfsVolume>,
    open: open_as::<fs_squashfs::layers::SquashfsVolume>,
    capabilities: || {
        fs_squashfs::layers::SquashfsAdapter::new(fs_squashfs::Superblock::decode(&[0; 96]))
            .capabilities()
    },
};

/// Every driver the engine reads through the layers.
pub fn registry() -> &'static [RegisteredDriver] {
    &[SQUASHFS]
}

/// The driver whose volumes begin with these bytes (the first 512 of a source).
pub fn recognise(head: &[u8]) -> Option<&'static RegisteredDriver> {
    registry().iter().find(|driver| {
        driver
            .magic
            .iter()
            .all(|(offset, bytes)| head.get(*offset..*offset + bytes.len()) == Some(*bytes))
    })
}

/// The registry's entry for a label.
pub fn of_kind(kind: FilesystemKind) -> Option<&'static RegisteredDriver> {
    registry().iter().find(|driver| driver.kind == kind)
}

/// Whether the engine lists the entries of a driver's filesystem (the scan, the map, browsing, preview and copy over
/// `EntrySource::Driver`). Until it does, the support table offers only what is true: the volume opens, and its structures
/// are kept.
const LISTS_ENTRIES: bool = true;

/// The rows of the support table for a driver's filesystem. What the driver decides (open, scan, browse, export,
/// rebuilds, damage, geometry hypotheses) is derived from what it says it lacks, with a volume that is clean and uses
/// nothing the driver cannot read. What the app decides, the same for every driver, is written here once.
pub fn capability_table(kind: FilesystemKind) -> Vec<Capability> {
    use Support::{Limited, Missing, Supported};
    let Some(driver) = of_kind(kind) else {
        return Vec::new();
    };
    let support = VolumeSupport {
        files: true,
        salvage: (driver.capabilities)(),
        unsupported: Vec::new(),
    };
    let derived = capability_rows(&support, &VolumeHealth::from_findings(Vec::new()));
    let row = |feature: &'static str| {
        derived
            .iter()
            .find(|row| row.feature == feature)
            .map(|row| (row.support, leak(&row.note)))
    };
    let mut table = Vec::new();
    let mut add = |feature: &'static str, support: Support, note: &'static str| {
        table.push(Capability {
            feature,
            support,
            note,
        })
    };
    let (support, note) = row("open").unwrap_or((Missing, "The driver cannot open a volume."));
    add("open", support, note);
    if !LISTS_ENTRIES {
        // The engine opens the volume and says what it is. It does not yet list what is in it, so no feature that
        // needs the entries is offered, whatever the driver can do.
        const NOT_YET: &str = "Not yet: the volume opens and says what it is, but the app does not list the entries of this filesystem yet.";
        for feature in [
            "scan",
            "map",
            "browse",
            "preview",
            "resume",
            "file_tests",
            "rebuilds",
            "export",
        ] {
            add(feature, Missing, NOT_YET);
        }
        add("baseline", Supported, "The volume's first structures (its superblock) are hashed and kept; changes are explained.");
        add(
            "mount_block",
            Missing,
            "macOS does not mount this filesystem, so there is nothing to block.",
        );
        return table;
    }
    let (support, note) = row("scan").unwrap_or((Missing, ""));
    add("scan", support, note);
    add("map", Limited, "The driver gives no allocation, so every cluster of the map is allocation unknown, which is not free and not in use. Content and ownership layers.");
    let (support, note) = row("browse").unwrap_or((Missing, ""));
    add(
        "browse",
        if support == Supported { Limited } else { support },
        leak(&format!("{note} Symbolic links and special files are not listed yet: a link's target is read through the driver's file layer, not through storage it walks.")),
    );
    add("preview", Limited, "Bytes are read through the driver, which decodes compressed blocks and fragment tails. A range that cannot be read is marked, as for the other filesystems.");
    add(
        "resume",
        Supported,
        "A paused or interrupted scan continues from where it was.",
    );
    add("baseline", Supported, "The volume's first structures (its superblock) are hashed and kept; changes are explained.");
    add(
        "file_tests",
        Supported,
        "File formats are walked through the bytes the driver reads.",
    );
    let (support, note) = row("rebuilds").unwrap_or((Missing, ""));
    add("rebuilds", support, note);
    let (support, note) = row("export").unwrap_or((Missing, ""));
    add("export", support, note);
    add(
        "mount_block",
        Missing,
        "macOS does not mount this filesystem, so there is nothing to block.",
    );
    table
}

/// A note derived at run time, kept for the life of the process: the support table hands out `&'static str`, and a
/// driver's table is made once per call, with a handful of short notes.
fn leak(note: &str) -> &'static str {
    Box::leak(note.to_string().into_boxed_str())
}

/// The record number of a driver volume's root folder in the map.
pub const ROOT_RECORD: u32 = 5;

/// How much of the start of a source is read to find a driver's structures. They are small and near the start (a
/// superblock, a boot record); a table that is megabytes or gigabytes long is read from the source when it is needed,
/// never held.
const PREFIX_BYTES: usize = 1 << 20;
/// More than any volume: a reader of the start of a source says this when it is asked only what the volume declares.
const UNBOUNDED: u64 = 1 << 62;

/// The first bytes of a volume and nothing after them, as a driver reads a volume it was not lent whole. A read past the
/// bytes fails as a read past a device does.
struct Prefix {
    bytes: Vec<u8>,
    size: u64,
}

impl BlockRead for Prefix {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        let outside = || fs_core_layers::Error::OutOfBounds {
            offset,
            len: output.len() as u64,
            size: self.size,
        };
        let start = usize::try_from(offset).map_err(|_| outside())?;
        let bytes = self
            .bytes
            .get(start..start.saturating_add(output.len()))
            .ok_or_else(outside)?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.size
    }
}

/// What a source holds at its start, up to `limit` bytes: a source shorter than that gives what it has.
fn read_prefix(source: &dyn BlockSource, limit: usize) -> Vec<u8> {
    let mut bytes = Vec::new();
    for step in [64 * 1024, 4096, 512] {
        while bytes.len() < limit {
            let mut chunk = vec![0; step.min(limit - bytes.len())];
            if source
                .read_exact_at(bytes.len() as u64, &mut chunk)
                .is_err()
            {
                break;
            }
            bytes.extend_from_slice(&chunk);
        }
    }
    bytes
}

/// The length a volume declares, from the driver's own probe over the bytes at its start. A volume that does not declare
/// one is read as far as the bytes go.
fn declared_bytes(driver: &RegisteredDriver, head: &[u8]) -> Result<u64> {
    let probe = (driver.probe)(&Prefix {
        bytes: head.to_vec(),
        size: UNBOUNDED,
    })?;
    if probe.recognition == Recognition::No {
        bail!("{}: {}", driver.format, probe.detail);
    }
    Ok(probe.declared_bytes.unwrap_or(head.len() as u64))
}

/// Saves what a driver's volume is known by: the bytes from the start of the source to the end of the last of its
/// primary boot and superblock structures. They are the baseline, and what the driver is lent in place of the disk's own
/// when the disk's are written over. The other regions of a copy are empty: a driver's tables are read from the source.
pub fn read_structures(
    source: &dyn BlockSource,
    driver: &'static RegisteredDriver,
) -> Result<StructureCopy> {
    let bytes = read_prefix(source, PREFIX_BYTES);
    let size = declared_bytes(driver, &bytes)?;
    let access = (driver.open)(Arc::new(Prefix {
        bytes: bytes.clone(),
        size,
    }))?;
    let end = access
        .structures()?
        .iter()
        .filter(|structure| {
            structure.copy == 0
                && matches!(
                    structure.role,
                    StructureRole::Boot | StructureRole::Superblock
                )
        })
        .map(|structure| structure.location.offset + structure.location.length)
        .filter(|end| *end <= bytes.len() as u64)
        .max()
        .ok_or_else(|| anyhow!("{}: no boot record or superblock to keep", driver.format))?;
    Ok(StructureCopy {
        main: bytes[..end as usize].to_vec(),
        backup: Vec::new(),
        fat: Vec::new(),
        bitmap: Vec::new(),
        root: Vec::new(),
        unread: Vec::new(),
    })
}

/// What a saved copy says of the volume: its geometry, as the driver reports it from the saved bytes, and what is wrong
/// with its own structures. The driver gives no allocation, so none is known.
pub fn inspect(structures: &StructureCopy, driver: &'static RegisteredDriver) -> Result<Inspected> {
    let size = declared_bytes(driver, &structures.main)?;
    let access = (driver.open)(Arc::new(Prefix {
        bytes: structures.main.clone(),
        size,
    }))?;
    let geometry = access.geometry()?;
    let unit = geometry.allocation_unit.max(4096);
    let volume_size = geometry.declared_bytes.max(size);
    let cluster_count = u32::try_from(volume_size.div_ceil(unit))
        .map_err(|_| anyhow!("{}: more allocation units than a map holds", driver.format))?;
    let details = FilesystemDetails::Driver {
        format: driver.format.to_string(),
        name: driver.kind.to_string(),
        allocation_unit: geometry.allocation_unit,
        allocation_units: geometry.allocation_units,
        declared_bytes: geometry.declared_bytes,
        label: geometry.label.clone(),
        serial: geometry.serial.clone(),
    };
    Ok(Inspected {
        boot: FilesystemBoot::Driver(details.clone()),
        backup_boot: None,
        boot_check: None,
        geometry: VolumeGeometry {
            volume_serial: 0,
            // The map's tree is made of record numbers, and the folder every claim names the root of is record 5, as on an NTFS
            // volume (`MapDb::link_driver_records`).
            root_cluster: ROOT_RECORD,
            partition_offset: 0,
            // Cluster 2 is the first allocation unit, at the start of the source.
            data_offset: 0,
            fat_offset: 0,
            fat_bytes: 0,
            bytes_per_sector: u64::from(geometry.sector_size.max(1)),
            cluster_size: unit,
            cluster_count,
            volume_size,
            record_size: 0,
        },
        fat: Vec::new(),
        bitmap: vec![0; usize::try_from(u64::from(cluster_count).div_ceil(8))?],
        root_clusters: Vec::new(),
        kind: driver.kind,
        details,
        dirty_flag: false,
        allocation_known: false,
        // What is wrong with the volume's own tables is judged over the source, when the driver opens it (`findings`): the
        // saved bytes hold no tables.
        damage: Vec::new(),
    })
}

/// What is wrong with a driver's volume, in words for the person: the driver's own findings about its structures, the
/// first few.
pub fn findings(access: &dyn DriverAccess) -> Vec<String> {
    match access.health() {
        Ok(health) => health
            .findings
            .iter()
            .take(20)
            .map(|finding| format!("{}: {}", finding.structure, finding.detail))
            .collect(),
        Err(error) => vec![format!("The driver could not judge the volume: {error:#}")],
    }
}

/// The entries whose records start in the cluster `cluster` of the scan, as the app's entries: what the driver says of each
/// candidate (live or not, whole or damaged, its size and its name), the storage it walked for it, and the number of the name
/// among those that share its record bytes (`name_slot`; the inodes of one SquashFS metadata block do), which keeps them
/// apart in the map. The map's own numbers (the `record_number` of each, and the record a folder is in) are given when the
/// entries are stored and linked.
pub fn entries_in_cluster(
    access: &dyn DriverAccess,
    geometry: &VolumeGeometry,
    cluster: u32,
) -> Vec<SalvageEntry> {
    let start = crate::allocation::cluster_offset(geometry, cluster);
    if start >= geometry.volume_size {
        return Vec::new();
    }
    let range = Extent::new(
        start,
        geometry.cluster_size.min(geometry.volume_size - start),
    );
    let mut records = Vec::new();
    if access
        .find(range, &mut |record| {
            records.push(record);
            ControlFlow::Continue(())
        })
        .is_err()
    {
        return Vec::new();
    }
    let mut slots: std::collections::HashMap<u64, u32> = std::collections::HashMap::new();
    let mut entries = Vec::new();
    for record in records {
        // A symbolic link or a special file is not a stream of bytes the driver walks: a link's target is read through the
        // driver's file layer. They are not listed yet.
        if matches!(record.kind, CandidateKind::Structure | CandidateKind::Other) {
            continue;
        }
        let Ok(walk) = access.walk(&record) else {
            continue;
        };
        let slot = slots.entry(record.location.offset).or_insert(0);
        let name_slot = *slot;
        *slot += 1;
        let is_dir = record.kind == CandidateKind::Directory;
        let length = if is_dir {
            0
        } else {
            record.size.unwrap_or(walk.claimed)
        };
        let complete = walk.end == fs_core_layers::layers::WalkEnd::Complete;
        entries.push(SalvageEntry {
            offset: (record.location.offset - start) as usize,
            name: record.name.clone().unwrap_or_default(),
            in_use: record.state != CandidateState::Deleted,
            checksum_ok: record.integrity != Integrity::Failed,
            attributes: 0,
            is_dir,
            first_cluster: 0,
            length,
            valid_length: length,
            no_fat_chain: false,
            created: None,
            modified: None,
            accessed: None,
            slots: 1,
            ntfs: Some(NtfsEntry {
                record_number: None,
                sequence: 0,
                parent_record: None,
                parent_sequence: None,
                storage: if complete {
                    STORAGE_DRIVER
                } else {
                    STORAGE_DRIVER_PARTIAL
                }
                .to_string(),
                runs: Vec::new(),
            }),
            ranges: crate::ranges::RangeList::default(),
            name_slot,
            weak: None,
            walk: None,
            driver: Some(DriverFacts { record, walk }),
        });
    }
    entries
}

/// Opens the volume through its driver, over the source with the saved structures read in place of the disk's own.
pub fn open(
    source: &Arc<dyn BlockSource>,
    geometry: &VolumeGeometry,
    structures: &StructureCopy,
    driver: &'static RegisteredDriver,
) -> Result<Box<dyn DriverAccess>> {
    (driver.open)(Arc::new(Overlay::new(
        Arc::clone(source),
        geometry.volume_size,
        vec![(0, structures.main.clone())],
    )))
}
