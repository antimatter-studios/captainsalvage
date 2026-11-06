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

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

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
