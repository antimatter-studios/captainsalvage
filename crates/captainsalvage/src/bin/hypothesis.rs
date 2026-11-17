use anyhow::{bail, Context, Result};
use captainsalvage::disk::RawDisk;
use serde_json::{json, Value};
use std::path::Path;

pub const USAGE: &str = "usage: captainsalvage hypothesis <list|save|update|delete|raise|read-layout> <device-or-image> [options]\n\
  list\n\
  save --format FORMAT --start BYTES --length BYTES [--label TEXT] [--confidence 0..1] [--boot-offset BYTES] [--virtual-start BYTES] [--disabled]\n\
  update --id ID [--start BYTES] [--length BYTES] [--format FORMAT] [--label TEXT] [--confidence 0..1] [--boot-offset BYTES|none] [--virtual-start BYTES] [--enabled | --disabled] [--layer N]\n\
  delete --id ID\n\
  raise --id ID\n\
  read-layout --offset BYTES --length BYTES   (prints hexadecimal)";

fn value<'a>(options: &mut std::slice::Iter<'a, String>, option: &str) -> Result<&'a String> {
    options
        .next()
        .with_context(|| format!("{option} needs a value"))
}

fn whole(text: &str, option: &str) -> Result<u64> {
    text.parse()
        .with_context(|| format!("{option} needs a whole number"))
}

/// Does what the matching MCP tool does, with the same arguments, and prints its answer.
pub fn run(arguments: &[String]) -> Result<()> {
    let command = arguments.first().map(String::as_str).context(USAGE)?;
    let tool = match command {
        "list" => "list_hypotheses",
        "save" => "save_hypothesis",
        "update" => "update_hypothesis",
        "delete" => "delete_hypothesis",
        "raise" => "raise_hypothesis",
        "read-layout" => "read_layout",
        other => bail!("unknown hypothesis command {other}\n{USAGE}"),
    };
    let path = arguments.get(1).context(USAGE)?;
    let mut args = json!({});
    let mut options = arguments[2..].iter();
    while let Some(option) = options.next() {
        let name = option.as_str();
        match name {
            "--id" => args["id"] = json!(value(&mut options, name)?),
            "--format" => args["format"] = json!(value(&mut options, name)?),
            "--label" => args["label"] = json!(value(&mut options, name)?),
            "--start" | "--length" | "--virtual-start" | "--layer" | "--offset" => {
                let key = match name {
                    "--virtual-start" => "virtualStart",
                    other => &other[2..],
                };
                args[key] = json!(whole(value(&mut options, name)?, name)?);
            }
            "--boot-offset" => {
                let text = value(&mut options, name)?;
                args["bootOffset"] = if text == "none" {
                    Value::Null
                } else {
                    json!(whole(text, name)?)
                };
            }
            "--confidence" => {
                args["confidence"] = json!(value(&mut options, name)?
                    .parse::<f64>()
                    .context("--confidence needs a number from 0 to 1")?)
            }
            "--enabled" => args["enabled"] = json!(true),
            "--disabled" => args["enabled"] = json!(false),
            other => bail!("unknown hypothesis option {other}\n{USAGE}"),
        }
    }
    let disk = RawDisk::open(Path::new(path))?;
    let answer = captainsalvage::range_tools::call(tool, &disk, &args)?;
    println!("{}", serde_json::to_string(&answer)?);
    Ok(())
}
