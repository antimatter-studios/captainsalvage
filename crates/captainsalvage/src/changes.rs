//! What changed on a volume since its baseline.
//!
//! The first time a volume is opened, its boot regions, FAT, allocation bitmap and root
//! directory are hashed, and a copy of them is kept on the Mac beside the hash. When the hash
//! later differs, comparing that copy with the disk says what was written: which clusters were
//! free then and are in use now, which FAT entries and root entries changed, and so which found
//! files lost data. Nothing here reads or writes anything but the Mac's own copy.

use crate::filesystem::{salvage, SalvageEntry as EntrySet};
use anyhow::{Context, Result};
use fs_exfat::primitives;
use fs_exfat::primitives::{
    checked_bitmap_bit as bit, checked_fat_entry as fat_entry, find_sets, Limits, FIRST_CLUSTER,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// The manifest's file name, beside the copied bytes.
const MANIFEST_FILE: &str = "manifest.json";

/// What a copy's manifest says now. Version 1 is a copy with no manifest: the five files below,
/// which every copy made before manifests was.
const MANIFEST_VERSION: u32 = 2;

/// The regions of a version 1 copy, in the order the structure hash covers them: file name and
/// what the bytes are.
const REGIONS_V1: [(&str, &str); 5] = [
    ("boot_main.bin", "the main boot region"),
    ("boot_backup.bin", "the backup boot region"),
    ("fat.bin", "the allocation table"),
    ("bitmap.bin", "the allocation bitmap"),
    ("root.bin", "the root directory"),
];

/// One named region of a copy: where its bytes are kept, how long they are, their hash and what
/// they are.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Region {
    pub name: String,
    pub file: String,
    /// Where on the volume the region's first byte was read from, in bytes, when the copy can say: the boot region at 0, the
    /// backup where the format keeps it, the table where its geometry puts it. A region made of several places (the NTFS records
    /// of the root, a FAT32 root directory in clusters) says where its first piece was. `None` for a region that has no place on
    /// the volume (an empty one), or that cannot be told from the copy (an exFAT allocation bitmap).
    pub offset: Option<u64>,
    pub length: u64,
    pub sha256: String,
    pub what: String,
}

/// What a copy holds, stored beside its bytes as `manifest.json`. The structure hash covers the
/// regions in this order, so a filesystem with other metadata regions adds regions here and not
/// fixed fields.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StructureManifest {
    pub version: u32,
    pub regions: Vec<Region>,
    /// The regions that could not be read when the copy was made, and why. Such a region is empty here.
    /// Left out when every region was read, so a copy of a volume read whole is as it always was.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unread: Vec<Unread>,
}

/// A region of a volume's structures that could not be read, and why.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Unread {
    /// The region's name, as in the manifest: `bitmap`.
    pub region: String,
    pub why: String,
}

fn hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The volume's structures, byte for byte, as the structure hash covers them.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructureCopy {
    pub main: Vec<u8>,
    pub backup: Vec<u8>,
    pub fat: Vec<u8>,
    pub bitmap: Vec<u8>,
    pub root: Vec<u8>,
    /// What could not be read, empty for every volume but one whose allocation bitmap is unreadable.
    pub unread: Vec<Unread>,
}

impl From<primitives::StructureCopy> for StructureCopy {
    fn from(copy: primitives::StructureCopy) -> Self {
        Self {
            main: copy.main,
            backup: copy.backup,
            fat: copy.fat,
            bitmap: copy.bitmap,
            root: copy.root,
            unread: Vec::new(),
        }
    }
}

impl From<StructureCopy> for primitives::StructureCopy {
    fn from(copy: StructureCopy) -> Self {
        Self {
            main: copy.main,
            backup: copy.backup,
            fat: copy.fat,
            bitmap: copy.bitmap,
            root: copy.root,
        }
    }
}

impl StructureCopy {
    /// Whether this region could not be read when the copy was made.
    pub fn is_unread(&self, region: &str) -> bool {
        self.unread.iter().any(|unread| unread.region == region)
    }

    fn parts(&self) -> [&Vec<u8>; 5] {
        [
            &self.main,
            &self.backup,
            &self.fat,
            &self.bitmap,
            &self.root,
        ]
    }

    /// What this copy holds, region by region, with where on the volume each was read from where that is known.
    pub fn manifest(&self) -> StructureManifest {
        let offsets = salvage::region_offsets(self);
        StructureManifest {
            version: MANIFEST_VERSION,
            regions: REGIONS_V1
                .iter()
                .zip(self.parts())
                .zip(offsets)
                .map(|(((file, what), part), offset)| Region {
                    name: file.trim_end_matches(".bin").to_string(),
                    file: (*file).to_string(),
                    offset,
                    length: part.len() as u64,
                    sha256: hex(part),
                    what: (*what).to_string(),
                })
                .collect(),
            unread: self.unread.clone(),
        }
    }

    /// The hash of the regions, in the manifest's order. It is the same for a version 1 copy and
    /// for the version 2 copy of the same bytes, so a baseline hash recorded before manifests
    /// still matches.
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
        let manifest = self.manifest();
        for (region, part) in manifest.regions.iter().zip(self.parts()) {
            fs::write(dir.join(&region.file), part)
                .with_context(|| format!("writing {}", region.file))?;
        }
        // The manifest goes last: a copy that stopped part way has none, so it reads as a
        // version 1 copy, and is then kept only if its bytes still hash to the baseline.
        fs::write(
            dir.join(MANIFEST_FILE),
            serde_json::to_string_pretty(&manifest)?,
        )
        .with_context(|| format!("writing {MANIFEST_FILE}"))?;
        Ok(())
    }

    /// The copy in `dir`, if every region is there. A copy with no manifest is version 1 and is
    /// read as it is, never rewritten. One with a manifest must hold exactly the regions it
    /// names, each with the length and hash it records.
    pub fn load(dir: &Path) -> Option<Self> {
        let manifest = match fs::read_to_string(dir.join(MANIFEST_FILE)) {
            Ok(text) => serde_json::from_str::<StructureManifest>(&text).ok()?,
            Err(_) => Self::version_one(dir)?,
        };
        if manifest.version > MANIFEST_VERSION {
            return None;
        }
        let mut parts = Vec::new();
        for (region, (file, _)) in manifest.regions.iter().zip(REGIONS_V1) {
            if region.file != file {
                return None;
            }
            let bytes = fs::read(dir.join(&region.file)).ok()?;
            if bytes.len() as u64 != region.length || hex(&bytes) != region.sha256 {
                return None;
            }
            parts.push(bytes);
        }
        if manifest.regions.len() != REGIONS_V1.len() {
            return None;
        }
        let mut parts = parts.into_iter();
        Some(Self {
            main: parts.next()?,
            backup: parts.next()?,
            fat: parts.next()?,
            bitmap: parts.next()?,
            root: parts.next()?,
            unread: manifest.unread,
        })
    }

    /// The manifest a copy with only its five files has, worked out from the files.
    fn version_one(dir: &Path) -> Option<StructureManifest> {
        let regions = REGIONS_V1
            .iter()
            .map(|(file, what)| {
                let bytes = fs::read(dir.join(file)).ok()?;
                Some(Region {
                    name: file.trim_end_matches(".bin").to_string(),
                    file: (*file).to_string(),
                    offset: None,
                    length: bytes.len() as u64,
                    sha256: hex(&bytes),
                    what: (*what).to_string(),
                })
            })
            .collect::<Option<Vec<_>>>()?;
        Some(StructureManifest {
            version: 1,
            regions,
            unread: Vec::new(),
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

fn root_changes(before: &[u8], after: &[u8], limits: Limits) -> Vec<RootChange> {
    entry_changes(
        find_sets(before, before.len(), limits)
            .into_iter()
            .map(EntrySet::from)
            .collect(),
        find_sets(after, after.len(), limits)
            .into_iter()
            .map(EntrySet::from)
            .collect(),
    )
}

fn entry_changes(before: Vec<EntrySet>, after: Vec<EntrySet>) -> Vec<RootChange> {
    let sets = |entries: Vec<EntrySet>| -> Vec<(usize, EntrySet)> {
        entries
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
    // With either bitmap unreadable nothing can be said of which clusters were written.
    let comparable = !baseline.is_unread("bitmap") && !current.is_unread("bitmap");
    for cluster in FIRST_CLUSTER..last_cluster_exclusive {
        if !comparable {
            break;
        }
        match (
            bit(&baseline.bitmap, cluster),
            bit(&current.bitmap, cluster),
        ) {
            (false, true) => written.push(cluster),
            (true, false) => freed += 1,
            _ => {}
        }
    }
    let fat_entries = (0..)
        .map(|cluster| {
            (
                fat_entry(&baseline.fat, cluster),
                fat_entry(&current.fat, cluster),
            )
        })
        .take_while(|(before, after)| before.is_some() || after.is_some())
        .filter(|(before, after)| before != after)
        .count() as u64;
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

pub fn compare_volume(
    baseline: &StructureCopy,
    current: &StructureCopy,
    limits: Limits,
    last: u32,
) -> Result<StructureChanges> {
    let before = salvage::inspect(baseline)?;
    if before.kind.keeps_legacy_report_shape() {
        return Ok(compare(baseline, current, limits, last));
    }
    let after = salvage::inspect(current)?;
    let mut changes = compare(
        &salvage::comparison_copy(baseline)?,
        &salvage::comparison_copy(current)?,
        limits,
        last,
    );
    changes.current_hash = current.hash();
    changes.fat_entries = salvage::changed_fat_entries(baseline, current)?;
    changes.root_entries = entry_changes(
        salvage::find_entries(
            before.kind,
            &before.geometry,
            &before.fat,
            &baseline.root,
            baseline.root.len(),
        ),
        salvage::find_entries(
            after.kind,
            &after.geometry,
            &after.fat,
            &current.root,
            current.root.len(),
        ),
    );
    Ok(changes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy() -> StructureCopy {
        StructureCopy {
            main: vec![1; 512],
            backup: vec![2; 512],
            fat: vec![3; 1024],
            bitmap: vec![4; 64],
            root: vec![5; 96],
            unread: Vec::new(),
        }
    }

    fn folder(label: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "captainsalvage-manifest-{}-{label}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A copy as the code before manifests kept it: five files, in the order the hash covers.
    fn write_version_one(dir: &Path, copy: &StructureCopy) {
        for (file, part) in [
            "boot_main.bin",
            "boot_backup.bin",
            "fat.bin",
            "bitmap.bin",
            "root.bin",
        ]
        .iter()
        .zip(copy.parts())
        {
            fs::write(dir.join(file), part).unwrap();
        }
    }

    #[test]
    fn a_saved_copy_has_a_manifest_naming_every_region() {
        let dir = folder("saved");
        let copy = copy();
        copy.save(&dir).unwrap();
        let manifest: StructureManifest =
            serde_json::from_str(&fs::read_to_string(dir.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest, copy.manifest());
        assert_eq!(manifest.version, 2);
        let names: Vec<&str> = manifest.regions.iter().map(|r| r.name.as_str()).collect();
        assert_eq!(names, ["boot_main", "boot_backup", "fat", "bitmap", "root"]);
        assert_eq!(manifest.regions[2].length, 1024);
        assert_eq!(manifest.regions[2].sha256, hex(&copy.fat));
        assert_eq!(StructureCopy::load(&dir), Some(copy));
    }

    #[test]
    fn a_region_that_could_not_be_read_is_named_in_the_manifest_and_comes_back() {
        let dir = folder("unread");
        let mut copy = copy();
        copy.bitmap = Vec::new();
        copy.unread = vec![Unread {
            region: "bitmap".into(),
            why: "$Bitmap has no data stream".into(),
        }];
        copy.save(&dir).unwrap();
        let text = fs::read_to_string(dir.join("manifest.json")).unwrap();
        assert!(text.contains("\"unread\""), "{text}");
        let loaded = StructureCopy::load(&dir).expect("a copy with an empty region loads");
        assert_eq!(loaded, copy);
        assert!(loaded.is_unread("bitmap") && !loaded.is_unread("fat"));
        // A copy with every region read has no such field, so it is written as it always was.
        let whole = folder("read");
        self::copy().save(&whole).unwrap();
        assert!(!fs::read_to_string(whole.join("manifest.json"))
            .unwrap()
            .contains("unread"));
    }

    #[test]
    fn nothing_is_said_written_when_either_bitmap_was_not_read() {
        let limits = Limits {
            cluster_count: 8,
            cluster_size: 4096,
        };
        let mut baseline = copy();
        baseline.bitmap = vec![0b0000_1111];
        let mut current = copy();
        current.bitmap = vec![0b1111_1111];
        assert_eq!(
            compare(&baseline, &current, limits, 10)
                .written_clusters
                .len(),
            4
        );
        baseline.bitmap = Vec::new();
        baseline.unread = vec![Unread {
            region: "bitmap".into(),
            why: "unreadable".into(),
        }];
        let changes = compare(&baseline, &current, limits, 10);
        assert!(changes.written_clusters.is_empty() && changes.freed_clusters == 0);
        let changes = compare(&current, &baseline, limits, 10);
        assert!(changes.written_clusters.is_empty() && changes.freed_clusters == 0);
    }

    #[test]
    fn a_copy_made_before_manifests_is_version_one_and_is_not_rewritten() {
        let dir = folder("version-one");
        let copy = copy();
        write_version_one(&dir, &copy);
        let before: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(StructureCopy::load(&dir), Some(copy.clone()));
        // Reading it adds nothing and changes nothing.
        let mut after: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        let mut before = before;
        before.sort();
        after.sort();
        assert_eq!(before, after);
        assert!(!dir.join("manifest.json").exists());
        assert_eq!(
            StructureCopy::version_one(&dir).unwrap().version,
            1,
            "a copy with no manifest is version 1"
        );
        // The hash is the one recorded for the same bytes before manifests existed.
        let mut hasher = Sha256::new();
        for part in copy.parts() {
            hasher.update(part);
        }
        assert_eq!(
            copy.hash(),
            hasher
                .finalize()
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        );
    }

    #[test]
    fn a_copy_that_does_not_match_its_manifest_is_not_read() {
        let dir = folder("tampered");
        copy().save(&dir).unwrap();
        fs::write(dir.join("fat.bin"), vec![9; 1024]).unwrap();
        assert_eq!(StructureCopy::load(&dir), None);
        copy().save(&dir).unwrap();
        fs::remove_file(dir.join("root.bin")).unwrap();
        assert_eq!(StructureCopy::load(&dir), None);
        copy().save(&dir).unwrap();
        let mut manifest = copy().manifest();
        manifest.version = 3;
        fs::write(
            dir.join("manifest.json"),
            serde_json::to_string(&manifest).unwrap(),
        )
        .unwrap();
        assert_eq!(
            StructureCopy::load(&dir),
            None,
            "a manifest from a newer version is not guessed at"
        );
    }
}
