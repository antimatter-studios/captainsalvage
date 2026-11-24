//! The NTFS flows against Windows itself.
//!
//! `ntfs_oracle.rs` grades the app with ntfs-3g and the Linux kernel. Neither of them formatted or
//! filled the volumes here. Windows did: `validation/windows/make-volume.ps1` runs Format-Volume
//! (512-byte and 4096-byte sectors; 512 to 65536-byte clusters; 24 to 320 MiB), writes a fixed
//! tree with Windows' own file system (nested folders, a folder of hundreds of files, long and
//! non-ASCII names with their 8.3 shadows, a fragmented file, sparse files, alternate data
//! streams, NTFS compression, hard links, junctions and symlinks, files deleted last), and
//! records what Get-ChildItem, Get-FileHash and fsutil report about it. Windows' chkdsk finds
//! nothing wrong with any of the volumes.
//!
//! The app then scans each image and must report the same tree, the same sizes and SHA-256 hashes,
//! the same streams, the same fragmentation and flags, and the same short names; what it copies
//! must be what Windows hashed; and the files Windows deleted must still be found and read back.
//!
//! The images are made by `validation/windows/capture.sh`, in the Windows guest of
//! fs-windows-test-harness (`chore test:windows-oracle`), or on a hosted Windows runner
//! (`.github/workflows/windows-oracle.yml`, which sets WINDOWS_ORACLE_DIR to what it made).
//! Behind the `windows-oracle` feature, and it FAILS, never skips, when Windows cannot be reached.
#![cfg(feature = "windows-oracle")]

mod ntfs_support;

use captainsalvage::engine::Engine;
use captainsalvage::export::CopyRequest;
use captainsalvage::session::Session;
use captainsalvage::source::FileSource;
use ntfs_support::*;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

// ------------------------------------------------------------------ Windows

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

const TASK: &str =
    "`chore test:windows-oracle` (its `chore windows:up` checks out fs-windows-test-harness \
    beside this repository and starts the Windows guest)";

/// The folder holding what Windows made: NAME.img, NAME.manifest.tsv and NAME.info.txt. Either a
/// folder somebody already filled (WINDOWS_ORACLE_DIR, as the hosted-runner job does), or the
/// result of asking Windows now.
fn windows_output() -> &'static PathBuf {
    static OUT: OnceLock<PathBuf> = OnceLock::new();
    OUT.get_or_init(|| {
        if let Ok(dir) = std::env::var("WINDOWS_ORACLE_DIR") {
            let dir = PathBuf::from(dir);
            assert!(
                dir.is_dir(),
                "WINDOWS_ORACLE_DIR={} is not a folder. {TASK} fills it.",
                dir.display()
            );
            return dir;
        }
        let dir = repo().join(".windows-oracle");
        let _ = std::fs::remove_dir_all(&dir);
        let status = Command::new("bash")
            .arg(repo().join("validation/windows/capture.sh"))
            .arg(&dir)
            .status();
        assert!(
            matches!(&status, Ok(status) if status.success()),
            "Windows did not format and fill the volumes ({status:?}), so nothing here can be \
             graded by it. {TASK} provides the guest."
        );
        dir
    })
}

/// One line of validation/windows/volumes.json.
#[derive(Clone, Debug)]
struct Spec {
    name: String,
    size_mib: u64,
    cluster: u64,
    sector: u64,
}

fn specs() -> Vec<Spec> {
    let text = std::fs::read_to_string(repo().join("validation/windows/volumes.json")).unwrap();
    serde_json::from_str::<Vec<Value>>(&text)
        .unwrap()
        .iter()
        .map(|row| Spec {
            name: row["name"].as_str().unwrap().to_string(),
            size_mib: row["sizeMB"].as_u64().unwrap(),
            cluster: row["clusterBytes"].as_u64().unwrap(),
            sector: row["sectorBytes"].as_u64().unwrap(),
        })
        .collect()
}

fn spec(name: &str) -> Spec {
    specs().into_iter().find(|spec| spec.name == name).unwrap()
}

// ------------------------------------------------------------- the manifest

#[derive(Debug, Default)]
struct Manifest {
    meta: BTreeMap<String, String>,
    dirs: BTreeSet<String>,
    /// path -> (size, sha256)
    files: BTreeMap<String, (u64, String)>,
    /// `path:stream` -> (size, sha256)
    streams: BTreeMap<String, (u64, String)>,
    /// deleted files: path -> (size, sha256)
    gone: BTreeMap<String, (u64, String)>,
    /// data extents fsutil reports
    extents: BTreeMap<String, usize>,
    attributes: BTreeMap<String, String>,
    short_names: BTreeMap<String, String>,
    reparse: BTreeMap<String, String>,
    hard_links: Vec<(String, String)>,
    /// deleted files whose first bytes Windows left somewhere in the volume
    kept: BTreeMap<String, bool>,
    owned: BTreeSet<String>,
}

fn parse_manifest(text: &str) -> Manifest {
    let mut manifest = Manifest::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        let size_sha = |fields: &[&str]| (fields[1].parse::<u64>().unwrap(), fields[2].to_string());
        match fields[0] {
            "m" => {
                manifest
                    .meta
                    .insert(fields[1].to_string(), fields[2].to_string());
            }
            "d" => {
                manifest.dirs.insert(fields[1].to_string());
            }
            "f" => {
                manifest
                    .files
                    .insert(fields[3].to_string(), size_sha(&fields));
            }
            "s" => {
                manifest
                    .streams
                    .insert(fields[3].to_string(), size_sha(&fields));
            }
            "g" => {
                manifest
                    .gone
                    .insert(fields[3].to_string(), size_sha(&fields));
            }
            "e" => {
                manifest
                    .extents
                    .insert(fields[2].to_string(), fields[1].parse().unwrap());
            }
            "a" => {
                manifest
                    .attributes
                    .insert(fields[2].to_string(), fields[1].to_string());
            }
            "n" => {
                manifest
                    .short_names
                    .insert(fields[2].to_string(), fields[1].to_string());
            }
            "l" => {
                manifest
                    .reparse
                    .insert(fields[2].to_string(), fields[1].to_string());
            }
            "h" => manifest
                .hard_links
                .push((fields[1].to_string(), fields[2].to_string())),
            "p" => {
                manifest
                    .kept
                    .insert(fields[2].to_string(), fields[1] == "1");
            }
            "x" => {
                manifest.owned.insert(fields[1].to_string());
            }
            other => panic!("unexpected manifest line {other:?}: {line}"),
        }
    }
    manifest
}

fn manifest(name: &str) -> Manifest {
    let path = windows_output().join(format!("{name}.manifest.tsv"));
    parse_manifest(
        &std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("{}: {error}. {TASK} makes it.", path.display())),
    )
}

// ------------------------------------------------------------------ the app

/// Everything the app reports about the live tree.
#[derive(Debug, Default)]
struct AppTree {
    dirs: BTreeMap<String, i64>,
    files: BTreeMap<String, i64>,
    /// path -> (size, sha256) of what read_entry_range returned
    content: BTreeMap<String, (u64, String)>,
    /// files the app could not read, and why
    unreadable: BTreeMap<String, String>,
    /// `path:stream` -> id, for the named data streams the app lists as entries of their own (issue #168)
    streams: BTreeMap<String, i64>,
    /// `path:stream` -> (size, sha256) of what read_entry_range returned for that entry
    stream_content: BTreeMap<String, (u64, String)>,
    /// stream entries the app could not read, and why
    stream_unreadable: BTreeMap<String, String>,
}

struct Scanned {
    session: Arc<Session>,
    engine: Arc<Engine>,
    image: PathBuf,
}

/// The app's view of the volume Windows made, or the reason it does not open it.
fn open_image(name: &str) -> Result<Scanned, String> {
    let image = windows_output().join(format!("{name}.img"));
    let source = FileSource::open(&image)
        .unwrap_or_else(|error| panic!("{}: {error}. {TASK} makes it.", image.display()));
    let session = Session::with_source(
        Path::new("/test/windows-image"),
        Arc::new(source),
        workspace(name),
    )
    .map_err(|error| format!("{error:#}"))?;
    scan(&session);
    let engine = Engine::new();
    engine.set_session(session.clone());
    Ok(Scanned {
        session,
        engine,
        image,
    })
}

fn ids(session: &Session, sql: &str) -> Vec<i64> {
    session.query_map(sql).unwrap()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect()
}

/// Where a file of this path lands in a copy: a name over 240 bytes is cut before its extension.
fn name_on_destination(path: &str) -> String {
    let (folder, name) = path.rsplit_once('/').unwrap_or(("", path));
    let mut name = name.to_string();
    if name.len() > 240 {
        let extension = match name.rfind('.') {
            Some(dot) if dot > 0 && name.len() - dot <= 16 => name.split_off(dot),
            _ => String::new(),
        };
        while name.len() + extension.len() > 240 {
            name.pop();
        }
        name.push_str(&extension);
    }
    if folder.is_empty() {
        name
    } else {
        format!("{folder}/{name}")
    }
}

fn path_of(session: &Session, id: i64) -> (captainsalvage::map::Row, String) {
    let row = session
        .with_database(|database| Ok(database.entry(id)?.unwrap()))
        .unwrap();
    // The tree path of a folder below the root begins with a slash, as the window shows it.
    let path = row
        .path
        .clone()
        .unwrap_or_default()
        .trim_start_matches('/')
        .to_string();
    let full = if path.is_empty() {
        row.name.clone()
    } else {
        format!("{path}/{}", row.name)
    };
    (row, full)
}

/// Windows' own folders, and the system files every NTFS volume has, are not part of the tree
/// Windows listed.
fn is_system(path: &str, owned: &BTreeSet<String>) -> bool {
    path.split('/').any(|part| part.starts_with('$'))
        || owned
            .iter()
            .any(|dir| path == dir || path.starts_with(&format!("{dir}/")))
}

fn app_tree(scanned: &Scanned, owned: &BTreeSet<String>) -> AppTree {
    let session = &scanned.session;
    let mut tree = AppTree::default();
    for id in ids(
        session,
        "SELECT id FROM sets WHERE is_dir = 0 AND in_use = 1 AND checksum_ok = 1",
    ) {
        let (row, path) = path_of(session, id);
        if is_system(&path, owned) {
            continue;
        }
        // A name NTFS allows has no colon, so `file:stream` is a named data stream listed as an entry of its own, which is
        // compared with Windows' list of streams, never with its list of files.
        let stream_entry = row.name.contains(':');
        if stream_entry {
            tree.streams.insert(path.clone(), id);
        } else {
            tree.files.insert(path.clone(), id);
        }
        match session.read_entry_range(id, 0, row.length + 8) {
            Ok(read) if read.data.len() as u64 == row.length => {
                let content = (row.length, sha(&read.data));
                if stream_entry {
                    tree.stream_content.insert(path, content);
                } else {
                    tree.content.insert(path, content);
                }
            }
            Ok(read) => {
                let why = format!("read {} bytes of {}", read.data.len(), row.length);
                if stream_entry {
                    tree.stream_unreadable.insert(path, why);
                } else {
                    tree.unreadable.insert(path, why);
                }
            }
            Err(error) => {
                if stream_entry {
                    tree.stream_unreadable.insert(path, format!("{error:#}"));
                } else {
                    tree.unreadable.insert(path, format!("{error:#}"));
                }
            }
        }
    }
    for id in ids(
        session,
        "SELECT id FROM sets WHERE is_dir = 1 AND in_use = 1 AND checksum_ok = 1",
    ) {
        let (row, path) = path_of(session, id);
        if is_system(&path, owned) || row.name == "." {
            continue;
        }
        tree.dirs.insert(path, id);
    }
    tree
}

fn details(scanned: &Scanned, id: i64) -> Value {
    call(&scanned.engine, "ntfs_entry", json!({ "id": id }))
}

fn unnamed_stream(details: &Value) -> Option<&Value> {
    details["streams"]
        .as_array()?
        .iter()
        .find(|stream| stream["name"] == "")
}

/// What the image holds at the runs of an entry's unnamed data stream, as its record details name them, cut to `size`: a
/// sparse run is zeros. Read from the image file itself, not through the app.
fn raw_bytes_of_runs(image: &Path, cluster: u64, details: &Value, size: u64) -> Option<Vec<u8>> {
    use std::io::{Read, Seek, SeekFrom};
    let runs = unnamed_stream(details)?["runs"].as_array()?;
    let mut file = std::fs::File::open(image).ok()?;
    let mut out = Vec::new();
    for run in runs {
        let bytes = run["count"].as_u64()? * cluster;
        let mut chunk = vec![0u8; bytes as usize];
        if let Some(lcn) = run["lcn"].as_u64() {
            file.seek(SeekFrom::Start(lcn * cluster)).ok()?;
            file.read_exact(&mut chunk).ok()?;
        }
        out.extend_from_slice(&chunk);
    }
    out.truncate(size as usize);
    (out.len() as u64 == size).then_some(out)
}

// ------------------------------------------------------------ the comparisons

/// One way the app's report differs from Windows': the path, and what is different.
#[derive(Clone, Debug)]
struct Diff {
    path: String,
    what: String,
}

impl std::fmt::Display for Diff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path, self.what)
    }
}

fn diff(path: &str, what: impl Into<String>) -> Diff {
    Diff {
        path: path.to_string(),
        what: what.into(),
    }
}

/// What the app does not do yet, as these volumes show it. Each is found by Windows' volumes, has
/// an issue, and is pinned here: the test fails for any other difference, and it fails too when a
/// gap that must show on a volume no longer does, so that fixing one means removing it here and
/// from the README. Compression (issue #174), hard links (#175) and the 4096-byte-sector volume with
/// 1024-byte records (#177) were gaps and are not now.
fn known_gap(_want: &Manifest, _difference: &Diff) -> Option<&'static str> {
    None
}

/// Which gaps this volume must show: it holds the shape that triggers each.
fn gaps_expected(_want: &Manifest) -> BTreeSet<&'static str> {
    BTreeSet::new()
}

/// What differs in the listing, one line each, so a failure names every path.
fn listing_differences(want: &Manifest, app: &AppTree) -> Vec<Diff> {
    let mut out = Vec::new();
    // A reparse point is listed by Windows without being followed, and the app may show it as a
    // file or a folder. It is checked for being there, not for what it holds.
    let reparse: BTreeSet<&String> = want.reparse.keys().collect();
    for path in &want.dirs {
        if !app.dirs.contains_key(path) {
            out.push(diff(path, "Windows lists this folder, the app does not"));
        }
    }
    for path in app.dirs.keys() {
        if !want.dirs.contains(path) && !reparse.contains(path) {
            out.push(diff(path, "the app lists this folder, Windows does not"));
        }
    }
    for (path, (size, sum)) in &want.files {
        match (app.files.get(path), app.content.get(path)) {
            (None, _) => out.push(diff(path, "Windows lists it, the app does not")),
            (Some(_), None) => out.push(diff(
                path,
                format!(
                    "could not be read: {}",
                    app.unreadable.get(path).map(String::as_str).unwrap_or("?")
                ),
            )),
            (Some(_), Some((have_size, have_sum))) => {
                if have_size != size {
                    out.push(diff(
                        path,
                        format!("size Windows {size}, the app {have_size}"),
                    ));
                } else if have_sum != sum {
                    out.push(diff(
                        path,
                        format!("SHA-256 Windows {sum}, the app {have_sum}"),
                    ));
                }
            }
        }
    }
    for path in app.files.keys() {
        if !want.files.contains_key(path) && !reparse.contains(path) {
            out.push(diff(path, "the app lists it, Windows does not"));
        }
    }
    for path in reparse {
        if !app.files.contains_key(path) && !app.dirs.contains_key(path) {
            out.push(diff(
                path,
                format!(
                    "Windows lists this {}, the app does not",
                    want.reparse[path]
                ),
            ));
        }
    }
    out
}

/// How many of each thing was compared, so a comparison that checked nothing cannot pass.
#[derive(Debug, Default)]
struct Checked {
    streams: usize,
    stream_entries: usize,
    short_names: usize,
    extents: usize,
    flags: usize,
}

/// Each named data stream Windows lists for a file is an entry of its own, `file:stream`, with Windows' size and bytes, and
/// every such entry the app lists is a stream Windows lists (issue #168). A folder's streams are not entries: they are
/// compared in the record's details by `record_differences`.
fn stream_entry_differences(want: &Manifest, app: &AppTree, checked: &mut Checked) -> Vec<Diff> {
    let mut out = Vec::new();
    for (key, (size, sum)) in &want.streams {
        let Some((owner, _)) = key.rsplit_once(':') else {
            continue;
        };
        if want.dirs.contains(owner) {
            continue;
        }
        checked.stream_entries += 1;
        match (app.streams.get(key), app.stream_content.get(key)) {
            (None, _) => out.push(diff(
                key,
                "Windows lists this stream of a file, the app lists no entry for it",
            )),
            (Some(_), None) => out.push(diff(
                key,
                format!(
                    "stream could not be read: {}",
                    app.stream_unreadable
                        .get(key)
                        .map(String::as_str)
                        .unwrap_or("?")
                ),
            )),
            (Some(_), Some((have_size, have_sum))) => {
                if have_size != size {
                    out.push(diff(
                        key,
                        format!("stream size Windows {size}, the app {have_size}"),
                    ));
                } else if have_sum != sum {
                    out.push(diff(
                        key,
                        format!("stream SHA-256 Windows {sum}, the app {have_sum}"),
                    ));
                }
            }
        }
    }
    for key in app.streams.keys() {
        if !want.streams.contains_key(key) {
            out.push(diff(
                key,
                "the app lists this stream as an entry, Windows lists no such stream",
            ));
        }
    }
    out
}

/// What Windows' manifest says about streams, extents, flags and short names, against what the
/// app's record details say.
fn record_differences(
    scanned: &Scanned,
    want: &Manifest,
    app: &AppTree,
    checked: &mut Checked,
) -> Vec<Diff> {
    let mut out = Vec::new();
    for (path, id) in app.files.iter().chain(app.dirs.iter()) {
        let streams_wanted: Vec<(&str, u64)> = want
            .streams
            .iter()
            .filter_map(|(key, (size, _))| {
                key.rsplit_once(':')
                    .filter(|(file, _)| file == path)
                    .map(|(_, stream)| (stream, *size))
            })
            .collect();
        let short = want.short_names.get(path);
        let extents = want.extents.get(path);
        let flags = want.attributes.get(path);
        if streams_wanted.is_empty() && short.is_none() && extents.is_none() && flags.is_none() {
            continue;
        }
        let record = details(scanned, *id);
        let streams = record["streams"].as_array().unwrap();
        for (stream, size) in streams_wanted {
            checked.streams += 1;
            match streams
                .iter()
                .find(|item| item["name"].as_str().unwrap().eq_ignore_ascii_case(stream))
            {
                None => out.push(diff(
                    &format!("{path}:{stream}"),
                    "Windows lists this stream, the app does not",
                )),
                Some(item) if item["length"] != size => out.push(diff(
                    &format!("{path}:{stream}"),
                    format!("stream length Windows {size}, the app {}", item["length"]),
                )),
                Some(_) => {}
            }
        }
        if let Some(short) = short {
            checked.short_names += 1;
            let names: Vec<&str> = record["names"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| item["name"].as_str().unwrap())
                .collect();
            if !names.iter().any(|name| name.eq_ignore_ascii_case(short)) {
                out.push(diff(
                    path,
                    format!("Windows made the short name {short}, the app lists {names:?}"),
                ));
            }
        }
        let main = unnamed_stream(&record);
        let compressed = flags.is_some_and(|flags| flags.contains("Compressed"));
        if let Some(count) = extents {
            let resident = main.is_some_and(|stream| stream["resident"] == true);
            let have = main.map_or(0, |stream| {
                stream["runs"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter(|run| !run["lcn"].is_null())
                    .count()
            });
            checked.extents += 1;
            if resident && *count != 0 {
                out.push(diff(
                    path,
                    format!(
                        "Windows reports {count} extents, the app has the data inside the record"
                    ),
                ));
            } else if !resident && !compressed && have != *count {
                out.push(diff(
                    path,
                    format!("extents Windows {count}, the app {have}"),
                ));
            }
        }
        if let (Some(flags), Some(main), false) = (flags, main, app.dirs.contains_key(path)) {
            checked.flags += 1;
            for (flag, key) in [("SparseFile", "sparse"), ("Compressed", "compressed")] {
                if main[key] != flags.contains(flag) {
                    out.push(diff(
                        path,
                        format!(
                            "Windows says {flag}={}, the app {}",
                            flags.contains(flag),
                            main[key]
                        ),
                    ));
                }
            }
        }
    }
    out
}

fn assert_expected_differences(label: &str, want: &Manifest, differences: Vec<Diff>) {
    let mut seen = BTreeSet::new();
    let mut unexpected = Vec::new();
    for difference in &differences {
        match known_gap(want, difference) {
            Some(gap) => {
                seen.insert(gap);
            }
            None => unexpected.push(difference.to_string()),
        }
    }
    assert!(
        unexpected.is_empty(),
        "{label}: {} difference(s) from what Windows reported:\n  {}",
        unexpected.len(),
        unexpected.join("\n  ")
    );
    let expected = gaps_expected(want);
    let gone: Vec<&&str> = expected.difference(&seen).collect();
    assert!(
        gone.is_empty(),
        "{label}: a known gap no longer shows, so it is fixed: {gone:?}. Remove it from the gaps \
         in this test and from the README."
    );
}

/// The boot sector's sector and cluster sizes, and its MFT record size and the number of words of
/// the first record's update sequence array, which is what the formatter chose.
fn geometry(image: &Path) -> (u64, u64, u64, u64) {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(image).unwrap();
    let mut boot = [0u8; 512];
    file.read_exact(&mut boot).unwrap();
    assert_eq!(&boot[3..11], b"NTFS    ");
    let sector = u16::from_le_bytes([boot[11], boot[12]]) as u64;
    let cluster = sector * boot[13] as u64;
    let record = match boot[64] as i8 {
        n if n > 0 => n as u64 * cluster,
        n => 1u64 << -(n as i32),
    };
    let mft = u64::from_le_bytes(boot[48..56].try_into().unwrap()) * cluster;
    let mut first = vec![0u8; record as usize];
    file.seek(SeekFrom::Start(mft)).unwrap();
    file.read_exact(&mut first).unwrap();
    assert_eq!(&first[0..4], b"FILE");
    let words = u16::from_le_bytes([first[6], first[7]]) as u64;
    (sector, cluster, record, words)
}

// ------------------------------------------------------------------ the tests

fn volume_matches_windows(name: &str) {
    let want_spec = spec(name);
    let want = manifest(name);
    let out = windows_output();
    let image = out.join(format!("{name}.img"));

    // The volume is what was asked for, and Windows called it clean.
    let (sector, cluster, record, words) = geometry(&image);
    assert_eq!(
        (sector, cluster),
        (want_spec.sector, want_spec.cluster),
        "{name}: the boot sector"
    );
    let info = std::fs::read_to_string(out.join(format!("{name}.info.txt"))).unwrap();
    assert!(
        info.contains("Windows has scanned the file system and found no problems"),
        "{name}: chkdsk: {info}"
    );
    let length = std::fs::metadata(&image).unwrap().len();
    assert!(
        length > want_spec.size_mib * 1024 * 1024 * 9 / 10
            && length <= want_spec.size_mib * 1024 * 1024,
        "{name}: a {length}-byte image for a {} MiB disk",
        want_spec.size_mib
    );
    let before = sha(&std::fs::read(&image).unwrap());

    // Windows' own volume on 4096-byte sectors has 1024-byte file records, held together by three words of update sequence
    // array, one per 512 bytes of the record (issue #177): it is opened and compared like every other volume.
    let scanned = match open_image(name) {
        Ok(scanned) => scanned,
        Err(error) => panic!("{name}: the app does not open the volume: {error}"),
    };
    if sector > record {
        assert_eq!((sector, record, words), (4096, 1024, 3), "{name}");
    }

    let app = app_tree(&scanned, &want.owned);
    let mut differences = listing_differences(&want, &app);

    // The folders Windows owns are there, even though what is inside them is not compared.
    for dir in &want.owned {
        let listed = ids(
            &scanned.session,
            &format!(
                "SELECT id FROM sets WHERE is_dir = 1 AND in_use = 1 AND name = '{}'",
                dir.replace('\'', "''")
            ),
        );
        if listed.is_empty() {
            differences.push(diff(
                dir,
                "Windows owns this folder, the app does not list it",
            ));
        }
    }

    // Both names of a hard link are listed, and read the same.
    for (first, second) in &want.hard_links {
        if let (Some(a), Some(b)) = (app.content.get(first), app.content.get(second)) {
            if a != b {
                differences.push(diff(
                    second,
                    format!("a hard link of {first}, read differently"),
                ));
            }
        }
    }

    // Every file Windows lists is called live.
    let not_live = scanned
        .session
        .query_map(
            "SELECT id, verdict FROM sets WHERE is_dir = 0 AND in_use = 1 AND checksum_ok = 1 \
             AND length > 0 AND verdict != 'live' \
             AND node IN (SELECT head FROM nodes WHERE kind IN ('root', 'live'))",
        )
        .unwrap();
    for row in not_live["rows"].as_array().unwrap() {
        let (_, path) = path_of(&scanned.session, row["id"].as_i64().unwrap());
        if !is_system(&path, &want.owned) && want.files.contains_key(&path) {
            differences.push(diff(
                &path,
                format!("is called {}, not live", row["verdict"]),
            ));
        }
    }

    let mut checked = Checked::default();
    differences.extend(record_differences(&scanned, &want, &app, &mut checked));
    differences.extend(stream_entry_differences(&want, &app, &mut checked));
    eprintln!(
        "{name}: compared {} files, {} folders, {checked:?}, {} deleted files",
        want.files.len(),
        want.dirs.len(),
        want.gone.len()
    );
    // Every stream and short name Windows reported was found in the app's record of its entry.
    assert_eq!(checked.streams, want.streams.len(), "{name}");
    assert_eq!(checked.short_names, want.short_names.len(), "{name}");
    // Every stream of a file Windows reported is an entry of its own with Windows' size and bytes, and one comparison
    // that found none cannot pass.
    let file_streams = want
        .streams
        .keys()
        .filter(|key| {
            key.rsplit_once(':')
                .is_some_and(|(owner, _)| !want.dirs.contains(owner))
        })
        .count();
    assert_eq!(checked.stream_entries, file_streams, "{name}");
    assert!(file_streams > 0, "{name}: Windows made no stream of a file");
    assert!(
        checked.extents > 100 && checked.flags >= 4,
        "{name}: {checked:?}"
    );

    // Files Windows deleted are still found, and read as they were while nothing wrote over them.
    for (path, (size, sum)) in &want.gone {
        let leaf = path.rsplit('/').next().unwrap();
        let candidates = ids(
            &scanned.session,
            &format!(
                "SELECT id FROM sets WHERE is_dir = 0 AND in_use = 0 AND name = '{}'",
                leaf.replace('\'', "''")
            ),
        );
        let Some(id) = candidates.first() else {
            // Windows scrubs some deleted files whole: nothing of their bytes is left in the volume,
            // and there is nothing to find.
            if want.kept.get(path) != Some(&false) {
                differences.push(diff(path, "deleted: the app does not find it"));
            }
            continue;
        };
        let (row, found) = path_of(&scanned.session, *id);
        if row.length != *size {
            differences.push(diff(
                path,
                format!("deleted: size Windows {size}, the app {}", row.length),
            ));
            continue;
        }
        if !path.starts_with("gone-folder/") && found != *path {
            differences.push(diff(path, format!("deleted: the app puts it at {found}")));
        }
        match scanned.session.read_entry_range(*id, 0, size + 8) {
            Ok(read) if sha(&read.data) == *sum => {}
            // Windows scrubbed this file, or let NTFS use its clusters again: none of its first bytes is anywhere in the volume
            // (`p 0` in the manifest). Its record is still there, so the app finds it with its size, and what it can read is
            // whatever the volume holds at the clusters the record names now: zeros on the 4096-byte-sector volume, where
            // Windows zeroed them on delete, and `RSTR` restart pages of `$LogFile` on a 320 MiB one, which took them. What is
            // checked is that the app reads exactly the bytes of the image at those runs, never others.
            Ok(read)
                if want.kept.get(path) == Some(&false)
                    && read.data.len() as u64 == *size
                    && raw_bytes_of_runs(&image, cluster, &details(&scanned, *id), *size)
                        .is_some_and(|raw| raw == read.data) => {}
            Ok(read) => differences.push(diff(
                path,
                format!(
                    "deleted: SHA-256 Windows {sum}, the app {} ({} bytes)",
                    sha(&read.data),
                    read.data.len()
                ),
            )),
            Err(error) => differences.push(diff(path, format!("deleted: not readable: {error:#}"))),
        }
    }

    // What the app copies is what Windows hashed.
    let copy_ids: Vec<i64> = app
        .files
        .iter()
        .filter(|(path, _)| want.files.contains_key(*path))
        .map(|(_, id)| *id)
        .collect();
    let destination = scanned.session.workspace.dir().join("copied");
    scanned
        .session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: copy_ids,
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(900);
    let status = loop {
        let status = scanned.session.copy_status().unwrap();
        if matches!(status.state.as_str(), "finished" | "failed" | "stopped") {
            break status;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{name}: copy timed out: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    };
    eprintln!("{name}: copy {status:?}");
    let root = destination.join("Volume root");
    let mut copied: BTreeMap<String, (u64, String)> = BTreeMap::new();
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for item in std::fs::read_dir(&directory).into_iter().flatten() {
            let path = item.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let data = std::fs::read(&path).unwrap();
                let relative = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                copied.insert(relative, (data.len() as u64, sha(&data)));
            }
        }
    }
    let mut expected_targets = BTreeSet::new();
    for (path, want_file) in &want.files {
        let target = name_on_destination(path);
        expected_targets.insert(target.clone());
        match copied.get(&target) {
            None => differences.push(diff(path, "not copied")),
            Some(have) if have != want_file => differences.push(diff(
                path,
                format!("copied {have:?}, Windows {want_file:?}"),
            )),
            Some(_) => {}
        }
    }
    for path in copied.keys() {
        if !expected_targets.contains(path) && !want.reparse.contains_key(path) {
            differences.push(diff(path, "copied, but Windows does not list it"));
        }
    }

    assert_expected_differences(name, &want, differences);

    // Nothing was written to the image.
    assert_eq!(
        sha(&std::fs::read(&scanned.image).unwrap()),
        before,
        "{name}: the image was written"
    );
    assert!(scanned.session.verify_unchanged().unwrap().unchanged);
    // The copy of a 320 MiB volume is not left in the temporary folder.
    let _ = std::fs::remove_dir_all(scanned.session.workspace.dir());
}

macro_rules! volume_tests {
    ($($test:ident => $name:literal,)*) => {
        $(
            #[test]
            fn $test() {
                volume_matches_windows($name);
            }
        )*
    };
}

volume_tests! {
    a_volume_windows_formatted_with_512_byte_clusters_reads_as_windows_listed_it => "win-small-c512",
    a_volume_windows_formatted_with_1024_byte_clusters_reads_as_windows_listed_it => "win-small-c1024",
    a_volume_windows_formatted_with_4096_byte_clusters_reads_as_windows_listed_it => "win-small-c4096",
    a_volume_on_a_4096_byte_sector_disk_reads_as_windows_listed_it => "win-4kn-c4096",
    a_320_mib_volume_with_4096_byte_clusters_reads_as_windows_listed_it => "win-large-c4096",
    a_320_mib_volume_with_65536_byte_clusters_reads_as_windows_listed_it => "win-large-c65536",
}

#[test]
fn every_volume_in_the_spec_was_made_by_windows_and_checked_by_its_chkdsk() {
    let out = windows_output();
    let specs = specs();
    assert!(specs.len() >= 6);
    let mut sectors = BTreeSet::new();
    let mut clusters = BTreeSet::new();
    for spec in &specs {
        let want = manifest(&spec.name);
        assert_eq!(want.meta["name"], spec.name);
        assert_eq!(want.meta["cluster_bytes"], spec.cluster.to_string());
        assert_eq!(want.meta["sector_bytes"], spec.sector.to_string());
        assert!(
            want.meta["windows"].starts_with("10."),
            "{}: {:?}",
            spec.name,
            want.meta
        );
        let info = std::fs::read_to_string(out.join(format!("{}.info.txt", spec.name))).unwrap();
        assert!(
            info.contains("Windows has scanned the file system and found no problems"),
            "{}: {info}",
            spec.name
        );
        assert!(info.contains("Microsoft Windows"), "{}", spec.name);
        // The tree has every shape it was meant to have, so the manifest is not thin where it matters.
        assert!(
            want.files.len() > 500,
            "{}: {} files",
            spec.name,
            want.files.len()
        );
        assert_eq!(want.streams.len(), 5, "{}", spec.name);
        assert_eq!(want.gone.len(), 6);
        assert_eq!(want.reparse.len(), 3);
        assert_eq!(want.hard_links.len(), 1);
        assert!(
            want.extents["fragmented/interleaved-one.bin"] >= 2,
            "{}",
            spec.name
        );
        assert!(want.attributes["sparse/sparse.bin"].contains("SparseFile"));
        assert!(
            want.short_names.len() > 100,
            "{}: Windows made {} short names",
            spec.name,
            want.short_names.len()
        );
        let (sector, cluster, _, _) = geometry(&out.join(format!("{}.img", spec.name)));
        assert_eq!(
            (sector, cluster),
            (spec.sector, spec.cluster),
            "{}",
            spec.name
        );
        sectors.insert(sector);
        clusters.insert(cluster);
        // NTFS compresses only where clusters are 4096 bytes or fewer: the 64 KiB volume has no
        // compressed file, and the others have.
        assert_eq!(
            want.attributes
                .get("compressed/lznt1-text.txt")
                .is_some_and(|flags| flags.contains("Compressed")),
            spec.cluster <= 4096,
            "{}",
            spec.name
        );
    }
    assert!(sectors.contains(&512) && sectors.contains(&4096));
    assert!(clusters.len() >= 4, "{clusters:?}");
}

#[test]
fn a_changed_hash_or_a_missing_file_in_what_the_app_reports_fails_the_comparison() {
    let want = manifest("win-small-c4096");
    let mut app = AppTree::default();
    for dir in &want.dirs {
        app.dirs.insert(dir.clone(), 1);
    }
    for (path, content) in &want.files {
        app.files.insert(path.clone(), 1);
        app.content.insert(path.clone(), content.clone());
    }
    for path in want.reparse.keys() {
        app.files.insert(path.clone(), 1);
    }
    assert!(listing_differences(&want, &app).is_empty());
    let first = want.files.keys().next().unwrap().clone();
    app.content
        .get_mut(&first)
        .unwrap()
        .1
        .replace_range(0..1, "x");
    assert!(listing_differences(&want, &app)[0]
        .to_string()
        .contains(&first));
    app.files.remove(&first);
    app.content.remove(&first);
    assert!(listing_differences(&want, &app)[0]
        .what
        .contains("the app does not"));
    app.dirs.insert("invented".into(), 1);
    assert!(listing_differences(&want, &app)
        .iter()
        .any(|line| line.path == "invented"));
    // A difference that is not a known gap is reported, and a known one is accepted.
    let unknown = diff("cluster-exact.bin", "SHA-256 Windows a, the app b");
    assert!(known_gap(&want, &unknown).is_none());
    // A name of a hard link the app does not list is not a known gap: every name is listed (#175).
    let second = &want.hard_links[0].1;
    assert!(known_gap(&want, &diff(second, "Windows lists it, the app does not")).is_none());
}

#[test]
fn a_missing_stream_entry_or_a_changed_stream_hash_or_an_invented_stream_fails_the_comparison() {
    let want = manifest("win-small-c4096");
    let mut app = AppTree::default();
    for (key, content) in &want.streams {
        let owner = key.rsplit_once(':').unwrap().0;
        if want.dirs.contains(owner) {
            continue;
        }
        app.streams.insert(key.clone(), 1);
        app.stream_content.insert(key.clone(), content.clone());
    }
    assert!(!app.streams.is_empty());
    let mut checked = Checked::default();
    assert!(stream_entry_differences(&want, &app, &mut checked).is_empty());
    assert_eq!(checked.stream_entries, app.streams.len());

    let first = app.streams.keys().next().unwrap().clone();
    app.stream_content
        .get_mut(&first)
        .unwrap()
        .1
        .replace_range(0..1, "x");
    let found = stream_entry_differences(&want, &app, &mut Checked::default());
    assert!(found
        .iter()
        .any(|line| line.path == first && line.what.contains("SHA-256")));

    app.stream_content.get_mut(&first).unwrap().0 += 1;
    let found = stream_entry_differences(&want, &app, &mut Checked::default());
    assert!(found
        .iter()
        .any(|line| line.path == first && line.what.contains("size")));

    app.streams.remove(&first);
    app.stream_content.remove(&first);
    let found = stream_entry_differences(&want, &app, &mut Checked::default());
    assert!(found
        .iter()
        .any(|line| line.path == first && line.what.contains("no entry")));

    app.streams.insert("invented.txt:extra".into(), 1);
    let found = stream_entry_differences(&want, &app, &mut Checked::default());
    assert!(found.iter().any(|line| line.path == "invented.txt:extra"));
}
