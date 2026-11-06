//! What the start screen shows: the disks attached now, and which of them this app has
//! seen before. Asking `diskutil` and checking permissions reads nothing from the disks.

use crate::map::MapDb;
use crate::mount::{diskutil_info, info_field, parse_volume_uuid, DISKUTIL};
use crate::workspace::{now_millis, Workspace};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::Command;

const VOLUME_FILE: &str = "volume.json";
const WORKSPACE_PREFIX: &str = "vol-";
const NOT_APPLICABLE: &str = "Not applicable";
const HIDDEN_PARTITIONS: &[&str] = &["EFI"];
/// An APFS container holds volumes of its own, and is never what someone means to recover.
const HIDDEN_PARTITION_TYPE_PREFIX: &str = "Apple_";
const OPENABLE_FORMATS: &[&str] = &["exfat", "unknown"];
/// Verdicts that count as something found by name.
const FOUND_VERDICTS: &[&str] = &["tested-good", "likely-intact", "plausible", "live"];
const FSTAB: &str = "/etc/fstab";
const PMSET: &str = "/usr/bin/pmset";
const FSTAB_NO_MOUNT: &str = "noauto";

/// What this app remembers about a volume it has opened.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VolumeRecord {
    pub uuid: Option<String>,
    pub name: String,
    pub bytes: u64,
    pub last_opened_millis: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KnownVolume {
    pub record: VolumeRecord,
    pub scanned: bool,
    pub files_found: u64,
    pub bytes_found: u64,
    pub files_passed: u64,
    pub files_damaged: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupVolume {
    pub device: String,
    pub whole_disk: String,
    pub name: String,
    pub bytes: u64,
    pub format: String,
    pub external: bool,
    pub mount_point: Option<String>,
    pub uuid: Option<String>,
    pub can_open: bool,
    pub readable: bool,
    pub known: Option<KnownVolume>,
    /// The `/etc/fstab` line that stops macOS mounting this volume.
    pub fstab_line: Option<String>,
    /// Whether that line, or one like it, is already in place.
    pub protected: bool,
    /// Whether the person asked Captain Salvage to keep this volume unmounted.
    pub mount_blocked: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupReport {
    pub volumes: Vec<StartupVolume>,
    /// Volumes seen before that are not attached now.
    pub elsewhere: Vec<KnownVolume>,
    /// macOS's `disksleep` setting, in minutes. Zero means disks never sleep.
    pub disk_sleep_minutes: Option<u32>,
}

/// Finds partitions in `diskutil list` output, which name them at the end of each line.
pub fn parse_partitions(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|word| is_partition(word))
        .map(str::to_string)
        .collect()
}

fn is_partition(word: &str) -> bool {
    word.strip_prefix("disk")
        .and_then(|rest| rest.split_once('s'))
        .is_some_and(|(disk, part)| {
            !disk.is_empty()
                && !part.is_empty()
                && disk.chars().all(|c| c.is_ascii_digit())
                && part.chars().all(|c| c.is_ascii_digit())
        })
}

fn applicable<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info_field(info, name).filter(|value| !value.starts_with(NOT_APPLICABLE))
}

fn fstab_line(uuid: &str, format: &str) -> Option<String> {
    let kind = match format.to_ascii_lowercase().as_str() {
        "exfat" => "exfat",
        "ntfs" => "ntfs",
        "apfs" => "apfs",
        name if name.starts_with("ms-dos") => "msdos",
        name if name.contains("mac os extended") => "hfs",
        _ => return None,
    };
    Some(format!("UUID={uuid} none {kind} rw,{FSTAB_NO_MOUNT}"))
}

/// True only for a live line that names this volume and stops it mounting. A commented
/// line does nothing, and neither does one that still says to mount it.
pub fn is_protected(fstab: &str, uuid: &str) -> bool {
    let wanted = format!("UUID={uuid}");
    fstab.lines().any(|line| {
        let mut words = line.split_whitespace();
        words.next() == Some(wanted.as_str())
            && words
                .nth(2)
                .is_some_and(|options| options.split(',').any(|option| option == FSTAB_NO_MOUNT))
    })
}

pub fn parse_disk_sleep(pmset: &str) -> Option<u32> {
    pmset.lines().find_map(|line| {
        let mut words = line.split_whitespace();
        (words.next() == Some("disksleep"))
            .then(|| words.next()?.parse().ok())
            .flatten()
    })
}

/// Reads one partition from `diskutil info` output.
pub fn parse_volume(info: &str) -> Option<StartupVolume> {
    let identifier = info_field(info, "Device Identifier:").filter(|id| is_partition(id))?;
    let node = info_field(info, "Device Node:")?;
    let device = node.replacen("/dev/disk", "/dev/rdisk", 1);
    let (whole, _) = identifier.strip_prefix("disk")?.split_once('s')?;
    let name = applicable(info, "Volume Name:").unwrap_or("");
    let partition_type = info_field(info, "Partition Type:").unwrap_or("");
    if HIDDEN_PARTITIONS.contains(&name) || partition_type.starts_with(HIDDEN_PARTITION_TYPE_PREFIX)
    {
        return None;
    }
    let format = applicable(info, "File System Personality:").unwrap_or("Unknown");
    let bytes = info_field(info, "Disk Size:")
        .and_then(|size| size.split_once('('))
        .and_then(|(_, rest)| rest.split_once(" Bytes"))
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(0);
    let mounted = info_field(info, "Mounted:") == Some("Yes");
    Some(StartupVolume {
        whole_disk: format!("/dev/rdisk{whole}"),
        name: name.to_string(),
        bytes,
        can_open: OPENABLE_FORMATS.contains(&format.to_ascii_lowercase().as_str()),
        format: format.to_string(),
        external: info_field(info, "Device Location:") == Some("External"),
        mount_point: mounted
            .then(|| applicable(info, "Mount Point:"))
            .flatten()
            .map(str::to_string),
        fstab_line: parse_volume_uuid(info)
            .as_deref()
            .and_then(|uuid| fstab_line(uuid, format)),
        protected: false,
        mount_blocked: false,
        uuid: parse_volume_uuid(info),
        readable: can_read(&device),
        device,
        known: None,
    })
}

/// Asks the OS about permission only. The device is not opened, so the disk is not touched.
fn can_read(device: &str) -> bool {
    match CString::new(Path::new(device).as_os_str().as_bytes()) {
        // SAFETY: `name` is a valid NUL-terminated string for the whole call.
        Ok(name) => unsafe { libc::access(name.as_ptr(), libc::R_OK) == 0 },
        Err(_) => false,
    }
}

fn list_volumes(place: &str) -> Vec<StartupVolume> {
    let Ok(output) = Command::new(DISKUTIL)
        .args(["list", place, "physical"])
        .output()
    else {
        return Vec::new();
    };
    parse_partitions(&String::from_utf8_lossy(&output.stdout))
        .iter()
        .filter_map(|id| parse_volume(&diskutil_info(id)?))
        .collect()
}

/// Records a volume being opened, so it can be recognised under any disk number later.
pub(crate) fn remember_volume(workspace: &Workspace, path: &Path) {
    let text = path.to_str().and_then(diskutil_info);
    let found = text.as_deref().and_then(parse_volume);
    let record = VolumeRecord {
        uuid: found.as_ref().and_then(|volume| volume.uuid.clone()),
        name: found.as_ref().map_or_else(
            || {
                path.file_stem()
                    .map(|stem| stem.to_string_lossy().into_owned())
                    .unwrap_or_default()
            },
            |volume| volume.name.clone(),
        ),
        bytes: found
            .as_ref()
            .map(|volume| volume.bytes)
            .or_else(|| fs::metadata(path).ok().map(|meta| meta.len()))
            .unwrap_or(0),
        last_opened_millis: now_millis(),
    };
    write_record(workspace.dir(), &record);
}

pub fn write_record(dir: &Path, record: &VolumeRecord) {
    if let Ok(text) = serde_json::to_string_pretty(record) {
        let _ = fs::write(dir.join(VOLUME_FILE), text);
    }
}

fn known_volume(dir: &Path) -> Option<KnownVolume> {
    let text = fs::read_to_string(dir.join(VOLUME_FILE)).ok()?;
    let record: VolumeRecord = serde_json::from_str(&text).ok()?;
    let mut known = KnownVolume {
        record,
        ..KnownVolume::default()
    };
    let database = dir.join("map.sqlite");
    if !database.exists() {
        return Some(known);
    }
    let Ok(database) = MapDb::open(&database) else {
        return Some(known);
    };
    if let Ok(Some(summary)) = database.summary() {
        known.scanned = true;
        for total in summary
            .verdicts
            .iter()
            .filter(|total| FOUND_VERDICTS.contains(&total.verdict.as_str()))
        {
            known.files_found += total.count;
            known.bytes_found += total.bytes;
        }
    }
    if let Ok((passed, damaged, _)) = database.file_test_counts() {
        known.files_passed = passed;
        known.files_damaged = damaged;
    }
    Some(known)
}

/// Everything recorded under this app's data folder, most recently opened first.
pub fn known_volumes(root: &Path) -> Vec<KnownVolume> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut known: Vec<KnownVolume> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(WORKSPACE_PREFIX)
        })
        .filter_map(|entry| known_volume(&entry.path()))
        .collect();
    known.sort_by_key(|volume| std::cmp::Reverse(volume.record.last_opened_millis));
    known
}

/// Matches by UUID, never by disk number, because the number changes on every attach.
pub fn join_known(mut volumes: Vec<StartupVolume>, known: Vec<KnownVolume>) -> StartupReport {
    let mut elsewhere = Vec::new();
    for volume in known {
        let attached = volumes
            .iter_mut()
            .find(|attached| attached.uuid.is_some() && attached.uuid == volume.record.uuid);
        match attached {
            Some(attached) => attached.known = Some(volume),
            None => elsewhere.push(volume),
        }
    }
    // What is recognised comes first, then what can be opened at all.
    volumes.sort_by_key(|volume| (volume.known.is_none(), !volume.can_open));
    StartupReport {
        volumes,
        elsewhere,
        disk_sleep_minutes: None,
    }
}

pub fn startup_report(include_internal: bool) -> Result<StartupReport> {
    let mut volumes = list_volumes("external");
    if include_internal {
        volumes.extend(list_volumes("internal"));
    }
    let mut report = join_known(volumes, known_volumes(&Workspace::root()?));
    let fstab = fs::read_to_string(FSTAB).unwrap_or_default();
    for volume in &mut report.volumes {
        volume.protected = volume
            .uuid
            .as_deref()
            .is_some_and(|uuid| is_protected(&fstab, uuid));
        volume.mount_blocked = volume
            .uuid
            .as_deref()
            .is_some_and(crate::mountblock::is_blocked);
    }
    report.disk_sleep_minutes = Command::new(PMSET)
        .arg("-g")
        .output()
        .ok()
        .and_then(|output| parse_disk_sleep(&String::from_utf8_lossy(&output.stdout)));
    Ok(report)
}
