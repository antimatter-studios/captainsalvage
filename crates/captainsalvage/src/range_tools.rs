//! The range tools, shared by MCP and the command line so both do the same thing.
//!
//! Every tool here only reads the source, and keeps its own records on this Mac.

use crate::disk::RawDisk;
use crate::hypotheses::{Hypothesis, HypothesisStore, Layout, LayoutSource, NewHypothesis, Patch};
use crate::range_analysis::{analyze, Options};
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

fn need_u64(args: &Value, key: &str) -> Result<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("argument {key} is required and must be a non-negative integer"))
}

/// The tools this module answers.
pub const TOOLS: &[&str] = &[
    "analyze_range",
    "list_hypotheses",
    "save_hypothesis",
    "update_hypothesis",
    "delete_hypothesis",
    "raise_hypothesis",
    "read_layout",
];

/// The largest read of the virtual device, as with raw reads.
const MAX_LAYOUT_READ: u64 = 64 * 1024;

/// One tool as MCP lists it, before the name gets its prefix.
pub struct Definition {
    pub name: &'static str,
    pub description: &'static str,
    pub properties: Value,
    pub required: &'static [&'static str],
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

pub fn definitions() -> Vec<Definition> {
    vec![Definition {
        name: "analyze_range",
        description: "Analyzes a selected byte range of an open raw source without trusting any partition table. Every filesystem driver (exFAT, FAT, NTFS, and BitLocker detection) is asked at every start offset the slide tries how much of the range decodes from there. Returns the best offsets per driver and findings with format, offset, how much decoded and a confidence. Signature probes make the slide fast but are not needed: directory entries and MFT records decode by shape even when boot sectors and record magic are gone. Data identified but not placed in a volume is an island with an offer: search the offered range for the missing metadata by calling this tool again on it with formats set. NTFS geometry is inferred from the $MFT record where it survives. A finding is evidence, not proof. Never writes the source. Ranges over 2 GiB are refused.",
        properties: json!({
            "diskId": {"type": "string", "description": "ID from open_raw_disk."},
            "offset": integer("Absolute start of the range."),
            "length": integer("Length of the range in bytes, at most 2 GiB."),
            "step": integer("How far the start offset moves each time. 0 (default) means 1 for ranges up to 128 MiB and 32 beyond. Counted from byte zero of the source."),
            "signatures": {"type": "boolean", "description": "Use signature probes. Default true. False makes the slide rely on structure alone."},
            "perFormat": integer("Findings kept per driver and kind. Default 5."),
            "formats": {"type": "array", "items": {"type": "string"}, "description": "Only these drivers: exFAT, FAT, NTFS, BitLocker. Default all."},
            "geometry": {"type": "boolean", "description": "Infer NTFS geometry from the MFT. Default true."}
        }),
        required: &["diskId", "offset", "length"],
    },
    Definition {
        name: "list_hypotheses",
        description: "Lists the recovery hypotheses saved for the open raw source, lowest layer first, and the layout they make. A hypothesis is an extent plus a format, with a confidence; it is a claim and costs nothing. Enabled hypotheses that overlap compete: `layout.competing` lists each overlapped stretch with its stack of hypothesis IDs, top layer first, and the first is the one read. `layout.segments` and `layout.partitionTable` are the overlay of source segments at virtual positions, like a Disk Rebuild whose input is partitions; `layout.gaps` are virtual bytes nothing covers. Records are kept on this Mac; nothing is ever written to the source.",
        properties: json!({"diskId": {"type": "string", "description": "ID from open_raw_disk."}}),
        required: &["diskId"],
    },
    Definition {
        name: "save_hypothesis",
        description: "Saves an extent and a format as a hypothesis, for example from an analyze_range finding. Nothing is written to the source. Overlap with other hypotheses is allowed: they stay separate layers. The new one is on top.",
        properties: json!({
            "diskId": {"type": "string", "description": "ID from open_raw_disk."},
            "format": {"type": "string", "description": "exFAT, FAT12, FAT16, FAT32 or NTFS."},
            "start": integer("Absolute start of the extent in the source."),
            "length": integer("Length of the extent in bytes."),
            "label": {"type": "string", "description": "A name for it."},
            "confidence": {"type": "number", "description": "0 to 1. From a finding's confidence."},
            "bootOffset": integer("Absolute byte of boot evidence when it is not at the start of the extent."),
            "virtualStart": integer("Where the segment sits in the layout's virtual device. Defaults to start."),
            "origin": {"type": "string", "description": "analysis or manual. Defaults to manual."},
            "enabled": {"type": "boolean", "description": "Default true."}
        }),
        required: &["diskId", "format", "start", "length"],
    },
    Definition {
        name: "update_hypothesis",
        description: "Changes a hypothesis: its start and length (type or drag the ends), format, label, confidence, boot evidence, virtual position, layer, or whether it is enabled. Fields left out stay as they are; bootOffset null clears it. Disabling a layer lets the ones below it show; it deletes nothing.",
        properties: json!({
            "diskId": {"type": "string", "description": "ID from open_raw_disk."},
            "id": {"type": "string", "description": "Hypothesis ID from list_hypotheses."},
            "start": integer("New start of the extent."),
            "length": integer("New length of the extent."),
            "format": {"type": "string"},
            "label": {"type": "string"},
            "confidence": {"type": "number"},
            "bootOffset": {"type": ["integer", "null"]},
            "virtualStart": integer("Where the segment sits in the virtual device."),
            "enabled": {"type": "boolean"},
            "layer": integer("Higher layers are on top.")
        }),
        required: &["diskId", "id"],
    },
    Definition {
        name: "delete_hypothesis",
        description: "Forgets a saved hypothesis. The source is untouched; this only removes the app's record.",
        properties: json!({"diskId": {"type": "string", "description": "ID from open_raw_disk."}, "id": {"type": "string", "description": "Hypothesis ID."}}),
        required: &["diskId", "id"],
    },
    Definition {
        name: "raise_hypothesis",
        description: "Puts a hypothesis on top of the other layers, so it wins where enabled extents overlap.",
        properties: json!({"diskId": {"type": "string", "description": "ID from open_raw_disk."}, "id": {"type": "string", "description": "Hypothesis ID."}}),
        required: &["diskId", "id"],
    },
    Definition {
        name: "open_hypothesis",
        description: "Tries a saved hypothesis as a filesystem interpretation of its extent, like open_disk_region, with its format, label and boot evidence. The source is read-only and the hypothesis stays saved whether or not it opens.",
        properties: json!({"diskId": {"type": "string", "description": "ID from open_raw_disk."}, "id": {"type": "string", "description": "Hypothesis ID from list_hypotheses."}}),
        required: &["diskId", "id"],
    },
    Definition {
        name: "read_layout",
        description: "Reads bytes of the virtual device the enabled hypotheses make, as hexadecimal: the overlay of source segments, upper layer first where they overlap. Bytes no segment covers are refused, not returned as zeros. At most 64 KiB. Read only.",
        properties: json!({"diskId": {"type": "string", "description": "ID from open_raw_disk."}, "offset": integer("Offset in the virtual device."), "length": integer("At most 64 KiB.")}),
        required: &["diskId", "offset", "length"],
    }]
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("argument {key} is required and must be a string"))
}

fn listing(hypotheses: Vec<Hypothesis>) -> Result<Value> {
    let layout = Layout::of(&hypotheses);
    Ok(json!({"hypotheses": hypotheses, "layout": layout}))
}

pub fn call(tool: &str, disk: &RawDisk, args: &Value) -> Result<Value> {
    match tool {
        "analyze_range" => {
            let options: Options = serde_json::from_value(args.clone())
                .context("the analysis options are not valid")?;
            let report = analyze(
                disk,
                need_u64(args, "offset")?,
                need_u64(args, "length")?,
                &options,
                &mut |_, _| true,
            )?;
            Ok(serde_json::to_value(report)?)
        }
        "list_hypotheses" => listing(HypothesisStore::for_user()?.list(disk)?),
        "save_hypothesis" => {
            let new: NewHypothesis =
                serde_json::from_value(args.clone()).context("the hypothesis is not valid")?;
            let mut store = HypothesisStore::for_user()?;
            let saved = store.create(disk, new)?;
            Ok(json!({"saved": saved, "layout": Layout::of(&store.list(disk)?)}))
        }
        "update_hypothesis" => {
            let patch: Patch =
                serde_json::from_value(args.clone()).context("the changes are not valid")?;
            let mut store = HypothesisStore::for_user()?;
            let changed = store.update(disk, need_str(args, "id")?, patch)?;
            Ok(json!({"changed": changed, "layout": Layout::of(&store.list(disk)?)}))
        }
        "raise_hypothesis" => {
            let mut store = HypothesisStore::for_user()?;
            let changed = store.raise(disk, need_str(args, "id")?)?;
            Ok(json!({"changed": changed, "layout": Layout::of(&store.list(disk)?)}))
        }
        "delete_hypothesis" => {
            let mut store = HypothesisStore::for_user()?;
            store.delete(disk, need_str(args, "id")?)?;
            listing(store.list(disk)?)
        }
        "read_layout" => {
            let length = need_u64(args, "length")?;
            anyhow::ensure!(length <= MAX_LAYOUT_READ, "a layout read is at most 64 KiB");
            let hypotheses = HypothesisStore::for_user()?.list(disk)?;
            let device = LayoutSource::new(Arc::clone(&disk.source), &Layout::of(&hypotheses));
            let offset = need_u64(args, "offset")?;
            let mut data = vec![0; length as usize];
            crate::source::BlockSource::read_exact_at(&device, offset, &mut data)?;
            Ok(
                json!({"offset": offset, "hex": data.iter().map(|byte| format!("{byte:02x}")).collect::<String>()}),
            )
        }
        other => anyhow::bail!("unknown range tool {other}"),
    }
}
