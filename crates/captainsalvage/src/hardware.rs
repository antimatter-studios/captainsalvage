//! What the drive's own hardware says it is: its model and serial number.
//!
//! The first sector and the markers say what a drive holds, which changes as it fails. A serial
//! number says which drive it is, whatever it holds, so a drive whose first sector cannot be read
//! any more still has an identity. The system tools (`ioreg`, `diskutil`) are asked about the
//! drive's place in the registry; nothing is read from the drive itself. A bridge that reports no
//! serial number, or one the same for every unit of its kind, gives none here.

use crate::catalog::Hardware;
use std::path::Path;

/// Serial numbers that tell nothing: bridges that report the same one for every unit.
const FAKE_SERIALS: &[&str] = &[
    "0123456789",
    "01234567890123456789",
    "123456789012",
    "1234567890",
    "NA",
    "N/A",
    "Unknown",
    "None",
];

impl Hardware {
    /// Whether the serial number can tell this drive from another of its kind.
    fn usable_serial(&self) -> Option<&str> {
        let serial = self.serial.as_deref()?.trim();
        let same_character = serial
            .chars()
            .all(|character| serial.starts_with(character));
        (serial.len() >= 4
            && !same_character
            && !FAKE_SERIALS
                .iter()
                .any(|fake| fake.eq_ignore_ascii_case(serial)))
        .then_some(serial)
    }

    /// The text that names this drive: its model and serial number. None without a usable serial
    /// number, because a model is the same for every drive of its kind.
    pub fn identity(&self) -> Option<String> {
        let serial = self.usable_serial()?;
        Some(format!(
            "{}|{serial}",
            self.model.as_deref().unwrap_or("").trim()
        ))
    }
}

/// The whole disk (`disk4`) a device path (`/dev/rdisk4s1`) belongs to.
pub fn whole_disk_name(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let name = name.strip_prefix('r').unwrap_or(name);
    let digits = name.strip_prefix("disk")?;
    let number: String = digits.chars().take_while(char::is_ascii_digit).collect();
    (!number.is_empty()).then(|| format!("disk{number}"))
}

/// The value of `"key"` in a line of `ioreg -l`, written `"key" = "value"` or inside a
/// dictionary as `"key"="value"`.
fn property(line: &str, key: &str) -> Option<String> {
    let quoted = format!("\"{key}\"");
    let mut from = 0;
    while let Some(found) = line[from..].find(&quoted) {
        let after = from + found + quoted.len();
        let rest = line[after..].trim_start();
        if let Some(value) = rest.strip_prefix('=') {
            if let Some(value) = value.trim_start().strip_prefix('"') {
                let end = value.find('"')?;
                return Some(value[..end].trim().to_string()).filter(|text| !text.is_empty());
            }
        }
        from = after;
    }
    None
}

/// The depth of an `ioreg -l` node line, from where its `+-o` starts.
fn node_column(line: &str) -> Option<usize> {
    line.find("+-o ")
}

/// Model and serial number of `disk` (a name such as `disk4`) from the text of `ioreg -l -w0`.
/// The properties of the drive's own entry and of the entries above it in the registry are
/// looked at, nearest first: a USB bridge keeps them on its device entry, an NVMe or SATA drive
/// on the entry for the drive.
pub fn parse_ioreg(text: &str, disk: &str) -> Hardware {
    let lines: Vec<&str> = text.lines().collect();
    let target = format!("\"BSD Name\" = \"{disk}\"");
    let Some(at) = lines.iter().position(|line| line.contains(&target)) else {
        return Hardware::default();
    };
    // The node the property belongs to, then each node above it.
    let mut nodes: Vec<(usize, usize)> = Vec::new();
    let mut column = usize::MAX;
    for index in (0..=at).rev() {
        if let Some(found) = node_column(lines[index]) {
            if found < column {
                nodes.push((index, found));
                column = found;
            }
        }
    }
    let mut vendor = None;
    let mut product = None;
    let mut serial = None;
    for (start, _) in nodes {
        // The node's properties run until the next node line.
        let end = (start + 1..lines.len())
            .find(|&index| node_column(lines[index]).is_some())
            .unwrap_or(lines.len());
        for line in &lines[start + 1..end] {
            serial = serial
                .or_else(|| property(line, "Serial Number"))
                .or_else(|| property(line, "USB Serial Number"))
                .or_else(|| property(line, "kUSBSerialNumberString"));
            product = product
                .or_else(|| property(line, "Product Name"))
                .or_else(|| property(line, "USB Product Name"))
                .or_else(|| property(line, "Model"));
            vendor = vendor
                .or_else(|| property(line, "Vendor Name"))
                .or_else(|| property(line, "USB Vendor Name"));
        }
    }
    let model = match (vendor, product) {
        (Some(vendor), Some(product)) if !product.starts_with(&vendor) => {
            Some(format!("{vendor} {product}"))
        }
        (_, Some(product)) => Some(product),
        (Some(vendor), None) => Some(vendor),
        (None, None) => None,
    };
    Hardware { model, serial }
}

/// The model `diskutil info` gives (`Device / Media Name:`), when the registry gave none.
pub fn parse_diskutil_model(info: &str) -> Option<String> {
    crate::mount::info_field(info, "Device / Media Name:").map(str::to_string)
}

/// What the hardware of the device at `path` says it is. Asks the system's own tools; reads
/// nothing from the drive. Empty off macOS, for an image file and when the tools give nothing.
pub fn read(path: &Path) -> Hardware {
    #[cfg(target_os = "macos")]
    {
        let Some(disk) = whole_disk_name(path).filter(|_| path.starts_with("/dev")) else {
            return Hardware::default();
        };
        let tree =
            crate::proc::output(std::process::Command::new("/usr/sbin/ioreg").args(["-l", "-w0"]))
                .ok()
                .map(|output| String::from_utf8_lossy(&output.stdout).into_owned());
        let mut hardware = tree
            .map(|text| parse_ioreg(&text, &disk))
            .unwrap_or_default();
        if hardware.model.is_none() {
            hardware.model = crate::mount::diskutil_info(&format!("/dev/{disk}"))
                .and_then(|info| parse_diskutil_model(&info));
        }
        hardware
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = path;
        Hardware::default()
    }
}

#[cfg(test)]
mod tests;
