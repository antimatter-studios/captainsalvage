//! What the window's status poll costs (issue #254). The window asks for the app's status every two
//! seconds, so a status call that sorts a large table, starts a process or reads a disk makes opening a
//! large volume slow: the real map of a 590 GB volume holds hundreds of thousands of file test results, and
//! opening it waited behind those calls for a minute.
//!
//! The map here is made up: a few hundred thousand rows written into a temporary folder. No real map, disk or
//! app folder is opened.

use captainsalvage::filetest::{OUTCOME_DAMAGED, OUTCOME_PASSED, OUTCOME_UNTESTED};
use captainsalvage::session::Session;
use captainsalvage::source::MemorySource;
use captainsalvage::workspace::Workspace;
use fs_exfat::fixtures::{bitmap_entry, upcase_entry, Builder, BITMAP_CLUSTER, ROOT_CLUSTER};
use fs_exfat::primitives::{bitmap_location, BootSector, FAT_END};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// File test results in the made-up map.
const TESTS: u32 = 300_000;
/// What one status call may take, with a map of that size.
const STATUS_BUDGET: Duration = Duration::from_millis(50);
const POLLS: usize = 20;

/// The tests time calls, so they take turns.
static TURN: Mutex<()> = Mutex::new(());

fn volume() -> Vec<u8> {
    let mut builder = Builder::new();
    for cluster in [BITMAP_CLUSTER, ROOT_CLUSTER, 4] {
        builder.allocate(cluster);
    }
    builder.fat(ROOT_CLUSTER, FAT_END);
    let boot = BootSector::parse(&builder.image).unwrap();
    let location = bitmap_location(&boot, &bitmap_entry(BITMAP_CLUSTER, u64::MAX)).unwrap();
    builder.directory(
        ROOT_CLUSTER,
        &[
            bitmap_entry(location.first_cluster, location.required_length),
            upcase_entry(4, &[0; 100]),
        ],
    );
    builder.image
}

fn open(folder: &Path) -> Arc<Session> {
    Session::with_source(
        Path::new("/test/status-poll"),
        Arc::new(MemorySource::new(volume())),
        Workspace::at(folder).unwrap(),
    )
    .unwrap()
}

/// A map folder of its own with `TESTS` file test results in it, as a map made before the `outcome` index
/// existed (the index is dropped), and the counts the results add up to.
fn large_map(label: &str) -> (PathBuf, (u64, u64, u64)) {
    let folder = std::env::temp_dir().join(format!(
        "captainsalvage-status-poll-{}-{label}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&folder);
    drop(open(&folder));
    let mut connection = rusqlite::Connection::open(folder.join("map.sqlite")).unwrap();
    connection
        .execute("DROP INDEX IF EXISTS file_tests_outcome", [])
        .unwrap();
    let transaction = connection.transaction().unwrap();
    let mut counts = (0, 0, 0);
    {
        let mut insert = transaction
            .prepare(
                "INSERT INTO file_tests (id, first_cluster, length, outcome, format, valid_fraction, first_break, decoded, note, sha256, tested_at_millis) \
                 VALUES (?1, ?2, ?3, ?4, 'mp3', 1.0, NULL, NULL, 'a made-up result', ?5, 1730000000000)",
            )
            .unwrap();
        for id in 0..TESTS {
            let outcome = match id % 3 {
                0 => {
                    counts.0 += 1;
                    OUTCOME_PASSED
                }
                1 => {
                    counts.1 += 1;
                    OUTCOME_DAMAGED
                }
                _ => {
                    counts.2 += 1;
                    OUTCOME_UNTESTED
                }
            };
            insert
                .execute(rusqlite::params![
                    id,
                    100 + id,
                    4096,
                    outcome,
                    format!("{:064x}", u64::from(id) * 7919)
                ])
                .unwrap();
        }
    }
    transaction.commit().unwrap();
    (folder, counts)
}

#[test]
fn a_status_poll_over_a_large_map_is_fast_and_starts_no_process() {
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (folder, counts) = large_map("poll");
    let opened = Instant::now();
    let session = open(&folder);
    println!(
        "opening the session over {TESTS} file test results took {:?}",
        opened.elapsed()
    );
    let processes = captainsalvage::proc::processes_started();
    let mut slowest = Duration::ZERO;
    let mut total = Duration::ZERO;
    for poll in 0..POLLS {
        let started = Instant::now();
        let status = session.status().unwrap();
        let took = started.elapsed();
        slowest = slowest.max(took);
        total += took;
        assert_eq!(
            (
                status.file_tests.passed,
                status.file_tests.damaged,
                status.file_tests.untested
            ),
            counts,
            "poll {poll}"
        );
        assert!(
            took <= STATUS_BUDGET,
            "poll {poll} of status took {took:?}, over {STATUS_BUDGET:?}"
        );
    }
    println!("status over {TESTS} file test results: slowest {slowest:?}, average {:?} over {POLLS} polls", total / POLLS as u32);
    assert_eq!(
        captainsalvage::proc::processes_started(),
        processes,
        "a status poll started a process (diskutil, mount, ioreg)"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(folder);
}

#[test]
fn counting_the_results_sorts_nothing() {
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (folder, _) = large_map("plan");
    let session = open(&folder);
    let connection = rusqlite::Connection::open(folder.join("map.sqlite")).unwrap();
    let mut statement = connection
        .prepare(&format!(
            "EXPLAIN QUERY PLAN {}",
            captainsalvage::map::FILE_TEST_COUNTS_SQL
        ))
        .unwrap();
    let plan: Vec<String> = statement
        .query_map([], |row| row.get::<_, String>(3))
        .unwrap()
        .map(Result::unwrap)
        .collect();
    println!("query plan: {plan:?}");
    assert!(
        plan.iter().all(|step| !step.contains("TEMP B-TREE")),
        "the count sorts: {plan:?}"
    );
    assert!(
        plan.iter().any(|step| step.contains("COVERING INDEX")),
        "the count reads the table: {plan:?}"
    );
    drop(session);
    let _ = std::fs::remove_dir_all(folder);
}

#[test]
fn the_counts_follow_the_results_without_counting_again() {
    let _turn = TURN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let (folder, counts) = large_map("forget");
    let session = open(&folder);
    assert_eq!(session.file_test_status().unwrap().passed, counts.0);
    // Forgetting the damaged results changes the counts the next status reads.
    let forgotten = session
        .file_tests_forget(Some(OUTCOME_DAMAGED), &[])
        .unwrap();
    assert_eq!(forgotten, counts.1);
    let status = session.file_test_status().unwrap();
    assert_eq!(
        (status.passed, status.damaged, status.untested),
        (counts.0, 0, counts.2)
    );
    drop(session);
    let _ = std::fs::remove_dir_all(folder);
}
