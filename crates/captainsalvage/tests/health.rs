//! The health tools through the engine, on a synthetic image: the map, the probe, forgiving a
//! range, and what opening does with a range an earlier run marked. Synthetic images only; the
//! sources never change, and everything is kept in a throwaway home folder.

use captainsalvage::engine::{Engine, AUTHOR_CLAUDE, AUTHOR_USER};
use captainsalvage::health::{source_key, HealthMap, HealthRange, HealthStore, RangeState};
use captainsalvage::source::FileSource;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::OnceLock;

const MIB: u64 = 1024 * 1024;

fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path =
            std::env::temp_dir().join(format!("captainsalvage-health-home-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("HOME", &path);
        path
    })
}

fn noise(length: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

struct Image(PathBuf, u64);

impl Image {
    fn new(label: &str, seed: u64) -> Self {
        home();
        let data = noise(8 * MIB as usize, seed);
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-health-{}-{label}.img",
            std::process::id()
        ));
        std::fs::write(&path, &data).unwrap();
        Self(path, data.len() as u64)
    }

    /// The key the app gives this image in the read journal.
    fn key(&self) -> String {
        source_key(&FileSource::open(&self.0).unwrap(), self.1)
    }
}

impl Drop for Image {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

fn open(engine: &Engine, image: &Image) -> String {
    let opened = call(engine, "open_raw_disk", json!({"path": image.0}));
    opened["id"].as_str().unwrap().to_string()
}

#[test]
fn a_probe_through_the_tools_reads_a_grid_and_keeps_where_it_is() {
    let image = Image::new("probe", 1);
    let engine = Engine::default();
    let disk = open(&engine, &image);
    let first = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "gridBytes": MIB, "maxReads": 3}),
    );
    assert_eq!(first["step"]["reads"], 3);
    assert_eq!(first["step"]["finished"], false);
    assert_eq!(first["probe"]["nextPoint"], 3);
    let status = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "action": "status"}),
    );
    assert_eq!(status["probe"]["reads"], 3, "status reads nothing");
    let finished = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "maxReads": 200}),
    );
    assert_eq!(finished["step"]["finished"], true);
    assert_eq!(finished["resetCulprit"]["ranges"], 0);
    let map = call(
        &engine,
        "health_map",
        json!({"diskId": disk, "includeJournal": true}),
    );
    assert!(map["good"]["bytes"].as_u64().unwrap() >= 8 * 4096, "{map}");
    assert!(
        map["journal"].as_array().unwrap().len() >= 8,
        "the probe's reads are in the journal"
    );
    assert!(map["journal"]
        .as_array()
        .unwrap()
        .iter()
        .all(|row| row["outcome"] == "ok"));
}

#[test]
fn a_range_an_earlier_run_marked_is_not_read_by_opening_and_markers_move_out_of_it() {
    let image = Image::new("marked", 2);
    let store = HealthStore::for_user().unwrap();
    // The fifth marker's block, and a stretch round it, were being read when the device reset.
    let nominal = (u128::from(image.1 - 4096) * 5 / 19) as u64 / 4096 * 4096;
    let range = HealthRange::new(
        nominal - MIB / 4,
        nominal + MIB / 4,
        RangeState::Unreadable,
        None,
        "a test marked it",
    );
    store
        .save_map(
            &image.key(),
            None,
            &HealthMap::from_ranges(vec![range.clone()]),
        )
        .unwrap();

    let engine = Engine::default();
    let disk = open(&engine, &image);
    let map = call(&engine, "health_map", json!({"diskId": disk}));
    assert_eq!(
        map["unreadable"]["ranges"], 1,
        "the map of the drive is loaded on opening"
    );

    let refused = engine
        .call(
            "read_raw_disk",
            &json!({"diskId": disk, "offset": nominal, "length": 512}),
            AUTHOR_USER,
        )
        .expect_err("a marked range is not read");
    assert!(format!("{refused:#}").contains("not read"), "{refused:#}");
    let beside = call(
        &engine,
        "read_raw_disk",
        json!({"diskId": disk, "offset": range.end, "length": 512}),
    );
    assert!(beside.is_object());

    let identified = call(&engine, "identify_source", json!({"diskId": disk}));
    assert_eq!(
        identified["fingerprint"]["movedForBadZones"],
        json!([nominal])
    );
    let offsets: Vec<u64> = identified["fingerprint"]["samples"]
        .as_array()
        .unwrap()
        .iter()
        .map(|sample| sample["offset"].as_u64().unwrap())
        .collect();
    assert!(
        offsets
            .iter()
            .all(|offset| *offset + 4096 <= range.start || *offset >= range.end),
        "{offsets:?}"
    );
    assert!(offsets.len() >= 19);
    // Opening it as a source does not read the range either, and says nothing is wrong.
    let opened = call(&engine, "open_source", json!({"diskId": disk}));
    assert!(opened["disk"].is_object());
}

#[test]
fn only_the_person_forgives_a_range_and_a_live_source_is_not_rebound() {
    let image = Image::new("forgive", 3);
    HealthStore::for_user()
        .unwrap()
        .save_map(
            &image.key(),
            None,
            &HealthMap::from_ranges(vec![HealthRange::new(
                2 * MIB,
                3 * MIB,
                RangeState::Unreadable,
                None,
                "a test",
            )]),
        )
        .unwrap();
    let engine = Engine::default();
    let disk = open(&engine, &image);
    let args = json!({"diskId": disk, "action": "forgive", "start": 2 * MIB, "end": 3 * MIB});
    let refused = engine
        .call("probe_health", &args, AUTHOR_CLAUDE)
        .expect_err("an agent may not forgive");
    assert!(
        format!("{refused:#}").contains("Only the person"),
        "{refused:#}"
    );
    assert_eq!(
        call(&engine, "health_map", json!({"diskId": disk}))["unreadable"]["ranges"],
        1
    );
    call(&engine, "probe_health", args);
    assert_eq!(
        call(&engine, "health_map", json!({"diskId": disk}))["unreadable"]["ranges"],
        0
    );

    let still = engine
        .call(
            "rebind_source",
            &json!({"diskId": disk, "path": image.0}),
            AUTHOR_USER,
        )
        .expect_err("a device that answers is not rebound");
    let message = format!("{still:#}");
    assert!(
        message.contains("markers") || message.contains("still answers"),
        "{message}"
    );
}

#[test]
fn a_probe_that_was_stopped_and_a_source_without_a_probe_say_so() {
    let image = Image::new("stop", 4);
    let engine = Engine::default();
    let disk = open(&engine, &image);
    call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "gridBytes": MIB, "maxReads": 2}),
    );
    let stopped = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "action": "stop"}),
    );
    assert_eq!(stopped["stopped"], true);
    assert!(stopped["probe"].is_null());
    let error = engine
        .call(
            "probe_health",
            &json!({"diskId": disk, "action": "step"}),
            AUTHOR_USER,
        )
        .expect_err("nothing to continue");
    assert!(
        format!("{error:#}").contains("start one first"),
        "{error:#}"
    );
}

#[test]
fn a_suspect_band_is_not_read_the_failing_flag_is_shown_and_only_the_person_clears_it() {
    let image = Image::new("suspect", 5);
    let store = HealthStore::for_user().unwrap();
    store
        .save_map(
            &image.key(),
            None,
            &HealthMap::from_ranges(vec![HealthRange::new(
                2 * MIB,
                4 * MIB,
                RangeState::Suspect,
                None,
                "a probe jumped over it",
            )]),
        )
        .unwrap();
    // Two resets in an earlier run make it a failing drive.
    store.add_resets(&image.key(), 2).unwrap();
    let engine = Engine::default();
    let disk = open(&engine, &image);
    let map = call(&engine, "health_map", json!({"diskId": disk}));
    assert_eq!(map["suspect"]["ranges"], 1);
    assert_eq!(
        (map["resets"].clone(), map["failing"].clone()),
        (json!(2), json!(true))
    );
    let refused = engine
        .call(
            "read_raw_disk",
            &json!({"diskId": disk, "offset": 3 * MIB, "length": 512}),
            AUTHOR_USER,
        )
        .expect_err("a suspect band is not read");
    assert!(format!("{refused:#}").contains("suspect"), "{refused:#}");

    let unflag = json!({"diskId": disk, "action": "unflag"});
    assert!(
        engine.call("probe_health", &unflag, AUTHOR_CLAUDE).is_err(),
        "an agent may not clear the count"
    );
    let cleared = call(&engine, "probe_health", unflag);
    assert_eq!(
        (cleared["resets"].clone(), cleared["failing"].clone()),
        (json!(0), json!(false))
    );
    // The band is still there until it is forgiven.
    assert_eq!(
        call(&engine, "health_map", json!({"diskId": disk}))["suspect"]["ranges"],
        1
    );
    call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "action": "forgive", "start": 2 * MIB, "end": 4 * MIB}),
    );
    assert_eq!(
        call(&engine, "health_map", json!({"diskId": disk}))["suspect"]["ranges"],
        0
    );
    let freed = call(
        &engine,
        "read_raw_disk",
        json!({"diskId": disk, "offset": 3 * MIB, "length": 512}),
    );
    assert!(freed.is_object());
}

#[test]
fn the_edges_wait_for_the_person_through_the_tools() {
    let image = Image::new("edges", 6);
    let engine = Engine::default();
    let disk = open(&engine, &image);
    // Nothing is bad on a healthy image: the grid finishes and there is nothing to map.
    let done = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "gridBytes": MIB, "skipBytes": 2 * MIB, "maxReads": 100}),
    );
    assert_eq!(done["step"]["finished"], true);
    assert_eq!(done["step"]["bands"], 0);
    let edges = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "action": "edges"}),
    );
    assert_eq!(
        edges["step"]["finished"], true,
        "edges with nothing to map is harmless"
    );
}

/// A whole disk image (an MBR and one partition from 1 MiB, 4 MiB long) and the partition's bytes on their own,
/// as the same drive under two device nodes.
fn blocked(engine: &Engine, disk: &str) -> Vec<(u64, u64)> {
    call(engine, "health_map", json!({"diskId": disk}))["ranges"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|range| {
            matches!(
                range["state"].as_str().unwrap(),
                "reset-culprit" | "unreadable" | "suspect"
            )
        })
        .map(|range| {
            (
                range["start"].as_u64().unwrap(),
                range["end"].as_u64().unwrap(),
            )
        })
        .collect()
}

fn disk_and_partition(label: &str, seed: u64) -> (Image, Image) {
    home();
    let mut data = noise(8 * MIB as usize, seed);
    data[..512].fill(0);
    // An MBR disk signature of its own, as every disk has, so two of these are two disks.
    data[440..444].copy_from_slice(&(seed as u32).to_le_bytes());
    data[446 + 4] = 0x07;
    data[446 + 8..446 + 12].copy_from_slice(&((MIB / 512) as u32).to_le_bytes());
    data[446 + 12..446 + 16].copy_from_slice(&((4 * MIB / 512) as u32).to_le_bytes());
    data[510..512].copy_from_slice(&[0x55, 0xAA]);
    let part = data[MIB as usize..5 * MIB as usize].to_vec();
    let write = |name: &str, bytes: &[u8]| {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-health-{}-{label}-{name}.img",
            std::process::id()
        ));
        std::fs::write(&path, bytes).unwrap();
        Image(path, bytes.len() as u64)
    };
    (write("disk", &data), write("part", &part))
}

#[test]
fn a_zone_marked_through_the_whole_disk_is_avoided_through_its_partition_and_the_other_way_round() {
    let (disk_image, part_image) = disk_and_partition("share", 11);
    // The whole disk is opened first: it says which stretch is the partition.
    let first = Engine::default();
    let disk = open(&first, &disk_image);
    let marked = call(
        &first,
        "probe_health",
        json!({"diskId": disk, "action": "mark_bad", "start": 3 * MIB, "end": 3 * MIB + MIB / 2, "why": "dd from 3 MiB dropped the drive"}),
    );
    assert_eq!(
        marked["marked"]["note"],
        "marked by you: dd from 3 MiB dropped the drive"
    );
    drop(first);

    // The partition opened alone, by its own bytes, finds the same zone, 2 MiB into it.
    let second = Engine::default();
    let part = open(&second, &part_image);
    assert_eq!(blocked(&second, &part), vec![(2 * MIB, 2 * MIB + MIB / 2)]);
    let refused = second
        .call(
            "read_raw_disk",
            &json!({"diskId": part, "offset": 2 * MIB + 4096, "length": 512}),
            AUTHOR_USER,
        )
        .expect_err("avoided through the partition too");
    assert!(format!("{refused:#}").contains("not read"), "{refused:#}");

    // A zone marked through the partition (1 MiB into it) is the disk's 2 MiB, seen from the whole disk.
    call(
        &second,
        "probe_health",
        json!({"diskId": part, "action": "mark_bad", "start": MIB, "end": MIB + 4096, "state": "unreadable", "why": "found through the partition"}),
    );
    drop(second);
    let third = Engine::default();
    let disk = open(&third, &disk_image);
    let starts: Vec<u64> = blocked(&third, &disk).iter().map(|range| range.0).collect();
    assert_eq!(starts, vec![2 * MIB, 3 * MIB], "{starts:?}");
}

#[test]
fn what_a_partition_found_alone_moves_into_the_disk_when_the_disk_is_opened_and_the_reset_count_is_the_disks(
) {
    let (disk_image, part_image) = disk_and_partition("merge", 12);
    let alone = Engine::default();
    let part = open(&alone, &part_image);
    call(
        &alone,
        "probe_health",
        json!({"diskId": part, "action": "mark_bad", "start": 2 * MIB, "end": 3 * MIB, "why": "found with the partition opened alone"}),
    );
    HealthStore::for_user()
        .unwrap()
        .add_resets(&part_image.key(), 1)
        .unwrap();
    drop(alone);
    let whole = Engine::default();
    let disk = open(&whole, &disk_image);
    assert_eq!(
        blocked(&whole, &disk)[0].0,
        3 * MIB,
        "the partition's 2 MiB is the disk's 3 MiB"
    );
    let map = call(&whole, "health_map", json!({"diskId": disk}));
    assert_eq!(
        map["resets"], 1,
        "the partition's reset count is the disk's now"
    );
    // A second reset, recorded under the disk's key, is a failing drive from the partition's node too.
    HealthStore::for_user()
        .unwrap()
        .add_resets(&disk_image.key(), 1)
        .unwrap();
    drop(whole);
    let again = Engine::default();
    let part = open(&again, &part_image);
    let map = call(&again, "health_map", json!({"diskId": part}));
    assert_eq!(
        (map["resets"].clone(), map["failing"].clone()),
        (json!(2), json!(true)),
        "{map}"
    );
}

#[test]
fn opening_a_disk_that_has_reset_reads_a_few_known_places_runs_no_analysis_and_says_so() {
    let image = Image::new("careful", 13);
    let engine = Engine::default();
    // Opened while well: the markers are stored.
    let disk = open(&engine, &image);
    let first = call(&engine, "open_source", json!({"diskId": disk}));
    assert!(
        first["carefulOpen"].is_null(),
        "nothing is wrong yet: {first}"
    );
    let stored = first["fingerprint"]["samples"].as_array().unwrap().len();
    assert!(stored >= 19);
    drop(engine);

    // The drive resets once somewhere: a zone is marked.
    HealthStore::for_user()
        .unwrap()
        .save_map(
            &image.key(),
            None,
            &HealthMap::from_ranges(vec![HealthRange::new(
                6 * MIB,
                6 * MIB + MIB / 2,
                RangeState::ResetCulprit,
                Some(108),
                "the device went away during a read of this range",
            )]),
        )
        .unwrap();
    let careful = Engine::default();
    let disk = open(&careful, &image);
    let opened = call(&careful, "open_source", json!({"diskId": disk}));
    let note = &opened["carefulOpen"];
    assert_eq!(note["analysisSkipped"], true, "{opened}");
    assert!(note["markersRead"].as_u64().unwrap() <= 5, "{note}");
    assert!(note["summary"]
        .as_str()
        .unwrap()
        .contains("did not read fresh markers"));
    // It is still the same source, found from the stored markers, and what is remembered of it is the full set.
    assert_eq!(opened["created"], false, "{opened}");
    assert_eq!(note["markersStored"].as_u64().unwrap() as usize, stored);
    // Analysis is for the person to ask for.
    let analysed = call(
        &careful,
        "analyze_range",
        json!({"diskId": disk, "offset": 0, "length": MIB, "step": 4096, "signatures": false}),
    );
    assert!(analysed.is_object());
}

#[test]
fn a_zone_is_marked_with_who_and_why_and_a_bad_request_is_refused() {
    let image = Image::new("markbad", 14);
    let engine = Engine::default();
    let disk = open(&engine, &image);
    let bad = |args: Value| {
        engine
            .call("probe_health", &args, AUTHOR_USER)
            .expect_err("refused")
    };
    assert!(format!(
        "{:#}",
        bad(json!({"diskId": disk, "action": "mark_bad", "start": MIB, "end": 2 * MIB}))
    )
    .contains("say why"));
    assert!(format!(
        "{:#}",
        bad(
            json!({"diskId": disk, "action": "mark_bad", "start": 2 * MIB, "end": MIB, "why": "x"})
        )
    )
    .contains("below end"));
    assert!(format!(
        "{:#}",
        bad(
            json!({"diskId": disk, "action": "mark_bad", "start": MIB, "end": 99 * MIB, "why": "x"})
        )
    )
    .contains("past the end"));
    assert!(format!("{:#}", bad(json!({"diskId": disk, "action": "mark_bad", "start": MIB, "end": 2 * MIB, "why": "x", "state": "good"}))).contains("state must be"));
    let person = call(
        &engine,
        "probe_health",
        json!({"diskId": disk, "action": "mark_bad", "start": 2 * MIB, "end": 2 * MIB + 4096, "why": "dd from 472,517,443,584 dropped the drive"}),
    );
    assert!(person["marked"]["note"]
        .as_str()
        .unwrap()
        .starts_with("marked by you: dd from"));
    let agent = engine
        .call("probe_health", &json!({"diskId": disk, "action": "mark_bad", "start": 6 * MIB, "end": 6 * MIB + 4096, "why": "an agent's dd run", "state": "suspect"}), AUTHOR_CLAUDE)
        .unwrap();
    assert!(agent["marked"]["note"]
        .as_str()
        .unwrap()
        .starts_with("marked by an agent:"));
    assert_eq!(blocked(&engine, &disk).len(), 2);
}
