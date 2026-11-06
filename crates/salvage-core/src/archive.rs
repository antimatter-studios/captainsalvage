//! Reads what a zip holds straight from the disk, without copying it anywhere. A damaged
//! zip often keeps most of its files, so a lost directory is rebuilt from the headers of
//! the files inside, and each file can be checked against its own checksum.

use crate::session::Session;
use anyhow::{anyhow, Result};
use flate2::read::DeflateDecoder;
use flate2::Crc;
use serde::Serialize;
use std::io::{self, Read};
use std::sync::Arc;

const END_SIGNATURE: &[u8] = b"PK\x05\x06";
const CENTRAL_SIGNATURE: &[u8] = b"PK\x01\x02";
const LOCAL_SIGNATURE: &[u8] = b"PK\x03\x04";
const ZIP64_END_SIGNATURE: &[u8] = b"PK\x06\x06";
const ZIP64_LOCATOR_SIGNATURE: &[u8] = b"PK\x06\x07";
const DESCRIPTOR_SIGNATURE: &[u8] = b"PK\x07\x08";

const END_BYTES: usize = 22;
const MAX_COMMENT_BYTES: usize = 65_535;
const ZIP64_LOCATOR_BYTES: u64 = 20;
const ZIP64_END_BYTES: u64 = 56;
const CENTRAL_BYTES: usize = 46;
const LOCAL_BYTES: usize = 30;
const DESCRIPTOR_BYTES: u64 = 24;
const ZIP64_EXTRA_ID: usize = 0x0001;
const SATURATED_32: u64 = 0xFFFF_FFFF;

const FLAG_ENCRYPTED: usize = 0x0001;
const FLAG_SIZES_FOLLOW_DATA: usize = 0x0008;
const METHOD_STORED: u16 = 0;
const METHOD_DEFLATED: u16 = 8;

/// A header that claims more than this is not a real header.
const MAX_HEADER_VERSION: usize = 100;
const MAX_HEADER_METHOD: usize = 99;
const MAX_NAME_BYTES: usize = 4096;

const MAX_MEMBERS: usize = 500_000;
const MAX_DIRECTORY_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SEARCH_BYTES: u64 = 1024 * 1024 * 1024;
const SEARCH_CHUNK_BYTES: u64 = 4 * 1024 * 1024;
const CHECK_CHUNK_BYTES: u64 = 1024 * 1024;
const MAX_CHECK_BYTES: u64 = 4 * 1024 * 1024 * 1024;

pub const SOURCE_DIRECTORY: &str = "directory";
pub const SOURCE_HEADERS: &str = "headers";
pub const CHECK_INTACT: &str = "intact";
pub const CHECK_DAMAGED: &str = "damaged";
pub const CHECK_UNCHECKED: &str = "unchecked";

/// One file or folder inside a zip. A size is missing when only a header was found, and
/// that header left its sizes for after the data.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZipMember {
    pub index: u32,
    pub path: String,
    pub is_dir: bool,
    pub packed: Option<u64>,
    pub size: Option<u64>,
    pub method: u16,
    pub crc: Option<u32>,
    pub encrypted: bool,
    pub modified: Option<String>,
    pub offset: u64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZipListing {
    /// What kind of archive it is: zip, tar, tar.gz, gz, 7z and so on.
    pub format: String,
    /// Where the list came from: the zip's own directory, or a search for file headers.
    pub source: &'static str,
    pub members: Vec<ZipMember>,
    /// False when the list may be missing files.
    pub complete: bool,
    pub total_bytes: u64,
    pub note: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ZipMemberCheck {
    pub status: &'static str,
    pub note: String,
}

type ReadRange<'a> = &'a dyn Fn(u64, u64) -> Result<Vec<u8>>;

fn u16_at(data: &[u8], at: usize) -> usize {
    usize::from(u16::from_le_bytes([data[at], data[at + 1]]))
}

fn u32_at(data: &[u8], at: usize) -> u64 {
    u64::from(u32::from_le_bytes([
        data[at],
        data[at + 1],
        data[at + 2],
        data[at + 3],
    ]))
}

fn u64_at(data: &[u8], at: usize) -> u64 {
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[at..at + 8]);
    u64::from_le_bytes(bytes)
}

fn find(data: &[u8], signature: &[u8]) -> Option<usize> {
    data.windows(signature.len())
        .position(|window| window == signature)
}

/// Zips store a date and time the way DOS did, in local time with no zone.
fn dos_stamp(date: usize, time: usize) -> Option<String> {
    let (month, day) = ((date >> 5) & 0xF, date & 0x1F);
    if month == 0 || month > 12 || day == 0 {
        return None;
    }
    Some(format!(
        "{:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        1980 + (date >> 9),
        time >> 11,
        (time >> 5) & 0x3F,
        (time & 0x1F) * 2,
    ))
}

/// For display only: nothing here ever becomes a path on a disk.
fn clean_name(raw: &[u8]) -> String {
    let name = String::from_utf8_lossy(raw).replace('\\', "/");
    name.trim_start_matches("./")
        .trim_start_matches('/')
        .to_string()
}

struct Directory {
    members: Vec<ZipMember>,
    complete: bool,
}

/// Follows the end record to the zip's own directory, if the end record survived.
fn read_directory(total: u64, read: ReadRange) -> Result<Option<Directory>> {
    let tail_length = total.min((END_BYTES + MAX_COMMENT_BYTES) as u64);
    let tail_start = total - tail_length;
    let tail = read(tail_start, tail_length)?;
    // The comment's length must fit, which rules out the signature turning up by chance.
    let found = tail
        .windows(END_SIGNATURE.len())
        .enumerate()
        .rev()
        .find(|(at, window)| {
            *window == END_SIGNATURE
                && at + END_BYTES <= tail.len()
                && at + END_BYTES + u16_at(&tail, at + 20) <= tail.len()
        })
        .map(|(at, _)| at);
    let Some(end) = found else {
        return Ok(None);
    };
    let end_at = tail_start + end as u64;
    let mut expected = u16_at(&tail, end + 10) as u64;
    let mut size = u32_at(&tail, end + 12);
    let mut start = u32_at(&tail, end + 16);
    let mut directory_end = end_at;

    if end_at >= ZIP64_LOCATOR_BYTES {
        let locator = read(end_at - ZIP64_LOCATOR_BYTES, ZIP64_LOCATOR_BYTES)?;
        if locator.starts_with(ZIP64_LOCATOR_SIGNATURE) {
            let record_at = u64_at(&locator, 8);
            let record = read(record_at, ZIP64_END_BYTES)?;
            if record.len() as u64 == ZIP64_END_BYTES && record.starts_with(ZIP64_END_SIGNATURE) {
                expected = u64_at(&record, 32);
                size = u64_at(&record, 40);
                start = u64_at(&record, 48);
                directory_end = record_at;
            }
        }
    }

    // Anything put in front of a zip, like a self-extractor, moves every offset it records.
    let shift = directory_end.saturating_sub(size).saturating_sub(start);
    // A zip that starts with a file header has nothing in front of it. An end record that
    // says otherwise belongs to a zip stored inside this one, whose own end was lost.
    if shift > 0 && read(0, LOCAL_SIGNATURE.len() as u64)?.starts_with(LOCAL_SIGNATURE) {
        return Ok(None);
    }
    let read_size = size.min(MAX_DIRECTORY_BYTES);
    let data = read(start + shift, read_size)?;
    let mut members = Vec::new();
    let mut position = 0;
    while (members.len() as u64) < expected && members.len() < MAX_MEMBERS {
        match parse_central(&data, position, members.len() as u32, shift) {
            Some((member, next)) => {
                members.push(member);
                position = next;
            }
            None => break,
        }
    }
    let complete = members.len() as u64 == expected;
    Ok(Some(Directory { members, complete }))
}

fn parse_central(data: &[u8], at: usize, index: u32, shift: u64) -> Option<(ZipMember, usize)> {
    let head = data.get(at..at + CENTRAL_BYTES)?;
    if !head.starts_with(CENTRAL_SIGNATURE) {
        return None;
    }
    let flags = u16_at(head, 8);
    let name_length = u16_at(head, 28);
    let extra_length = u16_at(head, 30);
    let comment_length = u16_at(head, 32);
    let name_end = at + CENTRAL_BYTES + name_length;
    let extra_end = name_end + extra_length;
    let raw_name = data.get(at + CENTRAL_BYTES..name_end)?;
    let extra = data.get(name_end..extra_end)?;
    let mut packed = u32_at(head, 20);
    let mut size = u32_at(head, 24);
    let mut offset = u32_at(head, 42);
    if let Some(wide) = zip64_extra(extra) {
        let mut fields = wide.chunks_exact(8).map(|bytes| u64_at(bytes, 0));
        if size == SATURATED_32 {
            size = fields.next()?;
        }
        if packed == SATURATED_32 {
            packed = fields.next()?;
        }
        if offset == SATURATED_32 {
            offset = fields.next()?;
        }
    }
    let path = clean_name(raw_name);
    let member = ZipMember {
        index,
        is_dir: path.ends_with('/'),
        path,
        packed: Some(packed),
        size: Some(size),
        method: u16_at(head, 10) as u16,
        crc: Some(u32_at(head, 16) as u32),
        encrypted: flags & FLAG_ENCRYPTED != 0,
        modified: dos_stamp(u16_at(head, 14), u16_at(head, 12)),
        offset: offset + shift,
    };
    Some((member, extra_end + comment_length))
}

fn zip64_extra(extra: &[u8]) -> Option<&[u8]> {
    let mut position = 0;
    while position + 4 <= extra.len() {
        let id = u16_at(extra, position);
        let length = u16_at(extra, position + 2);
        let body = extra.get(position + 4..position + 4 + length)?;
        if id == ZIP64_EXTRA_ID {
            return Some(body);
        }
        position += 4 + length;
    }
    None
}

/// Reads ahead in large pieces, so many small files do not mean many small reads.
struct SearchWindow<'a> {
    read: ReadRange<'a>,
    total: u64,
    start: u64,
    data: Vec<u8>,
    bytes_read: u64,
}

impl SearchWindow<'_> {
    fn bytes_at(&mut self, position: u64) -> Result<&[u8]> {
        let wanted = position.saturating_add((LOCAL_BYTES + MAX_NAME_BYTES) as u64);
        let held_to = self.start + self.data.len() as u64;
        if position < self.start || wanted.min(self.total) > held_to {
            self.data = (self.read)(position, SEARCH_CHUNK_BYTES)?;
            self.start = position;
            self.bytes_read += self.data.len() as u64;
        }
        Ok(&self.data[(position - self.start) as usize..])
    }
}

/// A file header with believable values, or nothing. The signature alone turns up by
/// chance in compressed data.
fn parse_local(data: &[u8], offset: u64, index: u32) -> Option<ZipMember> {
    let head = data.get(..LOCAL_BYTES)?;
    if !head.starts_with(LOCAL_SIGNATURE) {
        return None;
    }
    let flags = u16_at(head, 6);
    let method = u16_at(head, 8);
    let name_length = u16_at(head, 26);
    if u16_at(head, 4) > MAX_HEADER_VERSION
        || method > MAX_HEADER_METHOD
        || name_length == 0
        || name_length > MAX_NAME_BYTES
    {
        return None;
    }
    let raw_name = data.get(LOCAL_BYTES..LOCAL_BYTES + name_length)?;
    if raw_name.iter().any(|byte| *byte < 0x20) {
        return None;
    }
    let known = flags & FLAG_SIZES_FOLLOW_DATA == 0;
    let path = clean_name(raw_name);
    Some(ZipMember {
        index,
        is_dir: path.ends_with('/'),
        path,
        packed: known.then(|| u32_at(head, 18)),
        size: known.then(|| u32_at(head, 22)),
        method: method as u16,
        crc: known.then(|| u32_at(head, 14) as u32),
        encrypted: flags & FLAG_ENCRYPTED != 0,
        modified: dos_stamp(u16_at(head, 12), u16_at(head, 10)),
        offset,
    })
}

struct Search {
    members: Vec<ZipMember>,
    complete: bool,
}

/// Rebuilds the list from the header in front of each file.
fn search_headers(total: u64, read: ReadRange) -> Result<Search> {
    let mut window = SearchWindow {
        read,
        total,
        start: 0,
        data: Vec::new(),
        bytes_read: 0,
    };
    let mut members: Vec<ZipMember> = Vec::new();
    let mut position = 0u64;
    let mut complete = true;
    while position < total {
        if window.bytes_read >= MAX_SEARCH_BYTES || members.len() >= MAX_MEMBERS {
            complete = false;
            break;
        }
        let index = members.len() as u32;
        let rest = window.bytes_at(position)?;
        if rest.len() < LOCAL_BYTES {
            break;
        }
        let Some(at) = find(rest, LOCAL_SIGNATURE) else {
            position += (rest.len() - (LOCAL_SIGNATURE.len() - 1)) as u64;
            continue;
        };
        position += at as u64;
        let rest = window.bytes_at(position)?;
        let Some(member) = parse_local(rest, position, index) else {
            position += 1;
            continue;
        };
        let header = LOCAL_BYTES as u64 + u16_at(rest, 26) as u64 + u16_at(rest, 28) as u64;
        // A known size jumps over the data, so a zip stored inside this one stays one file.
        position += header + member.packed.unwrap_or(0);
        members.push(member);
    }
    Ok(Search { members, complete })
}

/// Compression methods the zip library unpacks here: stored, deflate, deflate64, and AES,
/// whose files say their real method inside.
const LIBRARY_METHODS: [u16; 4] = [0, 8, 9, 99];

fn library_stamp(stamp: zip::DateTime) -> Option<String> {
    (stamp.month() > 0 && stamp.day() > 0).then(|| {
        format!(
            "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
            stamp.year(),
            stamp.month(),
            stamp.day(),
            stamp.hour(),
            stamp.minute(),
            stamp.second()
        )
    })
}

/// Lists a zip whose directory is whole, with the zip library. Nothing when the library
/// cannot open it, which is what a damaged directory does; the search below then takes over.
fn list_with_library(total: u64, read: ReadRange) -> Option<Vec<ZipMember>> {
    let mut archive = zip::ZipArchive::new(crate::archives::Source::new(total, read)).ok()?;
    let mut members = Vec::with_capacity(archive.len());
    for index in 0..archive.len().min(MAX_MEMBERS) {
        let file = archive.by_index_raw(index).ok()?;
        // The library offers no other way to read the method's number, which the window shows.
        #[allow(deprecated)]
        let method = file.compression().to_u16();
        members.push(ZipMember {
            index: index as u32,
            path: clean_name(file.name_raw()),
            is_dir: file.is_dir(),
            packed: Some(file.compressed_size()),
            size: Some(file.size()),
            method,
            crc: Some(file.crc32()),
            encrypted: file.encrypted(),
            modified: file.last_modified().and_then(library_stamp),
            offset: file.header_start(),
        });
    }
    Some(members)
}

/// Checks one file with the zip library, when the zip opens with it and lists this file at the
/// same place. Nothing otherwise, so the app's own check takes over.
fn check_with_library(member: &ZipMember, total: u64, read: ReadRange) -> Option<ZipMemberCheck> {
    let mut archive = zip::ZipArchive::new(crate::archives::Source::new(total, read)).ok()?;
    let index = member.index as usize;
    let same = archive.by_index_raw(index).is_ok_and(|file| {
        file.header_start() == member.offset && clean_name(file.name_raw()) == member.path
    });
    if !same {
        return None;
    }
    let mut file = match archive.by_index(index) {
        Ok(file) => file,
        Err(zip::result::ZipError::UnsupportedArchive(why))
            if why == zip::result::ZipError::PASSWORD_REQUIRED =>
        {
            return Some(verdict(
                CHECK_UNCHECKED,
                "It is encrypted, so it cannot be checked without its password.",
            ));
        }
        Err(zip::result::ZipError::UnsupportedArchive(_)) => {
            return Some(verdict(
                CHECK_UNCHECKED,
                format!(
                    "It uses compression method {}, which this app cannot unpack yet.",
                    member.method
                ),
            ));
        }
        Err(error) => {
            return Some(verdict(
                CHECK_DAMAGED,
                format!("Its local header cannot be read: {error}."),
            ));
        }
    };
    let mut buffer = vec![0u8; 64 * 1024];
    let mut seen = 0u64;
    loop {
        match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(count) => seen += count as u64,
            Err(error) if error.to_string().to_lowercase().contains("checksum") => {
                return Some(verdict(
                    CHECK_DAMAGED,
                    "Its data does not match its checksum, so some of it is wrong.",
                ));
            }
            Err(error) => {
                return Some(verdict(
                    CHECK_DAMAGED,
                    format!("It stops unpacking after {seen} bytes: {error}."),
                ));
            }
        }
    }
    Some(match member.size {
        Some(size) if size != seen => verdict(
            CHECK_DAMAGED,
            format!("It unpacks to {seen} bytes, not the {size} it should."),
        ),
        _ => verdict(CHECK_INTACT, "It unpacks whole, and its checksum agrees."),
    })
}

/// Lists a zip: with the zip library when its directory is whole, and otherwise from what is
/// left of its directory or from its file headers, whichever finds more.
pub fn list_zip(total: u64, read: ReadRange) -> Result<ZipListing> {
    let directory = read_directory(total, read)?;
    // The library is trusted only once this zip's own directory is found whole at its end: a
    // zip cut short can still show the end of a zip stored inside it, which the library would
    // open as if it were this one.
    let whole = matches!(directory, Some(Directory { complete: true, .. }));
    if let Some(members) = whole.then(|| list_with_library(total, read)).flatten() {
        return Ok(ZipListing {
            format: "zip".into(),
            source: SOURCE_DIRECTORY,
            members,
            complete: true,
            total_bytes: total,
            note: None,
        });
    }
    if let Some(Directory {
        members,
        complete: true,
    }) = directory
    {
        return Ok(ZipListing {
            format: "zip".into(),
            source: SOURCE_DIRECTORY,
            members,
            complete: true,
            total_bytes: total,
            note: None,
        });
    }
    let partial = directory.map(|found| found.members).unwrap_or_default();
    let search = search_headers(total, read)?;
    let partial_count = partial.len();
    if partial_count >= search.members.len() && partial_count > 0 {
        return Ok(ZipListing {
            format: "zip".into(),
            source: SOURCE_DIRECTORY,
            members: partial,
            complete: false,
            total_bytes: total,
            note: Some(
                "The zip's own directory breaks part way through, so this list may be missing files."
                    .into(),
            ),
        });
    }
    let note = if search.members.is_empty() {
        "Nothing in this file looks like a zip: it has no directory and no file headers."
            .to_string()
    } else if search.complete {
        "The zip's own directory is missing or damaged, so this list was rebuilt from the headers of the files inside. Some files may be missing."
            .to_string()
    } else {
        "The zip's own directory is missing or damaged, so this list was rebuilt from the headers of the files inside. The search stopped early, so more files may be missing."
            .to_string()
    };
    Ok(ZipListing {
        format: "zip".into(),
        source: SOURCE_HEADERS,
        complete: false,
        members: search.members,
        total_bytes: total,
        note: Some(note),
    })
}

/// Feeds a range of the zip to an unpacker a piece at a time.
struct RangeReader<'a> {
    read: ReadRange<'a>,
    position: u64,
    end: u64,
    held: Vec<u8>,
    used: usize,
}

impl Read for RangeReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.used == self.held.len() {
            let length = CHECK_CHUNK_BYTES.min(self.end.saturating_sub(self.position));
            if length == 0 {
                return Ok(0);
            }
            self.held = (self.read)(self.position, length)
                .map_err(|error| io::Error::other(format!("{error:#}")))?;
            self.used = 0;
            self.position += self.held.len() as u64;
            if self.held.is_empty() {
                return Ok(0);
            }
        }
        let count = buffer.len().min(self.held.len() - self.used);
        buffer[..count].copy_from_slice(&self.held[self.used..self.used + count]);
        self.used += count;
        Ok(count)
    }
}

fn verdict(status: &'static str, note: impl Into<String>) -> ZipMemberCheck {
    ZipMemberCheck {
        status,
        note: note.into(),
    }
}

/// Runs a file's bytes into a checksum, up to one byte past the size it claims.
fn checksum_of(source: &mut dyn Read, limit: u64) -> io::Result<(u32, u64)> {
    let mut crc = Crc::new();
    let mut buffer = vec![0u8; 64 * 1024];
    let mut seen = 0u64;
    loop {
        let count = source.read(&mut buffer)?;
        if count == 0 {
            return Ok((crc.sum(), seen));
        }
        seen += count as u64;
        crc.update(&buffer[..count]);
        if seen > limit {
            return Ok((crc.sum(), seen));
        }
    }
}

/// Unpacks one file in memory to compare it with its checksum. Nothing is saved.
pub fn check_member(member: &ZipMember, total: u64, read: ReadRange) -> Result<ZipMemberCheck> {
    if member.is_dir {
        return Ok(verdict(CHECK_UNCHECKED, "A folder has nothing to check."));
    }
    if member.encrypted {
        return Ok(verdict(
            CHECK_UNCHECKED,
            "It is encrypted, so it cannot be checked without its password.",
        ));
    }
    let whole = matches!(
        read_directory(total, read)?,
        Some(Directory { complete: true, .. })
    );
    if whole && LIBRARY_METHODS.contains(&member.method) {
        if let Some(found) = check_with_library(member, total, read) {
            return Ok(found);
        }
    }
    if member.method != METHOD_STORED && member.method != METHOD_DEFLATED {
        return Ok(verdict(
            CHECK_UNCHECKED,
            format!(
                "It uses compression method {}, which this app cannot unpack yet.",
                member.method
            ),
        ));
    }
    let header = read(member.offset, LOCAL_BYTES as u64)?;
    if !header.starts_with(LOCAL_SIGNATURE) || header.len() < LOCAL_BYTES {
        return Ok(verdict(
            CHECK_DAMAGED,
            "Its header is not where the zip says, so other data has taken its place or the zip is cut short.",
        ));
    }
    let start = member.offset
        + LOCAL_BYTES as u64
        + u16_at(&header, 26) as u64
        + u16_at(&header, 28) as u64;
    let end = match member.packed {
        Some(packed) if packed > MAX_CHECK_BYTES => {
            return Ok(verdict(CHECK_UNCHECKED, "It is too big to check here."));
        }
        Some(packed) if start + packed > total => {
            return Ok(verdict(
                CHECK_DAMAGED,
                "It runs past the end of the zip, so the zip is cut short.",
            ));
        }
        Some(packed) => start + packed,
        None => total,
    };
    let mut source = RangeReader {
        read,
        position: start,
        end,
        held: Vec::new(),
        used: 0,
    };
    let limit = member.size.unwrap_or(MAX_CHECK_BYTES);

    if member.method == METHOD_STORED {
        if member.packed.is_none() {
            return Ok(verdict(
                CHECK_UNCHECKED,
                "Its header leaves out how long it is, and it is not compressed, so its end cannot be found.",
            ));
        }
        let (crc, seen) = checksum_of(&mut source, limit)?;
        return Ok(compare(member, crc, seen, member.crc));
    }

    let mut decoder = DeflateDecoder::new(source);
    let (crc, seen) = match checksum_of(&mut decoder, limit) {
        Ok(result) => result,
        Err(error) => {
            return Ok(verdict(
                CHECK_DAMAGED,
                format!("It stops unpacking: {error}."),
            ));
        }
    };
    let expected = match member.crc {
        Some(crc) => Some(crc),
        // Only a header was found, so the checksum is in the record after the data.
        None => {
            let record = read(start + decoder.total_in(), DESCRIPTOR_BYTES)?;
            let body = if record.starts_with(DESCRIPTOR_SIGNATURE) {
                record.get(4..)
            } else {
                record.get(0..)
            };
            body.filter(|bytes| bytes.len() >= 4)
                .map(|bytes| u32_at(bytes, 0) as u32)
        }
    };
    Ok(compare(member, crc, seen, expected))
}

fn compare(member: &ZipMember, crc: u32, seen: u64, expected: Option<u32>) -> ZipMemberCheck {
    if let Some(size) = member.size.filter(|size| *size != seen) {
        return verdict(
            CHECK_DAMAGED,
            format!("It unpacks to a different size than the {size} bytes it claims."),
        );
    }
    match expected {
        Some(expected) if expected == crc => verdict(
            CHECK_INTACT,
            "It unpacks, and matches its checksum, so its contents are as they were stored.",
        ),
        Some(_) => verdict(
            CHECK_DAMAGED,
            "It does not match its checksum, so its contents have changed.",
        ),
        None => verdict(CHECK_UNCHECKED, "Its checksum could not be found."),
    }
}

impl Session {
    /// Reads ranges of one file from the disk, as a reader an archive library can keep.
    fn shared_reader(self: &Arc<Self>, id: i64) -> crate::archives::SharedRead {
        let session = Arc::clone(self);
        Arc::new(move |start, length| {
            session.note_viewing();
            Ok(session.read_entry_range(id, start, length)?.data)
        })
    }

    fn zip_listing(self: &Arc<Self>, id: i64) -> Result<Arc<ZipListing>> {
        self.note_viewing();
        let (total, name) = self.entry_total(id)?;
        {
            let cache = self.zip.lock().map_err(|_| anyhow!("zip lock poisoned"))?;
            if let Some((cached, listing)) = cache.as_ref() {
                if *cached == id && listing.total_bytes == total {
                    return Ok(listing.clone());
                }
            }
        }
        let shared = self.shared_reader(id);
        // Anything not recognised is read as a zip, which says plainly if it is not one.
        let format = crate::archives::format_of(&name, total, &*shared)
            .unwrap_or(crate::archives::Format::Zip);
        let listing = Arc::new(crate::archives::list(format, &name, total, &shared)?);
        *self.zip.lock().map_err(|_| anyhow!("zip lock poisoned"))? = Some((id, listing.clone()));
        Ok(listing)
    }

    /// What an archive holds, read from the disk. A listing can be large, so it is kept.
    pub fn zip_contents(self: &Arc<Self>, id: i64) -> Result<ZipListing> {
        Ok((*self.zip_listing(id)?).clone())
    }

    pub fn zip_member_check(self: &Arc<Self>, id: i64, index: u32) -> Result<ZipMemberCheck> {
        let listing = self.zip_listing(id)?;
        let (total, name) = self.entry_total(id)?;
        let shared = self.shared_reader(id);
        let format = crate::archives::format_of(&name, total, &*shared)
            .unwrap_or(crate::archives::Format::Zip);
        crate::archives::check(format, &listing, index, listing.total_bytes, &shared)
    }
}
