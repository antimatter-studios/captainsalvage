//! End-to-end tests against a hand-built exFAT volume with known damage.

use lens_core::content::{CLASS_DIRECTORY, CLASS_JPEG, CLASS_MP3, CLASS_ZEROS};
use lens_core::engine::{Engine, AUTHOR_CLAUDE};
use lens_core::exfat::{boot_checksum, FAT_END};
use lens_core::map::MapDb;
use lens_core::mcp::{handle_message, tool_definitions};
use lens_core::scan::{read_progress, run_scan, ScanOptions};
use lens_core::session::{whole_disk_path, Layer, Lens, Session};
use lens_core::source::MemorySource;
use lens_core::verify::{verify_mp3, verify_png};
use lens_core::volume::{Volume, ALLOC_ALLOCATED, ALLOC_FREE_FAT, ALLOC_FREE_ZERO};
use lens_core::workspace::Workspace;
use serde_json::json;
use std::path::Path;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
    let mut checksum = 0u16;
    for (index, &byte) in set.iter().enumerate() {
        if index == 2 || index == 3 {
            continue;
        }
        let value = if index % 32 == 0 { byte | 0x80 } else { byte };
        checksum = checksum.rotate_right(1).wrapping_add(u16::from(value));
    }
    set[2..4].copy_from_slice(&checksum.to_le_bytes());
    set
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
    let dir = std::env::temp_dir().join(format!("lens-test-{}-{label}", std::process::id()));
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
    let volume = open_volume();
    let (workspace, dir) = temp_workspace("owners");
    scan(&volume, &workspace);
    let database = MapDb::open(&workspace.database()).expect("database");

    let chained = database
        .owners(&volume, 33)
        .expect("owners of a chain's tail");
    assert!(chained.iter().any(|row| row.name == "track.mp3"));
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
    let first = workspace.check_baseline("aaaa").expect("first");
    assert!(first.unchanged && first.first_recording);
    let same = workspace.check_baseline("aaaa").expect("same");
    assert!(same.unchanged && !same.first_recording);
    let changed = workspace.check_baseline("bbbb").expect("changed");
    assert!(!changed.unchanged);
    let _ = std::fs::remove_dir_all(dir);
}

/// An engine with a scanned synthetic volume open, as the app would have it.
fn engine_with_volume(label: &str) -> (Arc<Engine>, Arc<Session>, PathBuf) {
    let (workspace, dir) = temp_workspace(label);
    let source = Arc::new(MemorySource::new(build_volume()));
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
    let offset = lens_core_cluster_offset(30);
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

fn lens_core_cluster_offset(cluster: u32) -> u64 {
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
    assert!(hits.contains(&lens_core_cluster_offset(30)));
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
    assert_eq!(init["result"]["serverInfo"]["name"], "exfat-lens");

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
    let report = lens_core::verify::verify_jpeg(&jpeg);
    assert!(report.reached_end, "{report:?}");
    assert_eq!(report.valid_fraction, 1.0);
    jpeg.truncate(jpeg.len() - 2);
    assert!(!lens_core::verify::verify_jpeg(&jpeg).reached_end);
}

#[test]
fn opening_the_same_volume_again_reuses_the_session() {
    let (engine, session, dir) = engine_with_volume("reopen");
    let again = engine.open("/test/image").expect("same path");
    assert!(Arc::ptr_eq(&session, &again));
    let _ = std::fs::remove_dir_all(dir);
}

fn copy_destination(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("lens-copy-{}-{label}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// Runs a copy as the user and waits for it to finish.
fn copy_and_wait(
    engine: &Engine,
    session: &Session,
    args: serde_json::Value,
) -> lens_core::export::CopyStatus {
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
    std::fs::read_to_string(destination.join(lens_core::export::MANIFEST_NAME))
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
    use lens_core::export::{destination_problem, parse_physical_disks};
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
    use lens_core::export::relative_target;
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
        lens_core::regions::ntfs_record_name(&record).as_deref(),
        Some("a.txt")
    );
    assert_eq!(lens_core::regions::ntfs_record_name(&[0u8; 1024]), None);
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
    let good = lens_core::verify::verify_zip(&stored_zip("a.txt", b"hello world", HELLO_CRC));
    assert_eq!(good.units, 1);
    assert_eq!(good.valid_fraction, 1.0, "{good:?}");
    assert!(good.breaks.is_empty());

    let bad = lens_core::verify::verify_zip(&stored_zip("a.txt", b"hello worlD", HELLO_CRC));
    assert_eq!(bad.breaks.len(), 1);
    assert_eq!(bad.valid_fraction, 0.0);
    assert!(
        bad.notes
            .iter()
            .any(|note| note.contains("a.txt fails its CRC")),
        "{bad:?}"
    );

    let truncated = stored_zip("a.txt", b"hello world", HELLO_CRC);
    let report = lens_core::verify::verify_zip(&truncated[..truncated.len() - 30]);
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
    let old = lens_core::export::CopyStatus {
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
