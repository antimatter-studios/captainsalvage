//! End-to-end tests against a hand-built exFAT volume with known damage.

use salvage_core::content::{CLASS_DIRECTORY, CLASS_JPEG, CLASS_MP3, CLASS_ZEROS};
use salvage_core::engine::{Engine, AUTHOR_CLAUDE, AUTHOR_USER};
use salvage_core::exfat::{boot_checksum, FAT_END};
use salvage_core::map::MapDb;
use salvage_core::mcp::{handle_message, tool_definitions};
use salvage_core::scan::{read_progress, run_scan, ScanOptions};
use salvage_core::session::{whole_disk_path, Layer, Lens, Session};
use salvage_core::source::{BlockSource, MemorySource};
use salvage_core::verify::{verify_mp3, verify_png};
use salvage_core::volume::{Volume, ALLOC_ALLOCATED, ALLOC_FREE_FAT, ALLOC_FREE_ZERO};
use salvage_core::workspace::Workspace;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

#[path = "parity/synthetic.rs"]
mod parity;

const SECTOR: usize = 512;
const CLUSTER: usize = 4096;
const CLUSTER_COUNT: u32 = 200;
const FAT_OFFSET: u32 = 24;
const FAT_LENGTH: u32 = 8;
const HEAP_OFFSET: u32 = 32;
const BITMAP_CLUSTER: u32 = 2;
const ROOT_CLUSTER: u32 = 3;

struct Builder {
    image: Vec<u8>,
}

fn noise(seed: u32, length: usize) -> Vec<u8> {
    let mut state = seed.wrapping_mul(2654435761).wrapping_add(1);
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as u8
        })
        .collect()
}

fn entry_set(
    name: &str,
    is_dir: bool,
    first: u32,
    length: u64,
    no_fat_chain: bool,
    in_use: bool,
) -> Vec<u8> {
    let units: Vec<u16> = name.encode_utf16().collect();
    let name_entries = units.len().div_ceil(15);
    let secondary = 1 + name_entries;
    let mut set = vec![0u8; 32 * (secondary + 1)];
    set[0] = if in_use { 0x85 } else { 0x05 };
    set[1] = secondary as u8;
    set[4] = if is_dir { 0x10 } else { 0x20 };
    let timestamp: u32 = (42 << 25) | (9 << 21) | (5 << 16) | (17 << 11) | (53 << 5) | 27;
    set[8..12].copy_from_slice(&timestamp.to_le_bytes());
    set[12..16].copy_from_slice(&timestamp.to_le_bytes());
    set[32] = if in_use { 0xC0 } else { 0x40 };
    set[33] = 0x01 | if no_fat_chain { 0x02 } else { 0 };
    set[35] = units.len() as u8;
    set[40..48].copy_from_slice(&length.to_le_bytes());
    set[52..56].copy_from_slice(&first.to_le_bytes());
    set[56..64].copy_from_slice(&length.to_le_bytes());
    for index in 0..name_entries {
        let base = 32 * (2 + index);
        set[base] = if in_use { 0xC1 } else { 0x41 };
        for (position, unit) in units.iter().skip(index * 15).take(15).enumerate() {
            set[base + 2 + position * 2..base + 4 + position * 2]
                .copy_from_slice(&unit.to_le_bytes());
        }
    }
    seal_entry_set(&mut set);
    set
}

/// Changes when an entry says it was last modified.
fn modified_on(mut set: Vec<u8>, year: u32, month: u32, day: u32) -> Vec<u8> {
    let timestamp = ((year - 1980) << 25) | (month << 21) | (day << 16);
    set[12..16].copy_from_slice(&timestamp.to_le_bytes());
    seal_entry_set(&mut set);
    set
}

/// Changes when an entry says it was created.
fn created_on(mut set: Vec<u8>, year: u32, month: u32, day: u32) -> Vec<u8> {
    let timestamp = ((year - 1980) << 25) | (month << 21) | (day << 16);
    set[8..12].copy_from_slice(&timestamp.to_le_bytes());
    seal_entry_set(&mut set);
    set
}

fn seal_entry_set(set: &mut [u8]) {
    let mut checksum = 0u16;
    for (index, &byte) in set.iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        let value = if index % 32 == 0 { byte | 0x80 } else { byte };
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(value));
    }
    set[2..4].copy_from_slice(&checksum.to_le_bytes());
}

impl Builder {
    fn new() -> Self {
        let sectors = HEAP_OFFSET as usize + CLUSTER_COUNT as usize * (CLUSTER / SECTOR);
        let mut builder = Self {
            image: vec![0u8; sectors * SECTOR],
        };
        builder.write_boot();
        builder.fat(0, 0xFFFF_FFF8);
        builder.fat(1, FAT_END);
        builder
    }

    fn write_boot(&mut self) {
        let mut sector = vec![0u8; SECTOR];
        sector[0..3].copy_from_slice(&[0xEB, 0x76, 0x90]);
        sector[3..11].copy_from_slice(b"EXFAT   ");
        sector[72..80].copy_from_slice(&((self.image.len() / SECTOR) as u64).to_le_bytes());
        sector[80..84].copy_from_slice(&FAT_OFFSET.to_le_bytes());
        sector[84..88].copy_from_slice(&FAT_LENGTH.to_le_bytes());
        sector[88..92].copy_from_slice(&HEAP_OFFSET.to_le_bytes());
        sector[92..96].copy_from_slice(&CLUSTER_COUNT.to_le_bytes());
        sector[96..100].copy_from_slice(&ROOT_CLUSTER.to_le_bytes());
        sector[100..104].copy_from_slice(&0xABCD_1234u32.to_le_bytes());
        sector[104..106].copy_from_slice(&0x0100u16.to_le_bytes());
        sector[108] = 9;
        sector[109] = 3;
        sector[110] = 1;
        sector[112] = 5;
        sector[510] = 0x55;
        sector[511] = 0xAA;
        self.image[..SECTOR].copy_from_slice(&sector);
        let checksum = boot_checksum(&self.image[..SECTOR * 12], SECTOR);
        for position in (SECTOR * 11..SECTOR * 12).step_by(4) {
            self.image[position..position + 4].copy_from_slice(&checksum.to_le_bytes());
        }
        let region = self.image[..SECTOR * 12].to_vec();
        self.image[SECTOR * 12..SECTOR * 24].copy_from_slice(&region);
        // The same difference as the real volume: backup PercentInUse is stale.
        self.image[SECTOR * 12 + 112] = 59;
    }

    fn fat(&mut self, cluster: u32, value: u32) {
        let position = FAT_OFFSET as usize * SECTOR + cluster as usize * 4;
        self.image[position..position + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn cluster_position(cluster: u32) -> usize {
        HEAP_OFFSET as usize * SECTOR + (cluster as usize - 2) * CLUSTER
    }

    fn put(&mut self, cluster: u32, offset: usize, bytes: &[u8]) {
        let position = Self::cluster_position(cluster) + offset;
        self.image[position..position + bytes.len()].copy_from_slice(bytes);
    }

    fn allocate(&mut self, cluster: u32) {
        let index = (cluster - 2) as usize;
        self.image[Self::cluster_position(BITMAP_CLUSTER) + index / 8] |= 1 << (index % 8);
    }

    fn directory(&mut self, cluster: u32, sets: &[Vec<u8>]) {
        let mut offset = 0;
        for set in sets {
            self.put(cluster, offset, set);
            offset += set.len();
        }
    }
}

fn special(kind: u8, first: u32, length: u64) -> Vec<u8> {
    let mut entry = vec![0u8; 32];
    entry[0] = kind;
    entry[20..24].copy_from_slice(&first.to_le_bytes());
    entry[24..32].copy_from_slice(&length.to_le_bytes());
    entry
}

/// Builds a volume with a live tree, an orphan tree and every damage class.
fn build_volume() -> Vec<u8> {
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 5, 10, 11, 61] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
            entry_set("visible", true, 5, CLUSTER as u64, true, true),
            entry_set("gone", true, 6, CLUSTER as u64, true, false),
        ],
    );
    b.directory(
        5,
        &[entry_set(
            "keep.mp3",
            false,
            10,
            2 * CLUSTER as u64,
            true,
            true,
        )],
    );
    b.put(10, 0, b"ID3\x04");
    b.put(10, 16, &noise(10, 2000));
    b.put(11, 0, &noise(11, CLUSTER));

    // Cluster 6 was a folder, now holding other data.
    b.put(6, 0, &noise(6, CLUSTER));

    // Orphan folder: nothing points at cluster 20.
    b.directory(
        20,
        &[
            entry_set(
                "track.mp3",
                false,
                30,
                3 * CLUSTER as u64 - 100,
                false,
                true,
            ),
            entry_set(
                ".track.mp3.abc123",
                false,
                30,
                3 * CLUSTER as u64 - 100,
                false,
                false,
            ),
            entry_set("photo.jpg", false, 40, 2 * CLUSTER as u64, true, true),
            entry_set("album", true, 21, CLUSTER as u64, true, true),
            entry_set("wiped.mp3", false, 50, CLUSTER as u64, true, true),
            entry_set("reused.jpg", false, 60, 2 * CLUSTER as u64, true, true),
            entry_set("short.bin", false, 70, 2 * CLUSTER as u64, false, true),
            entry_set("notes.xyz", false, 80, 100, true, true),
        ],
    );
    b.directory(
        21,
        &[entry_set("song.mp3", false, 90, CLUSTER as u64, true, true)],
    );

    b.fat(30, 31);
    b.fat(31, 33);
    b.fat(33, FAT_END);
    b.put(30, 0, b"ID3\x03");
    b.put(30, 16, &noise(30, CLUSTER - 16));
    b.put(31, 0, &noise(31, CLUSTER));
    b.put(33, 0, &noise(33, CLUSTER));
    b.put(40, 0, &[0xFF, 0xD8, 0xFF, 0xE0]);
    b.put(40, 16, &noise(40, CLUSTER - 16));
    b.put(60, 0, &[0xFF, 0xD8, 0xFF, 0xE0]);
    b.put(61, 0, &noise(61, CLUSTER));
    b.fat(70, FAT_END);
    b.put(70, 0, &noise(70, CLUSTER));
    b.put(80, 0, &noise(80, CLUSTER));
    b.put(90, 0, b"ID3\x04");
    b.put(90, 16, &noise(90, CLUSTER - 16));

    // A damaged entry: one name character altered after the checksum was set.
    let mut damaged = entry_set("broken.jpg", false, 95, CLUSTER as u64, true, false);
    damaged[64 + 4] = b'X';
    b.directory(22, &[damaged]);

    // A folder reachable only through a damaged entry: cluster 23 holds nothing
    // but a broken entry, and that entry points at a real directory at cluster 24.
    let mut lost_link = entry_set("lost-album", true, 24, CLUSTER as u64, true, true);
    lost_link[64 + 2] = b'X';
    b.directory(23, &[lost_link]);
    b.directory(
        24,
        &[entry_set("deep.mp3", false, 96, CLUSTER as u64, true, true)],
    );
    b.put(96, 0, b"ID3\x04");
    b.put(96, 16, &noise(96, CLUSTER - 16));

    // Two small text files sharing clusters with older, random data. The first
    // is intact past its own bytes, and the second was overwritten at the start.
    b.directory(
        25,
        &[
            entry_set("small.php", false, 110, 100, true, true),
            entry_set("clobbered.php", false, 112, 1000, true, true),
        ],
    );
    b.put(110, 200, &noise(110, CLUSTER - 200));
    b.put(110, 0, &[b'<'; 100]);
    b.put(112, 0, &noise(112, CLUSTER));

    // A chain no entry claims, starting with a JPEG header.
    b.fat(100, 101);
    b.fat(101, FAT_END);
    b.put(100, 0, &[0xFF, 0xD8, 0xFF, 0xE0]);
    b.put(100, 16, &noise(100, CLUSTER - 16));
    b.put(101, 0, &noise(101, CLUSTER));

    // short.bin stops after one cluster. This nameless chain is its missing half.
    b.fat(75, FAT_END);
    b.put(75, 0, &noise(75, CLUSTER));
    b.image
}

fn open_volume() -> Volume {
    Volume::open(Arc::new(MemorySource::new(build_volume()))).expect("synthetic volume opens")
}

fn temp_workspace(label: &str) -> (Workspace, PathBuf) {
    let dir = std::env::temp_dir().join(format!("salvage-test-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    (Workspace::at(&dir).expect("workspace"), dir)
}

fn scan(volume: &Volume, workspace: &Workspace) {
    let stop = AtomicBool::new(false);
    run_scan(
        volume,
        workspace,
        ScanOptions { chunk_clusters: 16 },
        &stop,
        |_| {},
    )
    .expect("scan");
}

fn verdict_of(database: &MapDb, node: u32, name: &str) -> String {
    let page = database
        .children(node, 0, 100, true, 0.0)
        .expect("children");
    page.rows
        .iter()
        .find(|row| row.name == name)
        .unwrap_or_else(|| panic!("{name} not found in node {node}"))
        .verdict
        .clone()
}

#[test]
fn boot_regions_parse_and_checksums_ignore_percent_in_use() {
    let volume = open_volume();
    assert_eq!(volume.boot.cluster_count, CLUSTER_COUNT);
    assert_eq!(volume.boot.cluster_size(), CLUSTER as u64);
    assert_eq!(
        volume.boot_check.main_stored,
        volume.boot_check.main_computed
    );
    assert_eq!(
        volume.boot_check.backup_stored,
        volume.boot_check.backup_computed
    );
    assert!(!volume.boot_check.regions_identical);
    assert_eq!(volume.boot.percent_in_use, 5);
    assert_eq!(volume.backup_boot.percent_in_use, 59);

    for (main, label) in [(true, "main boot sector"), (false, "backup boot sector")] {
        for truncated in [false, true] {
            let mut structures = volume.structures.clone();
            let region = if main {
                &mut structures.main
            } else {
                &mut structures.backup
            };
            let reason = if truncated {
                region.truncate(SECTOR - 1);
                "boot sector is shorter than 512 bytes"
            } else {
                region[3] = 0;
                "no exFAT signature at offset 3: this is not an exFAT volume"
            };
            let error = Volume::from_structures(volume.source().clone(), structures)
                .err()
                .expect("damaged boot sector is rejected");
            assert_eq!(format!("{error:#}"), format!("{label}: {reason}"));
        }
    }
}

#[test]
fn allocation_classes_separate_bitmap_and_fat_state() {
    let volume = open_volume();
    assert_eq!(volume.allocation_class(10), ALLOC_ALLOCATED);
    assert_eq!(volume.allocation_class(30), ALLOC_FREE_FAT);
    assert_eq!(volume.allocation_class(40), ALLOC_FREE_ZERO);
    assert_eq!(volume.allocation_class(100), ALLOC_FREE_FAT);
    assert_eq!(volume.stats.allocated, 7);
    assert_eq!(volume.stats.free_with_fat, 7);
    assert_eq!(volume.stats.free_chains.chains, 4);
    assert_eq!(volume.stats.free_chains.clusters, 7);
}

#[test]
fn scan_classifies_content_and_rebuilds_orphan_tree() {
    let volume = open_volume();
    let (workspace, dir) = temp_workspace("tree");
    scan(&volume, &workspace);
    let database = MapDb::open(&workspace.database()).expect("database");

    let classes = workspace.read_cluster_map(&workspace.classes(), CLUSTER_COUNT);
    assert_eq!(classes[(20 - 2) as usize], CLASS_DIRECTORY);
    assert_eq!(classes[(30 - 2) as usize], CLASS_MP3);
    assert_eq!(classes[(50 - 2) as usize], CLASS_ZEROS);

    let roots = database.roots(0, 10).expect("roots");
    assert_eq!(roots.rows[0].node_kind.as_deref(), Some("root"));
    let orphan = roots
        .rows
        .iter()
        .find(|row| row.head == Some(20))
        .expect("orphan at cluster 20");
    assert_eq!(orphan.node_kind.as_deref(), Some("orphan"));
    assert!(orphan.bytes > 0);

    assert_eq!(verdict_of(&database, 5, "keep.mp3"), "live");
    assert_eq!(verdict_of(&database, 20, "track.mp3"), "likely-intact");
    assert_eq!(verdict_of(&database, 20, "photo.jpg"), "likely-intact");
    assert_eq!(
        verdict_of(&database, 20, ".track.mp3.abc123"),
        "rename-residue"
    );
    assert_eq!(verdict_of(&database, 20, "wiped.mp3"), "overwritten");
    assert_eq!(verdict_of(&database, 20, "reused.jpg"), "partial");
    assert_eq!(verdict_of(&database, 20, "short.bin"), "partial");
    assert_eq!(verdict_of(&database, 20, "notes.xyz"), "plausible");
    assert_eq!(verdict_of(&database, 21, "song.mp3"), "likely-intact");
    assert_eq!(verdict_of(&database, 3, "gone"), "folder-missing");

    // The subfolder hangs off its orphan parent, and its total reaches the orphan.
    assert_eq!(
        database.path_of(Some(21)).expect("path"),
        "[orphan @20]/album"
    );
    let album = database.node(21).expect("node").expect("album exists");
    assert_eq!(album.node_kind.as_deref(), Some("attached"));
    assert_eq!(album.intact_files, 1);
    assert!(orphan.intact_files >= 3);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_file_made_after_a_deletion_holds_the_clusters_it_reused() {
    let mut b = Builder {
        image: build_volume(),
    };
    // film.mp3 was written over clusters 130-133 after old.txt, already deleted,
    // had used cluster 131. mixed.mp3 shares cluster 141 with stale.txt, which was
    // made after mixed.mp3, so nothing settles which of them wrote it last.
    b.directory(
        26,
        &[
            created_on(
                entry_set("film.mp3", false, 130, 4 * CLUSTER as u64, true, true),
                2023,
                5,
                1,
            ),
            created_on(
                entry_set("old.txt", false, 131, 100, true, false),
                2020,
                1,
                1,
            ),
            created_on(
                entry_set("mixed.mp3", false, 140, 3 * CLUSTER as u64, true, true),
                2021,
                1,
                1,
            ),
            created_on(
                entry_set("stale.txt", false, 141, 100, true, false),
                2024,
                1,
                1,
            ),
        ],
    );
    for first in [130, 140] {
        b.put(first, 0, b"ID3\x04");
        b.put(first, 16, &noise(first, CLUSTER - 16));
    }
    for cluster in (131..134).chain(141..143) {
        b.put(cluster, 0, &noise(cluster, CLUSTER));
    }
    let volume = Volume::open(Arc::new(MemorySource::new(b.image))).expect("volume opens");
    let (workspace, dir) = temp_workspace("reuse");
    scan(&volume, &workspace);
    let mut database = MapDb::open(&workspace.database()).expect("database");

    assert_eq!(verdict_of(&database, 26, "film.mp3"), "likely-intact");
    assert_eq!(verdict_of(&database, 26, "old.txt"), "overwritten");
    assert_eq!(verdict_of(&database, 26, "mixed.mp3"), "partial");
    assert_eq!(verdict_of(&database, 26, "stale.txt"), "partial");

    // Ignoring the order makes every shared cluster a conflict again.
    let mut params = database.params().expect("params");
    params.ignore_reuse_order = true;
    database.set_params(&params).expect("set params");
    database.resolve(&volume, &workspace).expect("resolve");
    assert_eq!(verdict_of(&database, 26, "film.mp3"), "partial");
    assert_eq!(verdict_of(&database, 26, "old.txt"), "partial");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_rebuild_can_be_previewed_without_saving_it() {
    let (engine, _session, dir) = engine_with_volume("preview");
    let short = id_named(&engine, "short.bin");
    let verdict = |id: i64| call(&engine, "entry", json!({"id": id}))["entry"]["verdict"].clone();
    assert_eq!(verdict(short), "partial");

    // short.bin's missing half is the nameless chain at cluster 75.
    let preview = call(
        &engine,
        "resolve_preview",
        json!({"overlays": [{"kind": "file_extents", "target": short, "payload": {"extents": [{"first": 70, "count": 1}, {"first": 75, "count": 1}]}, "note": "try the bridge"}]}),
    );
    let changes = preview["changes"].as_array().expect("changes");
    let moved = changes
        .iter()
        .find(|change| {
            change["sampleIds"]
                .as_array()
                .expect("ids")
                .contains(&json!(short))
        })
        .expect("short.bin moves");
    assert_eq!(moved["from"], "partial");
    assert_ne!(moved["to"], "partial");
    assert!(preview["filesUnchanged"].as_u64().expect("unchanged") > 0);

    // Nothing was saved: the stored verdict and the overlays are as they were.
    assert_eq!(verdict(short), "partial");
    assert!(call(&engine, "list_overlays", json!({}))
        .as_array()
        .expect("overlays")
        .is_empty());

    // A parameter change previews too, and an unchanged map previews as no change.
    let none = call(&engine, "resolve_preview", json!({}));
    assert_eq!(none["filesChanged"], 0);
    let guessing = call(
        &engine,
        "resolve_preview",
        json!({"parameters": {"chainFallbackContiguous": true}}),
    );
    assert!(guessing["filesChanged"].as_u64().expect("changed") >= 1);
    assert_eq!(
        call(&engine, "get_parameters", json!({}))["chainFallbackContiguous"],
        false
    );

    let _ = std::fs::remove_dir_all(dir);
}

fn reuse_image() -> Vec<u8> {
    let mut b = Builder {
        image: build_volume(),
    };
    b.directory(
        26,
        &[
            created_on(
                entry_set("film.mp3", false, 130, 4 * CLUSTER as u64, true, true),
                2023,
                5,
                1,
            ),
            created_on(
                entry_set("old.txt", false, 131, 100, true, false),
                2020,
                1,
                1,
            ),
            created_on(
                entry_set("mixed.mp3", false, 140, 3 * CLUSTER as u64, true, true),
                2021,
                1,
                1,
            ),
            created_on(
                entry_set("stale.txt", false, 141, 100, true, false),
                2024,
                1,
                1,
            ),
        ],
    );
    for first in [130, 140] {
        b.put(first, 0, b"ID3\x04");
        b.put(first, 16, &noise(first, CLUSTER - 16));
    }
    for cluster in (131..134).chain(141..143) {
        b.put(cluster, 0, &noise(cluster, CLUSTER));
    }
    b.image
}

#[test]
fn conflicts_name_the_other_claimants_and_who_holds_the_clusters() {
    let (engine, _session, dir) = engine_with_image("conflicts", reuse_image());
    let film = id_named(&engine, "film.mp3");
    let answer = call(&engine, "conflicts", json!({"id": film}));
    assert_eq!(answer["sharedClusters"], 1);
    let claimants = answer["claimants"].as_array().expect("claimants");
    assert_eq!(claimants.len(), 1, "{claimants:?}");
    assert_eq!(claimants[0]["name"], "old.txt");
    assert_eq!(claimants[0]["holds"], "this-file");
    assert_eq!(claimants[0]["shared"][0]["first"], 131);
    assert!(answer["sharedContent"][0]["content"].is_string());

    let mixed = id_named(&engine, "mixed.mp3");
    let answer = call(&engine, "conflicts", json!({"id": mixed}));
    assert_eq!(answer["claimants"][0]["name"], "stale.txt");
    assert_eq!(answer["claimants"][0]["holds"], "unsettled");

    let photo = id_named(&engine, "photo.jpg");
    assert_eq!(
        call(&engine, "conflicts", json!({"id": photo}))["sharedClusters"],
        0
    );
    let _ = std::fs::remove_dir_all(dir);
}

/// The ownership map and owners() are worked out separately; they must agree. A cluster the
/// map says several entries claim must show at least two streams through owners().
#[test]
fn the_ownership_map_and_owners_agree() {
    let (engine, session, dir) = engine_with_image("agree", reuse_image());
    call(&engine, "resolve", json!({}));
    let ownership = session.ownership_map();
    let database = MapDb::open(&session.workspace.database()).expect("database");
    let mut checked = 0;
    for (index, &kind) in ownership.iter().enumerate() {
        let cluster = index as u32 + 2;
        if kind == 2 || kind == 1 {
            let rows = database.owners(&session.volume, cluster).expect("owners");
            let mut starts: Vec<u32> = rows.iter().map(|row| row.first_cluster).collect();
            starts.sort_unstable();
            starts.dedup();
            let needed = if kind == 2 { 2 } else { 1 };
            assert!(
                starts.len() >= needed,
                "cluster {cluster} is marked {kind} but owners() finds {starts:?}"
            );
            checked += 1;
        }
    }
    assert!(checked > 10);
    let _ = std::fs::remove_dir_all(dir);
}

/// Every field a script function hands back is named in run_script's description.
#[test]
fn the_script_reference_names_every_field_it_returns() {
    let (engine, _session, dir) = engine_with_volume("script-docs");
    let keys = |source: &str| -> Vec<String> {
        let answer = call(&engine, "run_script", json!({"source": source}));
        answer["result"]
            .as_object()
            .unwrap_or_else(|| panic!("{answer}"))
            .keys()
            .cloned()
            .collect()
    };
    let reference = salvage_core::script::API_REFERENCE;
    let set_keys = keys(&format!("parse_sets(read_cluster({ROOT_CLUSTER}))[0]"));
    let report_keys = keys("verify(read_cluster(30), \"track.mp3\")");
    for key in set_keys.iter().chain(&report_keys) {
        assert!(
            reference.contains(key.as_str()),
            "{key} is not in the script reference"
        );
    }
    assert!(set_keys.len() >= 10 && report_keys.len() >= 6);
    let _ = std::fs::remove_dir_all(dir);
}

/// MPEG-1 Layer III frames at 128 kbit/s and 44.1 kHz, each 417 bytes, with varied data.
fn mp3_frames(bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes);
    let mut n = 0u32;
    while out.len() + 417 <= bytes {
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        out.extend(noise(9000 + n, 413));
        n += 1;
    }
    out.resize(bytes, 0x55);
    out
}

#[test]
fn a_failed_mp3_that_still_plays_is_marked_as_playing_with_gaps() {
    let mut b = Builder {
        image: build_volume(),
    };
    let length = 8 * CLUSTER;
    let whole = mp3_frames(length);
    // song.mp3 loses 300 bytes at the start of one cluster; wreck.mp3 loses most of itself.
    let mut song = whole.clone();
    song[3 * CLUSTER..3 * CLUSTER + 300].copy_from_slice(&noise(77, 300));
    let mut wreck = whole.clone();
    wreck[CLUSTER..].copy_from_slice(&noise(78, length - CLUSTER));
    for (first, data) in [(150u32, &song), (160u32, &wreck)] {
        for (index, piece) in data.chunks(CLUSTER).enumerate() {
            b.put(first + index as u32, 0, piece);
        }
    }
    b.directory(
        28,
        &[
            entry_set("song.mp3", false, 150, length as u64, true, true),
            entry_set("wreck.mp3", false, 160, length as u64, true, true),
        ],
    );
    let (engine, session, dir) = engine_with_image("gaps", b.image);
    let song_id = id_named(&engine, "song.mp3");
    let wreck_id = id_named(&engine, "wreck.mp3");
    call(
        &engine,
        "file_tests_start",
        json!({"ids": [song_id, wreck_id]}),
    );
    wait_for_file_tests(&session);
    let verdict = |id: i64| call(&engine, "entry", json!({"id": id}))["entry"].clone();
    let song = verdict(song_id);
    assert_eq!(song["verdict"], "tested-gaps", "{song}");
    assert!(song["reason"]
        .as_str()
        .expect("reason")
        .contains("It plays"));
    assert_eq!(verdict(wreck_id)["verdict"], "tested-bad");

    // A rebuild keeps the same judgement.
    call(&engine, "resolve", json!({}));
    assert_eq!(verdict(song_id)["verdict"], "tested-gaps");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn rename_residue_is_hidden_by_default_and_damage_is_flagged() {
    let volume = open_volume();
    let (workspace, dir) = temp_workspace("hidden");
    scan(&volume, &workspace);
    let database = MapDb::open(&workspace.database()).expect("database");

    let visible = database.children(20, 0, 100, false, 0.0).expect("children");
    assert!(visible
        .rows
        .iter()
        .all(|row| row.verdict != "rename-residue"));

    let found = database.search("bXoken", 10, 0.0).expect("search");
    assert_eq!(
        found.rows.len(),
        1,
        "damaged entries stay searchable, not hidden"
    );
    assert_eq!(found.rows[0].verdict, "damaged-entry");
    assert!(found.rows[0].confidence < 0.5, "and carry a low score");
    assert!(database
        .search("bXoken", 10, 0.9)
        .expect("search")
        .rows
        .is_empty());
    assert_eq!(database.sets_in_cluster(22).expect("count"), 1);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn read_stream_follows_a_non_contiguous_chain_and_stops_at_valid_length() {
    let volume = open_volume();
    let length = 3 * CLUSTER as u64 - 100;
    let bytes = volume
        .read_stream(30, length, length, false, u64::MAX)
        .expect("stream");
    assert_eq!(bytes.len() as u64, length);
    assert!(bytes.starts_with(b"ID3\x03"));
    assert_eq!(bytes[CLUSTER..CLUSTER + 64], noise(31, 64)[..]);
    assert_eq!(bytes[CLUSTER * 2..CLUSTER * 2 + 64], noise(33, 64)[..]);
    let capped = volume
        .read_stream(30, length, length, false, 10)
        .expect("capped");
    assert_eq!(capped.len(), 10);
}

#[test]
fn owners_finds_a_file_from_any_of_its_clusters() {
    // A deleted entry whose chain starts in the middle of track.mp3's chain, at cluster 31.
    let mut b = Builder {
        image: build_volume(),
    };
    b.directory(
        27,
        &[entry_set(
            "tail.mp3",
            false,
            31,
            2 * CLUSTER as u64,
            false,
            false,
        )],
    );
    let volume =
        Volume::open(Arc::new(MemorySource::new(b.image))).expect("synthetic volume opens");
    let (workspace, dir) = temp_workspace("owners");
    scan(&volume, &workspace);
    let database = MapDb::open(&workspace.database()).expect("database");

    let chained = database
        .owners(&volume, 33)
        .expect("owners of a chain's tail");
    assert!(chained.iter().any(|row| row.name == "track.mp3"));
    assert!(
        chained.iter().any(|row| row.name == "tail.mp3"),
        "a chain claimed from its middle is found too"
    );
    let contiguous = database
        .owners(&volume, 41)
        .expect("owners of a contiguous file's second cluster");
    assert!(contiguous.iter().any(|row| row.name == "photo.jpg"));
    assert!(database
        .owners(&volume, 150)
        .expect("owners of an unused cluster")
        .is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_stopped_scan_resumes_to_the_same_result() {
    let volume = open_volume();

    let (whole, whole_dir) = temp_workspace("whole");
    scan(&volume, &whole);
    let whole_map = MapDb::open(&whole.database()).expect("database");
    let whole_sets = read_progress(&whole_map, &volume)
        .expect("progress")
        .sets_found;

    let (parts, parts_dir) = temp_workspace("parts");
    let stop = AtomicBool::new(false);
    let paused = run_scan(
        &volume,
        &parts,
        ScanOptions { chunk_clusters: 16 },
        &stop,
        |_| {
            stop.store(true, Ordering::Relaxed);
        },
    )
    .expect("first part");
    assert_eq!(paused.status, "paused");
    assert_eq!(paused.clusters_scanned, 16);

    scan(&volume, &parts);
    let parts_map = MapDb::open(&parts.database()).expect("database");
    let finished = read_progress(&parts_map, &volume).expect("progress");
    assert_eq!(finished.status, "complete");
    assert_eq!(finished.sets_found, whole_sets);
    assert_eq!(
        std::fs::read(whole.classes()).expect("classes"),
        std::fs::read(parts.classes()).expect("classes"),
    );

    let _ = std::fs::remove_dir_all(whole_dir);
    let _ = std::fs::remove_dir_all(parts_dir);
}

#[test]
fn a_second_scan_is_refused_while_the_lock_is_held() {
    let volume = open_volume();
    let (workspace, dir) = temp_workspace("lock");
    let holder = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(workspace.lock())
        .expect("lock file");
    holder.lock().expect("hold lock");
    let stop = AtomicBool::new(false);
    let result = run_scan(
        &volume,
        &workspace,
        ScanOptions { chunk_clusters: 16 },
        &stop,
        |_| {},
    );
    assert!(result.is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn baseline_detects_a_changed_structure_hash() {
    let (workspace, dir) = temp_workspace("baseline");
    let before = open_volume().structures;
    let mut after = before.clone();
    after.bitmap[20] ^= 1;
    let first = workspace.check_baseline(&before).expect("first");
    assert!(first.unchanged && first.first_recording);
    assert_eq!(
        workspace.baseline_copy(),
        Some(before.clone()),
        "a copy is kept"
    );
    let same = workspace.check_baseline(&before).expect("same");
    assert!(same.unchanged && !same.first_recording);
    // A baseline kept before copies were: the copy is made while the disk still matches.
    std::fs::remove_dir_all(dir.join("baseline")).expect("remove copy");
    assert_eq!(workspace.baseline_copy(), None);
    workspace.check_baseline(&before).expect("again");
    assert_eq!(workspace.baseline_copy(), Some(before.clone()));
    assert!(!workspace.check_baseline(&after).expect("changed").unchanged);
    assert_eq!(
        workspace.baseline_copy(),
        Some(before.clone()),
        "a changed disk never replaces the copy"
    );
    let changed = workspace.check_baseline(&after).expect("changed");
    assert!(!changed.unchanged);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_write_after_the_baseline_is_explained_and_its_victims_named() {
    // A deleted photo at cluster 150, free in the bitmap, in an orphan folder.
    let mut b = Builder {
        image: build_volume(),
    };
    b.directory(
        27,
        &[entry_set(
            "lost.jpg",
            false,
            150,
            CLUSTER as u64,
            true,
            false,
        )],
    );
    b.put(150, 0, &[0xFF, 0xD8, 0xFF, 0xE0]);
    b.put(150, 16, &noise(150, CLUSTER - 16));
    let before = b.image.clone();

    let (workspace, dir) = temp_workspace("written");
    let first = Session::with_source(
        Path::new("/test/image"),
        Arc::new(MemorySource::new(before)),
        workspace,
    )
    .expect("first session");
    scan(&first.volume, &first.workspace);
    let status = first.status().expect("status");
    assert!(status.changes.is_none() && !status.structures_from_baseline);
    drop(first);

    // Then something mounts it: a new folder in the root takes cluster 150 and writes there.
    b.put(
        ROOT_CLUSTER,
        256,
        &entry_set(".fseventsd", true, 150, CLUSTER as u64, true, true),
    );
    b.allocate(150);
    b.put(150, 0, &noise(9150, CLUSTER));
    // and breaks track.mp3's chain in the FAT on disk.
    b.fat(31, 0);
    let second = Session::with_source(
        Path::new("/test/image"),
        Arc::new(MemorySource::new(b.image)),
        Workspace::at(&dir).expect("workspace"),
    )
    .expect("second session");
    let status = second.status().expect("status");
    assert!(!status.baseline.unchanged);
    assert!(status.structures_from_baseline);
    // The map reads the structures from the baseline's copy, so the chain is still whole
    // and the photo's cluster still counts as free.
    assert_eq!(
        second
            .volume
            .clusters_for(30, 3 * CLUSTER as u64 - 100, false)
            .clusters,
        vec![30, 31, 33]
    );
    assert!(!second.volume.is_allocated(150));
    let summary = status.changes.expect("the change is explained");
    assert_eq!(summary.written_clusters, 1);
    assert_eq!(summary.root_entries, vec![".fseventsd".to_string()]);
    assert_eq!(summary.affected_files, 1);
    let changes = second.structure_changes().expect("changes").expect("some");
    assert_eq!(changes.written_clusters, vec![150]);
    assert_eq!(changes.affected[0].name, "lost.jpg");
    assert!(changes.root_entries[0].before.is_none());

    second.resolve().expect("resolve");
    let database = MapDb::open(&second.workspace.database()).expect("database");
    assert_eq!(verdict_of(&database, 27, "lost.jpg"), "overwritten");

    let _ = std::fs::remove_dir_all(dir);
}

/// An engine with a scanned synthetic volume open, as the app would have it.
fn engine_with_volume(label: &str) -> (Arc<Engine>, Arc<Session>, PathBuf) {
    engine_with_image(label, build_volume())
}

fn engine_with_image(label: &str, image: Vec<u8>) -> (Arc<Engine>, Arc<Session>, PathBuf) {
    let (workspace, dir) = temp_workspace(label);
    let source = Arc::new(MemorySource::new(image));
    let session =
        Session::with_source(Path::new("/test/image"), source, workspace).expect("session");
    scan(&session.volume, &session.workspace);
    let engine = Engine::new();
    engine.set_session(session.clone());
    (engine, session, dir)
}

fn call(engine: &Engine, tool: &str, args: serde_json::Value) -> serde_json::Value {
    engine
        .call(tool, &args, AUTHOR_CLAUDE)
        .unwrap_or_else(|error| panic!("{tool} failed: {error:#}"))
}

#[test]
fn tree_path_leads_from_the_top_folder_down_to_a_selection() {
    let (engine, _session, dir) = engine_with_volume("tree-path");
    let track = call(&engine, "search", json!({"query": "track.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    let heads: Vec<u64> = call(&engine, "tree_path", json!({"entry": track}))["heads"]
        .as_array()
        .expect("heads")
        .iter()
        .map(|head| head.as_u64().expect("head"))
        .collect();
    assert_eq!(heads.last(), Some(&20), "{heads:?}");
    let roots = call(&engine, "roots", json!({}));
    let top = roots["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .filter_map(|row| row["head"].as_u64())
        .collect::<Vec<_>>();
    assert!(
        top.contains(&heads[0]),
        "{heads:?} starts at a top folder of {top:?}"
    );
    assert_eq!(
        call(&engine, "tree_path", json!({"folder": 20}))["heads"],
        json!(heads)
    );
    assert!(engine.call("tree_path", &json!({}), AUTHOR_CLAUDE).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_damaged_entry_is_followed_to_the_real_folder_behind_it() {
    let (engine, _session, dir) = engine_with_volume("follow");

    // Strict parsing would stop at cluster 23. The trail continues instead.
    let lost = call(&engine, "node", json!({"head": 24}));
    assert_eq!(
        lost["name"], "Xost-album",
        "the damaged name is kept, not guessed at"
    );
    assert_eq!(lost["path"], "[orphan @23]/Xost-album");
    assert_eq!(lost["linkConfidence"], 0.8);
    assert_eq!(
        lost["intactFiles"], 1,
        "the undamaged file behind the damaged link is found"
    );

    let children = call(&engine, "children", json!({"node": 24}));
    assert_eq!(children["rows"][0]["name"], "deep.mp3");
    assert_eq!(children["rows"][0]["verdict"], "likely-intact");

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn scores_are_graded_and_the_focus_floor_filters_by_them() {
    let (engine, _session, dir) = engine_with_volume("floor");

    let everything = call(
        &engine,
        "children",
        json!({"node": 20, "minConfidence": 0.0}),
    );
    let confidence_of = |name: &str| {
        everything["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["name"] == name)
            .expect("row")["confidence"]
            .as_f64()
            .expect("number")
    };
    let intact = confidence_of("track.mp3");
    let plausible = confidence_of("notes.xyz");
    let half_chain = confidence_of("short.bin");
    let overwritten = confidence_of("wiped.mp3");
    assert_eq!(intact, 1.0);
    assert!(
        plausible < intact && plausible > half_chain,
        "{plausible} {half_chain}"
    );
    assert!(overwritten < half_chain, "{overwritten} {half_chain}");

    let focused = call(
        &engine,
        "children",
        json!({"node": 20, "minConfidence": 0.9}),
    );
    let names: Vec<&str> = focused["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| row["name"].as_str().unwrap_or(""))
        .collect();
    assert!(names.contains(&"track.mp3"));
    assert!(
        !names.contains(&"short.bin"),
        "low-confidence data drops out of focus"
    );
    assert!(!names.contains(&"wiped.mp3"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn nameless_chains_and_bridges_route_around_a_broken_chain() {
    let (engine, _session, dir) = engine_with_volume("bridge");

    let nameless = call(&engine, "nameless_chains", json!({"headClass": CLASS_JPEG}));
    assert_eq!(nameless["total"], 1);
    assert_eq!(nameless["chains"][0]["head"], 100);
    assert_eq!(nameless["chains"][0]["length"], 2);

    let broken = call(&engine, "broken_files", json!({}));
    let short = broken
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["name"] == "short.bin")
        .expect("short.bin");
    let id = short["id"].as_i64().expect("id");

    let candidates = call(&engine, "bridge_candidates", json!({"entryId": id}));
    assert_eq!(
        candidates[0]["island"]["head"], 75,
        "the matching fragment ranks first: {candidates}"
    );

    // Applying the bridge as an overlay raises the score, and withdrawing it restores it.
    let before = call(&engine, "entry", json!({"id": id}))["entry"]["confidence"]
        .as_f64()
        .expect("number");
    let overlay = call(
        &engine,
        "add_overlay",
        json!({
            "kind": "file_extents",
            "target": id,
            "payload": {"extents": [{"first": 70, "count": 1}, {"first": 75, "count": 1}]},
            "note": "Nameless chain at 75 fits the gap exactly.",
        }),
    );
    let after = call(&engine, "entry", json!({"id": id}));
    assert!(after["entry"]["confidence"].as_f64().expect("number") > before);
    assert_eq!(after["clusters"], 2);
    assert_eq!(after["usesOverlay"], true);

    call(
        &engine,
        "set_overlay_active",
        json!({"id": overlay["id"], "active": false}),
    );
    let restored = call(&engine, "entry", json!({"id": id}));
    assert_eq!(
        restored["entry"]["confidence"].as_f64().expect("number"),
        before
    );
    assert_eq!(restored["usesOverlay"], false);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn overlays_can_correct_an_entry_and_move_an_orphan_without_touching_raw_values() {
    let (engine, session, dir) = engine_with_volume("overlay");

    let found = call(&engine, "search", json!({"query": "bXoken"}));
    let id = found["rows"][0]["id"].as_i64().expect("id");
    call(
        &engine,
        "add_overlay",
        json!({
            "kind": "accept_entry",
            "target": id,
            "payload": {"name": "broken.jpg"},
            "note": "One character is corrupted.",
        }),
    );
    let corrected = call(&engine, "entry", json!({"id": id}));
    assert_eq!(corrected["entry"]["name"], "broken.jpg");
    assert_eq!(corrected["entry"]["checksumOk"], true);
    let raw = session
        .query_map(&format!(
            "SELECT name, checksum_ok FROM sets WHERE id = {id}"
        ))
        .expect("query");
    assert_eq!(
        raw["rows"][0]["name"], "bXoken.jpg",
        "the raw value stays as found"
    );
    assert_eq!(raw["rows"][0]["checksum_ok"], 0);

    call(
        &engine,
        "add_overlay",
        json!({
            "kind": "folder_parent",
            "target": 20,
            "payload": {"parent": 5, "name": "recovered"},
            "note": "Test: attach the orphan under visible.",
        }),
    );
    assert_eq!(
        call(&engine, "node", json!({"head": 20}))["path"],
        "/visible/recovered"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn map_layers_downsample_and_a_lens_pulls_matching_clusters_into_focus() {
    let (_engine, session, dir) = engine_with_volume("lens");
    let last = session.volume.boot.last_cluster_exclusive();

    let allocation = session.map_bytes(
        Layer::Allocation,
        2,
        last,
        CLUSTER_COUNT as usize,
        0.0,
        None,
    );
    assert_eq!(allocation[(10 - 2) as usize], ALLOC_ALLOCATED);
    assert_eq!(allocation[(30 - 2) as usize], ALLOC_FREE_FAT);
    assert_eq!(allocation[(40 - 2) as usize], ALLOC_FREE_ZERO);

    let confidence = session.map_bytes(
        Layer::Confidence,
        2,
        last,
        CLUSTER_COUNT as usize,
        0.0,
        None,
    );
    assert_eq!(confidence[(30 - 2) as usize], 255);
    let focused = session.map_bytes(
        Layer::Confidence,
        2,
        last,
        CLUSTER_COUNT as usize,
        0.9,
        None,
    );
    assert_eq!(focused[(30 - 2) as usize], 255);
    assert_eq!(
        focused[(70 - 2) as usize],
        0,
        "below the floor, a cluster drops out of focus"
    );

    let ownership = session.map_bytes(Layer::Ownership, 2, last, CLUSTER_COUNT as usize, 0.0, None);
    assert_eq!(
        ownership[(100 - 2) as usize],
        5,
        "a nameless chain is shown as its own category"
    );

    let lens = Lens {
        ownership: Some(vec![4]),
        ..Lens::default()
    };
    let view = session.map_bytes(
        Layer::Lens,
        2,
        last,
        CLUSTER_COUNT as usize,
        0.0,
        Some(&lens),
    );
    assert!(view[(100 - 2) as usize] > 0);
    assert_eq!(view[(10 - 2) as usize], 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn format_walkers_find_where_a_stream_breaks() {
    // Twenty valid 128 kbps frames, with garbage spliced into the middle.
    let frame_length = 417;
    let mut frame = vec![0u8; frame_length];
    frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x00]);
    let good: Vec<u8> = frame
        .iter()
        .copied()
        .cycle()
        .take(frame_length * 20)
        .collect();
    let clean = verify_mp3(&good);
    assert_eq!(clean.units, 20);
    assert!(clean.breaks.is_empty());
    assert_eq!(clean.valid_fraction, 1.0);

    let mut spliced = good[..frame_length * 10].to_vec();
    spliced.extend(std::iter::repeat_n(0u8, 1000));
    spliced.extend_from_slice(&good[..frame_length * 10]);
    let broken = verify_mp3(&spliced);
    // The tenth frame is the last before the splice. It cannot be confirmed,
    // because no valid header follows it, so the break is reported at its start.
    assert_eq!(
        broken.breaks.first().copied(),
        Some((frame_length * 9) as u64),
        "{:?}",
        broken.breaks
    );
    assert!(broken.valid_fraction < 1.0);
    assert!(
        broken.units >= 18,
        "the walk recovers after the damage: {}",
        broken.units
    );

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    png.extend_from_slice(&[0, 0, 0, 0]);
    png.extend_from_slice(b"IEND");
    png.extend_from_slice(&[0xAE, 0x42, 0x60, 0x82]);
    let report = verify_png(&png);
    assert!(report.reached_end && report.breaks.is_empty(), "{report:?}");
    let last = png.len() - 1;
    png[last] ^= 0xFF;
    assert_eq!(
        verify_png(&png).breaks.len(),
        1,
        "a CRC mismatch is reported"
    );
}

#[test]
fn test_stream_scores_a_proposed_join() {
    let (engine, _session, dir) = engine_with_volume("stream");
    let report = call(
        &engine,
        "test_stream",
        json!({
            "extents": [{"first": 30, "count": 2}, {"first": 33, "count": 1}],
            "length": 3 * CLUSTER - 100,
            "name": "track.mp3",
        }),
    );
    assert_eq!(report["clusters"], 3);
    assert_eq!(report["report"]["format"], "mp3");
    assert_eq!(report["extents"].as_array().expect("extents").len(), 2);
    let _ = std::fs::remove_dir_all(dir);
}

/// An MP3 stream of 417-byte frames whose data is seeded, so two seeds are two songs.
fn mp3_stream(seed: u32, bytes: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes + 417);
    let mut n = 0u32;
    while out.len() < bytes {
        out.extend_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
        out.extend(noise(seed * 1000 + n, 413));
        n += 1;
    }
    out.truncate(bytes);
    out
}

#[test]
fn a_bridge_counts_only_when_the_stream_crosses_the_join() {
    let mut b = Builder {
        image: build_volume(),
    };
    let song = mp3_stream(1, 4 * CLUSTER);
    // The file's chain stops after two clusters; its true rest lies at 170, another song at 180.
    for (cluster, piece) in [(150u32, 0usize), (151, 1), (170, 2), (171, 3)] {
        b.put(cluster, 0, &song[piece * CLUSTER..(piece + 1) * CLUSTER]);
    }
    let other = mp3_stream(2, 2 * CLUSTER);
    b.put(180, 0, &other[..CLUSTER]);
    b.put(181, 0, &other[CLUSTER..]);
    for (from, to) in [(150, 151), (170, 171), (180, 181)] {
        b.fat(from, to);
        b.fat(to, FAT_END);
    }
    b.directory(
        28,
        &[entry_set(
            "cut.mp3",
            false,
            150,
            4 * CLUSTER as u64,
            false,
            true,
        )],
    );
    let (engine, _session, dir) = engine_with_image("join", b.image);
    let id = id_named(&engine, "cut.mp3");
    let try_bridge = |first: u32| {
        call(
            &engine,
            "test_stream",
            json!({"entryId": id, "appendExtents": [{"first": first, "count": 2}], "summary": true}),
        )
    };
    let true_rest = try_bridge(170);
    assert_eq!(true_rest["join"]["offset"], 2 * CLUSTER);
    assert_eq!(true_rest["join"]["crossing"], "clean", "{true_rest}");
    let decoy = try_bridge(180);
    assert_eq!(decoy["join"]["crossing"], "breaks", "{decoy}");
    // The decoy is still mostly valid frames, which is why the valid fraction alone misleads.
    assert!(decoy["report"]["validFraction"].as_f64().expect("fraction") > 0.9);
    let plain = call(
        &engine,
        "test_stream",
        json!({"entryId": id, "summary": true}),
    );
    assert!(plain.get("join").is_none());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn scripts_can_analyse_but_have_no_way_to_write() {
    let (engine, _session, dir) = engine_with_volume("script");

    let result = call(
        &engine,
        "run_script",
        json!({"source": r#"
        let data = read_cluster(30);
        print(ascii(data, 0, 3));
        let total = 0;
        for c in chain(30, 10) { total += 1; }
        #{ clusters: total, class: class_of(30), next: fat(30), free: !allocated(30), rows: sql("SELECT COUNT(*) AS n FROM islands")[0].n }
    "#}),
    );
    assert_eq!(result["printed"], "ID3\n", "{result}");
    assert_eq!(result["result"]["clusters"], 3);
    assert_eq!(result["result"]["class"], "MP3 start");
    assert_eq!(result["result"]["next"], 31);
    assert_eq!(result["result"]["free"], true);
    assert!(result["result"]["rows"].as_i64().expect("count") > 0);

    for attempt in ["write(0, 1)", r#"open("/etc/hosts")"#, r#"system("ls")"#] {
        let refused = call(&engine, "run_script", json!({"source": attempt}));
        assert!(
            refused.get("error").is_some(),
            "{attempt} must not exist: {refused}"
        );
    }

    let endless = call(
        &engine,
        "run_script",
        json!({"source": "loop { }", "timeoutSeconds": 1}),
    );
    assert!(
        endless["error"]
            .as_str()
            .expect("error")
            .contains("time limit"),
        "{endless}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn breakdown_and_chain_summary_count_without_names() {
    let (engine, _session, dir) = engine_with_volume("breakdown");

    for (limit, count, stopped, last) in [
        (0, 0, "limit", "next cluster is 100"),
        (1, 1, "limit", "next cluster is 101"),
        (2, 2, "limit", "end of chain"),
        (3, 2, "chain left the valid cluster range", "end of chain"),
    ] {
        let followed = call(
            &engine,
            "follow_chain",
            json!({"cluster": 100, "max": limit}),
        );
        assert_eq!(followed["clusters"], count);
        assert_eq!(followed["stoppedBecause"], stopped);
        assert_eq!(followed["lastFatValue"], last);
    }

    let by_reason = call(&engine, "breakdown", json!({"by": ["verdict", "reason"]}));
    let rows = by_reason["rows"].as_array().expect("rows");
    let has = |verdict: &str, reason: &str| {
        rows.iter()
            .any(|row| row["verdict"] == verdict && row["reason"] == reason)
    };
    assert!(
        has("partial", "chain_broken"),
        "short.bin stops early: {rows:?}"
    );
    assert!(has("likely-intact", "content_matches"));
    assert!(has("rename-residue", "rename_residue"));
    assert!(rows
        .iter()
        .all(|row| row.get("name").is_none() && row.get("path").is_none()));
    let total: i64 = rows
        .iter()
        .map(|row| row["files"].as_i64().expect("files"))
        .sum();
    assert_eq!(by_reason["totalFiles"].as_i64(), Some(total));

    let music = call(
        &engine,
        "breakdown",
        json!({"by": ["extension"], "type": ["music"], "deleted": false}),
    );
    assert!(music["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .all(|row| row["extension"] == "mp3"));
    assert!(
        engine
            .call("breakdown", &json!({"by": ["name"]}), AUTHOR_CLAUDE)
            .is_err(),
        "names are not a dimension"
    );

    let chains = call(&engine, "chain_summary", json!({}));
    assert!(chains["groups"]
        .as_array()
        .expect("groups")
        .iter()
        .any(|group| group["headContent"] == "JPEG start" && group["chains"] == 1));

    let tools = tool_definitions();
    let query = tools
        .iter()
        .find(|tool| tool["name"] == "query_map")
        .expect("query_map");
    let description = query["description"].as_str().expect("description");
    assert!(description.contains("reason_code") && description.contains("islands(head"));

    let _ = std::fs::remove_dir_all(dir);

    let mut builder = Builder {
        image: build_volume(),
    };
    builder.fat(101, 100);
    let (engine, _session, dir) = engine_with_image("chain-cycle", builder.image);
    let followed = call(&engine, "follow_chain", json!({"cluster": 100}));
    assert_eq!(followed["clusters"], 2);
    assert_eq!(followed["stoppedBecause"], "chain loops back on itself");
    assert_eq!(followed["lastFatValue"], "next cluster is 100");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hidden_names_are_masked_for_agents_but_not_for_the_window() {
    let (engine, _session, dir) = engine_with_volume("privacy");
    engine.set_names_hidden(true);
    assert_eq!(
        call(&engine, "agent_privacy", json!({}))["namesHidden"],
        true
    );

    let found = call(&engine, "search", json!({"query": "track"}));
    let rows = found["rows"].as_array().expect("rows");
    assert!(!rows.is_empty());
    assert!(
        rows.iter().all(|row| row["name"] == "[hidden].mp3"),
        "{rows:?}"
    );
    let seen = engine
        .call("search", &json!({"query": "track"}), AUTHOR_USER)
        .expect("the window searches");
    assert!(seen["rows"][0]["name"]
        .as_str()
        .expect("name")
        .contains("track"));

    let sql = call(
        &engine,
        "query_map",
        json!({"sql": "SELECT name AS n, length FROM sets WHERE is_dir = 0 LIMIT 3"}),
    );
    assert!(sql["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .all(|row| row["n"].is_null() && row["length"].is_number()));
    let script = call(
        &engine,
        "run_script",
        json!({"source": "sql(\"SELECT name FROM sets WHERE is_dir = 0 LIMIT 1\")[0].name"}),
    );
    assert!(script["result"].is_null(), "{script}");

    let refused = engine.call(
        "materialize",
        &json!({"extents": [{"first": 40, "count": 2}], "name": "photo.jpg"}),
        AUTHOR_CLAUDE,
    );
    assert!(format!("{:#}", refused.expect_err("previews are refused")).contains("kept file names"));
    assert!(
        engine
            .call(
                "set_agent_privacy",
                &json!({"namesHidden": false}),
                AUTHOR_CLAUDE
            )
            .is_err(),
        "only the person decides"
    );

    engine.set_names_hidden(false);
    let open = call(&engine, "search", json!({"query": "track"}));
    assert!(open["rows"][0]["name"]
        .as_str()
        .expect("name")
        .contains("track"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_map_query_tool_cannot_write() {
    let (engine, session, dir) = engine_with_volume("sql");
    let rows = call(
        &engine,
        "query_map",
        json!({"sql": "SELECT COUNT(*) AS n FROM sets"}),
    );
    let before = rows["rows"][0]["n"].as_i64().expect("count");
    assert!(before > 0);
    for attempt in [
        "DELETE FROM sets",
        "DROP TABLE sets",
        "UPDATE sets SET name = 'x'",
    ] {
        assert!(
            session.query_map(attempt).is_err(),
            "{attempt} must be refused"
        );
    }
    let after = call(
        &engine,
        "query_map",
        json!({"sql": "SELECT COUNT(*) AS n FROM sets"}),
    );
    assert_eq!(after["rows"][0]["n"].as_i64().expect("count"), before);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn custom_structures_decode_with_labels_and_repeat() {
    let (engine, _session, dir) = engine_with_volume("struct");
    let offset = salvage_core_cluster_offset(30);
    let fields = call(
        &engine,
        "decode_struct",
        json!({
            "offset": offset,
            "definition": {
                "count": 2,
                "stride": 16,
                "fields": [
                    {"name": "magic", "offset": 0, "type": "ascii", "length": 3},
                    {"name": "version", "offset": 3, "type": "u8"},
                ],
            },
        }),
    );
    assert_eq!(fields[0]["name"], "[0].magic");
    assert_eq!(fields[0]["value"], "ID3");
    assert_eq!(fields[1]["value"], 3);
    assert_eq!(fields.as_array().expect("fields").len(), 4);
    let _ = std::fs::remove_dir_all(dir);
}

fn salvage_core_cluster_offset(cluster: u32) -> u64 {
    (HEAP_OFFSET as u64) * SECTOR as u64 + u64::from(cluster - 2) * CLUSTER as u64
}

#[test]
fn byte_search_finds_patterns_with_alignment_and_resumes() {
    let (engine, _session, dir) = engine_with_volume("find");
    let found = call(&engine, "find_bytes", json!({"text": "ID3"}));
    let hits: Vec<u64> = found["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .map(|hit| hit.as_u64().expect("offset"))
        .collect();
    assert!(hits.contains(&salvage_core_cluster_offset(30)));
    assert_eq!(found["finished"], true);
    let limited = call(&engine, "find_bytes", json!({"text": "ID3", "limit": 1}));
    assert_eq!(limited["hits"].as_array().expect("hits").len(), 1);
    assert_eq!(
        limited["finished"], false,
        "reaching the limit reports where to resume"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn mcp_protocol_lists_every_tool_and_reports_tool_errors_in_band() {
    let (engine, _session, dir) = engine_with_volume("mcp");

    let init = handle_message(&engine, &json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}},
    })).expect("reply");
    assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
    assert_eq!(init["result"]["serverInfo"]["name"], "captain-salvage");

    assert!(handle_message(
        &engine,
        &json!({"jsonrpc": "2.0", "method": "notifications/initialized"})
    )
    .is_none());

    let listed = handle_message(
        &engine,
        &json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}),
    )
    .expect("reply");
    let tools = listed["result"]["tools"].as_array().expect("tools");
    assert_eq!(tools.len(), tool_definitions().len());

    // Every advertised tool must exist. An unknown tool would say so in its error.
    for definition in tools {
        let name = definition["name"].as_str().expect("name");
        if matches!(name, "start_scan" | "open_volume") {
            continue;
        }
        let reply = handle_message(&engine, &json!({
            "jsonrpc": "2.0", "id": 3, "method": "tools/call", "params": {"name": name, "arguments": {}},
        })).expect("reply");
        let text = reply["result"]["content"][0]["text"]
            .as_str()
            .expect("text");
        assert!(
            !text.contains("unknown tool"),
            "{name} is advertised but not implemented"
        );
    }

    let failed = handle_message(
        &engine,
        &json!({
            "jsonrpc": "2.0", "id": 4, "method": "tools/call",
            "params": {"name": "inspect_cluster", "arguments": {"cluster": 99999999}},
        }),
    )
    .expect("reply");
    assert_eq!(failed["result"]["isError"], true);

    let missing = handle_message(
        &engine,
        &json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}),
    )
    .expect("reply");
    assert_eq!(missing["error"]["code"], -32601);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_activity_log_shows_what_the_ai_did_and_a_selection_is_shared() {
    let (engine, _session, dir) = engine_with_volume("activity");
    call(&engine, "inspect_cluster", json!({"cluster": 30}));
    let log = call(&engine, "activity", json!({}));
    assert_eq!(log[0]["action"], "inspect_cluster");
    assert_eq!(log[0]["author"], AUTHOR_CLAUDE);

    engine
        .call("set_selection", &json!({"cluster": 30}), "you")
        .expect("selection");
    assert_eq!(call(&engine, "get_selection", json!({}))["cluster"], 30);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn whole_disk_paths_derive_from_partition_paths() {
    assert_eq!(
        whole_disk_path(Path::new("/dev/rdisk12s2")),
        Some(PathBuf::from("/dev/rdisk12"))
    );
    assert_eq!(
        whole_disk_path(Path::new("/dev/disk3s1")),
        Some(PathBuf::from("/dev/rdisk3"))
    );
    assert_eq!(whole_disk_path(Path::new("/dev/rdisk12")), None);
    assert_eq!(whole_disk_path(Path::new("/tmp/image.bin")), None);
}

#[test]
fn a_small_file_is_judged_by_its_own_bytes_and_not_by_older_data_in_its_cluster() {
    let (engine, _session, dir) = engine_with_volume("slack");
    let children = call(
        &engine,
        "children",
        json!({"node": 25, "includeHidden": true}),
    );
    let verdict = |name: &str| {
        children["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["name"] == name)
            .expect("row")["verdict"]
            .clone()
    };
    assert_eq!(
        verdict("small.php"),
        "likely-intact",
        "older data after the file's end must not condemn it: {children}"
    );
    assert_eq!(
        verdict("clobbered.php"),
        "overwritten",
        "random bytes at the start of a text file still do"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_jpeg_walker_sees_an_end_marker_in_the_last_two_bytes() {
    let mut jpeg = vec![
        0xFF, 0xD8, 0xFF, 0xDA, 0x00, 0x02, 0x11, 0x22, 0xFF, 0x00, 0x33, 0xFF, 0xD9,
    ];
    let report = salvage_core::verify::verify_jpeg(&jpeg);
    assert!(report.reached_end, "{report:?}");
    assert_eq!(report.valid_fraction, 1.0);
    jpeg.truncate(jpeg.len() - 2);
    assert!(!salvage_core::verify::verify_jpeg(&jpeg).reached_end);
}

#[test]
fn opening_the_same_volume_again_reuses_the_session() {
    let (engine, session, dir) = engine_with_volume("reopen");
    let again = engine.open("/test/image").expect("same path");
    assert!(Arc::ptr_eq(&session, &again));
    let _ = std::fs::remove_dir_all(dir);
}

fn copy_destination(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("salvage-copy-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Runs a copy as the user and waits for it to finish.
fn copy_and_wait(
    engine: &Engine,
    session: &Session,
    args: serde_json::Value,
) -> salvage_core::export::CopyStatus {
    engine
        .call("copy_start", &args, "you")
        .expect("copy starts");
    loop {
        let status = session.copy_status().expect("status");
        if status.state != "running" {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

fn manifest_rows(destination: &Path) -> Vec<serde_json::Value> {
    std::fs::read_to_string(destination.join(salvage_core::export::MANIFEST_NAME))
        .expect("manifest")
        .lines()
        .map(|line| serde_json::from_str(line).expect("json"))
        .collect()
}

#[test]
fn copy_out_recreates_a_lost_folder_with_correct_bytes_and_a_manifest() {
    let (engine, session, dir) = engine_with_volume("copy");
    let destination = copy_destination("basic");
    let status = copy_and_wait(
        &engine,
        &session,
        json!({"destination": destination, "folders": [20]}),
    );
    assert_eq!(status.state, "finished", "{status:?}");

    // track.mp3 is chained across clusters 30, 31 and 33, which do not follow on disk.
    let track = std::fs::read(destination.join("Lost folder 20/track.mp3")).expect("track");
    assert_eq!(track.len(), 3 * CLUSTER - 100);
    assert!(track.starts_with(b"ID3\x03"));
    assert_eq!(track[CLUSTER * 2..CLUSTER * 2 + 64], noise(33, 64)[..]);

    // Subfolders come along, and the unconfirmed rename leftover does not.
    assert!(destination.join("Lost folder 20/album/song.mp3").exists());
    assert!(!destination
        .join("Lost folder 20/.track.mp3.abc123")
        .exists());
    // Partial files are skipped unless asked for.
    assert!(!destination.join("Lost folder 20/short.bin").exists());
    assert!(!destination.join("Lost folder 20/reused.jpg").exists());

    let rows = manifest_rows(&destination);
    let row = rows
        .iter()
        .find(|row| row["name"] == "track.mp3")
        .expect("manifest row");
    assert_eq!(row["status"], "complete");
    assert_eq!(row["sha256"].as_str().expect("hash").len(), 64);
    assert_eq!(row["format"], "mp3");

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn copy_out_can_include_a_partial_file_and_says_so() {
    let (engine, session, dir) = engine_with_volume("partial");
    let destination = copy_destination("partial");
    let found = call(&engine, "search", json!({"query": "short.bin"}));
    let id = found["rows"][0]["id"].as_i64().expect("id");

    let plan = call(
        &engine,
        "copy_plan",
        json!({"destination": destination, "entryIds": [id]}),
    );
    assert_eq!(plan["files"], 0);
    assert_eq!(plan["skippedPartial"], 1);

    let status = copy_and_wait(
        &engine,
        &session,
        json!({"destination": destination, "entryIds": [id], "includePartial": true}),
    );
    assert_eq!(status.files_partial, 1, "{status:?}");
    let row = &manifest_rows(&destination)[0];
    assert_eq!(row["status"], "partial: chain ended early");
    assert_eq!(row["bytesWritten"], CLUSTER);
    assert_eq!(row["length"], 2 * CLUSTER);

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn copy_out_uses_an_overlay_reassembly() {
    let (engine, session, dir) = engine_with_volume("reassemble");
    let destination = copy_destination("reassemble");
    let id = call(&engine, "search", json!({"query": "short.bin"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    call(
        &engine,
        "add_overlay",
        json!({
            "kind": "file_extents", "target": id,
            "payload": {"extents": [{"first": 70, "count": 1}, {"first": 75, "count": 1}]},
            "note": "test",
        }),
    );
    let status = copy_and_wait(
        &engine,
        &session,
        json!({"destination": destination, "entryIds": [id]}),
    );
    assert_eq!(status.state, "finished");
    let bytes = std::fs::read(destination.join("Lost folder 20/short.bin")).expect("file");
    assert_eq!(bytes.len(), 2 * CLUSTER);
    assert_eq!(bytes[CLUSTER..CLUSTER + 64], noise(75, 64)[..]);
    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn copy_out_never_overwrites_and_a_repeat_run_skips_what_is_done() {
    let (engine, session, dir) = engine_with_volume("repeat");
    let destination = copy_destination("repeat");
    let track_id = call(&engine, "search", json!({"query": "track.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");

    // A file of the same name already exists there, and it must survive.
    let existing = destination.join("Lost folder 20/track.mp3");
    std::fs::create_dir_all(existing.parent().expect("parent")).expect("dirs");
    std::fs::write(&existing, b"precious").expect("write");

    let first = copy_and_wait(
        &engine,
        &session,
        json!({"destination": destination, "entryIds": [track_id]}),
    );
    assert_eq!(first.files_done, 1);
    assert_eq!(std::fs::read(&existing).expect("read"), b"precious");
    let row = &manifest_rows(&destination)[0];
    let renamed = row["target"].as_str().expect("target");
    assert!(renamed.contains("(entry"), "{renamed}");

    let second = copy_and_wait(
        &engine,
        &session,
        json!({"destination": destination, "entryIds": [track_id]}),
    );
    assert_eq!(second.files_done, 0);
    assert_eq!(second.files_skipped, 1);
    assert_eq!(manifest_rows(&destination).len(), 1);

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn files_can_be_browsed_by_type_without_copying() {
    let (engine, _session, dir) = engine_with_volume("browse");
    let types = call(&engine, "browse_types", json!({}));
    let count_of = |group: &str| {
        types
            .as_array()
            .expect("types")
            .iter()
            .find(|item| item["group"] == group)
            .expect("group")["files"]
            .as_u64()
            .expect("files")
    };
    assert!(count_of("photos") >= 2);
    assert_eq!(count_of("music"), 5);

    let photos = call(&engine, "browse_files", json!({"group": "photos"}));
    let rows = photos["rows"].as_array().expect("rows");
    assert_eq!(rows.len() as u64, photos["total"].as_u64().expect("total"));
    assert!(rows.iter().any(|row| row["name"] == "photo.jpg"));
    // Best first, and the leftovers of a rename are not offered as files.
    let scores: Vec<f64> = rows
        .iter()
        .map(|row| row["confidence"].as_f64().expect("score"))
        .collect();
    assert!(scores.windows(2).all(|pair| pair[0] >= pair[1]));
    let music = call(&engine, "browse_files", json!({"group": "music"}));
    assert_eq!(music["rows"].as_array().expect("rows").len(), 5);

    let named = call(
        &engine,
        "browse_files",
        json!({"group": "photos", "text": "reused"}),
    );
    assert_eq!(named["rows"][0]["name"], "reused.jpg");
    assert_eq!(named["total"], 1);
    let picky = call(
        &engine,
        "browse_files",
        json!({"group": "photos", "minConfidence": 2.0}),
    );
    assert_eq!(picky["total"], 0);
    let names_in = |folder: u32| -> Vec<String> {
        call(
            &engine,
            "browse_files",
            json!({"group": "music", "folder": folder}),
        )["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| row["name"].as_str().expect("name").to_string())
            .collect()
    };
    let in_lost_folder = names_in(20);
    assert!(in_lost_folder.iter().any(|name| name == "track.mp3"));
    assert!(!in_lost_folder.iter().any(|name| name == "keep.mp3"));
    assert_eq!(names_in(5), ["keep.mp3"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_preview_reads_any_range_of_a_file_from_its_clusters() {
    let (engine, session, dir) = engine_with_volume("range");
    let id = call(&engine, "search", json!({"query": "track.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    let whole = session.read_entry(id, u64::MAX).expect("read").1;
    assert!(whole.len() > 2);

    let (total, name) = session.entry_total(id).expect("total");
    assert_eq!(total, whole.len() as u64);
    assert_eq!(name, "track.mp3");
    let middle = whole.len() as u64 / 2;
    for (start, length) in [(0, 1), (1, 7), (middle - 1, 3), (0, total), (total - 1, 50)] {
        let part = session.read_entry_range(id, start, length).expect("range");
        let end = (start + length).min(total) as usize;
        assert_eq!(part.data, whole[start as usize..end], "{start}+{length}");
        assert_eq!(part.total, total);
    }
    assert!(session
        .read_entry_range(id, total, 10)
        .expect("range")
        .data
        .is_empty());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn byte_ranges_are_read_the_way_players_send_them() {
    use salvage_core::browse::parse_range;
    assert_eq!(parse_range("bytes=0-1", 100), Some((0, 2)));
    assert_eq!(parse_range("bytes=10-", 100), Some((10, 90)));
    assert_eq!(parse_range("bytes=-20", 100), Some((80, 20)));
    assert_eq!(parse_range("bytes=90-500", 100), Some((90, 10)));
    assert_eq!(parse_range("bytes=100-", 100), None);
    assert_eq!(parse_range("bytes=5-2", 100), None);
    assert_eq!(parse_range("bytes=0-1", 0), None);
    assert_eq!(parse_range("items=0-1", 100), None);
}

#[test]
fn thumbnails_are_decoded_from_real_bytes_and_damage_fails_plainly() {
    use salvage_core::browse::{make_thumbnail, Thumb};
    let mut png = Vec::new();
    image::RgbImage::from_pixel(600, 300, image::Rgb([200, 40, 40]))
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode");
    let small = make_thumbnail(&png).expect("thumbnail");
    assert_eq!(&small[..2], &[0xff, 0xd8], "a JPEG");
    let decoded = image::load_from_memory(&small).expect("decode");
    assert_eq!((decoded.width(), decoded.height()), (256, 128));

    assert!(make_thumbnail(b"this is not an image").is_err());
    assert!(make_thumbnail(&png[..png.len() / 2]).is_err());

    // The test volume's photo holds noise, not an image. It says so, and it stays known.
    let (engine, session, dir) = engine_with_volume("thumbs");
    let id = call(&engine, "search", json!({"query": "photo.jpg"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    assert!(matches!(
        session.entry_thumbnail(id).expect("thumb"),
        Thumb::Failed(_)
    ));
    assert!(matches!(
        session.entry_thumbnail(id).expect("thumb"),
        Thumb::Failed(_)
    ));

    let _ = std::fs::remove_dir_all(dir);
}

/// A short real video made by ffmpeg, or `None` where ffmpeg is not installed.
fn sample_video(label: &str) -> Option<(salvage_core::media::Tools, PathBuf)> {
    let tools = salvage_core::media::detect_tools()?;
    let path = std::env::temp_dir().join(format!("salvage-{}-{label}.avi", std::process::id()));
    let made = std::process::Command::new(&tools.ffmpeg)
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-y",
            "-f",
            "lavfi",
            "-i",
        ])
        .arg("testsrc=duration=8:size=320x240:rate=10")
        .args(["-f", "lavfi", "-i", "sine=duration=8"])
        .args(["-c:v", "mpeg4", "-c:a", "mp3", "-shortest"])
        .arg(&path)
        .status()
        .ok()?;
    made.success().then_some((tools, path))
}

#[test]
fn ffmpeg_is_found_where_it_is_and_missing_is_a_plain_answer() {
    use salvage_core::media::find_tool;
    let dir = std::env::temp_dir().join(format!("salvage-tools-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("dir");
    assert_eq!(find_tool("ffmpeg", std::slice::from_ref(&dir)), None);
    std::fs::write(dir.join("ffmpeg"), b"").expect("write");
    assert_eq!(
        find_tool("ffmpeg", std::slice::from_ref(&dir)),
        Some(dir.join("ffmpeg"))
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_playlist_covers_the_whole_file_in_short_pieces() {
    use salvage_core::media::{playlist, SEGMENT_SECONDS};
    let text = playlist(SEGMENT_SECONDS * 2.0 + 1.5, "secret");
    assert!(text.starts_with("#EXTM3U"));
    assert!(text.trim_end().ends_with("#EXT-X-ENDLIST"));
    assert!(text.contains("#EXTINF:1.500,"));
    assert!(text.contains("seg2.ts?t=secret"));
    assert!(!text.contains("seg3.ts"));
}

#[test]
fn conversion_only_ever_writes_to_a_pipe() {
    use salvage_core::media::{frame_command, segment_command, Probe, Tools};
    let tools = Tools {
        ffmpeg: "ffmpeg".into(),
        ffprobe: "ffprobe".into(),
        encoder: "libx264".into(),
    };
    let found = Probe {
        duration: 60.0,
        has_video: true,
        has_audio: true,
    };
    for command in [
        segment_command(&tools, "http://127.0.0.1:1/file/1", &found, 6.0, 6.0),
        frame_command(&tools, "http://127.0.0.1:1/file/1", 6.0),
    ] {
        let args: Vec<String> = command
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(args.last().map(String::as_str), Some("pipe:1"), "{args:?}");
        assert!(!args.iter().any(|arg| arg == "-y"), "{args:?}");
        // One input, and a damaged file can't make ffmpeg open anything but that port.
        let whitelist = args
            .iter()
            .position(|arg| arg == "-protocol_whitelist")
            .expect("whitelist");
        assert_eq!(args[whitelist + 1], "http,tcp");
    }
}

#[test]
fn ffmpeg_converts_a_real_video_when_it_is_installed() {
    use salvage_core::media::{frame_command, probe, segment_command, spawn_output};
    use std::io::Read;
    let Some((tools, path)) = sample_video("convert") else {
        eprintln!("ffmpeg is not installed, so this test has nothing to check");
        return;
    };
    let input = path.to_string_lossy().into_owned();
    let found = probe(&tools, &input).expect("probe");
    assert!(found.has_video && found.has_audio);
    assert!((found.duration - 8.0).abs() < 1.0, "{}", found.duration);

    let mut piece = Vec::new();
    spawn_output(segment_command(&tools, &input, &found, 6.0, 2.0))
        .expect("start")
        .read_to_end(&mut piece)
        .expect("read");
    assert!(piece.len() > 188, "{} bytes", piece.len());
    assert_eq!(piece[0], 0x47, "MPEG-TS packets start with a sync byte");

    let mut image = Vec::new();
    spawn_output(frame_command(&tools, &input, 1.0))
        .expect("start")
        .read_to_end(&mut image)
        .expect("read");
    assert_eq!(&image[..2], &[0xff, 0xd8], "a JPEG");

    assert!(probe(&tools, "/does/not/exist.avi").is_err());
    let _ = std::fs::remove_file(path);
}

/// A bare HTTP request, because the test should see exactly what the port answers.
fn local_get(port: &str, path: &str, range: Option<&str>) -> (u16, Vec<u8>) {
    use std::io::{Read, Write};
    let mut stream = std::net::TcpStream::connect(format!("127.0.0.1:{port}")).expect("connect");
    let extra = range
        .map(|text| format!("Range: {text}\r\n"))
        .unwrap_or_default();
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: localhost\r\n{extra}Connection: close\r\n\r\n"
    )
    .expect("send");
    let mut reply = Vec::new();
    stream.read_to_end(&mut reply).expect("read");
    let split = reply
        .windows(4)
        .position(|part| part == b"\r\n\r\n")
        .expect("headers");
    let head = String::from_utf8_lossy(&reply[..split]).into_owned();
    let status = head
        .split_whitespace()
        .nth(1)
        .expect("status")
        .parse()
        .expect("number");
    (status, reply[split + 4..].to_vec())
}

#[test]
fn the_media_port_needs_its_token_and_serves_exact_ranges() {
    use salvage_core::media::MediaServer;
    let (engine, session, dir) = engine_with_volume("port");
    let media = MediaServer::start(engine.clone()).expect("start");
    let info = media.info();
    let port = info.base_url.rsplit(':').next().expect("port").to_string();
    assert!(info.base_url.starts_with("http://127.0.0.1:"));
    assert_eq!(info.token.len(), 64);

    let id = call(&engine, "search", json!({"query": "track.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    let whole = session.read_entry(id, u64::MAX).expect("read").1;

    assert_eq!(local_get(&port, &format!("/file/{id}"), None).0, 403);
    assert_eq!(
        local_get(&port, &format!("/file/{id}?t=wrong"), None).0,
        403
    );

    let path = format!("/file/{id}?t={}", info.token);
    let (status, body) = local_get(&port, &path, None);
    assert_eq!(status, 200);
    assert_eq!(body, whole);
    let (status, body) = local_get(&port, &path, Some("bytes=5-14"));
    assert_eq!(status, 206);
    assert_eq!(body, whole[5..15]);
    let past = format!("bytes={}-", whole.len());
    assert_eq!(local_get(&port, &path, Some(&past)).0, 416);
    let unknown = format!("/nothing/{id}?t={}", info.token);
    assert_eq!(local_get(&port, &unknown, None).0, 404);

    let _ = std::fs::remove_dir_all(dir);
}

fn encoded_image(format: image::ImageFormat) -> Vec<u8> {
    let mut data = Vec::new();
    image::RgbImage::from_fn(64, 64, |x, y| image::Rgb([x as u8 * 3, y as u8 * 3, 90]))
        .write_to(&mut std::io::Cursor::new(&mut data), format)
        .expect("encode");
    data
}

#[test]
fn a_whole_image_passes_and_a_damaged_one_fails_with_a_reason() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED};
    let png = encoded_image(image::ImageFormat::Png);
    let good = judge("a.png", &png);
    assert_eq!(good.outcome, OUTCOME_PASSED, "{}", good.note);
    assert_eq!(good.format, "png");
    let jpeg = encoded_image(image::ImageFormat::Jpeg);
    assert_eq!(judge("a.jpg", &jpeg).outcome, OUTCOME_PASSED);

    // A wrong cluster in the middle, as a bad chain would leave.
    let mut hit = png.clone();
    let middle = hit.len() / 2;
    hit[middle..middle + 20].fill(0xaa);
    let bad = judge("a.png", &hit);
    assert_eq!(bad.outcome, OUTCOME_DAMAGED);
    assert!(bad.note.contains("Tested:"), "{}", bad.note);

    let noise = judge("a.jpg", &vec![0x5a; 4096]);
    assert_eq!(noise.outcome, OUTCOME_DAMAGED);
}

/// A JPEG's outline, by hand, so its image data can be exactly what a test needs.
fn sample_jpeg(scan: &[u8], after: &[u8]) -> Vec<u8> {
    let mut data = b"\xff\xd8\xff\xdb\x00\x04\x00\x00\xff\xda\x00\x02".to_vec();
    data.extend_from_slice(scan);
    data.extend_from_slice(b"\xff\xd9");
    data.extend_from_slice(after);
    data
}

#[test]
fn a_jpeg_walk_catches_what_a_wrong_cluster_leaves() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED};
    let image_data: Vec<u8> = (0..60_000u32).map(|n| (n % 251) as u8 + 1).collect();
    assert_eq!(
        judge("a.jpg", &sample_jpeg(&image_data, b"")).outcome,
        OUTCOME_PASSED
    );

    // A wrong cluster of other compressed data has stray markers that lead nowhere.
    let mut foreign = image_data.clone();
    foreign[30_000..30_004].copy_from_slice(b"\xff\xe1\x00\x10");
    assert_eq!(
        judge("a.jpg", &sample_jpeg(&foreign, b"")).outcome,
        OUTCOME_DAMAGED
    );

    // A cluster nothing was ever written to.
    let mut empty = image_data.clone();
    empty[20_000..30_000].fill(0);
    let hit = judge("a.jpg", &sample_jpeg(&empty, b""));
    assert_eq!(hit.outcome, OUTCOME_DAMAGED);
    assert!(hit.first_break.is_some());

    // Restart markers must count up, so a piece from elsewhere shows.
    let mut in_order = image_data.clone();
    in_order[1_000..1_002].copy_from_slice(b"\xff\xd0");
    in_order[2_000..2_002].copy_from_slice(b"\xff\xd1");
    assert_eq!(
        judge("a.jpg", &sample_jpeg(&in_order, b"")).outcome,
        OUTCOME_PASSED
    );
    let mut out_of_order = in_order.clone();
    out_of_order[2_000..2_002].copy_from_slice(b"\xff\xd5");
    assert_eq!(
        judge("a.jpg", &sample_jpeg(&out_of_order, b"")).outcome,
        OUTCOME_DAMAGED
    );

    // What real cameras add after the end is not damage.
    let padded = sample_jpeg(&image_data, &vec![0u8; 50_000]);
    assert_eq!(judge("a.jpg", &padded).outcome, OUTCOME_PASSED);
    let second = sample_jpeg(&image_data, &sample_jpeg(&image_data, b""));
    assert_eq!(judge("a.jpg", &second).outcome, OUTCOME_PASSED);

    // Real cameras also add about 145 KB of their own data, and phones a motion clip.
    let camera_data = sample_jpeg(&image_data, &vec![0x41u8; 50_000]);
    assert_eq!(judge("a.jpg", &camera_data).outcome, OUTCOME_PASSED);

    // Cut short, it fails.
    let whole = sample_jpeg(&image_data, b"");
    assert_eq!(
        judge("a.jpg", &whole[..whole.len() / 2]).outcome,
        OUTCOME_DAMAGED
    );
}

#[test]
fn image_types_with_no_walker_are_judged_by_decoding() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED};
    let gif = encoded_image(image::ImageFormat::Gif);
    assert_eq!(judge("a.gif", &gif).outcome, OUTCOME_PASSED);
    assert_eq!(
        judge("a.gif", &gif[..gif.len() / 3]).outcome,
        OUTCOME_DAMAGED
    );
}

/// A small PDF with a real cross-reference table, built with correct offsets.
fn sample_pdf() -> Vec<u8> {
    let mut data = b"%PDF-1.4\n".to_vec();
    let mut offsets = Vec::new();
    for number in 1..=3 {
        offsets.push(data.len());
        data.extend_from_slice(format!("{number} 0 obj\n<< /N {number} >>\nendobj\n").as_bytes());
    }
    let table = data.len();
    data.extend_from_slice(b"xref\n0 4\n0000000000 65535 f \n");
    for offset in offsets {
        data.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
    }
    data.extend_from_slice(
        format!("trailer\n<< /Size 4 >>\nstartxref\n{table}\n%%EOF\n").as_bytes(),
    );
    data
}

#[test]
fn a_pdf_is_checked_against_its_own_table_of_objects() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED};
    let pdf = sample_pdf();
    let good = judge("a.pdf", &pdf);
    assert_eq!(good.outcome, OUTCOME_PASSED, "{}", good.note);
    assert_eq!(good.format, "pdf");

    let mut hit = pdf.clone();
    hit[9..30].fill(b'#');
    assert_eq!(judge("a.pdf", &hit).outcome, OUTCOME_DAMAGED);
    assert_eq!(judge("a.pdf", b"not a pdf at all").outcome, OUTCOME_DAMAGED);
    assert_eq!(
        judge("a.pdf", &pdf[..pdf.len() - 30]).outcome,
        OUTCOME_DAMAGED
    );
}

#[test]
fn a_pdf_with_unused_entries_or_an_unreadable_table_is_not_called_damaged() {
    use salvage_core::filetest::{judge, OUTCOME_PASSED};
    let pdf = String::from_utf8(sample_pdf()).expect("text");
    // Some writers mark unused objects as in use at offset zero. Same length as a free
    // entry, so the table stays where the file says it is.
    let zeroed = pdf.replacen("0000000000 65535 f ", "0000000000 00000 n ", 1);
    let good = judge("a.pdf", zeroed.as_bytes());
    assert_eq!(good.outcome, OUTCOME_PASSED, "{}", good.note);

    // A table this check cannot read says nothing about the file.
    let odd = pdf.replace("xref\n0 4\n", "xref\nno table\n");
    let start = odd.find("xref").expect("table");
    let end = odd.find("trailer").expect("trailer");
    let padded = format!("{}{}", &odd[..start + 4], &odd[end..]);
    let fixed = padded.replace(
        &format!("startxref\n{}", pdf.find("xref").expect("table")),
        &format!("startxref\n{start}"),
    );
    let unread = judge("a.pdf", fixed.as_bytes());
    assert_eq!(unread.outcome, OUTCOME_PASSED, "{}", unread.note);
}

#[test]
fn a_file_is_judged_by_its_contents_before_its_name() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED, OUTCOME_UNTESTED};
    // A macOS alias named as a picture was found on a real disk. It is not damaged.
    let alias = judge("holiday.png", b"book\0\0\0\0mark\0\0\0\0 target");
    assert_eq!(alias.outcome, OUTCOME_UNTESTED);
    assert_eq!(alias.format, "alias");
    let companion = judge("._holiday.png", b"\x00\x05\x16\x07\x00\x02\x00\x00Mac OS X");
    assert_eq!(companion.outcome, OUTCOME_UNTESTED);
    assert_eq!(companion.format, "companion");

    // A renamed file is judged as what it really is.
    let renamed = judge("a.png", &encoded_image(image::ImageFormat::Jpeg));
    assert_eq!(renamed.outcome, OUTCOME_PASSED, "{}", renamed.note);
    assert_eq!(renamed.format, "jpeg");

    // Several older formats use .doc, one was plain text on a real disk, and none of that is damage.
    assert_eq!(
        judge("old.doc", b"This document was moved").outcome,
        OUTCOME_UNTESTED
    );
    // A name that promises a checkable format, over bytes that are nothing like it, is.
    assert_eq!(
        judge("a.png", b"not a picture at all").outcome,
        OUTCOME_DAMAGED
    );

    // A .key is a Keynote document or a key in text form, and only the bytes can tell.
    assert_eq!(
        judge("server.key", b"-----BEGIN SAMPLE-----\nabc\n").outcome,
        OUTCOME_UNTESTED
    );
    assert_eq!(
        judge("slides.key", b"PK\x03\x04 and nothing after it").outcome,
        OUTCOME_DAMAGED
    );
}

const SAMPLE_DISK_INFO: &str = "
   Device Identifier:         disk12s2
   Device Node:               /dev/disk12s2
   Whole:                     No
   Part of Whole:             disk12

   Volume Name:               Untitled
   Mounted:                   No

   File System Personality:   ExFAT

   Disk Size:                 590.7 GB (590744649728 Bytes) (exactly 1153798144 512-Byte-Units)
   Device Location:           External
   Volume UUID:               52D6C729-7E3F-310C-9073-7142E9EA7C05
";

#[test]
fn the_start_screen_reads_what_diskutil_says() {
    use salvage_core::startup::{parse_partitions, parse_volume};
    let list = "/dev/disk12 (external, physical):
   #:                       TYPE NAME                    SIZE       IDENTIFIER
   0:     FDisk_partition_scheme                        *2.0 TB     disk12
   1:               Windows_NTFS seagate_2tb             1.4 TB     disk12s1
   2:               Windows_NTFS Untitled                590.7 GB   disk12s2
";
    assert_eq!(parse_partitions(list), ["disk12s1", "disk12s2"]);

    let volume = parse_volume(SAMPLE_DISK_INFO).expect("volume");
    assert_eq!(volume.device, "/dev/rdisk12s2");
    assert_eq!(volume.whole_disk, "/dev/rdisk12");
    assert_eq!(volume.name, "Untitled");
    assert_eq!(volume.bytes, 590_744_649_728);
    assert_eq!(volume.format, "ExFAT");
    assert!(volume.external && volume.can_open);
    assert_eq!(volume.mount_point, None);

    let mounted = SAMPLE_DISK_INFO.replace(
        "Mounted:                   No",
        "Mounted:                   Yes\n   Mount Point:               /Volumes/Untitled",
    );
    assert_eq!(
        parse_volume(&mounted)
            .expect("volume")
            .mount_point
            .as_deref(),
        Some("/Volumes/Untitled")
    );

    // Another format can be seen but not opened.
    let ntfs = SAMPLE_DISK_INFO.replace("ExFAT", "NTFS");
    assert!(!parse_volume(&ntfs).expect("volume").can_open);
    // A damaged volume may show no format at all, and that is exactly what this app is for.
    let unknown = SAMPLE_DISK_INFO.replace(
        "File System Personality:   ExFAT",
        "File System Personality:   Not applicable (no file system)",
    );
    let unknown = parse_volume(&unknown).expect("volume");
    assert_eq!(unknown.format, "Unknown");
    assert!(unknown.can_open);

    assert!(parse_volume("Could not find disk: x").is_none());
    let efi = SAMPLE_DISK_INFO.replace("Untitled", "EFI");
    assert!(parse_volume(&efi).is_none());
    // An APFS container has no format of its own to show, and would otherwise look openable.
    let container = format!("{SAMPLE_DISK_INFO}   Partition Type:            Apple_APFS\n");
    assert!(parse_volume(&container).is_none());
    // The type a damaged volume on this disk really has must still come through.
    let windows = format!("{SAMPLE_DISK_INFO}   Partition Type:            Windows_NTFS\n");
    assert!(parse_volume(&windows).is_some());
}

#[test]
fn the_start_screen_offers_the_right_protection_for_each_volume() {
    use salvage_core::startup::{is_protected, parse_disk_sleep, parse_volume};
    let uuid = "52D6C729-7E3F-310C-9073-7142E9EA7C05";
    let line = format!("UUID={uuid} none exfat rw,noauto");
    assert_eq!(
        parse_volume(SAMPLE_DISK_INFO)
            .expect("volume")
            .fstab_line
            .as_deref(),
        Some(line.as_str())
    );
    let ntfs = parse_volume(&SAMPLE_DISK_INFO.replace("ExFAT", "NTFS")).expect("volume");
    assert!(ntfs
        .fstab_line
        .expect("line")
        .contains(" none ntfs rw,noauto"));
    // With no known format there is no honest line to offer.
    let unknown = SAMPLE_DISK_INFO.replace(
        "File System Personality:   ExFAT",
        "File System Personality:   Not applicable (no file system)",
    );
    assert_eq!(parse_volume(&unknown).expect("volume").fstab_line, None);

    assert!(is_protected(&format!("# notes\n{line}\n"), uuid));
    assert!(!is_protected("", uuid));
    // The commented line in a real fstab does nothing, and this one still mounts.
    assert!(!is_protected(&format!("#{line}\n"), uuid));
    assert!(!is_protected(
        &format!("UUID={uuid} none exfat rw,auto,nobrowse\n"),
        uuid
    ));
    // Another volume's line is not this one's.
    assert!(!is_protected(&line.replace("52D6C729", "AAAAAAAA"), uuid));

    assert_eq!(
        parse_disk_sleep(" standby 1\n disksleep            10\n sleep 1\n"),
        Some(10)
    );
    assert_eq!(parse_disk_sleep(" disksleep 0\n"), Some(0));
    assert_eq!(parse_disk_sleep("nothing here\n"), None);
}

/// Disk numbers change on every attach, so a volume must be known by its UUID alone.
#[test]
fn a_volume_is_recognised_by_uuid_under_any_disk_number() {
    use salvage_core::startup::{
        join_known, known_volumes, parse_volume, write_record, VolumeRecord,
    };
    let root = std::env::temp_dir().join(format!("salvage-known-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (workspace, _) = (
        Workspace::at(root.join("vol-db8aa101")).expect("workspace"),
        (),
    );
    write_record(
        workspace.dir(),
        &VolumeRecord {
            uuid: Some("52D6C729-7E3F-310C-9073-7142E9EA7C05".into()),
            name: "Untitled".into(),
            bytes: 590_744_649_728,
            last_opened_millis: 1000,
        },
    );
    // A folder with no record is not listed, and neither is a stray file.
    Workspace::at(root.join("vol-00000000")).expect("workspace");
    std::fs::write(root.join("last-device"), "/dev/rdisk12s2").expect("write");

    let known = known_volumes(&root);
    assert_eq!(known.len(), 1);
    assert!(!known[0].scanned);

    let renumbered = SAMPLE_DISK_INFO.replace("disk12", "disk19");
    let report = join_known(
        vec![parse_volume(&renumbered).expect("volume")],
        known.clone(),
    );
    assert_eq!(report.volumes[0].device, "/dev/rdisk19s2");
    assert_eq!(report.volumes[0].known.as_ref(), Some(&known[0]));
    assert!(report.elsewhere.is_empty());

    // What is recognised comes first, then what can be opened, whatever the disk numbers.
    let ntfs = parse_volume(
        &SAMPLE_DISK_INFO
            .replace("ExFAT", "NTFS")
            .replace("52D6C729", "BBBBBBBB"),
    )
    .expect("volume");
    let fresh = parse_volume(&SAMPLE_DISK_INFO.replace("52D6C729", "CCCCCCCC")).expect("volume");
    let ours = parse_volume(&renumbered).expect("volume");
    let order = join_known(vec![ntfs, fresh, ours], known.clone());
    let names: Vec<_> = order
        .volumes
        .iter()
        .map(|v| (v.known.is_some(), v.can_open))
        .collect();
    assert_eq!(names, [(true, true), (false, true), (false, false)]);

    // A different volume on the same disk number is not mistaken for it.
    let other = SAMPLE_DISK_INFO.replace("52D6C729", "AAAAAAAA");
    let report = join_known(vec![parse_volume(&other).expect("volume")], known.clone());
    assert!(report.volumes[0].known.is_none());
    assert_eq!(report.elsewhere.len(), 1);

    // Nor does a volume with no UUID match a record with none.
    let no_uuid = SAMPLE_DISK_INFO.replace(
        "   Volume UUID:               52D6C729-7E3F-310C-9073-7142E9EA7C05\n",
        "",
    );
    let mut blank = known[0].clone();
    blank.record.uuid = None;
    let report = join_known(vec![parse_volume(&no_uuid).expect("volume")], vec![blank]);
    assert!(report.volumes[0].known.is_none());
    let _ = std::fs::remove_dir_all(root);
}

/// A scanned volume whose disk can be pulled out from under it.
fn engine_with_unpluggable_disk(
    label: &str,
) -> (Arc<Engine>, Arc<Session>, Arc<MemorySource>, PathBuf) {
    let (workspace, dir) = temp_workspace(label);
    let source = Arc::new(MemorySource::new(build_volume()));
    let session =
        Session::with_source(Path::new("/test/image"), source.clone(), workspace).expect("session");
    scan(&session.volume, &session.workspace);
    let engine = Engine::new();
    engine.set_session(session.clone());
    (engine, session, source, dir)
}

fn id_named(engine: &Engine, name: &str) -> i64 {
    call(engine, "search", json!({"query": name}))["rows"][0]["id"]
        .as_i64()
        .expect("id")
}

/// Found on a real disk: it dropped off USB, and every file then failed to open with
/// nothing to say why, while reads returned zeros and the status looked like damage.
#[test]
fn a_disconnected_disk_is_reported_and_files_are_not_read_as_zeros() {
    let (engine, session, source, dir) = engine_with_unpluggable_disk("gone-read");
    let id = id_named(&engine, "keep.mp3");
    assert_eq!(
        &session.read_entry_range(id, 0, 4).unwrap().data,
        b"ID3\x04"
    );
    assert_eq!(call(&engine, "status", json!({}))["diskGone"], false);

    source.disconnect();
    let error = session
        .read_entry_range(id, 0, 4)
        .err()
        .expect("an error, not zeros");
    assert!(format!("{error:#}").contains("disconnected"), "{error:#}");
    let status = call(&engine, "status", json!({}));
    assert_eq!(status["diskGone"], true);
    assert!(status["diskBack"].is_null());
    let _ = std::fs::remove_dir_all(dir);
}

/// Found by unplugging a real device in a test: a disk can come back with the same number.
#[test]
fn opening_a_disconnected_disk_again_does_not_return_the_dead_session() {
    let (engine, session, source, dir) = engine_with_unpluggable_disk("gone-reopen");
    let path = session.path.display().to_string();
    assert!(Arc::ptr_eq(&engine.open(&path).expect("open"), &session));
    source.disconnect();
    // This path is not a real file, so a fresh open fails, which proves one was attempted.
    assert!(engine.open(&path).is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn file_tests_can_be_narrowed_and_a_stream_test_summarised() {
    let (engine, session, dir) = engine_with_volume("narrow-tests");
    let photo = id_named(&engine, "photo.jpg");
    assert!(
        engine
            .call("file_tests_start", &json!({"groups": []}), AUTHOR_CLAUDE)
            .is_err(),
        "something must be chosen"
    );

    let started = call(&engine, "file_tests_start", json!({"ids": [photo]}));
    assert_eq!(started["total"], 1, "only the one entry asked for");
    let status = wait_for_file_tests(&session);
    assert_eq!(status.state, "finished");
    assert!(!call(&engine, "file_test", json!({"id": photo})).is_null());

    let music = call(
        &engine,
        "file_tests_start",
        json!({"extensions": ["mp3"], "verdicts": ["likely-intact"], "minBytes": 1}),
    );
    assert!(music["total"].as_u64().expect("total") >= 1);
    wait_for_file_tests(&session);

    let track = id_named(&engine, "track.mp3");
    let full = call(&engine, "test_stream", json!({"entryId": track}));
    assert!(full.get("extents").is_some());
    let short = call(
        &engine,
        "test_stream",
        json!({"entryId": track, "summary": true}),
    );
    assert!(short.get("extents").is_none() && short.get("profile").is_none());
    assert_eq!(short["report"], full["report"]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn test_results_can_be_forgotten_for_some_formats_only() {
    let (engine, session, dir) = engine_with_volume("forget-formats");
    let photo = id_named(&engine, "photo.jpg");
    let track = id_named(&engine, "track.mp3");
    call(&engine, "file_tests_start", json!({"ids": [photo, track]}));
    wait_for_file_tests(&session);
    let format = |id: i64| call(&engine, "file_test", json!({"id": id}))["format"].clone();
    assert_eq!(format(photo), "jpeg");
    assert_eq!(format(track), "mp3");
    let forgotten = call(&engine, "file_tests_forget", json!({"formats": ["mp3"]}));
    assert_eq!(forgotten["forgotten"], 1);
    assert!(call(&engine, "file_test", json!({"id": track})).is_null());
    assert_eq!(format(photo), "jpeg", "other formats keep their results");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_disk_that_goes_during_file_tests_marks_nothing_as_damaged() {
    let (engine, session, source, dir) = engine_with_unpluggable_disk("gone-tests");
    let id = id_named(&engine, "photo.jpg");
    let before = call(&engine, "entry", json!({"id": id}))["entry"]["verdict"].clone();

    source.disconnect();
    call(&engine, "file_tests_start", json!({"groups": ["photos"]}));
    let status = wait_for_file_tests(&session);
    assert_eq!(status.state, "failed");
    assert!(status.error.unwrap_or_default().contains("disconnected"));
    assert_eq!(status.damaged, 0);
    assert!(call(&engine, "file_test", json!({"id": id})).is_null());
    assert_eq!(
        call(&engine, "entry", json!({"id": id}))["entry"]["verdict"],
        before
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_disk_that_goes_during_a_scan_leaves_the_map_alone() {
    let (workspace, dir) = temp_workspace("gone-scan");
    let source = Arc::new(MemorySource::new(build_volume()));
    let session =
        Session::with_source(Path::new("/test/image"), source.clone(), workspace).expect("session");
    source.disconnect();
    let stop = AtomicBool::new(false);
    let error = run_scan(
        &session.volume,
        &session.workspace,
        ScanOptions { chunk_clusters: 16 },
        &stop,
        |_| {},
    )
    .expect_err("the scan stops");
    assert!(format!("{error:#}").contains("disconnected"), "{error:#}");
    let database = MapDb::open(&session.workspace.database()).expect("map");
    let progress = salvage_core::scan::read_progress(&database, &session.volume).expect("progress");
    assert_eq!(progress.read_errors, 0);
    assert_eq!(progress.clusters_scanned, 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_disk_that_goes_during_a_copy_stops_it_and_leaves_no_file_of_zeros() {
    let (engine, session, source, dir) = engine_with_unpluggable_disk("gone-copy");
    let destination = copy_destination("gone");
    let request = json!({"destination": destination, "folders": [20]});

    // The planning a copy does first is allowed through, and the disk goes as it starts.
    let before = source.read_stats().operations;
    call(&engine, "copy_plan", request.clone());
    source.disconnect_after_reads(source.read_stats().operations - before);
    let status = copy_and_wait(&engine, &session, request.clone());
    assert_eq!(status.state, "failed", "{status:?}");
    assert!(status.error.unwrap_or_default().contains("disconnected"));
    assert_eq!(status.files_done, 0);
    let copied: Vec<_> = walk_files(&destination)
        .into_iter()
        .filter(|path| {
            path.file_name()
                .is_some_and(|name| name != salvage_core::export::MANIFEST_NAME)
        })
        .collect();
    assert!(copied.is_empty(), "{copied:?}");

    // A new copy is refused with the real reason, not "nothing to copy".
    let refused = engine
        .call("copy_start", &request, "you")
        .expect_err("refused");
    assert!(
        format!("{refused:#}").contains("disconnected"),
        "{refused:#}"
    );
    let _ = std::fs::remove_dir_all(destination);
    let _ = std::fs::remove_dir_all(dir);
}

fn walk_files(root: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(root) else {
        return Vec::new();
    };
    entries
        .flatten()
        .flat_map(|entry| {
            let path = entry.path();
            if path.is_dir() {
                walk_files(&path)
            } else {
                vec![path]
            }
        })
        .collect()
}

/// caffeinate is macOS's own, so this runs only there.
#[cfg(target_os = "macos")]
#[test]
fn disks_are_kept_awake_once_and_only_when_asked() {
    use salvage_core::mount::{disks_kept_awake, keep_disks_awake};
    keep_disks_awake();
    assert!(disks_kept_awake());
    // A second call must not leave a second process behind.
    keep_disks_awake();
    assert!(disks_kept_awake());
}

#[test]
fn a_test_disconnect_is_refused_outside_a_test_run() {
    let (engine, _session, dir) = engine_with_volume("no-test-tool");
    assert!(!tool_definitions()
        .iter()
        .any(|tool| tool.to_string().contains("test_disconnect")));
    if std::env::var_os("SALVAGE_TEST_UI").is_none() {
        assert!(engine
            .call("test_disconnect", &json!({}), AUTHOR_CLAUDE)
            .is_err());
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn only_a_lost_device_counts_as_gone() {
    use salvage_core::source::means_device_gone;
    assert!(means_device_gone(&std::io::Error::from_raw_os_error(
        libc::ENXIO
    )));
    assert!(means_device_gone(&std::io::Error::from_raw_os_error(
        libc::ENODEV
    )));
    // A bad spot on a disk that is still there must keep reading the rest of the file.
    assert!(!means_device_gone(&std::io::Error::from_raw_os_error(
        libc::EIO
    )));
    assert!(!means_device_gone(&std::io::Error::from(
        std::io::ErrorKind::UnexpectedEof
    )));

    let source = MemorySource::new(vec![0u8; 512]);
    let mut past_the_end = [0u8; 512];
    assert!(source.read_exact_at(4096, &mut past_the_end).is_err());
    assert!(!source.is_gone());
}

#[test]
fn a_volume_is_found_again_under_a_new_disk_number() {
    use salvage_core::mount::{parse_disk_return, parse_volume_uuid, DiskReturn};
    // As diskutil printed it for a real volume, unmounted and then mounted.
    let unmounted = "
   Device Identifier:         disk14s2
   Device Node:               /dev/disk14s2
   Whole:                     No
   Part of Whole:             disk14

   Volume Name:               Untitled
   Mounted:                   No

   File System Personality:   ExFAT
   Volume UUID:               52D6C729-7E3F-310C-9073-7142E9EA7C05
";
    assert_eq!(
        parse_volume_uuid(unmounted).as_deref(),
        Some("52D6C729-7E3F-310C-9073-7142E9EA7C05")
    );
    assert_eq!(
        parse_disk_return(unmounted),
        Some(DiskReturn {
            device: "/dev/rdisk14s2".into(),
            mounted: false
        })
    );
    let mounted = unmounted.replace(
        "Mounted:                   No",
        "Mounted:                   Yes",
    );
    assert!(parse_disk_return(&mounted).expect("found").mounted);

    // What diskutil prints for something it cannot find.
    let missing = "Could not find disk: 00000000-0000-0000-0000-000000000000\n";
    assert_eq!(parse_disk_return(missing), None);
    assert_eq!(parse_volume_uuid(missing), None);
    assert_eq!(
        parse_disk_return("   Device Identifier:         \n   Device Node:               \n"),
        None
    );
}

/// One file for a hand-built zip: its local header and data, and its directory record.
fn zip_file(name: &str, content: &[u8], deflate: bool, offset: u32) -> (Vec<u8>, Vec<u8>) {
    let mut crc = flate2::Crc::new();
    crc.update(content);
    let body = if deflate {
        use std::io::Write;
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(content).unwrap();
        encoder.finish().unwrap()
    } else {
        content.to_vec()
    };
    let method: u16 = if deflate { 8 } else { 0 };
    let date: u16 = (42 << 9) | (9 << 5) | 5;
    let time: u16 = (17 << 11) | (53 << 5) | 13;
    let mut shared = Vec::new();
    shared.extend_from_slice(&0u16.to_le_bytes());
    shared.extend_from_slice(&method.to_le_bytes());
    shared.extend_from_slice(&time.to_le_bytes());
    shared.extend_from_slice(&date.to_le_bytes());
    shared.extend_from_slice(&crc.sum().to_le_bytes());
    shared.extend_from_slice(&(body.len() as u32).to_le_bytes());
    shared.extend_from_slice(&(content.len() as u32).to_le_bytes());
    shared.extend_from_slice(&(name.len() as u16).to_le_bytes());
    shared.extend_from_slice(&0u16.to_le_bytes());

    let mut local = b"PK\x03\x04\x14\x00".to_vec();
    local.extend_from_slice(&shared);
    local.extend_from_slice(name.as_bytes());
    local.extend_from_slice(&body);

    let mut central = b"PK\x01\x02\x14\x00\x14\x00".to_vec();
    central.extend_from_slice(&shared);
    central.extend_from_slice(&[0u8; 10]);
    central.extend_from_slice(&offset.to_le_bytes());
    central.extend_from_slice(name.as_bytes());
    (local, central)
}

fn hand_built_zip(files: &[(&str, &[u8], bool)]) -> Vec<u8> {
    let (mut zip, mut directory) = (Vec::new(), Vec::new());
    for (name, content, deflate) in files {
        let (local, central) = zip_file(name, content, *deflate, zip.len() as u32);
        zip.extend_from_slice(&local);
        directory.extend_from_slice(&central);
    }
    let start = zip.len() as u32;
    zip.extend_from_slice(&directory);
    zip.extend_from_slice(b"PK\x05\x06\0\0\0\0");
    zip.extend_from_slice(&(files.len() as u16).to_le_bytes());
    zip.extend_from_slice(&(files.len() as u16).to_le_bytes());
    zip.extend_from_slice(&(directory.len() as u32).to_le_bytes());
    zip.extend_from_slice(&start.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip
}

fn sample_zip() -> Vec<u8> {
    hand_built_zip(&[
        ("docs/", b"", false),
        ("docs/notes.txt", b"plain words kept as they are", false),
        ("docs/deep/report.txt", &[b'r'; 5000], true),
        ("top.bin", &[7u8; 300], true),
    ])
}

fn zip_reader(data: &[u8]) -> impl Fn(u64, u64) -> anyhow::Result<Vec<u8>> + '_ {
    move |start, length| {
        let from = (start as usize).min(data.len());
        let to = start.saturating_add(length).min(data.len() as u64) as usize;
        Ok(data[from..to].to_vec())
    }
}

#[test]
fn a_zip_lists_what_its_directory_says() {
    use salvage_core::archive::{list_zip, SOURCE_DIRECTORY};
    let zip = sample_zip();
    let listing = list_zip(zip.len() as u64, &zip_reader(&zip)).unwrap();
    assert_eq!(listing.source, SOURCE_DIRECTORY);
    assert!(listing.complete);
    assert_eq!(listing.note, None);
    let paths: Vec<_> = listing.members.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(
        paths,
        ["docs/", "docs/notes.txt", "docs/deep/report.txt", "top.bin"]
    );
    assert!(listing.members[0].is_dir);
    let report = &listing.members[2];
    assert_eq!(report.size, Some(5000));
    assert!(report.packed.unwrap() < 5000);
    assert_eq!(report.method, 8);
    assert_eq!(report.modified.as_deref(), Some("2022-09-05T17:53:26"));

    // Bytes in front of a zip, as a self-extractor has, move every offset it records.
    let mut prefixed = vec![0x41u8; 777];
    prefixed.extend_from_slice(&zip);
    let listing = list_zip(prefixed.len() as u64, &zip_reader(&prefixed)).unwrap();
    assert!(listing.complete);
    assert_eq!(listing.members[1].offset, zip_offset(&zip, 1) + 777);
}

fn zip_offset(zip: &[u8], index: usize) -> u64 {
    use salvage_core::archive::list_zip;
    list_zip(zip.len() as u64, &zip_reader(zip))
        .unwrap()
        .members[index]
        .offset
}

#[test]
fn a_zip_with_no_directory_is_listed_from_its_file_headers() {
    use salvage_core::archive::{list_zip, SOURCE_HEADERS};
    let zip = sample_zip();
    let directory_at = zip.windows(4).position(|w| w == b"PK\x01\x02").unwrap();

    // Cut off before its directory, as a lost last cluster would leave it.
    let cut = &zip[..directory_at];
    let listing = list_zip(cut.len() as u64, &zip_reader(cut)).unwrap();
    assert_eq!(listing.source, SOURCE_HEADERS);
    assert!(!listing.complete);
    assert_eq!(listing.members.len(), 4);
    assert_eq!(listing.members[2].path, "docs/deep/report.txt");
    assert_eq!(listing.members[2].size, Some(5000));
    assert!(listing.note.as_deref().unwrap().contains("rebuilt"));

    // A zip stored inside a zip stays one file, because a known size jumps over its data.
    // The end record still in view belongs to the inner zip, and must not be taken for this one.
    let inner = sample_zip();
    let mut outer = hand_built_zip(&[("inner.zip", &inner, false), ("after.txt", b"after", false)]);
    let end = outer.len() - 22;
    let directory = u32::from_le_bytes(outer[end + 16..end + 20].try_into().unwrap());
    outer.truncate(directory as usize);
    let listing = list_zip(outer.len() as u64, &zip_reader(&outer)).unwrap();
    let paths: Vec<_> = listing.members.iter().map(|m| m.path.as_str()).collect();
    assert_eq!(paths, ["inner.zip", "after.txt"]);

    // A stray signature with no believable header around it is not a file.
    let mut noise = vec![0u8; 500];
    noise[100..104].copy_from_slice(b"PK\x03\x04");
    let listing = list_zip(noise.len() as u64, &zip_reader(&noise)).unwrap();
    assert!(listing.members.is_empty());
    assert!(listing
        .note
        .as_deref()
        .unwrap()
        .contains("looks like a zip"));
}

#[test]
fn a_zip_with_a_broken_directory_keeps_the_longer_list() {
    use salvage_core::archive::{list_zip, SOURCE_DIRECTORY, SOURCE_HEADERS};
    let zip = sample_zip();
    let directory_at = zip.windows(4).position(|w| w == b"PK\x01\x02").unwrap();
    let second = zip[directory_at + 4..]
        .windows(4)
        .position(|w| w == b"PK\x01\x02")
        .unwrap()
        + directory_at
        + 4;

    // The directory breaks after one record, and the headers still find all four.
    let mut broken = zip.clone();
    broken[second] = 0;
    let listing = list_zip(broken.len() as u64, &zip_reader(&broken)).unwrap();
    assert_eq!(listing.source, SOURCE_HEADERS);
    assert_eq!(listing.members.len(), 4);

    // With the headers gone too, the part of the directory that survives is what is left.
    let local_second = zip_offset(&zip, 1) as usize;
    let mut headers_gone = broken.clone();
    headers_gone[..local_second + 30].fill(0);
    headers_gone[local_second + 30..directory_at].fill(0);
    let listing = list_zip(headers_gone.len() as u64, &zip_reader(&headers_gone)).unwrap();
    assert_eq!(listing.source, SOURCE_DIRECTORY);
    assert!(!listing.complete);
    assert_eq!(listing.members.len(), 1);
}

#[test]
fn a_zip_member_is_checked_against_its_checksum() {
    use salvage_core::archive::{
        check_member, list_zip, CHECK_DAMAGED, CHECK_INTACT, CHECK_UNCHECKED,
    };
    let zip = sample_zip();
    let total = zip.len() as u64;
    let listing = list_zip(total, &zip_reader(&zip)).unwrap();
    let check = |data: &[u8], index: usize| {
        check_member(
            &listing.members[index],
            data.len() as u64,
            &zip_reader(data),
        )
        .unwrap()
    };
    assert_eq!(check(&zip, 0).status, CHECK_UNCHECKED);
    assert_eq!(check(&zip, 1).status, CHECK_INTACT);
    assert_eq!(check(&zip, 2).status, CHECK_INTACT);
    assert_eq!(check(&zip, 3).status, CHECK_INTACT);

    // One changed byte in a stored file.
    let mut stored = zip.clone();
    let at = zip.windows(5).position(|w| w == b"plain").unwrap();
    stored[at] = b'P';
    let hit = check(&stored, 1);
    assert_eq!(hit.status, CHECK_DAMAGED);
    assert!(hit.note.contains("checksum"), "{}", hit.note);

    // Other data over a compressed file, as a reused cluster leaves it.
    let start = listing.members[2].offset as usize + 30 + "docs/deep/report.txt".len();
    let mut compressed = zip.clone();
    compressed[start..start + 8].fill(0xFF);
    assert_eq!(check(&compressed, 2).status, CHECK_DAMAGED);
    // The file beside it is not touched.
    assert_eq!(check(&compressed, 3).status, CHECK_INTACT);

    // Its header overwritten.
    let mut no_header = zip.clone();
    no_header[listing.members[3].offset as usize] = 0;
    let hit = check(&no_header, 3);
    assert_eq!(hit.status, CHECK_DAMAGED);
    assert!(hit.note.contains("header"), "{}", hit.note);

    // The zip cut short before the file's end.
    let short = &zip[..start + 2];
    let hit = check(short, 2);
    assert_eq!(hit.status, CHECK_DAMAGED);

    // A method this app cannot unpack is not called damage.
    let mut other = listing.members[1].clone();
    other.method = 12;
    assert_eq!(
        check_member(&other, total, &zip_reader(&zip))
            .unwrap()
            .status,
        CHECK_UNCHECKED
    );
    let mut locked = listing.members[1].clone();
    locked.encrypted = true;
    assert_eq!(
        check_member(&locked, total, &zip_reader(&zip))
            .unwrap()
            .status,
        CHECK_UNCHECKED
    );
}

#[test]
fn a_zip_on_the_disk_can_be_listed_and_checked_through_the_tools() {
    let zip = sample_zip();
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 20] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
            entry_set("bundle.zip", false, 20, zip.len() as u64, true, true),
        ],
    );
    b.put(20, 0, &zip);
    let (engine, _session, dir) = engine_with_image("zip", b.image);
    let found = call(&engine, "search", json!({"query": "bundle.zip"}));
    let id = found["rows"][0]["id"].as_i64().unwrap();

    let listing = call(&engine, "zip_contents", json!({"id": id}));
    assert_eq!(listing["source"], "directory");
    assert_eq!(listing["members"].as_array().unwrap().len(), 4);
    assert_eq!(listing["members"][2]["path"], "docs/deep/report.txt");
    assert_eq!(listing["members"][2]["size"], 5000);

    let check = call(&engine, "zip_member_check", json!({"id": id, "index": 2}));
    assert_eq!(check["status"], "intact");
    assert!(engine
        .call(
            "zip_member_check",
            &json!({"id": id, "index": 99}),
            AUTHOR_CLAUDE
        )
        .is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_file_inside_a_zip_can_be_shown_and_copied_out() {
    let zip = sample_zip();
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 20] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
            entry_set("bundle.zip", false, 20, zip.len() as u64, true, true),
        ],
    );
    b.put(20, 0, &zip);
    let (engine, _session, dir) = engine_with_image("zip-out", b.image);
    let id = call(&engine, "search", json!({"query": "bundle.zip"}))["rows"][0]["id"]
        .as_i64()
        .unwrap();

    let text = call(&engine, "zip_member_preview", json!({"id": id, "index": 1}));
    assert_eq!(text["kind"], "text");
    assert_eq!(text["text"], "plain words kept as they are");
    assert_eq!(text["check"]["status"], "intact");
    let other = call(&engine, "zip_member_preview", json!({"id": id, "index": 3}));
    assert_eq!(other["kind"], "none");

    let out = std::env::temp_dir().join(format!("salvage-zip-out-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&out);
    std::fs::create_dir_all(&out).unwrap();
    let args = json!({"id": id, "index": 2, "destination": out});
    let refused = engine
        .call("zip_member_copy", &args, AUTHOR_CLAUDE)
        .expect_err("an agent must not copy");
    assert!(refused.to_string().contains("Only the person"), "{refused}");
    let first = engine.call("zip_member_copy", &args, AUTHOR_USER).unwrap();
    assert_eq!(first["check"]["status"], "intact");
    assert_eq!(first["bytes"], 5000);
    let path = first["path"].as_str().unwrap().to_string();
    assert!(path.ends_with("/report.txt"), "{path}");
    assert_eq!(std::fs::read(&path).unwrap(), vec![b'r'; 5000]);
    // A second copy never overwrites the first.
    let second = engine.call("zip_member_copy", &args, AUTHOR_USER).unwrap();
    assert!(second["path"]
        .as_str()
        .unwrap()
        .ends_with("/report (2).txt"));
    assert_eq!(second["sha256"], first["sha256"]);
    let left: Vec<_> = std::fs::read_dir(&out)
        .unwrap()
        .map(|item| item.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert_eq!(left.len(), 2, "no partial file is left behind: {left:?}");
    let missing = json!({"id": id, "index": 2, "destination": out.join("not-there")});
    assert!(engine
        .call("zip_member_copy", &missing, AUTHOR_USER)
        .is_err());
    let _ = std::fs::remove_dir_all(&out);
    let _ = std::fs::remove_dir_all(dir);
}

/// A volume with the two folders macOS keeps at the top, and one more that only looks alike.
fn volume_macos_has_visited() -> Vec<u8> {
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4, 5, 6, 7, 8, 9] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    let cluster = CLUSTER as u64;
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
            modified_on(
                entry_set(".fseventsd", true, 5, cluster, true, true),
                2026,
                11,
                4,
            ),
            modified_on(
                entry_set(".Spotlight-V100", true, 6, cluster, true, true),
                2022,
                9,
                8,
            ),
            entry_set("holiday", true, 7, cluster, true, true),
        ],
    );
    b.directory(
        5,
        &[
            modified_on(entry_set("0001", false, 20, 100, true, true), 2026, 11, 4),
            modified_on(
                entry_set("0002", false, 21, cluster + 1, true, true),
                2025,
                5,
                2,
            ),
        ],
    );
    b.directory(
        6,
        &[modified_on(
            entry_set("Store", true, 8, cluster, true, true),
            2022,
            9,
            8,
        )],
    );
    b.directory(
        8,
        &[modified_on(
            entry_set("live.0", false, 23, 200, true, true),
            2025,
            5,
            2,
        )],
    );
    // A person's own file, changed on a mount date, was not written by the mount.
    b.directory(
        7,
        &[
            modified_on(
                entry_set("photo.jpg", false, 24, 300, true, true),
                2026,
                11,
                4,
            ),
            modified_on(
                entry_set(".fseventsd", true, 9, cluster, true, true),
                2019,
                1,
                1,
            ),
        ],
    );
    // A copy of that folder from another disk says nothing about this one.
    b.directory(
        9,
        &[modified_on(
            entry_set("0003", false, 25, 400, true, true),
            2019,
            1,
            1,
        )],
    );
    b.image
}

/// Writes the volume macOS has visited to `SALVAGE_MACOS_IMAGE`, for testing the window.
/// Run with: SALVAGE_MACOS_IMAGE=/path/macos.img cargo test -p salvage-core --test synthetic write_the_macos_visited
#[test]
fn write_the_macos_visited_volume_for_window_tests() {
    verify_window_fixture("macos", "SALVAGE_MACOS_IMAGE", volume_macos_has_visited());
}

#[test]
fn the_folders_macos_keeps_on_a_volume_date_its_visits() {
    let (engine, _session, dir) = engine_with_image("mounts", volume_macos_has_visited());
    let history = call(&engine, "mount_history", json!({}));
    let dates: Vec<_> = history["dates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|day| {
            (
                day["date"].as_str().unwrap(),
                day["entries"].as_u64().unwrap(),
                day["bytes"].as_u64().unwrap(),
            )
        })
        .collect();
    let cluster = CLUSTER as u64;
    assert_eq!(
        dates,
        [
            ("2022-09-08", 2, 2 * cluster),
            ("2025-05-02", 2, cluster + 201),
            ("2026-11-04", 2, cluster + 100),
        ]
    );
    assert_eq!(history["dirtyFlag"], false);
    assert!(history["baselineRecordedAtMillis"].as_u64().unwrap() > 0);

    // Where those writes landed, as cluster runs per date.
    let writes = call(&engine, "macos_writes", json!({}));
    let runs: Vec<_> = writes["dates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|day| {
            (
                day["date"].as_str().unwrap().to_string(),
                day["extents"].clone(),
            )
        })
        .collect();
    assert_eq!(
        runs,
        [
            (
                "2022-09-08".to_string(),
                json!([{"first": 6, "count": 1}, {"first": 8, "count": 1}])
            ),
            ("2025-05-02".to_string(), json!([{"first": 21, "count": 3}])),
            (
                "2026-11-04".to_string(),
                json!([{"first": 5, "count": 1}, {"first": 20, "count": 1}])
            ),
        ]
    );
    // A file in those clusters says so; the person's own file beside them does not.
    let wrote_on = |name: &str| {
        let id = call(&engine, "search", json!({"query": name}))["rows"][0]["id"]
            .as_i64()
            .unwrap();
        call(&engine, "entry", json!({"id": id}))["macosWroteOn"].clone()
    };
    assert_eq!(wrote_on("0001"), json!(["2026-11-04"]));
    assert_eq!(wrote_on("photo.jpg"), json!([]));
    let _ = std::fs::remove_dir_all(dir);

    // VolumeFlags is left out of the boot checksum, so setting it needs nothing else.
    let mut dirty = volume_macos_has_visited();
    dirty[106] = 0x02;
    let (engine, _session, dir) = engine_with_image("mounts-dirty", dirty);
    let history = call(&engine, "mount_history", json!({}));
    assert_eq!(history["dirtyFlag"], true);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_write_counter_is_read_for_the_right_disk() {
    use salvage_core::mount::{parse_write_counter, WriteCounter};
    let ioreg = r#"
+-o IOBlockStorageDriver  <class IOBlockStorageDriver>
  |   "Statistics" = {"Operations (Write)"=3454,"Latency Time (Write)"=0,"Bytes (Write)"=99,"Operations (Read)"=7}
    |   "BSD Name" = "disk4"
        |   "BSD Name" = "disk4s1"
+-o IOBlockStorageDriver  <class IOBlockStorageDriver>
  |   "Statistics" = {"Operations (Write)"=310,"Bytes (Read)"=713928624640,"Errors (Write)"=0,"Bytes (Write)"=1618944,"Retries (Write)"=0}
    |   "BSD Name" = "disk12"
        |   "BSD Name" = "disk12s1"
"#;
    assert_eq!(
        parse_write_counter(ioreg, "disk12"),
        Some(WriteCounter {
            operations: 310,
            bytes: 1_618_944
        })
    );
    assert_eq!(
        parse_write_counter(ioreg, "disk4").unwrap().operations,
        3454
    );
    assert_eq!(parse_write_counter(ioreg, "disk1"), None);
    let only_a_partition = "\"Statistics\" = {\"Operations (Write)\"=1,\"Bytes (Write)\"=2}\n\"BSD Name\" = \"disk12s1\"";
    assert_eq!(parse_write_counter(only_a_partition, "disk12"), None);
}

#[test]
fn a_moved_write_counter_counts_from_when_the_volume_opened() {
    use salvage_core::mount::{DiskWrites, WriteCounter};
    let counter = |operations, bytes| WriteCounter { operations, bytes };
    let open = Some(counter(310, 1_000));

    assert_eq!(DiskWrites::between(open, open).since_open, counter(0, 0));
    assert_eq!(
        DiskWrites::between(open, Some(counter(315, 1_500))).since_open,
        counter(5, 500)
    );
    // It only falls when the disk was attached again, which is when macOS writes.
    assert_eq!(
        DiskWrites::between(open, Some(counter(4, 80))).since_open,
        counter(4, 80)
    );
    // A disk image, or a disk that went away, has nothing to compare.
    assert_eq!(DiskWrites::between(None, None).since_open, counter(0, 0));
    assert_eq!(DiskWrites::between(open, None).since_open, counter(0, 0));
}

#[test]
fn a_disk_image_has_no_write_counter() {
    let (engine, _session, dir) = engine_with_volume("no-counter");
    let status = call(&engine, "status", json!({}));
    assert!(status["writes"]["atOpen"].is_null());
    assert!(status["writes"]["now"].is_null());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_damaged_jpeg_is_described_by_what_survives() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED};
    let image_data: Vec<u8> = (0..60_000u32).map(|n| (n % 251) as u8 + 1).collect();
    let scan_start = sample_jpeg(b"", b"").len() - 2;

    // Nothing below 0xC0 is a marker, so the break is at those bytes and not somewhere later.
    let mut illegal = image_data.clone();
    illegal[30_000..30_002].copy_from_slice(b"\xff\x75");
    let hit = judge("a.jpg", &sample_jpeg(&illegal, b""));
    assert_eq!(hit.outcome, OUTCOME_DAMAGED);
    assert_eq!(hit.first_break, Some((scan_start + 30_000) as u64));
    assert!(hit.note.contains("50% of its JPEG"), "{}", hit.note);

    // A legal marker that leads nowhere still counts what came before it.
    let mut stray = image_data.clone();
    stray[30_000..30_004].copy_from_slice(b"\xff\xe1\x00\x10");
    let hit = judge("a.jpg", &sample_jpeg(&stray, b""));
    assert_eq!(hit.outcome, OUTCOME_DAMAGED);
    assert!(hit.valid_fraction.unwrap() > 0.4, "{}", hit.note);

    // A file that only stops early has no break to report, and says what it does have.
    let whole = sample_jpeg(&image_data, b"");
    let cut = judge("a.jpg", &whole[..whole.len() / 2]);
    assert_eq!(cut.first_break, None);
    assert!(cut.note.contains("stops before its end"), "{}", cut.note);
}

#[test]
fn a_damaged_picture_says_whether_it_still_opens() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED};
    let jpeg = encoded_image(image::ImageFormat::Jpeg);

    // Everything but the end marker is there, so a viewer can still show it.
    let no_end = judge("a.jpg", &jpeg[..jpeg.len() - 2]);
    assert_eq!(no_end.outcome, OUTCOME_DAMAGED);
    assert_eq!(no_end.decoded, Some(true), "{}", no_end.note);
    assert!(no_end.note.contains("still opens"), "{}", no_end.note);

    let noise = judge("a.jpg", &vec![0x5a; 4096]);
    assert_eq!(noise.outcome, OUTCOME_DAMAGED);
    assert_eq!(noise.decoded, Some(false), "{}", noise.note);
    assert!(noise.note.contains("will not open"), "{}", noise.note);

    // A whole picture never pays for a decode: that is what keeps the test fast.
    assert_eq!(judge("a.jpg", &jpeg).decoded, None);
}

#[test]
fn forgetting_results_puts_the_old_score_back() {
    let (engine, session, dir) = engine_with_volume("forget");
    let id = call(&engine, "search", json!({"query": "photo.jpg"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    call(&engine, "file_tests_start", json!({"groups": ["photos"]}));
    wait_for_file_tests(&session);
    assert_eq!(
        call(&engine, "entry", json!({"id": id}))["entry"]["verdict"],
        "tested-bad"
    );

    let forgotten = call(&engine, "file_tests_forget", json!({"outcome": "damaged"}));
    assert!(forgotten["forgotten"].as_u64().expect("count") >= 1);
    assert_eq!(
        call(&engine, "entry", json!({"id": id}))["entry"]["verdict"],
        "likely-intact"
    );
    assert!(call(&engine, "file_test", json!({"id": id})).is_null());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn an_office_file_is_checked_against_its_own_allocation_table() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED};
    let mut doc = vec![0u8; 512 * 4];
    doc[..8].copy_from_slice(b"\xd0\xcf\x11\xe0\xa1\xb1\x1a\xe1");
    doc[30] = 9;
    doc[76..512].fill(0xff);
    doc[76..80].copy_from_slice(&0u32.to_le_bytes());
    doc[512..1024].fill(0xff);
    let good = judge("a.doc", &doc);
    assert_eq!(good.outcome, OUTCOME_PASSED, "{}", good.note);
    assert_eq!(good.format, "ole");

    let mut hit = doc.clone();
    hit[512..1024].fill(0x77);
    assert_eq!(judge("a.doc", &hit).outcome, OUTCOME_DAMAGED);
}

#[test]
fn a_text_file_with_a_binary_stretch_fails() {
    use salvage_core::filetest::{judge, OUTCOME_DAMAGED, OUTCOME_PASSED};
    let text = "ordinary words, line after line.\n"
        .repeat(1000)
        .into_bytes();
    assert_eq!(judge("a.txt", &text).outcome, OUTCOME_PASSED);
    let mut hit = text.clone();
    hit[8192..16384].fill(0);
    assert_eq!(judge("a.txt", &hit).outcome, OUTCOME_DAMAGED);
}

#[test]
fn a_file_type_with_no_test_changes_nothing() {
    use salvage_core::filetest::{judge, OUTCOME_UNTESTED};
    assert_eq!(judge("a.iso", b"anything").outcome, OUTCOME_UNTESTED);
}

fn wait_for_file_tests(session: &Session) -> salvage_core::filetest::FileTestStatus {
    loop {
        let status = session.file_test_status().expect("status");
        if status.state != "running" {
            return status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[test]
fn file_test_results_are_counted_per_file_type() {
    let (engine, session, dir) = engine_with_volume("by-type");
    let untested = call(&engine, "file_tests_by_type", json!({}));
    let jpg = |report: &serde_json::Value| {
        report["types"]
            .as_array()
            .expect("types")
            .iter()
            .find(|row| row["extension"] == "jpg")
            .cloned()
            .expect("a jpg row")
    };
    let before = jpg(&untested);
    assert_eq!(before["group"], "photos");
    assert!(before["files"].as_u64().unwrap() >= 1);
    assert_eq!(before["notTestedYet"], before["files"]);

    call(&engine, "file_tests_start", json!({"groups": ["photos"]}));
    let status = wait_for_file_tests(&session);
    assert_eq!(status.state, "finished", "{:?}", status.error);
    let after = jpg(&call(&engine, "file_tests_by_type", json!({})));
    // The test volume's photo holds noise, so it is counted as damaged.
    assert!(after["damaged"].as_u64().unwrap() >= 1, "{after}");
    let counted = ["passed", "damaged", "noTest", "notTestedYet"]
        .iter()
        .map(|key| after[*key].as_u64().unwrap())
        .sum::<u64>();
    assert_eq!(counted, after["files"].as_u64().unwrap());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn background_tests_change_the_score_and_survive_a_rebuild() {
    let (engine, session, dir) = engine_with_volume("filetests");
    let before = call(&engine, "search", json!({"query": "photo.jpg"}))["rows"][0].clone();
    assert_eq!(before["verdict"], "likely-intact");
    let id = before["id"].as_i64().expect("id");

    let started = call(&engine, "file_tests_start", json!({"groups": ["photos"]}));
    assert!(started["total"].as_u64().expect("total") >= 1);
    let status = wait_for_file_tests(&session);
    assert_eq!(status.state, "finished", "{:?}", status.error);
    assert!(status.damaged >= 1);

    // The test volume's photo holds noise, so whatever its entry said, it has failed.
    let after = call(&engine, "entry", json!({"id": id}))["entry"].clone();
    assert_eq!(after["verdict"], "tested-bad");
    assert!(after["confidence"].as_f64().expect("score") <= 0.1);
    assert!(after["reason"]
        .as_str()
        .expect("reason")
        .contains("Tested:"));
    let test = call(&engine, "file_test", json!({"id": id}));
    assert_eq!(test["outcome"], "damaged");
    assert_eq!(test["sha256"].as_str().expect("hash").len(), 64);

    // A rebuild works everything out again, and the result still stands.
    session.resolve().expect("resolve");
    let rebuilt = call(&engine, "entry", json!({"id": id}))["entry"].clone();
    assert_eq!(rebuilt["verdict"], "tested-bad");

    // A second run has nothing left to do in that group.
    let again = call(&engine, "file_tests_start", json!({"groups": ["photos"]}));
    assert_eq!(again["total"], 0);
    wait_for_file_tests(&session);

    // A pass, stored by hand, lifts a file that the map could not be sure of.
    let track = call(&engine, "search", json!({"query": "song.mp3"}))["rows"][0].clone();
    let track_id = track["id"].as_i64().expect("id");
    session
        .with_database(|database| {
            database.save_file_tests(&[salvage_core::filetest::FileTest {
                id: track_id,
                first_cluster: track["firstCluster"].as_u64().expect("cluster") as u32,
                length: track["length"].as_u64().expect("length"),
                outcome: "passed".into(),
                format: "mp3".into(),
                valid_fraction: Some(1.0),
                first_break: None,
                decoded: None,
                note: "Tested: its MP3 structure is whole.".into(),
                sha256: None,
                tested_at_millis: 1,
            }])
        })
        .expect("save");
    session.resolve().expect("resolve");
    let lifted = call(&engine, "entry", json!({"id": track_id}))["entry"].clone();
    assert_eq!(lifted["verdict"], "tested-good");
    assert!(lifted["confidence"].as_f64().expect("score") >= 0.99);

    // The next run counts it with the rest.
    call(&engine, "file_tests_start", json!({"groups": ["photos"]}));
    assert!(wait_for_file_tests(&session).passed >= 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_test_of_old_clusters_does_not_apply_to_a_changed_entry() {
    let (engine, session, dir) = engine_with_volume("old-test");
    let row = call(&engine, "search", json!({"query": "photo.jpg"}))["rows"][0].clone();
    let id = row["id"].as_i64().expect("id");
    session
        .with_database(|database| {
            database.save_file_tests(&[salvage_core::filetest::FileTest {
                id,
                first_cluster: 999_999,
                length: 1,
                outcome: "damaged".into(),
                format: "jpeg".into(),
                valid_fraction: Some(0.0),
                first_break: Some(0),
                decoded: Some(false),
                note: "Tested: old result.".into(),
                sha256: None,
                tested_at_millis: 1,
            }])
        })
        .expect("save");
    session.resolve().expect("resolve");
    let after = call(&engine, "entry", json!({"id": id}))["entry"].clone();
    assert_eq!(after["verdict"], "likely-intact");
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn identical_files_can_be_shown_as_one() {
    let (engine, session, dir) = engine_with_volume("copies");
    let find = |name: &str| call(&engine, "search", json!({"query": name}))["rows"][0].clone();
    let first = find("photo.jpg");
    let second = find("reused.jpg");
    let same = "ab".repeat(32);
    let stored = |row: &serde_json::Value| salvage_core::filetest::FileTest {
        id: row["id"].as_i64().expect("id"),
        first_cluster: row["firstCluster"].as_u64().expect("cluster") as u32,
        length: row["length"].as_u64().expect("length"),
        outcome: "untested".into(),
        format: "unknown".into(),
        valid_fraction: None,
        first_break: None,
        decoded: None,
        note: String::new(),
        sha256: Some(same.clone()),
        tested_at_millis: 1,
    };
    session
        .with_database(|database| database.save_file_tests(&[stored(&first), stored(&second)]))
        .expect("save");

    let all = call(&engine, "browse_files", json!({"group": "photos"}));
    let hidden = call(
        &engine,
        "browse_files",
        json!({"group": "photos", "hideCopies": true}),
    );
    assert_eq!(
        hidden["total"].as_u64().expect("total"),
        all["total"].as_u64().expect("total") - 1
    );
    assert_eq!(hidden["hiddenCopies"], 1);
    assert_eq!(all["hiddenCopies"], 0);

    // The one shown says how many it stands for, and the best of the set is the one shown.
    let rows = hidden["rows"].as_array().expect("rows");
    let shown = rows.iter().find(|row| row["copies"] == 2).expect("the set");
    let names: Vec<_> = rows
        .iter()
        .map(|row| row["name"].as_str().expect("name"))
        .collect();
    assert!(
        names.contains(&"photo.jpg") != names.contains(&"reused.jpg"),
        "{names:?}"
    );
    assert!(
        shown["confidence"].as_f64().expect("score")
            >= first["confidence"]
                .as_f64()
                .expect("score")
                .min(second["confidence"].as_f64().expect("score"))
    );

    // Every location stays reachable.
    let other = call(&engine, "file_copies", json!({"id": first["id"]}));
    assert_eq!(other[0]["name"], "reused.jpg");
    assert_eq!(other.as_array().expect("rows").len(), 1);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_resumed_copy_only_needs_space_for_what_is_not_there_yet() {
    let (engine, session, dir) = engine_with_volume("resume-space");
    let destination = copy_destination("resume-space");
    let track_id = call(&engine, "search", json!({"query": "track.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    let request = json!({"destination": destination, "entryIds": [track_id]});

    let before = call(&engine, "copy_plan", request.clone());
    let wanted = before["bytes"].as_u64().expect("bytes");
    assert!(wanted > 0);
    assert_eq!(before["alreadyCopiedBytes"], 0);

    copy_and_wait(&engine, &session, request.clone());
    let after = call(&engine, "copy_plan", request.clone());
    assert_eq!(after["bytes"].as_u64().expect("bytes"), wanted);
    assert_eq!(after["alreadyCopiedBytes"].as_u64().expect("bytes"), wanted);

    // A file removed from the destination has to be copied again, so it needs space again.
    let row = &manifest_rows(&destination)[0];
    std::fs::remove_file(row["target"].as_str().expect("target")).expect("remove");
    let removed = call(&engine, "copy_plan", request);
    assert_eq!(removed["alreadyCopiedBytes"], 0);

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn a_new_run_copies_again_a_recent_file_that_fails_its_hash() {
    let (engine, session, dir) = engine_with_volume("recheck");
    let destination = copy_destination("recheck");
    let track_id = call(&engine, "search", json!({"query": "track.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    let request = json!({"destination": destination, "entryIds": [track_id]});
    copy_and_wait(&engine, &session, request.clone());

    // What a power cut can leave: the right length, the wrong bytes.
    let row = &manifest_rows(&destination)[0];
    let target = row["target"].as_str().expect("target").to_string();
    let good = std::fs::read(&target).expect("read");
    let mut damaged = good.clone();
    damaged[0] ^= 0xff;
    std::fs::write(&target, &damaged).expect("write");

    let second = copy_and_wait(&engine, &session, request);
    assert_eq!(second.files_skipped, 0);
    assert_eq!(second.files_done, 1);
    assert!(second
        .error
        .is_some_and(|text| text.contains("failed their hash")));
    assert_eq!(std::fs::read(&target).expect("read"), good);
    // The bad copy is kept, because this app never deletes a finished file.
    let aside = format!("{target}.failed-check");
    assert_eq!(std::fs::read(aside).expect("read"), damaged);

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn a_new_run_trusts_copies_older_than_the_last_commit() {
    let (engine, session, dir) = engine_with_volume("older");
    let destination = copy_destination("older");
    let request = json!({"destination": destination, "folders": [20]});
    copy_and_wait(&engine, &session, request.clone());

    let manifest = destination.join(salvage_core::export::MANIFEST_NAME);
    let mut rows = manifest_rows(&destination);
    assert!(rows.len() >= 2, "needs two files, found {}", rows.len());
    let newest = rows[0]["copiedAtMillis"].as_u64().expect("time");
    rows[0]["copiedAtMillis"] = json!(newest - 10 * 60 * 1000);
    let lines: Vec<String> = rows.iter().map(|row| row.to_string()).collect();
    std::fs::write(&manifest, lines.join("\n") + "\n").expect("write");

    let target = rows[0]["target"].as_str().expect("target").to_string();
    let mut changed = std::fs::read(&target).expect("read");
    changed[0] ^= 0xff;
    std::fs::write(&target, &changed).expect("write");

    let second = copy_and_wait(&engine, &session, request);
    assert_eq!(second.files_done, 0);
    assert_eq!(std::fs::read(&target).expect("read"), changed);

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn only_the_person_can_start_or_stop_a_copy() {
    let (engine, _session, dir) = engine_with_volume("consent");
    let args = json!({"destination": copy_destination("consent"), "folders": [20]});
    let error = engine
        .call("copy_start", &args, AUTHOR_CLAUDE)
        .expect_err("an AI must not start a copy");
    assert!(error.to_string().contains("Only the person"), "{error}");
    assert!(engine.call("copy_stop", &json!({}), AUTHOR_CLAUDE).is_err());
    assert!(
        call(&engine, "copy_plan", args).get("files").is_some(),
        "planning is allowed"
    );
    let advertised: Vec<String> = tool_definitions()
        .iter()
        .map(|tool| tool["name"].as_str().expect("name").to_string())
        .collect();
    assert!(!advertised
        .iter()
        .any(|name| name == "copy_start" || name == "ui_eval"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_destination_on_the_source_disk_is_refused_and_unknowns_fail_closed() {
    use salvage_core::export::{destination_problem, parse_physical_disks};
    let apfs = "   Part of Whole:             disk7\n   APFS Physical Store:       disk5s1\n";
    assert_eq!(
        parse_physical_disks("/dev/disk7s1", apfs),
        vec!["disk7", "disk5"]
    );

    // Same disk, directly and through an APFS container's physical store.
    assert!(destination_problem(
        Some("disk12"),
        "/dev/disk12s1",
        Some("Part of Whole: disk12")
    )
    .is_some());
    assert!(destination_problem(Some("disk5"), "/dev/disk7s1", Some(apfs)).is_some());
    // A different disk is fine, and disk12 must not match disk1 or disk123.
    assert!(destination_problem(Some("disk12"), "/dev/disk7s1", Some(apfs)).is_none());
    assert!(destination_problem(
        Some("disk1"),
        "/dev/disk12s1",
        Some("Part of Whole: disk12")
    )
    .is_none());
    // If the disk cannot be identified, refuse.
    assert!(destination_problem(Some("disk12"), "/dev/disk7s1", None).is_some());
    // Disk images have no source disk to protect.
    assert!(destination_problem(None, "/dev/disk12s1", None).is_none());
}

#[test]
fn nothing_a_damaged_name_says_can_escape_the_destination() {
    use salvage_core::export::relative_target;
    let escape = relative_target("[orphan @5]/../../etc", "../passwd");
    assert!(
        escape.components().all(|part| !matches!(
            part,
            std::path::Component::ParentDir | std::path::Component::RootDir
        )),
        "{escape:?}"
    );
    assert_eq!(
        relative_target("/music", "a.mp3"),
        PathBuf::from("Volume root/music/a.mp3")
    );
    assert_eq!(
        relative_target("[orphan @526]", "film.avi"),
        PathBuf::from("Lost folder 526/film.avi")
    );
    assert_eq!(
        relative_target("[orphan @1]/a:b", "c/d\u{7}.txt"),
        PathBuf::from("Lost folder 1/a_b/c_d_.txt")
    );
}

#[test]
fn unexplained_regions_are_found_and_a_label_explains_them() {
    let (engine, _session, dir) = engine_with_volume("regions");
    let runs = call(&engine, "unexplained_runs", json!({"minClusters": 1}));
    let list = runs.as_array().expect("runs");
    assert!(!list.is_empty());
    let biggest = &list[0];
    let (start, end) = (
        biggest["startCluster"].as_u64().expect("start"),
        biggest["endCluster"].as_u64().expect("end"),
    );
    assert!(end > start);

    let before = call(&engine, "resolve", json!({}))["unexplainedFreeClusters"]
        .as_u64()
        .expect("count");
    let added = call(
        &engine,
        "add_overlay",
        json!({
            "kind": "region_label", "target": start, "payload": {"end": end, "label": "Test region"}, "note": "test",
        }),
    );
    let after = added["summary"]["unexplainedFreeClusters"]
        .as_u64()
        .expect("count");
    assert!(after < before, "{before} -> {after}");
    assert!(
        added["summary"]["labelledFreeClusters"]
            .as_u64()
            .expect("count")
            > 0
    );
    let inside = call(&engine, "inspect_cluster", json!({"cluster": start}));
    assert_eq!(inside["ownership"], "explained by a hypothesis");
    assert_eq!(inside["notes"][0]["payload"]["label"], "Test region");

    call(
        &engine,
        "set_overlay_active",
        json!({"id": added["id"], "active": false}),
    );
    assert_eq!(
        call(&engine, "resolve", json!({}))["unexplainedFreeClusters"]
            .as_u64()
            .expect("count"),
        before
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn explain_region_reports_what_it_samples() {
    let (engine, _session, dir) = engine_with_volume("explain");
    let report = call(
        &engine,
        "explain_region",
        json!({"startCluster": 100, "endCluster": 102, "samples": 2}),
    );
    assert_eq!(report["sampled"], 2);
    assert_eq!(
        report["signaturesAtClusterStart"]["JPEG start"], 1,
        "{report}"
    );
    assert_eq!(report["readErrors"], 0);
    let directory = call(
        &engine,
        "explain_region",
        json!({"startCluster": 20, "endCluster": 21, "samples": 1}),
    );
    assert!(directory["exfatEntriesValid"].as_u64().expect("count") > 0);
    assert!(directory["exfatNames"]
        .as_array()
        .expect("names")
        .iter()
        .any(|name| name == "track.mp3"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ntfs_record_names_are_read_and_other_data_is_ignored() {
    // An NTFS record with a long file name, at an arbitrary place in a cluster.
    let mut record = vec![0u8; 1024];
    record[..4].copy_from_slice(b"FILE");
    record[0x14] = 0x38;
    let attribute = 0x38;
    record[attribute..attribute + 4].copy_from_slice(&0x30u32.to_le_bytes());
    record[attribute + 4..attribute + 8].copy_from_slice(&0x80u32.to_le_bytes());
    record[attribute + 0x14] = 0x18;
    let content = attribute + 0x18;
    record[content + 0x40] = 5;
    record[content + 0x41] = 1;
    for (index, unit) in "a.txt".encode_utf16().enumerate() {
        record[content + 0x42 + index * 2..content + 0x44 + index * 2]
            .copy_from_slice(&unit.to_le_bytes());
    }
    record[attribute + 0x80..attribute + 0x84].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    assert_eq!(
        salvage_core::regions::ntfs_record_name(&record).as_deref(),
        Some("a.txt")
    );
    assert_eq!(salvage_core::regions::ntfs_record_name(&[0u8; 1024]), None);
}

/// Builds a one-member stored zip archive.
fn stored_zip(name: &str, contents: &[u8], crc: u32) -> Vec<u8> {
    let mut zip = Vec::new();
    zip.extend_from_slice(b"PK\x03\x04\x14\0\0\0\0\0\0\0\0\0");
    zip.extend_from_slice(&crc.to_le_bytes());
    for _ in 0..2 {
        zip.extend_from_slice(&(contents.len() as u32).to_le_bytes());
    }
    zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip.extend_from_slice(name.as_bytes());
    zip.extend_from_slice(contents);
    let central = zip.len() as u32;
    zip.extend_from_slice(b"PK\x01\x02\x14\0\x14\0\0\0\0\0\0\0\0\0");
    zip.extend_from_slice(&crc.to_le_bytes());
    for _ in 0..2 {
        zip.extend_from_slice(&(contents.len() as u32).to_le_bytes());
    }
    zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
    zip.extend_from_slice(&[0u8; 12]);
    zip.extend_from_slice(&0u32.to_le_bytes());
    zip.extend_from_slice(name.as_bytes());
    let size = zip.len() as u32 - central;
    zip.extend_from_slice(b"PK\x05\x06\0\0\0\0\x01\0\x01\0");
    zip.extend_from_slice(&size.to_le_bytes());
    zip.extend_from_slice(&central.to_le_bytes());
    zip.extend_from_slice(&0u16.to_le_bytes());
    zip
}

#[test]
fn the_zip_check_catches_a_single_changed_byte() {
    // CRC-32 of "hello world", from the published check value for this algorithm.
    const HELLO_CRC: u32 = 0x0D4A_1185;
    let good = salvage_core::verify::verify_zip(&stored_zip("a.txt", b"hello world", HELLO_CRC));
    assert_eq!(good.units, 1);
    assert_eq!(good.valid_fraction, 1.0, "{good:?}");
    assert!(good.breaks.is_empty());

    let bad = salvage_core::verify::verify_zip(&stored_zip("a.txt", b"hello worlD", HELLO_CRC));
    assert_eq!(bad.breaks.len(), 1);
    assert_eq!(bad.valid_fraction, 0.0);
    assert!(
        bad.notes
            .iter()
            .any(|note| note.contains("a.txt fails its CRC")),
        "{bad:?}"
    );

    let truncated = stored_zip("a.txt", b"hello world", HELLO_CRC);
    let report = salvage_core::verify::verify_zip(&truncated[..truncated.len() - 30]);
    assert!(
        !report.reached_end,
        "a zip with no central directory cannot pass"
    );
}

#[test]
fn a_salvage_drive_is_remembered_used_by_default_and_only_the_person_can_set_it() {
    let (engine, session, dir) = engine_with_volume("salvage");
    let destination = copy_destination("salvage");

    assert_eq!(
        call(&engine, "get_salvage_drive", json!({}))["destination"],
        serde_json::Value::Null
    );
    let unset = call(&engine, "copy_plan", json!({"folders": [20]}));
    assert!(
        unset["refused"]
            .as_str()
            .expect("refused")
            .contains("salvage drive"),
        "{unset}"
    );

    assert!(engine
        .call(
            "set_salvage_drive",
            &json!({"destination": destination}),
            AUTHOR_CLAUDE
        )
        .is_err());
    assert!(
        engine
            .call(
                "set_salvage_drive",
                &json!({"destination": "relative/path"}),
                "you"
            )
            .is_err(),
        "unsafe choices are refused when made"
    );
    engine
        .call(
            "set_salvage_drive",
            &json!({"destination": destination}),
            "you",
        )
        .expect("set");
    assert_eq!(
        call(&engine, "get_salvage_drive", json!({}))["destination"],
        destination.display().to_string()
    );

    // It survives a new database connection, as it must across restarts.
    let reopened = MapDb::open(&session.workspace.database()).expect("database");
    assert_eq!(
        reopened.state("salvage_destination").expect("state"),
        Some(destination.display().to_string())
    );

    // A copy with no destination goes there.
    let status = copy_and_wait(&engine, &session, json!({"folders": [21]}));
    assert_eq!(status.state, "finished", "{status:?}");
    assert!(destination.join("Lost folder 20/album/song.mp3").exists());

    engine
        .call("set_salvage_drive", &json!({}), "you")
        .expect("clear");
    assert_eq!(
        call(&engine, "get_salvage_drive", json!({}))["destination"],
        serde_json::Value::Null
    );

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn the_basket_keeps_its_order_copies_in_it_and_removes_overlaps() {
    let (engine, session, dir) = engine_with_volume("basket");
    let destination = copy_destination("basket");
    let song = call(&engine, "search", json!({"query": "song.mp3"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");
    let photo = call(&engine, "search", json!({"query": "photo.jpg"}))["rows"][0]["id"]
        .as_i64()
        .expect("id");

    call(
        &engine,
        "basket_add",
        json!({"kind": "entry", "target": photo}),
    );
    call(
        &engine,
        "basket_add",
        json!({"kind": "entry", "target": song}),
    );
    // Its song is also inside this folder, so it must only be copied once.
    let items = call(
        &engine,
        "basket_add",
        json!({"kind": "folder", "target": 21}),
    );
    assert_eq!(items.as_array().expect("items").len(), 3);
    call(
        &engine,
        "basket_add",
        json!({"kind": "folder", "target": 21}),
    );
    assert_eq!(
        call(&engine, "basket_list", json!({}))
            .as_array()
            .expect("items")
            .len(),
        3,
        "adding twice changes nothing"
    );
    assert!(engine
        .call(
            "basket_add",
            &json!({"kind": "entry", "target": 999_999}),
            AUTHOR_CLAUDE
        )
        .is_err());
    assert!(engine
        .call(
            "basket_add",
            &json!({"kind": "disk", "target": 1}),
            AUTHOR_CLAUDE
        )
        .is_err());

    // Reorder, and confirm the order survives a new database connection.
    let last_id = items[2]["id"].as_i64().expect("id");
    let moved = call(
        &engine,
        "basket_move",
        json!({"id": last_id, "position": 0}),
    );
    assert_eq!(moved[0]["kind"], "folder");
    assert_eq!(moved[1]["name"], "photo.jpg");
    assert_eq!(moved[2]["name"], "song.mp3");
    let reopened = MapDb::open(&session.workspace.database()).expect("database");
    assert_eq!(reopened.basket_items().expect("items")[0].kind, "folder");

    let plan = call(&engine, "basket_plan", json!({"destination": destination}));
    assert_eq!(plan["files"], 2, "overlaps count once: {plan}");

    // Only the person can start it, and an AI may fill it.
    assert!(engine
        .call(
            "basket_start",
            &json!({"destination": destination}),
            AUTHOR_CLAUDE
        )
        .is_err());
    assert!(!tool_definitions()
        .iter()
        .any(|tool| tool["name"] == "basket_start"));
    engine
        .call("basket_start", &json!({"destination": destination}), "you")
        .expect("start");
    loop {
        let status = session.copy_status().expect("status");
        if status.state != "running" {
            assert_eq!(status.state, "finished", "{status:?}");
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let rows = manifest_rows(&destination);
    assert_eq!(rows.len(), 2, "each file is copied once");
    assert_eq!(
        rows[0]["name"], "song.mp3",
        "the folder at the top of the basket went first"
    );
    assert_eq!(rows[1]["name"], "photo.jpg");

    call(&engine, "basket_remove", json!({"id": last_id}));
    assert_eq!(
        call(&engine, "basket_list", json!({}))
            .as_array()
            .expect("items")
            .len(),
        2
    );
    call(&engine, "basket_clear", json!({}));
    assert!(call(&engine, "basket_list", json!({}))
        .as_array()
        .expect("items")
        .is_empty());
    assert!(
        engine
            .call("basket_start", &json!({"destination": destination}), "you")
            .is_err(),
        "an empty basket has nothing to start"
    );

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
}

#[test]
fn another_process_can_see_a_copy_and_ask_it_to_stop() {
    let (engine, session, dir) = engine_with_volume("processes");
    let destination = copy_destination("processes");
    // A second session on the same workspace stands in for the app's window.
    let window = Session::with_source(
        Path::new("/test/image"),
        Arc::new(MemorySource::new(build_volume())),
        Workspace::at(&dir).expect("workspace"),
    )
    .expect("second session");
    assert_eq!(window.copy_status().expect("status").state, "");

    let status = copy_and_wait(
        &engine,
        &session,
        json!({"destination": destination, "folders": [20]}),
    );
    assert_eq!(status.state, "finished");
    let seen = window.copy_status().expect("status");
    assert_eq!(
        seen.state, "finished",
        "the window sees a copy it did not start"
    );
    assert_eq!(seen.files_done, status.files_done);
    assert_eq!(
        seen.bytes_done, status.bytes_total,
        "progress adds up to the total: {seen:?}"
    );

    // A stop request from the window is noticed by the copying process.
    window.copy_stop();
    let stored = session
        .with_database(|database| database.state("copy_stop_requested"))
        .expect("state");
    assert_eq!(stored.as_deref(), Some("1"));
    let second = copy_destination("processes-2");
    let next = copy_and_wait(
        &engine,
        &session,
        json!({"destination": second, "folders": [21]}),
    );
    assert_eq!(
        next.state, "finished",
        "starting a new copy clears an old stop request"
    );

    let _ = std::fs::remove_dir_all(dir);
    let _ = std::fs::remove_dir_all(destination);
    let _ = std::fs::remove_dir_all(second);
}

#[test]
fn a_recorded_copy_that_stopped_reporting_is_not_shown_as_running() {
    let (_engine, session, dir) = engine_with_volume("stale");
    let old = salvage_core::export::CopyStatus {
        state: "running".into(),
        updated_at_millis: 1000,
        ..Default::default()
    };
    session
        .with_database(|database| {
            database.set_state("copy_status", &serde_json::to_string(&old).expect("json"))
        })
        .expect("state");
    assert_eq!(session.copy_status().expect("status").state, "stopped");
    let _ = std::fs::remove_dir_all(dir);
}

/// Four lost folders and nothing else, for rebuilding a tree on a canvas. Two of them hold a
/// `shared.jpg`: the copy at cluster 32 is whole, and the one at cluster 23 was wiped to zeros.
fn build_jigsaw_volume() -> Vec<u8> {
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
        ],
    );
    let jpeg = |b: &mut Builder, cluster: u32| {
        b.put(cluster, 0, &[0xFF, 0xD8, 0xFF, 0xE0]);
        b.put(cluster, 16, &noise(cluster, CLUSTER - 16));
    };
    let mp3 = |b: &mut Builder, cluster: u32| {
        b.put(cluster, 0, b"ID3\x04");
        b.put(cluster, 16, &noise(cluster, CLUSTER - 16));
    };
    // Piece A at cluster 20, with a subfolder of its own at 21.
    b.directory(
        20,
        &[
            entry_set("a1.jpg", false, 22, CLUSTER as u64, true, true),
            entry_set("shared.jpg", false, 23, CLUSTER as u64, true, true),
            entry_set("sub", true, 21, CLUSTER as u64, true, true),
        ],
    );
    jpeg(&mut b, 22);
    b.directory(
        21,
        &[entry_set("deep.mp3", false, 24, CLUSTER as u64, true, true)],
    );
    mp3(&mut b, 24);
    // Piece B at cluster 30, holding the good copy of shared.jpg.
    b.directory(
        30,
        &[
            entry_set("b1.mp3", false, 31, CLUSTER as u64, true, true),
            entry_set("SHARED.jpg", false, 32, CLUSTER as u64, true, true),
        ],
    );
    mp3(&mut b, 31);
    jpeg(&mut b, 32);
    // Pieces C and D.
    b.directory(
        40,
        &[entry_set("c.mp3", false, 41, CLUSTER as u64, true, true)],
    );
    mp3(&mut b, 41);
    b.directory(
        50,
        &[entry_set("d.mp3", false, 51, CLUSTER as u64, true, true)],
    );
    mp3(&mut b, 51);
    b.image
}

fn call_as_person(engine: &Engine, tool: &str, args: serde_json::Value) -> serde_json::Value {
    engine
        .call(tool, &args, salvage_core::engine::AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool} failed: {error:#}"))
}

fn refusal(engine: &Engine, tool: &str, args: serde_json::Value) -> String {
    match engine.call(tool, &args, salvage_core::engine::AUTHOR_USER) {
        Ok(value) => panic!("{tool} should have been refused, and gave {value}"),
        Err(error) => format!("{error:#}"),
    }
}

fn names(listing: &serde_json::Value, kind: &str) -> Vec<String> {
    listing[kind]
        .as_array()
        .expect("list")
        .iter()
        .map(|entry| {
            entry["name"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| entry["pathName"].as_str().expect("path name").to_string())
        })
        .collect()
}

#[test]
fn a_canvas_rebuilds_a_tree_from_lost_pieces_without_touching_the_map() {
    let (engine, _session, dir) = engine_with_image("canvas", build_jigsaw_volume());
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Mine"}))["id"]
        .as_i64()
        .expect("id");

    // Claude reads canvases, and only the person changes them.
    let claude = engine.call(
        "canvas_place",
        &json!({"canvas": canvas, "head": 20}),
        AUTHOR_CLAUDE,
    );
    assert!(format!("{:#}", claude.expect_err("refused")).contains("Only the person"));

    // The volume root and four lost folders are the pieces, largest first.
    let unused = call(&engine, "canvas_unused", json!({"canvas": canvas}));
    assert_eq!(unused["total"], 5);

    // The first piece becomes the root. C goes inside it, and B merges into it.
    let root = call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20}),
    )["id"]
        .as_i64()
        .expect("root");
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 40}),
    );
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 30, "merge": true}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    assert_eq!(
        names(&top, "files"),
        ["a1.jpg", "b1.mp3", "SHARED.jpg"],
        "the whole copy's own name"
    );
    assert_eq!(
        names(&top, "folders").len(),
        2,
        "sub, and C with its name lost"
    );
    assert!(names(&top, "folders").contains(&"sub".to_string()));
    assert!(names(&top, "folders").contains(&"lost:40".to_string()));

    // The two copies of shared.jpg clash, and the whole one wins over the wiped one.
    assert_eq!(top["clashes"], 1);
    let shared = top["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| {
            file["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("shared.jpg"))
        })
        .expect("shared")
        .clone();
    assert_eq!(shared["copies"].as_array().expect("copies").len(), 2);
    assert_eq!(shared["chosen"]["firstCluster"], 32);
    assert_eq!(shared["byPerson"], false);

    // The person can pick the other copy instead, and take the pick back.
    let wiped = shared["copies"]
        .as_array()
        .expect("copies")
        .iter()
        .find(|copy| copy["firstCluster"] == 23)
        .expect("wiped")["id"]
        .clone();
    call_as_person(
        &engine,
        "canvas_choose",
        json!({"canvas": canvas, "path": ["shared.jpg"], "entry": wiped}),
    );
    let picked = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    let shared = picked["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| {
            file["name"]
                .as_str()
                .is_some_and(|name| name.eq_ignore_ascii_case("shared.jpg"))
        })
        .expect("shared")
        .clone();
    assert_eq!(shared["chosen"]["firstCluster"], 23);
    assert_eq!(shared["byPerson"], true);
    call_as_person(
        &engine,
        "canvas_choose",
        json!({"canvas": canvas, "path": ["shared.jpg"]}),
    );

    // A made-up folder, and a piece moved into it.
    let photos = call_as_person(
        &engine,
        "canvas_folder",
        json!({"canvas": canvas, "name": "Photos"}),
    )["id"]
        .as_i64()
        .expect("folder");
    let items = call(&engine, "canvas_items", json!({"canvas": canvas}));
    let c_item = items
        .as_array()
        .expect("items")
        .iter()
        .find(|item| item["head"] == 40)
        .expect("C")["id"]
        .as_i64()
        .expect("id");
    call_as_person(
        &engine,
        "canvas_move",
        json!({"canvas": canvas, "item": c_item, "place": {"item": photos}}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    let made = top["folders"]
        .as_array()
        .expect("folders")
        .iter()
        .find(|folder| folder["name"] == "Photos")
        .expect("Photos")
        .clone();
    assert_eq!(
        made["origin"], "made",
        "the person made this folder, whatever is in it"
    );
    assert!(made["files"].as_u64().expect("files") >= 1);
    let inside = call(
        &engine,
        "canvas_tree",
        json!({"canvas": canvas, "path": ["Photos"]}),
    );
    assert_eq!(names(&inside, "folders"), ["lost:40"]);
    assert_eq!(
        names(
            &call(
                &engine,
                "canvas_tree",
                json!({"canvas": canvas, "path": ["Photos", "lost:40"]})
            ),
            "files"
        ),
        ["c.mp3"],
        "a folder whose name was lost is reached by its path name"
    );

    // A piece can go inside a found folder of another piece.
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 50, "place": {"item": root, "head": 21}}),
    );
    let sub = call(
        &engine,
        "canvas_tree",
        json!({"canvas": canvas, "path": ["sub"]}),
    );
    assert_eq!(names(&sub, "files"), ["deep.mp3"]);
    assert_eq!(names(&sub, "folders"), ["lost:50"]);

    // Each piece is used once, and only whole pieces can be placed.
    assert!(refusal(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20})
    )
    .contains("already on this canvas"));
    // A folder from inside a piece can be placed as a layer of its own, and taken off again.
    let inner = call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 21}),
    )["id"]
        .as_i64()
        .expect("inner");
    call_as_person(
        &engine,
        "canvas_remove",
        json!({"canvas": canvas, "item": inner}),
    );
    assert!(refusal(
        &engine,
        "canvas_move",
        json!({"canvas": canvas, "item": photos, "place": {"item": photos}})
    )
    .contains("inside itself"));
    assert_eq!(
        call(&engine, "canvas_unused", json!({"canvas": canvas}))["total"],
        1,
        "only the volume root is left"
    );

    // Taking a folder off puts what was in it back among the unused pieces.
    assert_eq!(
        call_as_person(
            &engine,
            "canvas_remove",
            json!({"canvas": canvas, "item": photos})
        )["removed"],
        2
    );
    assert_eq!(
        call(&engine, "canvas_unused", json!({"canvas": canvas}))["total"],
        2
    );

    // The rest can be put away in one go.
    let away = call_as_person(
        &engine,
        "canvas_leftovers",
        json!({"canvas": canvas, "name": "Leftovers"}),
    );
    assert_eq!(away["pieces"], 2);
    assert_eq!(
        call(&engine, "canvas_unused", json!({"canvas": canvas}))["total"],
        0
    );

    // Rebuilding the map keeps the canvas, because pieces are known by their first cluster.
    call_as_person(&engine, "resolve", json!({}));
    let again = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    assert!(names(&again, "folders").contains(&"Leftovers".to_string()));
    assert_eq!(names(&again, "files"), ["a1.jpg", "b1.mp3", "SHARED.jpg"]);

    // A second canvas is independent of the first.
    let other = call_as_person(&engine, "canvas_create", json!({"name": "Another try"}))["id"]
        .as_i64()
        .expect("id");
    assert_eq!(
        call(&engine, "canvas_unused", json!({"canvas": other}))["total"],
        5
    );
    assert_eq!(
        call(&engine, "canvas_list", json!({}))
            .as_array()
            .expect("canvases")
            .len(),
        2
    );
    call_as_person(&engine, "canvas_delete", json!({"canvas": other}));
    assert_eq!(
        call(&engine, "canvas_list", json!({}))
            .as_array()
            .expect("canvases")
            .len(),
        1
    );

    let _ = std::fs::remove_dir_all(dir);
}

/// Writes the jigsaw volume to `SALVAGE_JIGSAW_IMAGE`, for testing the window against it.
/// Run with: SALVAGE_JIGSAW_IMAGE=/path/jigsaw.img cargo test -p salvage-core --test synthetic write_the_jigsaw
#[test]
fn write_the_jigsaw_volume_for_window_tests() {
    verify_window_fixture("jigsaw", "SALVAGE_JIGSAW_IMAGE", build_jigsaw_volume());
}

fn verify_window_fixture(label: &str, variable: &str, image: Vec<u8>) {
    let (_workspace, dir) = temp_workspace(&format!("window-fixture-{label}"));
    let path = dir.join("fixture.img");
    std::fs::write(&path, &image).expect("written");
    let written = std::fs::read(&path).expect("read back");
    assert_eq!(written, image);
    let volume = Volume::open(Arc::new(MemorySource::new(written))).expect("valid fixture");
    assert_eq!(volume.boot.cluster_count, CLUSTER_COUNT);
    assert_eq!(
        volume.boot_check.main_stored,
        volume.boot_check.main_computed
    );
    if let Some(destination) = std::env::var_os(variable) {
        std::fs::copy(&path, destination).expect("exported window fixture");
    }
    std::fs::remove_dir_all(dir).expect("remove temporary fixture");
}

#[test]
fn a_lost_folder_says_what_its_files_mostly_are() {
    let (engine, _session, dir) = engine_with_image("contents", build_jigsaw_volume());
    let roots = call(&engine, "roots", json!({}));
    let rows = roots["rows"].as_array().expect("rows");
    let lost = rows
        .iter()
        .find(|row| row["head"] == 40)
        .expect("the lost folder at cluster 40");
    assert_eq!(lost["name"], "", "it has lost its name");
    assert_eq!(lost["contents"]["group"], "music", "{lost}");
    assert_eq!(lost["contents"]["files"], 1);
    assert!(lost["contents"]["share"].as_f64().unwrap() > 0.99);
    // A folder with its own name, and the volume root, get no guess.
    for named in rows
        .iter()
        .filter(|row| row["name"] != "" || row["nodeKind"] == "root")
    {
        assert!(named.get("contents").is_none(), "{named}");
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn clusters_put_in_order_can_be_shown_as_a_file() {
    let (engine, _session, dir) = engine_with_image("materialize", build_jigsaw_volume());
    let id = call(&engine, "search", json!({"query": "a1.jpg"}))["rows"][0]["id"]
        .as_i64()
        .unwrap();
    let entry = call(&engine, "entry", json!({"id": id}));
    let extents = entry["extents"].clone();
    let length = entry["entry"]["length"].as_u64().unwrap();
    let picture = call(
        &engine,
        "materialize",
        json!({"extents": extents, "length": length, "name": "a1.jpg"}),
    );
    assert_eq!(picture["kind"], "image");
    assert_eq!(picture["mime"], "image/jpeg");
    let encoded = picture["dataBase64"].as_str().unwrap();
    assert_eq!(
        encoded.len() as u64,
        length.div_ceil(3) * 4,
        "the whole file, base64"
    );
    // The same clusters read as text when the name says so, and not at all when it says
    // nothing the window can show.
    let text = call(
        &engine,
        "materialize",
        json!({"extents": extents, "length": 4, "name": "a.txt"}),
    );
    assert_eq!(text["kind"], "text");
    assert_eq!(text["text"].as_str().unwrap().chars().count(), 4);
    let other = call(
        &engine,
        "materialize",
        json!({"extents": extents, "name": "a.bin"}),
    );
    assert_eq!(other["kind"], "none");
    assert!(engine
        .call("materialize", &json!({}), AUTHOR_CLAUDE)
        .is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn similar_pieces_are_found_by_shared_files_and_can_be_swapped_in() {
    let (engine, _session, dir) = engine_with_image("similar", build_jigsaw_volume());

    // A and B both hold shared.jpg, of the same size, whatever the case of the name.
    let similar = call(&engine, "similar_pieces", json!({"head": 20}));
    assert_eq!(
        similar["files"], 3,
        "a1.jpg, shared.jpg and deep.mp3 in its subfolder"
    );
    let found = similar["similar"].as_array().expect("similar");
    assert_eq!(found.len(), 1, "only B shares files with A");
    assert_eq!(found[0]["piece"]["head"], 30);
    assert_eq!(found[0]["shared"], 1);
    assert!((found[0]["covers"].as_f64().expect("covers") - 1.0 / 3.0).abs() < 1e-9);
    assert!((found[0]["coveredBy"].as_f64().expect("covered") - 0.5).abs() < 1e-9);
    assert!(
        engine
            .call("similar_pieces", &json!({"head": 21}), AUTHOR_CLAUDE)
            .is_err(),
        "a subfolder is not a piece"
    );
    assert_eq!(
        call(&engine, "similar_pieces", json!({"head": 40}))["similar"]
            .as_array()
            .expect("none")
            .len(),
        0
    );

    // A placed piece can be swapped for another, keeping its place.
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Swap"}))["id"]
        .as_i64()
        .expect("id");
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20}),
    );
    let c = call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 40}),
    )["id"]
        .as_i64()
        .expect("C");
    call_as_person(
        &engine,
        "canvas_swap",
        json!({"canvas": canvas, "item": c, "head": 50}),
    );
    let swapped = call(
        &engine,
        "canvas_tree",
        json!({"canvas": canvas, "path": ["lost:50"]}),
    );
    assert_eq!(names(&swapped, "files"), ["d.mp3"]);
    assert_eq!(
        call(&engine, "canvas_unused", json!({"canvas": canvas}))["total"],
        3,
        "C is free again, D is used"
    );
    assert!(refusal(
        &engine,
        "canvas_swap",
        json!({"canvas": canvas, "item": c, "head": 20})
    )
    .contains("already on this canvas"));
    let made = call_as_person(
        &engine,
        "canvas_folder",
        json!({"canvas": canvas, "name": "Made"}),
    )["id"]
        .as_i64()
        .expect("folder");
    assert!(refusal(
        &engine,
        "canvas_swap",
        json!({"canvas": canvas, "item": made, "head": 40})
    )
    .contains("no piece to swap"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn claude_suggests_placements_and_only_the_person_accepts_them() {
    let (engine, _session, dir) = engine_with_image("suggest", build_jigsaw_volume());
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Ideas"}))["id"]
        .as_i64()
        .expect("id");

    // Claude may suggest, with a reason, and the canvas is unchanged until the person acts.
    let first = call(
        &engine,
        "canvas_suggest",
        json!({"canvas": canvas, "head": 20, "note": "Holds the most files."}),
    )["id"]
        .as_i64()
        .expect("id");
    assert_eq!(
        call(&engine, "canvas_items", json!({"canvas": canvas}))
            .as_array()
            .expect("items")
            .len(),
        0
    );
    assert!(
        engine
            .call(
                "canvas_suggest",
                &json!({"canvas": canvas, "head": 40, "note": "  "}),
                AUTHOR_CLAUDE
            )
            .is_err(),
        "a suggestion needs a reason"
    );
    assert!(
        engine
            .call(
                "canvas_accept",
                &json!({"canvas": canvas, "suggestion": first}),
                AUTHOR_CLAUDE
            )
            .is_err(),
        "Claude cannot accept its own"
    );

    let root = call_as_person(
        &engine,
        "canvas_accept",
        json!({"canvas": canvas, "suggestion": first}),
    )["id"]
        .as_i64()
        .expect("root");
    assert_eq!(
        call(&engine, "canvas_suggestions", json!({"canvas": canvas}))
            .as_array()
            .expect("open")
            .len(),
        0
    );
    assert_eq!(
        call(&engine, "canvas_items", json!({"canvas": canvas}))[0]["head"],
        20
    );

    // A suggestion into a place, and a merge, carry over when accepted.
    let merge = call(&engine, "canvas_suggest", json!({"canvas": canvas, "head": 30, "place": {"item": root}, "merge": true, "note": "Shares shared.jpg."}))["id"].as_i64().expect("id");
    let inside = call(&engine, "canvas_suggest", json!({"canvas": canvas, "head": 50, "place": {"item": root, "head": 21}, "note": "Also music."}))["id"].as_i64().expect("id");
    let dismissed = call(
        &engine,
        "canvas_suggest",
        json!({"canvas": canvas, "head": 40, "note": "Probably nothing."}),
    )["id"]
        .as_i64()
        .expect("id");
    let open = call(&engine, "canvas_suggestions", json!({"canvas": canvas}));
    assert_eq!(open.as_array().expect("open").len(), 3);
    assert_eq!(open[0]["author"], "claude");
    call_as_person(
        &engine,
        "canvas_accept",
        json!({"canvas": canvas, "suggestion": merge}),
    );
    call_as_person(
        &engine,
        "canvas_accept",
        json!({"canvas": canvas, "suggestion": inside}),
    );
    call_as_person(
        &engine,
        "canvas_dismiss",
        json!({"canvas": canvas, "suggestion": dismissed}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    assert!(
        names(&top, "files").contains(&"b1.mp3".to_string()),
        "merged into the top"
    );
    assert_eq!(
        names(
            &call(
                &engine,
                "canvas_tree",
                json!({"canvas": canvas, "path": ["sub"]})
            ),
            "folders"
        ),
        ["lost:50"]
    );
    assert_eq!(
        call(&engine, "canvas_suggestions", json!({"canvas": canvas}))
            .as_array()
            .expect("open")
            .len(),
        0
    );

    // A suggestion whose piece was placed by hand meanwhile drops out by itself.
    call(
        &engine,
        "canvas_suggest",
        json!({"canvas": canvas, "head": 40, "note": "Second thoughts."}),
    );
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 40}),
    );
    assert_eq!(
        call(&engine, "canvas_suggestions", json!({"canvas": canvas}))
            .as_array()
            .expect("open")
            .len(),
        0
    );
    assert!(
        engine
            .call(
                "canvas_suggest",
                &json!({"canvas": canvas, "head": 40, "note": "Again."}),
                AUTHOR_CLAUDE
            )
            .is_err(),
        "a used piece cannot be suggested"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_canvas_copies_out_at_its_rebuilt_paths_with_the_chosen_copies() {
    let (engine, session, dir) = engine_with_image("canvas-copy", build_jigsaw_volume());
    let destination = copy_destination("canvas-copy");
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Rebuilt one"}))["id"]
        .as_i64()
        .expect("id");
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20}),
    );
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 40}),
    );
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 30, "merge": true}),
    );
    let args = json!({"canvas": canvas, "destination": destination.display().to_string()});

    // Claude can plan the copy, but only the person starts it.
    let plan = call(&engine, "canvas_copy_plan", args.clone());
    assert_eq!(plan["files"], 5, "a1, b1, the whole shared, deep and c");
    let target = destination.join("Rebuilt one");
    assert_eq!(plan["destination"], target.display().to_string());
    assert!(engine
        .call("canvas_copy_start", &args, AUTHOR_CLAUDE)
        .is_err());

    call_as_person(&engine, "canvas_copy_start", args);
    let status = loop {
        let status = session.copy_status().expect("status");
        if status.state != "running" {
            break status;
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    assert_eq!(status.state, "finished", "{:?}", status.error);
    assert_eq!(status.files_done, 5);

    // Each file is where the canvas puts it, and the piece left off the canvas is not copied.
    for path in [
        "a1.jpg",
        "b1.mp3",
        "SHARED.jpg",
        "sub/deep.mp3",
        "Lost folder 40/c.mp3",
    ] {
        assert!(target.join(path).is_file(), "{path} is copied");
    }
    assert!(!target.join("d.mp3").exists());
    // The whole copy of the clashing file is the one copied, not the wiped one.
    let shared = std::fs::read(target.join("SHARED.jpg")).expect("shared");
    assert_eq!(&shared[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
    assert_eq!(manifest_rows(&target).len(), 5);

    let _ = std::fs::remove_dir_all(&destination);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn finder_reads_a_canvas_through_the_media_port_and_only_for_the_open_disk() {
    use salvage_core::media::{serial_text, MediaServer};
    let (engine, session, dir) = engine_with_image("finder", build_jigsaw_volume());
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Finder"}))["id"]
        .as_i64()
        .expect("id");
    for args in [
        json!({"canvas": canvas, "head": 20}),
        json!({"canvas": canvas, "head": 40}),
        json!({"canvas": canvas, "head": 30, "merge": true}),
    ] {
        call_as_person(&engine, "canvas_place", args);
    }
    let media = MediaServer::start(engine.clone()).expect("start");
    let info = media.info();
    let port = info.base_url.rsplit(':').next().expect("port").to_string();
    let serial = serial_text(&session);
    let hex = |path: serde_json::Value| -> String {
        path.to_string()
            .bytes()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    };
    let get = |query: String| {
        let (status, body) = local_get(&port, &format!("/canvas/{canvas}?{query}"), None);
        let value = serde_json::from_slice(&body).unwrap_or(serde_json::Value::Null);
        (status, value)
    };

    assert_eq!(get(String::new()).0, 403, "it needs the token");
    let (status, top) = get(format!("t={}&v={serial}", info.token));
    assert_eq!(status, 200);
    let mut folders: Vec<&str> = top["folders"]
        .as_array()
        .expect("folders")
        .iter()
        .map(|folder| folder["name"].as_str().expect("name"))
        .collect();
    folders.sort_unstable();
    assert_eq!(folders, ["Lost folder 40", "sub"]);
    let files: Vec<&str> = top["files"]
        .as_array()
        .expect("files")
        .iter()
        .map(|file| file["name"].as_str().expect("name"))
        .collect();
    assert_eq!(files, ["a1.jpg", "b1.mp3", "SHARED.jpg"]);

    // A folder is named by its path names, and a lost one by its path name.
    let (_, lost) = get(format!(
        "t={}&v={serial}&p={}",
        info.token,
        hex(json!(["lost:40"]))
    ));
    assert_eq!(lost["files"][0]["name"], "c.mp3");
    let (status, _) = get(format!(
        "t={}&v={serial}&p={}",
        info.token,
        hex(json!(["nowhere"]))
    ));
    assert_eq!(status, 404);

    // The chosen copy's bytes come from the file route, and only while this disk is open.
    let shared = top["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["name"] == "SHARED.jpg")
        .expect("shared")["id"]
        .as_i64()
        .expect("id");
    let (status, body) = local_get(
        &port,
        &format!("/file/{shared}?t={}&v={serial}", info.token),
        None,
    );
    assert_eq!(status, 200);
    assert_eq!(&body[..4], &[0xFF, 0xD8, 0xFF, 0xE0]);
    assert_eq!(get(format!("t={}&v=00000000", info.token)).0, 409);
    assert_eq!(
        local_get(
            &port,
            &format!("/file/{shared}?t={}&v=00000000", info.token),
            None
        )
        .0,
        409
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn a_canvas_survives_a_restart_a_new_scan_and_a_map_rebuild() {
    let (engine, session, dir) = engine_with_image("canvas-keeps", build_jigsaw_volume());
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Weeks of work"}))["id"]
        .as_i64()
        .expect("id");
    let root = call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20}),
    )["id"]
        .as_i64()
        .expect("root");
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 30, "merge": true}),
    );
    let made = call_as_person(
        &engine,
        "canvas_folder",
        json!({"canvas": canvas, "name": "Music", "place": {"item": root}}),
    )["id"]
        .as_i64()
        .expect("folder");
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 40, "place": {"item": made}}),
    );
    // The person prefers the wiped copy of the clashing file, for their own reasons.
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    let wiped = top["files"]
        .as_array()
        .expect("files")
        .iter()
        .flat_map(|file| file["copies"].as_array().expect("copies").clone())
        .find(|copy| copy["firstCluster"] == 23)
        .expect("wiped")["id"]
        .clone();
    call_as_person(
        &engine,
        "canvas_choose",
        json!({"canvas": canvas, "path": ["SHARED.jpg"], "entry": wiped}),
    );
    call(
        &engine,
        "canvas_suggest",
        json!({"canvas": canvas, "head": 50, "place": {"item": made}, "note": "More music."}),
    );
    let before = (
        call(&engine, "canvas_tree", json!({"canvas": canvas})),
        call(
            &engine,
            "canvas_tree",
            json!({"canvas": canvas, "path": ["Music"]}),
        ),
        call(&engine, "canvas_suggestions", json!({"canvas": canvas})),
    );
    let image = build_jigsaw_volume();
    drop(engine);
    drop(session);

    // Weeks later: the app starts again on the same workspace, scans again and rebuilds the map.
    let workspace = Workspace::at(&dir).expect("workspace");
    let session = Session::with_source(
        Path::new("/test/image"),
        Arc::new(MemorySource::new(image)),
        workspace,
    )
    .expect("session");
    scan(&session.volume, &session.workspace);
    session.resolve().expect("rebuild the map");
    let engine = Engine::new();
    engine.set_session(session.clone());

    let after = (
        call(&engine, "canvas_tree", json!({"canvas": canvas})),
        call(
            &engine,
            "canvas_tree",
            json!({"canvas": canvas, "path": ["Music"]}),
        ),
        call(&engine, "canvas_suggestions", json!({"canvas": canvas})),
    );
    assert_eq!(
        after.0, before.0,
        "the top of the tree, with the person's choice"
    );
    assert_eq!(
        after.1, before.1,
        "the folder they made, with what they put in it"
    );
    assert_eq!(after.2, before.2, "the suggestion still waiting for them");
    let shared = after.0["files"]
        .as_array()
        .expect("files")
        .iter()
        .find(|file| file["byPerson"] == true)
        .expect("their choice");
    assert_eq!(shared["chosen"]["firstCluster"], 23);
    assert_eq!(
        call(&engine, "canvas_list", json!({}))[0]["name"],
        "Weeks of work"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn the_old_app_folder_moves_to_the_real_name_whole_and_only_once() {
    use salvage_core::workspace::app_folder_in;
    let support = std::env::temp_dir().join(format!("salvage-support-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&support);
    let old = support.join("exfat-lens");
    std::fs::create_dir_all(old.join("vol-12345678")).expect("old folder");
    std::fs::write(old.join("vol-12345678/map.sqlite"), b"weeks of work").expect("map");
    std::fs::write(old.join("theme"), b"dark").expect("theme");

    let folder = app_folder_in(&support).expect("folder");
    assert_eq!(folder, support.join("Captain Salvage"));
    assert_eq!(
        std::fs::read(folder.join("vol-12345678/map.sqlite")).expect("moved map"),
        b"weeks of work"
    );
    // The old name still leads to the same files, for a copy of the app from before.
    assert!(std::fs::symlink_metadata(&old)
        .expect("link")
        .file_type()
        .is_symlink());
    assert_eq!(
        std::fs::read(old.join("theme")).expect("through link"),
        b"dark"
    );

    // Once moved, nothing moves again, and a folder already under the real name is never replaced.
    std::fs::write(folder.join("theme"), b"light").expect("theme");
    assert_eq!(app_folder_in(&support).expect("again"), folder);
    assert_eq!(std::fs::read(folder.join("theme")).expect("kept"), b"light");

    let both = std::env::temp_dir().join(format!("salvage-support-both-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&both);
    std::fs::create_dir_all(both.join("exfat-lens")).expect("old");
    std::fs::create_dir_all(both.join("Captain Salvage")).expect("new");
    std::fs::write(both.join("Captain Salvage/theme"), b"new").expect("new theme");
    app_folder_in(&both).expect("both");
    assert!(
        both.join("exfat-lens").is_dir(),
        "an old folder beside a new one is left alone"
    );
    assert_eq!(
        std::fs::read(both.join("Captain Salvage/theme")).expect("new"),
        b"new"
    );

    let _ = std::fs::remove_dir_all(&support);
    let _ = std::fs::remove_dir_all(&both);
}

#[test]
fn a_copy_started_under_the_old_manifest_name_keeps_it() {
    use salvage_core::export::{manifest_path, MANIFEST_NAME};
    let destination = copy_destination("old-manifest");
    std::fs::create_dir_all(&destination).expect("destination");
    assert_eq!(manifest_path(&destination), destination.join(MANIFEST_NAME));
    std::fs::write(destination.join("exfat-lens-manifest.jsonl"), b"").expect("old");
    assert_eq!(
        manifest_path(&destination),
        destination.join("exfat-lens-manifest.jsonl")
    );
    let _ = std::fs::remove_dir_all(&destination);
}

/// Everything the window can do through the engine, an MCP client can find too: each engine
/// tool the window calls is listed over MCP, unless it is only the person's to do, or only
/// means something inside the window.
#[test]
fn every_engine_tool_the_window_uses_is_listed_over_mcp() {
    // Writing files, choosing where they go, and arranging a canvas stay the person's.
    const PERSON_ONLY: &[&str] = &[
        "basket_start",
        "canvas_choose",
        "canvas_copy_start",
        "canvas_create",
        "canvas_delete",
        "canvas_folder",
        "canvas_leftovers",
        "canvas_move",
        "canvas_place",
        "canvas_place_file",
        "canvas_remove",
        "canvas_rename",
        "canvas_rename_item",
        "canvas_swap",
        "copy_start",
        "copy_stop",
        "set_salvage_drive",
    ];
    // The window tells the engine what is selected. An agent reads it with get_selection, and
    // moves the window with show_in_app.
    const WINDOW_ONLY: &[&str] = &["set_selection"];

    fn calls_in(folder: &std::path::Path, found: &mut std::collections::BTreeSet<String>) {
        for entry in std::fs::read_dir(folder).expect("source folder") {
            let path = entry.expect("entry").path();
            if path.is_dir() {
                calls_in(&path, found);
            } else if path
                .extension()
                .is_some_and(|ext| ext == "ts" || ext == "tsx")
            {
                let text = std::fs::read_to_string(&path).expect("source");
                for (index, _) in text.match_indices("call") {
                    let rest = &text[index + 4..];
                    // call("name" or call<Type>("name"
                    let rest = rest.strip_prefix('<').map_or(rest, |typed| {
                        typed.find(">(").map_or("", |end| &typed[end + 1..])
                    });
                    if let Some(name) = rest.strip_prefix("(\"") {
                        let name: String = name
                            .chars()
                            .take_while(|c| c.is_ascii_lowercase() || *c == '_')
                            .collect();
                        if !name.is_empty() {
                            found.insert(name);
                        }
                    }
                }
            }
        }
    }
    let window = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../src");
    let mut used = std::collections::BTreeSet::new();
    calls_in(&window, &mut used);
    assert!(
        used.contains("basket_add"),
        "the window's calls were not found"
    );
    let listed: std::collections::BTreeSet<String> = tool_definitions()
        .iter()
        .filter_map(|tool| tool["name"].as_str().map(str::to_string))
        .collect();
    let missing: Vec<&String> = used
        .iter()
        .filter(|name| !listed.contains(*name))
        .filter(|name| {
            !PERSON_ONLY.contains(&name.as_str()) && !WINDOW_ONLY.contains(&name.as_str())
        })
        .collect();
    assert!(
        missing.is_empty(),
        "the window uses these, but MCP does not list them: {missing:?}"
    );
}

/// A PNG left in free space, with no entry and no chain, is found by carving, and its end
/// is read from its own chunks.
#[test]
fn a_lost_png_in_free_space_is_carved_with_its_measured_length() {
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    for (kind, body) in [
        (&b"IHDR"[..], vec![0u8; 13]),
        (b"IDAT", vec![7u8; 6000]),
        (b"IEND", Vec::new()),
    ] {
        png.extend_from_slice(&(body.len() as u32).to_be_bytes());
        png.extend_from_slice(kind);
        png.extend_from_slice(&body);
        png.extend_from_slice(&[0u8; 4]);
    }
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
        ],
    );
    // Not allocated and named by nothing: a deleted file whose entry is gone.
    b.put(40, 0, &png);
    let (engine, _session, dir) = engine_with_image("carve", b.image);
    let page = call(&engine, "carve_candidates", json!({}));
    let found: Vec<&serde_json::Value> = page["candidates"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|candidate| candidate["kind"] == "png")
        .collect();
    assert_eq!(found.len(), 1, "{page}");
    assert_eq!(found[0]["startCluster"], 40);
    assert_eq!(found[0]["bytes"], png.len() as u64);
    assert_eq!(found[0]["endBy"], "structure");
    let _ = std::fs::remove_dir_all(dir);
}

/// Two pieces laid over one place each hold a different song.mp3 that measures the same:
/// the upper layer's copy is picked, and the clash is marked for the person to look at.
#[test]
fn clashing_copies_that_measure_the_same_go_to_the_upper_layer_and_ask_the_person() {
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
        ],
    );
    for (folder, song) in [(20u32, 21u32), (30, 31), (40, 41)] {
        b.directory(
            folder,
            &[entry_set(
                "song.mp3",
                false,
                song,
                CLUSTER as u64,
                true,
                true,
            )],
        );
        b.put(song, 0, b"ID3\x04");
        b.put(song, 16, &noise(song, CLUSTER - 16));
    }
    let (engine, _session, dir) = engine_with_image("layers", b.image);
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Layers"}))["id"]
        .as_i64()
        .expect("id");
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20}),
    );
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 30, "merge": true}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    let song = &top["files"][0];
    assert_eq!(song["copies"].as_array().expect("copies").len(), 2);
    assert_eq!(
        song["chosen"]["firstCluster"], 31,
        "the later drop is the upper layer"
    );
    assert_eq!(song["decidedBy"], "layer");
    assert_eq!(song["needsYou"], true);
    assert_eq!(top["clashesNeedYou"], 1);
    assert_eq!(top["clashesPicked"], 0);

    // A third layer on top takes the pick again.
    call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 40, "merge": true}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    assert_eq!(top["files"][0]["chosen"]["firstCluster"], 41);
    assert_eq!(
        top["files"][0]["copies"].as_array().expect("copies").len(),
        3
    );

    // The whole canvas's clashes, those needing the person first.
    let clashes = call(
        &engine,
        "canvas_clashes",
        json!({"canvas": canvas, "needYou": true}),
    );
    assert_eq!(clashes["needYou"], 1);
    assert_eq!(clashes["clashes"][0]["path"], json!(["song.mp3"]));

    // The person's pick ends the question.
    let lower = top["files"][0]["copies"]
        .as_array()
        .expect("copies")
        .iter()
        .find(|copy| copy["firstCluster"] == 21)
        .expect("lowest")["id"]
        .clone();
    call_as_person(
        &engine,
        "canvas_choose",
        json!({"canvas": canvas, "path": ["song.mp3"], "entry": lower}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    assert_eq!(top["files"][0]["decidedBy"], "person");
    assert_eq!(top["clashesNeedYou"], 0);
    let _ = std::fs::remove_dir_all(dir);
}

/// One file from a piece can be placed on its own, and takes its place as the upper layer.
#[test]
fn a_single_file_can_be_laid_over_a_folder_as_its_own_layer() {
    let mut b = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
        b.allocate(cluster);
    }
    b.fat(ROOT_CLUSTER, FAT_END);
    b.directory(
        ROOT_CLUSTER,
        &[
            special(0x81, BITMAP_CLUSTER, u64::from(CLUSTER_COUNT).div_ceil(8)),
            special(0x82, 4, 100),
        ],
    );
    for (folder, song) in [(20u32, 21u32), (30, 31)] {
        b.directory(
            folder,
            &[entry_set(
                "song.mp3",
                false,
                song,
                CLUSTER as u64,
                true,
                true,
            )],
        );
        b.put(song, 0, b"ID3\x04");
        b.put(song, 16, &noise(song, CLUSTER - 16));
    }
    let (engine, _session, dir) = engine_with_image("file-layer", b.image);
    let canvas = call_as_person(&engine, "canvas_create", json!({"name": "Files"}))["id"]
        .as_i64()
        .expect("id");
    let root = call_as_person(
        &engine,
        "canvas_place",
        json!({"canvas": canvas, "head": 20}),
    )["id"]
        .as_i64()
        .expect("root");
    let found = call(&engine, "search", json!({"query": "song.mp3"}));
    let other = found["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["firstCluster"] == 31)
        .expect("the copy in piece 30")["id"]
        .clone();
    assert!(refusal(
        &engine,
        "canvas_place_file",
        json!({"canvas": canvas, "entry": 999_999, "place": {"item": root}})
    )
    .contains("no file"));
    call_as_person(
        &engine,
        "canvas_place_file",
        json!({"canvas": canvas, "entry": other, "place": {"item": root}}),
    );
    let top = call(&engine, "canvas_tree", json!({"canvas": canvas}));
    assert_eq!(
        top["files"][0]["copies"].as_array().expect("copies").len(),
        2
    );
    assert_eq!(
        top["files"][0]["chosen"]["firstCluster"], 31,
        "the file placed later is on top"
    );
    assert_eq!(top["files"][0]["decidedBy"], "layer");
    let _ = std::fs::remove_dir_all(dir);
}
