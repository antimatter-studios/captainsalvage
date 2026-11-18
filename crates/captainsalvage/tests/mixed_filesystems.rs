//! One engine, five filesystems, opened in turn again and again: FAT12, FAT16, FAT32, exFAT and
//! NTFS. Every volume holds a file with the same name and different bytes, so a cache, an
//! identity or a workspace that leaked from one volume to another would show as the wrong bytes.
//! Synthetic images only; the source of each is read back afterwards and must be what it was.

use captainsalvage::engine::{Engine, AUTHOR_USER};
use captainsalvage::export::CopyRequest;
use captainsalvage::filesystem::FilesystemKind;
use captainsalvage::session::Session;
use captainsalvage::source::{BlockSource, MemorySource};
use fs_ntfs::file::FileAPI;
use fs_ntfs::volume::{Volume, VolumeAPI};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

mod fat_support;
mod ntfs_support;

/// The name every volume holds, with different bytes in each.
const SHARED_NAME: &str = "Captain log.txt";

struct Disk {
    kind: FilesystemKind,
    /// A name only this volume holds, or nothing when it is one of three FAT volumes that
    /// share their layout.
    own_name: Option<&'static str>,
    /// What `SHARED_NAME` holds on this volume.
    shared_bytes: Vec<u8>,
    source: Arc<MemorySource>,
    image_hash: String,
    length: usize,
    session: Arc<Session>,
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn exfat_image() -> (Vec<u8>, Vec<u8>) {
    use fs_exfat::fixtures::{
        bitmap_entry, entry_set, upcase_entry, Builder, BITMAP_CLUSTER, ROOT_CLUSTER,
    };
    let text = b"the exFAT volume's bytes".to_vec();
    let mut builder = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 5, 6] {
        builder.allocate(cluster);
    }
    builder.fat(ROOT_CLUSTER, fs_exfat::primitives::FAT_END);
    let boot = fs_exfat::primitives::BootSector::parse(&builder.image).unwrap();
    let bitmap_length = u64::from(boot.cluster_count).div_ceil(8);
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(BITMAP_CLUSTER, bitmap_length),
            upcase_entry(4, &[0; 100]),
            entry_set(SHARED_NAME, false, 5, text.len() as u64, true, true),
            entry_set("exfat-only.txt", false, 6, 9, true, true),
        ],
    );
    builder.put(5, 0, &text);
    builder.put(6, 0, b"exfat own");
    (builder.image, text)
}

fn ntfs_image() -> (Vec<u8>, Vec<u8>) {
    let text = b"the NTFS volume's bytes".to_vec();
    let image = ntfs_support::format();
    let volume = Volume::open_writable(image.clone()).unwrap();
    volume.create_file("/", SHARED_NAME).unwrap();
    volume
        .write_file_at(&format!("/{SHARED_NAME}"), 0, &text)
        .unwrap();
    volume.create_file("/", "ntfs-only.txt").unwrap();
    volume
        .write_file_at("/ntfs-only.txt", 0, b"ntfs own")
        .unwrap();
    volume.flush().unwrap();
    drop(volume);
    let bytes = image.0.lock().unwrap().clone();
    (bytes, text)
}

fn disks() -> Vec<Disk> {
    let mut disks = Vec::new();
    let mut add = |kind: FilesystemKind,
                   own_name: Option<&'static str>,
                   image: Vec<u8>,
                   shared_bytes: Vec<u8>| {
        let source = Arc::new(MemorySource::new(image.clone()));
        let session = Session::with_source(
            Path::new(&format!("/test/mixed-{kind}")),
            source.clone(),
            ntfs_support::workspace(&format!("mixed-{kind}")),
        )
        .unwrap();
        assert_eq!(session.volume.kind, kind);
        ntfs_support::scan(&session);
        disks.push(Disk {
            kind,
            own_name,
            shared_bytes,
            source,
            image_hash: sha256(&image),
            length: image.len(),
            session,
        });
    };
    for kind in [
        FilesystemKind::Fat12,
        FilesystemKind::Fat16,
        FilesystemKind::Fat32,
    ] {
        let text = format!("the {kind} volume's bytes\n").into_bytes();
        let fixture = fat_support::Fixture::with_text(kind, &text);
        let bytes: Vec<u8> = fixture.contents.clone();
        add(kind, None, fixture.image, bytes);
    }
    let (image, text) = exfat_image();
    add(FilesystemKind::Exfat, Some("exfat-only.txt"), image, text);
    let (image, text) = ntfs_image();
    add(FilesystemKind::Ntfs, Some("ntfs-only.txt"), image, text);
    disks
}

fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

fn rows(engine: &Engine, name: &str) -> Vec<Value> {
    call(engine, "search", json!({"query": name}))["rows"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|row| row["name"] == name)
        .cloned()
        .collect()
}

fn whole_source(disk: &Disk) -> Vec<u8> {
    let mut bytes = vec![0; disk.length];
    disk.source.read_exact_at(0, &mut bytes).unwrap();
    bytes
}

fn find_file(folder: &Path, name: &str) -> Option<PathBuf> {
    for entry in std::fs::read_dir(folder).ok()? {
        let path = entry.ok()?.path();
        if path.is_dir() {
            if let Some(found) = find_file(&path, name) {
                return Some(found);
            }
        } else if path.file_name().is_some_and(|file| file == name) {
            return Some(path);
        }
    }
    None
}

/// One visit: what this engine answers for this volume is this volume's, and nothing else's.
fn visit(engine: &Engine, disk: &Disk, others: &[Disk]) {
    engine.set_session(disk.session.clone());
    assert_eq!(disk.session.volume.kind, disk.kind);
    let overview = disk.session.overview();
    match disk.kind {
        FilesystemKind::Exfat => assert!(overview.get("filesystem").is_none()),
        FilesystemKind::Ntfs => assert_eq!(overview["filesystem"]["kind"], "ntfs"),
        kind => assert_eq!(overview["filesystem"]["variant"], kind.to_string()),
    }
    let reported = call(engine, "capabilities", json!({}));
    assert_eq!(reported["filesystem"], disk.kind.to_string());
    assert_eq!(
        reported["capabilities"],
        serde_json::to_value(disk.kind.capabilities()).unwrap()
    );
    let found = rows(engine, SHARED_NAME);
    assert_eq!(found.len(), 1, "{}: {found:?}", disk.kind);
    let id = found[0]["id"].as_i64().unwrap();
    let read = disk
        .session
        .read_entry_range(id, 0, disk.shared_bytes.len() as u64 + 64)
        .unwrap();
    assert_eq!(read.data, disk.shared_bytes, "{}", disk.kind);
    for other in others.iter().filter(|other| other.kind != disk.kind) {
        if let Some(name) = other.own_name {
            assert!(
                rows(engine, name).is_empty(),
                "{} shows {name}, which only {} holds",
                disk.kind,
                other.kind
            );
        }
    }
    if let Some(name) = disk.own_name {
        assert_eq!(rows(engine, name).len(), 1, "{}", disk.kind);
    }
    assert!(disk.session.verify_unchanged().unwrap().unchanged);
}

#[test]
fn one_engine_switching_between_five_filesystems_never_mixes_names_bytes_or_workspaces() {
    let disks = disks();
    let engine = Engine::new();
    let workspaces: std::collections::HashSet<_> = disks
        .iter()
        .map(|disk| disk.session.workspace.dir().to_path_buf())
        .collect();
    assert_eq!(workspaces.len(), disks.len(), "each volume has a workspace");
    let order = |round: usize| -> Vec<usize> {
        let mut order: Vec<usize> = (0..disks.len()).collect();
        match round % 3 {
            0 => {}
            1 => order.reverse(),
            _ => order.rotate_left(2),
        }
        order
    };
    for round in 0..6 {
        for index in order(round) {
            visit(&engine, &disks[index], &disks);
        }
    }
    for disk in &disks {
        assert_eq!(
            sha256(&whole_source(disk)),
            disk.image_hash,
            "{} source changed",
            disk.kind
        );
    }
}

#[test]
fn copies_made_between_switches_hold_each_volumes_own_bytes() {
    let disks = disks();
    let engine = Engine::new();
    // Start every copy, then switch the engine around while it runs and between waits.
    for disk in &disks {
        engine.set_session(disk.session.clone());
        let id = rows(&engine, SHARED_NAME)[0]["id"].as_i64().unwrap();
        let destination = disk.session.workspace.dir().join("recovered");
        disk.session
            .copy_start(CopyRequest {
                destination: destination.to_string_lossy().into_owned(),
                entry_ids: vec![id],
                ..CopyRequest::default()
            })
            .unwrap();
        ntfs_support::wait_for_copy(&disk.session, 1);
    }
    for disk in disks.iter().rev() {
        engine.set_session(disk.session.clone());
        let destination = disk.session.workspace.dir().join("recovered");
        let copied = find_file(&destination, SHARED_NAME)
            .unwrap_or_else(|| panic!("{} copied nothing", disk.kind));
        assert_eq!(
            std::fs::read(copied).unwrap(),
            disk.shared_bytes,
            "{}",
            disk.kind
        );
    }
    for disk in &disks {
        assert_eq!(
            sha256(&whole_source(disk)),
            disk.image_hash,
            "{} source changed by the copy",
            disk.kind
        );
    }
}

/// A home folder of this test run, set once before anything opens the catalog.
fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("captainsalvage-mixed-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("HOME", &path);
        path
    })
}

#[test]
fn two_images_with_one_serial_number_stay_two_scans_when_opened_by_path() {
    // Two FAT16 volumes formatted with the same serial number but holding different bytes: a
    // 32-bit serial is not an identity, so each must find its own workspace every time.
    static IMAGES: Mutex<()> = Mutex::new(());
    let _guard = IMAGES.lock().unwrap();
    home();
    // Past the volume's own structures each disk holds data of its own. A disk whose sampled
    // blocks were all zeros could not be told from any other by its markers, whatever its serial.
    let make = |label: &str, text: &[u8], seed: u64| {
        let mut fixture = fat_support::Fixture::with_text(FilesystemKind::Fat16, text);
        let mut state = seed | 1;
        for byte in &mut fixture.image[100 * 512..] {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state >> 24) as u8;
        }
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-mixed-{}-{label}.img",
            std::process::id()
        ));
        std::fs::write(&path, &fixture.image).unwrap();
        (path, fixture.image, fixture.contents)
    };
    let first = make("first", b"the first disk\n", 0x1234_5678_9abc_def1);
    let second = make("second", b"the second disk\n", 0x0fed_cba9_8765_4321);
    assert_eq!(
        first.1[39..43],
        second.1[39..43],
        "the serial numbers are the same"
    );
    assert_ne!(first.1, second.1);
    let mut workspaces = Vec::new();
    for round in 0..3 {
        for (path, _, expected) in [&first, &second] {
            let session = Session::open(path).unwrap();
            if round == 0 {
                ntfs_support::scan(&session);
            }
            let engine = Engine::new();
            engine.set_session(session.clone());
            let id = rows(&engine, SHARED_NAME)[0]["id"].as_i64().unwrap();
            assert_eq!(
                session.read_entry_range(id, 0, 4096).unwrap().data,
                *expected,
                "round {round}: {}",
                path.display()
            );
            workspaces.push((path.clone(), session.workspace.dir().to_path_buf()));
        }
    }
    for (path, workspace) in &workspaces {
        let same: Vec<_> = workspaces
            .iter()
            .filter(|(other, _)| other == path)
            .map(|(_, folder)| folder)
            .collect();
        assert!(
            same.iter().all(|folder| *folder == workspace),
            "{} moved between workspaces",
            path.display()
        );
    }
    assert_ne!(
        workspaces[0].1, workspaces[1].1,
        "two disks share a workspace"
    );
    for (path, image, _) in [&first, &second] {
        assert_eq!(&std::fs::read(path).unwrap(), image, "{}", path.display());
        let _ = std::fs::remove_file(path);
    }
}
