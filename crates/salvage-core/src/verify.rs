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
/// Set in an ID3v2.4 header when a 10-byte footer follows the tag.
const ID3_FOOTER_FLAG: u8 = 0x10;
const ID3V1_TAG_BYTES: usize = 128;
/// Zeros or 0xFF bytes after a tag or at the end that players skip, at most.
const MAX_MP3_PADDING: usize = 64 * 1024;
const MPEG_HEADER_BYTES: usize = 4;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";
/// Unwritten clusters read as zeros, which compressed image data never is for this long.
const JPEG_MAX_ZERO_RUN: usize = 4096;
const JPEG_FIRST_RESTART: u8 = 0xD0;
const JPEG_RESTART_COUNT: u8 = 8;
/// Every marker a JPEG uses is this or above, so a lower byte after 0xFF is not a marker.
const JPEG_LOWEST_MARKER: u8 = 0xC0;
const PDF_SIGNATURE: &[u8] = b"%PDF-";
const OLE_SIGNATURE: &[u8] = b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1";
/// Office and similar documents that are zip files inside. A Keynote .key is one too, but
/// .key is also the usual name for a key in text form, so only its signature can route it.
const ZIP_EXTENSIONS: &[&str] = &["zip", "docx", "xlsx", "pptx", "odt", "pages", "numbers"];
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
/// MP4, QuickTime and their relatives: a file of boxes, ISO/IEC 14496-12.
const BMFF_EXTENSIONS: &[&str] = &["mp4", "m4v", "m4a", "mov", "3gp", "3g2"];
const BMFF_HEADER_BYTES: usize = 8;
const BMFF_LARGE_HEADER_BYTES: usize = 16;
/// Clusters macOS never wrote read back as zeros. Compressed video or audio does not hold this
/// many zero bytes in a row, so a run this long inside the media data is a hole.
const MEDIA_MAX_ZERO_RUN: usize = 32 * 1024;
const FLAC_SIGNATURE: &[u8] = b"fLaC";
const FLAC_BLOCK_HEADER_BYTES: usize = 4;
const FLAC_STREAMINFO_BYTES: usize = 34;
const FLAC_LAST_BLOCK: u8 = 0x80;
const FLAC_INVALID_BLOCK_TYPE: u8 = 127;
/// The smallest a FLAC frame can be: a header, one subframe byte and the CRC-16.
const FLAC_MIN_FRAME_BYTES: usize = 10;
/// Frames are rarely over a megabyte, so a frame not closed by then is broken.
const FLAC_MAX_FRAME_BYTES: usize = 1024 * 1024;

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
    if data.get(4..8) == Some(b"ftyp".as_slice()) {
        return verify_bmff(data);
    }
    if data.starts_with(b"RIFF") && data.get(8..12) == Some(b"AVI ".as_slice()) {
        return verify_avi(data);
    }
    if data.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
        return verify_matroska(data);
    }
    if data.starts_with(&[0, 0, 1, 0xBA]) {
        return walk_whole("film.mpg", data);
    }
    if data.starts_with(b"FLV\x01") {
        return walk_whole("film.flv", data);
    }
    if data.starts_with(&[0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11]) {
        return walk_whole("film.wmv", data);
    }
    if data.starts_with(FLAC_SIGNATURE) {
        return verify_flac(data);
    }
    // Without a signature, the name says what it should have been. A .doc has no entry
    // here, because several older formats use that name and have no signature to check.
    match extension.as_str() {
        "png" => return verify_png(data),
        "jpg" | "jpeg" => return verify_jpeg(data),
        "mp3" => return verify_mp3(data),
        "pdf" => return verify_pdf(data),
        name if BMFF_EXTENSIONS.contains(&name) => return verify_bmff(data),
        "avi" => return verify_avi(data),
        "mkv" | "webm" | "mka" | "mk3d" => return verify_matroska(data),
        "mpg" | "mpeg" | "vob" | "m2p" | "mpe" | "flv" | "wmv" | "asf" | "wma" => {
            return walk_whole(&format!("film.{extension}"), data)
        }
        "flac" => return verify_flac(data),
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
        // ID3v2.4 can add a 10-byte footer after the tag.
        let footer = if data[3] == 4 && data[5] & ID3_FOOTER_FLAG != 0 {
            ID3_HEADER_BYTES
        } else {
            0
        };
        position = ID3_HEADER_BYTES + size + footer;
        report.notes.push(format!("ID3v2 tag of {size} bytes."));
        // Writers often pad after the tag with zeros its size does not count.
        let padding = data.get(position..).map_or(0, |rest| {
            rest.iter()
                .take(MAX_MP3_PADDING)
                .take_while(|&&byte| byte == 0)
                .count()
        });
        position += padding;
        report.valid_bytes += position.min(data.len()) as u64;
    }
    while position + MPEG_HEADER_BYTES <= data.len() {
        // A frame counts only if another valid header follows it, a tag or padding that
        // players skip ends the file after it, or the data ends.
        let frame = mpeg_frame_length(&data[position..]).filter(|&length| {
            let next = position + length;
            next + MPEG_HEADER_BYTES > data.len()
                || mpeg_frame_length(&data[next..]).is_some()
                || mp3_trailer(&data[next..]).is_some()
        });
        if let Some(length) = frame {
            report.units += 1;
            report.valid_bytes += length.min(data.len() - position) as u64;
            position += length;
            continue;
        }
        if let Some(kind) = mp3_trailer(&data[position..]) {
            report.valid_bytes += (data.len() - position) as u64;
            report.notes.push(format!("{kind} at the end."));
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

/// What ends an MP3 after its last frame and players skip: tags, or padding. None when the
/// rest of the data is something else.
fn mp3_trailer(rest: &[u8]) -> Option<&'static str> {
    if rest.is_empty() {
        return None;
    }
    if rest.starts_with(b"TAG") && rest.len() <= ID3V1_TAG_BYTES {
        Some("An ID3v1 tag")
    } else if rest.starts_with(b"APETAGEX") {
        Some("An APE tag")
    } else if rest.starts_with(b"LYRICSBEGIN") {
        Some("A Lyrics3 tag")
    } else if rest.starts_with(b"ID3") {
        Some("An ID3v2 tag")
    } else if rest.len() <= MAX_MP3_PADDING && rest.iter().all(|&byte| byte == 0 || byte == 0xFF) {
        Some("Padding")
    } else {
        None
    }
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
        let marker = data[position + 1];
        if data[position] != 0xFF || (marker != 0xFF && marker < JPEG_LOWEST_MARKER) {
            report.breaks.push(position as u64);
            report
                .notes
                .push(format!("Expected a marker at {position}."));
            report.valid_bytes = position as u64;
            return report.finish(data.len());
        }
        if marker == 0xD9 {
            position += 2;
            // Some cameras add a second image, such as a preview, right after the first.
            if data[position..].starts_with(b"\xff\xd8") {
                position += 2;
                continue;
            }
            report.reached_end = true;
            // The picture is complete here. Cameras and phones often add more after it,
            // such as a preview or a motion clip, and none of that can harm the picture.
            let extra = &data[position..];
            if !extra.iter().all(|byte| matches!(byte, 0x00 | 0xFF)) {
                report.notes.push(format!(
                    "{} bytes follow the end of the image.",
                    extra.len()
                ));
            }
            position = data.len();
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

/// Whether four bytes could name a box or chunk: printable ASCII, as the formats require.
fn is_four_cc(bytes: &[u8]) -> bool {
    bytes.len() == 4 && bytes.iter().all(|byte| (0x20..=0x7e).contains(byte))
}

/// The first run of at least `MEDIA_MAX_ZERO_RUN` zero bytes, as an offset into `data`.
fn long_zero_run(data: &[u8]) -> Option<usize> {
    let mut run = 0usize;
    for (index, &byte) in data.iter().enumerate() {
        if byte == 0 {
            run += 1;
            if run == MEDIA_MAX_ZERO_RUN {
                return Some(index + 1 - run);
            }
        } else {
            run = 0;
        }
    }
    None
}

/// Walks the top-level boxes of an MP4 or QuickTime file (ISO/IEC 14496-12, 4.2): each is a
/// 32-bit size and a four-character type, with size 1 meaning a 64-bit size follows and 0
/// meaning the box runs to the end. A file without a `moov` box will not play. Long runs of
/// zeros inside `mdat` are unwritten clusters.
pub fn verify_bmff(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "mp4".into(),
        ..VerifyReport::default()
    };
    let mut position = 0usize;
    let mut has_movie = false;
    while position + BMFF_HEADER_BYTES <= data.len() {
        let size =
            u32::from_be_bytes(data[position..position + 4].try_into().expect("four bytes")) as u64;
        let kind = &data[position + 4..position + 8];
        if !is_four_cc(kind) {
            report.breaks.push(position as u64);
            report.notes.push(format!(
                "No box starts at {position}: its type is not four letters."
            ));
            return report.finish(data.len());
        }
        let (header, length) = match size {
            0 => (BMFF_HEADER_BYTES, (data.len() - position) as u64),
            1 => {
                let Some(large) = data.get(position + 8..position + BMFF_LARGE_HEADER_BYTES) else {
                    report.breaks.push(position as u64);
                    report
                        .notes
                        .push(format!("The box at {position} is cut off in its header."));
                    return report.finish(data.len());
                };
                (
                    BMFF_LARGE_HEADER_BYTES,
                    u64::from_be_bytes(large.try_into().expect("eight bytes")),
                )
            }
            size => (BMFF_HEADER_BYTES, size),
        };
        if length < header as u64 {
            report.breaks.push(position as u64);
            report.notes.push(format!(
                "The box at {position} is smaller than its own header."
            ));
            return report.finish(data.len());
        }
        let end = position as u64 + length;
        if end > data.len() as u64 {
            report.breaks.push(position as u64);
            report.notes.push(format!(
                "The {} box at {position} claims {length} bytes, past the end of the data.",
                String::from_utf8_lossy(kind)
            ));
            return report.finish(data.len());
        }
        let end = end as usize;
        if kind == b"moov" {
            has_movie = true;
        }
        if kind == b"mdat" {
            if let Some(hole) = long_zero_run(&data[position + header..end]) {
                let at = position + header + hole;
                report.valid_bytes += (at - position) as u64;
                report.breaks.push(at as u64);
                report.notes.push(format!(
                    "The media data turns to zeros at {at}, as unwritten clusters do."
                ));
                return report.finish(data.len());
            }
        }
        report.units += 1;
        report.valid_bytes += length;
        position = end;
    }
    if position == data.len() {
        report.reached_end = true;
    } else {
        report.breaks.push(position as u64);
        report.notes.push(format!(
            "{} stray bytes follow the last box.",
            data.len() - position
        ));
    }
    if !has_movie {
        report.notes.push(
            "There is no moov box, which says how to play the media, so it will not play as it is."
                .into(),
        );
    }
    report.finish(data.len())
}

/// Walks an AVI file (a RIFF form of type "AVI ", then "AVIX" forms past a gigabyte) chunk by
/// chunk, down to each frame inside its lists, through the same walker large files stream through.
pub fn verify_avi(data: &[u8]) -> VerifyReport {
    walk_whole("film.avi", data)
}

/// Walks a Matroska file (MKV, WebM) element by element, down to each block in its clusters.
pub fn verify_matroska(data: &[u8]) -> VerifyReport {
    walk_whole("film.mkv", data)
}

fn walk_whole(name: &str, data: &[u8]) -> VerifyReport {
    let mut walk = crate::stream_walk::StreamWalk::new(name, data.len() as u64)
        .expect("a type the stream walker knows");
    walk.feed(data);
    walk.finish()
}

fn flac_crc8(data: &[u8]) -> u8 {
    let mut crc = 0u8;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

fn flac_crc16(data: &[u8]) -> u16 {
    let mut crc = 0u16;
    for &byte in data {
        crc ^= u16::from(byte) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x8005
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Whether a FLAC frame header starts at `at`: the 14-bit sync code 0b11111111111110, then
/// fields whose last byte is a CRC-8 of the header before it. The header's length depends on
/// its fields, so each possible length is tried against the CRC.
fn flac_frame_header_at(data: &[u8], at: usize) -> bool {
    let Some(head) = data.get(at..at + 4) else {
        return false;
    };
    if head[0] != 0xFF || head[1] & 0xFE != 0xF8 {
        return false;
    }
    // A header is 6 to 16 bytes; only a correct CRC-8 makes one real.
    (6..=16).any(|length| {
        data.get(at..at + length)
            .is_some_and(|header| flac_crc8(&header[..length - 1]) == header[length - 1])
    })
}

/// Walks a FLAC file: the "fLaC" marker, metadata blocks starting with STREAMINFO, then audio
/// frames. A frame runs from one frame header to the next and ends with a CRC-16 of itself, so
/// each frame is proved whole or not.
pub fn verify_flac(data: &[u8]) -> VerifyReport {
    let mut report = VerifyReport {
        format: "flac".into(),
        ..VerifyReport::default()
    };
    if !data.starts_with(FLAC_SIGNATURE) {
        report.breaks.push(0);
        report.notes.push("The fLaC marker is missing.".into());
        return report.finish(data.len());
    }
    let mut position = FLAC_SIGNATURE.len();
    let mut first = true;
    loop {
        let Some(header) = data.get(position..position + FLAC_BLOCK_HEADER_BYTES) else {
            report.breaks.push(position as u64);
            report
                .notes
                .push("The metadata ends before its last block.".into());
            return report.finish(data.len());
        };
        let kind = header[0] & !FLAC_LAST_BLOCK;
        let length =
            (usize::from(header[1]) << 16) | (usize::from(header[2]) << 8) | usize::from(header[3]);
        if kind == FLAC_INVALID_BLOCK_TYPE
            || (first && (kind != 0 || length != FLAC_STREAMINFO_BYTES))
        {
            report.breaks.push(position as u64);
            report
                .notes
                .push(format!("The metadata block at {position} is not valid."));
            return report.finish(data.len());
        }
        let end = position + FLAC_BLOCK_HEADER_BYTES + length;
        if end > data.len() {
            report.breaks.push(position as u64);
            report.notes.push(format!(
                "The metadata block at {position} runs past the end of the data."
            ));
            return report.finish(data.len());
        }
        report.units += 1;
        position = end;
        first = false;
        if header[0] & FLAC_LAST_BLOCK != 0 {
            break;
        }
    }
    report.valid_bytes = position as u64;
    if !flac_frame_header_at(data, position) {
        report.breaks.push(position as u64);
        report.notes.push(format!(
            "No audio frame starts at {position}, after the metadata."
        ));
        return report.finish(data.len());
    }
    while position < data.len() {
        // The frame ends where the next real header starts, or at the end of the data, and its
        // last two bytes are a CRC-16 of the rest.
        let mut next = position + FLAC_MIN_FRAME_BYTES;
        let limit = (position + FLAC_MAX_FRAME_BYTES).min(data.len());
        let mut closed = None;
        while next <= limit {
            if next == data.len() || flac_frame_header_at(data, next) {
                let stored = u16::from_be_bytes([data[next - 2], data[next - 1]]);
                if flac_crc16(&data[position..next - 2]) == stored {
                    closed = Some(next);
                    break;
                }
            }
            next += 1;
        }
        match closed {
            Some(end) => {
                report.units += 1;
                report.valid_bytes += (end - position) as u64;
                position = end;
            }
            None => {
                report.breaks.push(position as u64);
                report
                    .notes
                    .push(format!("The audio frame at {position} fails its checksum."));
                return report.finish(data.len());
            }
        }
    }
    report.reached_end = true;
    report.finish(data.len())
}

#[cfg(test)]
mod walker_tests {
    use super::*;

    /// MPEG-1 Layer III frames at 128 kbit/s and 44.1 kHz, each 417 bytes.
    fn mp3_frames(count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|n| {
                let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
                frame.extend((0..413).map(|k| ((k + n * 7) % 251 + 1) as u8));
                frame
            })
            .collect()
    }

    fn id3v2(version: u8, flags: u8, size: usize) -> Vec<u8> {
        let mut tag = vec![b'I', b'D', b'3', version, 0, flags];
        tag.extend(
            (0..4)
                .rev()
                .map(|shift| ((size >> (7 * shift)) & 0x7F) as u8),
        );
        tag.extend(vec![b'x'; size]);
        tag
    }

    #[test]
    fn mp3_tags_and_padding_that_players_skip_are_not_breaks() {
        let frames = mp3_frames(40);
        let whole = |parts: &[&[u8]]| verify_mp3(&parts.concat());
        let clean = |report: VerifyReport| {
            assert!(
                report.breaks.is_empty() && report.reached_end,
                "{:?}",
                report.notes
            );
            assert!(report.valid_fraction > 0.999, "{}", report.valid_fraction);
        };
        let mut id3v1 = b"TAG".to_vec();
        id3v1.resize(128, b' ');
        clean(whole(&[&id3v2(3, 0, 300), &frames, &id3v1]));
        clean(whole(&[&id3v2(3, 0, 300), &[0u8; 2000], &frames]));
        let mut footer = id3v2(4, 0x10, 300);
        footer.extend_from_slice(b"3DI\x04\x00\x10\x00\x00\x02\x2c");
        clean(whole(&[&footer, &frames]));
        clean(whole(&[
            &frames,
            b"APETAGEX\xd0\x07\x00\x00",
            &[1u8; 60],
            &id3v1,
        ]));
        clean(whole(&[&frames, &[0u8; 900]]));

        // Something else after the last frame, or inside the stream, is still a break.
        let tail = whole(&[&frames, &[0x5Au8; 600]]);
        assert!(!tail.breaks.is_empty());
        let mut holed = frames.clone();
        holed[417 * 20..417 * 20 + 300].fill(0x33);
        assert_eq!(verify_mp3(&holed).breaks.len(), 1);
    }

    fn bmff_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn an_mp4_is_walked_box_by_box() {
        let mut file = bmff_box(b"ftyp", b"isom\0\0\x02\0isomiso2");
        file.extend(bmff_box(b"moov", &[1u8; 40]));
        file.extend(bmff_box(b"mdat", &[7u8; 5000]));
        let report = verify_stream("film.mp4", &file);
        assert_eq!(
            (report.format.as_str(), report.units, report.reached_end),
            ("mp4", 3, true)
        );
        assert!(report.breaks.is_empty() && report.valid_fraction == 1.0);

        let mut cut = file.clone();
        cut.truncate(file.len() - 100);
        let report = verify_bmff(&cut);
        assert_eq!(
            report.breaks,
            vec![(file.len() - 5008) as u64],
            "the mdat box is cut short"
        );

        let mut holed = bmff_box(b"ftyp", b"isom\0\0\x02\0");
        holed.extend(bmff_box(b"moov", &[1u8; 40]));
        let mut media = vec![9u8; 1000];
        media.extend(vec![0u8; MEDIA_MAX_ZERO_RUN + 10]);
        holed.extend(bmff_box(b"mdat", &media));
        let report = verify_bmff(&holed);
        assert_eq!(report.breaks.len(), 1);
        assert!(report.notes[0].contains("zeros"), "{:?}", report.notes);

        let no_movie = [bmff_box(b"ftyp", b"isom"), bmff_box(b"mdat", &[3u8; 64])].concat();
        assert!(verify_bmff(&no_movie)
            .notes
            .iter()
            .any(|note| note.contains("moov")));
    }

    fn riff_chunk(id: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = id.to_vec();
        out.extend_from_slice(&(body.len() as u32).to_le_bytes());
        out.extend_from_slice(body);
        if body.len() % 2 == 1 {
            out.push(0);
        }
        out
    }

    #[test]
    fn an_avi_is_walked_chunk_by_chunk() {
        let mut body = b"AVI ".to_vec();
        body.extend(riff_chunk(
            b"LIST",
            &[b"hdrl".as_slice(), &riff_chunk(b"avih", &[7u8; 56])].concat(),
        ));
        let frames = [
            riff_chunk(b"00dc", &[5u8; 3001]),
            riff_chunk(b"01wb", &[6u8; 400]),
        ]
        .concat();
        body.extend(riff_chunk(b"LIST", &[b"movi".as_slice(), &frames].concat()));
        body.extend(riff_chunk(b"idx1", &[1u8; 32]));
        let file = [
            b"RIFF".as_slice(),
            &(body.len() as u32).to_le_bytes(),
            &body,
        ]
        .concat();
        let report = verify_stream("clip.avi", &file);
        assert_eq!(
            (report.format.as_str(), report.units, report.reached_end),
            ("avi", 4, true),
            "{:?}",
            report.notes
        );
        assert!(report.breaks.is_empty());

        // A frame header overwritten inside the movi list is found where it is.
        let second = file.len() - (8 + 32) - (8 + 400);
        let mut overwritten = file.clone();
        overwritten[second..second + 4].copy_from_slice(&[0x91, 0x02, 0xfe, 0x33]);
        assert_eq!(verify_avi(&overwritten).breaks, vec![second as u64]);

        let mut cut = file.clone();
        cut.truncate(file.len() - 10);
        assert!(!verify_avi(&cut).breaks.is_empty());
    }

    /// A FLAC frame: a fixed-blocksize header with its CRC-8, a payload and its CRC-16.
    fn flac_frame(number: u8, payload: &[u8]) -> Vec<u8> {
        let mut frame = vec![0xFF, 0xF8, 0x69, 0x08, number];
        frame.push(flac_crc8(&frame));
        frame.extend_from_slice(payload);
        let crc = flac_crc16(&frame);
        frame.extend_from_slice(&crc.to_be_bytes());
        frame
    }

    #[test]
    fn a_flac_is_walked_frame_by_frame() {
        let mut file = FLAC_SIGNATURE.to_vec();
        file.extend_from_slice(&[0x80, 0, 0, 34]);
        file.extend_from_slice(&[0x12; 34]);
        for number in 0..3 {
            file.extend(flac_frame(number, &[0x5A ^ number; 400]));
        }
        let report = verify_stream("song.flac", &file);
        assert_eq!(
            (report.format.as_str(), report.units, report.reached_end),
            ("flac", 4, true)
        );
        assert!(report.breaks.is_empty(), "{:?}", report.notes);

        let mut damaged = file.clone();
        // Each frame is a 6-byte header, 400 bytes of audio and a 2-byte CRC-16.
        let middle = 4 + 4 + 34 + 408 + 200;
        damaged[middle] ^= 0xFF;
        let report = verify_flac(&damaged);
        assert_eq!(
            report.breaks,
            vec![(4 + 4 + 34 + 408) as u64],
            "{:?}",
            report.notes
        );
    }
}
