//! Tools for explaining the parts of the disk nothing accounts for yet.
//!
//! Free clusters that no entry or FAT chain claims are the map's blank areas.
//! This module finds them and gathers evidence about what they might be. It
//! looks inside each cluster at 4 KiB boundaries, because NTFS and the older
//! file generations on this disk used smaller units than exFAT's 128 KiB, so a
//! file's start can sit in the middle of a cluster.

use crate::content::{entropy_bits, signature_class, CLASS_NAMES, GROUP_COUNT, GROUP_NAMES};
use crate::content::content_group;
use crate::exfat::{find_sets, FIRST_CLUSTER};
use crate::resolve::OWNERSHIP_UNEXPLAINED;
use crate::session::Session;
use anyhow::Result;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;

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
    Some(usize::from(u16::from_le_bytes(data.get(offset..offset + 2)?.try_into().ok()?)))
}

fn u32_at(data: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(data.get(offset..offset + 4)?.try_into().ok()?))
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
            let units: Vec<u16> = bytes.chunks_exact(2).map(|pair| u16::from_le_bytes([pair[0], pair[1]])).collect();
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
    pub fn unexplained_runs(&self, offset: usize, limit: usize, min_clusters: u32) -> Vec<UnexplainedRun> {
        let cluster_size = self.volume.boot.cluster_size();
        let last = self.volume.boot.last_cluster_exclusive();
        let ownership = self.ownership_map();
        let classes = self.class_map();
        let entropy = self.entropy_map();
        let unexplained = |cluster: u32| {
            !self.volume.is_allocated(cluster) && ownership[(cluster - FIRST_CLUSTER) as usize] == OWNERSHIP_UNEXPLAINED
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
                let (top, top_count) =
                    groups.iter().enumerate().max_by_key(|(_, &value)| value).map_or((0, 0), |(index, &value)| (index, value));
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
        let last = end.min(self.volume.boot.last_cluster_exclusive());
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
            for set in find_sets(&data, data.len(), self.volume.limits()) {
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

        Ok(json!({
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
        }))
    }
}
