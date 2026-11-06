//! Reads what other kinds of archive hold, straight from the disk, as `archive` does for
//! zips: tar, the single-file compressors gzip, xz, bzip2 and zstd, tar packed with any of
//! them, and 7z. Nothing is written anywhere: each file is unpacked in memory to list or check
//! it, then dropped.
//!
//! An archive on a damaged disk is often cut short or has holes in it. A listing goes as far
//! as the data allows and says where it stopped, and a check says whether a file unpacks
//! whole, and whether its own checksum agrees, where the format keeps one.

use crate::archive::{
    ZipListing, ZipMember, ZipMemberCheck, CHECK_DAMAGED, CHECK_INTACT, CHECK_UNCHECKED,
    SOURCE_DIRECTORY,
};
use anyhow::{bail, Result};
use std::io::{self, Read, Seek, SeekFrom};

const CHUNK_BYTES: u64 = 1024 * 1024;
const TAR_MAGIC_AT: usize = 257;
const TAR_BLOCK: usize = 512;
const MAX_MEMBERS: usize = 500_000;
const XZ_INPUT_BYTES: usize = 64 * 1024;

const GZIP_MAGIC: &[u8] = &[0x1F, 0x8B];
const XZ_MAGIC: &[u8] = &[0xFD, b'7', b'z', b'X', b'Z', 0x00];
const BZIP2_MAGIC: &[u8] = b"BZh";
const ZSTD_MAGIC: &[u8] = &[0x28, 0xB5, 0x2F, 0xFD];
const SEVEN_Z_MAGIC: &[u8] = &[b'7', b'z', 0xBC, 0xAF, 0x27, 0x1C];
const RAR_MAGIC: &[u8] = b"Rar!\x1a\x07";
const ZIP_MAGIC: &[u8] = b"PK";

type ReadRange<'a> = &'a dyn Fn(u64, u64) -> Result<Vec<u8>>;
/// Reads a range of the archive from the disk, and can be kept: the rar reader holds its
/// source for as long as the archive is open, so it needs one it owns.
pub type SharedRead = std::sync::Arc<dyn Fn(u64, u64) -> Result<Vec<u8>> + Send + Sync>;

/// How a stream of bytes is compressed, if at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Packing {
    Plain,
    Gzip,
    Xz,
    Bzip2,
    Zstd,
}

/// What kind of archive a file is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Zip,
    /// A tar, perhaps compressed as a whole.
    Tar(Packing),
    /// One file, compressed.
    Packed(Packing),
    SevenZ,
    Rar,
}

impl Packing {
    fn name(self) -> &'static str {
        match self {
            Packing::Plain => "",
            Packing::Gzip => "gz",
            Packing::Xz => "xz",
            Packing::Bzip2 => "bz2",
            Packing::Zstd => "zst",
        }
    }
}

impl Format {
    /// How the window names it: zip, tar, tar.gz, gz, 7z and so on.
    pub fn name(self) -> String {
        match self {
            Format::Zip => "zip".into(),
            Format::Tar(Packing::Plain) => "tar".into(),
            Format::Tar(packing) => format!("tar.{}", packing.name()),
            Format::Packed(packing) => packing.name().into(),
            Format::SevenZ => "7z".into(),
            Format::Rar => "rar".into(),
        }
    }
}

const ZIP_EXTENSIONS: &[&str] = &[
    "zip", "jar", "apk", "ipa", "epub", "cbz", "docx", "xlsx", "pptx", "odt", "ods", "odp",
    "pages", "numbers", "key",
];

/// The kind of archive, by the name's ending.
pub fn format_by_name(name: &str) -> Option<Format> {
    let name = name.to_ascii_lowercase();
    if name.starts_with("._") {
        return None;
    }
    let ends = |ending: &str| name.ends_with(ending);
    let found = if ends(".tar.gz") || ends(".tgz") {
        Format::Tar(Packing::Gzip)
    } else if ends(".tar.xz") || ends(".txz") {
        Format::Tar(Packing::Xz)
    } else if ends(".tar.bz2") || ends(".tbz") || ends(".tbz2") {
        Format::Tar(Packing::Bzip2)
    } else if ends(".tar.zst") || ends(".tzst") {
        Format::Tar(Packing::Zstd)
    } else if ends(".tar") {
        Format::Tar(Packing::Plain)
    } else if ends(".gz") {
        Format::Packed(Packing::Gzip)
    } else if ends(".xz") {
        Format::Packed(Packing::Xz)
    } else if ends(".bz2") {
        Format::Packed(Packing::Bzip2)
    } else if ends(".zst") {
        Format::Packed(Packing::Zstd)
    } else if ends(".7z") {
        Format::SevenZ
    } else if ends(".rar") {
        Format::Rar
    } else {
        let extension = name.rsplit_once('.')?.1;
        if !ZIP_EXTENSIONS.contains(&extension) {
            return None;
        }
        Format::Zip
    };
    Some(found)
}

/// The kind of archive, by its first bytes, for a file whose name was lost or is wrong.
pub fn format_by_bytes(head: &[u8]) -> Option<Format> {
    let packing = if head.starts_with(GZIP_MAGIC) {
        Packing::Gzip
    } else if head.starts_with(XZ_MAGIC) {
        Packing::Xz
    } else if head.starts_with(BZIP2_MAGIC) {
        Packing::Bzip2
    } else if head.starts_with(ZSTD_MAGIC) {
        Packing::Zstd
    } else if head.starts_with(SEVEN_Z_MAGIC) {
        return Some(Format::SevenZ);
    } else if head.starts_with(RAR_MAGIC) {
        return Some(Format::Rar);
    } else if head.starts_with(ZIP_MAGIC) {
        return Some(Format::Zip);
    } else if is_tar(head) {
        return Some(Format::Tar(Packing::Plain));
    } else {
        return None;
    };
    Some(Format::Packed(packing))
}

fn is_tar(head: &[u8]) -> bool {
    head.get(TAR_MAGIC_AT..TAR_MAGIC_AT + 5) == Some(b"ustar")
}

/// The kind of archive a file is: by its name first, then by its bytes. A compressed file
/// that holds a tar is a tar, whatever it is called.
pub fn format_of(name: &str, total: u64, read: ReadRange) -> Option<Format> {
    let head = read(0, (TAR_BLOCK as u64).min(total)).unwrap_or_default();
    let found = format_by_name(name).or_else(|| format_by_bytes(&head))?;
    if let Format::Packed(packing) = found {
        let mut inner = vec![0u8; TAR_BLOCK];
        let mut unpacked = unpacker(packing, Source::new(total, read)).ok()?;
        let filled = read_up_to(&mut *unpacked, &mut inner).unwrap_or(0);
        if is_tar(&inner[..filled]) {
            return Some(Format::Tar(packing));
        }
    }
    Some(found)
}

/// Reads until the buffer is full or the stream ends.
fn read_up_to(source: &mut dyn Read, buffer: &mut [u8]) -> io::Result<usize> {
    let mut filled = 0;
    while filled < buffer.len() {
        match source.read(&mut buffer[filled..])? {
            0 => break,
            count => filled += count,
        }
    }
    Ok(filled)
}

/// The archive's bytes, read from the disk a piece at a time, and seekable for 7z.
pub(crate) struct Source<'a> {
    read: ReadRange<'a>,
    total: u64,
    position: u64,
    held_at: u64,
    held: Vec<u8>,
}

impl<'a> Source<'a> {
    pub(crate) fn new(total: u64, read: ReadRange<'a>) -> Source<'a> {
        Source {
            read,
            total,
            position: 0,
            held_at: 0,
            held: Vec::new(),
        }
    }
}

impl Read for Source<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.total {
            return Ok(0);
        }
        let inside =
            self.position >= self.held_at && self.position < self.held_at + self.held.len() as u64;
        if !inside {
            let length = CHUNK_BYTES.min(self.total - self.position);
            self.held = (self.read)(self.position, length)
                .map_err(|error| io::Error::other(format!("{error:#}")))?;
            self.held_at = self.position;
            if self.held.is_empty() {
                return Ok(0);
            }
        }
        let from = (self.position - self.held_at) as usize;
        let count = buffer.len().min(self.held.len() - from);
        buffer[..count].copy_from_slice(&self.held[from..from + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for Source<'_> {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(back) => self.total.checked_add_signed(back),
            SeekFrom::Current(step) => self.position.checked_add_signed(step),
        };
        self.position = target.ok_or_else(|| io::Error::other("seek outside the archive"))?;
        Ok(self.position)
    }
}

/// The same as `Source`, over a reader it owns, for the rar reader.
struct OwnedSource {
    read: SharedRead,
    total: u64,
    position: u64,
    held_at: u64,
    held: Vec<u8>,
}

impl Read for OwnedSource {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position >= self.total {
            return Ok(0);
        }
        let inside =
            self.position >= self.held_at && self.position < self.held_at + self.held.len() as u64;
        if !inside {
            let length = CHUNK_BYTES.min(self.total - self.position);
            self.held = (self.read)(self.position, length)
                .map_err(|error| io::Error::other(format!("{error:#}")))?;
            self.held_at = self.position;
            if self.held.is_empty() {
                return Ok(0);
            }
        }
        let from = (self.position - self.held_at) as usize;
        let count = buffer.len().min(self.held.len() - from);
        buffer[..count].copy_from_slice(&self.held[from..from + count]);
        self.position += count as u64;
        Ok(count)
    }
}

impl Seek for OwnedSource {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let target = match to {
            SeekFrom::Start(at) => Some(at),
            SeekFrom::End(back) => self.total.checked_add_signed(back),
            SeekFrom::Current(step) => self.position.checked_add_signed(step),
        };
        self.position = target.ok_or_else(|| io::Error::other("seek outside the archive"))?;
        Ok(self.position)
    }
}

/// Unpacks a compressed stream as it is read. Each decoder checks the stream's own checksum
/// as it reaches it, and fails if the data does not agree.
fn unpacker<'a>(packing: Packing, source: Source<'a>) -> io::Result<Box<dyn Read + 'a>> {
    Ok(match packing {
        Packing::Plain => Box::new(source),
        Packing::Gzip => Box::new(flate2::read::MultiGzDecoder::new(source)),
        Packing::Xz => Box::new(XzStream::new(source)),
        Packing::Bzip2 => Box::new(bzip2_rs::DecoderReader::new(source)),
        Packing::Zstd => {
            Box::new(ruzstd::decoding::StreamingDecoder::new(source).map_err(io::Error::other)?)
        }
    })
}

/// Unpacks xz as it is read, with xz4rust's decoder. Its own reader needs a source that lives
/// for the whole program, and this one borrows the disk, so this feeds the decoder itself.
/// The decoder checks each block's checksum as it reaches it.
struct XzStream<'a> {
    decoder: Box<xz4rust::XzDecoder<'static>>,
    source: Source<'a>,
    input: Vec<u8>,
    start: usize,
    end: usize,
    finished: bool,
}

impl<'a> XzStream<'a> {
    fn new(source: Source<'a>) -> XzStream<'a> {
        XzStream {
            decoder: xz4rust::XzDecoder::in_heap(),
            source,
            input: vec![0; XZ_INPUT_BYTES],
            start: 0,
            end: 0,
            finished: false,
        }
    }
}

impl Read for XzStream<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if buffer.is_empty() || self.finished {
            return Ok(0);
        }
        loop {
            if self.start == self.end {
                self.start = 0;
                self.end = self.source.read(&mut self.input)?;
                if self.end == 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "the xz stream ends early",
                    ));
                }
            }
            let result = self
                .decoder
                .decode(&self.input[self.start..self.end], buffer)
                .map_err(|error| io::Error::other(format!("{error:?}")))?;
            self.start += result.input_consumed();
            if let xz4rust::XzNextBlockResult::EndOfStream(..) = result {
                self.finished = true;
            }
            if result.output_produced() > 0 || self.finished {
                return Ok(result.output_produced());
            }
        }
    }
}

/// Seconds since 1970 as a date and time, in the same form as the zip listing uses.
fn unix_stamp(seconds: i64) -> Option<String> {
    if seconds <= 0 {
        return None;
    }
    let days = seconds.div_euclid(86_400);
    let rest = seconds.rem_euclid(86_400);
    // Days to a civil date, after Howard Hinnant's public-domain algorithm.
    let shifted = days + 719_468;
    let era = shifted.div_euclid(146_097);
    let day_of_era = shifted.rem_euclid(146_097);
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}",
        rest / 3600,
        (rest / 60) % 60,
        rest % 60
    ))
}

fn member(index: usize, path: String, is_dir: bool, size: Option<u64>) -> ZipMember {
    ZipMember {
        index: index as u32,
        path,
        is_dir,
        packed: None,
        size,
        method: 0,
        crc: None,
        encrypted: false,
        modified: None,
        offset: 0,
    }
}

fn listing(
    format: Format,
    members: Vec<ZipMember>,
    total: u64,
    stopped: Option<String>,
) -> ZipListing {
    ZipListing {
        format: format.name(),
        source: SOURCE_DIRECTORY,
        complete: stopped.is_none(),
        members,
        total_bytes: total,
        note: stopped,
    }
}

/// What an archive holds. A tar is read from its start to as far as it goes.
pub fn list(format: Format, name: &str, total: u64, shared: &SharedRead) -> Result<ZipListing> {
    let read: ReadRange = &**shared;
    match format {
        Format::Zip => crate::archive::list_zip(total, read),
        Format::Tar(packing) => list_tar(format, packing, total, read),
        Format::Packed(_) => Ok(listing(
            format,
            vec![member(0, unpacked_name(name, format), false, None)],
            total,
            None,
        )),
        Format::SevenZ => list_seven_z(total, read),
        Format::Rar => list_rar(total, shared),
    }
}

/// The name of the one file a compressed file holds: its own name without the ending.
fn unpacked_name(name: &str, format: Format) -> String {
    let ending = format!(".{}", format.name());
    match name.to_ascii_lowercase().strip_suffix(&ending) {
        Some(stem) if !stem.is_empty() => name[..stem.len()].to_string(),
        _ => format!("{name} (unpacked)"),
    }
}

fn list_tar(format: Format, packing: Packing, total: u64, read: ReadRange) -> Result<ZipListing> {
    let mut archive = tar::Archive::new(unpacker(packing, Source::new(total, read))?);
    let mut members = Vec::new();
    let mut stopped = None;
    let entries = match archive.entries() {
        Ok(entries) => entries,
        Err(error) => bail!("this does not read as a tar: {error}"),
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                stopped = Some(format!(
                    "The archive cannot be read past its first {} files: {error}. The files after that point are lost or damaged.",
                    members.len()
                ));
                break;
            }
        };
        let header = entry.header();
        let path = entry
            .path()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| String::from_utf8_lossy(&entry.path_bytes()).into_owned());
        let is_dir = header.entry_type().is_dir();
        let mut found = member(
            members.len(),
            path.trim_end_matches('/').to_string(),
            is_dir,
            header.size().ok(),
        );
        found.modified = header
            .mtime()
            .ok()
            .and_then(|seconds| unix_stamp(seconds as i64));
        members.push(found);
        if members.len() >= MAX_MEMBERS {
            stopped = Some(format!("Only the first {MAX_MEMBERS} files are listed."));
            break;
        }
    }
    if members.is_empty() && stopped.is_some() {
        bail!("this does not read as a tar");
    }
    Ok(listing(format, members, total, stopped))
}

/// Opens a rar on the disk. The reader reads its headers now, and a file's data only when
/// that file is unpacked, so nothing like the whole archive is held in memory.
fn open_rar(total: u64, shared: &SharedRead) -> Result<rars::Archive> {
    let source = OwnedSource {
        read: shared.clone(),
        total,
        position: 0,
        held_at: 0,
        held: Vec::new(),
    };
    rars::ArchiveReader::read_reader(source)
        .map_err(|error| anyhow::anyhow!("this rar's list of files cannot be read: {error}"))
}

fn list_rar(total: u64, shared: &SharedRead) -> Result<ZipListing> {
    let archive = open_rar(total, shared)?;
    let members = archive
        .members()
        .enumerate()
        .map(|(index, entry)| {
            let meta = &entry.meta;
            let mut found = member(
                index,
                String::from_utf8_lossy(&meta.name).replace('\\', "/"),
                meta.is_directory,
                Some(meta.unpacked_size),
            );
            found.packed = Some(meta.packed_size);
            found.encrypted = meta.is_encrypted;
            found.modified = meta
                .modification_time()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .and_then(|since| unix_stamp(since.as_secs() as i64));
            found
        })
        .collect();
    Ok(listing(Format::Rar, members, total, None))
}

fn check_rar(wanted: &ZipMember, total: u64, shared: &SharedRead) -> Result<ZipMemberCheck> {
    if wanted.encrypted {
        return Ok(verdict(
            CHECK_UNCHECKED,
            "It is encrypted, so it cannot be checked without its password.",
        ));
    }
    let archive = match open_rar(total, shared) {
        Ok(archive) => archive,
        Err(error) => return Ok(verdict(CHECK_DAMAGED, format!("{error}."))),
    };
    // The reader checks each file's checksum as it unpacks it. A solid rar's files depend
    // on the ones before, so those are unpacked too, and only this one is counted.
    let target = wanted.index as usize;
    let mut position = 0usize;
    let mut seen: Option<u64> = None;
    let counted = std::sync::Arc::new(std::sync::Mutex::new(0u64));
    const STOP: &str = "captain-salvage: the file is checked";
    let result = archive.extract_to(None, |_meta| {
        let index = position;
        position += 1;
        if index > target {
            // The file before this one has been unpacked and checked, so the rest can wait.
            return Err(rars::Error::from(io::Error::other(STOP)));
        }
        if index == target {
            seen = Some(0);
            let counted = counted.clone();
            Ok(Box::new(SharedCounter(counted)) as Box<dyn std::io::Write>)
        } else {
            Ok(Box::new(std::io::sink()) as Box<dyn std::io::Write>)
        }
    });
    let bytes = *counted
        .lock()
        .map_err(|_| anyhow::anyhow!("count lock poisoned"))?;
    Ok(match (result, seen) {
        (_, None) => verdict(
            CHECK_DAMAGED,
            "The archive cannot be unpacked as far as this file.",
        ),
        (Ok(()), Some(_)) => verdict(
            CHECK_INTACT,
            format!("It unpacks whole, {bytes} bytes, and its checksum agrees."),
        ),
        (Err(error), Some(_)) if error.to_string().contains(STOP) => verdict(
            CHECK_INTACT,
            format!("It unpacks whole, {bytes} bytes, and its checksum agrees."),
        ),
        (Err(error), Some(_)) => verdict(CHECK_DAMAGED, format!("It cannot be unpacked: {error}.")),
    })
}

/// Counts what one rar file unpacks to, shared with the checker that reads the count after.
struct SharedCounter(std::sync::Arc<std::sync::Mutex<u64>>);

impl std::io::Write for SharedCounter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        if let Ok(mut count) = self.0.lock() {
            *count += data.len() as u64;
        }
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

fn list_seven_z(total: u64, read: ReadRange) -> Result<ZipListing> {
    let mut source = Source::new(total, read);
    let archive = sevenz_rust2::Archive::read(&mut source, &sevenz_rust2::Password::empty())
        .map_err(|error| anyhow::anyhow!("this 7z's list of files cannot be read: {error}"))?;
    let members = archive
        .files
        .iter()
        .enumerate()
        .map(|(index, entry)| {
            let mut found = member(
                index,
                entry.name.clone(),
                entry.is_directory,
                Some(entry.size),
            );
            found.packed = Some(entry.compressed_size).filter(|size| *size > 0);
            found.crc = entry.has_crc.then_some(entry.crc as u32);
            found.modified = entry
                .has_last_modified_date
                .then(|| {
                    std::time::SystemTime::from(entry.last_modified_date)
                        .duration_since(std::time::UNIX_EPOCH)
                        .ok()
                })
                .flatten()
                .and_then(|since| unix_stamp(since.as_secs() as i64));
            found
        })
        .collect();
    Ok(listing(Format::SevenZ, members, total, None))
}

fn verdict(status: &'static str, note: impl Into<String>) -> ZipMemberCheck {
    ZipMemberCheck {
        status,
        note: note.into(),
    }
}

/// Reads a stream to its end, counting its bytes, and stops at the first error.
fn drain(source: &mut dyn Read) -> (u64, Option<io::Error>) {
    let mut buffer = vec![0u8; 64 * 1024];
    let mut seen = 0u64;
    loop {
        match source.read(&mut buffer) {
            Ok(0) => return (seen, None),
            Ok(count) => seen += count as u64,
            Err(error) => return (seen, Some(error)),
        }
    }
}

/// Unpacks one file in memory and says whether it came out whole. Nothing is saved.
pub fn check(
    format: Format,
    listing: &ZipListing,
    index: u32,
    total: u64,
    shared: &SharedRead,
) -> Result<ZipMemberCheck> {
    let read: ReadRange = &**shared;
    let wanted = listing
        .members
        .get(index as usize)
        .ok_or_else(|| anyhow::anyhow!("this archive has no file number {index}"))?;
    if wanted.is_dir {
        return Ok(verdict(
            CHECK_UNCHECKED,
            "A folder holds no data of its own to check.",
        ));
    }
    match format {
        Format::Zip => crate::archive::check_member(wanted, total, read),
        Format::Packed(packing) => {
            let mut unpacked = unpacker(packing, Source::new(total, read))?;
            Ok(match drain(&mut *unpacked) {
                (seen, None) => verdict(CHECK_INTACT, format!("It unpacks whole, {seen} bytes, and the compressed stream's own checksum agrees.")),
                (seen, Some(error)) => verdict(CHECK_DAMAGED, format!("It stops unpacking after {seen} bytes: {error}.")),
            })
        }
        Format::Tar(packing) => check_tar(packing, wanted, total, read),
        Format::SevenZ => check_seven_z(wanted, total, read),
        Format::Rar => check_rar(wanted, total, shared),
    }
}

fn check_tar(
    packing: Packing,
    wanted: &ZipMember,
    total: u64,
    read: ReadRange,
) -> Result<ZipMemberCheck> {
    let mut archive = tar::Archive::new(unpacker(packing, Source::new(total, read))?);
    for (index, entry) in archive.entries()?.enumerate() {
        let mut entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                return Ok(verdict(
                    CHECK_DAMAGED,
                    format!("The archive cannot be read as far as this file: {error}."),
                ))
            }
        };
        if index != wanted.index as usize {
            continue;
        }
        let expected = entry.header().size().unwrap_or(0);
        let (seen, error) = drain(&mut entry);
        return Ok(match error {
            None if seen == expected && packing == Packing::Plain => verdict(CHECK_INTACT, "All of its bytes are there. A tar keeps no checksum for a file's data, so this cannot prove them right."),
            None if seen == expected => verdict(CHECK_INTACT, "All of its bytes unpack without an error. A tar keeps no checksum for a file's data, and the stream's checksum covers the whole archive, so this cannot prove this file right on its own."),
            None => verdict(CHECK_DAMAGED, format!("Only {seen} of its {expected} bytes are there.")),
            Some(error) => verdict(CHECK_DAMAGED, format!("It stops after {seen} of its {expected} bytes: {error}.")),
        });
    }
    Ok(verdict(CHECK_DAMAGED, "The archive ends before this file."))
}

fn check_seven_z(wanted: &ZipMember, total: u64, read: ReadRange) -> Result<ZipMemberCheck> {
    let mut reader =
        sevenz_rust2::ArchiveReader::new(Source::new(total, read), sevenz_rust2::Password::empty())
            .map_err(|error| anyhow::anyhow!("this 7z cannot be opened: {error}"))?;
    let mut outcome = None;
    // A solid 7z unpacks files in order, so each one before this is unpacked too.
    let result = reader.for_each_entries(|entry, data| {
        if entry.name != wanted.path {
            return Ok(true);
        }
        let (seen, error) = drain(data);
        outcome = Some(match error {
            None if entry.has_crc => verdict(
                CHECK_INTACT,
                format!("It unpacks whole, {seen} bytes, and its checksum agrees."),
            ),
            None => verdict(
                CHECK_INTACT,
                format!("It unpacks whole, {seen} bytes. The archive keeps no checksum for it."),
            ),
            Some(error) => verdict(
                CHECK_DAMAGED,
                format!("It stops after {seen} bytes: {error}."),
            ),
        });
        Ok(false)
    });
    Ok(match (outcome, result) {
        (Some(found), _) => found,
        (None, Err(error)) => verdict(
            CHECK_DAMAGED,
            format!("The archive cannot be unpacked as far as this file: {error}."),
        ),
        (None, Ok(())) => verdict(CHECK_DAMAGED, "The archive ends before this file."),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_are_known_by_name_and_by_bytes() {
        assert_eq!(
            format_by_name("Backup.TAR.GZ"),
            Some(Format::Tar(Packing::Gzip))
        );
        assert_eq!(format_by_name("a.tbz2"), Some(Format::Tar(Packing::Bzip2)));
        assert_eq!(
            format_by_name("notes.txt.xz"),
            Some(Format::Packed(Packing::Xz))
        );
        assert_eq!(format_by_name("photos.7z"), Some(Format::SevenZ));
        assert_eq!(format_by_name("book.epub"), Some(Format::Zip));
        assert_eq!(format_by_name("._hidden.zip"), None);
        assert_eq!(format_by_name("song.mp3"), None);
        assert_eq!(
            format_by_bytes(&[0x28, 0xB5, 0x2F, 0xFD, 0]),
            Some(Format::Packed(Packing::Zstd))
        );
        assert_eq!(
            format_by_bytes(b"7z\xBC\xAF\x27\x1C\0\x04"),
            Some(Format::SevenZ)
        );
    }

    #[test]
    fn dates_come_out_in_the_zip_form() {
        assert_eq!(unix_stamp(0), None);
        assert_eq!(
            unix_stamp(951_782_400).as_deref(),
            Some("2000-02-29T00:00:00")
        );
        assert_eq!(
            unix_stamp(1_700_000_000).as_deref(),
            Some("2023-11-14T22:13:20")
        );
    }

    #[test]
    fn a_compressed_file_is_named_after_itself() {
        assert_eq!(
            unpacked_name("Notes.TXT.gz", Format::Packed(Packing::Gzip)),
            "Notes.TXT"
        );
        assert_eq!(
            unpacked_name("gz", Format::Packed(Packing::Gzip)),
            "gz (unpacked)"
        );
    }
}
