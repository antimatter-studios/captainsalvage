//! What a copy did, file by file, and a later check that the copies are still right.
//!
//! Every copy already hashes a file as it reads it from the disk, and again from the
//! destination before keeping it. The manifest records each file with that hash. The report
//! reads the manifest, so it can be opened at any time, also after a restart. Checking again
//! reads every copied file from the destination, never the disk being recovered, and compares
//! it with the hash in the manifest: a copy that was changed or lost since is caught. Results
//! go to a file beside the manifest, so a check that is stopped or interrupted is not lost.

use crate::export::{manifest_path, ManifestRow};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

const CHECK_FILE: &str = "captain-salvage-check.jsonl";
const HASH_BUFFER_BYTES: usize = 1024 * 1024;

pub const CHECK_SAME: &str = "same";
pub const CHECK_CHANGED: &str = "changed";
pub const CHECK_MISSING: &str = "missing";

/// One copied file, as the copy left it and as the last check found it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportRow {
    pub id: i64,
    pub name: String,
    pub source_path: String,
    pub target: String,
    pub length: u64,
    pub bytes_written: u64,
    /// What the copy said: complete, or partial with the reason.
    pub status: String,
    /// What the last check found: same, changed or missing. Nothing before a check.
    pub check: Option<String>,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ReportTotals {
    pub files: u64,
    pub bytes: u64,
    pub complete: u64,
    pub partial: u64,
    pub checked: u64,
    pub same: u64,
    pub changed: u64,
    pub missing: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CopyReport {
    pub destination: String,
    pub manifest: String,
    pub totals: ReportTotals,
    /// A check is running on this destination now.
    pub checking: bool,
    pub rows: Vec<ReportRow>,
    /// Rows matching the filter, of which these are a page.
    pub matching: u64,
}

#[derive(Serialize, Deserialize)]
struct CheckLine {
    id: i64,
    check: String,
}

fn manifest_rows(destination: &Path) -> Vec<ManifestRow> {
    let mut rows: HashMap<i64, ManifestRow> = HashMap::new();
    if let Ok(file) = File::open(manifest_path(destination)) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(row) = serde_json::from_str::<ManifestRow>(&line) {
                rows.insert(row.id, row);
            }
        }
    }
    let mut rows: Vec<ManifestRow> = rows.into_values().collect();
    rows.sort_by(|a, b| a.target.cmp(&b.target));
    rows
}

fn checks(destination: &Path) -> HashMap<i64, String> {
    let mut found = HashMap::new();
    if let Ok(file) = File::open(destination.join(CHECK_FILE)) {
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            if let Ok(line) = serde_json::from_str::<CheckLine>(&line) {
                found.insert(line.id, line.check);
            }
        }
    }
    found
}

fn running() -> &'static Mutex<HashSet<PathBuf>> {
    static RUNNING: OnceLock<Mutex<HashSet<PathBuf>>> = OnceLock::new();
    RUNNING.get_or_init(|| Mutex::new(HashSet::new()))
}

fn is_checking(destination: &Path) -> bool {
    running().lock().is_ok_and(|set| set.contains(destination))
}

/// The copy at a destination, file by file. `filter` is all, partial, changed, missing or
/// unchecked.
pub fn copy_report(
    destination: &Path,
    filter: &str,
    offset: usize,
    limit: usize,
) -> Result<CopyReport> {
    let manifest = manifest_path(destination);
    if !manifest.exists() {
        bail!("Nothing has been copied to {} yet.", destination.display());
    }
    let checked = checks(destination);
    let mut totals = ReportTotals::default();
    let mut rows = Vec::new();
    for row in manifest_rows(destination) {
        let check = checked.get(&row.id).cloned();
        totals.files += 1;
        totals.bytes += row.bytes_written;
        if row.status == "complete" {
            totals.complete += 1;
        } else {
            totals.partial += 1;
        }
        match check.as_deref() {
            Some(CHECK_SAME) => totals.same += 1,
            Some(CHECK_CHANGED) => totals.changed += 1,
            Some(CHECK_MISSING) => totals.missing += 1,
            _ => {}
        }
        totals.checked += u64::from(check.is_some());
        let keep = match filter {
            "partial" => row.status != "complete",
            "changed" => check.as_deref() == Some(CHECK_CHANGED),
            "missing" => check.as_deref() == Some(CHECK_MISSING),
            "unchecked" => check.is_none(),
            _ => true,
        };
        if keep {
            rows.push(ReportRow {
                id: row.id,
                name: row.name,
                source_path: row.source_path,
                target: row.target,
                length: row.length,
                bytes_written: row.bytes_written,
                status: row.status,
                check,
            });
        }
    }
    let matching = rows.len() as u64;
    Ok(CopyReport {
        destination: destination.display().to_string(),
        manifest: manifest.display().to_string(),
        totals,
        checking: is_checking(destination),
        rows: rows.into_iter().skip(offset).take(limit).collect(),
        matching,
    })
}

fn hash_file(path: &Path) -> std::io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; HASH_BUFFER_BYTES];
    loop {
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        hasher.update(&buffer[..count]);
    }
    Ok(hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}

/// What one copied file is now: the same as when copied, changed, or gone.
fn check_one(row: &ManifestRow) -> &'static str {
    match hash_file(Path::new(&row.target)) {
        Err(_) => CHECK_MISSING,
        Ok(hash) if hash == row.sha256 => CHECK_SAME,
        Ok(_) => CHECK_CHANGED,
    }
}

/// Reads every copied file at a destination again and compares it with the manifest, in the
/// background. It reads only the destination. Earlier results are replaced.
pub fn check_copies(destination: &Path) -> Result<()> {
    let destination = destination.to_path_buf();
    if !manifest_path(&destination).exists() {
        bail!("Nothing has been copied to {} yet.", destination.display());
    }
    {
        let mut set = running()
            .lock()
            .map_err(|_| anyhow::anyhow!("check lock poisoned"))?;
        if !set.insert(destination.clone()) {
            bail!("The copies there are already being checked.");
        }
    }
    let results = destination.join(CHECK_FILE);
    let _ = fs::remove_file(&results);
    std::thread::spawn(move || {
        let output = OpenOptions::new().create(true).append(true).open(&results);
        if let Ok(mut output) = output {
            for row in manifest_rows(&destination) {
                let line = CheckLine {
                    id: row.id,
                    check: check_one(&row).to_string(),
                };
                if let Ok(text) = serde_json::to_string(&line) {
                    let _ = writeln!(output, "{text}");
                }
            }
        }
        if let Ok(mut set) = running().lock() {
            set.remove(&destination);
        }
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: i64, target: &Path, sha256: &str, status: &str) -> ManifestRow {
        serde_json::from_value(serde_json::json!({
            "id": id, "name": format!("f{id}"), "sourcePath": "/a", "target": target.display().to_string(),
            "length": 3, "bytesWritten": 3, "sha256": sha256, "status": status, "verdict": "live",
            "confidence": 1.0, "reason": "", "firstCluster": 5, "clustersFound": 1, "clustersNeeded": 1,
            "unreadableClusters": 0, "copiedAtMillis": 1
        }))
        .unwrap()
    }

    #[test]
    fn a_check_finds_copies_that_changed_or_went() {
        let place = std::env::temp_dir().join(format!("salvage-report-{}", std::process::id()));
        let _ = fs::remove_dir_all(&place);
        fs::create_dir_all(&place).unwrap();
        let (same, changed, gone) = (
            place.join("same"),
            place.join("changed"),
            place.join("gone"),
        );
        fs::write(&same, b"abc").unwrap();
        fs::write(&changed, b"abc").unwrap();
        let abc = hash_file(&same).unwrap();
        let mut manifest = String::new();
        for line in [
            row(1, &same, &abc, "complete"),
            row(2, &changed, &abc, "partial: chain ended early"),
            row(3, &gone, &abc, "complete"),
        ] {
            manifest += &serde_json::to_string(&line).unwrap();
            manifest.push('\n');
        }
        fs::write(manifest_path(&place), manifest).unwrap();
        fs::write(&changed, b"abd").unwrap();

        let before = copy_report(&place, "all", 0, 10).unwrap();
        assert_eq!(
            (
                before.totals.files,
                before.totals.complete,
                before.totals.partial,
                before.totals.checked
            ),
            (3, 2, 1, 0)
        );
        check_copies(&place).unwrap();
        for _ in 0..100 {
            if !is_checking(&place) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        let after = copy_report(&place, "all", 0, 10).unwrap();
        assert_eq!(
            (
                after.totals.same,
                after.totals.changed,
                after.totals.missing
            ),
            (1, 1, 1)
        );
        assert_eq!(copy_report(&place, "changed", 0, 10).unwrap().rows[0].id, 2);
        assert_eq!(copy_report(&place, "partial", 0, 10).unwrap().matching, 1);
        let _ = fs::remove_dir_all(&place);
    }
}
