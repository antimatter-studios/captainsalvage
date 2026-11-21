//! Format dispatch and common read-only file access across filesystem drivers.
//!
//! This is the adapter boundary for bringing additional filesystem drivers
//! into Captain Salvage. Format-specific forensic structures remain in their
//! drivers; callers use normalized entries and bounded reads here.

pub mod driver;
pub mod drivers;
pub mod fat_generic;
pub mod generic;
pub mod ntfs;
pub(crate) mod ntfs_salvage;
pub mod probe;
pub(crate) mod salvage;

use anyhow::{bail, Context, Result};
use fs_core::BlockRead;
// The FAT driver's own file trait is deprecated for `fs_core::layers::FileAPI` and goes in its next release. The generic
// adapter over those layers (`filesystem::generic`) is what replaces its use here; until then it is the one thing read
// through it, the root directory and the open of an entry.
#[allow(deprecated)]
use fs_fat32::file::FileAPI as FatFileAPI;
use serde::Serialize;
use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_VOLUME_ID: AtomicU64 = AtomicU64::new(1);

/// The OEM identifier a BitLocker-encrypted volume carries in place of `NTFS    `.
pub const BITLOCKER_OEM_ID: &[u8; 8] = b"-FVE-FS-";
/// What the person is told when a volume is BitLocker encrypted. Nothing is read or guessed.
pub const BITLOCKER_MESSAGE: &str = "This volume is encrypted with BitLocker. Captain Salvage cannot read its files until it is unlocked and decrypted by Windows, and it does not guess at encrypted data.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum FilesystemKind {
    Exfat,
    Fat12,
    Fat16,
    Fat32,
    Ntfs,
    /// A filesystem read through its driver's implementation of rust-fs-core's layers, by the registry in `drivers`. This
    /// is a label: nothing in the engine tests for it, and every driver in the registry has one.
    Squashfs,
    /// ext2, ext3 and ext4, which one driver reads (its layers name the flavour of each volume).
    Ext4,
    /// EROFS, the read-only filesystem of images and Android partitions.
    Erofs,
}

impl fmt::Display for FilesystemKind {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Exfat => "exFAT",
            Self::Fat12 => "FAT12",
            Self::Fat16 => "FAT16",
            Self::Fat32 => "FAT32",
            Self::Ntfs => "NTFS",
            Self::Squashfs => "SquashFS",
            Self::Ext4 => "ext2/3/4",
            Self::Erofs => "EROFS",
        })
    }
}

/// How much of the start of a volume tells its filesystem: a driver's magic may lie past the first sector (an ext2/3/4
/// superblock begins at byte 1024), so the callers that name a volume by its start read this much when the first sector
/// does not name it.
pub const HEAD_BYTES: usize = 2048;

/// The start of a volume: its first sector, and, when that names no filesystem, the bytes up to [`HEAD_BYTES`] if the source
/// has them. A first sector that cannot be read is the error, once; a source that ends before [`HEAD_BYTES`] gives the sector.
pub fn read_head<E: Into<anyhow::Error>>(
    mut read: impl FnMut(u64, &mut [u8]) -> std::result::Result<(), E>,
) -> Result<Vec<u8>> {
    let mut head = vec![0; 512];
    read(0, &mut head).map_err(Into::into)?;
    if FilesystemKind::detect_boot_sector(&head).is_ok() || &head[3..11] == BITLOCKER_OEM_ID {
        return Ok(head);
    }
    let mut more = vec![0; HEAD_BYTES - 512];
    if read(512, &mut more).is_ok() {
        head.extend_from_slice(&more);
    }
    Ok(head)
}

impl FilesystemKind {
    pub fn detect_boot_sector(boot_sector: &[u8]) -> Result<Self> {
        if boot_sector.len() < 512 {
            bail!("filesystem boot sector is shorter than 512 bytes");
        }
        if &boot_sector[3..11] == b"EXFAT   " {
            return Ok(Self::Exfat);
        }
        if &boot_sector[3..11] == b"NTFS    " {
            return Ok(Self::Ntfs);
        }
        if &boot_sector[3..11] == BITLOCKER_OEM_ID {
            bail!("{BITLOCKER_MESSAGE}");
        }
        if let Some(driver) = drivers::recognise(boot_sector) {
            return Ok(driver.kind);
        }
        let boot = fs_fat32::BootSector::parse(boot_sector).context("identifying FAT variant")?;
        Ok(match boot.kind {
            fs_fat32::FatKind::Fat12 => Self::Fat12,
            fs_fat32::FatKind::Fat16 => Self::Fat16,
            fs_fat32::FatKind::Fat32 => Self::Fat32,
        })
    }
}

/// Whether a feature works for a filesystem, works with stated limits, or is not offered.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    Supported,
    Limited,
    Missing,
}

/// One row of what the app can and cannot do for a filesystem. The window, the command line
/// and MCP all report this table, so none of them claims more than the adapter provides.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Capability {
    pub feature: &'static str,
    pub support: Support,
    pub note: &'static str,
}

const fn capability(feature: &'static str, support: Support, note: &'static str) -> Capability {
    Capability {
        feature,
        support,
        note,
    }
}

/// How a filesystem says which clusters an entry holds. The shared engine asks this instead of
/// asking which filesystem it has, so a new filesystem answers once, here.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Claims {
    /// A first cluster, followed through a table of links (FAT, exFAT), or contiguously where an
    /// exFAT entry says so.
    Chain,
    /// A run list in the entry's own record (NTFS). There is no table of links to follow, and so
    /// no chain to lead back to a head.
    RunList,
    /// The driver says: its own records name the storage (extents, blocks, fragments), and there are no links between
    /// clusters to follow and no run list of ours to read (SquashFS and every driver in `drivers`).
    Driver,
}

impl Claims {
    /// Whether there are links between clusters to follow, from a cluster to the next or back.
    pub fn has_links(self) -> bool {
        self == Self::Chain
    }

    /// The sentence for a stream whose claim starts outside the volume.
    pub fn outside_volume(self) -> &'static str {
        match self {
            Self::Chain => "First cluster is outside the volume.",
            Self::RunList => "Its run list names clusters outside the volume.",
            Self::Driver => "Its record names storage outside the volume.",
        }
    }

    /// What is said of a stream that is reachable only as far as the clusters found.
    pub fn ends_early(self) -> &'static str {
        match self {
            Self::Chain => "FAT chain ends early:",
            Self::RunList => "Its run list ends early:",
            Self::Driver => "Its storage ends early:",
        }
    }
}

impl FilesystemKind {
    /// exFAT's reports keep the field names, and the missing `filesystem` details, they had before
    /// other filesystems were added, because the frozen parity references pin them. Every other
    /// filesystem names its fields for what they are and carries its typed details. The shared
    /// engine asks this and never tests for exFAT.
    pub fn keeps_legacy_report_shape(self) -> bool {
        self == Self::Exfat
    }

    /// FAT12, FAT16 and FAT32: a folder is a chain of clusters of 32-byte entries, a long name is
    /// the entries before its short entry, and an entry is placed by its short entry.
    pub fn is_fat_family(self) -> bool {
        matches!(self, Self::Fat12 | Self::Fat16 | Self::Fat32)
    }

    /// exFAT: an entry set is placed by its first entry and may run on into the folder's next
    /// cluster, which is where the folder's FAT chain says, not always the next on the disk.
    pub fn places_entry_sets_by_first_entry(self) -> bool {
        self == Self::Exfat
    }

    /// FAT12 and FAT16 keep the root folder in a fixed region before the first cluster, which no
    /// cluster scan reaches, so it is read from the saved structures.
    pub fn has_fixed_root(self) -> bool {
        matches!(self, Self::Fat12 | Self::Fat16)
    }

    /// NTFS clusters can hold file records, which the cluster classes name.
    pub fn classes_name_file_records(self) -> bool {
        self == Self::Ntfs
    }

    pub fn claims(self) -> Claims {
        match self {
            Self::Ntfs => Claims::RunList,
            Self::Squashfs | Self::Ext4 | Self::Erofs => Claims::Driver,
            Self::Exfat | Self::Fat12 | Self::Fat16 | Self::Fat32 => Claims::Chain,
        }
    }

    /// What the app does for this filesystem, validated on synthetic images.
    pub fn capabilities(self) -> Vec<Capability> {
        use Support::{Limited, Missing, Supported};
        match self {
            Self::Exfat => vec![
                capability("open", Supported, "Boot sector, FAT and allocation bitmap are read and saved as a baseline."),
                capability("scan", Supported, "Every cluster is read once for directory entries; resumable."),
                capability("map", Supported, "Allocation, content, ownership and confidence layers."),
                capability("browse", Supported, "The folder tree is rebuilt from directory clusters."),
                capability("preview", Limited, "Bytes are read through the FAT chain, or contiguously where the entry says so. A cluster that cannot be read is marked: the text and byte previews show each of its bytes as ?, and name the range; an image or a film shows it as zeros."),
                capability("resume", Supported, "A paused or interrupted scan continues from its last saved cluster."),
                capability("baseline", Supported, "Boot regions, FAT, bitmap and root are hashed and kept; changes are explained."),
                capability("file_tests", Supported, "File formats are walked through the reassembled bytes."),
                capability("rebuilds", Supported, "Disk Rebuilds overlay cluster lists and folder links without touching the disk."),
                capability("export", Limited, "Copies are hashed, never overwrite, and record complete or partial status. A cluster that cannot be read is written as zeros, so the file keeps its layout, and the copy is marked partial; the manifest names the unreadable byte ranges."),
                capability("mount_block", Supported, "macOS is refused a mount of the volume while the app is open."),
            ],
            Self::Squashfs | Self::Ext4 | Self::Erofs => drivers::capability_table(self),
            Self::Fat12 | Self::Fat16 | Self::Fat32 => vec![
                capability("open", Supported, "The boot sector, the reserved sectors, the FAT copies and the root directory are read and saved as a baseline. FAT12, FAT16 and FAT32 are told apart by their cluster count. FAT has no allocation bitmap on disk. Checked on synthetic images and by the driver's independent tests, not yet on a real damaged drive. The entries are read through the FAT driver's generic layers adapter, which the driver's own checks cover; the old FAT adapter stays for one release (set SALVAGE_FAT_ADAPTER=legacy) and reads a volume the driver does not open."),
                capability("scan", Supported, "Every cluster is read once for directory entries, deleted ones included; a deleted entry has lost the first character of its name and its chain is cleared. FAT12 and FAT16 keep a fixed-size root directory that is scanned apart from the data clusters; the FAT32 root is a chain. Resumable. An entry in a cluster that is free in the FAT is deleted, as the driver reads it. A file's clusters are the driver's walk of what its entry claims: its FAT chain, and, for a deleted file, whose chain the FAT cleared, one run from its first cluster, which is a guess and is marked as one (the file is copied whole, from the guess, instead of one cluster). An entry the driver found by its content only, not through a folder that names it, is a weak candidate: marked unchecked, and lone_entry when it is the only entry in its cluster, with half the confidence."),
                capability("map", Supported, "A cluster is allocated when its FAT entry is not zero, because there is no bitmap to read. Content, ownership and confidence layers."),
                capability("browse", Supported, "The folder tree is rebuilt from directory clusters and the fixed root of FAT12 and FAT16. A long name is used only when its checksum agrees with its short name."),
                capability("preview", Limited, "Bytes are read through the FAT chain. A deleted file's chain is cleared, so it is partial unless the person turns on the contiguous-fallback hypothesis, which assumes the clusters after the first follow it. A cluster that cannot be read is marked: the text and byte previews show each of its bytes as ?, and name the range; an image or a film shows it as zeros."),
                capability("resume", Supported, "A paused or interrupted scan continues from its last saved cluster."),
                capability("baseline", Supported, "The boot sector, the reserved sectors, every FAT copy and the root directory are hashed and kept, and a changed copy of a FAT32 table is reported; changes are explained."),
                capability("file_tests", Supported, "File formats are walked through the reassembled bytes."),
                capability("rebuilds", Supported, "Disk Rebuilds overlay cluster lists and folder links without touching the disk."),
                capability("export", Limited, "Copies are hashed, never overwrite, and record complete or partial status. A cluster that cannot be read is written as zeros, so the file keeps its layout, and the copy is marked partial; the manifest names the unreadable byte ranges."),
                capability("mount_block", Supported, "macOS is refused a mount of the volume while the app is open."),
            ],
            Self::Ntfs => vec![
                capability("open", Supported, "The boot sector, $MFT, $Volume, root directory and $Bitmap are read and saved as a baseline, and the baseline says which of them could not be read. A BitLocker volume is refused by name. A volume whose $Bitmap cannot be read still opens, with its allocation unknown, and a $MFT, $Volume or root record that fails its update-sequence check is reported as damage."),
                capability("scan", Supported, "Every cluster is read for FILE records, so deleted files, old copies of the MFT and files whose folders are gone are found. Resumable."),
                capability("map", Supported, "Allocation comes from $Bitmap. Clusters holding FILE records are shown as directory structure. The first two clusters, the boot area, are not drawn. When $Bitmap cannot be read every cluster is drawn as allocation unknown, which is not free and not in use, and unexplained-space totals do not count it as free."),
                capability("browse", Supported, "The folder tree is rebuilt from each record's parent reference. A folder whose own record was lost is listed unnamed."),
                capability("preview", Limited, "Resident, fragmented, sparse, deleted and NTFS-compressed (LZNT1) data is read. Encrypted (EFS) and attribute-list streams are listed but cannot be read, and say why. A stretch of a stream held in runs that cannot be read (a damaged or missing run, a run outside the volume, a failed read, a compression unit that does not decode) is marked: the text and byte previews show each of its bytes as ?, and name the range."),
                capability("resume", Supported, "A paused or interrupted scan continues from its last saved cluster."),
                capability("baseline", Supported, "The boot sector, its backup, $MFT, $Volume, the root record and $Bitmap are hashed and kept; clusters written since are reported."),
                capability("file_tests", Limited, "Formats are walked through the bytes the driver reads. Streams that cannot be read are not tested."),
                capability("rebuilds", Limited, "Disk Rebuilds can replace a file's clusters and folder links, and are read as clusters. The record, its names and its run list are never altered."),
                capability("export", Limited, "Copies are hashed, never overwrite, and record complete or partial status. A stretch of a stream held in runs that cannot be read is written as zeros, so the file keeps its layout, and the copy is marked partial; the manifest names the unreadable byte ranges. A stream that cannot be read at all (encrypted, compressed in a way this does not decode, in other records, incomplete, or a record that changed) is reported failed, writes no file and is never filled with zeros."),
                capability("mount_block", Limited, "Blocked by the volume UUID when macOS gives one, which it does for a real NTFS volume on a drive. When it gives none (a synthetic image, a disk image), the volume is blocked by where it is: the disk's identity (the device's own GUID, or for a disk with none a fingerprint of its first 4 KiB) with the partition's offset and length. So a disk with no GUID whose start is rewritten, or a partition that is moved or resized, is no longer recognised as blocked. macOS mounts NTFS read-only itself; only a third-party NTFS driver would write."),
                capability("named_streams", Limited, "Alternate data streams are listed in the record details, not copied as separate files. Streams held in extension records are not followed."),
                capability("hard_links", Supported, "A file with several names is listed under each, in the folder the name belongs to, with the file's size and bytes, and a copy holds it under every name. Each name is an entry of its own, told apart from the others by its place in the record's names, so a rescan does not duplicate it."),
                capability("attribute_lists", Missing, "A file whose attributes continue in extension records is listed, with its size, but its data is not read (rust-fs-ntfs#1379)."),
                capability("compression", Supported, "A file NTFS compressed (LZNT1) is read: each compression unit is mapped to its clusters here and decoded by the driver. Checked on synthetic images and on volumes Windows formatted and compressed files on (512, 1024 and 4096-byte clusters), where every file has the SHA-256 Windows reports. Another compression method, or a stream whose runs continue in another record, is listed and refused."),
                capability("encryption", Missing, "EFS-encrypted files are listed and marked unreadable. BitLocker volumes are refused."),
                capability("journal", Missing, "$LogFile and $UsnJrnl are not interpreted, and nothing is replayed onto the volume."),
                capability("large_volumes", Limited, "Volumes of up to 4,294,967,294 clusters (2^32 - 2), which is 16 TiB with 4 KiB clusters. Windows itself does not create or mount a volume with more, so this app refuses one at open, before it reads anything else."),
            ],
        }
    }
}

impl Support {
    fn label(self) -> &'static str {
        match self {
            Self::Supported => "Supported",
            Self::Limited => "Limited",
            Self::Missing => "Missing",
        }
    }
}

/// The feature as the window and the README name it.
fn feature_label(feature: &str) -> String {
    match feature {
        "baseline" => "Baseline copy".into(),
        "export" => "Copy out".into(),
        "rebuilds" => "Disk Rebuilds".into(),
        _ => {
            let mut words = feature.replace('_', " ");
            if let Some(first) = words.get_mut(..1) {
                first.make_ascii_uppercase();
            }
            words
        }
    }
}

/// The support table as the README shows it: one matrix of the three filesystem families, then
/// every note for the two FAT-shaped ones. NTFS's notes are longer and have a section of their
/// own. The README block is checked against this text, so the README cannot claim more than the
/// table that the window, the command line and MCP report.
pub fn capabilities_markdown() -> String {
    let mut families = vec![
        ("exFAT".to_string(), FilesystemKind::Exfat),
        ("FAT12, FAT16, FAT32".to_string(), FilesystemKind::Fat32),
        ("NTFS".to_string(), FilesystemKind::Ntfs),
    ];
    // One column for each driver the registry reads through the layers, with the rows its own `Capabilities` give.
    for driver in drivers::registry() {
        families.push((driver.kind.to_string(), driver.kind));
    }
    let tables: Vec<Vec<Capability>> = families
        .iter()
        .map(|(_, kind)| kind.capabilities())
        .collect();
    let mut features: Vec<&'static str> = Vec::new();
    for table in &tables {
        for row in table {
            if !features.contains(&row.feature) {
                features.push(row.feature);
            }
        }
    }
    let mut text = String::from("| Feature |");
    for (name, _) in &families {
        text.push_str(&format!(" {name} |"));
    }
    text.push_str("\n|---|");
    for _ in &families {
        text.push_str("---|");
    }
    text.push('\n');
    for feature in &features {
        text.push_str(&format!("| {} |", feature_label(feature)));
        for table in &tables {
            let cell = table
                .iter()
                .find(|row| row.feature == *feature)
                .map_or("Not offered", |row| row.support.label());
            text.push_str(&format!(" {cell} |"));
        }
        text.push('\n');
    }
    for (name, kind) in families
        .iter()
        .filter(|(_, kind)| !matches!(kind, FilesystemKind::Ntfs))
    {
        text.push_str(&format!("\n**{name}**\n\n"));
        for row in kind.capabilities() {
            text.push_str(&format!(
                "- {} ({}): {}\n",
                feature_label(row.feature),
                row.support.label(),
                row.note
            ));
        }
    }
    text
}

#[derive(Clone)]
enum DriverEntry {
    Exfat(fs_exfat::directory::DirectoryEntry),
    Fat(fs_fat32::directory::DirectoryEntry),
}

#[derive(Clone)]
pub struct FileEntry {
    pub name: String,
    pub size: u64,
    pub is_directory: bool,
    volume_id: u64,
    driver_entry: DriverEntry,
}

/// A device both generations of rust-fs-core read: the exFAT and NTFS drivers use 0.4's `BlockRead`, the FAT driver 0.5's.
pub trait DriverDevice: BlockRead + fs_core_layers::BlockRead {}

impl<T: BlockRead + fs_core_layers::BlockRead> DriverDevice for T {}

enum DriverVolume<Device: DriverDevice> {
    Exfat(Box<fs_exfat::volume::Volume<std::sync::Arc<Device>>>),
    Fat(fs_fat32::Volume<std::sync::Arc<Device>>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct VolumeGeometry {
    pub volume_serial: u32,
    pub root_cluster: u32,
    pub partition_offset: u64,
    pub data_offset: u64,
    pub fat_offset: u64,
    pub fat_bytes: u64,
    pub bytes_per_sector: u64,
    pub cluster_size: u64,
    pub cluster_count: u32,
    pub volume_size: u64,
    /// The size of one NTFS FILE record; zero for every other filesystem.
    pub record_size: u64,
}

/// One run of an NTFS data stream: `count` clusters starting at virtual cluster `vcn`,
/// at logical cluster `lcn`, or a sparse hole when there is none.
#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct NtfsRun {
    pub vcn: u64,
    pub lcn: Option<u64>,
    pub count: u64,
}

/// How an NTFS entry's bytes are held, and so whether they can be read at all.
pub const STORAGE_RESIDENT: &str = "resident";
pub const STORAGE_RUNS: &str = "runs";
pub const STORAGE_NONE: &str = "none";
/// An LZNT1-compressed stream, which is decoded when it is read.
pub const STORAGE_COMPRESSED: &str = "compressed";
/// A stream flagged as compressed in a way this app does not decode: another method, or no usable
/// compression unit.
pub const STORAGE_COMPRESSION_UNSUPPORTED: &str = "compression-unsupported";
pub const STORAGE_ENCRYPTED: &str = "encrypted";
pub const STORAGE_ATTRIBUTE_LIST: &str = "attribute-list";
pub const STORAGE_INCOMPLETE: &str = "incomplete";
/// Something shaped like a FILE record was here, but it failed its update-sequence check.
pub const STORAGE_DAMAGED: &str = "damaged-record";
/// A driver's candidate whose storage the driver walked to its end, and reads through the driver (`drivers`).
pub const STORAGE_DRIVER: &str = "driver";
/// A driver's candidate whose walk did not reach the end of what the record claims: what it reached is read, and the rest is
/// named as missing.
pub const STORAGE_DRIVER_PARTIAL: &str = "driver-partial";
/// A driver's candidate that is not a stream of bytes the driver walks (a symbolic link, a special file): it is read by its
/// path through the driver's file layer, which gives a link's target and nothing for a device.
pub const STORAGE_DRIVER_NODE: &str = "driver-node";

/// What an NTFS FILE record says about an entry, kept beside the common fields. A file
/// reference names a record and the sequence it had; neither is a cluster.
#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecordEntry {
    /// The record's own number, when its header says one.
    pub record_number: Option<u64>,
    pub sequence: u16,
    /// The directory its chosen name was last linked in, when it has a name.
    pub parent_record: Option<u64>,
    pub parent_sequence: Option<u16>,
    pub storage: String,
    /// The unnamed `$DATA` stream's runs, in virtual-cluster order. Empty when resident.
    pub runs: Vec<NtfsRun>,
}

impl RecordEntry {
    /// Whether the bytes can be read through the driver rather than only described.
    pub fn is_readable(&self) -> bool {
        matches!(
            self.storage.as_str(),
            STORAGE_RESIDENT
                | STORAGE_RUNS
                | STORAGE_COMPRESSED
                | STORAGE_DRIVER
                | STORAGE_DRIVER_PARTIAL
                | STORAGE_DRIVER_NODE
        )
    }
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SalvageEntry {
    pub offset: usize,
    pub name: String,
    pub in_use: bool,
    pub checksum_ok: bool,
    pub attributes: u16,
    pub is_dir: bool,
    pub first_cluster: u32,
    pub length: u64,
    pub valid_length: u64,
    pub no_fat_chain: bool,
    pub created: Option<String>,
    pub modified: Option<String>,
    pub accessed: Option<String>,
    pub slots: usize,
    /// Set for entries found in NTFS FILE records.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub record: Option<RecordEntry>,
    /// Where the entry's bytes are, as typed ranges derived from the same data as its cluster
    /// list (issue #212, first slice). Nothing reads it but tests, and it is never serialised or
    /// stored, so saved scans and what scripts and tools report are as they were.
    #[serde(skip)]
    pub ranges: crate::ranges::RangeList,
    /// NTFS: 0 for the name a FILE record is shown under, and 1, 2, ... for each further hard link
    /// of the same file, in order of (parent record, name). Each is an entry of its own, in the
    /// folder its name belongs to, with the file's size and bytes; the slot is what keeps it apart
    /// from the first in the map (see `source_key`). Every other entry has 0.
    #[serde(skip_serializing_if = "is_first_name")]
    pub name_slot: u32,
    /// A FAT entry the driver found by its content only, not through a folder that names it: `unchecked`, or `lone_entry` when it
    /// is the only entry in its cluster (so it may be file bytes that look like one). Such an entry is a weak candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub weak: Option<String>,
    /// A FAT file's clusters as the FAT driver's `walk_extents` gives them, where the generic adapter reads the volume. The engine
    /// takes the entry's clusters from it, instead of following the FAT chain itself.
    #[serde(skip)]
    pub walk: Option<FatWalk>,
    /// A driver's candidate, whole, with the storage the driver walked for it: what the engine keeps to read it through the
    /// driver again (`drivers`). Its `ntfs` field carries the record numbers the map's tree is built from, as an NTFS
    /// record's do.
    #[serde(skip)]
    pub driver: Option<DriverFacts>,
}

/// What a driver gave of one candidate: the candidate, and the walk of its storage.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DriverFacts {
    pub record: drivers::DriverRecord,
    pub walk: fs_core_layers::layers::ExtentWalk,
}

/// The clusters a FAT file claims, as the driver walked them: runs of `(first cluster, count)`, in file order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FatWalk {
    /// The FAT does not say where the file goes on (its chain was cleared when it was deleted), so the driver guessed: only its
    /// first cluster is known, the rest is where the driver's guess put it.
    pub guessed: bool,
    pub extents: Vec<(u32, u32)>,
}

fn is_first_name(slot: &u32) -> bool {
    *slot == 0
}

/// The bits of a stored `source_offset` that say which of a FILE record's names an entry is. A
/// record's offset in its cluster is below the largest cluster size (2 MiB, 21 bits), and NTFS gives
/// one file at most 1024 names, so the slot fits in the rest of 32 bits.
pub const NAME_SLOT_SHIFT: u32 = 21;

/// What identifies an entry in the map: the record's offset in its cluster, and for a further name of
/// the same file, its slot above that.
pub fn source_key(offset: usize, slot: u32) -> i64 {
    (offset as i64) | (i64::from(slot) << NAME_SLOT_SHIFT)
}

/// The record's offset in its cluster, from a stored `source_offset`.
pub fn record_offset(source_offset: u32) -> u32 {
    source_offset & ((1 << NAME_SLOT_SHIFT) - 1)
}

impl From<fs_exfat::primitives::EntrySet> for SalvageEntry {
    fn from(entry: fs_exfat::primitives::EntrySet) -> Self {
        Self {
            offset: entry.offset,
            name: entry.name,
            in_use: entry.in_use,
            checksum_ok: entry.checksum_ok,
            attributes: entry.attributes,
            is_dir: entry.is_dir,
            first_cluster: entry.first_cluster,
            length: entry.length,
            valid_length: entry.valid_length,
            no_fat_chain: entry.no_fat_chain,
            created: entry.created,
            modified: entry.modified,
            accessed: entry.accessed,
            slots: entry.slots,
            record: None,
            ranges: crate::ranges::RangeList::default(),
            name_slot: 0,
            weak: None,
            walk: None,
            driver: None,
        }
    }
}

impl VolumeGeometry {
    pub fn cluster_size(&self) -> u64 {
        self.cluster_size
    }
    pub fn bytes_per_sector(&self) -> u64 {
        self.bytes_per_sector
    }
    pub fn heap_offset(&self) -> u64 {
        self.data_offset
    }
    pub fn last_cluster_exclusive(&self) -> u32 {
        self.cluster_count + 2
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SalvageFatEntry {
    Free,
    Next(u32),
    End,
    Bad,
    Invalid,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum FilesystemDetails {
    Exfat,
    Fat {
        variant: String,
        bytes_per_sector: u16,
        sectors_per_cluster: u8,
        reserved_sectors: u16,
        fat_count: u8,
        root_entry_count: u16,
        sectors_per_fat: u32,
        total_sectors: u32,
        first_data_sector: u32,
        root_cluster: u32,
    },
    /// A filesystem read through its driver's layers (`drivers`), by what the driver's own `Geometry` says.
    #[serde(rename_all = "camelCase")]
    Driver {
        format: String,
        /// The filesystem's name as a person reads it (`SquashFS`).
        name: String,
        allocation_unit: u64,
        allocation_units: u64,
        declared_bytes: u64,
        label: Option<String>,
        serial: Option<String>,
    },
    #[serde(rename_all = "camelCase")]
    Ntfs {
        bytes_per_sector: u16,
        sectors_per_cluster: u64,
        cluster_size: u64,
        mft_cluster: u64,
        file_record_size: u64,
        index_block_size: u32,
        total_sectors: u64,
        serial: String,
        /// `$Volume`'s version and label, when the record was readable.
        version: Option<String>,
        label: Option<String>,
    },
}

pub struct FilesystemVolume<Device: DriverDevice> {
    kind: FilesystemKind,
    volume_id: u64,
    device: std::sync::Arc<Device>,
    driver: DriverVolume<Device>,
}

impl<Device: DriverDevice> FilesystemVolume<Device> {
    pub fn open(device: Device) -> Result<Self> {
        let device = std::sync::Arc::new(device);
        let boot_sector = read_head(|offset, buffer| BlockRead::read_at(&*device, offset, buffer))
            .context("reading filesystem boot sector")?;
        let kind = FilesystemKind::detect_boot_sector(&boot_sector)?;
        if kind == FilesystemKind::Exfat {
            return Ok(Self {
                kind,
                volume_id: NEXT_VOLUME_ID.fetch_add(1, Ordering::Relaxed),
                device: std::sync::Arc::clone(&device),
                driver: DriverVolume::Exfat(Box::new(
                    fs_exfat::volume::Volume::open(std::sync::Arc::clone(&device))
                        .context("opening exFAT volume")?,
                )),
            });
        }
        if kind == FilesystemKind::Ntfs {
            bail!("NTFS has no clusters of links to follow: read it through the NTFS driver adapter, filesystem::driver::Driver");
        }
        let volume =
            fs_fat32::Volume::open(std::sync::Arc::clone(&device)).context("opening FAT volume")?;
        Ok(Self {
            kind,
            volume_id: NEXT_VOLUME_ID.fetch_add(1, Ordering::Relaxed),
            device: std::sync::Arc::clone(&device),
            driver: DriverVolume::Fat(volume),
        })
    }

    pub fn kind(&self) -> FilesystemKind {
        self.kind
    }

    pub fn details(&self) -> FilesystemDetails {
        match &self.driver {
            DriverVolume::Exfat(_) => FilesystemDetails::Exfat,
            DriverVolume::Fat(volume) => {
                let boot = volume.boot();
                FilesystemDetails::Fat {
                    variant: self.kind.to_string(),
                    bytes_per_sector: boot.bytes_per_sector,
                    sectors_per_cluster: boot.sectors_per_cluster,
                    reserved_sectors: boot.reserved_sectors,
                    fat_count: boot.fat_count,
                    root_entry_count: boot.root_entry_count,
                    sectors_per_fat: boot.sectors_per_fat,
                    total_sectors: boot.total_sectors,
                    first_data_sector: boot.first_data_sector,
                    root_cluster: boot.root_cluster,
                }
            }
        }
    }

    pub fn geometry(&self) -> VolumeGeometry {
        match &self.driver {
            DriverVolume::Exfat(volume) => {
                let boot = volume.boot();
                let bytes_per_sector = boot.bytes_per_sector();
                VolumeGeometry {
                    volume_serial: boot.volume_serial,
                    root_cluster: boot.root_cluster,
                    partition_offset: boot.partition_offset * bytes_per_sector,
                    data_offset: u64::from(boot.cluster_heap_offset) * bytes_per_sector,
                    fat_offset: u64::from(boot.fat_offset) * bytes_per_sector,
                    fat_bytes: u64::from(boot.fat_length) * bytes_per_sector,
                    bytes_per_sector,
                    cluster_size: boot.cluster_size(),
                    cluster_count: boot.cluster_count,
                    volume_size: boot.volume_length * bytes_per_sector,
                    record_size: 0,
                }
            }
            DriverVolume::Fat(volume) => {
                let boot = volume.boot();
                VolumeGeometry {
                    volume_serial: boot.volume_serial,
                    root_cluster: boot.root_cluster,
                    partition_offset: u64::from(boot.hidden_sectors)
                        * u64::from(boot.bytes_per_sector),
                    data_offset: u64::from(boot.first_data_sector)
                        * u64::from(boot.bytes_per_sector),
                    fat_offset: (u64::from(boot.reserved_sectors)
                        + u64::from(boot.active_fat) * u64::from(boot.sectors_per_fat))
                        * u64::from(boot.bytes_per_sector),
                    fat_bytes: u64::from(boot.sectors_per_fat) * u64::from(boot.bytes_per_sector),
                    bytes_per_sector: u64::from(boot.bytes_per_sector),
                    cluster_size: boot.cluster_size(),
                    cluster_count: boot.cluster_count,
                    volume_size: u64::from(boot.total_sectors) * u64::from(boot.bytes_per_sector),
                    record_size: 0,
                }
            }
        }
    }

    pub fn read_volume_bytes(&self, offset: u64, output: &mut [u8]) -> Result<()> {
        let end = offset
            .checked_add(output.len() as u64)
            .context("volume read range overflow")?;
        if end > self.geometry().volume_size {
            bail!("volume read exceeds the declared filesystem size");
        }
        BlockRead::read_at(&*self.device, offset, output)?;
        Ok(())
    }

    pub fn cluster_offset(&self, cluster: u32) -> Result<u64> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(volume.boot().cluster_offset(cluster)?),
            DriverVolume::Fat(volume) => Ok(volume.cluster_offset(cluster)?),
        }
    }

    /// Reads one allocation-table entry through the selected filesystem driver.
    pub fn fat_entry(&self, cluster: u32) -> Result<SalvageFatEntry> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(match volume.fat_entry(cluster)? {
                fs_exfat::metadata::FatEntry::Free => SalvageFatEntry::Free,
                fs_exfat::metadata::FatEntry::Next(next) => SalvageFatEntry::Next(next),
                fs_exfat::metadata::FatEntry::End => SalvageFatEntry::End,
                fs_exfat::metadata::FatEntry::Bad => SalvageFatEntry::Bad,
            }),
            DriverVolume::Fat(volume) => Ok(match volume.fat_value(cluster)? {
                fs_fat32::primitives::FatValue::Free => SalvageFatEntry::Free,
                fs_fat32::primitives::FatValue::Next(next) => SalvageFatEntry::Next(next),
                fs_fat32::primitives::FatValue::EndOfChain => SalvageFatEntry::End,
                fs_fat32::primitives::FatValue::Bad => SalvageFatEntry::Bad,
                fs_fat32::primitives::FatValue::Reserved(_)
                | fs_fat32::primitives::FatValue::OutOfRange(_) => SalvageFatEntry::Invalid,
            }),
        }
    }

    /// Returns the clusters reachable from a candidate head, stopping safely at malformed
    /// links, cycles, or the caller's limit rather than treating a damaged chain as fatal.
    pub fn chain_candidates(&self, first: u32, limit: usize) -> Result<Vec<u32>> {
        let mut clusters = Vec::new();
        let mut visited = std::collections::HashSet::new();
        let mut current = first;
        while clusters.len() < limit && visited.insert(current) {
            if current < 2 || current >= self.geometry().cluster_count.saturating_add(2) {
                break;
            }
            let entry = self.fat_entry(current)?;
            clusters.push(current);
            match entry {
                SalvageFatEntry::Next(next) => current = next,
                SalvageFatEntry::Free
                | SalvageFatEntry::End
                | SalvageFatEntry::Bad
                | SalvageFatEntry::Invalid => break,
            }
        }
        Ok(clusters)
    }

    /// Reports allocation consistently while retaining each format's source of truth:
    /// the exFAT bitmap, or non-zero FAT links for FAT12/16/32.
    pub fn is_allocated(&self, cluster: u32) -> Result<bool> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(volume.bitmap().allocated(cluster)?),
            DriverVolume::Fat(volume) => Ok(volume.fat_entry_raw(cluster)? != 0),
        }
    }

    pub fn read_cluster(&self, cluster: u32, output: &mut [u8]) -> Result<()> {
        let cluster_size = self.geometry().cluster_size;
        if output.len() as u64 != cluster_size {
            bail!("cluster buffer length does not match filesystem geometry");
        }
        self.read_volume_bytes(self.cluster_offset(cluster)?, output)
    }

    pub fn root_directory(&self) -> Result<Vec<FileEntry>> {
        match &self.driver {
            DriverVolume::Exfat(volume) => Ok(volume
                .root_directory()?
                .into_iter()
                .map(|entry| FileEntry {
                    name: entry.name().to_owned(),
                    size: entry.stream().data_length(),
                    is_directory: entry.is_directory(),
                    volume_id: self.volume_id,
                    driver_entry: DriverEntry::Exfat(entry),
                })
                .collect()),
            #[allow(deprecated)]
            DriverVolume::Fat(volume) => Ok(volume
                .root_directory()?
                .into_iter()
                .map(|entry| FileEntry {
                    name: entry.name.clone(),
                    size: u64::from(entry.size),
                    is_directory: entry.attributes & 0x10 != 0,
                    volume_id: self.volume_id,
                    driver_entry: DriverEntry::Fat(entry),
                })
                .collect()),
        }
    }

    /// Reads a bounded range from an entry returned by this volume's root listing.
    pub fn read_file_at(&self, entry: &FileEntry, offset: u64, output: &mut [u8]) -> Result<usize> {
        if entry.volume_id != self.volume_id {
            bail!("file entry belongs to a different volume");
        }
        match (&self.driver, &entry.driver_entry) {
            (DriverVolume::Exfat(volume), DriverEntry::Exfat(driver_entry)) => {
                Ok(volume.open_file(driver_entry)?.read_at(offset, output)?)
            }
            (DriverVolume::Fat(volume), DriverEntry::Fat(driver_entry)) =>
            {
                #[allow(deprecated)]
                Ok(fs_fat32::file::FileAPI::open_file(volume, driver_entry)?
                    .read_at(offset, output)?)
            }
            _ => bail!("file entry belongs to a different filesystem driver"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestDisk(Vec<u8>);

    impl fs_core_layers::BlockRead for TestDisk {
        fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
            BlockRead::read_at(self, offset, output).map_err(|error| match error {
                fs_core::Error::OutOfBounds { offset, len, size } => {
                    fs_core_layers::Error::OutOfBounds { offset, len, size }
                }
                other => fs_core_layers::Error::Io(std::io::Error::other(other.to_string())),
            })
        }
        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    impl BlockRead for TestDisk {
        fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
            let start = usize::try_from(offset).map_err(|_| fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: self.0.len() as u64,
            })?;
            let end = start
                .checked_add(output.len())
                .ok_or(fs_core::Error::OutOfBounds {
                    offset,
                    len: output.len() as u64,
                    size: self.0.len() as u64,
                })?;
            let bytes = self.0.get(start..end).ok_or(fs_core::Error::OutOfBounds {
                offset,
                len: output.len() as u64,
                size: self.0.len() as u64,
            })?;
            output.copy_from_slice(bytes);
            Ok(())
        }

        fn size_bytes(&self) -> u64 {
            self.0.len() as u64
        }
    }

    fn fat_image(kind: FilesystemKind, contents: &[u8]) -> Vec<u8> {
        let (sectors, fat_sectors, reserved, root_entries, root_sector, data_sector) = match kind {
            FilesystemKind::Fat12 => (4000usize, 12usize, 1usize, 224u16, 25usize, 39usize),
            FilesystemKind::Fat16 => (5000, 20, 1, 512, 41, 73),
            FilesystemKind::Fat32 => (67_000, 520, 32, 0, 1072, 1073),
            FilesystemKind::Exfat
            | FilesystemKind::Ntfs
            | FilesystemKind::Squashfs
            | FilesystemKind::Ext4
            | FilesystemKind::Erofs => {
                panic!("not a FAT variant")
            }
        };
        let mut image = vec![0; sectors * 512];
        let boot = &mut image[..512];
        boot[11..13].copy_from_slice(&512u16.to_le_bytes());
        boot[13] = 1;
        boot[14..16].copy_from_slice(&(reserved as u16).to_le_bytes());
        boot[16] = 2;
        boot[17..19].copy_from_slice(&root_entries.to_le_bytes());
        if kind == FilesystemKind::Fat32 {
            boot[32..36].copy_from_slice(&(sectors as u32).to_le_bytes());
            boot[36..40].copy_from_slice(&(fat_sectors as u32).to_le_bytes());
            boot[44..48].copy_from_slice(&2u32.to_le_bytes());
        } else {
            boot[19..21].copy_from_slice(&(sectors as u16).to_le_bytes());
            boot[22..24].copy_from_slice(&(fat_sectors as u16).to_le_bytes());
        }
        boot[510..512].copy_from_slice(&[0x55, 0xAA]);

        let fat_offset = reserved * 512;
        match kind {
            FilesystemKind::Fat12 => {
                image[fat_offset..fat_offset + 5].copy_from_slice(&[0xF8, 0xFF, 0xFF, 0xFF, 0x0F])
            }
            FilesystemKind::Fat16 => {
                image[fat_offset + 4..fat_offset + 6].copy_from_slice(&0xFFFFu16.to_le_bytes());
            }
            FilesystemKind::Fat32 => {
                image[fat_offset + 8..fat_offset + 12]
                    .copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
                image[fat_offset + 12..fat_offset + 16]
                    .copy_from_slice(&0x0FFF_FFFFu32.to_le_bytes());
            }
            FilesystemKind::Exfat
            | FilesystemKind::Ntfs
            | FilesystemKind::Squashfs
            | FilesystemKind::Ext4
            | FilesystemKind::Erofs => {
                unreachable!()
            }
        }
        let root_start = root_sector * 512;
        image[root_start..root_start + 11].copy_from_slice(b"SAMPLE  TXT");
        image[root_start + 11] = 0x20;
        image[root_start + 26..root_start + 28].copy_from_slice(
            &(if kind == FilesystemKind::Fat32 {
                3u16
            } else {
                2u16
            })
            .to_le_bytes(),
        );
        image[root_start + 28..root_start + 32]
            .copy_from_slice(&(contents.len() as u32).to_le_bytes());
        let data_start = data_sector * 512;
        image[data_start..data_start + contents.len()].copy_from_slice(contents);
        image
    }

    fn assert_fat_image(kind: FilesystemKind, contents: &[u8]) {
        let volume = FilesystemVolume::open(TestDisk(fat_image(kind, contents))).unwrap();
        assert_eq!(volume.kind(), kind);
        let details = serde_json::to_value(volume.details()).unwrap();
        assert_eq!(details["kind"], "fat");
        assert_eq!(details["variant"], kind.to_string());
        assert_eq!(details["bytesPerSector"], 512);
        assert!(details["firstDataSector"].is_number());
        let files = volume.root_directory().unwrap();
        assert_eq!(files[0].name, "SAMPLE.TXT");
        assert_eq!(files[0].size, contents.len() as u64);
        let mut output = vec![0; contents.len()];
        assert_eq!(
            volume.read_file_at(&files[0], 0, &mut output).unwrap(),
            contents.len()
        );
        assert_eq!(output, contents);
    }

    #[test]
    fn fat12_fat16_and_fat32_dispatch_and_read_independent_images() {
        assert_fat_image(FilesystemKind::Fat12, b"from FAT12");
        assert_fat_image(FilesystemKind::Fat16, b"from FAT16");
        assert_fat_image(FilesystemKind::Fat32, b"from FAT32");
    }

    #[test]
    fn multiple_open_images_keep_reads_scoped_to_their_volume() {
        let first =
            FilesystemVolume::open(TestDisk(fat_image(FilesystemKind::Fat12, b"first disk")))
                .unwrap();
        let second =
            FilesystemVolume::open(TestDisk(fat_image(FilesystemKind::Fat16, b"second disk")))
                .unwrap();
        let first_entry = first.root_directory().unwrap().remove(0);
        let second_entry = second.root_directory().unwrap().remove(0);
        let mut first_bytes = [0; 10];
        let mut second_bytes = [0; 11];
        first
            .read_file_at(&first_entry, 0, &mut first_bytes)
            .unwrap();
        second
            .read_file_at(&second_entry, 0, &mut second_bytes)
            .unwrap();
        assert_eq!(&first_bytes, b"first disk");
        assert_eq!(&second_bytes, b"second disk");
        assert!(second
            .read_file_at(&first_entry, 0, &mut first_bytes)
            .is_err());
    }

    #[test]
    fn fat_variants_expose_geometry_bounded_volume_reads_and_cluster_reads() {
        let cases: [(FilesystemKind, &[u8], u32, u64, u32); 3] = [
            (FilesystemKind::Fat12, b"fat12 data".as_slice(), 2, 39, 4000),
            (FilesystemKind::Fat16, b"fat16 data".as_slice(), 2, 73, 5000),
            (
                FilesystemKind::Fat32,
                b"fat32 data".as_slice(),
                3,
                1073,
                67_000,
            ),
        ];
        for (kind, contents, cluster, data_sector, sectors) in cases {
            let volume = FilesystemVolume::open(TestDisk(fat_image(kind, contents))).unwrap();
            let geometry = volume.geometry();
            assert_eq!(geometry.bytes_per_sector, 512);
            assert_eq!(geometry.cluster_size, 512);
            assert!(geometry.cluster_count > 0);
            assert_eq!(geometry.volume_size, u64::from(sectors) * 512);
            assert_eq!(volume.cluster_offset(cluster).unwrap(), data_sector * 512);

            let mut raw_bytes = [0; 10];
            volume
                .read_volume_bytes(data_sector * 512, &mut raw_bytes)
                .unwrap();
            assert_eq!(&raw_bytes, contents);

            let mut cluster_bytes = [0; 512];
            volume.read_cluster(cluster, &mut cluster_bytes).unwrap();
            assert_eq!(&cluster_bytes[..contents.len()], contents);
            assert!(volume
                .read_cluster(geometry.cluster_count + 2, &mut cluster_bytes)
                .is_err());
            assert!(volume.read_cluster(cluster, &mut [0; 511]).is_err());
            assert!(volume
                .read_volume_bytes(geometry.volume_size, &mut [0; 1])
                .is_err());
        }
    }

    #[test]
    fn fat12_fat16_and_fat32_expose_normalized_salvage_primitives() {
        for kind in [
            FilesystemKind::Fat12,
            FilesystemKind::Fat16,
            FilesystemKind::Fat32,
        ] {
            let volume = FilesystemVolume::open(TestDisk(fat_image(kind, b"salvage"))).unwrap();
            let file_cluster = if kind == FilesystemKind::Fat32 { 3 } else { 2 };
            assert_eq!(
                volume.fat_entry(file_cluster).unwrap(),
                SalvageFatEntry::End
            );
            assert!(volume.is_allocated(file_cluster).unwrap());
            assert!(!volume.is_allocated(10).unwrap());
            assert_eq!(
                volume.chain_candidates(file_cluster, 8).unwrap(),
                vec![file_cluster]
            );
        }
    }

    #[test]
    fn exfat_boot_sector_is_recognized_for_driver_dispatch() {
        use fs_exfat::fixtures::{
            bitmap_entry, upcase_entry, Builder, BITMAP_CLUSTER, ROOT_CLUSTER,
        };
        use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};

        let mut builder = Builder::new();
        for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
            builder.allocate(cluster);
        }
        builder.fat(ROOT_CLUSTER, FAT_END);
        let boot = BootSector::parse(&builder.image).unwrap();
        let bitmap = bitmap_entry(BITMAP_CLUSTER, u64::MAX);
        let location = bitmap_location(&boot, &bitmap).unwrap();
        builder.directory(
            ROOT_CLUSTER,
            &[
                bitmap_entry(location.first_cluster, location.required_length),
                upcase_entry(4, &[0; 100]),
            ],
        );
        builder.put(4, 0, &[0; 100]);
        assert_eq!(
            FilesystemKind::detect_boot_sector(&builder.image[..512]).unwrap(),
            FilesystemKind::Exfat
        );
    }

    #[test]
    fn ntfs_is_identified_but_not_opened_without_its_driver() {
        let mut boot_sector = [0; 512];
        boot_sector[3..11].copy_from_slice(b"NTFS    ");
        assert_eq!(
            FilesystemKind::detect_boot_sector(&boot_sector).unwrap(),
            FilesystemKind::Ntfs
        );
        let error = FilesystemVolume::open(TestDisk(boot_sector.to_vec()))
            .err()
            .unwrap()
            .to_string();
        assert!(error.contains("NTFS driver adapter"));
    }

    #[test]
    fn common_driver_reads_all_fat_variants() {
        for kind in [
            FilesystemKind::Fat12,
            FilesystemKind::Fat16,
            FilesystemKind::Fat32,
        ] {
            let volume = driver::Driver::open(TestDisk(fat_image(kind, b"treasure"))).unwrap();
            assert_eq!(volume.geometry().format, kind.to_string());
            let files = volume.root_directory().unwrap();
            let mut output = [0; 8];
            assert_eq!(volume.read_file_at(&files[0], 0, &mut output).unwrap(), 8);
            assert_eq!(&output, b"treasure");
        }
    }
}
