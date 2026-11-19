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
use std::path::{Path, PathBuf};
use std::process::Command;

pub(crate) const VOLUME_FILE: &str = "volume.json";
const WORKSPACE_PREFIX: &str = "vol-";
const NOT_APPLICABLE: &str = "Not applicable";
const HIDDEN_PARTITIONS: &[&str] = &["EFI"];
/// An APFS container holds volumes of its own, and is never what someone means to recover.
const HIDDEN_PARTITION_TYPE_PREFIX: &str = "Apple_";
const OPENABLE_FORMATS: &[&str] = &[
    "exfat",
    "ntfs",
    "unknown",
    "fat12",
    "fat16",
    "fat32",
    "ms-dos",
    "ms-dos fat12",
    "ms-dos fat16",
    "ms-dos fat32",
    "ms-dos (fat12)",
    "ms-dos (fat16)",
    "ms-dos (fat32)",
];
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
    /// What was found, by kind of file, largest first, for the start screen's chart.
    pub kinds: Vec<KindTotal>,
    /// Found folders whose parent is gone.
    pub lost_folders: u64,
    pub scanned: bool,
    pub files_found: u64,
    pub bytes_found: u64,
    pub files_passed: u64,
    pub files_damaged: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct KindTotal {
    pub group: String,
    pub files: u64,
    pub bytes: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupVolume {
    pub device: String,
    pub whole_disk: String,
    /// The physical disk this volume lives on, when `whole_disk` is a container macOS made from
    /// it (an APFS volume's container is `disk8`, made from a store on `disk7`). The start
    /// screen groups the volume under it, and not under a disk that is not a drive.
    pub physical_disk: Option<String>,
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

/// A whole physical disk, listed whether or not its partition table can be read, so a disk with
/// a corrupt or missing table can still be inspected as one run of bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupDisk {
    pub device: String,
    /// The maker's name for the disk, which says nothing about what is on it.
    pub name: String,
    pub bytes: u64,
    pub external: bool,
    /// What macOS reads the partition scheme as, if it reads one.
    pub scheme: Option<String>,
    pub readable: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct StartupReport {
    pub volumes: Vec<StartupVolume>,
    /// Every whole disk found, with or without volumes on it.
    pub disks: Vec<StartupDisk>,
    /// Volumes seen before that are not attached now.
    pub elsewhere: Vec<KnownVolume>,
    /// macOS's `disksleep` setting, in minutes. Zero means disks never sleep.
    pub disk_sleep_minutes: Option<u32>,
    /// What the disks and mounts looked like when this was built (see `startup_fingerprint`). Ask
    /// again with it as `since` and nothing is rebuilt unless something changed.
    #[serde(default)]
    pub fingerprint: String,
}

/// Finds partitions in `diskutil list` output, which name them at the end of each line.
pub fn parse_partitions(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|line| line.split_whitespace().last())
        .filter(|word| is_partition(word))
        .map(str::to_string)
        .collect()
}

/// Finds whole disks in `diskutil list` output, where each starts a block with `/dev/diskN (`.
pub fn parse_disks(list: &str) -> Vec<String> {
    list.lines()
        .filter_map(|line| line.strip_prefix("/dev/")?.split_whitespace().next())
        .filter(|word| is_whole_disk(word))
        .map(str::to_string)
        .collect()
}

fn is_whole_disk(word: &str) -> bool {
    word.strip_prefix("disk")
        .is_some_and(|number| !number.is_empty() && number.chars().all(|c| c.is_ascii_digit()))
}

/// Reads one whole disk from `diskutil info` output.
pub fn parse_disk(info: &str) -> Option<StartupDisk> {
    let identifier = info_field(info, "Device Identifier:").filter(|id| is_whole_disk(id))?;
    let node = info_field(info, "Device Node:")?;
    let device = node.replacen("/dev/disk", "/dev/rdisk", 1);
    let bytes = info_field(info, "Disk Size:")
        .and_then(|size| size.split_once('('))
        .and_then(|(_, rest)| rest.split_once(" Bytes"))
        .and_then(|(number, _)| number.parse().ok())
        .unwrap_or(0);
    Some(StartupDisk {
        name: info_field(info, "Device / Media Name:")
            .unwrap_or(identifier)
            .to_string(),
        bytes,
        external: info_field(info, "Device Location:") == Some("External"),
        scheme: applicable(info, "Content (IOContent):").map(str::to_string),
        readable: can_read(&device),
        device,
    })
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
        physical_disk: applicable(info, "APFS Physical Store:").and_then(physical_store_disk),
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

/// `disk7s2`, an APFS container's physical store, names `/dev/rdisk7`.
fn physical_store_disk(store: &str) -> Option<String> {
    let (disk, _) = store.strip_prefix("disk")?.split_once('s')?;
    (!disk.is_empty() && disk.chars().all(|c| c.is_ascii_digit()))
        .then(|| format!("/dev/rdisk{disk}"))
}

/// Asks the OS about permission only. The device is not opened, so the disk is not touched.
fn can_read(device: &str) -> bool {
    match CString::new(Path::new(device).as_os_str().as_bytes()) {
        // SAFETY: `name` is a valid NUL-terminated string for the whole call.
        Ok(name) => unsafe { libc::access(name.as_ptr(), libc::R_OK) == 0 },
        Err(_) => false,
    }
}

/// What `diskutil list` is asked for: the external physical disks, and the internal ones when
/// wanted. A window test (in a test-hooks build, with `SALVAGE_TEST_UI`) can name instead the one
/// disk it attached, in a file whose text is its identifier, so no real disk is ever listed. A
/// missing or empty file lists nothing.
fn list_requests(include_internal: bool) -> Vec<Vec<String>> {
    #[cfg(feature = "test-hooks")]
    if std::env::var_os("SALVAGE_TEST_UI").is_some() {
        if let Some(file) = std::env::var_os("SALVAGE_TEST_STARTUP_DISK_FILE") {
            let disk = fs::read_to_string(file).unwrap_or_default();
            let disk = disk.trim().trim_start_matches("/dev/");
            return if is_whole_disk(disk) {
                vec![vec![format!("/dev/{disk}")]]
            } else {
                Vec::new()
            };
        }
    }
    let mut places = vec![vec!["external".to_string(), "physical".to_string()]];
    if include_internal {
        places.push(vec!["internal".to_string(), "physical".to_string()]);
    }
    places
}

fn diskutil_list(request: &[String]) -> Option<String> {
    let output = Command::new(DISKUTIL)
        .arg("list")
        .args(request)
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// A short text that is the same while the start screen's disks are the same: the lists of disks and
/// partitions `diskutil` gives, which of those are mounted and where (from `mount`, for the listed
/// devices only), and the settings the report shows beside them. It asks the operating system's
/// list of disks and mounts and nothing else: no disk is read, and no volume is looked at one by one.
pub(crate) fn fingerprint_of(lists: &[String], mounts: &str, settings: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut listed: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for list in lists {
        listed.extend(parse_disks(list));
        listed.extend(parse_partitions(list));
    }
    let mut mounted: Vec<&str> = mounts
        .lines()
        .filter(|line| {
            line.split_once(" on ")
                .and_then(|(device, _)| device.strip_prefix("/dev/"))
                .is_some_and(|device| listed.contains(device))
        })
        .collect();
    mounted.sort_unstable();
    let mut hasher = Sha256::new();
    for list in lists {
        hasher.update(list.as_bytes());
        hasher.update([0]);
    }
    for line in mounted {
        hasher.update(line.as_bytes());
        hasher.update([0]);
    }
    hasher.update(settings.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(12)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The fingerprint of the disks attached now: one `diskutil list` per place and one `mount`.
pub fn startup_fingerprint(include_internal: bool) -> String {
    let lists: Vec<String> = list_requests(include_internal)
        .iter()
        .filter_map(|request| diskutil_list(request))
        .collect();
    let mounts = Command::new("/sbin/mount")
        .output()
        .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
        .unwrap_or_default();
    let settings = format!(
        "{}\n{:?}",
        fs::read_to_string(FSTAB).unwrap_or_default(),
        crate::mountblock::list()
    );
    fingerprint_of(&lists, &mounts, &settings)
}

/// Whether a report has to be built for someone who last saw the one with fingerprint `since`.
pub(crate) fn report_needed(since: Option<&str>, now: &str) -> bool {
    since != Some(now)
}

/// The report, unless `since` is the fingerprint of the disks as they are now, when nothing has
/// changed and `None` says so. The fingerprint is taken before the report, so a change that
/// happens while the report is built shows on the next ask.
pub fn startup_report_since(
    include_internal: bool,
    since: Option<&str>,
) -> Result<Option<StartupReport>> {
    let fingerprint = startup_fingerprint(include_internal);
    if !report_needed(since, &fingerprint) {
        return Ok(None);
    }
    let mut report = startup_report(include_internal)?;
    report.fingerprint = fingerprint;
    Ok(Some(report))
}

fn list_volumes(request: &[String]) -> (Vec<StartupDisk>, Vec<StartupVolume>) {
    let Some(list) = diskutil_list(request) else {
        return (Vec::new(), Vec::new());
    };
    let disks = parse_disks(&list)
        .iter()
        .filter_map(|id| parse_disk(&diskutil_info(id)?))
        .collect();
    let volumes = parse_partitions(&list)
        .iter()
        .filter_map(|id| parse_volume(&diskutil_info(id)?))
        .collect();
    (disks, volumes)
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
    if let Ok(kinds) = database.kind_totals(FOUND_VERDICTS) {
        known.kinds = kinds
            .into_iter()
            .map(|(group, files, bytes)| KindTotal {
                group: group.to_string(),
                files,
                bytes,
            })
            .collect();
    }
    known.lost_folders = database.lost_folder_count().unwrap_or(0);
    if let Ok((passed, damaged, _)) = database.file_test_counts() {
        known.files_passed = passed;
        known.files_damaged = damaged;
    }
    Some(known)
}

/// The folders of scans that remembered a volume: the workspaces of earlier versions, named from
/// a serial number, and the folders of the scans a source has.
fn scan_folders(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(root) else {
        return Vec::new();
    };
    let mut folders: Vec<PathBuf> = entries
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(WORKSPACE_PREFIX)
        })
        .map(|entry| entry.path())
        .collect();
    let sources = fs::read_dir(root.join("sources"))
        .into_iter()
        .flatten()
        .flatten();
    for source in sources {
        folders.extend(
            fs::read_dir(source.path().join("scans"))
                .into_iter()
                .flatten()
                .flatten()
                .map(|scan| scan.path()),
        );
    }
    folders
}

/// Everything recorded under this app's data folder, most recently opened first. A volume scanned
/// more than once (another format, another extent) is listed once, with its latest scan.
pub fn known_volumes(root: &Path) -> Vec<KnownVolume> {
    let mut known: Vec<KnownVolume> = scan_folders(root)
        .iter()
        .filter_map(|folder| known_volume(folder))
        .collect();
    known.sort_by_key(|volume| std::cmp::Reverse(volume.record.last_opened_millis));
    let mut seen = std::collections::HashSet::new();
    known.retain(|volume| {
        volume
            .record
            .uuid
            .as_ref()
            .is_none_or(|uuid| seen.insert(uuid.clone()))
    });
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
        disks: Vec::new(),
        elsewhere,
        disk_sleep_minutes: None,
        fingerprint: String::new(),
    }
}

pub fn startup_report(include_internal: bool) -> Result<StartupReport> {
    let (mut disks, mut volumes) = (Vec::new(), Vec::new());
    for request in list_requests(include_internal) {
        let (more_disks, more_volumes) = list_volumes(&request);
        disks.extend(more_disks);
        volumes.extend(more_volumes);
    }
    let mut report = join_known(volumes, known_volumes(&Workspace::root()?));
    report.disks = disks;
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

#[cfg(test)]
mod tests {
    use super::*;

    const LIST: &str = "/dev/disk7 (external, physical):\n   #:                       TYPE NAME                    SIZE       IDENTIFIER\n   0:     FDisk_partition_scheme                        *2.0 TB     disk7\n   1:               Windows_NTFS seagate_2tb             1.4 TB     disk7s1\n";
    const MOUNTS: &str = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n/dev/disk7s1 on /Volumes/seagate_2tb (ntfs, local, nodev, nosuid, read-only, noowners)\nmap auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n";

    fn print(list: &str, mounts: &str) -> String {
        fingerprint_of(&[list.to_string()], mounts, "")
    }

    #[test]
    fn the_same_disks_and_mounts_give_the_same_fingerprint() {
        assert_eq!(print(LIST, MOUNTS), print(LIST, MOUNTS));
    }

    #[test]
    fn a_volume_that_was_unmounted_changes_it() {
        let unmounted: String = MOUNTS
            .lines()
            .filter(|line| !line.starts_with("/dev/disk7s1"))
            .map(|line| format!("{line}\n"))
            .collect();
        assert_ne!(print(LIST, MOUNTS), print(LIST, &unmounted));
    }

    #[test]
    fn a_volume_that_was_mounted_somewhere_else_changes_it() {
        let moved = MOUNTS.replace("/Volumes/seagate_2tb", "/Volumes/seagate_2tb 1");
        assert_ne!(print(LIST, MOUNTS), print(LIST, &moved));
    }

    #[test]
    fn a_disk_attached_or_removed_changes_it() {
        let more = format!("{LIST}\n/dev/disk9 (external, physical):\n   0:     FDisk_partition_scheme                        *1.0 GB     disk9\n");
        assert_ne!(print(LIST, MOUNTS), print(&more, MOUNTS));
        assert_ne!(print(&more, MOUNTS), print("", MOUNTS));
    }

    #[test]
    fn mounts_of_things_that_are_not_listed_do_not_change_it() {
        // The system volume's snapshots, a network share and the like come and go without any disk
        // on the start screen having changed.
        let busy = format!(
            "{MOUNTS}/dev/disk3s5 on /System/Volumes/Data (apfs, local)\n//user@host/share on /Volumes/share (smbfs)\n"
        );
        assert_eq!(print(LIST, MOUNTS), print(LIST, &busy));
    }

    #[test]
    fn a_changed_setting_changes_it() {
        let one = fingerprint_of(&[LIST.to_string()], MOUNTS, "UUID=A none ntfs rw,noauto");
        let two = fingerprint_of(&[LIST.to_string()], MOUNTS, "");
        assert_ne!(one, two);
    }

    #[test]
    fn the_report_is_only_rebuilt_when_the_fingerprint_is_not_the_one_asked_about() {
        assert!(report_needed(None, "abc"));
        assert!(report_needed(Some("xyz"), "abc"));
        assert!(!report_needed(Some("abc"), "abc"));
    }
}
