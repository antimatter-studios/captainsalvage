//! Identifies what a cluster appears to hold, from its bytes alone.

pub const CLASS_UNSCANNED: u8 = 0;
pub const CLASS_ZEROS: u8 = 1;
pub const CLASS_FILL: u8 = 2;
pub const CLASS_DIRECTORY: u8 = 3;
pub const CLASS_TEXT: u8 = 4;
pub const CLASS_BINARY: u8 = 5;
pub const CLASS_HIGH_ENTROPY: u8 = 6;
pub const CLASS_MP3: u8 = 7;
pub const CLASS_JPEG: u8 = 8;
pub const CLASS_PNG: u8 = 9;
pub const CLASS_GIF: u8 = 10;
pub const CLASS_VIDEO: u8 = 11;
pub const CLASS_AUDIO: u8 = 12;
pub const CLASS_PDF: u8 = 13;
pub const CLASS_ARCHIVE: u8 = 14;
pub const CLASS_EXECUTABLE: u8 = 15;
pub const CLASS_NTFS: u8 = 16;
pub const CLASS_OLE: u8 = 17;
pub const CLASS_READ_ERROR: u8 = 18;
pub const CLASS_COUNT: usize = 19;

pub const CLASS_NAMES: [&str; CLASS_COUNT] = [
    "not scanned",
    "zeros",
    "single-byte fill",
    "exFAT directory",
    "text",
    "binary",
    "high entropy",
    "MP3 start",
    "JPEG start",
    "PNG start",
    "GIF start",
    "video start",
    "audio start",
    "PDF start",
    "archive start",
    "executable start",
    "NTFS metadata",
    "Office (OLE) start",
    "read error",
];

const HIGH_ENTROPY_BITS: f64 = 7.5;
const TEXT_PROBE_BYTES: usize = 4096;
const TEXT_MIN_RATIO: f64 = 0.93;
const ENTROPY_SCALE: f64 = 255.0 / 8.0;

const SIGNATURES: &[(&[u8], u8)] = &[
    (b"ID3", CLASS_MP3),
    (b"\xff\xd8\xff", CLASS_JPEG),
    (b"\x89PNG", CLASS_PNG),
    (b"GIF8", CLASS_GIF),
    (b"fLaC", CLASS_AUDIO),
    (b"OggS", CLASS_AUDIO),
    (b"MThd", CLASS_AUDIO),
    (b"FORM", CLASS_AUDIO),
    (b"%PDF", CLASS_PDF),
    (b"PK\x03\x04", CLASS_ARCHIVE),
    (b"Rar!", CLASS_ARCHIVE),
    (b"\x1f\x8b", CLASS_ARCHIVE),
    (b"7z\xbc\xaf", CLASS_ARCHIVE),
    (b"MZ", CLASS_EXECUTABLE),
    (b"\xcf\xfa\xed\xfe", CLASS_EXECUTABLE),
    (b"\x7fELF", CLASS_EXECUTABLE),
    (b"\x1a\x45\xdf\xa3", CLASS_VIDEO),
    (b"\x30\x26\xb2\x75", CLASS_VIDEO),
    (b"\x00\x00\x01\xba", CLASS_VIDEO),
    (b"FILE0", CLASS_NTFS),
    (b"INDX", CLASS_NTFS),
    (b"\xd0\xcf\x11\xe0", CLASS_OLE),
];

pub fn signature_class(data: &[u8]) -> Option<u8> {
    for (magic, class) in SIGNATURES {
        if data.starts_with(magic) {
            return Some(*class);
        }
    }
    if data.len() >= 12 {
        if data.starts_with(b"RIFF") {
            return Some(match &data[8..12] {
                b"WAVE" => CLASS_AUDIO,
                _ => CLASS_VIDEO,
            });
        }
        if &data[4..8] == b"ftyp" {
            return Some(CLASS_VIDEO);
        }
    }
    if data.len() >= 2 && data[0] == 0xFF && matches!(data[1], 0xFB | 0xFA | 0xF3 | 0xF2) {
        return Some(CLASS_MP3);
    }
    None
}

/// Granularity of the text-head map. One byte per cluster covers about 4 KiB.
pub const TEXT_HEAD_UNIT: u64 = 16;
pub const TEXT_HEAD_MAX_BYTES: usize = 254 * TEXT_HEAD_UNIT as usize;

/// Records how many leading bytes of a cluster read as text, as units plus one.
/// Zero is reserved for "not measured". A small file shares its cluster with
/// whatever was there before, so the whole cluster says little about the file:
/// macOS clears only the rest of the 4 KiB page, and older data stays beyond it.
pub fn text_head_value(data: &[u8]) -> u8 {
    let probe = &data[..data.len().min(TEXT_HEAD_MAX_BYTES)];
    let run = probe
        .iter()
        .take_while(|&&b| b == 9 || b == 10 || b == 13 || b >= 32 && b != 127)
        .count();
    (run as u64 / TEXT_HEAD_UNIT) as u8 + 1
}

/// Judges a text file by its own bytes, from the text-head map value.
pub fn text_head_match(head: u8, length: u64) -> ContentMatch {
    if head == 0 {
        return ContentMatch::Unknown;
    }
    let run = u64::from(head - 1) * TEXT_HEAD_UNIT;
    let wanted = length.min(TEXT_HEAD_MAX_BYTES as u64);
    if run + TEXT_HEAD_UNIT >= wanted * 9 / 10 {
        ContentMatch::Yes
    } else if run < wanted / 2 {
        ContentMatch::No
    } else {
        ContentMatch::Unknown
    }
}

/// Shannon entropy in bits per byte, computed over the whole buffer.
pub fn entropy_bits(data: &[u8]) -> f64 {
    let mut counts = [0u32; 256];
    for &byte in data {
        counts[usize::from(byte)] += 1;
    }
    let total = data.len() as f64;
    counts
        .iter()
        .filter(|&&count| count > 0)
        .map(|&count| {
            let share = f64::from(count) / total;
            -share * share.log2()
        })
        .sum()
}

/// Returns the content class of a cluster and its entropy scaled to 0-255.
pub fn classify(data: &[u8], holds_directory_sets: bool) -> (u8, u8) {
    let bits = entropy_bits(data);
    let scaled = (bits * ENTROPY_SCALE).round().min(255.0) as u8;
    if data.iter().all(|&byte| byte == 0) {
        return (CLASS_ZEROS, 0);
    }
    if data.iter().all(|&byte| byte == data[0]) {
        return (CLASS_FILL, 0);
    }
    if holds_directory_sets {
        return (CLASS_DIRECTORY, scaled);
    }
    if let Some(class) = signature_class(data) {
        return (class, scaled);
    }
    let probe = &data[..data.len().min(TEXT_PROBE_BYTES)];
    let printable = probe
        .iter()
        .filter(|&&b| (9..=13).contains(&b) || (32..127).contains(&b))
        .count();
    if printable as f64 / probe.len() as f64 > TEXT_MIN_RATIO {
        return (CLASS_TEXT, scaled);
    }
    let class = if bits >= HIGH_ENTROPY_BITS {
        CLASS_HIGH_ENTROPY
    } else {
        CLASS_BINARY
    };
    (class, scaled)
}

/// The class a file's first cluster should have, judged by its extension.
pub fn expected_class(name: &str) -> Option<u8> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    Some(match extension.as_str() {
        "mp3" => CLASS_MP3,
        "jpg" | "jpeg" => CLASS_JPEG,
        "png" => CLASS_PNG,
        "gif" => CLASS_GIF,
        "mp4" | "m4v" | "m4a" | "mov" | "avi" | "mkv" | "webm" | "3gp" | "wmv" | "wma" | "mpg"
        | "mpeg" => CLASS_VIDEO,
        "wav" | "flac" | "ogg" | "mid" | "midi" | "aif" | "aiff" => CLASS_AUDIO,
        "pdf" => CLASS_PDF,
        "zip" | "rar" | "gz" | "tgz" | "7z" | "jar" => CLASS_ARCHIVE,
        "exe" | "dll" => CLASS_EXECUTABLE,
        "doc" | "xls" | "ppt" => CLASS_OLE,
        "txt" | "md" | "js" | "ts" | "jsx" | "tsx" | "php" | "html" | "htm" | "css" | "json"
        | "xml" | "yml" | "yaml" | "rb" | "py" | "c" | "h" | "cpp" | "sql" | "sh" | "log"
        | "ini" | "csv" | "svg" => CLASS_TEXT,
        _ => return None,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ContentMatch {
    Yes,
    No,
    Unknown,
}

pub fn is_signature_class(class: u8) -> bool {
    (CLASS_MP3..=CLASS_OLE).contains(&class)
}

/// Compares the class a file's name implies with the content at its first cluster.
pub fn content_match(expected: Option<u8>, class: u8) -> ContentMatch {
    if class == CLASS_UNSCANNED {
        return ContentMatch::Unknown;
    }
    if class == CLASS_DIRECTORY || class == CLASS_READ_ERROR {
        return ContentMatch::No;
    }
    let Some(expected) = expected else {
        return ContentMatch::Unknown;
    };
    if class == expected {
        return ContentMatch::Yes;
    }
    let contradicted = class == CLASS_ZEROS
        || class == CLASS_FILL
        || is_signature_class(class)
        || (expected == CLASS_TEXT && class == CLASS_HIGH_ENTROPY);
    if contradicted {
        ContentMatch::No
    } else {
        ContentMatch::Unknown
    }
}

/// Fewer groups than classes, so the map can show them as distinct colors.
pub const GROUP_NOT_SCANNED: u8 = 0;
pub const GROUP_EMPTY: u8 = 1;
pub const GROUP_COMPRESSED: u8 = 2;
pub const GROUP_TEXT: u8 = 3;
pub const GROUP_AUDIO: u8 = 4;
pub const GROUP_IMAGE: u8 = 5;
pub const GROUP_VIDEO: u8 = 6;
pub const GROUP_DOCUMENT: u8 = 7;
pub const GROUP_BINARY: u8 = 8;
pub const GROUP_DIRECTORY: u8 = 9;
pub const GROUP_READ_ERROR: u8 = 10;
pub const GROUP_COUNT: usize = 11;

pub const GROUP_NAMES: [&str; GROUP_COUNT] = [
    "not scanned",
    "empty",
    "compressed or media data",
    "text",
    "audio start",
    "image start",
    "video start",
    "document or archive start",
    "binary",
    "exFAT directory",
    "read error",
];

pub fn content_group(class: u8) -> u8 {
    match class {
        CLASS_ZEROS | CLASS_FILL => GROUP_EMPTY,
        CLASS_HIGH_ENTROPY => GROUP_COMPRESSED,
        CLASS_TEXT => GROUP_TEXT,
        CLASS_MP3 | CLASS_AUDIO => GROUP_AUDIO,
        CLASS_JPEG | CLASS_PNG | CLASS_GIF => GROUP_IMAGE,
        CLASS_VIDEO => GROUP_VIDEO,
        CLASS_PDF | CLASS_ARCHIVE | CLASS_OLE | CLASS_EXECUTABLE => GROUP_DOCUMENT,
        CLASS_BINARY | CLASS_NTFS => GROUP_BINARY,
        CLASS_DIRECTORY => GROUP_DIRECTORY,
        CLASS_READ_ERROR => GROUP_READ_ERROR,
        _ => GROUP_NOT_SCANNED,
    }
}
