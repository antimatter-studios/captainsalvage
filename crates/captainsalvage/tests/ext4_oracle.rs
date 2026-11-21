//! ext2, ext3 and ext4 against things that are not us.
//!
//! The app reads these filesystems through the rust-fs-ext4 driver and the generic layers adapter. This file reads images
//! that nobody here built: `mke2fs` makes them from a tree (`-d`) as ext4 at 4 KiB and 1 KiB blocks, ext3 at 4 KiB and ext2 at
//! 4 KiB and 1 KiB (the last three map their files by indirect blocks), and ext4 again with small files stored in their inodes
//! (`inline_data`), with the older feature set (no 64-bit, checksums, journal or flex groups) and with linear directories
//! (no `dir_index`), `e2fsck` must call each clean, and the Linux kernel's
//! ext4 driver, mounted read-only on a loop device, reports what is in them as paths, sizes and SHA-256 hashes. They run in the
//! Linux test VM (`validation/ext4-oracle.sh`, started by `chore test:oracle`). The engine then opens each image as a person
//! opens it: scanned, browsed from the root, every file read in full by the preview's range reader and copied out, and what is
//! shown and copied is the kernel's tree, byte for byte, a symbolic link being a file that holds its target.
//!
//! What this does not check, so that nobody reads more into a pass than is there: damaged volumes and deleted files (the
//! driver's own suite checks them against the kernel), journals that need replaying, extended attributes, and any reader
//! but the kernel's.
//!
//! Behind the `oracle` feature, and it FAILS, never skips, when the oracles cannot run.
#![cfg(feature = "oracle")]

use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

/// The images `validation/ext4-oracle.sh` makes. A script that quietly makes fewer fails.
const IMAGES: [&str; 8] = [
    "ext4-4k",
    "ext4-1k",
    "ext3-4k",
    "ext2-4k",
    "ext2-1k",
    "ext4-inline",
    "ext4-legacy",
    "ext4-1k-nodirindex",
];

fn repo() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn in_guest() -> bool {
    std::env::var("FLTH_GUEST").as_deref() == Ok("1")
}

/// Where the guest and the host each see the share.
fn share_dir() -> PathBuf {
    if in_guest() {
        PathBuf::from("/share/ext4-oracle")
    } else {
        repo().join(".vm-share/ext4-oracle")
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
                .arg(repo().join("validation/ext4-oracle.sh"))
                .arg(&share)
                .status()
        } else {
            let vm = std::env::var("FLTH_VM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| repo().join("../fs-linux-test-harness/scripts/vm.sh"));
            let up = Command::new(&vm).current_dir(repo()).arg("up").status();
            assert!(
                matches!(&up, Ok(status) if status.success()),
                "the Linux test VM would not start ({up:?}), so mke2fs and the kernel's ext4 \
                 driver cannot run. `chore siblings` checks out ../fs-linux-test-harness, and \
                 `chore test:oracle` boots the guest."
            );
            Command::new(&vm)
                .current_dir(repo())
                .args([
                    "exec",
                    "bash /repo/validation/ext4-oracle.sh /share/ext4-oracle",
                ])
                .status()
        };
        assert!(
            matches!(&status, Ok(status) if status.success()),
            "the independent oracles did not run ({status:?}). They need e2fsprogs and the kernel's \
             ext4 driver, which `chore test:oracle` provides in the Linux test VM (scripts/vm-setup.sh)."
        );
        share.join("out")
    })
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Tree {
    dirs: BTreeSet<String>,
    /// path -> (size, sha256)
    files: BTreeMap<String, (u64, String)>,
    /// path -> target
    links: BTreeMap<String, String>,
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
            ["l", target, path] => {
                tree.links
                    .insert((*path).to_string(), (*target).to_string());
            }
            other => panic!("{}: unexpected line {other:?}", file.display()),
        }
    }
    tree
}

fn sha(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
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
    for (path, target) in &oracle.links {
        match app.links.get(path) {
            None => out += &format!("  the app does not list the link {path}\n"),
            Some(have) if have != target => {
                out += &format!("  {path}: oracle links to {target:?}, app to {have:?}\n")
            }
            Some(_) => {}
        }
    }
    for path in app
        .links
        .keys()
        .filter(|path| !oracle.links.contains_key(*path))
    {
        out += &format!("  the app lists the link {path}, which the oracle does not\n");
    }
    out
}

fn cases() -> Vec<(String, String, u64)> {
    let text = std::fs::read_to_string(oracle_output().join("cases.tsv")).unwrap();
    text.lines()
        .map(|line| {
            let fields: Vec<&str> = line.split('\t').collect();
            assert_eq!(fields.len(), 4, "{line}");
            (
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].parse().unwrap(),
            )
        })
        .collect()
}

fn app_view(name: &str, image: Vec<u8>) -> (Tree, BTreeMap<String, String>) {
    use captainsalvage::export::CopyRequest;
    use captainsalvage::scan::{run_scan, ScanOptions};
    use captainsalvage::session::Session;
    use captainsalvage::source::MemorySource;
    use captainsalvage::workspace::Workspace;
    use std::sync::atomic::AtomicBool;
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-ext4-oracle-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        Path::new("/test/ext4-oracle"),
        Arc::new(MemorySource::new(image)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    assert_eq!(
        session.volume.kind,
        captainsalvage::filesystem::FilesystemKind::Ext4
    );
    run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 64 },
        &AtomicBool::new(false),
        |_| {},
    )
    .unwrap();
    let mut tree = Tree::default();
    let mut ids = Vec::new();
    let mut queue = vec![(session.volume.geometry.root_cluster, String::new())];
    while let Some((node, folder)) = queue.pop() {
        let mut offset = 0;
        loop {
            let page = session
                .with_database(|database| database.children(node, offset, 500, true, 0.0))
                .unwrap();
            if page.rows.is_empty() {
                break;
            }
            offset += page.rows.len() as u64;
            for row in page.rows {
                let path = if folder.is_empty() {
                    row.name.clone()
                } else {
                    format!("{folder}/{}", row.name)
                };
                if row.kind == "file" {
                    let id = row.id.unwrap();
                    let range = session.read_entry_range(id, 0, row.length + 1).unwrap();
                    assert!(range.unreadable.is_empty(), "{path}");
                    assert_eq!(range.data.len() as u64, row.length, "{path}");
                    // Every file the driver follows whole and the root leads to is live; an empty one says it is empty.
                    assert!(
                        matches!(row.verdict.as_str(), "live" | "empty"),
                        "{path}: {}",
                        row.verdict
                    );
                    tree.files.insert(path, (row.length, sha(&range.data)));
                    ids.push(id);
                } else {
                    tree.dirs.insert(path.clone());
                    // A folder nothing names as a parent (lost+found, which the formatter makes empty) has no head to browse
                    // into: it is listed, and its verdict says it is empty.
                    match row.head {
                        Some(head) => queue.push((head, path)),
                        None => assert_eq!(row.verdict, "folder-empty", "{path}: {row:?}"),
                    }
                }
            }
        }
    }
    let destination = session.workspace.dir().join("copy");
    session
        .copy_start(CopyRequest {
            destination: destination.to_string_lossy().into_owned(),
            entry_ids: ids.clone(),
            include_partial: true,
            ..CopyRequest::default()
        })
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        let status = session.copy_status().unwrap();
        if status.state == "finished" {
            assert_eq!(
                (
                    status.files_done as usize,
                    status.files_failed,
                    status.files_partial
                ),
                (ids.len(), 0, 0),
                "{status:?}"
            );
            break;
        }
        assert!(
            !matches!(status.state.as_str(), "failed" | "stopped"),
            "{status:?}"
        );
        assert!(std::time::Instant::now() < deadline, "{status:?}");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    let manifest =
        std::fs::read_to_string(destination.join("captain-salvage-manifest.jsonl")).unwrap();
    let mut copies = BTreeMap::new();
    for line in manifest.lines() {
        let row: serde_json::Value = serde_json::from_str(line).unwrap();
        assert_eq!(row["status"], "complete", "{row}");
        let target = std::fs::read(row["target"].as_str().unwrap()).unwrap();
        assert_eq!(sha(&target), row["sha256"].as_str().unwrap());
        copies.insert(
            row["sourcePath"].as_str().unwrap_or_default().to_string()
                + "/"
                + row["name"].as_str().unwrap(),
            sha(&target),
        );
    }
    (tree, copies)
}

#[test]
fn the_app_opens_scans_browses_previews_and_copies_every_image_as_the_kernel_lists_it() {
    for (name, _, _) in cases() {
        let image = std::fs::read(oracle_output().join(format!("{name}.img"))).unwrap();
        let oracle = read_tree(&oracle_output().join(format!("{name}.kernel.tsv")));
        let oracle_links = oracle.links.clone();
        assert!(!oracle_links.is_empty(), "{name}");
        let (shown, copies) = app_view(&name, image);
        // A symbolic link is listed as a file that holds its target: the kernel's links, so.
        let mut kernel = Tree {
            links: BTreeMap::new(),
            ..oracle
        };
        for (path, target) in &oracle_links {
            kernel
                .files
                .insert(path.clone(), (target.len() as u64, sha(target.as_bytes())));
        }
        let wrong = differences(&kernel, &shown);
        assert!(
            wrong.is_empty(),
            "{name}: what the app shows differs from the kernel's:\n{wrong}"
        );
        // Every file was copied out with the bytes the kernel read.
        assert_eq!(copies.len(), kernel.files.len(), "{name}");
        let mut hashes: Vec<&String> = copies.values().collect();
        let mut wanted: Vec<&String> = kernel.files.values().map(|(_, sha)| sha).collect();
        hashes.sort();
        wanted.sort();
        assert_eq!(hashes, wanted, "{name}: the copies' hashes");
    }
}

#[test]
fn the_script_made_every_image() {
    let names: Vec<String> = cases().into_iter().map(|(name, _, _)| name).collect();
    assert_eq!(
        names, IMAGES,
        "the oracle script made other images than the test holds the app to"
    );
}
