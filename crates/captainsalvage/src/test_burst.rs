//! One shared permit for tests that start a burst of worker threads.
//!
//! `RUST_TEST_THREADS` limits how many tests run at once, not the threads a test starts
//! inside itself. Each test that releases several workers together to reproduce a race holds
//! this permit for the whole burst, so two such tests never add their workers together. The
//! workers inside one test still start at the same moment, so the race stays reproducible.

use std::sync::{Mutex, MutexGuard};

static BURST: Mutex<()> = Mutex::new(());

/// Waits until no other test is running a burst, then holds the permit until dropped.
pub(crate) fn permit() -> MutexGuard<'static, ()> {
    BURST
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
