//! One adapter over rust-fs-core's layers, for every driver that implements them.
//!
//! A driver that implements `OpenVolume`, `VolumeAPI`, `PrimitiveAPI` (with a `SalvageAdapter`) and `FileAPI` of
//! `fs_core::layers` plugs in here, and Captain Salvage gets, without a line of code for that format: the volume opened for
//! salvage, its geometry and health, the salvage verbs (candidates, the extents each one claims, the tree among them, the
//! damage found, a copy rebuilt from a plan), browsing by path, and the rows of the support table, each derived from what the
//! driver says it lacks (`Capabilities`) and not written by hand.
//!
//! This is the first slice. FAT is the first driver on the layers, and the tests run this beside the adapter the app uses for
//! FAT, on the same images, and compare what each says. The app's own FAT path is not switched: that waits for the other
//! drivers (ext4 first) to land on the same layers, so what is shared is shaped by more than one of them.
//!
//! What is generic and what is not. A candidate is a record, its `detail` the driver's own fields (a FAT directory entry's
//! short name, attributes and first cluster), kept whole and handed back to the driver to walk. Nothing in this module names a
//! format. The app's own notions (`SalvageEntry`, the map's ownership classes) are built from what this gives, by a later slice.

use super::Support;
use anyhow::{anyhow, Context, Result};
use fs_core_layers::layers::{
    Candidate, CandidateKind, CandidateState, Capabilities, Damage, DamageReport, DirEntry,
    ExtentWalk, Feature, FileAPI, Geometry, Integrity, MaterializeReport, NodeKind, OpenVolume,
    Parent, Plan, PrimitiveAPI, SalvageAdapter, Tree, VolumeHealth, VolumeSupport,
};
use serde::Serialize;
use std::collections::{BTreeMap, HashSet};
use std::fmt::Debug;

/// A row of the support table derived from what a driver says it can do. The shape of `filesystem::Capability`, with an
/// owned note, because the note says which features of this driver are missing.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DerivedCapability {
    pub feature: &'static str,
    pub support: Support,
    pub note: String,
}

/// A candidate as the app reads it: the fields every format has, and the driver's own `detail` as data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CandidateView {
    /// Where the record is on the source: the number that names the candidate in a tree.
    pub id: u64,
    pub record_length: u64,
    pub name: Option<String>,
    pub kind: CandidateKind,
    pub state: CandidateState,
    pub integrity: Integrity,
    pub size: Option<u64>,
    pub detail: serde_json::Value,
}

/// One candidate's place in the tree, as the paths its parent claims give it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacedCandidate {
    pub id: u64,
    pub name: Option<String>,
    /// The folders above it, joined with `/`, from the root. `None` for an orphan, whose parent is not among the candidates.
    pub folder: Option<String>,
    /// More than one parent claim: hard links, or conflicting claims.
    pub claims: usize,
}

/// A volume of any driver on the layers, opened for salvage.
pub struct GenericVolume<V: OpenVolume + PrimitiveAPI + FileAPI>
where
    <V as PrimitiveAPI>::Adapter: SalvageAdapter,
{
    volume: V,
}

type Detail<V> = <<V as PrimitiveAPI>::Adapter as SalvageAdapter>::Detail;

impl<V> GenericVolume<V>
where
    V: OpenVolume + PrimitiveAPI + FileAPI,
    <V as PrimitiveAPI>::Adapter: SalvageAdapter,
    <V as PrimitiveAPI>::Adapter: Sized,
{
    /// Opens the volume with only what salvage needs, so damage below the geometry does not refuse it.
    pub fn open_for_salvage(source: <V as OpenVolume>::Source) -> Result<Self> {
        let volume = V::open_for_salvage(source).map_err(|error| anyhow!("{error}"))?;
        Ok(Self { volume })
    }

    pub fn volume(&self) -> &V {
        &self.volume
    }

    /// The format's name, as the driver gives it: `fat32`, `exfat`, `ntfs`, `ext4`.
    pub fn format(&self) -> &'static str {
        self.volume.format()
    }

    pub fn geometry(&self) -> Result<Geometry> {
        self.volume.geometry().map_err(|error| anyhow!("{error}"))
    }

    pub fn health(&self) -> Result<VolumeHealth> {
        self.volume.health().map_err(|error| anyhow!("{error}"))
    }

    pub fn support(&self) -> Result<VolumeSupport> {
        self.volume.support().map_err(|error| anyhow!("{error}"))
    }

    /// What the driver lacks of the salvage verbs.
    pub fn salvage_capabilities(&self) -> Result<Capabilities> {
        Ok(self.volume.adapter()?.capabilities())
    }

    /// The support table's rows for this driver, derived from what it says it lacks and what this volume uses that it
    /// cannot read. A feature the driver has is Supported; one it has in part is Limited, with the part named; one it lacks
    /// is Missing, with its name. Nothing here knows a format.
    pub fn capability_rows(&self) -> Result<Vec<DerivedCapability>> {
        let support = self.support()?;
        let health = self.health()?;
        Ok(capability_rows(&support, &health))
    }

    /// Every candidate on the volume, live, deleted or damaged, in the order the driver finds them.
    pub fn candidates(&self) -> Result<Vec<Candidate<Detail<V>>>> {
        Ok(self.volume.salvage()?.all_candidates()?)
    }

    /// The number that names a candidate in a tree and in a parent claim: the driver's `candidate_id`, which is where
    /// its record starts unless the driver's records share bytes (SquashFS inodes share a metadata block).
    pub fn id(&self, candidate: &Candidate<Detail<V>>) -> u64 {
        self.volume
            .adapter()
            .map(|adapter| adapter.candidate_id(candidate))
            .unwrap_or(candidate.location.offset)
    }

    /// A candidate as the app reads it.
    pub fn view(&self, candidate: &Candidate<Detail<V>>) -> CandidateView {
        CandidateView {
            id: self.id(candidate),
            record_length: candidate.location.length,
            name: candidate.name.clone(),
            kind: candidate.kind,
            state: candidate.state,
            integrity: candidate.integrity,
            size: candidate.size,
            detail: serde_json::to_value(&candidate.detail).unwrap_or(serde_json::Value::Null),
        }
    }

    /// The storage a candidate claims, as far as its map could be followed.
    pub fn extents(&self, candidate: &Candidate<Detail<V>>) -> Result<ExtentWalk> {
        Ok(self.volume.salvage()?.walk_extents(candidate)?)
    }

    /// The parent claims among `candidates`, and the folder each one sits in, read from them.
    pub fn tree(
        &self,
        candidates: &[Candidate<Detail<V>>],
    ) -> Result<(Tree, Vec<PlacedCandidate>)> {
        let tree = self.volume.salvage()?.rebuild_tree(candidates)?;
        let ids: Vec<u64> = candidates
            .iter()
            .map(|candidate| self.id(candidate))
            .collect();
        let placed = place(candidates, &ids, &tree);
        Ok((tree, placed))
    }

    /// What the driver found wrong in the volume's own structures.
    pub fn damage(&self) -> Result<(Vec<Damage>, DamageReport)> {
        let mut found = Vec::new();
        let report = self.volume.salvage()?.report_damage(&mut found)?;
        Ok((found, report))
    }

    /// The bytes a candidate's walk rebuilds, written to `sink`, with where each came from. Holes and bytes the walk could
    /// not reach are zeros, said by the report and not by the bytes.
    pub fn copy(
        &self,
        candidate: &Candidate<Detail<V>>,
        sink: &mut dyn std::io::Write,
    ) -> Result<(ExtentWalk, MaterializeReport)> {
        let salvage = self.volume.salvage()?;
        let walk = salvage.walk_extents(candidate)?;
        let plan: Plan = walk.plan();
        let report = salvage.materialize(&plan, sink)?;
        Ok((walk, report))
    }

    /// The entries of a folder, by path from the root, through the driver's own file layer.
    pub fn list(&self, path: &str) -> Result<Vec<DirEntry<<V as FileAPI>::Node>>> {
        let folder = self
            .volume
            .lookup(path)
            .map_err(|error| anyhow!("{error}"))?;
        self.volume
            .read_dir(&folder)
            .map_err(|error| anyhow!("{error}"))
    }

    /// A file's bytes, by path from the root.
    pub fn read(&self, path: &str) -> Result<Vec<u8>> {
        let file = self
            .volume
            .lookup(path)
            .map_err(|error| anyhow!("{error}"))?;
        let mut bytes = Vec::new();
        self.volume
            .copy_to(&file, &mut bytes)
            .map_err(|error| anyhow!("{error}"))
            .with_context(|| format!("reading {path}"))?;
        Ok(bytes)
    }

    /// Every file and folder reachable from the root, as `path` and size, in order: the browse tree.
    pub fn browse(&self) -> Result<BTreeMap<String, (NodeKind, u64)>> {
        let mut found = BTreeMap::new();
        let mut queue = vec![String::new()];
        let mut seen = HashSet::new();
        while let Some(folder) = queue.pop() {
            if !seen.insert(folder.clone()) {
                continue;
            }
            for entry in self.list(&folder)? {
                let path = format!("{folder}/{}", entry.name);
                if entry.kind == NodeKind::Directory {
                    queue.push(path.clone());
                }
                found.insert(path, (entry.kind, entry.size));
            }
        }
        Ok(found)
    }
}

/// The folder of each candidate, from the parent claims: following `Parent::Candidate` up to `Parent::Root`.
fn place<D>(candidates: &[Candidate<D>], ids: &[u64], tree: &Tree) -> Vec<PlacedCandidate> {
    let names: BTreeMap<u64, Option<&str>> = candidates
        .iter()
        .zip(ids)
        .map(|(candidate, id)| (*id, candidate.name.as_deref()))
        .collect();
    let parents: BTreeMap<u64, Parent> = tree
        .nodes
        .iter()
        .filter_map(|node| {
            node.parents
                .first()
                .map(|claim| (node.candidate, claim.parent))
        })
        .collect();
    tree.nodes
        .iter()
        .map(|node| {
            let mut folders = Vec::new();
            let mut at = parents.get(&node.candidate).copied();
            let mut hops = 0;
            let mut orphan = at.is_none();
            while let Some(Parent::Candidate(id)) = at {
                hops += 1;
                if hops > 64 {
                    orphan = true;
                    break;
                }
                let Some(name) = names.get(&id) else {
                    orphan = true;
                    break;
                };
                folders.push(name.unwrap_or("").to_string());
                at = parents.get(&id).copied();
                if at.is_none() {
                    orphan = true;
                }
            }
            folders.reverse();
            PlacedCandidate {
                id: node.candidate,
                name: names
                    .get(&node.candidate)
                    .and_then(|name| name.map(str::to_string)),
                folder: (!orphan).then(|| folders.join("/")),
                claims: node.parents.len(),
            }
        })
        .collect()
}

/// The support table's rows, from what a driver says of itself.
pub fn capability_rows(support: &VolumeSupport, health: &VolumeHealth) -> Vec<DerivedCapability> {
    let lacks = |feature: Feature| support.salvage.missing.contains(&feature);
    let named = |features: &[Feature]| {
        features
            .iter()
            .filter(|feature| lacks(**feature))
            .map(|feature| feature.name())
            .collect::<Vec<_>>()
    };
    let row = |feature: &'static str, essential: &[Feature], partial: &[Feature], says: &str| {
        let missing_essential = named(essential);
        let missing_partial = named(partial);
        let (support, note) = if !missing_essential.is_empty() {
            (
                Support::Missing,
                format!("The driver lacks {}.", missing_essential.join(", ")),
            )
        } else if !missing_partial.is_empty() {
            (
                Support::Limited,
                format!("{says} The driver lacks {}.", missing_partial.join(", ")),
            )
        } else {
            (Support::Supported, says.to_string())
        };
        DerivedCapability {
            feature,
            support,
            note,
        }
    };
    let mut rows = Vec::new();
    // Open: the volume opened for salvage, with the quick verdict on its structures.
    let mut open = row(
        "open",
        &[],
        &[],
        "The volume opens for salvage, from a backup of its geometry when the first is damaged.",
    );
    if !health.is_clean() || !support.unsupported.is_empty() {
        open.support = Support::Limited;
        let mut notes = Vec::new();
        if !health.is_clean() {
            notes.push(format!("Its structures are {:?}.", health.state));
        }
        if !support.unsupported.is_empty() {
            notes.push(format!(
                "It uses {}, which the driver cannot read.",
                support.unsupported.join(", ")
            ));
        }
        open.note = format!("{} {}", open.note, notes.join(" "));
    }
    rows.push(open);
    rows.push(row(
        "scan",
        &[Feature::FindCandidates],
        &[Feature::DeletedCandidates],
        "Every record is found, live, deleted or damaged, and damaged ones are kept as evidence.",
    ));
    let mut browse = row(
        "browse",
        &[Feature::RebuildTree],
        &[],
        "The folder tree is rebuilt from the parent claims of the records.",
    );
    if !support.files && browse.support == Support::Supported {
        browse.support = Support::Limited;
        browse.note = format!(
            "{} The namespace did not validate on this volume, so it is not browsed by path.",
            browse.note
        );
    }
    rows.push(browse);
    rows.push(row(
        "export",
        &[Feature::WalkExtents, Feature::Materialize],
        &[Feature::DeletedExtents, Feature::EncodedExtents],
        "A file is rebuilt from the storage it claims, and a range that cannot be read is zeros, marked in the report.",
    ));
    rows.push(row(
        "rebuilds",
        &[Feature::Materialize],
        &[Feature::ClusterSteps],
        "A rebuild is a plan of ranges of the source.",
    ));
    rows.push(row(
        "damage_report",
        &[Feature::ReportDamage],
        &[],
        "Damage in the format's own structures is reported as evidence.",
    ));
    rows.push(row(
        "geometry_hypotheses",
        &[],
        &[],
        "A geometry the driver is given can be tried.",
    ));
    // The last is optional: a driver without hypotheses is Missing, not Limited.
    if let Some(last) = rows.last_mut() {
        if lacks(Feature::GeometryHypotheses) {
            last.support = Support::Missing;
            last.note = "The driver lacks geometry-hypotheses.".to_string();
        }
    }
    rows
}
