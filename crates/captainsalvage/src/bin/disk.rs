use anyhow::{bail, ensure, Context, Result};
use captainsalvage::disk::RawDisk;
use captainsalvage::raw_scan::ScanStore;
use std::path::Path;

pub fn run(arguments: &[String]) -> Result<()> {
    let path = arguments.first().context(
        "usage: captainsalvage disk <device-or-image> [--scan | --list-scans | --resume ID --accept-sampled-match] [--offset BYTES] [--length BYTES]",
    )?;
    let mut scan = false;
    let mut start = 0;
    let mut length = None;
    let mut resume = None;
    let mut list_scans = false;
    let mut accept_sampled_match = false;
    let mut range_supplied = false;
    let mut options = arguments[1..].iter();
    while let Some(option) = options.next() {
        match option.as_str() {
            "--scan" => scan = true,
            "--list-scans" => list_scans = true,
            "--resume" => {
                resume = Some(options.next().context("--resume needs a scan ID")?.as_str())
            }
            "--accept-sampled-match" => accept_sampled_match = true,
            "--offset" => {
                range_supplied = true;
                start = options
                    .next()
                    .context("--offset needs bytes")?
                    .parse::<u64>()?
            }
            "--length" => {
                range_supplied = true;
                length = Some(
                    options
                        .next()
                        .context("--length needs bytes")?
                        .parse::<u64>()?,
                )
            }
            other => bail!("unknown disk option {other}"),
        }
    }
    ensure!(
        !list_scans || (!scan && resume.is_none()),
        "--list-scans cannot start or resume a scan"
    );
    ensure!(
        resume.is_none() || !range_supplied,
        "a resumed scan uses its saved range; do not supply --offset or --length"
    );
    ensure!(
        !accept_sampled_match || resume.is_some(),
        "--accept-sampled-match requires --resume"
    );
    ensure!(resume.is_none() || accept_sampled_match, "--resume requires --accept-sampled-match; matching samples do not prove every byte is identical");
    let disk = RawDisk::open(Path::new(path))?;
    let length = length.unwrap_or_else(|| disk.length.saturating_sub(start));
    start
        .checked_add(length)
        .filter(|end| *end <= disk.length)
        .context("scan range exceeds source")?;
    ensure!(length > 0, "scan range is empty");
    println!("{}", serde_json::to_string(&disk.overview())?);
    if list_scans {
        println!(
            "{}",
            serde_json::to_string(&ScanStore::for_user()?.list(&disk)?)?
        );
    } else if scan || resume.is_some() {
        let mut store = ScanStore::for_user()?;
        let saved = match resume {
            Some(id) => store.resume(&disk, id, accept_sampled_match)?,
            None => store.create(&disk, start, length)?,
        };
        println!("{}", serde_json::to_string(&saved)?);
        start = saved.next_offset;
        while start < saved.scan_end {
            let result = store.step(&disk, &saved.scan_id)?;
            start = result.batch.next_offset;
            println!("{}", serde_json::to_string(&result)?);
        }
    }
    Ok(())
}
