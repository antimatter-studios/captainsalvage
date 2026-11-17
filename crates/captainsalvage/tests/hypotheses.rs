//! Hypotheses: an extent and a format saved with a confidence, competing as toggleable layers,
//! and the overlay of segments they make. Synthetic images only; the source never changes, and
//! the records go to a throwaway home folder, never the real app's.

use captainsalvage::engine::{Engine, AUTHOR_USER};
use fs_exfat::fixtures::{self, Builder};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::OnceLock;

/// Points HOME at a folder of this test run, once, before anything opens a store.
fn home() -> &'static PathBuf {
    static HOME: OnceLock<PathBuf> = OnceLock::new();
    HOME.get_or_init(|| {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-hypotheses-home-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        std::env::set_var("HOME", &path);
        path
    })
}

fn noise(length: usize, seed: u32) -> Vec<u8> {
    let mut state = seed | 1;
    (0..length)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            (state >> 8) as u8
        })
        .collect()
}

fn exfat() -> Vec<u8> {
    let mut builder = Builder {
        image: fixtures::build_volume(),
    };
    builder.image.resize(2 * 1024 * 1024, 0);
    builder.write_boot();
    builder.image
}

struct ImageFile(PathBuf);

impl ImageFile {
    fn new(label: &str, data: &[u8]) -> Self {
        let path = std::env::temp_dir().join(format!(
            "captainsalvage-hypotheses-{}-{label}.img",
            std::process::id()
        ));
        std::fs::write(&path, data).unwrap();
        Self(path)
    }
}

impl Drop for ImageFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

fn call(engine: &Engine, tool: &str, args: Value) -> Value {
    engine
        .call(tool, &args, AUTHOR_USER)
        .unwrap_or_else(|error| panic!("{tool}: {error:#}"))
}

fn open(engine: &Engine, file: &ImageFile) -> String {
    call(engine, "open_raw_disk", json!({"path": file.0}))["id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn hex_bytes(value: &Value) -> Vec<u8> {
    value["hex"]
        .as_str()
        .unwrap()
        .as_bytes()
        .chunks(2)
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect()
}

#[test]
fn a_finding_becomes_a_hypothesis_that_opens_competes_persists_and_never_writes() {
    home();
    let volume = exfat();
    let start = 3 * 512;
    let mut data = noise(volume.len() + 20_000, 11);
    data[start..start + volume.len()].copy_from_slice(&volume);
    let file = ImageFile::new("flow", &data);
    let before = std::fs::read(&file.0).unwrap();
    let engine = Engine::default();
    let disk = open(&engine, &file);

    // Analysis finds the volume; its extent and confidence become a hypothesis.
    let report = call(
        &engine,
        "analyze_range",
        json!({"diskId": disk, "offset": 0, "length": data.len()}),
    );
    let finding = report["findings"]
        .as_array()
        .unwrap()
        .iter()
        .find(|finding| finding["status"] == "volume")
        .unwrap()
        .clone();
    let saved = call(
        &engine,
        "save_hypothesis",
        json!({
            "diskId": disk,
            "format": finding["format"],
            "start": finding["volumeStart"],
            "length": finding["volumeLength"],
            "confidence": finding["confidence"],
            "bootOffset": finding["bootOffset"],
            "origin": "analysis",
            "label": "found by analysis",
        }),
    );
    let first = saved["saved"]["id"].as_str().unwrap().to_string();
    assert_eq!(saved["saved"]["start"], start);
    assert_eq!(saved["saved"]["origin"], "analysis");

    // A competing guess over the same bytes stays a separate layer on top.
    let second = call(&engine, "save_hypothesis", json!({
        "diskId": disk, "format": "NTFS", "start": start + 4096, "length": 100_000, "label": "maybe NTFS", "confidence": 0.2,
    }))["saved"]["id"].as_str().unwrap().to_string();
    let listed = call(&engine, "list_hypotheses", json!({"diskId": disk}));
    assert_eq!(listed["hypotheses"].as_array().unwrap().len(), 2);
    let competing = &listed["layout"]["competing"];
    assert_eq!(competing.as_array().unwrap().len(), 1);
    assert_eq!(competing[0]["stack"], json!([second, first]));

    // Switching the upper layer off lets the lower one be the only reading. Nothing is deleted.
    call(
        &engine,
        "update_hypothesis",
        json!({"diskId": disk, "id": second, "enabled": false}),
    );
    let listed = call(&engine, "list_hypotheses", json!({"diskId": disk}));
    assert!(listed["layout"]["competing"].as_array().unwrap().is_empty());
    assert_eq!(listed["hypotheses"].as_array().unwrap().len(), 2);
    call(
        &engine,
        "raise_hypothesis",
        json!({"diskId": disk, "id": first}),
    );

    // The start and end are typed: moving the end of the saved hypothesis changes the extent.
    let moved = call(
        &engine,
        "update_hypothesis",
        json!({"diskId": disk, "id": second, "start": 0, "length": 5000}),
    );
    assert_eq!(moved["changed"]["start"], 0);
    assert!(engine
        .call(
            "update_hypothesis",
            &json!({"diskId": disk, "id": second, "length": data.len() * 2}),
            AUTHOR_USER
        )
        .is_err());

    // The hypothesis opens as the volume it names, through the existing interpretation path.
    let opened = call(
        &engine,
        "open_hypothesis",
        json!({"diskId": disk, "id": first}),
    );
    assert_eq!(opened["filesystem"]["kind"], "exfat");

    // The overlay reads the enabled segments at their virtual positions.
    call(
        &engine,
        "update_hypothesis",
        json!({"diskId": disk, "id": second, "enabled": true, "virtualStart": 3_000_000, "format": "FAT16"}),
    );
    let identity = call(
        &engine,
        "read_layout",
        json!({"diskId": disk, "offset": start, "length": 512}),
    );
    assert_eq!(hex_bytes(&identity), data[start..start + 512]);
    let relocated = call(
        &engine,
        "read_layout",
        json!({"diskId": disk, "offset": 3_000_000, "length": 64}),
    );
    assert_eq!(hex_bytes(&relocated), data[..64]);
    // A byte no segment covers is refused, never returned as zeros.
    assert!(engine
        .call(
            "read_layout",
            &json!({"diskId": disk, "offset": 2_500_000, "length": 16}),
            AUTHOR_USER
        )
        .is_err());
    let table =
        &call(&engine, "list_hypotheses", json!({"diskId": disk}))["layout"]["partitionTable"];
    assert_eq!(table.as_array().unwrap().len(), 2);
    assert_eq!(table[0]["virtualStart"], start);

    // A second run of the app finds the same records: they are saved on this Mac.
    let fresh = Engine::default();
    let again = open(&fresh, &file);
    let persisted = call(&fresh, "list_hypotheses", json!({"diskId": again}));
    assert_eq!(persisted["hypotheses"].as_array().unwrap().len(), 2);

    // The command line does the same as the tool.
    let output = Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
        .args(["hypothesis", "list"])
        .arg(&file.0)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let command: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(command, persisted);

    let after = call(
        &engine,
        "delete_hypothesis",
        json!({"diskId": disk, "id": second}),
    );
    assert_eq!(after["hypotheses"].as_array().unwrap().len(), 1);
    assert_eq!(
        std::fs::read(&file.0).unwrap(),
        before,
        "the source must not change"
    );
}

#[test]
fn another_source_does_not_see_these_hypotheses_and_the_cli_writes_the_same_records() {
    home();
    let first = ImageFile::new("source-a", &noise(300_000, 21));
    let second = ImageFile::new("source-b", &noise(300_000, 22));
    let run = |image: &ImageFile, arguments: &[&str]| -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
            .arg("hypothesis")
            .arg(arguments[0])
            .arg(&image.0)
            .args(&arguments[1..])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    };
    let saved = run(
        &first,
        &[
            "save",
            "--format",
            "FAT32",
            "--start",
            "1024",
            "--length",
            "65536",
            "--confidence",
            "0.4",
            "--label",
            "cli guess",
        ],
    );
    assert_eq!(saved["saved"]["label"], "cli guess");
    let id = saved["saved"]["id"].as_str().unwrap().to_string();
    let updated = run(
        &first,
        &[
            "update",
            "--id",
            &id,
            "--start",
            "2048",
            "--disabled",
            "--boot-offset",
            "none",
        ],
    );
    assert_eq!(updated["changed"]["start"], 2048);
    assert_eq!(updated["changed"]["enabled"], false);
    assert_eq!(
        run(&first, &["list"])["hypotheses"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert!(run(&second, &["list"])["hypotheses"]
        .as_array()
        .unwrap()
        .is_empty());
}
