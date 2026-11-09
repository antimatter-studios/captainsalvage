//! Walks a large film as it is read, piece by piece, so a file of many gigabytes is tested
//! without holding it in memory: MP4 and QuickTime box by box, AVI chunk by chunk down to each
//! video and audio frame inside its lists, and Matroska (MKV, WebM) element by element down to
//! each block inside its clusters, MPEG program streams pack by pack and packet by packet, FLV
//! tag by tag with each tag's back-pointer, and ASF (WMV) object by object and packet by
//! packet. It checks every header in turn, that nothing runs past the
//! end of what holds it, and that media data holds no long run of zeros, which is what
//! unwritten clusters read back as. A cluster overwritten with something else breaks the chain
//! of headers where it starts. Bytes are fed in order, once, so the same pass can hash the file.

use crate::verify::VerifyReport;

const BMFF_EXTENSIONS: &[&str] = &["mp4", "m4v", "m4a", "mov", "3gp", "3g2"];
const MATROSKA_EXTENSIONS: &[&str] = &["mkv", "webm", "mka", "mk3d"];
const MPEG_PS_EXTENSIONS: &[&str] = &["mpg", "mpeg", "vob", "m2p", "mpe"];
const ASF_EXTENSIONS: &[&str] = &["wmv", "asf", "wma"];
const MPEG_START_CODE: [u8; 3] = [0, 0, 1];
const MPEG_PACK: u8 = 0xBA;
const MPEG_END: u8 = 0xB9;
const MPEG_SYSTEM_HEADER: u8 = 0xBB;
const MPEG_PADDING_STREAM: u8 = 0xBE;
const MPEG1_PACK_BYTES: usize = 12;
const MPEG2_PACK_BYTES: usize = 14;
const MPEG_PACKET_HEADER_BYTES: usize = 6;
const FLV_FILE_HEADER_BYTES: usize = 9;
/// Each FLV tag follows the size of the one before it: 4 bytes, then an 11-byte tag header.
const FLV_TAG_HEADER_BYTES: usize = 4 + 11;
const FLV_TAG_HEADER_ONLY: u64 = 11;
const FLV_AUDIO: u8 = 8;
const FLV_VIDEO: u8 = 9;
const FLV_SCRIPT: u8 = 18;
const ASF_HEADER_OBJECT: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_DATA_OBJECT: [u8; 16] = [
    0x36, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];
const ASF_FILE_PROPERTIES: [u8; 16] = [
    0xA1, 0xDC, 0xAB, 0x8C, 0x47, 0xA9, 0xCF, 0x11, 0x8E, 0xE4, 0x00, 0xC0, 0x0C, 0x20, 0x53, 0x65,
];
const ASF_OBJECT_HEADER_BYTES: usize = 24;
/// The header object adds a count of its objects and two reserved bytes.
const ASF_HEADER_OBJECT_BYTES: usize = 30;
/// The data object adds a file id, a packet count and two reserved bytes.
const ASF_DATA_OBJECT_BYTES: usize = 50;
/// Enough of the file properties object to hold its minimum and maximum packet sizes.
const ASF_FILE_PROPERTIES_BYTES: usize = 104;
const ASF_MIN_PACKET_AT: usize = 92;
const ASF_MAX_PACKET_AT: usize = 96;
/// Where a packet's error-correction flags and data sit, which repeat in every packet.
const ASF_PACKET_START_BYTES: usize = 3;
/// How much of the start is looked at before choosing a walker: the bytes, not the name, say
/// what a file is, and a renamed file is common.
const SNIFF_BYTES: usize = 4096;
/// Bytes an MPEG program stream may have before its first pack, such as web headers left by a
/// download. Players scan past them to the first pack.
const MPEG_MAX_LEAD_IN: usize = SNIFF_BYTES;
/// Breaks recorded for one file. A stream that resyncs after each loss can lose many times.
const MAX_RECORDED_BREAKS: usize = 100;
const ASF_HEADER_CONTAINER: u32 = 1;
const ASF_DATA_CONTAINER: u32 = 2;
/// Clusters macOS never wrote read back as zeros, and compressed media does not hold this many
/// zero bytes in a row. The same limit as the in-memory walkers.
const MAX_ZERO_RUN: u64 = 32 * 1024;
const BMFF_HEADER_BYTES: usize = 8;
const BMFF_LARGE_HEADER_BYTES: usize = 16;
const RIFF_FORM_HEADER_BYTES: usize = 12;
const RIFF_CHUNK_HEADER_BYTES: usize = 8;
const RIFF_LIST_TYPE_BYTES: u64 = 4;

/// Matroska element ids, with their length marker kept, as the specification writes them.
const EBML_HEADER: u32 = 0x1A45_DFA3;
const SEGMENT: u32 = 0x1853_8067;
const CLUSTER: u32 = 0x1F43_B675;
const BLOCK_GROUP: u32 = 0xA0;
const SIMPLE_BLOCK: u32 = 0xA3;
const ENCRYPTED_BLOCK: u32 = 0xAF;
const VOID: u32 = 0xEC;
const CRC_32: u32 = 0xBF;
/// What a Segment holds: seek head, info, tracks, clusters, cues, attachments, chapters, tags.
const SEGMENT_CHILDREN: &[u32] = &[
    0x114D_9B74,
    0x1549_A966,
    0x1654_AE6B,
    CLUSTER,
    0x1C53_BB6B,
    0x1941_A469,
    0x1043_A770,
    0x1254_C367,
    VOID,
    CRC_32,
];
/// What a Cluster holds: timestamp, position, previous size, blocks and silent tracks.
const CLUSTER_CHILDREN: &[u32] = &[
    0xE7,
    0xA7,
    0xAB,
    SIMPLE_BLOCK,
    BLOCK_GROUP,
    ENCRYPTED_BLOCK,
    0x5854,
    VOID,
    CRC_32,
];
const TOP_LEVEL: &[u32] = &[EBML_HEADER, SEGMENT, VOID];
/// Elements holding media data, checked for runs of zeros.
const MEDIA_ELEMENTS: &[u32] = &[SIMPLE_BLOCK, BLOCK_GROUP, ENCRYPTED_BLOCK];

/// Whether a file of this name can be walked as it is read.
pub fn streams(name: &str) -> bool {
    kind_of(name).is_some()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bmff,
    Avi,
    Matroska,
    MpegPs,
    Flv,
    Asf,
}

fn kind_of(name: &str) -> Option<Kind> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if BMFF_EXTENSIONS.contains(&extension.as_str()) {
        Some(Kind::Bmff)
    } else if extension == "avi" {
        Some(Kind::Avi)
    } else if MATROSKA_EXTENSIONS.contains(&extension.as_str()) {
        Some(Kind::Matroska)
    } else if MPEG_PS_EXTENSIONS.contains(&extension.as_str()) {
        Some(Kind::MpegPs)
    } else if extension == "flv" {
        Some(Kind::Flv)
    } else if ASF_EXTENSIONS.contains(&extension.as_str()) {
        Some(Kind::Asf)
    } else {
        None
    }
}

fn is_four_cc(bytes: &[u8]) -> bool {
    bytes.len() == 4 && bytes.iter().all(|byte| (0x20..=0x7e).contains(byte))
}

/// How many bytes an EBML variable-length number takes, from its first byte. Zero is invalid.
fn vint_length(first: u8) -> usize {
    first.leading_zeros() as usize + 1
}

fn matroska_name(id: u32) -> &'static str {
    match id {
        EBML_HEADER => "EBML header",
        SEGMENT => "Segment",
        CLUSTER => "Cluster",
        _ => "element",
    }
}

/// An AVI list or a Matroska segment or cluster being walked inside. A Matroska element of
/// unknown size ends where its parent does, or, for a cluster, where the next one starts.
#[derive(Clone, Copy, Debug)]
struct Container {
    id: u32,
    end: Option<u64>,
}

/// A walk in progress. Feed it the file's bytes in order, then finish it.
pub struct StreamWalk {
    kind: Kind,
    total: u64,
    /// Bytes fed so far.
    at: u64,
    /// The header being gathered, while between boxes or chunks.
    header: Vec<u8>,
    /// Where the current box or chunk started, and where it ends.
    start: u64,
    end: u64,
    /// Inside a box or chunk whose data is checked for holes.
    checking: bool,
    /// Whether the current region is a box, chunk or element of its own, rather than a list's type.
    counts: bool,
    zero_run: u64,
    /// For AVI: where the current RIFF form ends, and how many forms there have been.
    form_end: u64,
    forms: u32,
    /// AVI lists and Matroska segments and clusters, innermost last.
    stack: Vec<Container>,
    /// Matroska elements, MPEG units, FLV tags and ASF objects opened so far.
    opened: u64,
    /// For FLV: what the next back-pointer must say, the size of the tag before it.
    flv_previous: u64,
    /// For ASF: the fixed packet size, and the flags byte every packet starts with.
    asf_packet: u64,
    asf_packet_flags: Option<u8>,
    /// The data is not the format its name says, such as raw MPEG video in a .mpg file, so
    /// this walker has nothing to say about it.
    unsupported: bool,
    /// For MPEG: where the walk lost the stream and is looking for the next pack, the last
    /// four bytes seen while looking, and how many bytes the losses cost.
    resync_from: Option<u64>,
    window: u32,
    skipped: u64,
    /// The first bytes, kept until there are enough to say what the file really is.
    head: Vec<u8>,
    sniffed: bool,
    has_movie: bool,
    report: VerifyReport,
    broken: bool,
}

impl StreamWalk {
    /// A walk for a file of this name and size, or None for a type that does not stream.
    pub fn new(name: &str, total: u64) -> Option<Self> {
        let kind = kind_of(name)?;
        Some(Self {
            kind,
            total,
            at: 0,
            header: Vec::new(),
            start: 0,
            end: 0,
            checking: false,
            counts: true,
            zero_run: 0,
            form_end: 0,
            forms: 0,
            stack: Vec::new(),
            opened: 0,
            flv_previous: 0,
            asf_packet: 0,
            asf_packet_flags: None,
            unsupported: false,
            resync_from: None,
            window: u32::MAX,
            skipped: 0,
            head: Vec::new(),
            sniffed: false,
            has_movie: false,
            report: VerifyReport {
                format: Self::format_name(kind).into(),
                ..VerifyReport::default()
            },
            broken: false,
        })
    }

    /// Whether valid bytes are counted by position: everything before a break, or all of it.
    fn counts_by_position(&self) -> bool {
        matches!(self.kind, Kind::Matroska | Kind::Flv | Kind::Asf)
    }

    fn fail(&mut self, offset: u64, note: String) {
        if self.counts_by_position() {
            // Everything before the break was walked; Matroska counts bytes by position.
            self.report.valid_bytes = offset;
        }
        self.report.breaks.push(offset);
        self.report.notes.push(note);
        self.broken = true;
    }

    fn header_wanted(&self) -> usize {
        match self.kind {
            Kind::Bmff => {
                if self.header.len() >= 4 && self.header[..4] == [0, 0, 0, 1] {
                    BMFF_LARGE_HEADER_BYTES
                } else {
                    BMFF_HEADER_BYTES
                }
            }
            Kind::Avi if self.stack.is_empty() && self.at >= self.form_end => {
                RIFF_FORM_HEADER_BYTES
            }
            Kind::Avi => RIFF_CHUNK_HEADER_BYTES,
            Kind::Matroska => {
                let Some(&first) = self.header.first() else {
                    return 1;
                };
                let id_length = vint_length(first);
                if first == 0 || id_length > 4 {
                    // Invalid: hand what there is to open, which says so.
                    return self.header.len();
                }
                if self.header.len() < id_length {
                    return id_length;
                }
                let Some(&size_first) = self.header.get(id_length) else {
                    return id_length + 1;
                };
                if size_first == 0 {
                    return self.header.len();
                }
                id_length + vint_length(size_first)
            }
            Kind::MpegPs => match self.header.get(3) {
                None => 4,
                Some(&MPEG_PACK) => match self.header.get(4) {
                    None => 5,
                    Some(byte) if byte & 0xC0 == 0x40 => MPEG2_PACK_BYTES,
                    Some(_) => MPEG1_PACK_BYTES,
                },
                Some(&MPEG_END) => 4,
                Some(_) => MPEG_PACKET_HEADER_BYTES,
            },
            Kind::Flv if self.opened == 0 => FLV_FILE_HEADER_BYTES,
            Kind::Flv => FLV_TAG_HEADER_BYTES,
            Kind::Asf => {
                if self
                    .stack
                    .last()
                    .is_some_and(|top| top.id == ASF_DATA_CONTAINER)
                {
                    return ASF_PACKET_START_BYTES;
                }
                if self.header.len() < 16 {
                    return 16;
                }
                let guid = &self.header[..16];
                if guid == ASF_HEADER_OBJECT {
                    ASF_HEADER_OBJECT_BYTES
                } else if guid == ASF_DATA_OBJECT {
                    ASF_DATA_OBJECT_BYTES
                } else if guid == ASF_FILE_PROPERTIES {
                    ASF_FILE_PROPERTIES_BYTES
                } else {
                    ASF_OBJECT_HEADER_BYTES
                }
            }
        }
    }

    /// Leaves every list or element whose end has been reached.
    fn close_finished(&mut self) {
        while let Some(top) = self.stack.last() {
            if top.end.is_some_and(|end| self.at >= end) {
                self.stack.pop();
            } else {
                break;
            }
        }
    }

    /// Where the innermost container ends: the first known end, else the end of the data.
    fn parent_end(&self) -> u64 {
        self.stack
            .iter()
            .rev()
            .find_map(|container| container.end)
            .unwrap_or(if self.kind == Kind::Avi && self.forms > 0 {
                self.form_end
            } else {
                self.total
            })
    }

    /// Takes in the next bytes of the file. The first few kilobytes are held until they say
    /// what the file is.
    pub fn feed(&mut self, data: &[u8]) {
        if self.sniffed {
            return self.walk(data);
        }
        self.head.extend_from_slice(data);
        if self.head.len() >= SNIFF_BYTES.min(self.total as usize) {
            self.sniff();
        }
    }

    /// Chooses the walker from the first bytes, falling back to the name, and walks them.
    fn sniff(&mut self) {
        self.sniffed = true;
        let head = std::mem::take(&mut self.head);
        let by_content = if head.get(4..8) == Some(b"ftyp".as_slice()) {
            Some(Kind::Bmff)
        } else if head.starts_with(b"RIFF") && head.get(8..12) == Some(b"AVI ".as_slice()) {
            Some(Kind::Avi)
        } else if head.starts_with(&[0x1A, 0x45, 0xDF, 0xA3]) {
            Some(Kind::Matroska)
        } else if head.starts_with(&[0, 0, 1, MPEG_PACK]) {
            Some(Kind::MpegPs)
        } else if head.starts_with(b"FLV\x01") {
            Some(Kind::Flv)
        } else if head.starts_with(&ASF_HEADER_OBJECT) {
            Some(Kind::Asf)
        } else {
            None
        };
        if let Some(kind) = by_content.filter(|kind| *kind != self.kind) {
            let note = format!(
                "The data is {}, whatever its name says.",
                Self::format_name(kind)
            );
            self.kind = kind;
            self.report.format = Self::format_name(kind).into();
            self.report.notes.push(note);
        }
        if self.kind == Kind::MpegPs && !head.starts_with(&[0, 0, 1, MPEG_PACK]) {
            let window = &head[..head.len().min(MPEG_MAX_LEAD_IN)];
            if let Some(first) = window
                .windows(4)
                .position(|bytes| bytes == [0, 0, 1, MPEG_PACK])
            {
                self.report.notes.push(format!(
                    "{first} bytes come before the first pack, which players skip."
                ));
                self.start = 0;
                self.end = first as u64;
                self.counts = false;
            }
        }
        self.walk(&head);
    }

    fn format_name(kind: Kind) -> &'static str {
        match kind {
            Kind::Bmff => "mp4",
            Kind::Avi => "avi",
            Kind::Matroska => "matroska",
            Kind::MpegPs => "mpeg-ps",
            Kind::Flv => "flv",
            Kind::Asf => "asf",
        }
    }

    fn walk(&mut self, mut data: &[u8]) {
        while !data.is_empty() && !self.broken {
            if let Some(from) = self.resync_from {
                // Looking for the next pack, as a player does after damage.
                let found = data.iter().position(|&byte| {
                    self.window = self.window << 8 | u32::from(byte);
                    self.window == u32::from_be_bytes([0, 0, 1, MPEG_PACK])
                });
                let Some(index) = found else {
                    self.at += data.len() as u64;
                    return;
                };
                self.at += index as u64 + 1;
                data = &data[index + 1..];
                self.skipped += self.at - 4 - from;
                self.resync_from = None;
                self.header = vec![0, 0, 1, MPEG_PACK];
                continue;
            }
            if self.at < self.end {
                // Inside a box or chunk: pass over its data, looking for holes.
                let take = ((self.end - self.at) as usize).min(data.len());
                if self.checking {
                    for (index, &byte) in data[..take].iter().enumerate() {
                        if byte == 0 {
                            self.zero_run += 1;
                            if self.zero_run == MAX_ZERO_RUN {
                                let hole = self.at + index as u64 + 1 - MAX_ZERO_RUN;
                                self.report.valid_bytes += hole - self.start;
                                self.fail(hole, format!("The media data turns to zeros at {hole}, as unwritten clusters do."));
                                return;
                            }
                        } else {
                            self.zero_run = 0;
                        }
                    }
                }
                self.at += take as u64;
                data = &data[take..];
                if self.at == self.end {
                    self.complete();
                }
                continue;
            }
            // Between boxes or chunks: gather the next header.
            if self.header.is_empty() {
                self.close_finished();
            }
            let wanted = self.header_wanted();
            let take = (wanted.saturating_sub(self.header.len())).min(data.len());
            self.header.extend_from_slice(&data[..take]);
            self.at += take as u64;
            data = &data[take..];
            if self.header.len() >= self.header_wanted() {
                let header = std::mem::take(&mut self.header);
                self.open(&header);
            }
        }
    }

    fn complete(&mut self) {
        if self.counts {
            self.report.units += 1;
        }
        self.report.valid_bytes += self.end - self.start;
        self.checking = false;
        self.counts = true;
        self.zero_run = 0;
    }

    fn open(&mut self, header: &[u8]) {
        let start = self.at - header.len() as u64;
        match self.kind {
            Kind::Bmff => self.open_box(header, start),
            Kind::Avi => self.open_chunk(header, start),
            Kind::Matroska => self.open_element(header, start),
            Kind::MpegPs => self.open_mpeg(header, start),
            Kind::Flv => self.open_tag(header, start),
            Kind::Asf => self.open_object(header, start),
        }
    }

    /// Stops walking a file that is not the format its name says, without calling it damaged.
    fn give_up(&mut self) {
        self.unsupported = true;
        self.broken = true;
    }

    /// Starts a region of data to pass over, from the end of the header to `end`.
    fn region(&mut self, start: u64, end: u64, checking: bool) {
        self.start = start;
        self.end = end;
        self.checking = checking;
        if end == self.at {
            self.complete();
        }
    }

    /// Notes where an MPEG stream was lost and looks for the next pack from there, re-reading
    /// the header bytes already taken after its first, as a player resyncs.
    fn lose_mpeg_sync(&mut self, start: u64, header: &[u8], note: String) {
        if self.report.breaks.len() < MAX_RECORDED_BREAKS {
            self.report.breaks.push(start);
            self.report.notes.push(note);
        }
        self.resync_from = Some(start);
        self.window = u32::MAX;
        self.at = start + 1;
        self.walk(&header[1..]);
    }

    fn open_mpeg(&mut self, header: &[u8], start: u64) {
        if header[..3] != MPEG_START_CODE {
            if self.opened == 0 {
                // No pack where the stream should start: look for the first one.
                return self.lose_mpeg_sync(
                    start,
                    header,
                    format!("No MPEG pack starts at {start}, as overwritten data would not; the walk resumes at the next pack."),
                );
            }
            return self.lose_mpeg_sync(
                start,
                header,
                format!("No MPEG start code at {start}, as overwritten data would not have; the walk resumes at the next pack."),
            );
        }
        let id = header[3];
        if self.opened == 0 && id != MPEG_PACK {
            // Raw MPEG video, with no packs to walk.
            return self.give_up();
        }
        self.opened += 1;
        match id {
            MPEG_PACK => {
                let stuffing = if header.len() == MPEG2_PACK_BYTES {
                    u64::from(header[13] & 0x07)
                } else {
                    0
                };
                self.region(start, self.at + stuffing, false);
            }
            // The program end: what follows, if anything, is padding.
            MPEG_END => self.region(start, self.total, false),
            MPEG_SYSTEM_HEADER..=0xFF => {
                let length = u64::from(u16::from_be_bytes([header[4], header[5]]));
                let end = self.at + length;
                if end > self.total {
                    // A packet is at most 64 KB, so this is the last one, cut short as
                    // recordings and downloads often are. Players stop there.
                    self.report.notes.push(format!("The last MPEG packet, at {start}, is cut short by {} bytes.", end - self.total));
                    return self.region(start, self.total, false);
                }
                self.region(start, end, id != MPEG_PADDING_STREAM);
            }
            _ => self.lose_mpeg_sync(
                start,
                header,
                format!("The start code at {start} names no program-stream unit, as overwritten data would not; the walk resumes at the next pack."),
            ),
        }
    }

    fn open_tag(&mut self, header: &[u8], start: u64) {
        if self.opened == 0 {
            if &header[..3] != b"FLV" {
                return self.fail(start, "The file does not start with an FLV header.".into());
            }
            let offset = u64::from(u32::from_be_bytes(
                header[5..9].try_into().expect("four bytes"),
            ));
            if offset < FLV_FILE_HEADER_BYTES as u64 || offset > self.total {
                return self.fail(
                    start,
                    format!("The FLV header says its data starts at {offset}, which cannot be."),
                );
            }
            self.opened += 1;
            self.counts = false;
            return self.region(start, start + offset, false);
        }
        let previous = u64::from(u32::from_be_bytes(
            header[..4].try_into().expect("four bytes"),
        ));
        if previous != self.flv_previous {
            return self.fail(
                start,
                format!("The tag at {start} follows a size of {previous}, where the tag before it was {}, as overwritten data would.", self.flv_previous),
            );
        }
        let kind = header[4] & 0x1F;
        let size = u64::from(header[5]) << 16 | u64::from(header[6]) << 8 | u64::from(header[7]);
        let stream = &header[12..15];
        if !matches!(kind, FLV_AUDIO | FLV_VIDEO | FLV_SCRIPT) || stream != [0, 0, 0] {
            return self.fail(
                start + 4,
                format!(
                    "No FLV tag starts at {}: its type or stream is not one a tag has.",
                    start + 4
                ),
            );
        }
        let end = self.at + size;
        if end > self.total {
            return self.fail(
                start + 4,
                format!(
                    "The tag at {} claims {size} bytes, past the end of the data.",
                    start + 4
                ),
            );
        }
        self.opened += 1;
        self.flv_previous = FLV_TAG_HEADER_ONLY + size;
        self.region(start, end, true);
    }

    fn open_object(&mut self, header: &[u8], start: u64) {
        if self
            .stack
            .last()
            .is_some_and(|top| top.id == ASF_DATA_CONTAINER)
        {
            let flags = header[0];
            let expected = *self.asf_packet_flags.get_or_insert(flags);
            if expected & 0x80 != 0 && (flags != expected || header[1..3] != [0, 0]) {
                return self.fail(
                    start,
                    format!("The ASF packet at {start} does not start like the others, as overwritten data would not."),
                );
            }
            // A last stretch shorter than a packet is padding up to the end of the data object.
            let end = (start + self.asf_packet).min(self.parent_end());
            return self.region(start, end, false);
        }
        let guid = &header[..16];
        let size = u64::from_le_bytes(header[16..24].try_into().expect("eight bytes"));
        if self.opened == 0 && guid != ASF_HEADER_OBJECT {
            return self.fail(
                start,
                "The file does not start with an ASF header object.".into(),
            );
        }
        if size < header.len() as u64 || start + size > self.parent_end() {
            return self.fail(
                start,
                format!("The ASF object at {start} claims {size} bytes, which does not fit what holds it."),
            );
        }
        self.opened += 1;
        let end = start + size;
        if guid == ASF_HEADER_OBJECT {
            self.stack.push(Container {
                id: ASF_HEADER_CONTAINER,
                end: Some(end),
            });
            return self.region(start, self.at, false);
        }
        if guid == ASF_FILE_PROPERTIES {
            let at = |offset: usize| {
                u64::from(u32::from_le_bytes(
                    header[offset..offset + 4].try_into().expect("four bytes"),
                ))
            };
            let (min, max) = (at(ASF_MIN_PACKET_AT), at(ASF_MAX_PACKET_AT));
            if min == max && min as usize >= ASF_PACKET_START_BYTES {
                self.asf_packet = min;
            }
        }
        if guid == ASF_DATA_OBJECT && self.asf_packet > 0 {
            self.stack.push(Container {
                id: ASF_DATA_CONTAINER,
                end: Some(end),
            });
            self.has_movie = true;
            return self.region(start, self.at, false);
        }
        self.has_movie |= guid == ASF_DATA_OBJECT;
        self.region(start, end, guid == ASF_DATA_OBJECT)
    }

    fn open_box(&mut self, header: &[u8], start: u64) {
        let kind = &header[4..8];
        if !is_four_cc(kind) {
            return self.fail(
                start,
                format!("No box starts at {start}: its type is not four letters."),
            );
        }
        let size = u64::from(u32::from_be_bytes(
            header[..4].try_into().expect("four bytes"),
        ));
        let length = match size {
            0 => self.total - start,
            1 => u64::from_be_bytes(header[8..16].try_into().expect("eight bytes")),
            size => size,
        };
        if length < header.len() as u64 {
            return self.fail(
                start,
                format!("The box at {start} is smaller than its own header."),
            );
        }
        if start + length > self.total {
            return self.fail(
                start,
                format!(
                    "The {} box at {start} claims {length} bytes, past the end of the data.",
                    String::from_utf8_lossy(kind)
                ),
            );
        }
        self.has_movie |= kind == b"moov";
        self.checking = kind == b"mdat";
        self.start = start;
        self.end = start + length;
    }

    fn open_chunk(&mut self, header: &[u8], start: u64) {
        if self.stack.is_empty() && start >= self.form_end {
            let tag = &header[..4];
            if tag == b"JUNK" && self.forms > 0 {
                // Padding after the last form, which writers leave to round the file up.
                let length = u64::from(u32::from_le_bytes(
                    header[4..8].try_into().expect("four bytes"),
                ));
                let end = start + RIFF_CHUNK_HEADER_BYTES as u64 + length + (length & 1);
                if end > self.total || end < self.at {
                    return self.fail(
                        start,
                        format!("The padding after the last RIFF form at {start} does not fit the data."),
                    );
                }
                self.start = start;
                self.end = end;
                self.counts = false;
                if end == self.at {
                    self.complete();
                }
                return;
            }
            let expected: &[u8] = if self.forms == 0 { b"AVI " } else { b"AVIX" };
            if tag != b"RIFF" || &header[8..12] != expected {
                return self.fail(
                    start,
                    format!(
                        "No RIFF {} form starts at {start}.",
                        String::from_utf8_lossy(expected)
                    ),
                );
            }
            let size = u64::from(u32::from_le_bytes(
                header[4..8].try_into().expect("four bytes"),
            ));
            self.form_end = start + RIFF_CHUNK_HEADER_BYTES as u64 + size;
            if self.form_end > self.total {
                return self.fail(
                    start,
                    format!(
                        "The RIFF form at {start} claims {size} bytes, past the end of the data."
                    ),
                );
            }
            self.forms += 1;
            self.report.valid_bytes += RIFF_FORM_HEADER_BYTES as u64;
            self.start = self.at;
            self.end = self.at;
            return;
        }
        let id = &header[..4];
        if !is_four_cc(id) {
            return self.fail(
                start,
                format!("No chunk starts at {start}: its id is not four letters, as overwritten data would be."),
            );
        }
        let length = u64::from(u32::from_le_bytes(
            header[4..8].try_into().expect("four bytes"),
        ));
        let end = start + RIFF_CHUNK_HEADER_BYTES as u64 + length + (length & 1);
        let parent_end = self.parent_end();
        if end > parent_end {
            return self.fail(
                start,
                format!(
                    "The {} chunk at {start} claims {length} bytes, past the end of what holds it.",
                    String::from_utf8_lossy(id)
                ),
            );
        }
        self.start = start;
        if id == b"LIST" {
            if length < RIFF_LIST_TYPE_BYTES {
                return self.fail(
                    start,
                    format!("The list at {start} is too small to say what it holds."),
                );
            }
            // Walk inside: its type comes next, then its own chunks.
            self.stack.push(Container {
                id: 0,
                end: Some(end),
            });
            self.end = self.at + RIFF_LIST_TYPE_BYTES;
            self.counts = false;
            return;
        }
        // JUNK chunks are padding, and zeros are what they hold.
        self.checking = id != b"JUNK";
        self.end = end;
        if end == self.at {
            self.complete();
        }
    }

    fn open_element(&mut self, header: &[u8], start: u64) {
        let id_length = vint_length(header[0]);
        if header[0] == 0 || id_length > 4 || header.len() <= id_length {
            return self.fail(
                start,
                format!("No element starts at {start}: its id is not valid, as overwritten data would be."),
            );
        }
        let id = header[..id_length]
            .iter()
            .fold(0u32, |value, &byte| value << 8 | u32::from(byte));
        let size_bytes = &header[id_length..];
        let size_length = vint_length(size_bytes[0]);
        if size_bytes[0] == 0 || size_length > 8 || size_bytes.len() != size_length {
            return self.fail(
                start,
                format!("The element at {start} has no valid size, as overwritten data would."),
            );
        }
        let marker_bits = 0xFFu8.checked_shr(size_length as u32).unwrap_or(0);
        let size = size_bytes[1..]
            .iter()
            .fold(u64::from(size_bytes[0] & marker_bits), |value, &byte| {
                value << 8 | u64::from(byte)
            });
        let unknown = size == (1u64 << (7 * size_length)) - 1;

        // A cluster of unknown size ends where the next part of the segment begins.
        if let Some(top) = self.stack.last() {
            if top.id == CLUSTER && top.end.is_none() && SEGMENT_CHILDREN.contains(&id) {
                self.stack.pop();
            }
        }
        let parent = self.stack.last().map(|container| container.id);
        let allowed = match parent {
            None if self.opened == 0 => id == EBML_HEADER,
            None => TOP_LEVEL.contains(&id),
            Some(SEGMENT) => SEGMENT_CHILDREN.contains(&id),
            Some(CLUSTER) => CLUSTER_CHILDREN.contains(&id),
            Some(_) => true,
        };
        if !allowed {
            let place = parent.map_or("the top of the file", matroska_name);
            return self.fail(
                start,
                format!("The element at {start} (id {id:#x}) does not belong in {place}, as overwritten data would not."),
            );
        }
        self.opened += 1;
        let parent_end = self.parent_end();
        let end = (!unknown).then_some(start + header.len() as u64 + size);
        if let Some(end) = end {
            if end > parent_end {
                return self.fail(
                    start,
                    format!(
                        "The {} at {start} claims {size} bytes, past the end of what holds it.",
                        matroska_name(id)
                    ),
                );
            }
        }
        self.start = start;
        if id == SEGMENT || id == CLUSTER {
            self.has_movie |= id == SEGMENT;
            self.stack.push(Container { id, end });
            self.end = self.at;
            return;
        }
        let Some(end) = end else {
            return self.fail(
                start,
                format!("The element at {start} has an unknown size, which only a segment or cluster may."),
            );
        };
        self.checking = MEDIA_ELEMENTS.contains(&id);
        self.end = end;
        if end == self.at {
            self.complete();
        }
    }

    /// The report, once every byte has been fed, or as far as the bytes went.
    pub fn finish(mut self) -> VerifyReport {
        if !self.sniffed {
            self.sniff();
        }
        if self.kind == Kind::MpegPs && !self.unsupported {
            if let Some(from) = self.resync_from.take() {
                self.skipped += self.at - from;
            }
            if !self.header.is_empty() && self.at == self.total {
                self.report
                    .notes
                    .push("The stream stops partway through its last header.".into());
                self.skipped += self.header.len() as u64;
                self.header.clear();
            }
        }
        if self.unsupported {
            self.report.format = "unknown".into();
            self.report.notes.push("The data is not the format its name says, so this walker has nothing to say about it.".into());
            self.report.bytes_examined = self.total;
            return self.report;
        }
        // An FLV ends with the size of its last tag, a header with nothing after it.
        if self.kind == Kind::Flv && !self.broken && self.header.len() == 4 && self.at == self.total
        {
            let previous = u64::from(u32::from_be_bytes(
                self.header[..4].try_into().expect("four bytes"),
            ));
            if previous == self.flv_previous {
                self.header.clear();
            }
        }
        if !self.broken {
            if self.at < self.total || self.at < self.end || !self.header.is_empty() {
                let at = self.at;
                self.fail(
                    at,
                    format!("The data ends at {at}, before its last box or chunk."),
                );
            } else {
                self.report.reached_end = true;
            }
        }
        if self.kind == Kind::Bmff && !self.has_movie && !self.broken {
            self.report.notes.push("There is no moov box, which says how to play the media, so it will not play as it is.".into());
        }
        if self.counts_by_position() && !self.broken {
            self.report.valid_bytes = self.at.min(self.total);
        }
        if self.kind == Kind::MpegPs && !self.broken {
            self.report.valid_bytes = self.at.min(self.total).saturating_sub(self.skipped);
        }
        if self.kind == Kind::Matroska && !self.broken && !self.has_movie {
            self.report.notes.push(
                "There is no Segment, which holds the media, so it will not play as it is.".into(),
            );
        }
        self.report.bytes_examined = self.total;
        self.report.valid_fraction = if self.total == 0 {
            0.0
        } else {
            self.report.valid_bytes as f64 / self.total as f64
        };
        self.report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bmff_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut out = ((body.len() + 8) as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(body);
        out
    }

    fn walk_in_pieces(name: &str, data: &[u8], piece: usize) -> VerifyReport {
        let mut walk = StreamWalk::new(name, data.len() as u64).expect("streams");
        for chunk in data.chunks(piece) {
            walk.feed(chunk);
        }
        walk.finish()
    }

    #[test]
    fn a_film_walked_in_pieces_agrees_with_the_whole_walk() {
        let mut file = bmff_box(b"ftyp", b"isom\0\0\x02\0isomiso2");
        file.extend(bmff_box(b"moov", &[1u8; 40]));
        file.extend(bmff_box(
            b"mdat",
            &(0..50_000).map(|n| (n % 251 + 1) as u8).collect::<Vec<_>>(),
        ));
        for piece in [1, 7, 4096, file.len()] {
            let streamed = walk_in_pieces("film.mp4", &file, piece);
            let whole = crate::verify::verify_bmff(&file);
            assert_eq!(
                (
                    streamed.units,
                    streamed.reached_end,
                    streamed.valid_bytes,
                    streamed.breaks.clone()
                ),
                (
                    whole.units,
                    whole.reached_end,
                    whole.valid_bytes,
                    whole.breaks.clone()
                ),
                "pieces of {piece}"
            );
        }
        let mut holed = file.clone();
        let mdat_data = file.len() - 50_000;
        holed[mdat_data + 1000..mdat_data + 1000 + MAX_ZERO_RUN as usize].fill(0);
        let report = walk_in_pieces("film.mov", &holed, 3000);
        assert_eq!(report.breaks, vec![(mdat_data + 1000) as u64]);

        let cut = &file[..file.len() - 10];
        let mut walk = StreamWalk::new("film.mp4", file.len() as u64).unwrap();
        walk.feed(cut);
        assert!(
            !walk.finish().reached_end,
            "a read that stops short is not whole"
        );
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

    fn avi_with(frames: &[u8], tail: &[u8]) -> Vec<u8> {
        let mut body = b"AVI ".to_vec();
        body.extend(riff_chunk(
            b"LIST",
            &[b"hdrl".as_slice(), &riff_chunk(b"avih", &[7u8; 56])].concat(),
        ));
        body.extend(riff_chunk(b"JUNK", &vec![0u8; MAX_ZERO_RUN as usize + 100]));
        body.extend(riff_chunk(b"LIST", &[b"movi".as_slice(), frames].concat()));
        body.extend(riff_chunk(b"idx1", &[1u8; 32]));
        [
            b"RIFF".as_slice(),
            &(body.len() as u32).to_le_bytes(),
            &body,
            tail,
        ]
        .concat()
    }

    #[test]
    fn an_avi_walked_in_pieces_skips_padding_and_finds_holes() {
        let frames = [
            riff_chunk(b"00dc", &[5u8; 3001]),
            riff_chunk(b"01wb", &[6u8; 500]),
        ]
        .concat();
        let file = avi_with(&frames, &[]);
        let report = walk_in_pieces("clip.avi", &file, 999);
        assert!(report.breaks.is_empty(), "{:?}", report.notes);
        assert!(report.reached_end);
        assert_eq!(report.units, 5, "avih, JUNK, two frames, idx1");

        // Padding after the last form, as writers leave it, is not damage.
        let padded = avi_with(&frames, &riff_chunk(b"JUNK", &[0u8; 1648]));
        let report = walk_in_pieces("clip.avi", &padded, 700);
        assert!(
            report.breaks.is_empty() && report.reached_end,
            "{:?}",
            report.notes
        );
        // Anything else after it is.
        let trailing = avi_with(&frames, b"garbage-after-the-form");
        assert!(!walk_in_pieces("clip.avi", &trailing, 700).breaks.is_empty());

        // A frame's header overwritten inside the movi list breaks the walk exactly there.
        let second = file.len() - (8 + 32) - (8 + 500);
        let mut overwritten = file.clone();
        overwritten[second..second + 8]
            .copy_from_slice(&[0xc6, 0x41, 0xd5, 0x98, 0xda, 0x7f, 0xa3, 0x18]);
        assert_eq!(
            walk_in_pieces("clip.avi", &overwritten, 333).breaks,
            vec![second as u64]
        );
        // A frame whose size runs past its list is caught too.
        let mut too_long = file.clone();
        too_long[second + 4..second + 8].copy_from_slice(&900_000u32.to_le_bytes());
        assert!(!walk_in_pieces("clip.avi", &too_long, 333).breaks.is_empty());

        let mut holed = file.clone();
        let first = second - (8 + 3002) + 8;
        holed[first..first + 3001].fill(0);
        // A short chunk of zeros is not a hole; only a long run is.
        assert!(walk_in_pieces("clip.avi", &holed, 500).breaks.is_empty());
        let mut cut = holed.clone();
        cut.truncate(first + 3001);
        assert!(!walk_in_pieces("clip.avi", &cut, 500).reached_end);
    }

    /// One Matroska element: its id bytes, a one-byte size and the body.
    fn ebml(id: &[u8], body: &[u8]) -> Vec<u8> {
        assert!(body.len() < 0x7f);
        [id, &[0x80 | body.len() as u8], body].concat()
    }

    /// An element of eight-byte size, so bodies of any length fit.
    fn ebml_long(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut size = (body.len() as u64).to_be_bytes();
        size[0] = 0x01;
        [id, &size, body].concat()
    }

    fn matroska_file(cluster_size_unknown: bool, block: &[u8]) -> Vec<u8> {
        let header = ebml(&[0x1A, 0x45, 0xDF, 0xA3], &ebml(&[0x42, 0x82], b"matroska"));
        let info = ebml(
            &[0x15, 0x49, 0xA9, 0x66],
            &ebml(&[0x2A, 0xD7, 0xB1], &[0x0F, 0x42, 0x40]),
        );
        let blocks = [
            ebml(&[0xE7], &[0x00]),
            ebml_long(&[0xA3], block),
            ebml_long(&[0xA3], block),
        ]
        .concat();
        let cluster = |body: &[u8]| {
            if cluster_size_unknown {
                [&[0x1F, 0x43, 0xB6, 0x75, 0xFF][..], body].concat()
            } else {
                ebml_long(&[0x1F, 0x43, 0xB6, 0x75], body)
            }
        };
        let segment_body = [info, cluster(&blocks), cluster(&blocks)].concat();
        [header, ebml_long(&[0x18, 0x53, 0x80, 0x67], &segment_body)].concat()
    }

    #[test]
    fn a_matroska_film_is_walked_block_by_block() {
        let block: Vec<u8> = (0..5000).map(|n| (n % 251 + 1) as u8).collect();
        for unknown in [false, true] {
            let file = matroska_file(unknown, &block);
            for piece in [1, 13, 4096, file.len()] {
                let report = walk_in_pieces("film.mkv", &file, piece);
                assert!(
                    report.breaks.is_empty(),
                    "{unknown} {piece}: {:?}",
                    report.notes
                );
                assert!(report.reached_end);
                assert_eq!(report.valid_bytes, file.len() as u64);
            }
            assert_eq!(
                crate::verify::verify_stream("film.webm", &file).format,
                "matroska"
            );
        }

        // An overwritten block header inside a cluster breaks the walk where it is.
        let file = matroska_file(false, &block);
        let second_block = file.len() - (block.len() + 9);
        let mut overwritten = file.clone();
        overwritten[second_block] = 0x00;
        assert_eq!(
            walk_in_pieces("film.mkv", &overwritten, 777).breaks,
            vec![second_block as u64]
        );
        let mut stray = file.clone();
        stray[second_block] = 0x86; // a real Matroska id, but not one a cluster holds
        assert_eq!(
            walk_in_pieces("film.mkv", &stray, 777).breaks,
            vec![second_block as u64]
        );
        let mut too_long = file.clone();
        too_long[second_block + 8] = 0xFF;
        assert!(!walk_in_pieces("film.mkv", &too_long, 777).breaks.is_empty());

        // A long run of zeros inside a block is unwritten clusters; a short one is not.
        let big: Vec<u8> = (0..40_000).map(|n| (n % 251 + 1) as u8).collect();
        let large = matroska_file(false, &big);
        let mut zeroed = large.clone();
        let from = large.len() - 35_000;
        zeroed[from..from + 1000].fill(0);
        assert!(walk_in_pieces("film.mkv", &zeroed, 1000).breaks.is_empty());
        zeroed[from..from + MAX_ZERO_RUN as usize].fill(0);
        assert_eq!(
            walk_in_pieces("film.mkv", &zeroed, 1000).breaks,
            vec![from as u64]
        );

        // A file that does not start with an EBML header is not Matroska.
        assert!(!walk_in_pieces("film.mkv", &file[10..], 500)
            .breaks
            .is_empty());
    }

    fn mpeg_pack() -> Vec<u8> {
        let mut pack = vec![0, 0, 1, 0xBA, 0x44, 0, 4, 0, 4, 1, 0x01, 0x89, 0xC3, 0xF8];
        pack.extend_from_slice(&[]);
        pack
    }

    fn mpeg_packet(id: u8, body: &[u8]) -> Vec<u8> {
        [&[0, 0, 1, id][..], &(body.len() as u16).to_be_bytes(), body].concat()
    }

    fn mpeg_file(packs: usize) -> Vec<u8> {
        let mut file = Vec::new();
        for n in 0..packs {
            file.extend(mpeg_pack());
            if n == 0 {
                file.extend(mpeg_packet(0xBB, &[0x80; 12]));
            }
            file.extend(mpeg_packet(
                0xE0,
                &(0..2000).map(|k| (k % 251 + 1) as u8).collect::<Vec<_>>(),
            ));
            file.extend(mpeg_packet(0xC0, &[0x33; 400]));
            file.extend(mpeg_packet(0xBE, &[0xFF; 100]));
        }
        file.extend_from_slice(&[0, 0, 1, 0xB9]);
        file.extend_from_slice(&[0; 300]);
        file
    }

    #[test]
    fn an_mpeg_program_stream_is_walked_packet_by_packet() {
        let file = mpeg_file(20);
        for piece in [1, 7, 1000, file.len()] {
            let report = walk_in_pieces("film.mpg", &file, piece);
            assert!(
                report.breaks.is_empty() && report.reached_end,
                "{piece}: {:?}",
                report.notes
            );
        }
        assert_eq!(
            crate::verify::verify_stream("clip.mpeg", &file).format,
            "mpeg-ps"
        );
        // An overwritten packet header breaks the walk where it is.
        let second_pack = mpeg_pack().len() + 18 + 2006 + 406 + 106;
        let mut overwritten = file.clone();
        overwritten[second_pack..second_pack + 4].copy_from_slice(&[0x9A, 0x13, 0x77, 0x02]);
        assert_eq!(
            walk_in_pieces("film.mpg", &overwritten, 500).breaks,
            vec![second_pack as u64]
        );
        // A foreign 4 KB block in the middle, as the volume's NTFS past left: the walk loses the
        // stream there, picks it up at the next pack, and counts only what was lost.
        let mut holed = file.clone();
        let block = 4096 * 3;
        holed[block..block + 4096].copy_from_slice(&[0x5Au8; 4096]);
        for piece in [1, 333, holed.len()] {
            let report = walk_in_pieces("film.mpg", &holed, piece);
            assert_eq!(report.breaks.len(), 1, "{piece}: {:?}", report.notes);
            assert!(report.reached_end);
            assert!(
                report.valid_fraction > 0.85 && report.valid_fraction < 1.0,
                "{}",
                report.valid_fraction
            );
        }
        // A stream cut short partway through its last packet still reaches its end.
        let last_packet = file.len() - 4 - 300 - 106 - 406 - 2006;
        let cut = &file[..last_packet + 1000];
        let report = walk_in_pieces("film.mpg", cut, 500);
        assert!(
            report.breaks.is_empty() && report.reached_end,
            "{:?}",
            report.notes
        );
        assert!(report.notes.iter().any(|note| note.contains("cut short")));
        // Web headers before the first pack are skipped, as players do.
        let downloaded = [b"Content-type: video/mpeg\r\n\r\n".as_slice(), &file].concat();
        let report = walk_in_pieces("film.mpg", &downloaded, 333);
        assert!(
            report.breaks.is_empty() && report.reached_end,
            "{:?}",
            report.notes
        );
        // A WMV named .mpg is walked as what it is.
        let wmv = asf_file(5, 1000);
        let report = walk_in_pieces("film.mpg", &wmv, 400);
        assert_eq!(report.format, "asf");
        assert!(
            report.breaks.is_empty() && report.reached_end,
            "{:?}",
            report.notes
        );
        // Raw MPEG video has no packs: the walker says nothing rather than calling it damaged.
        let raw = [&[0u8, 0, 1, 0xB3][..], &[7u8; 5000]].concat();
        let report = walk_in_pieces("film.mpg", &raw, 999);
        assert_eq!(report.format, "unknown");
        assert!(report.breaks.is_empty());
    }

    fn flv_file(tags: usize) -> Vec<u8> {
        let mut file = b"FLV\x01\x05\x00\x00\x00\x09".to_vec();
        let mut previous = 0u32;
        for n in 0..tags {
            let body: Vec<u8> = (0..(300 + n * 7)).map(|k| (k % 249 + 1) as u8).collect();
            file.extend_from_slice(&previous.to_be_bytes());
            let kind = if n % 2 == 0 { 9 } else { 8 };
            let size = body.len() as u32;
            file.push(kind);
            file.extend_from_slice(&size.to_be_bytes()[1..]);
            file.extend_from_slice(&[0, 0, n as u8, 0, 0, 0, 0]);
            file.extend_from_slice(&body);
            previous = 11 + size;
        }
        file.extend_from_slice(&previous.to_be_bytes());
        file
    }

    #[test]
    fn an_flv_is_walked_tag_by_tag() {
        let file = flv_file(40);
        for piece in [1, 13, 4096, file.len()] {
            let report = walk_in_pieces("clip.flv", &file, piece);
            assert!(
                report.breaks.is_empty() && report.reached_end,
                "{piece}: {:?}",
                report.notes
            );
            assert_eq!(report.units, 40);
        }
        assert_eq!(
            crate::verify::verify_stream("clip.flv", &file).format,
            "flv"
        );
        // A tag whose back-pointer is wrong, as a foreign cluster's bytes would be, breaks there.
        let tenth = 9 + (0..9).map(|n| 4 + 11 + 300 + n * 7).sum::<usize>();
        let mut overwritten = file.clone();
        overwritten[tenth] ^= 0x40;
        assert_eq!(
            walk_in_pieces("clip.flv", &overwritten, 700).breaks,
            vec![tenth as u64]
        );
        let cut = &file[..file.len() - 100];
        assert!(!walk_in_pieces("clip.flv", cut, 700).reached_end);
    }

    fn asf_file(packets: usize, packet: usize) -> Vec<u8> {
        let mut properties = ASF_FILE_PROPERTIES.to_vec();
        properties.extend_from_slice(&104u64.to_le_bytes());
        properties.resize(104, 0);
        properties[ASF_MIN_PACKET_AT..ASF_MIN_PACKET_AT + 4]
            .copy_from_slice(&(packet as u32).to_le_bytes());
        properties[ASF_MAX_PACKET_AT..ASF_MAX_PACKET_AT + 4]
            .copy_from_slice(&(packet as u32).to_le_bytes());
        let header_size = 30 + properties.len();
        let mut file = ASF_HEADER_OBJECT.to_vec();
        file.extend_from_slice(&(header_size as u64).to_le_bytes());
        file.extend_from_slice(&[1, 0, 0, 0, 1, 2]);
        file.extend(properties);
        let data_size = 50 + packets * packet;
        file.extend_from_slice(&ASF_DATA_OBJECT);
        file.extend_from_slice(&(data_size as u64).to_le_bytes());
        file.extend_from_slice(&[0; 26]);
        for n in 0..packets {
            file.extend_from_slice(&[0x82, 0, 0]);
            file.extend((0..packet - 3).map(|k| ((k + n) % 251 + 1) as u8));
        }
        let mut index = vec![
            0x90, 0x08, 0x00, 0x33, 0xB1, 0xE5, 0xCF, 0x11, 0x89, 0xF4, 0x00, 0xA0, 0xC9, 0x03,
            0x49, 0xCB,
        ];
        index.extend_from_slice(&40u64.to_le_bytes());
        index.extend_from_slice(&[0; 16]);
        file.extend(index);
        file
    }

    #[test]
    fn an_asf_is_walked_object_by_object_and_packet_by_packet() {
        let file = asf_file(30, 3200);
        for piece in [1, 11, 4096, file.len()] {
            let report = walk_in_pieces("clip.wmv", &file, piece);
            assert!(
                report.breaks.is_empty() && report.reached_end,
                "{piece}: {:?}",
                report.notes
            );
        }
        assert_eq!(
            crate::verify::verify_stream("clip.asf", &file).format,
            "asf"
        );
        // A packet that no longer starts like the others is where a foreign cluster begins.
        let header = 30 + 104 + 50;
        let twelfth = header + 11 * 3200;
        let mut overwritten = file.clone();
        overwritten[twelfth] = 0x13;
        assert_eq!(
            walk_in_pieces("clip.wmv", &overwritten, 1000).breaks,
            vec![twelfth as u64]
        );
    }
}
