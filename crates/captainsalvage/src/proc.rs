//! Runs the system's own tools (`diskutil`, `ioreg`) and counts how many processes this app has started.
//!
//! The count is for tests: the window asks for the app's status every two seconds, and a status call that
//! starts a process (or reads a disk) makes opening a large volume slow, so a test can prove that a status
//! call starts none by reading the count before and after.

use std::io;
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU64, Ordering};

static STARTED: AtomicU64 = AtomicU64::new(0);

/// Runs `command` to the end and returns what it printed, counting it.
pub fn output(command: &mut Command) -> io::Result<Output> {
    STARTED.fetch_add(1, Ordering::Relaxed);
    command.output()
}

/// How many processes `output` has started in this app so far.
pub fn processes_started() -> u64 {
    STARTED.load(Ordering::Relaxed)
}
