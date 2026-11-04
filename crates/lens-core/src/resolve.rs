//! Turns raw scan results into a best-effort picture of the volume.
//!
//! Nothing here is a yes-or-no gate. A damaged entry is weighed, not discarded,
//! and is followed when what it points at checks out. Every file gets a
//! confidence score and a plain-language reason. Overlays and parameters feed in,
//! so a hypothesis can be applied, examined and withdrawn at no cost to the disk.

use crate::content::{
    content_match, expected_class, text_head_match, ContentMatch, CLASS_DIRECTORY, CLASS_NAMES,
    CLASS_TEXT, CLASS_ZEROS,
};
use crate::exfat::FIRST_CLUSTER;
use crate::map::{
    from_extents, Extent, MapDb, MapSummary, Overlay, ResolveParams, OVERLAY_ACCEPT_ENTRY,
    OVERLAY_DIRECTORY_CLUSTER, OVERLAY_FILE_EXTENTS, OVERLAY_FOLDER_PARENT, OVERLAY_REGION_LABEL,
    VERDICT_DAMAGED, VERDICT_EMPTY, VERDICT_FOLDER, VERDICT_FOLDER_EMPTY, VERDICT_FOLDER_MISSING,
    VERDICT_INTACT, VERDICT_LIVE, VERDICT_OVERWRITTEN, VERDICT_PARTIAL, VERDICT_PLAUSIBLE,
    VERDICT_RESIDUE,
};
use crate::volume::{StreamClusters, Volume};
use crate::workspace::Workspace;
use anyhow::Result;
use rusqlite::params;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::time::Instant;

/// How much an entry itself can be believed, before looking at its data.
const TRUST_VALID: f64 = 1.0;
const TRUST_ACCEPTED_BY_OVERLAY: f64 = 0.9;
/// A failed checksum, but what the entry points at matches what it claims.
const TRUST_DAMAGED_CONFIRMED: f64 = 0.8;
const TRUST_DAMAGED: f64 = 0.5;

const CONTENT_MATCHES: f64 = 1.0;
const CONTENT_INCONCLUSIVE: f64 = 0.7;
const CONTENT_CONTRADICTS: f64 = 0.15;
const CHAIN_GUESSED_PENALTY: f64 = 0.75;
const FOLDER_NOT_FOUND_PENALTY: f64 = 0.2;
const LINK_OVERLAY: f64 = 0.9;
const CONFIDENCE_SCALE: f64 = 255.0;

pub const OWNERSHIP_UNEXPLAINED: u8 = 0;
pub const OWNERSHIP_ONE_ENTRY: u8 = 1;
pub const OWNERSHIP_SEVERAL_ENTRIES: u8 = 2;
pub const OWNERSHIP_DIRECTORY: u8 = 3;
pub const OWNERSHIP_NAMELESS_CHAIN: u8 = 4;
/// Nothing on disk claims it, but a hypothesis says what it is.
pub const OWNERSHIP_LABELLED: u8 = 5;

struct LoadedSet {
    id: i64,
    source_cluster: u32,
    name: String,
    in_use: bool,
    checksum_ok: bool,
    accepted: bool,
    is_dir: bool,
    first_cluster: u32,
    length: u64,
    no_fat_chain: bool,
    modified: Option<String>,
}

#[derive(Clone, Copy, Default)]
struct Totals {
    files: u64,
    bytes: u64,
    intact_files: u64,
    intact_bytes: u64,
}

struct Assessment {
    verdict: &'static str,
    reason: String,
    confidence: f64,
    needed: u64,
    found: u64,
    allocated: u64,
    conflict: u64,
}

impl Assessment {
    fn plain(verdict: &'static str, reason: impl Into<String>, confidence: f64) -> Self {
        Self {
            verdict,
            reason: reason.into(),
            confidence,
            needed: 0,
            found: 0,
            allocated: 0,
            conflict: 0,
        }
    }
}

fn payload_u32(payload: &serde_json::Value, key: &str) -> Option<u32> {
    payload
        .get(key)
        .and_then(serde_json::Value::as_u64)
        .map(|value| value as u32)
}

/// Parses a `file_extents` overlay payload into a cluster list.
pub fn parse_extents_payload(payload: &serde_json::Value) -> Option<Vec<u32>> {
    let extents: Vec<Extent> = serde_json::from_value(payload.get("extents")?.clone()).ok()?;
    Some(from_extents(&extents))
}

/// Applies each active accept_entry overlay to the in-memory entries. Returns
/// the corrections, which are stored beside the raw values for display.
fn apply_entry_overlays(
    sets: &mut [LoadedSet],
    overlays: &[Overlay],
) -> Vec<(i64, serde_json::Value)> {
    let positions: HashMap<i64, usize> = sets
        .iter()
        .enumerate()
        .map(|(index, set)| (set.id, index))
        .collect();
    let mut corrections = Vec::new();
    for overlay in overlays
        .iter()
        .rev()
        .filter(|overlay| overlay.kind == OVERLAY_ACCEPT_ENTRY)
    {
        let Some(id) = overlay.target else { continue };
        let Some(&index) = positions.get(&id) else {
            continue;
        };
        let set = &mut sets[index];
        let payload = &overlay.payload;
        set.accepted = true;
        set.checksum_ok = true;
        if let Some(name) = payload.get("name").and_then(serde_json::Value::as_str) {
            set.name = name.to_string();
        }
        if let Some(first) = payload_u32(payload, "firstCluster") {
            set.first_cluster = first;
        }
        if let Some(length) = payload.get("length").and_then(serde_json::Value::as_u64) {
            set.length = length;
        }
        if let Some(no_fat) = payload
            .get("noFatChain")
            .and_then(serde_json::Value::as_bool)
        {
            set.no_fat_chain = no_fat;
        }
        if let Some(is_dir) = payload.get("isDir").and_then(serde_json::Value::as_bool) {
            set.is_dir = is_dir;
        }
        corrections.push((id, payload.clone()));
    }
    corrections
}

/// Works out which clusters an entry claims, honouring overlays and parameters.
struct Streams<'a> {
    volume: &'a Volume,
    params: ResolveParams,
    overrides: HashMap<i64, Vec<u32>>,
}

impl Streams<'_> {
    /// Returns the claim, and how many trailing clusters were assumed, not known.
    fn claim(&self, set: &LoadedSet) -> (StreamClusters, u64) {
        if let Some(clusters) = self.overrides.get(&set.id) {
            let needed = set
                .length
                .div_ceil(self.volume.boot.cluster_size())
                .max(clusters.len() as u64);
            return (
                StreamClusters {
                    clusters: clusters.clone(),
                    needed,
                },
                0,
            );
        }
        let mut stream = self
            .volume
            .clusters_for(set.first_cluster, set.length, set.no_fat_chain);
        let mut assumed = 0;
        if self.params.chain_fallback_contiguous && !stream.clusters.is_empty() {
            let mut next = stream.clusters[stream.clusters.len() - 1] + 1;
            while (stream.clusters.len() as u64) < stream.needed && self.volume.valid_cluster(next)
            {
                stream.clusters.push(next);
                next += 1;
                assumed += 1;
            }
        }
        (stream, assumed)
    }
}

fn counts_as_file(verdict: &str) -> bool {
    matches!(
        verdict,
        VERDICT_LIVE
            | VERDICT_INTACT
            | VERDICT_PLAUSIBLE
            | VERDICT_PARTIAL
            | VERDICT_OVERWRITTEN
            | VERDICT_DAMAGED
    )
}

/// Cuts one edge from any loop in the parent map, leaving a forest.
fn break_cycles(parent: &mut HashMap<u32, Option<u32>>) {
    let mut finished: HashSet<u32> = HashSet::new();
    let nodes: Vec<u32> = parent.keys().copied().collect();
    for start in nodes {
        let mut trail: HashSet<u32> = HashSet::new();
        let mut current = start;
        loop {
            if finished.contains(&current) {
                break;
            }
            if !trail.insert(current) {
                parent.insert(current, None);
                break;
            }
            match parent.get(&current).copied().flatten() {
                Some(next) => current = next,
                None => break,
            }
        }
        finished.extend(trail);
    }
}

/// Labels each node: the root, reachable from it, attached to an orphan, or an orphan.
fn classify_nodes(
    parent: &HashMap<u32, Option<u32>>,
    root_head: u32,
) -> HashMap<u32, &'static str> {
    let mut kinds = HashMap::with_capacity(parent.len());
    for (&node, &up) in parent {
        let mut current = node;
        while let Some(next) = parent.get(&current).copied().flatten() {
            current = next;
        }
        let kind = if node == root_head {
            "root"
        } else if current == root_head {
            "live"
        } else if up.is_none() {
            "orphan"
        } else {
            "attached"
        };
        kinds.insert(node, kind);
    }
    kinds
}

struct Context<'a> {
    volume: &'a Volume,
    classes: &'a [u8],
    text_heads: &'a [u8],
    claims: &'a [u16],
    node_heads: &'a HashSet<u32>,
    params: ResolveParams,
}

impl Context<'_> {
    /// Whether a file's first bytes fit its name, and how to describe them. A
    /// text file smaller than a cluster is judged only by its own bytes, because
    /// the rest of its cluster can still hold data from before it was written.
    fn content_of(&self, set: &LoadedSet) -> (ContentMatch, String) {
        let class = self.class_at(set.first_cluster);
        let held = CLASS_NAMES[usize::from(class)].to_string();
        let expected = expected_class(&set.name);
        if expected != Some(CLASS_TEXT) || !self.volume.valid_cluster(set.first_cluster) {
            return (content_match(expected, class), held);
        }
        let head = self
            .text_heads
            .get((set.first_cluster - FIRST_CLUSTER) as usize)
            .copied()
            .unwrap_or(0);
        if head != 0 {
            return match text_head_match(head, set.length) {
                ContentMatch::Yes => (ContentMatch::Yes, "readable text".to_string()),
                ContentMatch::No => (ContentMatch::No, "not text in its first bytes".to_string()),
                ContentMatch::Unknown => (
                    ContentMatch::Unknown,
                    "only partly readable text".to_string(),
                ),
            };
        }
        if set.length < self.volume.boot.cluster_size() {
            let verdict = match content_match(expected, class) {
                ContentMatch::No if class == CLASS_DIRECTORY => ContentMatch::No,
                _ => ContentMatch::Unknown,
            };
            return (verdict, "not yet measured".to_string());
        }
        (content_match(expected, class), held)
    }

    fn class_at(&self, cluster: u32) -> u8 {
        if self.volume.valid_cluster(cluster) {
            self.classes
                .get((cluster - FIRST_CLUSTER) as usize)
                .copied()
                .unwrap_or(0)
        } else {
            0
        }
    }

    fn assess_folder(&self, set: &LoadedSet, trust: f64) -> Assessment {
        let prefix = if set.checksum_ok {
            ""
        } else {
            "Entry checksum failed, but it points at a real directory. "
        };
        if self.node_heads.contains(&set.first_cluster) {
            let confidence = if set.checksum_ok {
                trust
            } else {
                TRUST_DAMAGED_CONFIRMED
            };
            return Assessment::plain(
                VERDICT_FOLDER,
                format!("{prefix}Folder contents found."),
                confidence,
            );
        }
        let class = self.class_at(set.first_cluster);
        if class == CLASS_ZEROS {
            return Assessment::plain(
                VERDICT_FOLDER_EMPTY,
                "Folder cluster is all zeros: the folder was empty, or was cleared.",
                trust * FOLDER_NOT_FOUND_PENALTY,
            );
        }
        Assessment::plain(
            VERDICT_FOLDER_MISSING,
            format!(
                "Folder cluster holds no directory entries. It now holds: {}.",
                CLASS_NAMES[usize::from(class)]
            ),
            trust * FOLDER_NOT_FOUND_PENALTY,
        )
    }

    fn assess_file(
        &self,
        set: &LoadedSet,
        stream: &StreamClusters,
        assumed: u64,
        node_kind: Option<&str>,
    ) -> Assessment {
        let found = stream.clusters.len() as u64;
        let allocated = stream
            .clusters
            .iter()
            .filter(|&&c| self.volume.is_allocated(c))
            .count() as u64;
        let overlapping = stream
            .clusters
            .iter()
            .filter(|&&c| self.claims[c as usize] > 1)
            .count() as u64;
        let conflict = if self.params.ignore_entry_conflicts {
            0
        } else {
            overlapping.saturating_sub(self.params.conflict_tolerance_clusters)
        };
        let build = |verdict, reason: String, confidence: f64| Assessment {
            verdict,
            reason,
            confidence,
            needed: stream.needed,
            found,
            allocated,
            conflict,
        };

        if set.in_use
            && set.checksum_ok
            && matches!(node_kind, Some("root") | Some("live"))
            && found == stream.needed
            && allocated == found
        {
            return build(
                VERDICT_LIVE,
                "Visible in the current filesystem.".into(),
                1.0,
            );
        }

        let (content, held) = self.content_of(set);
        let trust = if set.accepted {
            TRUST_ACCEPTED_BY_OVERLAY
        } else if set.checksum_ok {
            TRUST_VALID
        } else if content == ContentMatch::Yes {
            TRUST_DAMAGED_CONFIRMED
        } else {
            TRUST_DAMAGED
        };
        let content_factor = match content {
            ContentMatch::Yes => CONTENT_MATCHES,
            ContentMatch::Unknown => CONTENT_INCONCLUSIVE,
            ContentMatch::No => CONTENT_CONTRADICTS,
        };
        let coverage = if stream.needed == 0 {
            0.0
        } else {
            found as f64 / stream.needed as f64
        };
        let clean = if found == 0 {
            0.0
        } else {
            1.0 - ((allocated + conflict) as f64 / found as f64).min(1.0)
        };
        let guessed = if assumed > 0 {
            CHAIN_GUESSED_PENALTY
        } else {
            1.0
        };
        let confidence = trust * coverage * clean * content_factor * guessed;

        let mut sentences = Vec::new();
        if set.accepted {
            sentences.push("An overlay accepted this entry.".to_string());
        } else if !set.checksum_ok {
            sentences.push("Entry checksum failed, so its fields may be damaged.".to_string());
        }
        if assumed > 0 {
            sentences.push(format!(
                "Last {assumed} clusters are assumed contiguous, because the FAT chain ends early."
            ));
        }
        let verdict = if found == 0 {
            sentences.push("First cluster is outside the volume.".to_string());
            VERDICT_PARTIAL
        } else if found < stream.needed {
            sentences.push(format!(
                "FAT chain ends early: {found} of {} clusters reachable.",
                stream.needed
            ));
            VERDICT_PARTIAL
        } else if allocated > 0 {
            sentences.push(format!(
                "{allocated} of {found} clusters are in use by current files."
            ));
            VERDICT_PARTIAL
        } else if conflict > 0 {
            sentences.push(format!(
                "{conflict} of {found} clusters are also claimed by other entries."
            ));
            VERDICT_PARTIAL
        } else {
            match content {
                ContentMatch::Yes => {
                    sentences.push(format!(
                        "Clusters free and unclaimed. First cluster is {held}, matching the name."
                    ));
                    VERDICT_INTACT
                }
                ContentMatch::No => {
                    sentences.push(format!(
                        "First cluster holds {held}, which contradicts the name."
                    ));
                    VERDICT_OVERWRITTEN
                }
                ContentMatch::Unknown => {
                    sentences.push(format!(
                        "Clusters free and unclaimed. First cluster is {held}, which neither confirms nor contradicts the name."
                    ));
                    VERDICT_PLAUSIBLE
                }
            }
        };
        let verdict = if set.checksum_ok {
            verdict
        } else {
            VERDICT_DAMAGED
        };
        build(verdict, sentences.join(" "), confidence)
    }
}

impl MapDb {
    /// Rebuilds the folder tree and scores every entry. Safe to run repeatedly,
    /// including while a scan is in progress.
    pub fn resolve(&mut self, volume: &Volume, workspace: &Workspace) -> Result<MapSummary> {
        let started = Instant::now();
        let count = volume.boot.cluster_count;
        let classes = workspace.read_cluster_map(&workspace.classes(), count);
        let text_heads = workspace.read_cluster_map(&workspace.text_heads(), count);
        let entropy = workspace.read_cluster_map(&workspace.entropy(), count);
        let params = self.params()?;
        let overlays = self.overlays(true)?;
        let mut sets = self.load_sets()?;
        let corrections = apply_entry_overlays(&mut sets, &overlays);
        let overrides: HashMap<i64, Vec<u32>> = overlays
            .iter()
            .rev()
            .filter(|overlay| overlay.kind == OVERLAY_FILE_EXTENTS)
            .filter_map(|overlay| Some((overlay.target?, parse_extents_payload(&overlay.payload)?)))
            .collect();
        let streams = Streams {
            volume,
            params,
            overrides,
        };
        let root_head = volume
            .root_clusters
            .first()
            .copied()
            .unwrap_or(volume.boot.root_cluster);

        // Clusters holding at least one trustworthy entry set are real directories.
        let found: HashSet<u32> = sets
            .iter()
            .filter(|set| set.checksum_ok)
            .map(|set| set.source_cluster)
            .collect();

        // A damaged folder entry still counts if it points at a real directory.
        // Trustworthy entries rank first, so they win any disagreement.
        let mut folder_sets: Vec<&LoadedSet> = sets
            .iter()
            .filter(|set| {
                set.is_dir
                    && set.length > 0
                    && (set.checksum_ok || found.contains(&set.first_cluster))
            })
            .collect();
        folder_sets.sort_by(|a, b| {
            b.checksum_ok
                .cmp(&a.checksum_ok)
                .then_with(|| b.in_use.cmp(&a.in_use))
                .then_with(|| b.modified.cmp(&a.modified))
        });

        let mut head_of: HashMap<u32, u32> = HashMap::new();
        for &cluster in &volume.root_clusters {
            head_of.insert(cluster, root_head);
        }
        for overlay in overlays
            .iter()
            .filter(|overlay| overlay.kind == OVERLAY_DIRECTORY_CLUSTER)
        {
            if let (Some(cluster), Some(folder)) =
                (overlay.target, payload_u32(&overlay.payload, "head"))
            {
                head_of.insert(cluster as u32, folder);
            }
        }
        for set in folder_sets.iter().filter(|set| set.checksum_ok) {
            let (stream, _) = streams.claim(set);
            for cluster in stream.clusters {
                if found.contains(&cluster) {
                    head_of.entry(cluster).or_insert(set.first_cluster);
                }
            }
        }
        let head = |cluster: u32| head_of.get(&cluster).copied().unwrap_or(cluster);

        let mut node_heads: HashSet<u32> = found.iter().map(|&cluster| head(cluster)).collect();
        node_heads.insert(root_head);

        // A cluster holding only damaged entries still becomes a folder, as long
        // as one of those entries points at data that matches what it claims.
        for set in sets.iter().filter(|set| !set.checksum_ok) {
            let confirmed = if set.is_dir {
                found.contains(&set.first_cluster)
            } else {
                let class = if volume.valid_cluster(set.first_cluster) {
                    classes[(set.first_cluster - FIRST_CLUSTER) as usize]
                } else {
                    0
                };
                set.length > 0
                    && content_match(expected_class(&set.name), class) == ContentMatch::Yes
            };
            if confirmed {
                node_heads.insert(head(set.source_cluster));
            }
        }

        // Each node takes its name and parent from the best entry pointing at it.
        let mut parent: HashMap<u32, Option<u32>> =
            node_heads.iter().map(|&node| (node, None)).collect();
        let mut names: HashMap<u32, &LoadedSet> = HashMap::new();
        for set in &folder_sets {
            if set.first_cluster == root_head || !node_heads.contains(&set.first_cluster) {
                continue;
            }
            if names.contains_key(&set.first_cluster) {
                continue;
            }
            let source_head = head(set.source_cluster);
            if source_head == set.first_cluster || !node_heads.contains(&source_head) {
                continue;
            }
            names.insert(set.first_cluster, set);
            parent.insert(set.first_cluster, Some(source_head));
        }

        // A folder_parent overlay outranks whatever the entries say.
        let mut name_overrides: HashMap<u32, String> = HashMap::new();
        let mut linked_by_overlay: HashSet<u32> = HashSet::new();
        for overlay in overlays
            .iter()
            .rev()
            .filter(|overlay| overlay.kind == OVERLAY_FOLDER_PARENT)
        {
            let Some(folder) = overlay.target.map(|value| value as u32) else {
                continue;
            };
            node_heads.insert(folder);
            let new_parent = payload_u32(&overlay.payload, "parent");
            if let Some(value) = new_parent {
                node_heads.insert(value);
                parent.entry(value).or_insert(None);
            }
            parent.insert(folder, new_parent);
            linked_by_overlay.insert(folder);
            if let Some(name) = overlay
                .payload
                .get("name")
                .and_then(serde_json::Value::as_str)
            {
                name_overrides.insert(folder, name.to_string());
            }
        }
        break_cycles(&mut parent);
        let kinds = classify_nodes(&parent, root_head);

        // Count how many distinct streams claim each cluster.
        let last = volume.boot.last_cluster_exclusive() as usize;
        let mut claims = vec![0u16; last];
        let mut ownership = vec![OWNERSHIP_UNEXPLAINED; count as usize];
        for &cluster in &found {
            if volume.valid_cluster(cluster) {
                claims[cluster as usize] = claims[cluster as usize].saturating_add(1);
                ownership[(cluster - FIRST_CLUSTER) as usize] = OWNERSHIP_DIRECTORY;
            }
        }
        let in_use_first: HashSet<u32> = sets
            .iter()
            .filter(|set| !set.is_dir && set.checksum_ok && set.in_use)
            .map(|set| set.first_cluster)
            .collect();
        let mut claimed_first: HashSet<u32> = HashSet::new();
        for set in sets
            .iter()
            .filter(|set| !set.is_dir && set.checksum_ok && set.length > 0)
        {
            if claimed_first.insert(set.first_cluster) {
                let (stream, _) = streams.claim(set);
                for cluster in stream.clusters {
                    let index = (cluster - FIRST_CLUSTER) as usize;
                    claims[cluster as usize] = claims[cluster as usize].saturating_add(1);
                    if ownership[index] != OWNERSHIP_DIRECTORY {
                        ownership[index] = if claims[cluster as usize] > 1 {
                            OWNERSHIP_SEVERAL_ENTRIES
                        } else {
                            OWNERSHIP_ONE_ENTRY
                        };
                    }
                }
            }
        }

        let context = Context {
            volume,
            classes: &classes,
            text_heads: &text_heads,
            claims: &claims,
            node_heads: &node_heads,
            params,
        };

        let mut owners_by_first: HashMap<u32, u32> = HashMap::new();
        for set in sets.iter().filter(|set| set.checksum_ok && set.length > 0) {
            *owners_by_first.entry(set.first_cluster).or_insert(0) += 1;
        }

        // Everything is worked out before taking the write lock, so a rebuild holds
        // up a running scan only for the moment it takes to store the results.
        let mut confidence_map = vec![0u8; count as usize];
        let mut totals: HashMap<u32, Totals> = HashMap::new();
        let mut results = Vec::with_capacity(sets.len());
        for set in &sets {
            let source_head = head(set.source_cluster);
            let node = node_heads.contains(&source_head).then_some(source_head);
            let node_kind = node.and_then(|value| kinds.get(&value).copied());
            let trust = if set.accepted {
                TRUST_ACCEPTED_BY_OVERLAY
            } else if set.checksum_ok {
                TRUST_VALID
            } else {
                TRUST_DAMAGED
            };
            let assessment = if set.is_dir {
                context.assess_folder(set, trust)
            } else if set.length == 0 {
                Assessment::plain(VERDICT_EMPTY, "Zero-length file.", trust)
            } else if !set.in_use && set.checksum_ok && in_use_first.contains(&set.first_cluster) {
                Assessment::plain(
                    VERDICT_RESIDUE,
                    "Deleted entry for clusters a live entry also uses: a leftover from a rename, such as an rsync temp file.",
                    0.0,
                )
            } else {
                let (stream, assumed) = streams.claim(set);
                let assessment = context.assess_file(set, &stream, assumed, node_kind);
                let scaled = (assessment.confidence * CONFIDENCE_SCALE)
                    .round()
                    .min(CONFIDENCE_SCALE) as u8;
                for cluster in &stream.clusters {
                    let index = (*cluster - FIRST_CLUSTER) as usize;
                    confidence_map[index] = confidence_map[index].max(scaled);
                }
                assessment
            };
            if let Some(node) = node {
                if counts_as_file(assessment.verdict) && !set.is_dir {
                    let total = totals.entry(node).or_default();
                    total.files += 1;
                    total.bytes += set.length;
                    if matches!(assessment.verdict, VERDICT_INTACT | VERDICT_PLAUSIBLE) {
                        total.intact_files += 1;
                        total.intact_bytes += set.length;
                    }
                }
            }
            results.push((set.id, node, assessment));
        }

        // Add each node's own totals to every ancestor.
        let mut cumulative: HashMap<u32, Totals> = HashMap::new();
        for (&node, total) in &totals {
            let mut current = Some(node);
            while let Some(value) = current {
                let entry = cumulative.entry(value).or_default();
                entry.files += total.files;
                entry.bytes += total.bytes;
                entry.intact_files += total.intact_files;
                entry.intact_bytes += total.intact_bytes;
                current = parent.get(&value).copied().flatten();
            }
        }

        // Summarise FAT islands, including chains that no entry claims.
        let islands = volume.chain_islands();
        let mut island_rows = Vec::with_capacity(islands.len());
        for island in &islands {
            let owners = owners_by_first.get(&island.head).copied().unwrap_or(0);
            let mut sum = 0u64;
            let mut current = island.head;
            for _ in 0..island.length {
                let index = (current - FIRST_CLUSTER) as usize;
                sum += u64::from(entropy[index]);
                if owners == 0 && island.allocated == 0 && ownership[index] == OWNERSHIP_UNEXPLAINED
                {
                    ownership[index] = OWNERSHIP_NAMELESS_CHAIN;
                }
                current = volume.fat[current as usize];
                if !volume.valid_cluster(current) {
                    break;
                }
            }
            let head_class = classes[(island.head - FIRST_CLUSTER) as usize];
            island_rows.push((
                island,
                owners,
                head_class,
                (sum / island.length.max(1)) as i64,
            ));
        }
        for overlay in overlays
            .iter()
            .filter(|overlay| overlay.kind == OVERLAY_REGION_LABEL)
        {
            let (Some(start), Some(end)) = (overlay.target, payload_u32(&overlay.payload, "end"))
            else {
                continue;
            };
            let last = volume.boot.last_cluster_exclusive();
            for cluster in (start.max(i64::from(FIRST_CLUSTER)) as u32)..end.min(last) {
                let index = (cluster - FIRST_CLUSTER) as usize;
                if !volume.is_allocated(cluster) && ownership[index] == OWNERSHIP_UNEXPLAINED {
                    ownership[index] = OWNERSHIP_LABELLED;
                }
            }
        }
        let computed_millis = started.elapsed().as_millis();

        let transaction = self.connection.transaction()?;
        {
            transaction.execute_batch(
                "UPDATE sets SET o_name = NULL, o_first = NULL, o_length = NULL, o_no_fat = NULL, o_is_dir = NULL, o_ok = NULL \
                 WHERE o_ok IS NOT NULL;",
            )?;
            let mut correct = transaction.prepare_cached(
                "UPDATE sets SET o_ok = 1, o_name = ?2, o_first = ?3, o_length = ?4, o_no_fat = ?5, o_is_dir = ?6 WHERE id = ?1",
            )?;
            for (id, payload) in &corrections {
                correct.execute(params![
                    id,
                    payload.get("name").and_then(serde_json::Value::as_str),
                    payload_u32(payload, "firstCluster"),
                    payload
                        .get("length")
                        .and_then(serde_json::Value::as_u64)
                        .map(|value| value as i64),
                    payload
                        .get("noFatChain")
                        .and_then(serde_json::Value::as_bool),
                    payload.get("isDir").and_then(serde_json::Value::as_bool),
                ])?;
            }

            let mut update = transaction.prepare_cached(
                "UPDATE sets SET node = ?2, verdict = ?3, reason = ?4, clusters_needed = ?5, clusters_found = ?6, \
                 clusters_allocated = ?7, clusters_conflict = ?8, confidence = ?9 WHERE id = ?1",
            )?;
            for (id, node, assessment) in &results {
                update.execute(params![
                    id,
                    node,
                    assessment.verdict,
                    assessment.reason,
                    assessment.needed as i64,
                    assessment.found as i64,
                    assessment.allocated as i64,
                    assessment.conflict as i64,
                    assessment.confidence,
                ])?;
            }

            transaction.execute("DELETE FROM nodes", [])?;
            let mut insert = transaction.prepare_cached(
                "INSERT INTO nodes (head, parent, name, kind, modified, files, bytes, intact_files, intact_bytes, link_confidence) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            )?;
            for &node in &node_heads {
                let total = cumulative.get(&node).copied().unwrap_or_default();
                let set = names.get(&node);
                let name = name_overrides
                    .get(&node)
                    .cloned()
                    .or_else(|| set.map(|value| value.name.clone()));
                let link = if linked_by_overlay.contains(&node) {
                    LINK_OVERLAY
                } else {
                    match set {
                        Some(value) if !value.checksum_ok => TRUST_DAMAGED_CONFIRMED,
                        _ => TRUST_VALID,
                    }
                };
                insert.execute(params![
                    node,
                    parent.get(&node).copied().flatten(),
                    name,
                    kinds.get(&node).copied().unwrap_or("orphan"),
                    set.and_then(|value| value.modified.clone()),
                    total.files as i64,
                    total.bytes as i64,
                    total.intact_files as i64,
                    total.intact_bytes as i64,
                    link,
                ])?;
            }

            transaction.execute("DELETE FROM islands", [])?;
            let mut island_insert = transaction.prepare_cached(
                "INSERT INTO islands (head, tail, length, allocated, end_kind, owners, head_class, mean_entropy) \
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            )?;
            for (island, owners, head_class, mean_entropy) in &island_rows {
                island_insert.execute(params![
                    island.head,
                    island.tail,
                    island.length as i64,
                    island.allocated as i64,
                    island.end,
                    owners,
                    head_class,
                    mean_entropy,
                ])?;
            }
        }
        transaction.commit()?;
        let total_millis = started.elapsed().as_millis();
        self.set_state(
            "resolve_timing_millis",
            &format!("computed {computed_millis}, total {total_millis}"),
        )?;

        // Directory clusters are as trustworthy as the entries that found them.
        for &cluster in &found {
            if volume.valid_cluster(cluster)
                && classes[(cluster - FIRST_CLUSTER) as usize] == CLASS_DIRECTORY
            {
                confidence_map[(cluster - FIRST_CLUSTER) as usize] = CONFIDENCE_SCALE as u8;
            }
        }
        fs::write(workspace.confidence(), &confidence_map)?;
        fs::write(workspace.ownership(), &ownership)?;

        let mut summary = self.compute_summary()?;
        for cluster in FIRST_CLUSTER..volume.boot.last_cluster_exclusive() {
            if !volume.is_allocated(cluster) {
                match ownership[(cluster - FIRST_CLUSTER) as usize] {
                    OWNERSHIP_UNEXPLAINED => summary.unexplained_free_clusters += 1,
                    OWNERSHIP_LABELLED => summary.labelled_free_clusters += 1,
                    _ => {}
                }
            }
        }
        self.set_state("summary", &serde_json::to_string(&summary)?)?;
        Ok(summary)
    }

    fn load_sets(&self) -> Result<Vec<LoadedSet>> {
        let mut statement = self.connection.prepare(
            "SELECT id, source_cluster, name, in_use, checksum_ok, is_dir, first_cluster, length, no_fat_chain, modified \
             FROM sets",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(LoadedSet {
                id: row.get(0)?,
                source_cluster: row.get(1)?,
                name: row.get(2)?,
                in_use: row.get(3)?,
                checksum_ok: row.get(4)?,
                accepted: false,
                is_dir: row.get(5)?,
                first_cluster: row.get(6)?,
                length: row.get::<_, i64>(7)? as u64,
                no_fat_chain: row.get(8)?,
                modified: row.get(9)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    }
}
