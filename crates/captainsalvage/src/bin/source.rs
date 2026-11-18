use anyhow::{bail, Context, Result};
use captainsalvage::engine::{Engine, AUTHOR_USER};
use serde_json::{json, Value};

pub const USAGE: &str = "usage: captainsalvage source <list|identify|scans|open|switch|label|migrate> [device-or-image] [options]\n\
  list                                      every source stored on this Mac\n\
  identify <device-or-image> [--show-all]   which stored source the device is, marker by marker\n\
  scans <device-or-image> [--show-all]      the stored scans of the sources the device matches\n\
  scans --source ID                         the stored scans of one source\n\
  open <device-or-image> --scan ID [--accept-changed-markers] [--accept-sampled-match]\n\
                                            open a stored scan, with its results, and start nothing\n\
  switch <device-or-image> --format FORMAT --start BYTES --length BYTES [--boot-offset BYTES] [--options JSON] [--accept-changed-markers]\n\
                                            make the scan for another format, extent or options; earlier scans stay\n\
  label --scan ID --label TEXT              name a scan\n\
  label --source ID --label TEXT            name a source\n\
  migrate                                   register what earlier versions stored (also runs at start)";

fn value<'a>(options: &mut std::slice::Iter<'a, String>, option: &str) -> Result<&'a String> {
    options
        .next()
        .with_context(|| format!("{option} needs a value"))
}

fn whole(text: &str, option: &str) -> Result<u64> {
    text.parse()
        .with_context(|| format!("{option} needs a whole number"))
}

/// Does what the matching MCP tool does, with the same arguments, as the person using the app,
/// and prints its answer.
pub fn run(arguments: &[String]) -> Result<()> {
    let command = arguments.first().map(String::as_str).context(USAGE)?;
    let tool = match command {
        "list" => "list_sources",
        "identify" => "identify_source",
        "scans" => "list_scans",
        "open" => "open_stored_scan",
        "switch" => "switch_interpretation",
        "label" => "label",
        "migrate" => "migrate_catalog",
        other => bail!("unknown source command {other}\n{USAGE}"),
    };
    let path = arguments.get(1).filter(|first| !first.starts_with("--"));
    let mut args = json!({});
    let mut options = arguments[1 + usize::from(path.is_some())..].iter();
    while let Some(option) = options.next() {
        let name = option.as_str();
        match name {
            "--show-all" => args["showAll"] = json!(true),
            "--accept-changed-markers" => args["acceptChangedMarkers"] = json!(true),
            "--accept-sampled-match" => args["acceptSampledMatch"] = json!(true),
            "--scan" => args["scanId"] = json!(value(&mut options, name)?),
            "--source" => args["sourceId"] = json!(value(&mut options, name)?),
            "--label" => args["label"] = json!(value(&mut options, name)?),
            "--format" => args["format"] = json!(value(&mut options, name)?),
            "--start" | "--length" | "--boot-offset" => {
                let key = if name == "--boot-offset" {
                    "bootOffset"
                } else {
                    &name[2..]
                };
                args[key] = json!(whole(value(&mut options, name)?, name)?);
            }
            "--options" => {
                args["options"] = serde_json::from_str(value(&mut options, name)?)
                    .context("--options needs a JSON object")?
            }
            other => bail!("unknown source option {other}\n{USAGE}"),
        }
    }
    let tool = if tool == "label" {
        if args.get("scanId").is_some() {
            "label_scan"
        } else {
            "label_source"
        }
    } else {
        tool
    };
    let engine = Engine::new();
    let needs_disk = matches!(
        tool,
        "identify_source" | "open_stored_scan" | "switch_interpretation"
    ) || (tool == "list_scans" && args.get("sourceId").is_none());
    if needs_disk {
        let path = path.with_context(|| format!("{command} needs a device or image\n{USAGE}"))?;
        let disk = engine.call("open_raw_disk", &json!({"path": path}), AUTHOR_USER)?;
        args["diskId"] = disk["id"].clone();
    }
    let answer: Value = engine.call(tool, &args, AUTHOR_USER)?;
    println!("{}", serde_json::to_string(&answer)?);
    Ok(())
}
