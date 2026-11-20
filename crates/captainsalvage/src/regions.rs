//! Tools for explaining the parts of the disk nothing accounts for yet.
//!
//! Free clusters that no entry or FAT chain claims are the map's blank areas.
//! This module finds them and gathers evidence about what they might be. It
//! looks inside each cluster at 4 KiB boundaries, because NTFS and the older
//! file generations on this disk used smaller units than exFAT's 128 KiB, so a
//! file's start can sit in the middle of a cluster.

use crate::content::content_group;
use crate::content::{entropy_bits, signature_class, CLASS_NAMES, GROUP_COUNT, GROUP_NAMES};
use crate::resolve::OWNERSHIP_UNEXPLAINED;
use crate::session::Session;
use anyhow::Result;
use fs_exfat::primitives::FIRST_CLUSTER;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

/// Unexplained runs separated by no more than this many clusters are one region.
const MERGE_GAP_CLUSTERS: u32 = 16;
const BLOCK_BYTES: usize = 4096;
const NTFS_RECORD_BYTES: usize = 1024;
const MAX_SAMPLES: usize = 256;
const MAX_EXCERPTS: usize = 40;
const MAX_NAMES: usize = 60;
const MIN_STRING_BYTES: usize = 24;
const MAX_STRING_BYTES: usize = 120;
const HIGH_ENTROPY_BITS: f64 = 7.5;
const LOW_ENTROPY_BITS: f64 = 5.0;
/// `unexplained_summary`: how many clusters it samples in all unless asked for another number, the most
/// it will, and how long it may take.
pub const DEFAULT_SUMMARY_SAMPLES: usize = 2048;
pub const MAX_SUMMARY_SAMPLES: usize = 20_000;
pub const DEFAULT_SUMMARY_MILLIS: u64 = 20_000;
pub const MAX_SUMMARY_MILLIS: u64 = 120_000;
const SUMMARY_EXAMPLES: usize = 20;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UnexplainedRun {
    pub start_cluster: u32,
    pub end_cluster: u32,
    pub clusters: u64,
    pub bytes: u64,
    pub unexplained_clusters: u64,
    pub main_content: String,
    pub main_content_share: f64,
    pub mean_entropy_bits: f64,
}

fn u16_at(data: &[u8], offset: usize) -> Option<usize> {
    Some(usize::from(u16::from_le_bytes(
        data.get(offset..offset + 2)?.try_into().ok()?,
    )))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        data.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

/// Reads the file name from an NTFS file record, preferring a long name over its
/// DOS 8.3 form. Follows Microsoft's published attribute layout. Update-sequence
/// fixups are not applied, so a name crossing a sector end could be off by a character.
pub fn ntfs_record_name(record: &[u8]) -> Option<String> {
    const ATTRIBUTE_FILE_NAME: u32 = 0x30;
    const ATTRIBUTE_END: u32 = 0xFFFF_FFFF;
    const NAMESPACE_DOS: u8 = 2;
    if !record.starts_with(b"FILE") {
        return None;
    }
    let mut position = u16_at(record, 0x14)?;
    let mut found = None;
    while position + 8 <= record.len() {
        let kind = u32_at(record, position)?;
        if kind == ATTRIBUTE_END {
            break;
        }
        let length = u32_at(record, position + 4)? as usize;
        if length == 0 || position + length > record.len() {
            break;
        }
        if kind == ATTRIBUTE_FILE_NAME && record[position + 8] == 0 {
            let content = position + u16_at(record, position + 0x14)?;
            let name_length = usize::from(*record.get(content + 0x40)?);
            let namespace = *record.get(content + 0x41)?;
            let bytes = record.get(content + 0x42..content + 0x42 + name_length * 2)?;
            let units: Vec<u16> = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            let name = String::from_utf16_lossy(&units);
            if namespace != NAMESPACE_DOS {
                return Some(name);
            }
            found = Some(name);
        }
        position += length;
    }
    found
}

fn readable_strings(data: &[u8], out: &mut Vec<String>) {
    let mut start = None;
    for (index, &byte) in data.iter().enumerate() {
        let readable = (32..127).contains(&byte) || byte == b'\t';
        match (readable, start) {
            (true, None) => start = Some(index),
            (false, Some(from)) => {
                if index - from >= MIN_STRING_BYTES && out.len() < MAX_EXCERPTS {
                    let end = index.min(from + MAX_STRING_BYTES);
                    out.push(String::from_utf8_lossy(&data[from..end]).into_owned());
                }
                start = None;
            }
            _ => {}
        }
    }
}

impl Session {
    /// Regions of free space nothing explains, largest first.
    pub fn unexplained_runs(
        &self,
        offset: usize,
        limit: usize,
        min_clusters: u32,
    ) -> Vec<UnexplainedRun> {
        let cluster_size = self.volume.geometry.cluster_size();
        let last = self.volume.geometry.last_cluster_exclusive();
        let ownership = self.ownership_map();
        let classes = self.class_map();
        let entropy = self.entropy_map();
        let unexplained = |cluster: u32| {
            self.volume.is_free(cluster)
                && ownership[(cluster - FIRST_CLUSTER) as usize] == OWNERSHIP_UNEXPLAINED
        };

        let mut spans: Vec<(u32, u32)> = Vec::new();
        let mut cluster = FIRST_CLUSTER;
        while cluster < last {
            if !unexplained(cluster) {
                cluster += 1;
                continue;
            }
            let start = cluster;
            while cluster < last && unexplained(cluster) {
                cluster += 1;
            }
            match spans.last_mut() {
                Some(previous) if start - previous.1 <= MERGE_GAP_CLUSTERS => previous.1 = cluster,
                _ => spans.push((start, cluster)),
            }
        }

        let mut runs: Vec<UnexplainedRun> = spans
            .into_iter()
            .filter(|(start, end)| end - start >= min_clusters)
            .map(|(start, end)| {
                let mut groups = [0u64; GROUP_COUNT];
                let mut entropy_sum = 0u64;
                let mut count = 0u64;
                for inner in start..end {
                    if unexplained(inner) {
                        let index = (inner - FIRST_CLUSTER) as usize;
                        groups[usize::from(content_group(classes[index]))] += 1;
                        entropy_sum += u64::from(entropy[index]);
                        count += 1;
                    }
                }
                let (top, top_count) = groups
                    .iter()
                    .enumerate()
                    .max_by_key(|(_, &value)| value)
                    .map_or((0, 0), |(index, &value)| (index, value));
                UnexplainedRun {
                    start_cluster: start,
                    end_cluster: end,
                    clusters: u64::from(end - start),
                    bytes: u64::from(end - start) * cluster_size,
                    unexplained_clusters: count,
                    main_content: GROUP_NAMES[top].to_string(),
                    main_content_share: top_count as f64 / count.max(1) as f64,
                    mean_entropy_bits: entropy_sum as f64 / count.max(1) as f64 * 8.0 / 255.0,
                }
            })
            .collect();
        runs.sort_by_key(|run| std::cmp::Reverse(run.clusters));
        runs.into_iter().skip(offset).take(limit).collect()
    }

    /// Samples clusters across a region and reports what they look like inside:
    /// file signatures at 4 KiB boundaries, NTFS records with their names, exFAT
    /// entries, readable text, and how many clusters mix different kinds of data.
    pub fn explain_region(&self, start: u32, end: u32, samples: usize) -> Result<Value> {
        let first = start.max(FIRST_CLUSTER);
        let last = end.min(self.volume.geometry.last_cluster_exclusive());
        let samples = samples.clamp(1, MAX_SAMPLES);
        let span = last.saturating_sub(first);
        let step = (span as usize / samples).max(1);
        let picks: Vec<u32> = (first..last).step_by(step).take(samples).collect();

        let mut cluster_start_signatures: HashMap<&'static str, u64> = HashMap::new();
        let mut inner_signatures: HashMap<&'static str, u64> = HashMap::new();
        let mut ntfs_file_records = 0u64;
        let mut ntfs_index_blocks = 0u64;
        let mut ntfs_names: Vec<String> = Vec::new();
        let mut exfat_valid = 0u64;
        let mut exfat_damaged = 0u64;
        let mut exfat_names: Vec<String> = Vec::new();
        let mut excerpts: Vec<String> = Vec::new();
        let mut all_zero = 0u64;
        let mut mixed = 0u64;
        let mut high_entropy = 0u64;
        let mut low_entropy = 0u64;
        let mut read_errors = 0u64;
        let mut examples: Vec<Value> = Vec::new();

        for &cluster in &picks {
            let Ok(data) = self.volume.read_cluster(cluster) else {
                read_errors += 1;
                continue;
            };
            if data.iter().all(|&byte| byte == 0) {
                all_zero += 1;
                continue;
            }
            let bits = entropy_bits(&data);
            if bits >= HIGH_ENTROPY_BITS {
                high_entropy += 1;
            } else if bits < LOW_ENTROPY_BITS {
                low_entropy += 1;
            }

            let mut has_random = false;
            let mut has_plain = false;
            let mut block_signatures = Vec::new();
            for (index, block) in data.chunks(BLOCK_BYTES).enumerate() {
                if let Some(class) = signature_class(block) {
                    let name = CLASS_NAMES[usize::from(class)];
                    if index == 0 {
                        *cluster_start_signatures.entry(name).or_insert(0) += 1;
                    } else {
                        *inner_signatures.entry(name).or_insert(0) += 1;
                    }
                    block_signatures.push(json!({"offset": index * BLOCK_BYTES, "kind": name}));
                }
                let block_bits = entropy_bits(block);
                has_random |= block_bits >= HIGH_ENTROPY_BITS;
                has_plain |= block_bits < LOW_ENTROPY_BITS;
                if block.starts_with(b"INDX") {
                    ntfs_index_blocks += 1;
                }
            }
            if has_random && has_plain {
                mixed += 1;
            }
            for record in data.chunks_exact(NTFS_RECORD_BYTES) {
                if record.starts_with(b"FILE") {
                    ntfs_file_records += 1;
                    if let Some(name) = ntfs_record_name(record) {
                        if ntfs_names.len() < MAX_NAMES {
                            ntfs_names.push(name);
                        }
                    }
                }
            }
            for set in self.volume.find_sets(&data, data.len()) {
                if set.checksum_ok {
                    exfat_valid += 1;
                } else {
                    exfat_damaged += 1;
                }
                if exfat_names.len() < MAX_NAMES {
                    exfat_names.push(set.name);
                }
            }
            readable_strings(&data, &mut excerpts);
            if !block_signatures.is_empty() && examples.len() < 20 {
                examples.push(json!({"cluster": cluster, "signatures": block_signatures}));
            }
        }

        let mut explanation = json!({
            "startCluster": first,
            "endCluster": last,
            "sampled": picks.len(),
            "readErrors": read_errors,
            "allZero": all_zero,
            "highEntropy": high_entropy,
            "lowEntropy": low_entropy,
            "mixedInsideOneCluster": mixed,
            "signaturesAtClusterStart": cluster_start_signatures,
            "signaturesInsideClusters": inner_signatures,
            "ntfsFileRecords": ntfs_file_records,
            "ntfsIndexBlocks": ntfs_index_blocks,
            "ntfsNames": ntfs_names,
            "exfatEntriesValid": exfat_valid,
            "exfatEntriesDamaged": exfat_damaged,
            "exfatNames": exfat_names,
            "textExcerpts": excerpts,
            "signatureExamples": examples,
        });
        if !self.volume.kind.keeps_legacy_report_shape() {
            let fields = explanation
                .as_object_mut()
                .expect("region explanation is an object");
            for (old, new) in [
                ("exfatEntriesValid", "directoryEntriesRecognized"),
                ("exfatEntriesDamaged", "directoryEntriesDamaged"),
                ("exfatNames", "directoryNames"),
            ] {
                if let Some(value) = fields.remove(old) {
                    fields.insert(new.into(), value);
                }
            }
            fields.insert(
                "filesystem".into(),
                serde_json::to_value(&self.volume.filesystem)?,
            );
        }
        Ok(explanation)
    }

    /// One call over the whole unexplained space: how much there is and of what, and what a sample of
    /// its clusters holds. The sample is spread over the biggest `top` regions (all of them when none is
    /// given) in proportion to their size, so every region has some, within `samples` clusters in all and
    /// `max_millis` of time. Names and text are left out unless `examples` asks for them.
    pub fn unexplained_summary(
        &self,
        top: Option<usize>,
        min_clusters: u32,
        samples: usize,
        max_millis: u64,
        per_run: bool,
        examples: bool,
    ) -> Result<Value> {
        let started = Instant::now();
        let budget = Duration::from_millis(max_millis.clamp(1, MAX_SUMMARY_MILLIS));
        let samples = samples.clamp(1, MAX_SUMMARY_SAMPLES);
        let cluster_size = self.volume.geometry.cluster_size();
        let all = self.unexplained_runs(0, usize::MAX, min_clusters);
        let total_unexplained: u64 = all.iter().map(|run| run.unexplained_clusters).sum();
        let chosen: Vec<&UnexplainedRun> = all.iter().take(top.unwrap_or(usize::MAX)).collect();
        let chosen_clusters: u64 = chosen.iter().map(|run| run.unexplained_clusters).sum();
        let mut by_content: BTreeMap<&str, u64> = BTreeMap::new();
        for run in &chosen {
            *by_content.entry(run.main_content.as_str()).or_insert(0) +=
                run.unexplained_clusters * cluster_size;
        }

        let ownership = self.ownership_map();
        let unexplained = |cluster: u32| {
            self.volume.is_free(cluster)
                && ownership[(cluster - FIRST_CLUSTER) as usize] == OWNERSHIP_UNEXPLAINED
        };
        // What the scan made of each cluster, and how far it got: a cluster that holds valid directory
        // entries now and is still unexplained was either not searched (past where the scan stopped) or
        // could not be read when it was.
        let classes = self.class_map();
        let progress = self
            .with_database(|database| crate::scan::read_progress(database, &self.volume))
            .ok();
        let scanned_to = progress.as_ref().map_or(u32::MAX, |scan| scan.next_cluster);
        let mut tally = Tally::default();
        let mut runs_out: Vec<Value> = Vec::new();
        let mut stopped: Option<&str> = None;
        let mut sampled_total = 0usize;
        'runs: for run in &chosen {
            // This run's share of the sample, at least one cluster, and no more than is left.
            let share = (samples as u128 * u128::from(run.unexplained_clusters)
                / u128::from(chosen_clusters.max(1))) as usize;
            let wanted = share.max(1).min(samples - sampled_total);
            if wanted == 0 {
                stopped = Some("sample budget");
                break;
            }
            let span = run.end_cluster - run.start_cluster;
            let step = (span as usize / wanted).max(1);
            let mut mine = Tally::default();
            for cluster in (run.start_cluster..run.end_cluster)
                .step_by(step)
                .take(wanted)
            {
                if !unexplained(cluster) {
                    continue;
                }
                if started.elapsed() >= budget {
                    stopped = Some("time");
                    break 'runs;
                }
                if self.tally_cluster(cluster, &mut mine, examples) {
                    let class = classes[(cluster - FIRST_CLUSTER) as usize];
                    *mine
                        .valid_directory_by_scan_class
                        .entry(CLASS_NAMES[usize::from(class)])
                        .or_insert(0) += 1;
                    if cluster >= scanned_to {
                        mine.valid_directory_not_scanned += 1;
                    }
                }
            }
            sampled_total += mine.sampled as usize;
            if per_run {
                runs_out.push(json!({
                    "startCluster": run.start_cluster,
                    "endCluster": run.end_cluster,
                    "unexplainedClusters": run.unexplained_clusters,
                    "mainContent": run.main_content,
                    "sampled": mine.sampled,
                    "holdingValidDirectoryEntries": mine.valid_directory,
                    "mixedInsideOneCluster": mine.mixed,
                }));
            }
            tally.merge(mine);
        }

        let sampled = tally.sampled.max(1) as f64;
        let share = |count: u64| count as f64 / sampled;
        let estimate = |count: u64| (share(count) * chosen_clusters as f64).round() as u64;
        let mut answer = json!({
            "regions": {
                "total": all.len(),
                "considered": chosen.len(),
                "unexplainedClusters": total_unexplained,
                "unexplainedBytes": total_unexplained * cluster_size,
                "consideredClusters": chosen_clusters,
                "consideredBytes": chosen_clusters * cluster_size,
            },
            "scan": progress.as_ref().map(|scan| json!({
                "status": scan.status,
                "nextCluster": scan.next_cluster,
                "totalClusters": scan.total_clusters,
                "readErrors": scan.read_errors,
            })),
            "bytesByMainContent": by_content,
            "sample": {
                "clusters": tally.sampled,
                "budget": samples,
                "millis": started.elapsed().as_millis() as u64,
                "stoppedBecause": stopped,
                "readErrors": tally.read_errors,
            },
            "sampled": {
                "allZero": tally.all_zero,
                "highEntropy": tally.high_entropy,
                "lowEntropy": tally.low_entropy,
                "mixedInsideOneCluster": tally.mixed,
                "holdingValidDirectoryEntries": tally.valid_directory,
                "holdingOnlyDamagedDirectoryEntries": tally.damaged_directory_only,
                "validDirectoryEntrySets": tally.valid_sets,
                "damagedDirectoryEntrySets": tally.damaged_sets,
                "startingAFileAndMixed": tally.start_and_mixed,
                "holdingValidDirectoryEntriesByClassAtScan": tally.valid_directory_by_scan_class,
                "holdingValidDirectoryEntriesNotYetScanned": tally.valid_directory_not_scanned,
                "ntfsFileRecords": tally.ntfs_records,
                "ntfsIndexBlocks": tally.ntfs_index_blocks,
                "signaturesAtClusterStart": tally.start_signatures,
                "signaturesInsideClusters": tally.inner_signatures,
            },
            "shareOfSampled": {
                "holdingValidDirectoryEntries": share(tally.valid_directory),
                "mixedInsideOneCluster": share(tally.mixed),
                "startingAFileAndMixed": share(tally.start_and_mixed),
                "allZero": share(tally.all_zero),
            },
            "estimatedInConsideredRegions": {
                "clustersHoldingValidDirectoryEntries": estimate(tally.valid_directory),
                "clustersMixingOldAndNewData": estimate(tally.mixed),
            },
            "note": "Estimates scale the sampled share to the considered regions; the sample is evenly spaced, not random.",
        });
        if per_run {
            answer["perRun"] = json!(runs_out);
        }
        if examples {
            answer["examples"] = json!({
                "directoryNames": tally.names,
                "ntfsNames": tally.ntfs_names,
                "textExcerpts": tally.excerpts,
            });
        }
        Ok(answer)
    }

    /// What one sampled cluster holds, added to `tally`.
    /// Whether it holds a valid directory entry set.
    fn tally_cluster(&self, cluster: u32, tally: &mut Tally, examples: bool) -> bool {
        let Ok(data) = self.volume.read_cluster(cluster) else {
            tally.read_errors += 1;
            return false;
        };
        tally.sampled += 1;
        if data.iter().all(|&byte| byte == 0) {
            tally.all_zero += 1;
            return false;
        }
        let bits = entropy_bits(&data);
        if bits >= HIGH_ENTROPY_BITS {
            tally.high_entropy += 1;
        } else if bits < LOW_ENTROPY_BITS {
            tally.low_entropy += 1;
        }
        let (mut has_random, mut has_plain, mut starts) = (false, false, false);
        for (index, block) in data.chunks(BLOCK_BYTES).enumerate() {
            if let Some(class) = signature_class(block) {
                let name = CLASS_NAMES[usize::from(class)];
                if index == 0 {
                    starts = true;
                    *tally.start_signatures.entry(name).or_insert(0) += 1;
                } else {
                    *tally.inner_signatures.entry(name).or_insert(0) += 1;
                }
            }
            let block_bits = entropy_bits(block);
            has_random |= block_bits >= HIGH_ENTROPY_BITS;
            has_plain |= block_bits < LOW_ENTROPY_BITS;
            if block.starts_with(b"INDX") {
                tally.ntfs_index_blocks += 1;
            }
        }
        if has_random && has_plain {
            tally.mixed += 1;
            if starts {
                tally.start_and_mixed += 1;
            }
        }
        for record in data.chunks_exact(NTFS_RECORD_BYTES) {
            if record.starts_with(b"FILE") {
                tally.ntfs_records += 1;
                if examples {
                    if let Some(name) = ntfs_record_name(record) {
                        if tally.ntfs_names.len() < SUMMARY_EXAMPLES {
                            tally.ntfs_names.push(name);
                        }
                    }
                }
            }
        }
        let (mut valid, mut damaged) = (0u64, 0u64);
        for set in self.volume.find_sets(&data, data.len()) {
            if set.checksum_ok {
                valid += 1;
            } else {
                damaged += 1;
            }
            if examples && tally.names.len() < SUMMARY_EXAMPLES {
                tally.names.push(set.name);
            }
        }
        tally.valid_sets += valid;
        tally.damaged_sets += damaged;
        if valid > 0 {
            tally.valid_directory += 1;
        } else if damaged > 0 {
            tally.damaged_directory_only += 1;
        }
        if examples {
            readable_strings(&data, &mut tally.excerpts);
        }
        valid > 0
    }
}

/// What a sample of clusters held, counted.
#[derive(Default)]
struct Tally {
    sampled: u64,
    read_errors: u64,
    all_zero: u64,
    high_entropy: u64,
    low_entropy: u64,
    mixed: u64,
    start_and_mixed: u64,
    valid_directory: u64,
    damaged_directory_only: u64,
    valid_sets: u64,
    damaged_sets: u64,
    valid_directory_not_scanned: u64,
    valid_directory_by_scan_class: BTreeMap<&'static str, u64>,
    ntfs_records: u64,
    ntfs_index_blocks: u64,
    start_signatures: BTreeMap<&'static str, u64>,
    inner_signatures: BTreeMap<&'static str, u64>,
    names: Vec<String>,
    ntfs_names: Vec<String>,
    excerpts: Vec<String>,
}

impl Tally {
    fn merge(&mut self, other: Tally) {
        self.sampled += other.sampled;
        self.read_errors += other.read_errors;
        self.all_zero += other.all_zero;
        self.high_entropy += other.high_entropy;
        self.low_entropy += other.low_entropy;
        self.mixed += other.mixed;
        self.start_and_mixed += other.start_and_mixed;
        self.valid_directory += other.valid_directory;
        self.damaged_directory_only += other.damaged_directory_only;
        self.valid_sets += other.valid_sets;
        self.damaged_sets += other.damaged_sets;
        self.valid_directory_not_scanned += other.valid_directory_not_scanned;
        for (name, count) in other.valid_directory_by_scan_class {
            *self.valid_directory_by_scan_class.entry(name).or_insert(0) += count;
        }
        self.ntfs_records += other.ntfs_records;
        self.ntfs_index_blocks += other.ntfs_index_blocks;
        for (name, count) in other.start_signatures {
            *self.start_signatures.entry(name).or_insert(0) += count;
        }
        for (name, count) in other.inner_signatures {
            *self.inner_signatures.entry(name).or_insert(0) += count;
        }
        self.names.extend(other.names);
        self.ntfs_names.extend(other.ntfs_names);
        self.excerpts.extend(other.excerpts);
        self.names.truncate(SUMMARY_EXAMPLES);
        self.ntfs_names.truncate(SUMMARY_EXAMPLES);
        self.excerpts.truncate(MAX_EXCERPTS);
    }
}
