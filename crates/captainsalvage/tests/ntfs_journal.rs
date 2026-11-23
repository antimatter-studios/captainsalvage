//! An NTFS volume's change journal and `$LogFile`, read as a separate labelled interpretation and without writing the volume.
//!
//! The images are formatted by the `rust-fs-ntfs` driver, which also writes the journal's files into them here: `$UsnJrnl`
//! with its `$Max` and `$J` streams, the records encoded as `USN_RECORD_V2` is. The driver has no journal writer of its own, so
//! these records are the test's: what they prove is that the app reads them back from the stream, names their flags, pages
//! through them, and never writes. That Windows' own volumes read the same is graded against `fsutil` by the Windows guest
//! (`tests/ntfs_windows.rs`), and a log holding pending work is the driver's own Windows-captured images' to show: the log
//! of a volume formatted here is empty.
//!
//! Every read below is through a device that has `fs_core::BlockRead` and nothing else, and the image's hash is the same after.

use captainsalvage::engine::Engine;
use captainsalvage::ntfs_journal::{self, Request};
use fs_core::BlockRead;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

mod ntfs_support;
use ntfs_support::*;

/// Windows' FILETIME for 2026-03-01T12:00:00Z.
const TIME: i64 = (1_772_366_400 + 11_644_473_600) * 10_000_000;
const JOURNAL_ID: u64 = 0x01DC_0000_1234_5678;

/// A `USN_RECORD_V2`.
fn usn_record(usn: i64, segment: u64, parent: u64, reason: u32, name: &str) -> Vec<u8> {
    let units: Vec<u8> = name.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let length = (60 + units.len()).next_multiple_of(8);
    let mut record = vec![0u8; length];
    record[0..4].copy_from_slice(&(length as u32).to_le_bytes());
    record[4..6].copy_from_slice(&2u16.to_le_bytes());
    record[8..16].copy_from_slice(&((3u64 << 48) | segment).to_le_bytes());
    record[16..24].copy_from_slice(&((5u64 << 48) | parent).to_le_bytes());
    record[24..32].copy_from_slice(&usn.to_le_bytes());
    record[32..40].copy_from_slice(&(TIME + usn).to_le_bytes());
    record[40..44].copy_from_slice(&reason.to_le_bytes());
    record[52..56].copy_from_slice(&0x20u32.to_le_bytes());
    record[56..58].copy_from_slice(&(units.len() as u16).to_le_bytes());
    record[58..60].copy_from_slice(&60u16.to_le_bytes());
    record[60..60 + units.len()].copy_from_slice(&units);
    record
}

/// What the journal holds, in order: (segment, reason, name). A file is made, written and closed, renamed, and another is
/// deleted, with enough of them that `$J` does not fit in its file record.
fn history() -> Vec<(u64, u32, String)> {
    let mut found = vec![
        (64, 0x100, "report.txt".to_string()),
        (64, 0x102, "report.txt".to_string()),
        (64, 0x8000_0102, "report.txt".to_string()),
        (64, 0x1000, "report.txt".to_string()),
        (64, 0x2000, "report-final.txt".to_string()),
        (64, 0x8000_2000, "report-final.txt".to_string()),
        (65, 0x200, "scratch.tmp".to_string()),
        (65, 0x8000_0200, "scratch.tmp".to_string()),
    ];
    for number in 0..20 {
        found.push((70 + number, 0x100, format!("generated-{number:02}.dat")));
    }
    found
}

/// A formatted volume with a journal holding `history()`: the stream's own offset is each record's USN.
///
/// The driver does not change `$Extend` through its file API, so the journal's file is made in the root, with its two
/// streams, and then entered in `$Extend`'s index by the driver's own index code, where `$Extend\$UsnJrnl` is looked up.
fn with_journal() -> (Image, Vec<i64>) {
    let mut image = format();
    fs_ntfs::write::create_file_io(&mut image, "/", "UsnJrnl").unwrap();
    let mut max = Vec::new();
    for word in [1u64 << 25, 1 << 22, JOURNAL_ID, 0] {
        max.extend_from_slice(&word.to_le_bytes());
    }
    fs_ntfs::write::write_named_stream_io(&mut image, "/UsnJrnl", "$Max", &max).unwrap();
    let mut stream = Vec::new();
    let mut usns = Vec::new();
    for (segment, reason, name) in history() {
        let usn = stream.len() as i64;
        usns.push(usn);
        stream.extend_from_slice(&usn_record(usn, segment, 5, reason, &name));
    }
    fs_ntfs::write::write_named_stream_io(&mut image, "/UsnJrnl", "$J", &stream).unwrap();
    let number = fs_ntfs::read::resolve_path(&mut image, "/UsnJrnl").unwrap();
    let sequence = |image: &mut Image, number: u64| {
        let (_, record) = fs_ntfs::mft_io::read_mft_record_io(image, number).unwrap();
        u64::from(fs_ntfs::mft_io::record_sequence(&record))
    };
    let file = (sequence(&mut image, number) << 48) | number;
    let extend = (sequence(&mut image, 11) << 48) | 11;
    let entry =
        fs_ntfs::index_io::build_file_name_index_entry(file, extend, "$UsnJrnl", 0, false).unwrap();
    fs_ntfs::mft_io::update_mft_record_io(&mut image, 11, |record| {
        fs_ntfs::index_io::insert_entry_into_index_root(record, &entry, "$UsnJrnl")
    })
    .unwrap();
    (image, usns)
}

/// A device that can be read and nothing else, and counts its reads.
struct ReadOnly {
    bytes: Vec<u8>,
    reads: AtomicU64,
}

impl BlockRead for ReadOnly {
    fn read_at(&self, offset: u64, output: &mut [u8]) -> fs_core::Result<()> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        let start = offset as usize;
        let bytes =
            self.bytes
                .get(start..start + output.len())
                .ok_or(fs_core::Error::OutOfBounds {
                    offset,
                    len: output.len() as u64,
                    size: self.bytes.len() as u64,
                })?;
        output.copy_from_slice(bytes);
        Ok(())
    }
    fn size_bytes(&self) -> u64 {
        self.bytes.len() as u64
    }
}

fn read_only(image: &Image) -> ReadOnly {
    ReadOnly {
        bytes: image.0.lock().unwrap().clone(),
        reads: AtomicU64::new(0),
    }
}

#[test]
fn a_volume_with_no_journal_says_so_and_a_fresh_log_is_empty() {
    let device = read_only(&format());
    let before = sha(&device.bytes);
    let found = ntfs_journal::read(&device, &Request::default());
    assert_eq!(
        found.interpretation,
        "NTFS change journal and $LogFile evidence"
    );
    assert!(found.read_only);
    assert_eq!(found.usn_journal.state, "absent");
    assert!(found.usn_journal.journal.is_none() && found.usn_journal.records.is_empty());
    assert!(found.usn_journal.finished);
    assert!(
        matches!(found.log_file.state, "empty" | "clean"),
        "{:?}",
        found.log_file
    );
    assert!(!found.log_file.needs_replay);
    assert!(found.log_file.records.is_empty() || found.log_file.state == "clean");
    assert_eq!(sha(&device.bytes), before, "the volume is the same");
}

#[test]
fn the_journal_is_read_record_by_record_with_where_each_is_and_what_it_records() {
    let (image, usns) = with_journal();
    let device = read_only(&image);
    let before = sha(&device.bytes);
    let found = ntfs_journal::read(&device, &Request::default());
    let journal = &found.usn_journal;
    assert_eq!(journal.state, "active", "{journal:?}");
    let bounds = journal.journal.as_ref().unwrap();
    assert_eq!(bounds.journal_id, JOURNAL_ID);
    assert_eq!((bounds.first_usn, bounds.maximum_size), (0, 1 << 25));
    assert!(bounds.next_usn > 0 && journal.finished && journal.next_usn.is_none());
    assert_eq!(journal.records.len(), history().len());
    assert_eq!(
        journal
            .records
            .iter()
            .map(|record| record.usn)
            .collect::<Vec<_>>(),
        usns
    );
    // A record is where its USN says: its byte offset in `$J`.
    assert!(journal
        .records
        .iter()
        .all(|record| record.stream_offset == record.usn));
    let names: Vec<&str> = journal.records[..6]
        .iter()
        .map(|record| record.name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "report.txt",
            "report.txt",
            "report.txt",
            "report.txt",
            "report-final.txt",
            "report-final.txt"
        ]
    );
    assert_eq!(journal.records[0].reasons, ["FILE_CREATE"]);
    assert_eq!(journal.records[1].reasons, ["DATA_EXTEND", "FILE_CREATE"]);
    assert_eq!(
        journal.records[2].reasons,
        ["DATA_EXTEND", "FILE_CREATE", "CLOSE"]
    );
    assert_eq!(journal.records[3].reasons, ["RENAME_OLD_NAME"]);
    assert_eq!(journal.records[4].reasons, ["RENAME_NEW_NAME"]);
    assert_eq!(journal.records[6].reasons, ["FILE_DELETE"]);
    // The file and its parent, by MFT segment and sequence.
    assert_eq!(
        (
            journal.records[0].file_segment,
            journal.records[0].file_sequence,
            journal.records[0].parent_segment,
            journal.records[0].parent_sequence
        ),
        (64, 3, 5, 5)
    );
    assert_eq!(
        journal.records[0].iso_time.as_deref(),
        Some("2026-03-01T12:00:00Z")
    );
    assert!(device.reads.load(Ordering::Relaxed) > 0);
    assert_eq!(
        sha(&device.bytes),
        before,
        "reading the journal wrote nothing"
    );
}

#[test]
fn the_journal_is_paged_and_the_pages_are_the_whole_of_it() {
    let (image, usns) = with_journal();
    let device = read_only(&image);
    let whole = ntfs_journal::read(&device, &Request::default());
    let mut seen = Vec::new();
    let mut from = None;
    let mut pages = 0;
    loop {
        let page = ntfs_journal::read(
            &device,
            &Request {
                from_usn: from,
                usn_bytes: Some(300),
                ..Request::default()
            },
        );
        assert_eq!(page.usn_journal.state, "active");
        assert!(page.usn_journal.stopped.is_none(), "{:?}", page.usn_journal);
        seen.extend(page.usn_journal.records.iter().map(|record| record.usn));
        pages += 1;
        match page.usn_journal.next_usn {
            Some(next) => from = Some(next),
            None => break,
        }
        assert!(pages < 100, "the paging does not end");
    }
    assert!(pages > 1, "300 bytes cannot hold the whole journal");
    assert_eq!(seen, usns);
    assert_eq!(
        seen,
        whole
            .usn_journal
            .records
            .iter()
            .map(|record| record.usn)
            .collect::<Vec<_>>()
    );
}

#[test]
fn a_session_returns_the_interpretation_by_its_tool_and_never_writes_the_source() {
    let (image, _) = with_journal();
    let bytes = image.0.lock().unwrap().clone();
    let before = sha(&bytes);
    let session = open(bytes.clone(), "journal");
    let engine = Engine::new();
    engine.set_session(session.clone());
    let answer = call(&engine, "ntfs_journal_evidence", json!({}));
    assert_eq!(
        answer["interpretation"],
        "NTFS change journal and $LogFile evidence"
    );
    assert_eq!(answer["readOnly"], true);
    assert!(answer["caution"].as_str().unwrap().contains("not proof"));
    assert_eq!(answer["usnJournal"]["state"], "active");
    assert_eq!(
        answer["usnJournal"]["records"].as_array().unwrap().len(),
        history().len()
    );
    assert_eq!(
        answer["usnJournal"]["records"][4]["name"],
        "report-final.txt"
    );
    assert_eq!(
        answer["usnJournal"]["records"][4]["reasons"],
        json!(["RENAME_NEW_NAME"])
    );
    assert!(matches!(
        answer["logFile"]["state"].as_str().unwrap(),
        "empty" | "clean"
    ));
    // Paged through the tool, by the cursor it gave.
    let first = call(
        &engine,
        "ntfs_journal_evidence",
        json!({"maxUsnBytes": 300}),
    );
    let next = first["usnJournal"]["nextUsn"].as_i64().unwrap();
    let second = call(
        &engine,
        "ntfs_journal_evidence",
        json!({"fromUsn": next, "maxUsnBytes": 300}),
    );
    assert_eq!(second["usnJournal"]["records"][0]["usn"], next);
    // Nothing of it is an entry the scan lists, and the capability row still says the journal is missing.
    let table = call(&engine, "capabilities", json!({}));
    let row = table["capabilities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["feature"] == "journal")
        .cloned()
        .unwrap_or(Value::Null);
    assert_eq!(row["support"], "missing", "{row}");
    assert_eq!(
        sha(&bytes),
        before,
        "the image the session read is unchanged"
    );
}

#[test]
fn a_volume_that_is_not_ntfs_has_no_such_evidence() {
    let session = captainsalvage::session::Session::with_source(
        std::path::Path::new("/test/exfat-image"),
        Arc::new(captainsalvage::source::MemorySource::new(
            fs_exfat::fixtures::build_volume(),
        )),
        workspace("journal-exfat"),
    )
    .unwrap();
    let engine = Engine::new();
    engine.set_session(session);
    let error = engine
        .call(
            "ntfs_journal_evidence",
            &json!({}),
            captainsalvage::engine::AUTHOR_USER,
        )
        .unwrap_err();
    assert!(format!("{error:#}").contains("not NTFS"), "{error:#}");
}

#[test]
fn the_command_line_prints_the_same_interpretation_and_leaves_the_image_as_it_was() {
    let (image, usns) = with_journal();
    let bytes = image.0.lock().unwrap().clone();
    let path = std::env::temp_dir().join(format!(
        "captainsalvage-journal-cli-{}.img",
        std::process::id()
    ));
    std::fs::write(&path, &bytes).unwrap();
    let before = sha(&std::fs::read(&path).unwrap());
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_captainsalvage"))
        .args(["filesystem", "journal"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let printed: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        printed["interpretation"],
        "NTFS change journal and $LogFile evidence"
    );
    assert_eq!(printed["usnJournal"]["state"], "active");
    let listed: Vec<i64> = printed["usnJournal"]["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["usn"].as_i64().unwrap())
        .collect();
    assert_eq!(listed, usns);
    assert_eq!(sha(&std::fs::read(&path).unwrap()), before);
    let _ = std::fs::remove_file(&path);
}
