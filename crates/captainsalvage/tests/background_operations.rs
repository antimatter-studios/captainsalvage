//! A session never runs two of the same background operation: one scan thread, one copy thread and one file-test thread at
//! most, none of which starts another. That is what bounds the threads the engine's own tests start (`export.rs`, `filetest.rs`
//! and `session.rs` each spawn one thread per operation), so the bound is pinned here and cannot be lost without a test
//! failing (Refs #383).
//!
//! Each operation is held in the middle of its work by a source whose reads wait at a gate, so the second start is made while
//! the first thread is certainly running, and is refused. Nothing is timed.

use captainsalvage::export::CopyRequest;
use captainsalvage::filetest::TestFilter;
use captainsalvage::source::{BlockSource, MemorySource, ReadStats};
use captainsalvage::workspace::Workspace;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

mod ntfs_support;
use ntfs_support::*;

/// A source whose reads wait until the gate is open, and count the reads that are waiting.
struct Gate {
    inner: MemorySource,
    open: Mutex<bool>,
    changed: Condvar,
    waiting: AtomicU64,
}

impl Gate {
    fn new(image: Vec<u8>) -> Arc<Self> {
        Arc::new(Self {
            inner: MemorySource::new(image),
            open: Mutex::new(true),
            changed: Condvar::new(),
            waiting: AtomicU64::new(0),
        })
    }

    fn set(&self, open: bool) {
        *self.open.lock().unwrap() = open;
        self.changed.notify_all();
    }

    /// Waits until a read is held at the gate.
    fn wait_for_a_reader(&self) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while self.waiting.load(Ordering::SeqCst) == 0 {
            assert!(Instant::now() < deadline, "nothing read the source");
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

impl BlockSource for Gate {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> std::io::Result<()> {
        let mut open = self.open.lock().unwrap();
        if !*open {
            self.waiting.fetch_add(1, Ordering::SeqCst);
            while !*open {
                open = self.changed.wait(open).unwrap();
            }
            self.waiting.fetch_sub(1, Ordering::SeqCst);
        }
        drop(open);
        self.inner.read_exact_at(offset, buffer)
    }

    fn read_stats(&self) -> ReadStats {
        self.inner.read_stats()
    }
}

fn session_over(gate: &Arc<Gate>, label: &str) -> Arc<captainsalvage::session::Session> {
    captainsalvage::session::Session::with_source(
        Path::new("/test/ntfs-image"),
        gate.clone(),
        Workspace::at(std::env::temp_dir().join(format!(
            "captainsalvage-background-{}-{label}",
            std::process::id()
        )))
        .unwrap(),
    )
    .unwrap()
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(60);
    while !done() {
        assert!(Instant::now() < deadline, "{what} did not finish");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_second_scan_is_refused_while_one_is_running() {
    let gate = Gate::new(fixture().image);
    let session = session_over(&gate, "scan");
    gate.set(false);
    session.start_scan().unwrap();
    gate.wait_for_a_reader();
    let error = session.start_scan().unwrap_err();
    assert!(
        format!("{error:#}").contains("already running"),
        "{error:#}"
    );
    gate.set(true);
    wait_until("the scan", || {
        session.status().unwrap().scan.status == "complete"
    });
}

#[test]
fn a_second_copy_is_refused_while_one_is_running() {
    let gate = Gate::new(fixture().image);
    let session = session_over(&gate, "copy");
    scan(&session);
    let engine = captainsalvage::engine::Engine::new();
    engine.set_session(session.clone());
    let id = find(&engine, "pic.png")["id"].as_i64().unwrap();
    let destination = std::env::temp_dir().join(format!(
        "captainsalvage-background-copy-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&destination);
    let request = || CopyRequest {
        destination: destination.to_string_lossy().into_owned(),
        entry_ids: vec![id],
        include_partial: true,
        ..CopyRequest::default()
    };
    gate.set(false);
    session.copy_start(request()).unwrap();
    gate.wait_for_a_reader();
    let error = session.copy_start(request()).unwrap_err();
    assert!(
        format!("{error:#}").contains("already running"),
        "{error:#}"
    );
    gate.set(true);
    wait_until("the copy", || {
        session.copy_status().unwrap().state != "running"
    });
    let _ = std::fs::remove_dir_all(&destination);
}

#[test]
fn a_second_file_test_is_refused_while_one_is_running() {
    let gate = Gate::new(fixture().image);
    let session = session_over(&gate, "file-test");
    scan(&session);
    let engine = captainsalvage::engine::Engine::new();
    engine.set_session(session.clone());
    let id = find(&engine, "pic.png")["id"].as_i64().unwrap();
    let filter = || TestFilter {
        ids: vec![id],
        ..TestFilter::default()
    };
    gate.set(false);
    session.file_test_start(Vec::new(), filter()).unwrap();
    gate.wait_for_a_reader();
    let error = session.file_test_start(Vec::new(), filter()).unwrap_err();
    assert!(
        format!("{error:#}").contains("already being tested"),
        "{error:#}"
    );
    gate.set(true);
    wait_until("the file test", || {
        session.file_test_status().unwrap().state != "running"
    });
}
