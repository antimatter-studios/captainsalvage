use anyhow::{bail, Context, Result};
use captainsalvage::disk::RawDisk;
use serde_json::{json, Value};
use std::path::Path;

pub const USAGE: &str = "usage: captainsalvage range analyze <device-or-image> [--offset BYTES] [--length BYTES] [--step BYTES] [--no-signatures] [--no-geometry] [--format NAME]... [--per-format N]";

fn number(value: Option<&String>, option: &str) -> Result<u64> {
    value
        .with_context(|| format!("{option} needs a number"))?
        .parse()
        .with_context(|| format!("{option} needs a whole number of bytes"))
}

/// Does what the MCP tool `analyze_range` does, with the same options, and prints its answer.
pub fn run(arguments: &[String]) -> Result<()> {
    match arguments.first().map(String::as_str) {
        Some("analyze") => {}
        _ => bail!("{USAGE}"),
    }
    let path = arguments.get(1).context(USAGE)?;
    let mut offset = 0;
    let mut length = None;
    let mut args = json!({});
    let mut formats: Vec<Value> = Vec::new();
    let mut options = arguments[2..].iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--offset" => offset = number(options.next(), "--offset")?,
            "--length" => length = Some(number(options.next(), "--length")?),
            "--step" => args["step"] = json!(number(options.next(), "--step")?),
            "--per-format" => args["perFormat"] = json!(number(options.next(), "--per-format")?),
            "--no-signatures" => args["signatures"] = json!(false),
            "--no-geometry" => args["geometry"] = json!(false),
            "--format" => formats.push(json!(options.next().context("--format needs a name")?)),
            other => bail!("unknown range option {other}\n{USAGE}"),
        }
    }
    if !formats.is_empty() {
        args["formats"] = Value::Array(formats);
    }
    let disk = RawDisk::open(Path::new(path))?;
    args["offset"] = json!(offset);
    args["length"] = json!(length.unwrap_or_else(|| disk.length.saturating_sub(offset)));
    let report = captainsalvage::range_tools::call("analyze_range", &disk, &args)?;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}
