//! Marks the rows and test results of an open filesystem scan that may be out of date.
//!
//! A scan whose source changed near some of its markers has stale regions (see [`crate::stale`]).
//! Every answer that holds an entry, a folder or a file test result is checked against them
//! before it leaves the engine, and one whose bytes overlap a stale region gets `"stale": true`.
//! An answer with nothing stale is left exactly as it was, so no key appears for it.
//!
//! What counts as an entry's bytes: the place its entry set was found, and the clusters its data
//! occupies when it is laid out one after another. A folder is its first cluster. A file test
//! result is its file's data, and it also stays flagged after a rescan, until the test is run
//! again, because a rescan reads the map's entries and cannot say the file's bytes passed.

use crate::catalog::StaleRegion;
use crate::stale::Span;
use crate::volume::Volume;
use serde_json::Value;

/// How long an entry set is, as far as marking where it sits goes.
const ENTRY_BYTES: u64 = 32;

pub struct Flagger<'a> {
    regions: &'a [StaleRegion],
    volume: &'a Volume,
    /// Where the scan's interpretation starts in its source, which is where the stale regions'
    /// offsets are measured from.
    scan_start: u64,
}

impl<'a> Flagger<'a> {
    pub fn new(regions: &'a [StaleRegion], volume: &'a Volume, scan_start: u64) -> Self {
        Self {
            regions,
            volume,
            scan_start,
        }
    }

    /// Whether any region is stale.
    pub fn any(&self) -> bool {
        self.regions.iter().any(StaleRegion::is_stale)
    }

    fn cluster_span(&self, cluster: u32, clusters: u64) -> Span {
        let start = self.scan_start + self.volume.cluster_offset(cluster);
        Span {
            start,
            end: start + clusters.max(1) * self.volume.geometry.cluster_size(),
        }
    }

    fn stale_now(&self, span: Span) -> bool {
        self.regions
            .iter()
            .any(|region| region.is_stale() && region.span().overlaps(span.start, span.len()))
    }

    /// Sets `stale` on every row and test result inside `value` that overlaps a stale region.
    pub fn flag(&self, value: &mut Value) {
        if !self.any() {
            return;
        }
        self.walk(value);
    }

    fn walk(&self, value: &mut Value) {
        match value {
            Value::Array(items) => items.iter_mut().for_each(|item| self.walk(item)),
            Value::Object(map) => {
                if self.is_stale(map) {
                    map.insert("stale".into(), Value::Bool(true));
                }
                for item in map.values_mut() {
                    self.walk(item);
                }
            }
            _ => {}
        }
    }

    fn is_stale(&self, object: &serde_json::Map<String, Value>) -> bool {
        let number = |key: &str| object.get(key).and_then(Value::as_u64);
        if let (Some(tested), Some(first)) = (
            number("testedAtMillis"),
            number("firstCluster").filter(|first| *first >= 2),
        ) {
            // A file test result: its file's bytes, and whether the test is older than the
            // marking of any region that holds them.
            let length = number("length").unwrap_or(0);
            let size = self.volume.geometry.cluster_size().max(1);
            let span = self.cluster_span(first as u32, length.div_ceil(size));
            return self.regions.iter().any(|region| {
                region.span().overlaps(span.start, span.len())
                    && (region.is_stale() || region.marked_at_millis > tested)
            });
        }
        let Some(kind) = object.get("kind").and_then(Value::as_str) else {
            // A row of the file list has no kind or entry location, only the file's data.
            if let (Some(_), Some(_), Some(first)) = (
                number("id"),
                object.get("verdict"),
                number("firstCluster").filter(|first| *first >= 2),
            ) {
                let size = self.volume.geometry.cluster_size().max(1);
                let length = number("length").unwrap_or(0);
                return self.stale_now(self.cluster_span(first as u32, length.div_ceil(size)));
            }
            return false;
        };
        match kind {
            "file" | "missing-folder" | "folder" => {}
            _ => return false,
        }
        let mut spans: Vec<Span> = Vec::new();
        if let Some(cluster) = number("sourceCluster") {
            let start = self.scan_start
                + self.volume.cluster_offset(cluster as u32)
                + number("sourceOffset").unwrap_or(0);
            spans.push(Span {
                start,
                end: start + ENTRY_BYTES,
            });
        }
        if kind == "folder" {
            if let Some(head) = number("head").filter(|head| *head >= 2) {
                spans.push(self.cluster_span(head as u32, 1));
            }
        } else if let Some(first) = number("firstCluster").filter(|first| *first >= 2) {
            spans.push(self.cluster_span(first as u32, number("clustersNeeded").unwrap_or(1)));
        }
        spans.iter().any(|span| self.stale_now(*span))
    }
}
