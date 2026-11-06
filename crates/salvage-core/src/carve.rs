//! Carving: finding files in space nothing on the disk accounts for.
//!
//! The scan already tags every cluster that starts like a known file: a JPEG, PNG, PDF,
//! video, archive and so on. In space no entry and no FAT chain explains, each such cluster
//! is a file that lost its name and its place. This reads where each one ends, from the
//! file's own structure where the format allows it: a JPEG's end marker, a PNG's chunks, an
//! MP4's boxes, a zip's end record, a PDF's last end-of-file mark, a RIFF or 7z header's size.
//! Where a format says nothing about its length, it ends where the run of similar clusters
//! does.
//!
//! Carving assumes a file's clusters follow each other, as a file written to an empty disk
//! or copied in one go usually does. A file broken into pieces comes out partial, and the
//! confidence says how much the end was measured rather than guessed. Nothing is written.

use crate::content::{
    CLASS_ARCHIVE, CLASS_AUDIO, CLASS_DIRECTORY, CLASS_EXECUTABLE, CLASS_FILL, CLASS_GIF,
    CLASS_JPEG, CLASS_MP3, CLASS_OLE, CLASS_PDF, CLASS_PNG, CLASS_VIDEO, CLASS_ZEROS,
};
use crate::exfat::FIRST_CLUSTER;
use crate::resolve::OWNERSHIP_UNEXPLAINED;
use crate::session::Session;
use anyhow::Result;
use serde::Serialize;

/// The longest a carved file is followed, which covers all but very long videos.
const MAX_CARVE_BYTES: u64 = 4 * 1024 * 1024 * 1024;
/// How far a JPEG or PDF is read through looking for its end.
const MAX_SCAN_BYTES: u64 = 256 * 1024 * 1024;
const SCAN_CHUNK: usize = 1024 * 1024;
/// Each box or chunk walked costs a read, so a walk stops after this many.
const MAX_STEPS: usize = 100_000;

/// The end was read from the file's own structure.
const CONFIDENCE_MEASURED: f64 = 0.8;
/// The file's structure ends past where the space it lies in does, so it is cut short.
const CONFIDENCE_CUT: f64 = 0.3;
/// The format says nothing of its length, so the end is where similar clusters stop.
const CONFIDENCE_GUESSED: f64 = 0.4;

/// One file found in unexplained space.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CarveCandidate {
    pub start_cluster: u32,
    /// What it looks like: jpeg, png, pdf, mp4, zip and so on.
    pub kind: &'static str,
    pub extension: &'static str,
    /// Its length, as measured or guessed.
    pub bytes: u64,
    pub clusters: u64,
    /// How the end was found: "structure", "cut short" or "similar clusters".
    pub end_by: &'static str,
    pub confidence: f64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CarvePage {
    pub candidates: Vec<CarveCandidate>,
    /// Every file start found in unexplained space, of which this page is a part.
    pub total: u64,
    pub bytes_estimated_on_this_page: u64,
}

/// Where a file's structure says it ends, if it says.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum End {
    /// The file is this many bytes long.
    At(u64),
    /// The structure runs past the bytes there are.
    PastTheEnd,
    /// The format does not say, or the structure could not be followed.
    Unknown,
}

/// Reads bytes of the candidate, from its start. Short or missing past what there is.
pub type ReadAt<'a> = &'a dyn Fn(u64, usize) -> Option<Vec<u8>>;

fn find(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .position(|window| window == needle)
}

fn rfind(data: &[u8], needle: &[u8]) -> Option<usize> {
    data.windows(needle.len())
        .rposition(|window| window == needle)
}

fn u32_be(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_be_bytes(
        data.get(at..at + 4)?.try_into().ok()?,
    )))
}

fn u32_le(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from(u32::from_le_bytes(
        data.get(at..at + 4)?.try_into().ok()?,
    )))
}

fn u64_le(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

fn u64_be(data: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_be_bytes(data.get(at..at + 8)?.try_into().ok()?))
}

/// A JPEG ends at the end-of-image marker after its compressed data. Segments before the
/// data say their own length, so a thumbnail inside them, with its own end marker, is
/// stepped over.
fn jpeg_end(read: ReadAt, limit: u64) -> End {
    let mut at = 2u64;
    // Marker segments, up to the start of the compressed data.
    for _ in 0..MAX_STEPS {
        let Some(head) = read(at, 4) else {
            return End::PastTheEnd;
        };
        if head.len() < 4 || head[0] != 0xFF {
            return End::Unknown;
        }
        let marker = head[1];
        if marker == 0xD9 {
            return End::At(at + 2);
        }
        let length = u64::from(u16::from_be_bytes([head[2], head[3]]));
        at += 2 + length;
        if marker == 0xDA {
            break;
        }
        if at >= limit {
            return End::PastTheEnd;
        }
    }
    // The compressed data: a 0xFF byte inside it is always followed by 0x00 or a restart
    // marker, so the first 0xFF 0xD9 is the end.
    let stop = limit.min(MAX_SCAN_BYTES);
    while at < stop {
        let Some(chunk) = read(at, SCAN_CHUNK) else {
            return End::PastTheEnd;
        };
        if chunk.is_empty() {
            return End::PastTheEnd;
        }
        if let Some(found) = find(&chunk, &[0xFF, 0xD9]) {
            return End::At(at + found as u64 + 2);
        }
        // Keep one byte, in case the marker straddles two chunks.
        at += chunk.len().saturating_sub(1).max(1) as u64;
    }
    End::PastTheEnd
}

/// A PNG is a list of chunks, each with its length, ending with IEND.
fn png_end(read: ReadAt, limit: u64) -> End {
    let mut at = 8u64;
    for _ in 0..MAX_STEPS {
        let Some(head) = read(at, 8) else {
            return End::PastTheEnd;
        };
        let (Some(length), Some(kind)) = (u32_be(&head, 0), head.get(4..8)) else {
            return End::PastTheEnd;
        };
        if !kind.iter().all(u8::is_ascii_alphabetic) {
            return End::Unknown;
        }
        at += 12 + length;
        if kind == b"IEND" {
            return End::At(at);
        }
        if at > limit {
            return End::PastTheEnd;
        }
    }
    End::Unknown
}

/// An MP4 or MOV is a list of boxes, each with its size. The file ends where the boxes stop.
fn boxes_end(read: ReadAt, limit: u64) -> End {
    let mut at = 0u64;
    for step in 0..MAX_STEPS {
        let Some(head) = read(at, 16) else {
            return if step > 1 {
                End::At(at)
            } else {
                End::PastTheEnd
            };
        };
        let (Some(small), Some(kind)) = (u32_be(&head, 0), head.get(4..8)) else {
            return if step > 1 {
                End::At(at)
            } else {
                End::PastTheEnd
            };
        };
        if !kind
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || *byte == b' ')
        {
            return if step > 1 { End::At(at) } else { End::Unknown };
        }
        let size = match small {
            1 => match u64_be(&head, 8) {
                Some(large) => large,
                None => return End::PastTheEnd,
            },
            0 => return End::Unknown,
            size => size,
        };
        if size < 8 {
            return if step > 1 { End::At(at) } else { End::Unknown };
        }
        at += size;
        if at > limit {
            return End::PastTheEnd;
        }
    }
    End::At(at)
}

/// RIFF files (AVI, WAV) give their size after the signature.
fn riff_end(read: ReadAt, limit: u64) -> End {
    match read(0, 8).and_then(|head| u32_le(&head, 4)) {
        Some(size) if size + 8 <= limit => End::At(size + 8),
        Some(_) => End::PastTheEnd,
        None => End::Unknown,
    }
}

/// A zip ends with its end record; a 7z's start header gives where its last header ends.
fn archive_end(read: ReadAt, limit: u64) -> End {
    let Some(head) = read(0, 32) else {
        return End::Unknown;
    };
    if head.starts_with(&[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C]) {
        return match (u64_le(&head, 12), u64_le(&head, 20)) {
            (Some(offset), Some(size)) if 32 + offset + size <= limit => {
                End::At(32 + offset + size)
            }
            (Some(_), Some(_)) => End::PastTheEnd,
            _ => End::Unknown,
        };
    }
    if !head.starts_with(b"PK\x03\x04") {
        return End::Unknown;
    }
    let stop = limit.min(MAX_SCAN_BYTES);
    let mut at = 0u64;
    while at < stop {
        let Some(chunk) = read(at, SCAN_CHUNK) else {
            return End::PastTheEnd;
        };
        if chunk.is_empty() {
            return End::PastTheEnd;
        }
        if let Some(found) = find(&chunk, b"PK\x05\x06") {
            let record = at + found as u64;
            let comment = read(record + 20, 2)
                .filter(|bytes| bytes.len() == 2)
                .map_or(0, |bytes| {
                    u64::from(u16::from_le_bytes([bytes[0], bytes[1]]))
                });
            return End::At(record + 22 + comment);
        }
        at += chunk.len().saturating_sub(3).max(1) as u64;
    }
    End::PastTheEnd
}

/// A PDF ends after its last end-of-file mark: an edited PDF has several, one per update.
fn pdf_end(read: ReadAt, limit: u64) -> End {
    let Some(data) = read(0, limit.min(MAX_SCAN_BYTES) as usize) else {
        return End::Unknown;
    };
    match rfind(&data, b"%%EOF") {
        Some(found) => {
            let mut end = found + 5;
            while matches!(data.get(end), Some(b'\r' | b'\n')) {
                end += 1;
            }
            End::At(end as u64)
        }
        None => End::PastTheEnd,
    }
}

/// Reads where a file ends from its own structure.
type EndFinder = fn(ReadAt, u64) -> End;
/// A kind, its usual extension, and how its end is found, if its format says.
type Kind = (&'static str, &'static str, Option<EndFinder>);

/// What a file that starts like this is, and how its end is found.
fn kind_of(class: u8, head: &[u8]) -> Option<Kind> {
    Some(match class {
        CLASS_JPEG => ("jpeg", "jpg", Some(jpeg_end as EndFinder)),
        CLASS_PNG => ("png", "png", Some(png_end)),
        CLASS_GIF => ("gif", "gif", None),
        CLASS_PDF => ("pdf", "pdf", Some(pdf_end)),
        CLASS_MP3 => ("mp3", "mp3", None),
        CLASS_VIDEO if head.get(4..8) == Some(b"ftyp") => ("mp4", "mp4", Some(boxes_end)),
        CLASS_VIDEO if head.starts_with(b"RIFF") => ("avi", "avi", Some(riff_end)),
        CLASS_VIDEO => ("video", "bin", None),
        CLASS_AUDIO if head.starts_with(b"RIFF") => ("wav", "wav", Some(riff_end)),
        CLASS_AUDIO if head.get(4..8) == Some(b"ftyp") => ("m4a", "m4a", Some(boxes_end)),
        CLASS_AUDIO => ("audio", "bin", None),
        CLASS_ARCHIVE if head.starts_with(b"PK\x03\x04") => ("zip", "zip", Some(archive_end)),
        CLASS_ARCHIVE if head.starts_with(&[b'7', b'z', 0xBC, 0xAF]) => {
            ("7z", "7z", Some(archive_end))
        }
        CLASS_ARCHIVE if head.starts_with(&[0x1F, 0x8B]) => ("gz", "gz", None),
        CLASS_ARCHIVE if head.starts_with(b"Rar!") => ("rar", "rar", None),
        CLASS_ARCHIVE => ("archive", "bin", None),
        CLASS_OLE => ("office", "doc", None),
        CLASS_EXECUTABLE => ("executable", "exe", None),
        _ => return None,
    })
}

/// Classes that cannot be the inside of a file, so a guessed file ends before them.
fn ends_a_run(class: u8) -> bool {
    matches!(class, CLASS_ZEROS | CLASS_FILL | CLASS_DIRECTORY)
}

/// Works out one candidate: its kind from the cluster's class and first bytes, its end from
/// its structure or, failing that, from where similar clusters stop. `room` is how many
/// clusters it can run to before the next file starts or the unexplained space ends, and
/// `guessed` how many of those look like more of the same.
pub fn estimate(
    class: u8,
    read: ReadAt,
    cluster_size: u64,
    room: u64,
    guessed: u64,
) -> Option<(&'static str, &'static str, u64, &'static str, f64)> {
    let head = read(0, 32).unwrap_or_default();
    let (kind, extension, structure) = kind_of(class, &head)?;
    let limit = (room * cluster_size).min(MAX_CARVE_BYTES);
    let guess = guessed.max(1) * cluster_size;
    Some(
        match structure.map_or(End::Unknown, |end| end(read, limit)) {
            End::At(bytes) => (kind, extension, bytes, "structure", CONFIDENCE_MEASURED),
            End::PastTheEnd => (kind, extension, limit, "cut short", CONFIDENCE_CUT),
            End::Unknown => (
                kind,
                extension,
                guess.min(limit),
                "similar clusters",
                CONFIDENCE_GUESSED,
            ),
        },
    )
}

impl Session {
    /// Files found in space nothing explains, in disk order. Finding where each ends reads
    /// the disk, so only the page asked for is worked out.
    pub fn carve_candidates(&self, offset: usize, limit: usize) -> Result<CarvePage> {
        let classes = self.class_map();
        let ownership = self.ownership_map();
        let last = self.volume.boot.last_cluster_exclusive();
        let cluster_size = self.volume.boot.cluster_size();
        let unexplained = |cluster: u32| {
            !self.volume.is_allocated(cluster)
                && ownership[(cluster - FIRST_CLUSTER) as usize] == OWNERSHIP_UNEXPLAINED
        };
        let class_at = |cluster: u32| classes[(cluster - FIRST_CLUSTER) as usize];
        let starts_file = |cluster: u32| kind_of(class_at(cluster), b"PK\x03\x04").is_some();
        let starts: Vec<u32> = (FIRST_CLUSTER..last)
            .filter(|&cluster| unexplained(cluster) && starts_file(cluster))
            .collect();
        let mut candidates = Vec::new();
        for &start in starts.iter().skip(offset).take(limit) {
            // Room runs to the next file start, or the end of the unexplained space.
            let mut room = 1u64;
            let mut guessed = 1u64;
            let mut similar = true;
            let mut cluster = start + 1;
            while cluster < last
                && unexplained(cluster)
                && !starts_file(cluster)
                && room < MAX_CARVE_BYTES / cluster_size
            {
                if similar && ends_a_run(class_at(cluster)) {
                    similar = false;
                }
                if similar {
                    guessed += 1;
                }
                room += 1;
                cluster += 1;
            }
            let base = self.volume.cluster_offset(start);
            let space = room * cluster_size;
            let read = |at: u64, length: usize| -> Option<Vec<u8>> {
                if at >= space {
                    return None;
                }
                let length = (length as u64).min(space - at) as usize;
                self.volume.read_bytes(base + at, length).ok()
            };
            if let Some((kind, extension, bytes, end_by, confidence)) =
                estimate(class_at(start), &read, cluster_size, room, guessed)
            {
                candidates.push(CarveCandidate {
                    start_cluster: start,
                    kind,
                    extension,
                    bytes,
                    clusters: bytes.div_ceil(cluster_size),
                    end_by,
                    confidence,
                });
            }
        }
        let bytes_estimated_on_this_page = candidates.iter().map(|found| found.bytes).sum();
        Ok(CarvePage {
            candidates,
            total: starts.len() as u64,
            bytes_estimated_on_this_page,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(data: &[u8]) -> impl Fn(u64, usize) -> Option<Vec<u8>> + '_ {
        move |at, length| {
            let at = at as usize;
            (at < data.len()).then(|| data[at..(at + length).min(data.len())].to_vec())
        }
    }

    fn jpeg(body: &[u8]) -> Vec<u8> {
        let mut data = vec![0xFF, 0xD8];
        // An APP1 segment holding a thumbnail, with its own end marker, to be stepped over.
        let thumbnail = [0xFF, 0xD8, 0x01, 0x02, 0xFF, 0xD9];
        data.extend_from_slice(&[0xFF, 0xE1]);
        data.extend_from_slice(&((thumbnail.len() + 2) as u16).to_be_bytes());
        data.extend_from_slice(&thumbnail);
        data.extend_from_slice(&[0xFF, 0xDA, 0x00, 0x04, 0x00, 0x00]);
        data.extend_from_slice(body);
        data.extend_from_slice(&[0xFF, 0xD9]);
        data
    }

    #[test]
    fn a_jpeg_ends_at_its_own_end_marker_not_its_thumbnails() {
        let file = jpeg(&[0x12, 0xFF, 0x00, 0x34, 0x56]);
        let mut disk = file.clone();
        disk.extend_from_slice(&[0xAB; 100]);
        assert_eq!(
            jpeg_end(&reader(&disk), disk.len() as u64),
            End::At(file.len() as u64)
        );
        // Cut before its end: the space runs out first.
        assert_eq!(
            jpeg_end(&reader(&file[..file.len() - 2]), (file.len() - 2) as u64),
            End::PastTheEnd
        );
    }

    #[test]
    fn a_png_ends_after_its_iend_chunk() {
        let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
        for (kind, body) in [
            (&b"IHDR"[..], &[0u8; 13][..]),
            (b"IDAT", &[7u8; 40]),
            (b"IEND", &[]),
        ] {
            file.extend_from_slice(&(body.len() as u32).to_be_bytes());
            file.extend_from_slice(kind);
            file.extend_from_slice(body);
            file.extend_from_slice(&[0u8; 4]);
        }
        let mut disk = file.clone();
        disk.extend_from_slice(&[0x55; 64]);
        assert_eq!(
            png_end(&reader(&disk), disk.len() as u64),
            End::At(file.len() as u64)
        );
    }

    #[test]
    fn an_mp4_ends_where_its_boxes_stop() {
        let mut file = Vec::new();
        for (kind, size) in [(&b"ftyp"[..], 24u32), (b"moov", 100), (b"mdat", 1000)] {
            file.extend_from_slice(&size.to_be_bytes());
            file.extend_from_slice(kind);
            file.extend_from_slice(&vec![0u8; size as usize - 8]);
        }
        let mut disk = file.clone();
        disk.extend_from_slice(&[0xFF; 64]);
        assert_eq!(
            boxes_end(&reader(&disk), disk.len() as u64),
            End::At(file.len() as u64)
        );
    }

    #[test]
    fn a_zip_ends_after_its_end_record_and_a_pdf_after_its_last_eof() {
        let mut zip = b"PK\x03\x04".to_vec();
        zip.extend_from_slice(&[1u8; 50]);
        zip.extend_from_slice(b"PK\x05\x06");
        zip.extend_from_slice(&[0u8; 16]);
        zip.extend_from_slice(&3u16.to_le_bytes());
        zip.extend_from_slice(b"abc");
        let mut disk = zip.clone();
        disk.extend_from_slice(&[9u8; 40]);
        assert_eq!(
            archive_end(&reader(&disk), disk.len() as u64),
            End::At(zip.len() as u64)
        );

        let pdf = b"%PDF-1.4 body %%EOF\n update %%EOF\r\n".to_vec();
        let mut disk = pdf.clone();
        disk.extend_from_slice(&[0u8; 30]);
        assert_eq!(
            pdf_end(&reader(&disk), disk.len() as u64),
            End::At(pdf.len() as u64)
        );
    }

    #[test]
    fn a_format_without_a_length_ends_with_its_similar_clusters() {
        let mp3 = vec![0x49u8; 4096 * 3];
        let found = estimate(CLASS_MP3, &reader(&mp3), 1024, 12, 5).unwrap();
        assert_eq!(
            found,
            (
                "mp3",
                "mp3",
                5 * 1024,
                "similar clusters",
                CONFIDENCE_GUESSED
            )
        );
    }
}
