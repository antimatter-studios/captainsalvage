//! The range tools, shared by MCP and the command line so both do the same thing.
//!
//! Every tool here only reads the source, and keeps its own records on this Mac.

use crate::disk::RawDisk;
use crate::range_analysis::{analyze, Options};
use anyhow::{Context, Result};
use serde_json::{json, Value};

fn need_u64(args: &Value, key: &str) -> Result<u64> {
    args.get(key)
        .and_then(Value::as_u64)
        .with_context(|| format!("argument {key} is required and must be a non-negative integer"))
}

/// The tools this module answers.
pub const TOOLS: &[&str] = &["analyze_range"];

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
    }]
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
        other => anyhow::bail!("unknown range tool {other}"),
    }
}
