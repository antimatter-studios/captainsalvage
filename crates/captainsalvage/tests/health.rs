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
        RangeState::ResetCulprit,
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
        map["resetCulprit"]["ranges"], 1,
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
