//! Keeps chosen volumes unmounted while Captain Salvage is open.
//!
//! Every mount on macOS goes through Disk Arbitration: plugging a disk in, Finder, Disk
//! Utility and `diskutil mount`. A program may ask to approve each mount before it happens,
//! and refuse it. This registers for that, and refuses a mount when the volume's UUID is one
//! the person flagged. It needs no administrator rights and changes nothing on the system: the
//! registration ends when the app quits, and macOS mounts as usual again.
//!
//! The flags are the person's, kept in the app's folder, one per volume UUID. An agent may add
//! a flag, because that only protects a disk. Only the person may take one away.

use crate::mounttable::MountEntry;
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

const FILE_NAME: &str = "mount-blocks.json";

/// One volume the person asked to keep unmounted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MountBlock {
    pub uuid: String,
    /// The volume's name when it was flagged, to show it while it is not attached.
    pub name: String,
}

/// The flags in force, which the approval callback reads. Keyed by upper-case UUID.
fn blocked() -> &'static Mutex<BTreeMap<String, MountBlock>> {
    static BLOCKED: OnceLock<Mutex<BTreeMap<String, MountBlock>>> = OnceLock::new();
    BLOCKED.get_or_init(|| Mutex::new(BTreeMap::new()))
}

fn key(uuid: &str) -> String {
    uuid.trim().to_uppercase()
}

fn file(folder: &Path) -> PathBuf {
    folder.join(FILE_NAME)
}

/// Reads the flags saved in the app's folder.
pub fn load(folder: &Path) -> Vec<MountBlock> {
    std::fs::read_to_string(file(folder))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Puts these flags in force.
fn apply(blocks: &[MountBlock]) {
    if let Ok(mut current) = blocked().lock() {
        *current = blocks
            .iter()
            .map(|block| (key(&block.uuid), block.clone()))
            .collect();
    }
}

/// The flags in force now.
pub fn list() -> Vec<MountBlock> {
    blocked()
        .lock()
        .map(|current| current.values().cloned().collect())
        .unwrap_or_default()
}

pub fn is_blocked(uuid: &str) -> bool {
    blocked()
        .lock()
        .is_ok_and(|current| current.contains_key(&key(uuid)))
}

/// Adds or removes one flag, saves the flags and puts them in force. `by_person` is false for
/// an agent, which may add a flag but not remove one.
pub fn set(
    folder: &Path,
    uuid: &str,
    name: &str,
    block: bool,
    by_person: bool,
) -> Result<Vec<MountBlock>> {
    if uuid.trim().is_empty() {
        bail!("A volume UUID is needed.");
    }
    if !block && !by_person {
        bail!("Only the person using the app can let a volume mount again.");
    }
    let mut blocks: BTreeMap<String, MountBlock> = load(folder)
        .into_iter()
        .map(|found| (key(&found.uuid), found))
        .collect();
    if block {
        blocks.insert(
            key(uuid),
            MountBlock {
                uuid: key(uuid),
                name: name.to_string(),
            },
        );
    } else {
        blocks.remove(&key(uuid));
    }
    let blocks: Vec<MountBlock> = blocks.into_values().collect();
    std::fs::create_dir_all(folder)?;
    std::fs::write(file(folder), serde_json::to_string_pretty(&blocks)?)
        .context("saving the mount blocks")?;
    apply(&blocks);
    Ok(blocks)
}

/// Loads the saved flags and starts refusing their mounts, for as long as this process runs.
/// Calling it again only reloads the flags.
pub fn start(folder: &Path) -> Result<()> {
    apply(&load(folder));
    static STARTED: OnceLock<std::result::Result<(), String>> = OnceLock::new();
    STARTED
        .get_or_init(|| macos::register().map_err(|error| format!("{error:#}")))
        .clone()
        .map_err(|error| anyhow::anyhow!(error))
}

/// A flagged volume that is mounted anyway, because it was mounted before the app started:
/// at boot, at login, or while the app was closed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MountedAnyway {
    pub uuid: String,
    pub name: String,
    pub device: String,
    pub mount_point: String,
}

/// How long the answer to "which flagged volumes are mounted" is kept while the list of mounts does not
/// change. The window asks about once a quarter minute, and each ask used to start a `diskutil info`
/// process for every mounted volume, which also asks the disk.
const MOUNTED_CACHE_AGE: Duration = Duration::from_secs(30);

/// The flagged volumes that are mounted now. Every mounted volume is looked at, whatever kind of
/// disk it is on, by the UUID macOS reports for it. The list of mounts is the kernel's own, so asking
/// starts no process; `diskutil info` is run once for a mounted volume, and again only for a volume
/// that was not there before. The answer is kept until the mounts or the flags change, or for half a
/// minute. Callers that arrive together wait for one answer instead of each asking.
pub fn mounted_anyway() -> Result<Vec<MountedAnyway>> {
    Ok(real_watch().mounted(false))
}

fn real_watch() -> &'static MountedWatch {
    static WATCH: OnceLock<MountedWatch> = OnceLock::new();
    WATCH.get_or_init(|| {
        MountedWatch::new(
            Box::new(crate::mounttable::entries),
            Box::new(crate::mount::diskutil_info),
            Box::new(|| list().iter().map(|block| key(&block.uuid)).collect()),
            MOUNTED_CACHE_AGE,
        )
    })
}

type VolumeKey = (String, String, String);
/// Asks the system about one device, as `diskutil info` would.
type InfoReader = Box<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What is known about the mounted volumes, with the system asked through functions it is given, so a
/// test can count the questions.
pub(crate) struct MountedWatch {
    table: Box<dyn Fn() -> Vec<MountEntry> + Send + Sync>,
    info: InfoReader,
    flagged: Box<dyn Fn() -> Vec<String> + Send + Sync>,
    max_age: Duration,
    state: Mutex<WatchState>,
}

#[derive(Default)]
struct WatchState {
    /// For each mounted volume (device, mount point, type): its UUID and name, when it has them.
    volumes: HashMap<VolumeKey, Option<(String, String)>>,
    answer: Option<(String, Instant, Vec<MountedAnyway>)>,
}

impl MountedWatch {
    pub(crate) fn new(
        table: Box<dyn Fn() -> Vec<MountEntry> + Send + Sync>,
        info: InfoReader,
        flagged: Box<dyn Fn() -> Vec<String> + Send + Sync>,
        max_age: Duration,
    ) -> Self {
        Self {
            table,
            info,
            flagged,
            max_age,
            state: Mutex::new(WatchState::default()),
        }
    }

    /// The flagged volumes that are mounted. With `fresh`, the list of mounts is read again whatever the
    /// age of the kept answer (a volume already looked at is not asked about again).
    pub(crate) fn mounted(&self, fresh: bool) -> Vec<MountedAnyway> {
        let flagged = (self.flagged)();
        if flagged.is_empty() {
            return Vec::new();
        }
        let disks: Vec<MountEntry> = (self.table)()
            .into_iter()
            .filter(|entry| entry.device.starts_with("/dev/disk"))
            .collect();
        let fingerprint = format!(
            "{}|{}",
            disks
                .iter()
                .map(MountEntry::line)
                .collect::<Vec<_>>()
                .join("\n"),
            flagged.join(",")
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if !fresh {
            if let Some((seen, at, found)) = &state.answer {
                if *seen == fingerprint && at.elapsed() < self.max_age {
                    return found.clone();
                }
            }
        }
        let volume = |entry: &MountEntry| -> VolumeKey {
            (
                entry.device.clone(),
                entry.mount_point.clone(),
                entry.fs_type.clone(),
            )
        };
        state
            .volumes
            .retain(|kept, _| disks.iter().any(|entry| volume(entry) == *kept));
        for entry in &disks {
            if state.volumes.contains_key(&volume(entry)) {
                continue;
            }
            // A question that could not be asked is asked again next time; an answer without a UUID is kept.
            if let Some(info) = (self.info)(&entry.device) {
                let found = crate::mount::parse_volume_uuid(&info).map(|uuid| {
                    let name = crate::mount::info_field(&info, "Volume Name:").unwrap_or("");
                    (key(&uuid), name.to_string())
                });
                state.volumes.insert(volume(entry), found);
            }
        }
        let found: Vec<MountedAnyway> = disks
            .iter()
            .filter_map(|entry| {
                let (uuid, name) = state.volumes.get(&volume(entry))?.clone()?;
                flagged.contains(&uuid).then(|| MountedAnyway {
                    uuid,
                    name,
                    device: entry.device.clone(),
                    mount_point: entry.mount_point.clone(),
                })
            })
            .collect();
        state.answer = Some((fingerprint, Instant::now(), found.clone()));
        found
    }
}

/// Unmounts one flagged volume, and only that volume: never the whole disk, never an eject.
/// Unmounting writes a little to the volume, so only the person may ask for it.
pub fn unmount_flagged(uuid: &str, by_person: bool) -> Result<String> {
    if !by_person {
        bail!("Only the person using the app can unmount a disk. Tell them it is mounted, and why that matters.");
    }
    let found = real_watch()
        .mounted(true)
        .into_iter()
        .find(|volume| key(&volume.uuid) == key(uuid))
        .context("That volume is not mounted, or is not one you asked to keep unmounted.")?;
    let device = found.device.trim_start_matches("/dev/");
    // A volume is a slice of a disk, "disk6s2". A whole disk is never unmounted here.
    if !device.starts_with("disk") || !device[4..].contains('s') {
        bail!("{device} is not a single volume, so it is left alone.");
    }
    let output = crate::proc::output(
        std::process::Command::new(crate::mount::DISKUTIL).args(["unmount", device]),
    )
    .context("running diskutil")?;
    let said = String::from_utf8_lossy(&output.stdout).trim().to_string()
        + String::from_utf8_lossy(&output.stderr).trim();
    if !output.status.success() {
        bail!("macOS would not unmount {}: {said}", found.name);
    }
    Ok(said)
}

#[cfg(target_os = "macos")]
mod macos {
    use anyhow::{bail, Result};
    use std::ffi::{c_char, c_void, CStr};

    type CfRef = *const c_void;
    const UTF8: u32 = 0x0800_0100;
    /// kDAReturnNotPermitted.
    const NOT_PERMITTED: i32 = 0xF8DA_0008_u32 as i32;
    const REFUSAL: &CStr = c"Captain Salvage is keeping this volume unmounted.";
    const QUEUE_LABEL: &CStr = c"com.antimatterstudios.captainsalvage.mountblock";

    #[link(name = "CoreFoundation", kind = "framework")]
    extern "C" {
        fn CFDictionaryGetValue(dictionary: CfRef, key: CfRef) -> CfRef;
        fn CFUUIDCreateString(allocator: CfRef, uuid: CfRef) -> CfRef;
        fn CFStringGetCString(text: CfRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
        fn CFStringCreateWithCString(allocator: CfRef, text: *const c_char, encoding: u32)
            -> CfRef;
        fn CFRelease(object: CfRef);
    }

    #[link(name = "DiskArbitration", kind = "framework")]
    extern "C" {
        static kDADiskDescriptionVolumeUUIDKey: CfRef;
        fn DASessionCreate(allocator: CfRef) -> CfRef;
        fn DASessionSetDispatchQueue(session: CfRef, queue: *mut c_void);
        fn DARegisterDiskMountApprovalCallback(
            session: CfRef,
            matching: CfRef,
            callback: extern "C" fn(CfRef, *mut c_void) -> CfRef,
            context: *mut c_void,
        );
        fn DADiskCopyDescription(disk: CfRef) -> CfRef;
        fn DADissenterCreate(allocator: CfRef, status: i32, text: CfRef) -> CfRef;
    }

    extern "C" {
        fn dispatch_queue_create(label: *const c_char, attributes: *const c_void) -> *mut c_void;
    }

    /// The UUID of the volume on this disk, if it has one.
    fn volume_uuid(disk: CfRef) -> Option<String> {
        // SAFETY: Core Foundation calls on objects Disk Arbitration handed us. Each object
        // created here is released once, and the borrowed UUID is not.
        unsafe {
            let description = DADiskCopyDescription(disk);
            if description.is_null() {
                return None;
            }
            let uuid = CFDictionaryGetValue(description, kDADiskDescriptionVolumeUUIDKey);
            let text = if uuid.is_null() {
                None
            } else {
                let string = CFUUIDCreateString(std::ptr::null(), uuid);
                let mut buffer = [0 as c_char; 64];
                let found = !string.is_null()
                    && CFStringGetCString(string, buffer.as_mut_ptr(), buffer.len() as isize, UTF8)
                        != 0;
                if !string.is_null() {
                    CFRelease(string);
                }
                found.then(|| {
                    CStr::from_ptr(buffer.as_ptr())
                        .to_string_lossy()
                        .into_owned()
                })
            };
            CFRelease(description);
            text
        }
    }

    extern "C" fn approve(disk: CfRef, _context: *mut c_void) -> CfRef {
        match volume_uuid(disk) {
            Some(uuid) if super::is_blocked(&uuid) => {
                // SAFETY: the dissenter is handed to Disk Arbitration, which releases it.
                unsafe {
                    let text = CFStringCreateWithCString(std::ptr::null(), REFUSAL.as_ptr(), UTF8);
                    let dissenter = DADissenterCreate(std::ptr::null(), NOT_PERMITTED, text);
                    if !text.is_null() {
                        CFRelease(text);
                    }
                    dissenter
                }
            }
            _ => std::ptr::null(),
        }
    }

    pub fn register() -> Result<()> {
        // SAFETY: the session and queue live for the rest of the process, on purpose: the
        // registration must last as long as the app does.
        unsafe {
            let session = DASessionCreate(std::ptr::null());
            if session.is_null() {
                bail!("Disk Arbitration would not start a session");
            }
            let queue = dispatch_queue_create(QUEUE_LABEL.as_ptr(), std::ptr::null());
            DARegisterDiskMountApprovalCallback(
                session,
                std::ptr::null(),
                approve,
                std::ptr::null_mut(),
            );
            DASessionSetDispatchQueue(session, queue);
        }
        Ok(())
    }
}

#[cfg(not(target_os = "macos"))]
mod macos {
    pub fn register() -> anyhow::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The flags in force are one list for the whole process, so these tests take turns.
    fn folder() -> (std::sync::MutexGuard<'static, ()>, tempfile_folder::Folder) {
        static TURN: Mutex<()> = Mutex::new(());
        let turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        (turn, tempfile_folder::Folder::new())
    }

    /// A throwaway folder, without another dependency.
    mod tempfile_folder {
        pub struct Folder(pub std::path::PathBuf);
        impl Folder {
            pub fn new() -> Folder {
                let path = std::env::temp_dir().join(format!(
                    "salvage-mountblock-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map_or(0, |time| time.as_nanos())
                ));
                std::fs::create_dir_all(&path).unwrap();
                Folder(path)
            }
        }
        impl Drop for Folder {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
    }

    #[test]
    fn a_block_is_saved_and_in_force_whatever_the_case_of_the_uuid() {
        let (_turn, place) = folder();
        set(&place.0, "aaaa-1111", "Old disk", true, true).unwrap();
        assert!(is_blocked("AAAA-1111"));
        assert_eq!(
            load(&place.0),
            vec![MountBlock {
                uuid: "AAAA-1111".into(),
                name: "Old disk".into()
            }]
        );
        set(&place.0, "AAAA-1111", "Old disk", false, true).unwrap();
        assert!(!is_blocked("aaaa-1111"));
        assert!(load(&place.0).is_empty());
    }

    #[test]
    fn mounted_disks_are_read_from_mount() {
        let mounts = "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n\
            devfs on /dev (devfs, local, nobrowse)\n\
            /dev/disk6s2 on /Volumes/Un titled (exfat, local, nodev, nosuid, noowners)\n\
            map auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)";
        let found: Vec<(String, String)> = crate::mounttable::parse_mount_output(mounts)
            .into_iter()
            .filter(|entry| entry.device.starts_with("/dev/disk"))
            .map(|entry| (entry.device, entry.mount_point))
            .collect();
        assert_eq!(
            found,
            vec![
                ("/dev/disk3s1s1".to_string(), "/".to_string()),
                ("/dev/disk6s2".to_string(), "/Volumes/Un titled".to_string()),
            ]
        );
    }

    /// A watch over a made-up list of mounts that counts the questions put to the system.
    struct Pretend {
        watch: MountedWatch,
        table: std::sync::Arc<Mutex<Vec<MountEntry>>>,
        tables_read: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        asked: std::sync::Arc<Mutex<Vec<String>>>,
    }

    fn entry(device: &str, point: &str) -> MountEntry {
        MountEntry {
            device: device.into(),
            mount_point: point.into(),
            fs_type: "exfat".into(),
        }
    }

    fn pretend(max_age: Duration, flagged: &[&str]) -> Pretend {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::Arc;
        let table = Arc::new(Mutex::new(vec![
            entry("/dev/disk3s1s1", "/"),
            entry("/dev/disk7s1", "/Volumes/Holiday"),
            entry("devfs", "/dev"),
        ]));
        let tables_read = Arc::new(AtomicUsize::new(0));
        let asked = Arc::new(Mutex::new(Vec::new()));
        let (shown, counted, questions) = (table.clone(), tables_read.clone(), asked.clone());
        let flagged: Vec<String> = flagged.iter().map(|uuid| key(uuid)).collect();
        let watch = MountedWatch::new(
            Box::new(move || {
                counted.fetch_add(1, Ordering::Relaxed);
                shown.lock().unwrap().clone()
            }),
            Box::new(move |device| {
                questions.lock().unwrap().push(device.to_string());
                let uuid = match device {
                    "/dev/disk7s1" => "AAAA-1111",
                    "/dev/disk9s1" => "BBBB-2222",
                    _ => "",
                };
                Some(format!(
                    "   Volume Name:  Name of {device}\n   Volume UUID:  {uuid}\n"
                ))
            }),
            Box::new(move || flagged.clone()),
            max_age,
        );
        Pretend {
            watch,
            table,
            tables_read,
            asked,
        }
    }

    #[test]
    fn asking_again_with_the_same_mounts_asks_the_system_nothing() {
        let world = pretend(Duration::from_secs(30), &["aaaa-1111"]);
        let first = world.watch.mounted(false);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].uuid, "AAAA-1111");
        assert_eq!(first[0].name, "Name of /dev/disk7s1");
        assert_eq!(first[0].mount_point, "/Volumes/Holiday");
        let asked = world.asked.lock().unwrap().len();
        // One `diskutil info` for each disk volume mounted, and none for devfs.
        assert_eq!(asked, 2, "{:?}", world.asked.lock().unwrap());
        for _ in 0..50 {
            assert_eq!(world.watch.mounted(false), first);
        }
        assert_eq!(
            world.asked.lock().unwrap().len(),
            asked,
            "a poll asked diskutil again"
        );
    }

    #[test]
    fn a_new_mount_asks_about_that_volume_only() {
        let world = pretend(Duration::from_secs(30), &["aaaa-1111", "bbbb-2222"]);
        assert_eq!(world.watch.mounted(false).len(), 1);
        world
            .table
            .lock()
            .unwrap()
            .push(entry("/dev/disk9s1", "/Volumes/Backup"));
        let found = world.watch.mounted(false);
        assert_eq!(
            found
                .iter()
                .map(|volume| volume.uuid.as_str())
                .collect::<Vec<_>>(),
            ["AAAA-1111", "BBBB-2222"]
        );
        assert_eq!(
            *world.asked.lock().unwrap(),
            ["/dev/disk3s1s1", "/dev/disk7s1", "/dev/disk9s1"],
            "only the new volume was asked about"
        );
        world
            .table
            .lock()
            .unwrap()
            .retain(|mount| mount.device != "/dev/disk7s1");
        assert_eq!(
            world
                .watch
                .mounted(false)
                .iter()
                .map(|volume| volume.uuid.as_str())
                .collect::<Vec<_>>(),
            ["BBBB-2222"]
        );
        assert_eq!(
            world.asked.lock().unwrap().len(),
            3,
            "an unmount asks nothing"
        );
    }

    #[test]
    fn an_old_answer_reads_the_mounts_again_but_does_not_ask_about_volumes_it_knows() {
        let world = pretend(Duration::ZERO, &["aaaa-1111"]);
        for _ in 0..5 {
            assert_eq!(world.watch.mounted(false).len(), 1);
        }
        assert_eq!(
            world.tables_read.load(std::sync::atomic::Ordering::Relaxed),
            5
        );
        assert_eq!(world.asked.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_fresh_look_reads_the_mounts_whatever_the_age() {
        let world = pretend(Duration::from_secs(3600), &["aaaa-1111"]);
        assert_eq!(world.watch.mounted(false).len(), 1);
        world
            .table
            .lock()
            .unwrap()
            .retain(|mount| mount.device != "/dev/disk7s1");
        assert_eq!(
            world.watch.mounted(false).len(),
            0,
            "the mounts changed, so the kept answer is not used"
        );
        world
            .table
            .lock()
            .unwrap()
            .push(entry("/dev/disk7s1", "/Volumes/Holiday"));
        assert_eq!(world.watch.mounted(true).len(), 1);
    }

    #[test]
    fn with_nothing_flagged_nothing_is_asked() {
        let world = pretend(Duration::from_secs(30), &[]);
        assert!(world.watch.mounted(false).is_empty());
        assert_eq!(
            world.tables_read.load(std::sync::atomic::Ordering::Relaxed),
            0
        );
        assert!(world.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn a_change_of_flags_is_seen_without_waiting_for_the_age() {
        use std::sync::Arc;
        let flagged = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = flagged.clone();
        let watch = MountedWatch::new(
            Box::new(|| vec![entry("/dev/disk7s1", "/Volumes/Holiday")]),
            Box::new(|_| {
                Some("   Volume Name:  Holiday\n   Volume UUID:  AAAA-1111\n".to_string())
            }),
            Box::new(move || seen.lock().unwrap().clone()),
            Duration::from_secs(3600),
        );
        assert!(watch.mounted(false).is_empty());
        flagged.lock().unwrap().push("AAAA-1111".into());
        assert_eq!(watch.mounted(false).len(), 1);
        flagged.lock().unwrap().clear();
        assert!(watch.mounted(false).is_empty());
    }

    #[test]
    fn callers_that_arrive_together_wait_for_one_answer() {
        use std::sync::Arc;
        let world = Arc::new(pretend(Duration::from_secs(30), &["aaaa-1111"]));
        let handles: Vec<_> = (0..8)
            .map(|_| {
                let world = world.clone();
                std::thread::spawn(move || world.watch.mounted(false).len())
            })
            .collect();
        for handle in handles {
            assert_eq!(handle.join().unwrap(), 1);
        }
        assert_eq!(
            world.asked.lock().unwrap().len(),
            2,
            "each volume was asked about once, not once per caller"
        );
    }

    #[test]
    fn only_the_person_may_unmount_a_flagged_volume() {
        let error = unmount_flagged("52D6C729-7E3F-310C-9073-7142E9EA7C05", false)
            .expect_err("an agent must not unmount");
        assert!(error.to_string().contains("Only the person"), "{error}");
    }

    #[test]
    fn an_agent_may_block_but_not_unblock() {
        let (_turn, place) = folder();
        set(&place.0, "bbbb-2222", "Disk", true, false).unwrap();
        assert!(is_blocked("bbbb-2222"));
        let refused = set(&place.0, "bbbb-2222", "Disk", false, false).unwrap_err();
        assert!(refused.to_string().contains("Only the person"));
        assert!(is_blocked("bbbb-2222"));
    }

    #[test]
    fn saved_blocks_come_back_on_the_next_start() {
        let (_turn, place) = folder();
        set(&place.0, "cccc-3333", "Disk", true, true).unwrap();
        apply(&[]);
        assert!(!is_blocked("cccc-3333"));
        start(&place.0).unwrap();
        assert!(is_blocked("cccc-3333"));
    }
}
