//! The NTFS flows against things that are not us.
//!
//! The app reads NTFS through the rust-fs-ntfs driver, and the tests in `ntfs.rs` compare it with
//! another Rust reader. This file compares it with the two implementations nobody here wrote:
//! ntfs-3g's own tools (`ntfsls`, `ntfscat`, `ntfsfix -n`) and the Linux kernel's ntfs3 driver on a
//! loop mount. They run in the Linux test VM (`validation/oracle.sh`, started by `chore test:oracle`)
//! and report every tree as paths, sizes and SHA-256 hashes. Two directions:
//!
//! - images the rust-fs-ntfs driver made and wrote: both oracles must read them, and the app's
//!   scan, browse, read and copy must report what they report;
//! - images the oracles' own formatter and writer made (`mkntfs`, filled through a read-write
//!   ntfs3 mount): the app must read every file they hold, and find the files they deleted.
//!
//! Behind the `oracle` feature, and it FAILS, never skips, when the oracles cannot run.
#![cfg(feature = "oracle")]

mod ntfs_support;

use captainsalvage::engine::Engine;
use captainsalvage::export::CopyRequest;
use captainsalvage::session::Session;
use fs_ntfs::block_io::BlockIo;
use ntfs_support::*;
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};

// ------------------------------------------------------------------ the VM

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn in_guest() -> bool {
    std::env::var("FLTH_GUEST").as_deref() == Ok("1")
}

/// Where the guest and the host each see the share.
fn share_dir() -> PathBuf {
    if in_guest() {
        PathBuf::from("/share/ntfs-oracle")
    } else {
        repo().join(".vm-share/ntfs-oracle")
    }
}

/// The images the driver makes for the oracles to read, by name.
/// A named stream the driver wrote: the file, the stream's name, and its bytes.
type Stream = (&'static str, &'static str, Vec<u8>);

fn driver_images() -> Vec<(&'static str, Fixture, Vec<Stream>)> {
    let streams: Vec<Stream> = vec![("docs/note.txt", "meta", b"alternate stream bytes".to_vec())];
    let mut fixture = fixture();
    let mut device = Image(Arc::new(Mutex::new(fixture.image.clone())));
    for (file, stream, data) in &streams {
        fs_ntfs::write::write_named_stream_io(&mut device, &format!("/{file}"), stream, data)
            .unwrap();
    }
    device.sync().unwrap();
    fixture.image = device.0.lock().unwrap().clone();
    vec![("driver-4096", fixture, streams)]
}

/// Writes the driver's images, runs the oracles over them in the guest, and returns the folder
/// of what they produced.
fn oracle_output() -> &'static PathBuf {
    static OUT: OnceLock<PathBuf> = OnceLock::new();
    OUT.get_or_init(|| {
        let share = share_dir();
        let input = share.join("in");
        let _ = std::fs::remove_dir_all(&share);
        std::fs::create_dir_all(&input).unwrap();
        for (name, fixture, streams) in driver_images() {
            std::fs::write(input.join(format!("{name}.img")), &fixture.image).unwrap();
            let lines: String = streams
                .iter()
                .map(|(file, stream, _)| format!("{file}\t{stream}\n"))
                .collect();
            std::fs::write(input.join(format!("{name}.streams")), lines).unwrap();
        }
        let status = if in_guest() {
            Command::new("bash")
                .arg(repo().join("validation/oracle.sh"))
                .arg(&share)
                .status()
        } else {
            let vm = std::env::var("FLTH_VM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| repo().join("../fs-linux-test-harness/scripts/vm.sh"));
            let up = Command::new(&vm).current_dir(repo()).arg("up").status();
            assert!(
                matches!(&up, Ok(status) if status.success()),
                "the Linux test VM would not start ({up:?}), so ntfs-3g and ntfs3 cannot run. \
                 `chore siblings` checks out ../fs-linux-test-harness, and `chore test:oracle` \
                 boots the guest."
            );
            Command::new(&vm)
                .current_dir(repo())
                .args(["exec", "bash /repo/validation/oracle.sh /share/ntfs-oracle"])
                .status()
        };
        assert!(
            matches!(&status, Ok(status) if status.success()),
            "the independent oracles did not run ({status:?}). They need ntfs-3g's tools and \
             the kernel's ntfs3 driver, which `chore test:oracle` provides in the Linux test \
             VM (scripts/vm-setup.sh). validation/oracle.sh also fails when ntfs-3g and ntfs3 \
             disagree with each other."
        );
        share.join("out")
    })
}

// ----------------------------------------------------------- what is read

#[derive(Debug, Default, PartialEq, Eq)]
struct Tree {
    dirs: BTreeSet<String>,
    /// path -> (size, sha256)
    files: BTreeMap<String, (u64, String)>,
    /// `path:stream` -> (size, sha256)
    streams: BTreeMap<String, (u64, String)>,
}

fn read_tree(file: &Path) -> Tree {
    let text = std::fs::read_to_string(file)
        .unwrap_or_else(|error| panic!("read {}: {error}", file.display()));
    let mut tree = Tree::default();
    for line in text.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        match fields.as_slice() {
            ["d", path] => {
                tree.dirs.insert((*path).to_string());
            }
            ["f", size, sha, path] => {
                tree.files.insert(
                    (*path).to_string(),
                    (size.parse().unwrap(), (*sha).to_string()),
                );
            }
            ["s", size, sha, path] => {
                tree.streams.insert(
                    (*path).to_string(),
                    (size.parse().unwrap(), (*sha).to_string()),
                );
            }
            other => panic!("{}: unexpected line {other:?}", file.display()),
        }
    }
    tree
}

/// Every difference, one per line, so a failure names each path.
fn differences(oracle: &Tree, app: &Tree) -> String {
    let mut out = String::new();
    for (path, want) in &oracle.files {
        match app.files.get(path) {
            None => out += &format!("  the app does not list {path}\n"),
            Some(have) if have != want => {
                out += &format!("  {path}: oracle {want:?}, app {have:?}\n")
            }
            Some(_) => {}
        }
    }
    for path in app
        .files
        .keys()
        .filter(|path| !oracle.files.contains_key(*path))
    {
        out += &format!("  the app lists {path}, which the oracle does not\n");
    }
    for path in oracle.dirs.symmetric_difference(&app.dirs) {
        out += &format!("  directory {path}: in only one of them\n");
    }
    out
}

/// What the app reports for the live, non-system files and folders: every one read in full.
fn app_tree(session: &Arc<Session>) -> Tree {
    let ids = |sql: &str| -> Vec<i64> {
        session.query_map(sql).unwrap()["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect()
    };
    let mut tree = Tree::default();
    let entry = |id: i64| {
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
    };
    let system = |path: &str| path.split('/').any(|part| part.starts_with('$'));
    for id in ids("SELECT id FROM sets WHERE is_dir = 0 AND in_use = 1 AND checksum_ok = 1") {
        let (row, path) = entry(id);
        // NTFS named streams are reported separately by the oracles, not as ordinary files.
        if system(&path) || row.name.contains(':') {
            continue;
        }
        let data = session
            .read_entry_range(id, 0, row.length + 8)
            .unwrap_or_else(|error| panic!("{path}: {error:#}"))
            .data;
        assert_eq!(data.len() as u64, row.length, "{path}");
        tree.files.insert(path, (row.length, sha(&data)));
    }
    for id in ids("SELECT id FROM sets WHERE is_dir = 1 AND in_use = 1 AND checksum_ok = 1") {
        let (row, path) = entry(id);
        if system(&path) || row.name == "." {
            continue;
        }
        tree.dirs.insert(path);
    }
    tree
}

/// A file both oracles read from the live tree must be called live by the app: anything less
/// says a file that is plainly there may be gone.
fn assert_live(session: &Arc<Session>, name: &str) {
    let not_live = session
        .query_map(
            "SELECT name, verdict FROM sets WHERE is_dir = 0 AND in_use = 1 AND checksum_ok = 1 \
             AND length > 0 AND name NOT LIKE '$%' AND node IN (SELECT head FROM nodes WHERE kind IN ('root', 'live')) \
             AND verdict != 'live' AND node NOT IN (SELECT head FROM nodes WHERE name LIKE '$%')",
        )
        .unwrap();
    assert_eq!(
        not_live["rows"].as_array().unwrap().len(),
        0,
        "{name}: {not_live}"
    );
}

fn scanned(image: &Path, label: &str) -> Arc<Session> {
    let session = Session::with_source(
        Path::new("/test/oracle-image"),
        Arc::new(captainsalvage::source::MemorySource::new(
            std::fs::read(image).unwrap(),
        )),
        workspace(label),
    )
    .unwrap();
    scan(&session);
    session
}

fn fix_exit(name: &str) -> i32 {
    std::fs::read_to_string(oracle_output().join(format!("{name}.ntfsfix")))
        .unwrap()
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("exit="))
        .and_then(|number| number.parse().ok())
        .expect("the ntfsfix record starts with exit=N")
}

// ------------------------------------------------------------------ tests

#[test]
fn the_oracles_ran_and_agree_with_each_other_about_every_image() {
    let out = oracle_output();
    let tools = std::fs::read_to_string(out.join("tools.txt")).unwrap();
    assert!(
        tools.contains("kernel 6.") && tools.contains("ntfs-3g"),
        "{tools}"
    );
    for name in ["driver-4096", "mkntfs-512", "mkntfs-4096"] {
        let from_tools = read_tree(&out.join(format!("{name}.ntfs3g.tsv")));
        let from_kernel = read_tree(&out.join(format!("{name}.ntfs3.tsv")));
        assert!(!from_kernel.files.is_empty(), "{name} listed nothing");
        assert_eq!(from_tools.files, from_kernel.files, "{name}");
        assert_eq!(from_tools.dirs, from_kernel.dirs, "{name}");
        assert_eq!(fix_exit(name), 0, "ntfsfix -n found a problem in {name}");
    }
}

#[test]
fn a_changed_byte_in_what_an_oracle_reports_fails_the_comparison() {
    let out = oracle_output();
    let oracle = read_tree(&out.join("mkntfs-4096.ntfs3.tsv"));
    let mut app = Tree {
        dirs: oracle.dirs.clone(),
        files: oracle.files.clone(),
        streams: BTreeMap::new(),
    };
    assert_eq!(differences(&oracle, &app), "");
    let first = app.files.keys().next().unwrap().clone();
    app.files
        .get_mut(&first)
        .unwrap()
        .1
        .replace_range(0..1, "x");
    assert!(differences(&oracle, &app).contains(&first));
    app.files.remove(&first);
    assert!(differences(&oracle, &app).contains("does not list"));
}

#[test]
fn what_the_driver_wrote_is_read_the_same_by_ntfs3g_ntfs3_and_the_app() {
    let out = oracle_output();
    let image = share_dir().join("in/driver-4096.img");
    let oracle = read_tree(&out.join("driver-4096.ntfs3g.tsv"));
    let kernel = read_tree(&out.join("driver-4096.ntfs3.tsv"));
    let before = sha(&std::fs::read(&image).unwrap());
    let session = scanned(&image, "driver-oracle");
    let app = app_tree(&session);
    // What was written is what both oracles read, and what the app reports.
    let (_, fixture, streams) = driver_images().remove(0);
    for (path, content) in &fixture.files {
        let name = &path[1..];
        if name.starts_with("gone") {
            continue;
        }
        let want = (content.len() as u64, sha(content));
        assert_eq!(oracle.files.get(name), Some(&want), "ntfs-3g: {name}");
        assert_eq!(kernel.files.get(name), Some(&want), "ntfs3: {name}");
    }
    let found = differences(&oracle, &app);
    assert_eq!(found, "", "the app and the oracles disagree:\n{found}");
    assert_live(&session, "driver-4096");
    // The alternate stream: ntfs-3g reads it, and the app lists it with the same length.
    for (file, stream, data) in &streams {
        let key = format!("{file}:{stream}");
        assert_eq!(
            oracle.streams.get(&key),
            Some(&(data.len() as u64, sha(data))),
            "ntfs-3g: {key}"
        );
        let engine = Engine::new();
        engine.set_session(session.clone());
        let row = find(&engine, file.rsplit('/').next().unwrap());
        let details = call(&engine, "ntfs_entry", json!({"id": row["id"]}));
        let listed = details["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["name"] == *stream)
            .unwrap_or_else(|| panic!("the app does not list {key}"));
        assert_eq!(listed["length"], data.len());
    }
    // Deleted files, which neither oracle can see, are found by the scan.
    let engine = Engine::new();
    engine.set_session(session.clone());
    let gone = find(&engine, "gone.bin");
    assert_eq!(gone["inUse"], false);
    assert_eq!(
        sha(&session
            .read_entry_range(gone["id"].as_i64().unwrap(), 0, 12_000)
            .unwrap()
            .data),
        sha(fixture.content("/gone.bin"))
    );
    assert_eq!(
        sha(&std::fs::read(&image).unwrap()),
        before,
        "the image was written"
    );
    assert!(session.verify_unchanged().unwrap().unchanged);
}

#[test]
fn what_the_app_copies_is_what_the_oracles_read() {
    let out = oracle_output();
    let oracle = read_tree(&out.join("driver-4096.ntfs3g.tsv"));
    let session = scanned(&share_dir().join("in/driver-4096.img"), "driver-copy");
    let ids: Vec<i64> = session
        .query_map("SELECT id FROM sets WHERE is_dir = 0 AND in_use = 1 AND checksum_ok = 1")
        .unwrap()["rows"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["id"].as_i64().unwrap())
        .collect();
    let mut wanted = Vec::new();
    for id in ids {
        let row = session
            .with_database(|database| Ok(database.entry(id)?.unwrap()))
            .unwrap();
        // This comparison is for ordinary files; the oracle reports named streams separately.
        if !row.name.contains(':')
            && !row
                .path
                .clone()
                .unwrap_or_default()
                .split('/')
                .any(|part| part.starts_with('$'))
            && !row.name.starts_with('$')
        {
            wanted.push(id);
        }
    }
    let destination = session.workspace.dir().join("copied");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: wanted.clone(),
            ..CopyRequest::default()
        })
        .unwrap();
    wait_for_copy(&session, wanted.len() as u64);
    let mut copied = BTreeMap::new();
    let root = destination.join("Volume root");
    let mut stack = vec![root.clone()];
    while let Some(directory) = stack.pop() {
        for item in std::fs::read_dir(&directory).unwrap() {
            let path = item.unwrap().path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let data = std::fs::read(&path).unwrap();
                let name = path
                    .strip_prefix(&root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned();
                copied.insert(name, (data.len() as u64, sha(&data)));
            }
        }
    }
    let manifest: BTreeMap<String, (u64, String)> = copied
        .into_iter()
        .filter(|(name, _)| !name.starts_with('.'))
        .collect();
    assert_eq!(
        manifest, oracle.files,
        "the copy differs from what ntfs-3g reads"
    );
}

fn mkntfs_matches(name: &str) {
    let out = oracle_output();
    let image = out.join(format!("{name}.img"));
    let oracle = read_tree(&out.join(format!("{name}.ntfs3.tsv")));
    assert!(
        oracle.files.len() > 150,
        "{name}: the oracle's own image is too small"
    );
    let before = sha(&std::fs::read(&image).unwrap());
    let session = scanned(&image, name);
    let app = app_tree(&session);
    let found = differences(&oracle, &app);
    assert_eq!(
        found, "",
        "the app and the oracles disagree about {name}:\n{found}"
    );
    // The image holds the shapes the formatter and writer made, not only the ones the driver does.
    let engine = Engine::new();
    engine.set_session(session.clone());
    let fragmented = find(&engine, "fragmented.bin");
    let runs = session
        .with_database(|database| {
            database.entry_runs(
                fragmented["sourceCluster"].as_u64().unwrap() as u32,
                fragmented["sourceOffset"].as_u64().unwrap() as u32,
            )
        })
        .unwrap();
    assert!(runs.len() >= 2, "{name}: fragmented.bin has {runs:?}");
    // Files the oracle's writer deleted are still found, and read as they were while nothing is
    // written over them.
    let deleted = std::fs::read_to_string(out.join(format!("{name}.deleted.tsv"))).unwrap();
    for line in deleted.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        let (size, sum, file) = (fields[1].parse::<u64>().unwrap(), fields[2], fields[3]);
        let row = find(&engine, file);
        assert_eq!(row["inUse"], false, "{file}");
        assert_eq!(row["length"], size, "{file}");
        let data = session
            .read_entry_range(row["id"].as_i64().unwrap(), 0, size + 8)
            .unwrap_or_else(|error| panic!("{file}: {error:#}"))
            .data;
        assert_eq!(
            sha(&data),
            sum,
            "{name}: deleted {file} no longer reads as it was written"
        );
    }
    assert_eq!(sha(&std::fs::read(&image).unwrap()), before);
    assert!(session.verify_unchanged().unwrap().unchanged);
}

#[test]
fn an_image_mkntfs_made_with_512_byte_clusters_is_read_like_the_oracles_read_it() {
    mkntfs_matches("mkntfs-512");
}

#[test]
fn an_image_mkntfs_made_with_4096_byte_clusters_is_read_like_the_oracles_read_it() {
    mkntfs_matches("mkntfs-4096");
}
