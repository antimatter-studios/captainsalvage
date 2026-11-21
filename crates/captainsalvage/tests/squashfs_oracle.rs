//! SquashFS against things that are not us.
//!
//! The app reads SquashFS through the rust-fs-squashfs driver and the generic layers adapter, and
//! `generic_squashfs.rs` reads the driver's own committed fixture. This file reads images that nobody here
//! built: `mksquashfs` makes them, one per compressor the format has (gzip, xz, lzo, lz4, zstd) at 128 KiB
//! blocks, and gzip at 4 KiB and 1 MiB blocks, and the Linux kernel's squashfs driver, mounted read-only on a
//! loop device, reports what is in them as paths, sizes and SHA-256 hashes. They run in the Linux test VM
//! (`validation/squashfs-oracle.sh`, started by `chore test:oracle`). The app must then:
//!
//! - list the same folders, files and symbolic links, with the same size and SHA-256 for every file, and
//!   the same target for every link;
//! - copy out every file from the storage its record claims (its extents, compressed blocks, fragment tail and
//!   holes included), with the oracle's bytes, and call every one of them live and verified;
//! - place every record in the folder the kernel lists it in.
//!
//! The same images are then opened by the engine as a person opens them (`the_app_opens_scans_browses_...`): scanned, browsed
//! from the root, every file read in full by the preview's range reader and copied out, and what is shown and copied is the
//! kernel's tree, byte for byte. The engine does not list symbolic links yet, so that comparison leaves them out.
//!
//! What this does not check, so that nobody reads more into a pass than is there: damaged images (the driver's
//! own suite checks them, and `generic_squashfs.rs` reads one), extended attributes (the images are made without),
//! images with an export table or several fragments tables of more than one size, filesystems on a partition of a
//! disk, and any reader but the kernel's (unsquashfs is installed and is used by the driver's own suite).
//!
//! Behind the `oracle` feature, and it FAILS, never skips, when the oracles cannot run.
#![cfg(feature = "oracle")]

use captainsalvage::filesystem::generic::GenericVolume;
use fs_core_layers::layers::{CandidateKind, CandidateState, FileAPI, Integrity, NodeKind};
use fs_squashfs::layers::SquashfsVolume;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, OnceLock};

/// The images `validation/squashfs-oracle.sh` makes. A script that quietly makes fewer fails.
const IMAGES: [&str; 7] = [
    "gzip-128k",
    "xz-128k",
    "lzo-128k",
    "lz4-128k",
    "zstd-128k",
    "gzip-4k",
    "gzip-1m",
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
        PathBuf::from("/share/squashfs-oracle")
    } else {
        repo().join(".vm-share/squashfs-oracle")
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
                .arg(repo().join("validation/squashfs-oracle.sh"))
                .arg(&share)
                .status()
        } else {
            let vm = std::env::var("FLTH_VM")
                .map(PathBuf::from)
                .unwrap_or_else(|_| repo().join("../fs-linux-test-harness/scripts/vm.sh"));
            let up = Command::new(&vm).current_dir(repo()).arg("up").status();
            assert!(
                matches!(&up, Ok(status) if status.success()),
                "the Linux test VM would not start ({up:?}), so mksquashfs and the kernel's squashfs \
                 driver cannot run. `chore siblings` checks out ../fs-linux-test-harness, and \
                 `chore test:oracle` boots the guest."
            );
            Command::new(&vm)
                .current_dir(repo())
                .args([
                    "exec",
                    "bash /repo/validation/squashfs-oracle.sh /share/squashfs-oracle",
                ])
                .status()
        };
        assert!(
            matches!(&status, Ok(status) if status.success()),
            "the independent oracles did not run ({status:?}). They need squashfs-tools and the kernel's \
             squashfs driver, which `chore test:oracle` provides in the Linux test VM (scripts/vm-setup.sh)."
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

/// An image in memory, read through rust-fs-core's trait, which the SquashFS driver is built on.
struct Memory(Vec<u8>);

impl fs_core_layers::BlockRead for Memory {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core_layers::Result<()> {
        let outside = || fs_core_layers::Error::OutOfBounds {
            offset,
            len: output.len() as u64,
            size: self.0.len() as u64,
        };
        let start = usize::try_from(offset).map_err(|_| outside())?;
        let bytes = self
            .0
            .get(start..start.saturating_add(output.len()))
            .ok_or_else(outside)?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.0.len() as u64
    }
}

type Squashfs = GenericVolume<SquashfsVolume>;

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

/// What the app lists, read in full by the driver's file layer: folders, files with their hashes, links with their
/// targets. Paths have no leading slash, as the kernel's report has none.
fn listed(volume: &Squashfs) -> Tree {
    let files = volume.volume();
    let mut tree = Tree::default();
    let mut folders = vec![String::new()];
    while let Some(folder) = folders.pop() {
        for entry in volume.list(&folder).unwrap() {
            let path = format!("{folder}/{}", entry.name);
            let shown = path.trim_start_matches('/').to_string();
            match entry.kind {
                NodeKind::Directory => {
                    tree.dirs.insert(shown);
                    folders.push(path);
                }
                NodeKind::Symlink => {
                    let target = files.read_link(&entry.node).unwrap();
                    tree.links.insert(shown, String::from_utf8(target).unwrap());
                }
                NodeKind::File => {
                    let bytes = volume.read(&path).unwrap();
                    assert_eq!(bytes.len() as u64, entry.size, "{path}: the listed size");
                    tree.files.insert(shown, (bytes.len() as u64, sha(&bytes)));
                }
                other => panic!("{path}: {other:?}"),
            }
        }
    }
    tree
}

/// Every file the tree names, rebuilt from the storage its record claims, by the folder it is placed in.
fn copied(volume: &Squashfs) -> Tree {
    let candidates = volume.candidates().unwrap();
    let (_, placed) = volume.tree(&candidates).unwrap();
    let folders: BTreeMap<u64, String> = placed
        .iter()
        .map(|candidate| {
            (
                candidate.id,
                candidate.folder.clone().unwrap_or_else(|| "?".into()),
            )
        })
        .collect();
    let mut tree = Tree::default();
    for candidate in &candidates {
        let view = volume.view(candidate);
        assert_eq!(view.state, CandidateState::Live, "{:?}", view.name);
        assert_eq!(view.integrity, Integrity::Verified, "{:?}", view.name);
        let name = view.name.clone().unwrap_or_default();
        let folder = &folders[&view.id];
        assert_ne!(folder, "?", "{name} is an orphan");
        let path = if folder.is_empty() {
            name.clone()
        } else {
            format!("{folder}/{name}")
        };
        match view.kind {
            CandidateKind::Directory if !name.is_empty() => {
                tree.dirs.insert(path);
            }
            CandidateKind::File => {
                let mut bytes = Vec::new();
                let (_, report) = volume.copy(candidate, &mut bytes).unwrap();
                assert_eq!(report.unreadable().count(), 0, "{path}: {report:?}");
                assert_eq!(
                    view.size,
                    Some(bytes.len() as u64),
                    "{path}: the claimed size"
                );
                tree.files.insert(path, (bytes.len() as u64, sha(&bytes)));
            }
            _ => {}
        }
    }
    tree
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

#[test]
fn the_app_lists_and_copies_every_image_the_kernel_reads_byte_for_byte() {
    let cases = cases();
    let names: Vec<&str> = cases.iter().map(|(name, _, _)| name.as_str()).collect();
    assert_eq!(names, IMAGES, "the oracle made another set of images");
    for (name, compressor, block) in cases {
        let image = std::fs::read(oracle_output().join(format!("{name}.img"))).unwrap();
        let oracle = read_tree(&oracle_output().join(format!("{name}.kernel.tsv")));
        assert!(
            oracle.files.len() >= 400 && !oracle.links.is_empty() && oracle.dirs.len() >= 4,
            "{name}: the oracle's tree is too small to check anything: {oracle:?}"
        );
        let volume = Squashfs::open_for_salvage(Arc::new(Memory(image))).unwrap();
        assert_eq!(volume.format(), "squashfs", "{name}");
        assert_eq!(
            volume.geometry().unwrap().allocation_unit,
            block,
            "{name} ({compressor}): the block size"
        );
        assert!(volume.health().unwrap().is_clean(), "{name}");

        let by_path = listed(&volume);
        let wrong = differences(&oracle, &by_path);
        assert!(
            wrong.is_empty(),
            "{name}: the listing differs from the kernel's:\n{wrong}"
        );

        // The records, placed by their parents' claims, with the bytes their extents rebuild. A symbolic link is not
        // a file candidate here, so only folders and files are compared.
        let by_records = copied(&volume);
        let oracle_files_and_dirs = Tree {
            links: BTreeMap::new(),
            ..oracle
        };
        let wrong = differences(&oracle_files_and_dirs, &by_records);
        assert!(
            wrong.is_empty(),
            "{name}: the records' extents differ from the kernel's:\n{wrong}"
        );
    }
}

/// What the app shows of an image it opened as a person does: scanned, browsed from the root, every file read in full by the
/// preview's range reader and copied out, with the folders, and with the hash of the bytes each copy holds.
fn app_view(name: &str, image: Vec<u8>) -> (Tree, BTreeMap<String, String>) {
    use captainsalvage::export::CopyRequest;
    use captainsalvage::scan::{run_scan, ScanOptions};
    use captainsalvage::session::Session;
    use captainsalvage::source::MemorySource;
    use captainsalvage::workspace::Workspace;
    use std::sync::atomic::AtomicBool;
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-squashfs-oracle-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&path);
    let session = Session::with_source(
        Path::new("/test/squashfs-oracle"),
        Arc::new(MemorySource::new(image)),
        Workspace::at(&path).unwrap(),
    )
    .unwrap();
    assert_eq!(
        session.volume.kind,
        captainsalvage::filesystem::FilesystemKind::Squashfs
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
                    queue.push((row.head.unwrap(), path));
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
        let (shown, copies) = app_view(&name, image);
        // The app does not list symbolic links yet (the support table says so): the rest is the kernel's tree.
        let kernel = Tree {
            links: BTreeMap::new(),
            ..oracle
        };
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
