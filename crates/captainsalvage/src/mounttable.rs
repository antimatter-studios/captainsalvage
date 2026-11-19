//! The list of mounted file systems, from the kernel and not from a process.
//!
//! `mount` is a program: starting it for every status call costs a process each time. `getfsstat` with
//! `MNT_NOWAIT` returns the kernel's own list of mounts without asking any of the file systems, so it
//! neither starts a process nor reads a disk, however damaged the disk is.

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    /// The device, as `/dev/disk7s1`.
    pub device: String,
    pub mount_point: String,
    /// The file system's type name, as `exfat`.
    pub fs_type: String,
}

impl MountEntry {
    /// The entry as one line, shaped like a line of `mount`'s output.
    pub fn line(&self) -> String {
        format!("{} on {} ({})", self.device, self.mount_point, self.fs_type)
    }
}

/// Every mount the system has now.
#[cfg(target_os = "macos")]
pub fn entries() -> Vec<MountEntry> {
    use std::ffi::CStr;
    // SAFETY: the first call only counts; the second fills a buffer this function owns, of the size
    // it was given, and the kernel writes at most that many entries.
    unsafe {
        let count = libc::getfsstat(std::ptr::null_mut(), 0, libc::MNT_NOWAIT);
        if count <= 0 {
            return Vec::new();
        }
        // Mounts can appear between the two calls, so leave room for a few more.
        let room = count as usize + 8;
        let mut buffer: Vec<libc::statfs> = Vec::with_capacity(room);
        let size = (room * std::mem::size_of::<libc::statfs>()) as libc::c_int;
        let filled = libc::getfsstat(buffer.as_mut_ptr(), size, libc::MNT_NOWAIT);
        if filled <= 0 {
            return Vec::new();
        }
        buffer.set_len((filled as usize).min(room));
        buffer
            .iter()
            .map(|entry| MountEntry {
                device: CStr::from_ptr(entry.f_mntfromname.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
                mount_point: CStr::from_ptr(entry.f_mntonname.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
                fs_type: CStr::from_ptr(entry.f_fstypename.as_ptr())
                    .to_string_lossy()
                    .into_owned(),
            })
            .collect()
    }
}

/// Every mount the system has now, from the kernel's `/proc/self/mounts`, which is a file and not a
/// process.
#[cfg(target_os = "linux")]
pub fn entries() -> Vec<MountEntry> {
    std::fs::read_to_string("/proc/self/mounts")
        .map(|text| parse_proc_mounts(&text))
        .unwrap_or_default()
}

/// Reads the kernel's mount list: `device mount-point type options 0 0`, with spaces and other
/// awkward bytes written as octal escapes such as `\040`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub fn parse_proc_mounts(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.split(' ');
            let device = unescape_octal(fields.next()?);
            let mount_point = unescape_octal(fields.next()?);
            let fs_type = unescape_octal(fields.next()?);
            Some(MountEntry {
                device,
                mount_point,
                fs_type,
            })
        })
        .collect()
}

fn unescape_octal(field: &str) -> String {
    let bytes = field.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let escape = bytes.get(at + 1..at + 4).filter(|digits| {
            bytes[at] == b'\\' && digits.iter().all(|digit| (b'0'..=b'7').contains(digit))
        });
        match escape {
            Some(digits) => {
                let value = digits
                    .iter()
                    .fold(0u32, |value, digit| value * 8 + u32::from(digit - b'0'));
                out.push(value as u8);
                at += 4;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Every mount the system has now, from `mount`'s output where the kernel's list cannot be read
/// without a process.
#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn entries() -> Vec<MountEntry> {
    let Ok(output) = crate::proc::output(&mut std::process::Command::new("/sbin/mount")) else {
        return Vec::new();
    };
    parse_mount_output(&String::from_utf8_lossy(&output.stdout))
}

/// Reads `mount`'s own output: `/dev/disk7s1 on /Volumes/X (exfat, local, ...)`.
pub fn parse_mount_output(text: &str) -> Vec<MountEntry> {
    text.lines()
        .filter_map(|line| {
            let (device, rest) = line.split_once(" on ")?;
            let (point, flags) = rest.rsplit_once(" (").unwrap_or((rest, ""));
            Some(MountEntry {
                device: device.to_string(),
                mount_point: point.to_string(),
                fs_type: flags
                    .split(',')
                    .next()
                    .unwrap_or("")
                    .trim_end_matches(')')
                    .trim()
                    .to_string(),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mount_output_is_read_into_entries_and_back_into_a_line() {
        let found = parse_mount_output(
            "/dev/disk3s1s1 on / (apfs, sealed, local, read-only, journaled)\n/dev/disk7s1 on /Volumes/My Disk (exfat, local, nodev, nosuid, noowners)\nmap auto_home on /System/Volumes/Data/home (autofs, automounted, nobrowse)\n",
        );
        assert_eq!(found.len(), 3);
        assert_eq!(found[1].device, "/dev/disk7s1");
        assert_eq!(found[1].mount_point, "/Volumes/My Disk");
        assert_eq!(found[1].fs_type, "exfat");
        assert_eq!(found[1].line(), "/dev/disk7s1 on /Volumes/My Disk (exfat)");
    }

    #[test]
    fn the_kernels_mount_file_is_read_with_its_escapes() {
        let found = parse_proc_mounts(
            "/dev/sda1 / ext4 rw,relatime 0 0\n/dev/loop3 /mnt/My\\040Disk exfat ro 0 0\n",
        );
        assert_eq!(found.len(), 2);
        assert_eq!(found[1].device, "/dev/loop3");
        assert_eq!(found[1].mount_point, "/mnt/My Disk");
        assert_eq!(found[1].fs_type, "exfat");
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn the_kernels_list_has_the_root_and_starts_no_process() {
        let before = crate::proc::processes_started();
        let found = entries();
        assert!(
            found.iter().any(|entry| entry.mount_point == "/"),
            "{found:?}"
        );
        assert_eq!(crate::proc::processes_started(), before);
    }
}
