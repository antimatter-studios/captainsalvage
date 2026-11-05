//! Tests whether a byte stream behaves like the file type it claims to be.
//!
//! This is how a proposed reassembly gets checked against something other than
//! opinion. A walker follows the format's own structure through the stream and
//! reports how far it stays valid. If a join between two chains is wrong, the
//! walk usually breaks at or near that join, and the report gives the offset.

use flate2::read::DeflateDecoder;
use serde::Serialize;
use std::io::Read;

const ID3_HEADER_BYTES: usize = 10;
const MPEG_HEADER_BYTES: usize = 4;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Unwritten clusters read as zeros, which compressed image data never is for this long.
const JPEG_MAX_ZERO_RUN: usize = 4096;
const JPEG_FIRST_RESTART: u8 = 0xD0;
const JPEG_RESTART_COUNT: u8 = 8;
const PDF_SIGNATURE: &[u8] = b"%PDF-";
const OLE_SIGNATURE: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
/// Office and similar documents that are zip files inside.
const ZIP_EXTENSIONS: &[&str] = &[
    "zip", "docx", "xlsx", "pptx", "odt", "pages", "numbers", "key",
];
const TEXT_EXTENSIONS: &[&str] = &["txt", "md", "csv", "rtf"];
/// Bytes from the end of a PDF to look for its pointer to the cross-reference table.
const PDF_TAIL_BYTES: usize = 2048;
const PDF_XREF_LINE_BYTES: usize = 20;
const OLE_HEADER_BYTES: usize = 512;
const OLE_DIFAT_START: usize = 76;
const OLE_FREE_ENTRIES_FROM: u32 = 0xFFFF_FFFA;
const TEXT_WINDOW_BYTES: usize = 4096;
const MIN_PRINTABLE_SHARE: f64 = 0.85;
/// Bytes to search forward for the next MP3 frame after a break.
const RESYNC_WINDOW: usize = 64 * 1024;

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VerifyReport {
    pub format: String,
    pub bytes_examined: u64,
    /// Bytes inside structures the walker could validate.
    pub valid_bytes: u64,
    pub valid_fraction: f64,
    /// Number of frames, chunks or markers walked.
    pub units: u64,
    /// Offsets where the structure stopped making sense.
    pub breaks: Vec<u64>,
    pub reached_end: bool,
    pub notes: Vec<String>,
}

impl VerifyReport {
    fn finish(mut self, total: usize) -> Self {
        self.bytes_examined = total as u64;
        self.valid_fraction = if total == 0 {
            0.0
        } else {
            self.valid_bytes as f64 / total as f64
        };
        self
    }
}

/// Chooses a walker from the file's signature, falling back to the name.
pub fn verify_stream(name: &str, data: &[u8]) -> VerifyReport {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    // What the bytes say comes first: a name can be wrong, and renamed files are common.
    if data.starts_with(PNG_SIGNATURE) {
        return verify_png(data);
    }
    if data.starts_with(b"\xff\xd8\xff") {
        return verify_jpeg(data);
    }
    if data.starts_with(b"ID3") {
        return verify_mp3(data);
    }
    if data.starts_with(b"PK\x03\x04") {
        return verify_zip(data);
    }
    if data.starts_with(PDF_SIGNATURE) {
        return verify_pdf(data);
    }
    if data.starts_with(OLE_SIGNATURE) {
        return verify_ole(data);
    }
    // Without a signature, the name says what it should have been. A .doc has no entry
    // here, because several older formats use that name and have no signature to check.
    match extension.as_str() {
        "png" => return verify_png(data),
        "jpg" | "jpeg" => return verify_jpeg(data),
        "mp3" => return verify_mp3(data),
        "pdf" => return verify_pdf(data),
        name if ZIP_EXTENSIONS.contains(&name) => return verify_zip(data),
        name if TEXT_EXTENSIONS.contains(&name) => return verify_text(data),
        _ => {}
    }
    VerifyReport {
        format: "unknown".into(),
        notes: vec![
            "No structural walker for this type. Use entropy and class profiles instead.".into(),
        ],
        ..VerifyReport::default()
    }
    .finish(data.len())
}

fn mpeg_frame_length(header: &[u8]) -> Option<usize> {
    if header.len() < MPEG_HEADER_BYTES || header[0] != 0xFF || header[1] & 0xE0 != 0xE0 {
        return None;
    }
    let version = (header[1] >> 3) & 0x3;
    let layer = (header[1] >> 1) & 0x3;
    let bitrate_index = usize::from(header[2] >> 4);
    let rate_index = usize::from((header[2] >> 2) & 0x3);
    let padding = usize::from((header[2] >> 1) & 0x1);
    // Only MPEG audio layer III is expected in .mp3 files.
    if version == 1 || layer != 1 || bitrate_index == 0 || bitrate_index == 15 || rate_index == 3 {
        return None;
    }
    const BITRATES_V1: [usize; 16] = [
        0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0,
    ];
    const BITRATES_V2: [usize; 16] = [
        0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0,
    ];
    const RATES_V1: [usize; 3] = [44100, 48000, 32000];
    let is_v1 = version == 3;
    let bitrate = if is_v1 {
        BITRATES_V1[bitrate_index]
    } else {
        BITRATES_V2[bitrate_index]
    } * 1000;
    let divisor = if version == 0 {
        4
    } else if version == 2 {
        2
    } else {
        1
    };
    let rate = RATES_V1[rate_index] / divisor;
    let coefficient = if is_v1 { 144 } else { 72 };
    Some(coefficient * bitrate / rate + padding)
}

/// Walks MPEG frames, resynchronising after a break and recording its offset.
pub fn verify_mp3(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "mp3".into(),
        ..VerifyReport::default()
    };
    let mut position = 0;
    if data.starts_with(b"ID3") && data.len() >= ID3_HEADER_BYTES {
        let size = data[6..10].iter().fold(0usize, |total, &byte| {
            (total << 7) | usize::from(byte & 0x7F)
        });
        position = ID3_HEADER_BYTES + size;
        report.valid_bytes += position.min(data.len()) as u64;
        report.notes.push(format!("ID3v2 tag of {size} bytes."));
    }
    while position + MPEG_HEADER_BYTES <= data.len() {
        // A frame counts only if another valid header follows it, or the data ends.
        let frame = mpeg_frame_length(&data[position..]).filter(|&length| {
            let next = position + length;
            next + MPEG_HEADER_BYTES > data.len() || mpeg_frame_length(&data[next..]).is_some()
        });
        if let Some(length) = frame {
            report.units += 1;
            report.valid_bytes += length.min(data.len() - position) as u64;
            position += length;
            continue;
        }
        if data[position..].starts_with(b"TAG") && data.len() - position <= 128 {
            report.valid_bytes += (data.len() - position) as u64;
            report.notes.push("ID3v1 tag at the end.".into());
            position = data.len();
            break;
        }
        report.breaks.push(position as u64);
        let window_end = data.len().min(position + RESYNC_WINDOW);
        let resync = (position + 1..window_end).find(|&candidate| {
            mpeg_frame_length(&data[candidate..]).is_some_and(|length| {
                let next = candidate + length;
                next + MPEG_HEADER_BYTES <= data.len() && mpeg_frame_length(&data[next..]).is_some()
            })
        });
        match resync {
            Some(next) => position = next,
            None => {
                position = window_end;
                if window_end == data.len() {
                    break;
                }
            }
        }
    }
    report.reached_end = position >= data.len() || data.len() - position < MPEG_HEADER_BYTES;
    report.finish(data.len())
}

fn crc32(chunks: &[&[u8]]) -> u32 {
    let mut crc = !0u32;
    for chunk in chunks {
        for &byte in *chunk {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 {
                    (crc >> 1) ^ 0xEDB8_8320
                } else {
                    crc >> 1
                };
            }
        }
    }
    !crc
}

/// Walks PNG chunks and checks each chunk's CRC.
pub fn verify_png(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "png".into(),
        ..VerifyReport::default()
    };
    if !data.starts_with(PNG_SIGNATURE) {
        report.breaks.push(0);
        report.notes.push("PNG signature is missing.".into());
        return report.finish(data.len());
    }
    report.valid_bytes = PNG_SIGNATURE.len() as u64;
    let mut position = PNG_SIGNATURE.len();
    while position + 12 <= data.len() {
        let length =
            u32::from_be_bytes(data[position..position + 4].try_into().expect("four bytes"))
                as usize;
        let kind = &data[position + 4..position + 8];
        let end = position + 12 + length;
        if end > data.len() {
            report.breaks.push(position as u64);
            report.notes.push(format!(
                "Chunk at {position} claims {length} bytes, past the end of the data."
            ));
            return report.finish(data.len());
        }
        let stored = u32::from_be_bytes(data[end - 4..end].try_into().expect("four bytes"));
        if crc32(&[kind, &data[position + 8..end - 4]]) != stored {
            report.breaks.push(position as u64);
            report
                .notes
                .push(format!("CRC mismatch in chunk at {position}."));
            return report.finish(data.len());
        }
        report.units += 1;
        report.valid_bytes += (end - position) as u64;
        position = end;
        if kind == b"IEND" {
            report.reached_end = true;
            break;
        }
    }
    report.finish(data.len())
}

/// Walks JPEG markers to the scan data, then checks that the entropy-coded
/// segment holds only legal marker bytes up to the end-of-image marker.
pub fn verify_jpeg(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "jpeg".into(),
        ..VerifyReport::default()
    };
    if !data.starts_with(b"\xff\xd8") {
        report.breaks.push(0);
        report.notes.push("JPEG start marker is missing.".into());
        return report.finish(data.len());
    }
    let mut position = 2;
    report.units = 1;
    while position + 2 <= data.len() {
        if data[position] != 0xFF {
            report.breaks.push(position as u64);
            report
                .notes
                .push(format!("Expected a marker at {position}."));
            return report.finish(data.len());
        }
        let marker = data[position + 1];
        if marker == 0xD9 {
            position += 2;
            // Some cameras add a second image, such as a preview, right after the first.
            if data[position..].starts_with(b"\xff\xd8") {
                position += 2;
                continue;
            }
            report.reached_end = true;
            // Padding after the end is harmless, and so does not count against the file.
            let padding = data[position..]
                .iter()
                .all(|byte| matches!(byte, 0x00 | 0xFF));
            if padding {
                position = data.len();
            }
            break;
        }
        if marker == 0xFF {
            position += 1;
            continue;
        }
        if position + 4 > data.len() {
            break;
        }
        let length = usize::from(u16::from_be_bytes([data[position + 2], data[position + 3]]));
        report.units += 1;
        position += 2 + length;
        if marker == 0xDA {
            // Scan data: 0xFF may only precede 0x00, a restart marker or a new marker.
            let mut next_restart = JPEG_FIRST_RESTART;
            let mut zero_run = 0;
            while position + 1 < data.len() {
                let byte = data[position];
                zero_run = if byte == 0 { zero_run + 1 } else { 0 };
                if zero_run >= JPEG_MAX_ZERO_RUN {
                    let start = position + 1 - zero_run;
                    report.breaks.push(start as u64);
                    report.notes.push(format!(
                        "Its image data has a long empty stretch at {start}."
                    ));
                    report.valid_bytes = start as u64;
                    return report.finish(data.len());
                }
                if byte == 0xFF {
                    let next = data[position + 1];
                    let restart = (JPEG_FIRST_RESTART..JPEG_FIRST_RESTART + JPEG_RESTART_COUNT)
                        .contains(&next);
                    if restart && next != next_restart {
                        report.breaks.push(position as u64);
                        report
                            .notes
                            .push(format!("Restart markers go out of order at {position}."));
                        report.valid_bytes = position as u64;
                        return report.finish(data.len());
                    }
                    if restart {
                        next_restart = JPEG_FIRST_RESTART
                            + (next - JPEG_FIRST_RESTART + 1) % JPEG_RESTART_COUNT;
                    } else if next != 0x00 {
                        break;
                    }
                }
                position += 1;
            }
        }
    }
    report.valid_bytes = position.min(data.len()) as u64;
    if !report.reached_end {
        report.notes.push("No end-of-image marker found.".into());
    }
    report.finish(data.len())
}

const ZIP_END_SIGNATURE: &[u8] = b"PK\x05\x06";
const ZIP_CENTRAL_SIGNATURE: &[u8] = b"PK\x01\x02";
const ZIP_LOCAL_SIGNATURE: &[u8] = b"PK\x03\x04";
const ZIP_END_MIN_BYTES: usize = 22;
const ZIP_MAX_COMMENT_BYTES: usize = 65_535;
const ZIP_CENTRAL_HEADER_BYTES: usize = 46;
const ZIP_LOCAL_HEADER_BYTES: usize = 30;
const ZIP_METHOD_STORED: u16 = 0;
const ZIP_METHOD_DEFLATED: u16 = 8;
const MAX_NOTES: usize = 20;

fn find_last(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .rposition(|window| window == needle)
}

/// True if the bytes at this offset begin an object, such as `12 0 obj`.
fn pdf_object_at(data: &[u8], offset: usize) -> bool {
    let Some(rest) = data.get(offset..) else {
        return false;
    };
    let head = &rest[..rest.len().min(32)];
    let text = String::from_utf8_lossy(head);
    let mut parts = text.split_ascii_whitespace();
    let number = parts
        .next()
        .is_some_and(|part| part.bytes().all(|b| b.is_ascii_digit()));
    let generation = parts
        .next()
        .is_some_and(|part| part.bytes().all(|b| b.is_ascii_digit()));
    number && generation && parts.next().is_some_and(|part| part.starts_with("obj"))
}

/// A PDF's cross-reference table says where every object is, all through the file, so a
/// wrong cluster shows as objects that are not where the table says. Newer PDFs compress
/// that table, so for those only the two ends can be checked, and the report says so.
pub fn verify_pdf(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "pdf".into(),
        ..VerifyReport::default()
    };
    if !data.starts_with(PDF_SIGNATURE) {
        report.breaks.push(0);
        report
            .notes
            .push("It does not start with a PDF header.".into());
        return report.finish(data.len());
    }
    let tail_start = data.len().saturating_sub(PDF_TAIL_BYTES);
    let tail = &data[tail_start..];
    let pointer = find_last(tail, b"startxref").and_then(|at| {
        String::from_utf8_lossy(&tail[at + b"startxref".len()..])
            .split_ascii_whitespace()
            .next()
            .and_then(|text| text.parse::<usize>().ok())
    });
    report.reached_end = find_last(tail, b"%%EOF").is_some();
    let Some(start) = pointer.filter(|start| *start < data.len()) else {
        report.breaks.push(data.len() as u64);
        report
            .notes
            .push("Its pointer to the cross-reference table is missing.".into());
        return report.finish(data.len());
    };
    if pdf_object_at(data, start) {
        report.units = 1;
        report.valid_bytes = if report.reached_end {
            data.len() as u64
        } else {
            0
        };
        report.notes.push(
            "Its cross-reference table is compressed, so only the two ends were checked.".into(),
        );
        return report.finish(data.len());
    }
    if !data[start..].starts_with(b"xref") {
        report.breaks.push(start as u64);
        report
            .notes
            .push("Its cross-reference table is not where the file says it is.".into());
        return report.finish(data.len());
    }
    let mut position = start + b"xref".len();
    let (mut total, mut good) = (0u64, 0u64);
    loop {
        let rest = &data[position..data.len().min(position + 64)];
        let text = String::from_utf8_lossy(rest);
        let line = text
            .lines()
            .find(|line| !line.trim().is_empty())
            .unwrap_or("")
            .trim()
            .to_string();
        let mut parts = line.split_ascii_whitespace();
        let (Some(_first), Some(count), None) = (
            parts.next().and_then(|part| part.parse::<u64>().ok()),
            parts.next().and_then(|part| part.parse::<usize>().ok()),
            parts.next(),
        ) else {
            break;
        };
        let header_at = position + text.find(&line).unwrap_or(0) + line.len();
        let entries_at = header_at
            + data[header_at..]
                .iter()
                .take_while(|byte| byte.is_ascii_whitespace())
                .count();
        for index in 0..count {
            let at = entries_at + index * PDF_XREF_LINE_BYTES;
            let Some(entry) = data.get(at..at + PDF_XREF_LINE_BYTES) else {
                break;
            };
            let entry = String::from_utf8_lossy(entry);
            if !entry.trim_end().ends_with('n') {
                continue;
            }
            total += 1;
            let offset = entry
                .split_ascii_whitespace()
                .next()
                .and_then(|part| part.parse::<usize>().ok());
            // Some writers mark unused objects this way. Nothing can live at offset zero.
            if offset == Some(0) {
                total -= 1;
                continue;
            }
            if offset.is_some_and(|offset| pdf_object_at(data, offset)) {
                good += 1;
            } else if report.breaks.is_empty() {
                report.breaks.push(offset.unwrap_or(at) as u64);
            }
        }
        position = entries_at + count * PDF_XREF_LINE_BYTES;
        if position >= data.len() {
            break;
        }
    }
    report.units = total;
    if total == 0 {
        // A table this walker cannot read says nothing about the file, either way.
        report.units = 1;
        report.valid_bytes = if report.reached_end {
            data.len() as u64
        } else {
            0
        };
        report.notes.push(
            "Its cross-reference table could not be read, so only the two ends were checked."
                .into(),
        );
        return report.finish(data.len());
    }
    report.valid_bytes = (good as f64 / total as f64 * data.len() as f64) as u64;
    report.finish(data.len())
}

/// Older Word, Excel and PowerPoint files hold their own allocation table. Wrong clusters
/// in that table show as entries pointing outside the file.
pub fn verify_ole(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "ole".into(),
        ..VerifyReport::default()
    };
    if !data.starts_with(OLE_SIGNATURE) || data.len() < OLE_HEADER_BYTES {
        report.breaks.push(0);
        report
            .notes
            .push("It does not start with an Office document header.".into());
        return report.finish(data.len());
    }
    let shift = le16(data, 30);
    if shift != 9 && shift != 12 {
        report.breaks.push(30);
        report.notes.push("Its sector size is not valid.".into());
        return report.finish(data.len());
    }
    let sector = 1usize << shift;
    let sectors = (data.len() / sector) as u32 + 1;
    let (mut total, mut good) = (0u64, 0u64);
    for slot in 0..109 {
        let at = OLE_DIFAT_START + slot * 4;
        let fat_sector = le32(data, at);
        if fat_sector >= OLE_FREE_ENTRIES_FROM {
            continue;
        }
        let start = (fat_sector as usize + 1) * sector;
        let Some(table) = data.get(start..start + sector) else {
            report.breaks.push(at as u64);
            report
                .notes
                .push("Its allocation table lies outside the file.".into());
            return report.finish(data.len());
        };
        for entry in table.chunks_exact(4) {
            let value = u32::from_le_bytes([entry[0], entry[1], entry[2], entry[3]]);
            total += 1;
            if value >= OLE_FREE_ENTRIES_FROM || value < sectors {
                good += 1;
            } else if report.breaks.is_empty() {
                report.breaks.push(start as u64);
            }
        }
    }
    report.units = total;
    report.reached_end = total > 0;
    let share = if total == 0 {
        0.0
    } else {
        good as f64 / total as f64
    };
    report.valid_bytes = (share * data.len() as f64) as u64;
    report
        .notes
        .push("Only its allocation table was checked, not the document inside.".into());
    report.finish(data.len())
}

/// A text file should read as text all the way through. A wrong cluster is usually
/// binary, or empty, and shows as a stretch that is not.
pub fn verify_text(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "text".into(),
        ..VerifyReport::default()
    };
    let wide = data.starts_with(b"\xff\xfe") || data.starts_with(b"\xfe\xff");
    for (index, window) in data.chunks(TEXT_WINDOW_BYTES).enumerate() {
        report.units += 1;
        let printable = window
            .iter()
            .filter(|&&byte| (wide && byte == 0) || byte >= 32 || matches!(byte, 9 | 10 | 12 | 13))
            .count();
        if printable as f64 / window.len() as f64 >= MIN_PRINTABLE_SHARE {
            report.valid_bytes += window.len() as u64;
        } else if report.breaks.is_empty() {
            report.breaks.push((index * TEXT_WINDOW_BYTES) as u64);
        }
    }
    report.reached_end = true;
    report.finish(data.len())
}

fn le16(data: &[u8], offset: usize) -> usize {
    usize::from(u16::from_le_bytes([data[offset], data[offset + 1]]))
}

fn le32(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(data[offset..offset + 4].try_into().expect("four bytes"))
}

/// Checks every member of a zip archive against its stored CRC. This covers each
/// byte of the member's contents, so unlike a structure walk it also sees small
/// corruption inside a file. Reassemblies can be judged by it with real confidence.
pub fn verify_zip(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "zip".into(),
        ..VerifyReport::default()
    };
    let search_from = data
        .len()
        .saturating_sub(ZIP_MAX_COMMENT_BYTES + ZIP_END_MIN_BYTES);
    let Some(end) = data[search_from..]
        .windows(4)
        .rposition(|window| window == ZIP_END_SIGNATURE)
        .map(|position| position + search_from)
        .filter(|&position| position + ZIP_END_MIN_BYTES <= data.len())
    else {
        report.breaks.push(data.len() as u64);
        report.notes.push(
            "The zip's central directory was not found, so its end is missing or damaged.".into(),
        );
        return report.finish(data.len());
    };
    report.reached_end = true;
    let members = le16(data, end + 10);
    let mut position = le32(data, end + 16) as usize;
    let mut failed = 0u64;
    let mut valid = 0u64;
    let mut compressed_total = 0u64;
    for _ in 0..members {
        if data.get(position..position + 4) != Some(ZIP_CENTRAL_SIGNATURE)
            || position + ZIP_CENTRAL_HEADER_BYTES > data.len()
        {
            report.breaks.push(position as u64);
            report
                .notes
                .push(format!("The central directory breaks at byte {position}."));
            report.reached_end = false;
            break;
        }
        let method = le16(data, position + 10) as u16;
        let crc = le32(data, position + 16);
        let compressed = le32(data, position + 20) as usize;
        let name_length = le16(data, position + 28);
        let extra_length = le16(data, position + 30);
        let comment_length = le16(data, position + 32);
        let local = le32(data, position + 42) as usize;
        let name_end = (position + ZIP_CENTRAL_HEADER_BYTES + name_length).min(data.len());
        let name = String::from_utf8_lossy(&data[position + ZIP_CENTRAL_HEADER_BYTES..name_end])
            .into_owned();
        position += ZIP_CENTRAL_HEADER_BYTES + name_length + extra_length + comment_length;
        report.units += 1;
        compressed_total += compressed as u64;

        let body = match data.get(local..local + ZIP_LOCAL_HEADER_BYTES) {
            Some(header) if header.starts_with(ZIP_LOCAL_SIGNATURE) => {
                let start = local + ZIP_LOCAL_HEADER_BYTES + le16(header, 26) + le16(header, 28);
                data.get(start..start + compressed)
            }
            _ => None,
        };
        let member_ok = body.is_some_and(|bytes| match method {
            ZIP_METHOD_STORED => crc32(&[bytes]) == crc,
            ZIP_METHOD_DEFLATED => {
                let mut inflated = Vec::new();
                DeflateDecoder::new(bytes)
                    .read_to_end(&mut inflated)
                    .is_ok()
                    && crc32(&[&inflated]) == crc
            }
            _ => false,
        });
        if member_ok {
            valid += compressed as u64;
        } else {
            failed += 1;
            report.breaks.push(local as u64);
            if report.notes.len() < MAX_NOTES {
                let reason = if method != ZIP_METHOD_STORED && method != ZIP_METHOD_DEFLATED {
                    format!("uses compression method {method}, which this check cannot read")
                } else {
                    "fails its CRC".to_string()
                };
                report.notes.push(format!("{name} {reason}."));
            }
        }
    }
    report.notes.insert(
        0,
        format!(
            "{} of {members} members pass their CRC.",
            members as u64 - failed
        ),
    );
    let mut finished = report.finish(data.len());
    finished.valid_bytes = valid;
    finished.valid_fraction = if compressed_total == 0 {
        1.0
    } else {
        valid as f64 / compressed_total as f64
    };
    finished
}
