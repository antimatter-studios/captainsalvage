//! Browse recovered files by type, and read them live from the disk.
//!
//! Nothing here writes, to the disk or anywhere else. A preview is read from a file's
//! clusters when the window asks for it, one range at a time, so looking at a file
//! costs no copy. Thumbnails stay in memory.

use crate::session::Session;
use anyhow::{anyhow, Context, Result};
use image::codecs::jpeg::JpegEncoder;
use image::{DynamicImage, ImageDecoder, ImageReader};
use serde::Serialize;
use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Cursor;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::sync::Arc;
use std::time::SystemTime;

pub const THUMB_SIZE: u32 = 256;
const THUMB_QUALITY: u8 = 80;
/// Larger images are not read just to make a thumbnail.
const MAX_THUMB_SOURCE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_THUMB_CACHE_BYTES: usize = 128 * 1024 * 1024;
const MAX_PAGE_ROWS: u64 = 1000;
/// Thumbnails and a playing film read at the same time, so one is not enough.
const MAX_OPEN_STREAMS: usize = 8;
/// A failed thumbnail still takes a place in the cache, so it counts for this much.
const FAILED_THUMB_COST: usize = 256;
pub const SORT_BEST: &str = "best";
pub const SORT_LARGEST: &str = "largest";
pub const SORT_NEWEST: &str = "newest";
pub const GROUP_ALL: &str = "all";
const GROUP_OTHER: u8 = u8::MAX;
const LIKELY_GOOD_CONFIDENCE: f32 = 0.5;

pub struct TypeGroup {
    pub id: &'static str,
    pub label: &'static str,
    pub extensions: &'static [&'static str],
}

pub const TYPE_GROUPS: &[TypeGroup] = &[
    TypeGroup {
        id: "photos",
        label: "Photos",
        extensions: &[
            "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "heic", "heif",
        ],
    },
    TypeGroup {
        id: "videos",
        label: "Videos",
        extensions: &[
            "mp4", "m4v", "mov", "avi", "mkv", "webm", "wmv", "mpg", "mpeg", "3gp", "flv", "asf",
            "vob", "m2ts", "divx", "rm", "rmvb", "ogv",
        ],
    },
    TypeGroup {
        id: "music",
        label: "Music",
        extensions: &[
            "mp3", "wav", "flac", "ogg", "opus", "m4a", "aac", "wma", "aif", "aiff", "mid", "midi",
        ],
    },
    TypeGroup {
        id: "documents",
        label: "Documents",
        extensions: &[
            "pdf", "doc", "docx", "xls", "xlsx", "ppt", "pptx", "txt", "md", "rtf", "pages",
            "numbers", "key", "odt", "csv",
        ],
    },
    TypeGroup {
        id: "archives",
        label: "Archives",
        extensions: &[
            "zip", "rar", "gz", "tgz", "7z", "tar", "bz2", "xz", "iso", "dmg", "cso", "jar",
        ],
    },
];

fn extension_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default()
}

fn group_index(name: &str) -> u8 {
    let extension = extension_of(name);
    TYPE_GROUPS
        .iter()
        .position(|group| group.extensions.contains(&extension.as_str()))
        .map_or(GROUP_OTHER, |index| index as u8)
}

/// Files this app cannot decode into a picture by itself.
pub fn needs_ffmpeg_thumbnail(name: &str) -> bool {
    let extension = extension_of(name);
    extension == "heic"
        || extension == "heif"
        || TYPE_GROUPS
            .iter()
            .find(|group| group.id == "videos")
            .is_some_and(|group| group.extensions.contains(&extension.as_str()))
}

/// What the window should treat this file as, going by its name.
pub fn content_type(name: &str) -> &'static str {
    match extension_of(name).as_str() {
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "bmp" => "image/bmp",
        "webp" => "image/webp",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "mp4" | "m4v" => "video/mp4",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "3gp" => "video/3gpp",
        "mp3" => "audio/mpeg",
        "wav" => "audio/wav",
        "flac" => "audio/flac",
        "ogg" => "audio/ogg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "aif" | "aiff" => "audio/aiff",
        "pdf" => "application/pdf",
        _ => "application/octet-stream",
    }
}

/// One file in the database, as read for browsing.
pub struct BrowseScanRow {
    pub id: i64,
    pub name: String,
    pub length: u64,
    pub confidence: f64,
    pub modified: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowseRow {
    pub id: i64,
    pub name: String,
    pub length: u64,
    pub verdict: String,
    pub confidence: f64,
    pub first_cluster: u32,
    pub modified: Option<String>,
    pub in_use: bool,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeTotal {
    pub group: String,
    pub label: String,
    pub files: u64,
    pub bytes: u64,
    pub likely_good_files: u64,
    pub likely_good_bytes: u64,
}

#[derive(Clone, Debug, Serialize)]
pub struct BrowsePage {
    pub rows: Vec<BrowseRow>,
    pub total: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct BrowseQuery {
    pub group: String,
    pub folder: Option<u32>,
    pub min_confidence: f64,
    pub sort: String,
    pub text: String,
}

/// Kept small, because a damaged disk can hold a million of these.
#[derive(Clone, Copy)]
struct Compact {
    id: i64,
    length: u64,
    modified: u32,
    confidence: f32,
    group: u8,
}

struct BrowseIndex {
    stamp: Option<SystemTime>,
    rows: Vec<Compact>,
}

/// `2016-09-15T01:17:05` as a number that sorts the same way. Unknown dates sort last.
fn date_key(text: Option<&str>) -> u32 {
    const MINUTES_IN_MONTH: u32 = 31 * 24 * 60;
    const MINUTES_IN_YEAR: u32 = 12 * MINUTES_IN_MONTH;
    let Some(text) = text else { return 0 };
    let field = |range: std::ops::Range<usize>| -> u32 {
        text.get(range)
            .and_then(|part| part.parse().ok())
            .unwrap_or(0)
    };
    field(0..4) * MINUTES_IN_YEAR
        + field(5..7) * MINUTES_IN_MONTH
        + field(8..10) * 24 * 60
        + field(11..13) * 60
        + field(14..16)
}

/// A file's clusters, kept while it is being read in ranges.
pub(crate) struct EntryStream {
    id: i64,
    name: String,
    total: u64,
    valid: u64,
    clusters: Vec<u32>,
}

/// Part of a file, read live from the disk.
pub struct EntryRange {
    pub data: Vec<u8>,
    pub start: u64,
    pub total: u64,
    pub name: String,
}

#[derive(Clone)]
pub enum Thumb {
    Image(Arc<Vec<u8>>),
    Failed(String),
}

impl Thumb {
    fn cost(&self) -> usize {
        match self {
            Self::Image(data) => data.len(),
            Self::Failed(_) => FAILED_THUMB_COST,
        }
    }
}

#[derive(Default)]
struct ThumbCache {
    items: HashMap<i64, Thumb>,
    order: VecDeque<i64>,
    bytes: usize,
}

impl ThumbCache {
    fn insert(&mut self, id: i64, thumb: Thumb) {
        self.bytes += thumb.cost();
        if let Some(previous) = self.items.insert(id, thumb) {
            self.bytes -= previous.cost();
        } else {
            self.order.push_back(id);
        }
        while self.bytes > MAX_THUMB_CACHE_BYTES {
            let Some(oldest) = self.order.pop_front() else {
                break;
            };
            if let Some(gone) = self.items.remove(&oldest) {
                self.bytes -= gone.cost();
            }
        }
    }
}

#[derive(Default)]
pub(crate) struct BrowseCache {
    index: Option<Arc<BrowseIndex>>,
    folder: Option<(u32, Arc<HashSet<i64>>)>,
    list: Option<(BrowseQuery, Option<SystemTime>, Arc<Vec<u32>>)>,
    streams: VecDeque<Arc<EntryStream>>,
    thumbs: ThumbCache,
}

/// Makes a small JPEG. Damaged files are normal here, so a decoder panic counts as a
/// failed image instead of taking the app down.
pub fn make_thumbnail(data: &[u8]) -> Result<Vec<u8>> {
    catch_unwind(AssertUnwindSafe(|| decode_to_thumbnail(data)))
        .map_err(|_| anyhow!("the image decoder gave up on this data"))?
}

fn decode_to_thumbnail(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = ImageReader::new(Cursor::new(data))
        .with_guessed_format()?
        .into_decoder()?;
    let orientation = decoder.orientation()?;
    let mut image = DynamicImage::from_decoder(decoder)?;
    image.apply_orientation(orientation);
    let small = image.thumbnail(THUMB_SIZE, THUMB_SIZE).into_rgb8();
    let mut out = Vec::new();
    JpegEncoder::new_with_quality(&mut out, THUMB_QUALITY).encode_image(&small)?;
    Ok(out)
}

/// Reads an HTTP `Range` header of the form `bytes=a-b`, `bytes=a-` or `bytes=-n`.
/// Returns where to start and how many bytes, or `None` if it asks for nothing that exists.
pub fn parse_range(text: &str, total: u64) -> Option<(u64, u64)> {
    let (first, last) = text
        .strip_prefix("bytes=")?
        .split(',')
        .next()?
        .split_once('-')?;
    let (first, last) = (first.trim(), last.trim());
    if first.is_empty() {
        let suffix: u64 = last.parse().ok()?;
        return (suffix > 0 && total > 0).then(|| {
            let wanted = suffix.min(total);
            (total - wanted, wanted)
        });
    }
    let start: u64 = first.parse().ok()?;
    if start >= total {
        return None;
    }
    let end = match last {
        "" => total - 1,
        text => text.parse::<u64>().ok()?.min(total - 1),
    };
    (end >= start).then(|| (start, end - start + 1))
}

impl Session {
    fn lock_browse(&self) -> Result<std::sync::MutexGuard<'_, BrowseCache>> {
        self.browse
            .lock()
            .map_err(|_| anyhow!("browse lock poisoned"))
    }

    /// When the map last changed. A rebuild rewrites this file, whichever process runs it.
    fn map_stamp(&self) -> Option<SystemTime> {
        std::fs::metadata(self.workspace.confidence())
            .and_then(|metadata| metadata.modified())
            .ok()
    }

    fn browse_index(&self, cache: &mut BrowseCache) -> Result<Arc<BrowseIndex>> {
        let stamp = self.map_stamp();
        if let Some(index) = cache.index.as_ref().filter(|index| index.stamp == stamp) {
            return Ok(index.clone());
        }
        let rows = self.with_database(|database| database.browse_scan())?;
        let rows = rows
            .into_iter()
            .map(|row| Compact {
                id: row.id,
                length: row.length,
                modified: date_key(row.modified.as_deref()),
                confidence: row.confidence as f32,
                group: group_index(&row.name),
            })
            .collect();
        let index = Arc::new(BrowseIndex { stamp, rows });
        cache.index = Some(index.clone());
        cache.folder = None;
        cache.list = None;
        Ok(index)
    }

    fn folder_ids(&self, cache: &mut BrowseCache, folder: u32) -> Result<Arc<HashSet<i64>>> {
        if let Some((_, ids)) = cache.folder.as_ref().filter(|(head, _)| *head == folder) {
            return Ok(ids.clone());
        }
        let ids: HashSet<i64> = self
            .with_database(|database| database.files_under(folder))?
            .into_iter()
            .filter_map(|row| row.id)
            .collect();
        let ids = Arc::new(ids);
        cache.folder = Some((folder, ids.clone()));
        Ok(ids)
    }

    /// Files and bytes of each type, optionally only inside one folder.
    pub fn browse_types(&self, folder: Option<u32>, min_confidence: f64) -> Result<Vec<TypeTotal>> {
        let (index, ids) = {
            let mut cache = self.lock_browse()?;
            let index = self.browse_index(&mut cache)?;
            let ids = folder
                .map(|head| self.folder_ids(&mut cache, head))
                .transpose()?;
            (index, ids)
        };
        let mut totals = vec![[0u64; 4]; TYPE_GROUPS.len() + 1];
        for row in &index.rows {
            if f64::from(row.confidence) < min_confidence
                || ids.as_ref().is_some_and(|ids| !ids.contains(&row.id))
            {
                continue;
            }
            let slot = &mut totals[usize::from(row.group).min(TYPE_GROUPS.len())];
            slot[0] += 1;
            slot[1] += row.length;
            if row.confidence >= LIKELY_GOOD_CONFIDENCE {
                slot[2] += 1;
                slot[3] += row.length;
            }
        }
        let labels = TYPE_GROUPS
            .iter()
            .map(|group| (group.id, group.label))
            .chain([("other", "Other")]);
        Ok(labels
            .zip(totals)
            .map(|((group, label), total)| TypeTotal {
                group: group.into(),
                label: label.into(),
                files: total[0],
                bytes: total[1],
                likely_good_files: total[2],
                likely_good_bytes: total[3],
            })
            .collect())
    }

    fn browse_list(&self, query: &BrowseQuery) -> Result<(Arc<BrowseIndex>, Arc<Vec<u32>>)> {
        let mut cache = self.lock_browse()?;
        let index = self.browse_index(&mut cache)?;
        if let Some((_, _, list)) = cache
            .list
            .as_ref()
            .filter(|(key, stamp, _)| key == query && *stamp == index.stamp)
        {
            return Ok((index, list.clone()));
        }
        let ids = query
            .folder
            .map(|head| self.folder_ids(&mut cache, head))
            .transpose()?;
        let named = if query.text.trim().is_empty() {
            None
        } else {
            Some(self.with_database(|database| database.browse_ids_named(query.text.trim()))?)
        };
        let group = match query.group.as_str() {
            "" | GROUP_ALL => None,
            "other" => Some(GROUP_OTHER),
            id => Some(
                TYPE_GROUPS
                    .iter()
                    .position(|group| group.id == id)
                    .with_context(|| format!("no file type called {id}"))? as u8,
            ),
        };
        let mut list: Vec<u32> = index
            .rows
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                group.is_none_or(|group| row.group == group)
                    && f64::from(row.confidence) >= query.min_confidence
                    && ids.as_ref().is_none_or(|ids| ids.contains(&row.id))
                    && named.as_ref().is_none_or(|named| named.contains(&row.id))
            })
            .map(|(position, _)| position as u32)
            .collect();
        let rows = &index.rows;
        let row = |position: &u32| &rows[*position as usize];
        match query.sort.as_str() {
            SORT_LARGEST => list.sort_by_key(|position| std::cmp::Reverse(row(position).length)),
            SORT_NEWEST => list.sort_by_key(|position| std::cmp::Reverse(row(position).modified)),
            _ => list.sort_by(|a, b| {
                row(b)
                    .confidence
                    .total_cmp(&row(a).confidence)
                    .then(row(b).length.cmp(&row(a).length))
            }),
        }
        let list = Arc::new(list);
        cache.list = Some((query.clone(), index.stamp, list.clone()));
        Ok((index, list))
    }

    pub fn browse_files(&self, query: &BrowseQuery, offset: u64, limit: u64) -> Result<BrowsePage> {
        let (index, list) = self.browse_list(query)?;
        let ids: Vec<i64> = list
            .iter()
            .skip(offset as usize)
            .take(limit.min(MAX_PAGE_ROWS) as usize)
            .map(|position| index.rows[*position as usize].id)
            .collect();
        let mut found = self.with_database(|database| database.browse_rows(&ids))?;
        Ok(BrowsePage {
            rows: ids.iter().filter_map(|id| found.remove(id)).collect(),
            total: list.len() as u64,
        })
    }

    fn entry_stream(&self, id: i64, reuse: bool) -> Result<Arc<EntryStream>> {
        if reuse {
            let cache = self.lock_browse()?;
            if let Some(stream) = cache.streams.iter().find(|stream| stream.id == id) {
                return Ok(stream.clone());
            }
        }
        let (row, clusters) = self.with_database(|database| {
            let row = database
                .entry(id)?
                .with_context(|| format!("no entry with id {id}"))?;
            let clusters = database.entry_clusters(&self.volume, &row)?;
            Ok((row, clusters))
        })?;
        let stream = Arc::new(EntryStream {
            id,
            total: row
                .length
                .min(clusters.len() as u64 * self.volume.boot.cluster_size()),
            valid: row.valid_length.min(row.length),
            name: row.name,
            clusters,
        });
        let mut cache = self.lock_browse()?;
        cache.streams.retain(|known| known.id != id);
        cache.streams.push_back(stream.clone());
        if cache.streams.len() > MAX_OPEN_STREAMS {
            cache.streams.pop_front();
        }
        Ok(stream)
    }

    /// Clusters that follow each other are read together. A bad spot reads as zeros, so
    /// one damaged cluster never hides the rest of a file.
    fn read_cluster_run(&self, clusters: &[u32]) -> Vec<u8> {
        let size = self.volume.boot.cluster_size() as usize;
        if clusters
            .iter()
            .all(|&cluster| self.volume.valid_cluster(cluster))
        {
            if let Ok(data) = self.volume.read_bytes(
                self.volume.cluster_offset(clusters[0]),
                clusters.len() * size,
            ) {
                return data;
            }
        }
        let mut out = Vec::with_capacity(clusters.len() * size);
        for &cluster in clusters {
            let data = self
                .volume
                .valid_cluster(cluster)
                .then(|| self.volume.read_cluster(cluster).ok())
                .flatten();
            match data {
                Some(data) => out.extend_from_slice(&data),
                None => out.resize(out.len() + size, 0),
            }
        }
        out
    }

    /// How many bytes of this file can be read, and its name.
    pub fn entry_total(&self, id: i64) -> Result<(u64, String)> {
        let stream = self.entry_stream(id, true)?;
        Ok((stream.total, stream.name.clone()))
    }

    /// Reads part of a file from the disk, through the same clusters a copy would use.
    pub fn read_entry_range(&self, id: i64, start: u64, length: u64) -> Result<EntryRange> {
        let stream = self.entry_stream(id, start > 0)?;
        let total = stream.total;
        let end = start.saturating_add(length).min(total);
        let mut data = Vec::new();
        if start < end {
            let size = self.volume.boot.cluster_size();
            let last = ((end - 1) / size) as usize;
            let mut index = (start / size) as usize;
            let mut position = index as u64 * size;
            data.reserve((end - start) as usize);
            while index <= last {
                let mut run = 1;
                while index + run <= last
                    && stream.clusters[index + run] == stream.clusters[index + run - 1] + 1
                {
                    run += 1;
                }
                let bytes = self.read_cluster_run(&stream.clusters[index..index + run]);
                let from = start.saturating_sub(position) as usize;
                let to = (end - position).min(bytes.len() as u64) as usize;
                data.extend_from_slice(&bytes[from..to]);
                position += run as u64 * size;
                index += run;
            }
            // Bytes past the valid length are undefined on disk, and read as zeros.
            if end > stream.valid {
                let keep = stream.valid.saturating_sub(start) as usize;
                data[keep..].fill(0);
            }
        }
        Ok(EntryRange {
            data,
            start,
            total,
            name: stream.name.clone(),
        })
    }

    /// A small image of an entry, decoded from its real bytes, so damage shows.
    pub fn entry_thumbnail(&self, id: i64) -> Result<Thumb> {
        self.entry_thumbnail_using(id, None)
    }

    /// As `entry_thumbnail`. Videos and HEIC need ffmpeg, which is passed in when installed.
    pub fn entry_thumbnail_using(
        &self,
        id: i64,
        frame: Option<&dyn Fn(i64) -> Result<Vec<u8>>>,
    ) -> Result<Thumb> {
        if let Some(thumb) = self.lock_browse()?.thumbs.items.get(&id) {
            return Ok(thumb.clone());
        }
        let (_, name) = self.entry_total(id)?;
        if needs_ffmpeg_thumbnail(&name) {
            // Not remembered without ffmpeg, so installing it works without a restart.
            let Some(frame) = frame else {
                return Ok(Thumb::Failed("ffmpeg is not installed".into()));
            };
            let thumb = match frame(id) {
                Ok(image) => Thumb::Image(Arc::new(image)),
                Err(error) => Thumb::Failed(format!("{error:#}")),
            };
            self.lock_browse()?.thumbs.insert(id, thumb.clone());
            return Ok(thumb);
        }
        let range = self.read_entry_range(id, 0, MAX_THUMB_SOURCE_BYTES)?;
        let thumb = if range.total > MAX_THUMB_SOURCE_BYTES {
            Thumb::Failed("too large to sample".into())
        } else {
            match make_thumbnail(&range.data) {
                Ok(image) => Thumb::Image(Arc::new(image)),
                Err(error) => Thumb::Failed(format!("{error:#}")),
            }
        };
        self.lock_browse()?.thumbs.insert(id, thumb.clone());
        Ok(thumb)
    }
}
