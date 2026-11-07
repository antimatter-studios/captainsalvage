//! Walks a large MP4, QuickTime or AVI file as it is read, piece by piece, so a film of many
//! gigabytes is tested without holding it in memory. It checks the same things as the
//! in-memory walkers in `verify`: every box or chunk header in turn, that none runs past the
//! end, and that the media data holds no long run of zeros, which is what unwritten clusters
//! read back as. Bytes are fed in order, once, so the same pass can hash the file.

use crate::verify::VerifyReport;

const BMFF_EXTENSIONS: &[&str] = &["mp4", "m4v", "m4a", "mov", "3gp", "3g2"];
/// Clusters macOS never wrote read back as zeros, and compressed media does not hold this many
/// zero bytes in a row. The same limit as the in-memory walkers.
const MAX_ZERO_RUN: u64 = 32 * 1024;
const BMFF_HEADER_BYTES: usize = 8;
const BMFF_LARGE_HEADER_BYTES: usize = 16;
const RIFF_FORM_HEADER_BYTES: usize = 12;
const RIFF_CHUNK_HEADER_BYTES: usize = 8;

/// Whether a file of this name can be walked as it is read.
pub fn streams(name: &str) -> bool {
    kind_of(name).is_some()
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Kind {
    Bmff,
    Avi,
}

fn kind_of(name: &str) -> Option<Kind> {
    let extension = name.rsplit_once('.')?.1.to_ascii_lowercase();
    if BMFF_EXTENSIONS.contains(&extension.as_str()) {
        Some(Kind::Bmff)
    } else if extension == "avi" {
        Some(Kind::Avi)
    } else {
        None
    }
}

fn is_four_cc(bytes: &[u8]) -> bool {
    bytes.len() == 4 && bytes.iter().all(|byte| (0x20..=0x7e).contains(byte))
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
    zero_run: u64,
    /// For AVI: where the current RIFF form ends, and whether the next header is a form's.
    form_end: u64,
    forms: u32,
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
            zero_run: 0,
            form_end: 0,
            forms: 0,
            has_movie: false,
            report: VerifyReport {
                format: if kind == Kind::Bmff { "mp4" } else { "avi" }.into(),
                ..VerifyReport::default()
            },
            broken: false,
        })
    }

    fn fail(&mut self, offset: u64, note: String) {
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
            Kind::Avi if self.at >= self.form_end => RIFF_FORM_HEADER_BYTES,
            Kind::Avi => RIFF_CHUNK_HEADER_BYTES,
        }
    }

    /// Takes in the next bytes of the file.
    pub fn feed(&mut self, mut data: &[u8]) {
        while !data.is_empty() && !self.broken {
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
                    self.report.units += 1;
                    self.report.valid_bytes += self.end - self.start;
                    self.checking = false;
                    self.zero_run = 0;
                }
                continue;
            }
            // Between boxes or chunks: gather the next header.
            let wanted = self.header_wanted();
            let take = (wanted - self.header.len()).min(data.len());
            self.header.extend_from_slice(&data[..take]);
            self.at += take as u64;
            data = &data[take..];
            if self.header.len() == self.header_wanted() {
                let header = std::mem::take(&mut self.header);
                self.open(&header);
            }
        }
    }

    fn open(&mut self, header: &[u8]) {
        let start = self.at - header.len() as u64;
        match self.kind {
            Kind::Bmff => {
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
            Kind::Avi => {
                if self.at - header.len() as u64 >= self.form_end {
                    let expected: &[u8] = if self.forms == 0 { b"AVI " } else { b"AVIX" };
                    if &header[..4] != b"RIFF" || &header[8..12] != expected {
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
                        return self.fail(start, format!("The RIFF form at {start} claims {size} bytes, past the end of the data."));
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
                        format!("No chunk starts at {start}: its id is not four letters."),
                    );
                }
                let length = u64::from(u32::from_le_bytes(
                    header[4..8].try_into().expect("four bytes"),
                ));
                let end = start + RIFF_CHUNK_HEADER_BYTES as u64 + length + (length & 1);
                if end > self.form_end {
                    return self.fail(
                        start,
                        format!(
                        "The {} chunk at {start} claims {length} bytes, past the end of its form.",
                        String::from_utf8_lossy(id)
                    ),
                    );
                }
                // JUNK chunks are padding, and zeros are what they hold.
                self.checking = id != b"JUNK";
                self.start = start;
                self.end = end;
                if end == self.at {
                    self.report.units += 1;
                    self.report.valid_bytes += end - start;
                }
            }
        }
    }

    /// The report, once every byte has been fed, or as far as the bytes went.
    pub fn finish(mut self) -> VerifyReport {
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

    #[test]
    fn an_avi_walked_in_pieces_skips_padding_and_finds_holes() {
        let mut body = b"AVI ".to_vec();
        body.extend(riff_chunk(b"LIST", b"hdrlavih0123"));
        body.extend(riff_chunk(b"JUNK", &vec![0u8; MAX_ZERO_RUN as usize + 100]));
        body.extend(riff_chunk(b"00dc", &[5u8; 3001]));
        body.extend(riff_chunk(b"idx1", &[1u8; 32]));
        let file = [
            b"RIFF".as_slice(),
            &(body.len() as u32).to_le_bytes(),
            &body,
        ]
        .concat();
        let report = walk_in_pieces("clip.avi", &file, 999);
        assert!(report.breaks.is_empty(), "{:?}", report.notes);
        assert!(report.reached_end);
        assert_eq!(report.units, 4);

        let mut holed = file.clone();
        let frame = file.len() - (8 + 32) - (8 + 3002) + 8;
        holed[frame..frame + 3001].fill(0);
        let mut more = holed.clone();
        more.truncate(frame + 3001);
        // A short chunk of zeros is not a hole; only a long run is.
        assert!(walk_in_pieces("clip.avi", &holed, 500).breaks.is_empty());
        assert!(!walk_in_pieces("clip.avi", &more, 500).reached_end);
    }
}
