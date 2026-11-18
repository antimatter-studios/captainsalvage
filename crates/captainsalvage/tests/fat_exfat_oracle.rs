//! The FAT12, FAT16, FAT32 and exFAT flows against things that are not us.
//!
//! The app reads these through the rust-fs-fat and rust-fs-exfat drivers, and the tests in
//! `fat.rs` and `synthetic.rs` read images those tests built by hand. This file reads images that
//! nobody here built: `mkfs.fat` and `mkfs.exfat` format them, the Linux kernel's vfat and exfat
//! drivers fill them through a read-write loop mount, and the same kernel (read-only), mtools (FAT
//! only) and the formatter's own checker (`fsck.fat -n`, `fsck.exfat -n`) report what is in them.
//! They run in the Linux test VM (`validation/fat-oracle.sh`, started by `chore test:oracle`) and
//! write every tree as paths, sizes and SHA-256 hashes. The app must then:
//!
//! - list the same files and folders, with the same size and SHA-256 for every file, read in full
//!   by the app's own range reader, and call every one of them live;
//! - copy out files whose bytes are the oracle's bytes;
//! - find the two files the oracle wrote last and deleted, and read them as they were written;
//! - leave the image as it was (its hash, and the app's own check that the source is unchanged).
//!
//! The images differ in what the not-yet-validated list of rust-fs-fat names: 4096-byte sectors
//! (FAT12, FAT16, FAT32 and exFAT), volumes over 40 MiB (a 60 MiB FAT12 with 16 KiB clusters, a
//! 64 MiB FAT32, a 64 MiB FAT16, a 272 MiB FAT32), and a volume at a 1 MiB offset inside a disk
//! with a partition table that `sfdisk` wrote (opened through that table, as the window does).
//!
//! Damage is made by `validation/damage.py` on copies of the 512-byte-sector FAT12, FAT16, FAT32
//! and exFAT volumes: a zeroed FAT, a chain cut short, a chain that loops, a chain cross-linked
//! into another file's, FAT copies that differ, and a wrong exFAT entry-set checksum. The formatter's
//! own checker must object to every one, so a clean verdict is not vacuous. The oracles cannot
//! read most damaged volumes, so what the app is held to is what no reader may break: a file the
//! damage cannot reach is read exactly as before, nothing is listed that the volume never held, and
//! a file the app calls live reads with the bytes the oracle read from the undamaged volume.
//!
//! A limit, issue #234: the pinned rust-fs-exfat driver refuses an exFAT volume with 4096-byte
//! sectors when its structures are read. The exFAT image with 4096-byte sectors is made, and the
//! test asserts that the app refuses it with `unsupported sector size 4096` and shows nothing, so a
//! change that opens it, or opens it wrongly, fails; every other exFAT image is held to the full
//! comparison.
//!
//! What this does not check, so that nobody reads more into a pass than is there:
//!
//! - sectors of 1024 or 2048 bytes, and volumes over 272 MiB (large exFAT and FAT32 volumes);
//! - damage on the 4096-byte-sector and offset volumes (the recipes work on any sector size, the
//!   images were left out to keep the run short), and damage beyond the recipes: a damaged exFAT
//!   allocation bitmap or boot region, a FAT whose clusters are cross-linked between folders;
//! - a second reader for exFAT: the kernel and the formatter's checker are the oracles, and mtools
//!   does not read exFAT. mtools also reads only the 512-byte-sector FAT volumes here;
//! - the macOS FAT and exFAT implementations and Windows (rust-fs-exfat and rust-fs-fat check
//!   those against their own oracles);
//! - volume labels with the Windows-only features, files with extended attributes, and names the
//!   kernel does not allow.
//!
//! Behind the `oracle` feature, and it FAILS, never skips, when the oracles cannot run.
#![cfg(feature = "oracle")]

use captainsalvage::disk::{DiskRegion, RawDisk};
use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::export::CopyRequest;
use captainsalvage::scan::{run_scan, ScanOptions};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};

/// The images `validation/fat-oracle.sh` makes. A script that quietly makes fewer fails.
const IMAGES: [&str; 12] = [
    "fat12-512",
    "fat12-big",
    "fat12-4096",
    "fat16-512",
    "fat16-4096",
    "fat16-offset",
    "fat32-512",
    "fat32-offset",
    "fat32-4096",
    "exfat-512",
    "exfat-4096",
    "exfat-offset",
];

/// The damaged copies it makes: the image, then the recipes.
const DAMAGED: [(&str, &[&str]); 4] = [
    (
        "fat12-512",
        &[
            "fat-zeroed",
            "chain-cut",
            "chain-loop",
            "chain-crosslinked",
            "differing-fats",
        ],
    ),
    (
        "fat16-512",
        &[
            "fat-zeroed",
            "chain-cut",
            "chain-loop",
            "chain-crosslinked",
            "differing-fats",
        ],
    ),
    (
        "fat32-512",
        &[
            "fat-zeroed",
            "chain-cut",
            "chain-loop",
            "chain-crosslinked",
            "differing-fats",
        ],
    ),
    (
        "exfat-512",
        &[
            "chain-cut",
            "chain-loop",
            "chain-crosslinked",
            "entry-checksum",
        ],
    ),
];

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
        PathBuf::from("/share/fat-oracle")
    } else {
        repo().join(".vm-share/fat-oracle")
    }
}

/// Runs the oracles in the guest and returns the folder of what they produced.
fn oracle_output() -> &'static PathBuf {
    static OUT: OnceLock<PathBuf> = OnceLock::new();
    OUT.get_or_init(|| {
        let share = share_dir();
        let _ = std::fs::remove_dir_all(&share);
        std::fs::create_dir_all(&share).unwrap();
        let status = if in_guest() {
            Command::new("bash")
                .arg(repo().join("validation/fat-oracle.sh"))
                .arg(&share)
                .status()
        } else {
            let vm = std::env::var("FLTH_VM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| repo().join("../fs-linux-test-harness/scripts/vm.sh"));
            let up = Command::new(&vm).current_dir(repo()).arg("up").status();
            assert!(
                matches!(&up, Ok(status) if status.success()),
                "the Linux test VM would not start ({up:?}), so mkfs.fat, mkfs.exfat and the \
                 kernel's vfat and exfat drivers cannot run. `chore siblings` checks out \
                 ../fs-linux-test-harness, and `chore test:oracle` boots the guest."
            );
            Command::new(&vm)
                .current_dir(repo())
                .args([
                    "exec",
                    "bash /repo/validation/fat-oracle.sh /share/fat-oracle",
                ])
                .status()
        };
        assert!(
            matches!(&status, Ok(status) if status.success()),
            "the independent oracles did not run ({status:?}). They need dosfstools, exfatprogs \
             and mtools and the kernel's vfat and exfat drivers, which `chore test:oracle` \
             provides in the Linux test VM (scripts/vm-setup.sh). validation/fat-oracle.sh also \
             fails when the kernel and mtools disagree with each other."
        );
        share.join("out")
    })
}

/// A throwaway home folder: opening a volume inside a disk keeps its scan in the user's catalog.
fn home() {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-fat-exfat-oracle-home-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("HOME", &path);
        path
    });
}

// ----------------------------------------------------------- what is read

#[derive(Debug, Default, PartialEq, Eq)]
struct Tree {
    dirs: BTreeSet<String>,
    /// path -> (size, sha256)
    files: BTreeMap<String, (u64, String)>,
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

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// The hash of a whole file, read in pieces: some images are over 250 MiB.
fn file_sha(path: &Path) -> String {
    let mut file = std::fs::File::open(path).unwrap();
    let mut hasher = Sha256::new();
    let mut buffer = vec![0u8; 1 << 20];
    loop {
        let read = file.read(&mut buffer).unwrap();
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

// ------------------------------------------------------------ the images

/// One line of `cases.tsv`.
#[derive(Debug, Clone)]
struct Case {
    name: String,
    kind: String,
    sector: u64,
    offset: u64,
    size: u64,
    mtools: bool,
}

impl Case {
    fn image(&self) -> PathBuf {
        oracle_output().join(format!("{}.img", self.name))
    }

    fn oracle(&self) -> Tree {
        read_tree(&oracle_output().join(format!("{}.kernel.tsv", self.name)))
    }
}

fn cases() -> Vec<Case> {
    let text = std::fs::read_to_string(oracle_output().join("cases.tsv")).unwrap();
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 7, "{line}");
            Case {
                name: fields[0].to_string(),
                kind: fields[1].to_string(),
                sector: fields[2].parse().unwrap(),
                offset: fields[4].parse().unwrap(),
                size: fields[5].parse().unwrap(),
                mtools: fields[6] == "1",
            }
        })
        .collect()
}

fn cases_of(kind: &str) -> Vec<Case> {
    let found: Vec<Case> = cases().into_iter().filter(|c| c.kind == kind).collect();
    assert!(!found.is_empty(), "the oracle made no {kind} image");
    found
}

/// The exit status `fsck.fat -n` or `fsck.exfat -n` recorded.
fn fsck_exit(name: &str) -> i32 {
    std::fs::read_to_string(oracle_output().join(format!("{name}.fsck")))
        .unwrap()
        .lines()
        .next()
        .and_then(|line| line.strip_prefix("exit="))
        .and_then(|number| number.parse().ok())
        .expect("the checker's record starts with exit=N")
}

// ------------------------------------------------------------ the app

static NEXT: AtomicU64 = AtomicU64::new(0);

fn workspace(label: &str) -> Workspace {
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-fat-exfat-oracle-{}-{}-{label}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&path);
    Workspace::at(&path).unwrap()
}

fn scan(session: &Session) {
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions {
            chunk_clusters: 1024,
        },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
}

fn engine_call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

/// Opens an image the way the window does: a volume at the start of the source as itself, and a
/// volume inside a disk through the partition table the oracle wrote.
fn open_image(case: &Case, image: &Path, label: &str) -> Arc<Session> {
    if case.offset == 0 {
        return Session::with_source(
            Path::new("/test/oracle-image"),
            Arc::new(MemorySource::new(std::fs::read(image).unwrap())),
            workspace(label),
        )
        .unwrap();
    }
    home();
    let raw = RawDisk::open(image).unwrap();
    let overview = raw.overview();
    assert_eq!(
        overview.partitions.len(),
        1,
        "{}: sfdisk wrote one partition, and the app reads {:?}",
        case.name,
        overview.partitions
    );
    let partition = &overview.partitions[0];
    assert_eq!(
        (partition.start, partition.length),
        (case.offset, case.size),
        "{}: the app reads the partition table differently from sfdisk",
        case.name
    );
    Session::open_region(
        &raw,
        DiskRegion {
            start: partition.start,
            length: partition.length,
            format: case.kind.clone(),
            label: format!("{} in a partition", case.kind),
            boot_offset: partition.start,
        },
    )
    .unwrap()
}

/// A live file or folder the app lists: not a leftover deleted entry, and not one found without
/// its folder (those are named `[orphan @N]`).
struct Listed {
    id: i64,
    path: String,
    length: u64,
    verdict: String,
    is_dir: bool,
}

fn listed(session: &Arc<Session>) -> Vec<Listed> {
    let rows = session
        .query_map(
            "SELECT id, verdict, is_dir FROM sets WHERE in_use = 1 AND checksum_ok = 1 ORDER BY id",
        )
        .unwrap();
    let mut found = Vec::new();
    for row in rows["rows"].as_array().unwrap() {
        let id = row["id"].as_i64().unwrap();
        let entry = session
            .with_database(|database| Ok(database.entry(id)?.unwrap()))
            .unwrap();
        let folder = entry.path.clone().unwrap_or_default();
        if folder.starts_with('[') || entry.name == "." || entry.name == ".." {
            continue;
        }
        let folder = folder.trim_start_matches('/');
        let path = if folder.is_empty() {
            entry.name.clone()
        } else {
            format!("{folder}/{}", entry.name)
        };
        found.push(Listed {
            id,
            path,
            length: entry.length,
            verdict: row["verdict"].as_str().unwrap_or("").to_string(),
            is_dir: row["is_dir"].as_i64() == Some(1),
        });
    }
    found
}

/// What the app reports for the live files and folders, every file read in full.
fn app_tree(session: &Arc<Session>) -> (Tree, Vec<String>) {
    let mut tree = Tree::default();
    let mut not_live = Vec::new();
    for item in listed(session) {
        if item.is_dir {
            tree.dirs.insert(item.path);
            continue;
        }
        let data = session
            .read_entry_range(item.id, 0, item.length + 8)
            .unwrap_or_else(|error| panic!("{}: {error:#}", item.path))
            .data;
        assert_eq!(data.len() as u64, item.length, "{}", item.path);
        // A file both oracles read from the live tree must be called live by the app: anything
        // less says a file that is plainly there may be gone. An empty one has nothing to be.
        if item.length > 0 && item.verdict != "live" {
            not_live.push(format!("{} is called {}", item.path, item.verdict));
        }
        tree.files.insert(item.path, (item.length, sha(&data)));
    }
    (tree, not_live)
}

/// Copies every live file out and returns what landed in the folder, by path.
fn copied_files(session: &Arc<Session>, label: &str) -> BTreeMap<String, (u64, String)> {
    let ids: Vec<i64> = listed(session)
        .into_iter()
        .filter(|item| !item.is_dir)
        .map(|item| item.id)
        .collect();
    let destination = session.workspace.dir().join(format!("copied-{label}"));
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: ids.clone(),
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(status.files_done, ids.len() as u64, "{status:?}");
            break;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(
            std::time::Instant::now() < deadline,
            "copy timed out: {status:?}"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let root = destination.join("Volume root");
    let mut copied = BTreeMap::new();
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
    copied.retain(|name, _| !name.starts_with('.'));
    copied
}

/// KNOWN LIMIT, issue #234: the driver reads exFAT structures at 512-byte sectors only. The app
/// must say so when the volume is opened, and show nothing of it.
fn exfat_with_4096_byte_sectors_is_refused(case: &Case) {
    let image = case.image();
    let before = file_sha(&image);
    let opened = Session::with_source(
        Path::new("/test/oracle-image"),
        Arc::new(MemorySource::new(std::fs::read(&image).unwrap())),
        workspace(&case.name),
    );
    let error = match opened {
        Ok(_) => panic!(
            "{}: the app opened an exFAT volume with 4096-byte sectors",
            case.name
        ),
        Err(error) => format!("{error:#}"),
    };
    assert!(
        error.contains("unsupported sector size 4096"),
        "{}: the app refused it with another message: {error}",
        case.name
    );
    // The oracle read it: the refusal is the app's limit, not a broken image.
    assert!(case.oracle().files.len() > 130, "{}", case.name);
    assert_eq!(
        file_sha(&image),
        before,
        "{}: the image was written",
        case.name
    );
}

/// The checks every clean image has to pass.
fn read_like_the_oracles(case: &Case) {
    if case.kind == "exfat" && case.sector == 4096 {
        return exfat_with_4096_byte_sectors_is_refused(case);
    }
    let oracle = case.oracle();
    assert!(
        oracle.files.len() > 130 && oracle.dirs.len() >= 4,
        "{}: the oracle's own image is too small ({} files)",
        case.name,
        oracle.files.len()
    );
    assert_eq!(
        oracle
            .files
            .keys()
            .filter(|p| p.starts_with("many/"))
            .count(),
        120,
        "{}: the folder of 120 files is not there",
        case.name
    );
    let image = case.image();
    let before = file_sha(&image);
    let session = open_image(case, &image, &case.name);
    scan(&session);
    let (app, not_live) = app_tree(&session);
    let found = differences(&oracle, &app);
    assert_eq!(
        found, "",
        "the app and the oracles disagree about {}:\n{found}",
        case.name
    );
    assert!(
        not_live.is_empty(),
        "{}: the oracles read these files, and the app does not call them live:\n{}",
        case.name,
        not_live.join("\n")
    );
    // The image holds the shapes the formatter and the kernel made, not only the ones a driver does.
    for name in ["fraga.bin", "fragb.bin", "docs/nested/deeper/leaf.bin"] {
        assert!(oracle.files.contains_key(name), "{}: {name}", case.name);
    }

    // The two files the oracle wrote last and removed: the scan finds them, and what was written
    // still reads from the clusters nothing has been written over.
    let engine = Engine::new();
    engine.set_session(session.clone());
    engine_call(
        &engine,
        "set_parameters",
        json!({"chainFallbackContiguous": true}),
    );
    let deleted =
        std::fs::read_to_string(oracle_output().join(format!("{}.deleted.tsv", case.name)))
            .unwrap();
    assert_eq!(deleted.lines().count(), 2, "{}", case.name);
    for line in deleted.lines() {
        let fields: Vec<&str> = line.splitn(4, '\t').collect();
        let (size, sum, file) = (fields[1].parse::<u64>().unwrap(), fields[2], fields[3]);
        let rows = session
            .query_map(&format!(
                "SELECT id FROM sets WHERE in_use = 0 AND length = {size} AND is_dir = 0"
            ))
            .unwrap();
        let ids: Vec<i64> = rows["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["id"].as_i64().unwrap())
            .collect();
        assert_eq!(
            ids.len(),
            1,
            "{}: the scan finds {} deleted entries of {size} bytes, for {file}",
            case.name,
            ids.len()
        );
        let data = session
            .read_entry_range(ids[0], 0, size + 8)
            .unwrap_or_else(|error| panic!("{}: {file}: {error:#}", case.name))
            .data;
        assert_eq!(
            sha(&data),
            sum,
            "{}: deleted {file} no longer reads as it was written",
            case.name
        );
    }

    // What the app copies is byte for byte what the oracle read.
    let copied = copied_files(&session, &case.name);
    assert_eq!(
        copied, oracle.files,
        "{}: the copy differs from what the kernel reads",
        case.name
    );

    assert_eq!(
        file_sha(&image),
        before,
        "{}: the image was written",
        case.name
    );
    assert!(session.verify_unchanged().unwrap().unchanged);
}

// ------------------------------------------------------------------ tests

#[test]
fn the_oracles_ran_and_agree_with_each_other_about_every_image() {
    let out = oracle_output();
    let tools = std::fs::read_to_string(out.join("tools.txt")).unwrap();
    for needed in ["kernel 6.", "dosfstools", "exfatprogs", "mtools"] {
        assert!(tools.contains(needed), "{needed} is not in:\n{tools}");
    }
    let found = cases();
    let names: Vec<&str> = found.iter().map(|case| case.name.as_str()).collect();
    assert_eq!(names, IMAGES, "the oracle script made other images");
    for case in &found {
        let from_kernel = case.oracle();
        assert!(
            from_kernel.files.len() > 130,
            "{} listed {} files",
            case.name,
            from_kernel.files.len()
        );
        assert_eq!(
            fsck_exit(&case.name),
            0,
            "{}'s own checker found a problem in the volume it made",
            case.name
        );
        assert!(case.image().exists(), "{}", case.name);
        if case.mtools {
            // The script fails when they differ; this says the second reader was there.
            let from_mtools = read_tree(&out.join(format!("{}.mtools.tsv", case.name)));
            assert_eq!(from_mtools, from_kernel, "{}", case.name);
        } else {
            assert!(
                case.sector != 512 || case.kind == "exfat",
                "{} has 512-byte sectors, so mtools should have read it",
                case.name
            );
        }
    }
    assert!(
        found
            .iter()
            .any(|case| case.sector == 4096 && case.kind == "exfat")
            && found.iter().any(|case| case.offset > 0),
        "the images lack 4096-byte sectors or a volume inside a disk"
    );
}

#[test]
fn a_changed_byte_in_what_an_oracle_reports_fails_the_comparison() {
    let file = |size: u64, sum: &str| (size, sum.to_string());
    let oracle = Tree {
        dirs: ["docs".to_string()].into(),
        files: [
            ("a.txt".to_string(), file(3, "aa")),
            ("docs/b.txt".to_string(), file(5, "bb")),
        ]
        .into(),
    };
    let mut app = Tree {
        dirs: oracle.dirs.clone(),
        files: oracle.files.clone(),
    };
    assert_eq!(differences(&oracle, &app), "");
    app.files
        .get_mut("a.txt")
        .unwrap()
        .1
        .replace_range(0..1, "x");
    assert!(differences(&oracle, &app).contains("a.txt"));
    app.files.remove("a.txt");
    assert!(differences(&oracle, &app).contains("does not list"));
    app.files.insert("extra.txt".to_string(), file(1, "cc"));
    assert!(differences(&oracle, &app).contains("which the oracle does not"));
    app.dirs.clear();
    assert!(differences(&oracle, &app).contains("directory docs"));
}

#[test]
fn fat12_images_are_read_like_the_oracles_read_them() {
    for case in cases_of("fat12") {
        read_like_the_oracles(&case);
    }
}

#[test]
fn fat16_images_are_read_like_the_oracles_read_them() {
    for case in cases_of("fat16") {
        read_like_the_oracles(&case);
    }
}

#[test]
fn fat32_images_are_read_like_the_oracles_read_them() {
    for case in cases_of("fat32") {
        read_like_the_oracles(&case);
    }
}

#[test]
fn exfat_images_are_read_like_the_oracles_read_them() {
    for case in cases_of("exfat") {
        read_like_the_oracles(&case);
    }
}

#[test]
fn every_damaged_volume_is_one_the_formatters_own_checker_objects_to() {
    let out = oracle_output();
    let text = std::fs::read_to_string(out.join("damage.tsv")).unwrap();
    let mut names: Vec<String> = Vec::new();
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        assert_eq!(fields.len(), 4, "{line}");
        names.push(fields[0].to_string());
        assert_ne!(
            fsck_exit(fields[0]),
            0,
            "the checker accepts {}, so {} damaged nothing it looks at",
            fields[0],
            fields[2]
        );
    }
    let expected: Vec<String> = DAMAGED
        .iter()
        .flat_map(|(image, recipes)| {
            recipes
                .iter()
                .map(move |recipe| format!("{image}-{recipe}"))
        })
        .collect();
    assert_eq!(
        names, expected,
        "the oracle script made other damaged copies"
    );
}

#[test]
fn a_damaged_volume_never_shows_wrong_bytes_as_a_file_that_is_there() {
    let out = oracle_output();
    let text = std::fs::read_to_string(out.join("damage.tsv")).unwrap();
    assert!(!text.is_empty());
    for line in text.lines() {
        let fields: Vec<&str> = line.split('\t').collect();
        let (name, parent, recipe, touched) = (fields[0], fields[1], fields[2], fields[3]);
        let truth = read_tree(&out.join(format!("{parent}.kernel.tsv")));
        let case = cases()
            .into_iter()
            .find(|case| case.name == *parent)
            .unwrap();
        let image = out.join(format!("{name}.img"));
        let before = file_sha(&image);
        let session = open_image(&case, &image, name);
        scan(&session);
        let touched: Vec<&str> = touched.split(',').collect();
        let any = touched.contains(&"*");
        let mut problems = Vec::new();
        let mut seen = BTreeSet::new();
        for item in listed(&session).into_iter().filter(|item| !item.is_dir) {
            let read = session.read_entry_range(item.id, 0, item.length + 8);
            let Some(want) = truth.files.get(&item.path) else {
                problems.push(format!(
                    "the app lists {}, which the volume never held",
                    item.path
                ));
                continue;
            };
            seen.insert(item.path.clone());
            let reached = any || touched.contains(&item.path.as_str());
            match read {
                Ok(range) => {
                    let have = (range.data.len() as u64, sha(&range.data));
                    if have != *want {
                        if !reached {
                            problems.push(format!(
                                "{} cannot be reached by the damage, and reads {have:?}, not {want:?}",
                                item.path
                            ));
                        } else if item.verdict == "live" {
                            problems.push(format!(
                                "{} is called live and reads {have:?}, not {want:?}",
                                item.path
                            ));
                        }
                    }
                }
                Err(error) if !reached => {
                    problems.push(format!(
                        "{} cannot be reached by the damage, and does not read: {error:#}",
                        item.path
                    ));
                }
                Err(_) => {}
            }
        }
        if !any {
            for path in truth.files.keys() {
                if !touched.contains(&path.as_str()) && !seen.contains(path) {
                    problems.push(format!(
                        "{path} cannot be reached by the damage, and the app does not list it"
                    ));
                }
            }
        }
        assert!(
            problems.is_empty(),
            "{name} ({recipe} on {parent}):\n  {}",
            problems.join("\n  ")
        );
        assert_eq!(file_sha(&image), before, "{name}: the image was written");
        assert!(session.verify_unchanged().unwrap().unchanged, "{name}");
    }
}
