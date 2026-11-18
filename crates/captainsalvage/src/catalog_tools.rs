//! The catalog's tools, shared by MCP, the window and the command line so all three do the same.
//!
//! Listing is read-only. A tool that makes a scan or changes a name is the person's to run, as
//! changing a Disk Rebuild is, and an agent that asks is told so. Nothing here reads file
//! contents or writes to a source.

use crate::catalog::{
    migrate, Accept, Catalog, InterpretationRequest, ScanKind, ScanRecord, ScanRequest,
    SourceRecord, FILESYSTEM_SCANNER_VERSION,
};
use crate::disk::RawDisk;
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};

/// The tools this module answers. `open_stored_scan` is the engine's, because it opens a session.
pub const TOOLS: &[&str] = &[
    "list_sources",
    "identify_source",
    "list_scans",
    "switch_interpretation",
    "label_scan",
    "label_source",
    "migrate_catalog",
    "list_stale_regions",
];

/// The tools that change what is stored, which only the person using the app runs.
const PERSON_ONLY: &[&str] = &[
    "rescan_changed_regions",
    "switch_interpretation",
    "label_scan",
    "label_source",
    "migrate_catalog",
];

/// One tool as MCP lists it, before the name gets its prefix.
pub struct Definition {
    pub name: &'static str,
    pub description: &'static str,
    pub properties: Value,
    pub required: &'static [&'static str],
}

fn disk_id() -> Value {
    json!({"type": "string", "description": "ID from open_raw_disk or open_source."})
}

fn extent_argument() -> Value {
    json!({"type": "object", "description": "Compare this extent of the open source (a partition of a whole disk) as a source of its own, instead of the whole of it.", "properties": {"start": {"type": "integer"}, "length": {"type": "integer"}}, "required": ["start", "length"]})
}

/// The open source, or the extent of it the arguments name, as a source of its own.
fn view_of(disk: &RawDisk, args: &Value) -> Result<RawDisk> {
    match args.get("extent").filter(|value| !value.is_null()) {
        Some(extent) => disk.extent(need_u64(extent, "start")?, need_u64(extent, "length")?),
        None => RawDisk::with_source(&disk.path, disk.source.clone(), disk.length),
    }
}

pub fn definitions() -> Vec<Definition> {
    vec![
        Definition {
            name: "list_sources",
            description: "Lists every source stored on this Mac: a drive or image recognised by its markers (4 KiB blocks spread over it), with its size, when it was first and last seen, the path it was last at, and how many scans it has. A source is not a device number or a path: the same drive is the same source when it comes back under another number. Read only.",
            properties: json!({}),
            required: &[],
        },
        Definition {
            name: "identify_source",
            description: "Compares the open raw source with the stored sources of the same length, marker by marker, and says which it is: exact (all markers match), probable (60% or more, the rest changed or unreadable, with the changed offsets), weak (listed only with showAll) or different (never listed). Nothing stored is applied by this, and a match is sampled, not proof that every byte matches. Reads at most 20 blocks of 4 KiB, within a time limit. Read only.",
            properties: json!({"diskId": disk_id(), "extent": extent_argument(), "showAll": {"type": "boolean", "description": "Also list weak matches, below 60% but above 0%."}}),
            required: &["diskId"],
        },
        Definition {
            name: "list_scans",
            description: "Lists the scans stored for a source, newest work of the interpretation last used first. A scan is a source, an interpretation (an extent of the source and the format read there, or none for raw bytes), its options, the scanner version and the range it covers; changing any of them makes a new scan, and the earlier ones stay. Each shows its interpretation and where it came from (analysis or manual), what range it covers and how far it has read, its status, when it was made and last opened, its name, and how many bytes it stores on this Mac. Give sourceId for one source, or diskId to list the scans of every stored source the open raw source matches (showAll adds weak matches). Opening a scan reads what is stored and starts no scan. Read only.",
            properties: json!({"sourceId": {"type": "string", "description": "A source ID from list_sources."}, "diskId": disk_id(), "extent": extent_argument(), "showAll": {"type": "boolean", "description": "With diskId, also list weak matches."}}),
            required: &[],
        },
        Definition {
            name: "open_stored_scan",
            description: "Opens a stored scan on the open raw source, which must be its source. A filesystem scan opens as the volume, with the results it stored: nothing is scanned. A raw scan loads its findings, as resume_raw_scan does, and needs acceptSampledMatch. An exact match opens it; a probable match opens it only with acceptChangedMarkers, which says the changed markers were read and accepted; a weak or different one never does. The source is read-only.",
            properties: json!({"diskId": disk_id(), "scanId": {"type": "string", "description": "A scan ID from list_scans."}, "acceptChangedMarkers": {"type": "boolean", "description": "The changed markers of a probable match were read and accepted."}, "acceptSampledMatch": {"type": "boolean", "description": "Raw scans only: a sampled match does not prove the whole source is identical."}}),
            required: &["diskId", "scanId"],
        },
        Definition {
            name: "switch_interpretation",
            description: "Makes the scan for reading an extent of the open raw source as another format, or another extent, or with other options, and keeps the earlier scans. Returns the stored scan if this combination was made before (created is false), and starts nothing either way: open it with open_stored_scan or open_disk_region. Only the person using the app does this; an agent can suggest it.",
            properties: json!({
                "diskId": disk_id(),
                "format": {"type": "string", "description": "exFAT, FAT12, FAT16, FAT32 or NTFS."},
                "start": {"type": "integer", "description": "Absolute start of the extent in the source."},
                "length": {"type": "integer", "description": "Length of the extent in bytes."},
                "bootOffset": {"type": "integer", "description": "Absolute byte of boot evidence when it is not at the start of the extent."},
                "options": {"type": "object", "description": "Scan options. Different options are a different scan. Default none."},
                "origin": {"type": "string", "description": "analysis when boot evidence or a range analysis found it, manual when the person chose it. Default manual."},
                "acceptChangedMarkers": {"type": "boolean", "description": "Attach to a probable match of a stored source."}
            }),
            required: &["diskId", "format", "start", "length"],
        },
        Definition {
            name: "label_scan",
            description: "Gives a stored scan a name, so the list shows what it was for. Only the person using the app does this.",
            properties: json!({"scanId": {"type": "string"}, "label": {"type": "string", "description": "At most 128 bytes."}}),
            required: &["scanId", "label"],
        },
        Definition {
            name: "label_source",
            description: "Gives a stored source a name, such as the drive's label. Only the person using the app does this.",
            properties: json!({"sourceId": {"type": "string"}, "label": {"type": "string", "description": "At most 128 bytes."}}),
            required: &["sourceId", "label"],
        },
        Definition {
            name: "list_stale_regions",
            description: "Lists the regions of a scan that may be out of date, and the ones scanned again since. When a source is reopened on a probable match (some markers changed, for example because a mount wrote metadata), what the scan stored near the changed markers is marked stale until it is scanned again: each region has its start and end in the source, the changed marker offsets it came from, how far a rescan has read it, and when it was cleared. Each changed marker's 4 KiB block makes a region of that block with 1 MiB on each side, and a changed marker in the first 32 MiB makes the first 32 MiB stale (the boot sectors, allocation tables and root directory). Also lists the markers the source had before each rescan replaced them. Read only; only the person using the app scans the regions again, with rescan_changed_regions.",
            properties: json!({"scanId": {"type": "string", "description": "A scan ID from list_scans."}}),
            required: &["scanId"],
        },
        Definition {
            name: "rescan_changed_regions",
            description: "Scans the stale regions of a scan again, with the same interpretation and options, replacing what the scan stored there a batch at a time. Never starts by itself: opening a scan only marks the regions. A raw scan replaces its findings in the regions; a filesystem scan reads the clusters in them again, replaces their entries (the ones still there keep their ids) and rebuilds its tree. A region that is not finished stays stale and resumes where it stopped. When every region of the scan is done, the source gets the markers of the drive as it is now, so the next open of it is an exact match, and keeps its earlier markers in its history. The source is only read. Only the person using the app does this; an agent can suggest it.",
            properties: json!({
                "diskId": disk_id(),
                "scanId": {"type": "string", "description": "A scan ID from list_scans, opened on a probable match."},
                "extent": extent_argument(),
                "radiusBytes": {"type": "integer", "description": "Widen the regions to this margin on each side of a changed marker's block. Default 1048576 (1 MiB). Regions are never made smaller."},
                "headBytes": {"type": "integer", "description": "A changed marker before this offset makes everything before it stale. Default 33554432 (32 MiB)."}
            }),
            required: &["diskId", "scanId"],
        },
        Definition {
            name: "migrate_catalog",
            description: "Registers what earlier versions stored (volume workspaces, saved raw scans, saved hypotheses) as sources, interpretations and scans, in the places they are. It runs by itself when the app starts, so this is only for after another copy of an older version has been used. It changes nothing already registered, and copies the database to backups first if that was not done. Only the person using the app does this.",
            properties: json!({}),
            required: &[],
        },
    ]
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("argument {key} is required and must be a string"))
}

fn need_u64(args: &Value, key: &str) -> Result<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("argument {key} is required and must be a non-negative integer"))
}

fn flag(args: &Value, key: &str) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(false)
}

fn acceptance(args: &Value) -> Accept {
    if flag(args, "acceptChangedMarkers") {
        Accept::Probable
    } else {
        Accept::Exact
    }
}

fn listing(source: &SourceRecord, scans: Vec<ScanRecord>) -> Value {
    let stored: u64 = scans.iter().map(|scan| scan.stored_bytes).sum();
    json!({"source": source, "scans": scans, "storedBytes": stored})
}

/// Runs one tool. `disk` is the open raw source, for tools that name one. `person` is whether the
/// person using the app is asking, as opposed to an agent.
pub fn call(tool: &str, disk: Option<&RawDisk>, args: &Value, person: bool) -> Result<Value> {
    if PERSON_ONLY.contains(&tool) && !person {
        bail!("Only the person using the app changes what is stored. Suggest it to them instead.");
    }
    let need_disk = || disk.context("this tool needs diskId, from open_raw_disk");
    let catalog = Catalog::for_user()?;
    match tool {
        "list_sources" => Ok(json!({"sources": catalog.sources()?})),
        "identify_source" => {
            let view = view_of(need_disk()?, args)?;
            Ok(serde_json::to_value(
                catalog.identify(&view, flag(args, "showAll"))?,
            )?)
        }
        "list_scans" => {
            if let Some(id) = args.get("sourceId").and_then(Value::as_str) {
                return Ok(listing(&catalog.source(id)?, catalog.scans(id)?));
            }
            let view = view_of(need_disk()?, args)?;
            let identified = catalog.identify(&view, flag(args, "showAll"))?;
            let mut matches = Vec::new();
            for found in &identified.matches {
                let scans = catalog.scans(&found.source.id)?;
                let mut entry = serde_json::to_value(found)?;
                entry["scans"] = serde_json::to_value(&scans)?;
                matches.push(entry);
            }
            Ok(json!({
                "fingerprint": identified.fingerprint,
                "matches": matches,
                "hidden": identified.hidden,
            }))
        }
        "switch_interpretation" => {
            let disk = need_disk()?;
            let format = need_str(args, "format")?;
            ensure!(
                crate::hypotheses::FORMATS.contains(&format),
                "format must be one of {}",
                crate::hypotheses::FORMATS.join(", ")
            );
            let start = need_u64(args, "start")?;
            let length = need_u64(args, "length")?;
            ensure!(
                length > 0
                    && start
                        .checked_add(length)
                        .is_some_and(|end| end <= disk.length),
                "the extent is outside the source"
            );
            let boot_offset = args.get("bootOffset").and_then(Value::as_u64);
            let source = catalog.resolve_source(disk, acceptance(args), None)?;
            let (scan, created) = catalog.ensure_scan(&ScanRequest {
                source_id: source.source.id.clone(),
                kind: ScanKind::Filesystem,
                interpretation: InterpretationRequest {
                    format: format.into(),
                    start,
                    length,
                    boot_offset: boot_offset.filter(|offset| *offset != start),
                    origin: args
                        .get("origin")
                        .and_then(Value::as_str)
                        .unwrap_or("manual")
                        .into(),
                    confidence: 0.0,
                },
                options: args.get("options").cloned().unwrap_or_else(|| json!({})),
                scanner_version: FILESYSTEM_SCANNER_VERSION,
                range_start: start,
                range_end: start + length,
                volume_key: None,
                raw_scan_id: None,
                block_size: 0,
                block_count: 0,
            })?;
            Ok(json!({
                "scan": catalog.scan(&scan.id)?,
                "source": source.source,
                "created": created,
                "matchedBy": source.matched_by,
                "comparison": source.comparison,
                "declined": source.declined,
            }))
        }
        "label_scan" => Ok(json!({
            "scan": catalog.label_scan(need_str(args, "scanId")?, need_str(args, "label")?)?
        })),
        "label_source" => Ok(json!({
            "source": catalog.label_source(need_str(args, "sourceId")?, need_str(args, "label")?)?
        })),
        "migrate_catalog" => Ok(serde_json::to_value(migrate(&catalog)?)?),
        "list_stale_regions" => {
            let id = need_str(args, "scanId")?;
            let scan = catalog.scan(id)?;
            let history: Vec<Value> = catalog
                .marker_history(&scan.source_id)?
                .into_iter()
                .map(|entry| {
                    json!({
                        "replacedAtMillis": entry.replaced_at_millis,
                        "reason": entry.reason,
                        "markers": entry.markers,
                    })
                })
                .collect();
            Ok(json!({"scan": scan, "stale": catalog.stale_summary(id)?, "markerHistory": history}))
        }
        other => bail!("{other} is not a catalog tool"),
    }
}
