//! Evidence of what macOS wrote to a volume. macOS mounts a volume as soon as its disk is
//! attached, and writes to it while it is mounted, which can happen before this app opens.

use crate::session::whole_disk_path;
use crate::source::BlockSource;
use serde::Serialize;
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

const IOREG: &str = "/usr/sbin/ioreg";
const DISKUTIL: &str = "/usr/sbin/diskutil";
const VOLUME_UUID_FIELD: &str = "Volume UUID:";
const DEVICE_NODE_FIELD: &str = "Device Node:";
const MOUNTED_FIELD: &str = "Mounted:";
const PROBE_BYTES: usize = 512;
const CAFFEINATE: &str = "/usr/bin/caffeinate";
const STATISTICS_LINE: &str = "\"Statistics\" = {";
const OPERATIONS_FIELD: &str = "\"Operations (Write)\"=";
const BYTES_FIELD: &str = "\"Bytes (Write)\"=";
/// The counter is one process call, and a status check can come many times a second.
const CHECK_INTERVAL: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WriteCounter {
    pub operations: u64,
    pub bytes: u64,
}

/// The kernel's write counter for a whole disk. It counts from when the disk was attached,
/// covers every partition and has no dates, so it is evidence and not proof.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskWrites {
    pub at_open: Option<WriteCounter>,
    pub now: Option<WriteCounter>,
    pub since_open: WriteCounter,
}

impl DiskWrites {
    pub fn between(at_open: Option<WriteCounter>, now: Option<WriteCounter>) -> Self {
        let since_open = match (at_open, now) {
            // A counter only falls when the disk was attached again, so all of it is new.
            (Some(open), Some(now)) if now.operations < open.operations => now,
            (Some(open), Some(now)) => WriteCounter {
                operations: now.operations - open.operations,
                bytes: now.bytes.saturating_sub(open.bytes),
            },
            _ => WriteCounter::default(),
        };
        Self {
            at_open,
            now,
            since_open,
        }
    }
}

fn field(line: &str, name: &str) -> Option<u64> {
    let rest = &line[line.find(name)? + name.len()..];
    let digits = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    rest[..digits].parse().ok()
}

/// Finds one disk's counter in `ioreg` output, where each driver's statistics come just
/// before the name of the disk it drives. `disk12` must not match `disk12s1`.
pub fn parse_write_counter(ioreg: &str, disk: &str) -> Option<WriteCounter> {
    let name = format!("\"BSD Name\" = \"{disk}\"");
    let mut statistics = None;
    for line in ioreg.lines() {
        if line.contains(STATISTICS_LINE) && line.contains(OPERATIONS_FIELD) {
            statistics = Some(line);
        } else if line.trim_end().ends_with(&name) {
            let line = statistics?;
            return Some(WriteCounter {
                operations: field(line, OPERATIONS_FIELD)?,
                bytes: field(line, BYTES_FIELD)?,
            });
        }
    }
    None
}

fn read_write_counter(disk: &str) -> Option<WriteCounter> {
    let output = Command::new(IOREG)
        .args(["-r", "-c", "IOBlockStorageDriver", "-w0", "-l"])
        .output()
        .ok()?;
    parse_write_counter(&String::from_utf8_lossy(&output.stdout), disk)
}

/// Follows one disk's counter from the moment a volume on it was opened.
pub(crate) struct WriteWatch {
    disk: Option<String>,
    at_open: Option<WriteCounter>,
    latest: Option<WriteCounter>,
    checked: Instant,
}

impl WriteWatch {
    /// Disk images and whole-disk paths have no parent disk to follow.
    pub(crate) fn open(path: &Path) -> Self {
        let disk = whole_disk_path(path)
            .and_then(|whole| Some(whole.file_name()?.to_str()?.strip_prefix('r')?.to_string()));
        let at_open = disk.as_deref().and_then(read_write_counter);
        Self {
            disk,
            at_open,
            latest: at_open,
            checked: Instant::now(),
        }
    }

    pub(crate) fn report(&mut self) -> DiskWrites {
        if self.checked.elapsed() >= CHECK_INTERVAL {
            self.latest = self.disk.as_deref().and_then(read_write_counter);
            self.checked = Instant::now();
        }
        DiskWrites::between(self.at_open, self.latest)
    }
}

static KEEP_AWAKE: Mutex<Option<Child>> = Mutex::new(None);

/// Stops macOS putting disks to sleep for as long as this process lives. A damaged old disk
/// can drop off USB when woken, and then macOS mounts it again and writes to it. This writes
/// nothing, and it ends by itself with the app. A drive's own firmware may still sleep.
pub fn keep_disks_awake() {
    let Ok(mut held) = KEEP_AWAKE.lock() else {
        return;
    };
    if held
        .as_mut()
        .is_some_and(|child| matches!(child.try_wait(), Ok(None)))
    {
        return;
    }
    *held = Command::new(CAFFEINATE)
        .args(["-m", "-w", &std::process::id().to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok();
}

pub fn disks_kept_awake() -> bool {
    KEEP_AWAKE
        .lock()
        .ok()
        .and_then(|mut held| {
            held.as_mut()
                .map(|child| matches!(child.try_wait(), Ok(None)))
        })
        .unwrap_or(false)
}

/// Where a disconnected volume turned up again. Disk numbers change on each attach.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskReturn {
    pub device: String,
    pub mounted: bool,
}

fn info_field<'a>(info: &'a str, name: &str) -> Option<&'a str> {
    info.lines()
        .find_map(|line| line.trim().strip_prefix(name))
        .map(str::trim)
        .filter(|value| !value.is_empty())
}

pub fn parse_volume_uuid(info: &str) -> Option<String> {
    info_field(info, VOLUME_UUID_FIELD).map(str::to_string)
}

/// Reads where a volume is from `diskutil info`, by the raw path this app reads it through.
pub fn parse_disk_return(info: &str) -> Option<DiskReturn> {
    let node = info_field(info, DEVICE_NODE_FIELD)?;
    Some(DiskReturn {
        device: node.replacen("/dev/disk", "/dev/rdisk", 1),
        mounted: info_field(info, MOUNTED_FIELD) == Some("Yes"),
    })
}

fn diskutil_info(target: &str) -> Option<String> {
    let output = Command::new(DISKUTIL)
        .args(["info", target])
        .output()
        .ok()?;
    Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Notices a disk that has gone even while nothing is reading it, and looks for its volume
/// coming back. A reconnected disk gets a new number, so a check by the old path would say
/// "not mounted" about a volume macOS has just mounted somewhere else.
pub(crate) struct ReturnWatch {
    uuid: Option<String>,
    checked: Option<Instant>,
    back: Option<DiskReturn>,
}

impl ReturnWatch {
    pub(crate) fn open(path: &Path) -> Self {
        let uuid = whole_disk_path(path)
            .and_then(|_| diskutil_info(path.to_str()?))
            .as_deref()
            .and_then(parse_volume_uuid);
        Self {
            uuid,
            checked: None,
            back: None,
        }
    }

    pub(crate) fn report(&mut self, source: &dyn BlockSource) -> (bool, Option<DiskReturn>) {
        if self
            .checked
            .is_none_or(|checked| checked.elapsed() >= CHECK_INTERVAL)
        {
            self.checked = Some(Instant::now());
            // Only reading tells: a device node can outlive its disk while it is held open.
            let mut sector = [0u8; PROBE_BYTES];
            let _ = source.read_exact_at(0, &mut sector);
            self.back = None;
            if source.is_gone() {
                self.back = self
                    .uuid
                    .as_deref()
                    .and_then(diskutil_info)
                    .as_deref()
                    .and_then(parse_disk_return);
            }
        }
        (source.is_gone(), self.back.clone())
    }
}

/// One day on which the folders macOS keeps on a volume changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountDate {
    pub date: String,
    pub entries: u64,
    pub bytes: u64,
    pub clusters: u64,
}

/// What the volume itself says about earlier mounts.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountHistory {
    pub dates: Vec<MountDate>,
    pub dirty_flag: bool,
    pub baseline_recorded_at_millis: u64,
}
