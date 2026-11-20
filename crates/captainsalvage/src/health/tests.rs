use super::probe::{self, Budget, Options};
use super::*;
use crate::catalog::{identifier, Catalog};
use crate::disk::RawDisk;
use crate::raw_scan::Fingerprint;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

const MIB: u64 = 1024 * 1024;

/// A folder of this test, standing in for the app's, removed when the test ends.
struct Root(PathBuf);

impl Root {
    fn new() -> Self {
        let path =
            std::env::temp_dir().join(format!("captainsalvage-health-{}", identifier().unwrap()));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn store(&self) -> Arc<HealthStore> {
        Arc::new(HealthStore::from_catalog(
            Catalog::open_unmigrated(&self.0).unwrap(),
        ))
    }
}

impl Drop for Root {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Bytes that tell one block from another.
fn patterned(size: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    (0..size)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state >> 24) as u8
        })
        .collect()
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    /// The read fails, as a bad sector does.
    Error,
    /// The read succeeds after this many milliseconds, as a drive retrying inside does.
    Slow(u64),
    /// The read hangs this long, then the device is gone, as a bridge reset does.
    Reset(u64),
}

/// A drive that is bad in chosen places, and remembers every read it was asked for.
struct FakeDrive {
    data: Vec<u8>,
    faults: Vec<(u64, u64, Fault)>,
    gone: AtomicBool,
    reads: Mutex<Vec<(u64, u64)>>,
}

impl FakeDrive {
    fn new(data: Vec<u8>, faults: Vec<(u64, u64, Fault)>) -> Arc<Self> {
        Arc::new(Self {
            data,
            faults,
            gone: AtomicBool::new(false),
            reads: Mutex::new(Vec::new()),
        })
    }

    fn reads(&self) -> Vec<(u64, u64)> {
        self.reads.lock().unwrap().clone()
    }

    /// How many reads touched `start..end`.
    fn reads_in(&self, start: u64, end: u64) -> usize {
        self.reads()
            .iter()
            .filter(|(at, length)| *at < end && at + length > start)
            .count()
    }
}

impl BlockSource for FakeDrive {
    fn read_exact_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<()> {
        if self.gone.load(Ordering::Relaxed) {
            return Err(io::Error::from_raw_os_error(libc::ENXIO));
        }
        self.reads
            .lock()
            .unwrap()
            .push((offset, buffer.len() as u64));
        let end = offset + buffer.len() as u64;
        if let Some((_, _, fault)) = self
            .faults
            .iter()
            .find(|(start, stop, _)| offset < *stop && end > *start)
        {
            match fault {
                Fault::Error => return Err(io::Error::from_raw_os_error(libc::EIO)),
                Fault::Slow(ms) => std::thread::sleep(std::time::Duration::from_millis(*ms)),
                Fault::Reset(ms) => {
                    std::thread::sleep(std::time::Duration::from_millis(*ms));
                    self.gone.store(true, Ordering::Relaxed);
                    return Err(io::Error::from_raw_os_error(libc::ENXIO));
                }
            }
        }
        let slice = self
            .data
            .get(offset as usize..end as usize)
            .ok_or_else(|| io::Error::from(io::ErrorKind::UnexpectedEof))?;
        buffer.copy_from_slice(slice);
        Ok(())
    }

    fn read_stats(&self) -> ReadStats {
        ReadStats::default()
    }

    fn is_gone(&self) -> bool {
        self.gone.load(Ordering::Relaxed)
    }
}

fn health_of(drive: &Arc<FakeDrive>, store: &Arc<HealthStore>) -> Arc<Health> {
    let length = drive.data.len() as u64;
    let key = source_key(drive.as_ref(), length);
    Health::attach(drive.clone(), length, store.clone(), key).unwrap()
}

fn read(health: &Arc<Health>, offset: u64, length: usize) -> io::Result<()> {
    health.guard().read_exact_at(offset, &mut vec![0u8; length])
}

#[test]
fn a_failed_read_marks_its_range_and_the_range_is_not_read_again() {
    let root = Root::new();
    let drive = FakeDrive::new(
        patterned(8 * MIB as usize, 1),
        vec![(2 * MIB + 8192, 2 * MIB + 12288, Fault::Error)],
    );
    let health = health_of(&drive, &root.store());
    assert!(read(&health, 2 * MIB + 8192, 4096).is_err());
    let touched = drive.reads_in(2 * MIB, 3 * MIB);
    assert_eq!(touched, 1);
    // The same block, and a larger read over it, are refused without asking the drive.
    for (offset, length) in [(2 * MIB + 8192, 4096), (2 * MIB, 64 * 1024)] {
        let error = read(&health, offset, length).unwrap_err();
        let refused = known_bad(&error).expect("the refusal says which range");
        assert_eq!(refused.state, RangeState::Unreadable);
    }
    assert_eq!(
        drive.reads_in(2 * MIB, 3 * MIB),
        touched,
        "the drive was not asked again"
    );
    // Next to it still reads.
    assert!(read(&health, 2 * MIB + 4 * 4096, 4096).is_ok());
}

#[test]
fn a_slow_read_is_recorded_as_slow_and_still_allowed() {
    let root = Root::new();
    let drive = FakeDrive::new(
        patterned(2 * MIB as usize, 2),
        vec![(MIB, MIB + 8192, Fault::Slow(60))],
    );
    let health = health_of(&drive, &root.store());
    health.set_slow_ms(30);
    assert!(read(&health, MIB, 4096).is_ok());
    let map = health.snapshot();
    assert_eq!(map.count(RangeState::Slow), 1);
    assert!(map
        .ranges()
        .iter()
        .any(|range| range.state == RangeState::Slow && range.latency_ms >= Some(60)));
    assert!(read(&health, MIB, 4096).is_ok(), "slow does not block");
}

#[test]
fn a_read_the_device_reset_during_is_a_culprit_that_a_later_run_does_not_read() {
    let root = Root::new();
    let store = root.store();
    let data = patterned(8 * MIB as usize, 3);
    let drive = FakeDrive::new(
        data.clone(),
        vec![(5 * MIB, 5 * MIB + 8192, Fault::Reset(40))],
    );
    let key;
    {
        let health = health_of(&drive, &store);
        key = health.key().to_string();
        assert!(read(&health, 5 * MIB, 4096).is_err());
        assert!(health.is_gone());
        assert_eq!(health.snapshot().count(RangeState::ResetCulprit), 1);
    }
    // The drive comes back as another device, and the app starts again.
    let back = FakeDrive::new(data, vec![(5 * MIB, 5 * MIB + 8192, Fault::Reset(40))]);
    let health = health_of(&back, &store);
    assert_eq!(
        health.key(),
        key,
        "the same drive has the same key whatever it is called"
    );
    assert!(known_bad(&read(&health, 5 * MIB, 4096).unwrap_err()).is_some());
    assert!(known_bad(&read(&health, 5 * MIB - 4096, 8192).unwrap_err()).is_some());
    assert_eq!(
        back.reads_in(5 * MIB, 6 * MIB),
        0,
        "the culprit was never read again"
    );
    assert!(read(&health, 4 * MIB, 4096).is_ok());
}

#[test]
fn a_read_that_started_and_never_finished_is_a_culprit_when_the_source_is_opened() {
    let root = Root::new();
    let store = root.store();
    let data = patterned(8 * MIB as usize, 4);
    let first = FakeDrive::new(data.clone(), vec![]);
    let key;
    {
        let health = health_of(&first, &store);
        key = health.key().to_string();
        // The app was killed in the middle of a read: its row has a start and no end.
        store
            .started(&key, &health.owner, 3 * MIB, 4 * MIB)
            .unwrap();
        // A read that ended well leaves a finished row, and is not a culprit.
        let finished = store.started(&key, &health.owner, 0, 4096).unwrap();
        store.done(finished, 3, "ok").unwrap();
        // While this run is alive, its own open row is in flight and is not read as abandoned.
        assert!(store.abandoned(&key).unwrap().is_empty());
    }
    let second = FakeDrive::new(data, vec![]);
    let health = health_of(&second, &store);
    let found = health.found_at_open();
    assert_eq!(found.len(), 1);
    assert_eq!(
        (found[0].start, found[0].end, found[0].state),
        (3 * MIB, 7 * MIB, RangeState::ResetCulprit)
    );
    assert!(found[0].note.contains("somewhere inside"));
    assert!(known_bad(&read(&health, 3 * MIB + 4096, 4096).unwrap_err()).is_some());
    assert_eq!(second.reads_in(3 * MIB, 7 * MIB), 0);
    // It is counted once: a third run finds the range in the map, and no open row.
    drop(health);
    let third = FakeDrive::new(patterned(8 * MIB as usize, 4), vec![]);
    let again = health_of(&third, &store);
    assert!(again.found_at_open().is_empty());
    assert_eq!(again.snapshot().count(RangeState::ResetCulprit), 1);
    assert!(store.abandoned(&key).unwrap().is_empty());
}

#[test]
fn journaled_reads_write_their_start_before_the_read_and_their_time_after_it() {
    let root = Root::new();
    let store = root.store();
    let drive = FakeDrive::new(patterned(2 * MIB as usize, 5), vec![]);
    let health = health_of(&drive, &store);
    {
        let _journal = health.journal_scope();
        read(&health, 8192, 4096).unwrap();
    }
    // Outside a scope a small read is not journaled; a large one always is.
    read(&health, 16384, 4096).unwrap();
    read(&health, 0, MIB as usize).unwrap();
    let rows = store.recent(health.key(), 10).unwrap();
    assert_eq!(rows.len(), 2);
    assert!(rows
        .iter()
        .all(|row| row["outcome"] == "ok" && row["elapsedMs"].is_u64()));
    let starts: Vec<u64> = rows
        .iter()
        .map(|row| row["start"].as_u64().unwrap())
        .collect();
    assert!(starts.contains(&8192) && starts.contains(&0) && !starts.contains(&16384));
}

const BIG: Budget = Budget {
    max_reads: 2000,
    max_millis: 120_000,
};

fn options(grid_mib: u64, skip_mib: u64, slow_ms: u64) -> Options {
    Options {
        grid_bytes: grid_mib * MIB,
        edge_bytes: MIB,
        slow_ms,
        skip_bytes: skip_mib * MIB,
        failing_after: 2,
    }
}

/// Steps until the grid is done (or the step stops for another reason).
fn run_grid(health: &Health) -> probe::StepReport {
    loop {
        let (_, report) = probe::step(health, BIG).unwrap();
        if report.phase != probe::Phase::Grid
            || report
                .stopped_because
                .as_deref()
                .is_some_and(|why| !why.contains("budget"))
        {
            return report;
        }
    }
}

#[test]
fn the_grid_jumps_over_a_bad_band_and_marks_it_suspect_and_the_edges_wait_for_the_person() {
    let root = Root::new();
    let store = root.store();
    let zone = (20_000_000u64, 23_000_000u64);
    let drive = FakeDrive::new(
        patterned(64 * MIB as usize, 6),
        vec![(zone.0, zone.1, Fault::Error)],
    );
    let health = health_of(&drive, &store);
    probe::start(&health, options(4, 4, 500)).unwrap();
    let report = run_grid(&health);
    assert_eq!(report.phase, probe::Phase::Edges, "{report:?}");
    assert!(!report.finished);
    assert_eq!(
        (report.bands, report.bands_mapped, report.zones_found),
        (1, 0, 0)
    );
    assert!(
        report
            .stopped_because
            .as_deref()
            .unwrap()
            .contains("action edges"),
        "{report:?}"
    );
    // The grid read the band once, where it found it bad, and jumped the rest: 20 MiB was read, 24 MiB
    // was the first clean read after it, and nothing between was.
    assert_eq!(
        drive.reads_in(16 * MIB + 4096, 24 * MIB),
        1,
        "{:?}",
        drive.reads()
    );
    let map = health.snapshot();
    let band = map
        .blocked(18 * MIB, 18 * MIB + 4096)
        .expect("the band is marked");
    assert_eq!(band.state, RangeState::Suspect);
    // The block the grid found bad is its own range inside the band.
    assert_eq!(band.start, 16 * MIB + 4096);
    let suspect: Vec<_> = map
        .ranges()
        .iter()
        .filter(|range| range.state == RangeState::Suspect)
        .collect();
    assert_eq!(suspect.last().map(|range| range.end), Some(24 * MIB));
    assert_eq!(map.count(RangeState::Unreadable), 1);
    assert_eq!(
        known_bad(&read(&health, 18 * MIB, 4096).unwrap_err())
            .unwrap()
            .state,
        RangeState::Suspect
    );
    // Another step maps no edges and reads nothing.
    let reads = drive.reads().len();
    let (_, again) = probe::step(&health, BIG).unwrap();
    assert_eq!(again.reads, 0);
    assert_eq!(drive.reads().len(), reads);

    // The person says to map the edges: from the clean reads on each side, toward the band.
    let (state, report) = probe::edges(&health, BIG).unwrap();
    assert!(report.finished, "{report:?}");
    assert_eq!((report.bands_mapped, state.zones_found), (1, 1));
    let (from, to) = state.bands[0].zone.expect("the band's zone");
    assert!(from <= zone.0 && from >= zone.0 - 3 * MIB, "{from}");
    assert!(to >= zone.1 && to <= zone.1 + 3 * MIB, "{to}");
    assert!(
        drive.reads_in(zone.0, zone.1) <= 3,
        "never a sweep of the bad area: {:?}",
        drive.reads()
    );
    // What was suspect outside the zone is free again, what is in it is still not read.
    assert!(read(&health, 17 * MIB, 4096).is_ok());
    assert!(read(&health, 8 * MIB, 4096).is_ok());
    assert!(known_bad(&read(&health, 21 * MIB, 4096).unwrap_err()).is_some());
    // The band is now one bounded zone, as bad as the worst read in it was, and nothing is left suspect.
    let map = health.snapshot();
    assert_eq!(map.count(RangeState::Suspect), 0);
    let inside: Vec<_> = map
        .ranges()
        .iter()
        .filter(|range| range.state == RangeState::Unreadable)
        .collect();
    assert_eq!(
        (
            inside.first().map(|r| r.start),
            inside.last().map(|r| r.end)
        ),
        (Some(from), Some(to))
    );
    assert!(
        inside.windows(2).all(|pair| pair[0].end == pair[1].start),
        "one stretch: {inside:?}"
    );
}

#[test]
fn the_probe_is_bounded_per_call_and_resumable_after_the_app_is_closed() {
    let root = Root::new();
    let store = root.store();
    let length = 40 * MIB;
    let faults = vec![(25 * MIB, 26 * MIB, Fault::Slow(250))];
    let data = patterned(length as usize, 7);

    let drive = FakeDrive::new(data.clone(), faults.clone());
    let health = health_of(&drive, &store);
    probe::start(&health, options(1, 2, 150)).unwrap();
    let (_, first) = probe::step(
        &health,
        Budget {
            max_reads: 5,
            max_millis: 60_000,
        },
    )
    .unwrap();
    assert_eq!(first.reads, 5);
    assert!(!first.finished);
    assert!(first
        .stopped_because
        .as_deref()
        .unwrap()
        .contains("read budget"));
    drop(health);

    // The app is closed and opened again: the probe goes on where it was.
    let drive2 = FakeDrive::new(data.clone(), faults.clone());
    let health = health_of(&drive2, &store);
    let stored = probe::load(&health).unwrap().unwrap();
    assert_eq!(stored.reads, 5);
    let mut calls = 0;
    loop {
        let (_, report) = probe::step(
            &health,
            Budget {
                max_reads: 7,
                max_millis: 60_000,
            },
        )
        .unwrap();
        assert!(report.reads <= 7);
        calls += 1;
        if report.phase != probe::Phase::Grid {
            break;
        }
        assert!(calls < 50);
    }
    assert!(calls > 1);
    probe::edges(&health, BIG).unwrap();
    let resumed = health.snapshot();

    // One call to the end gives the same map.
    let root_two = Root::new();
    let drive3 = FakeDrive::new(data, faults);
    let health_three = health_of(&drive3, &root_two.store());
    probe::start(&health_three, options(1, 2, 150)).unwrap();
    let report = run_grid(&health_three);
    assert_eq!(report.phase, probe::Phase::Edges);
    let (_, report) = probe::edges(&health_three, BIG).unwrap();
    assert!(report.finished);
    // The same map, whatever the timings of the reads were.
    let shape = |map: &HealthMap| {
        map.ranges()
            .iter()
            .map(|range| (range.start, range.end, range.state))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&resumed), shape(&health_three.snapshot()));
    assert!(resumed.count(RangeState::Suspect) >= 1);
}

#[test]
fn a_probe_stops_when_the_device_resets_and_goes_on_after_it_is_rebound_without_reading_the_culprit(
) {
    let root = Root::new();
    let store = root.store();
    let data = patterned(32 * MIB as usize, 8);
    let faults = vec![(12 * MIB, 13 * MIB, Fault::Reset(40))];
    let drive = FakeDrive::new(data.clone(), faults.clone());
    let health = health_of(&drive, &store);
    probe::start(&health, options(4, 4, 500)).unwrap();
    let (_, report) = probe::step(&health, BIG).unwrap();
    assert!(!report.finished);
    assert!(
        report
            .stopped_because
            .as_deref()
            .unwrap()
            .contains("went away"),
        "{report:?}"
    );
    assert_eq!(health.snapshot().count(RangeState::ResetCulprit), 1);
    assert_eq!((report.resets, health.resets()), (1, 1));
    assert!(!health.failing(), "one reset is not a failing drive");
    let error = probe::step(&health, Budget::default()).unwrap_err();
    assert!(format!("{error:#}").contains("gone away"));

    // The drive comes back, found again, and the culprit stays marked: the grid jumps the band.
    let back = FakeDrive::new(data, faults);
    health.rebind(back.clone());
    assert!(!health.is_gone());
    let report = run_grid(&health);
    assert_eq!(report.phase, probe::Phase::Edges, "{report:?}");
    let (state, report) = probe::edges(&health, BIG).unwrap();
    assert!(report.finished, "{report:?}");
    assert_eq!(state.bands[0].resets, 1, "the band says what it cost");
    assert_eq!(
        back.reads_in(12 * MIB, 13 * MIB),
        0,
        "the culprit was not read again"
    );
    assert!(!back.is_gone());
}

#[test]
fn a_damaged_band_with_islands_costs_one_reset_not_two() {
    // Like the first real run: good for a long way, one slow read, then a reset, a slow island, a
    // good island, and another reset a little further on.
    let root = Root::new();
    let store = root.store();
    let data = patterned(48 * MIB as usize, 9);
    let block = 4096;
    let faults = vec![
        (13 * MIB, 13 * MIB + block, Fault::Slow(130)),
        (14 * MIB, 14 * MIB + block, Fault::Reset(40)),
        (15 * MIB, 15 * MIB + block, Fault::Slow(130)),
        (18 * MIB, 18 * MIB + block, Fault::Reset(40)),
    ];
    let drive = FakeDrive::new(data.clone(), faults.clone());
    let health = health_of(&drive, &store);
    probe::start(&health, options(1, 5, 500)).unwrap();
    let (_, report) = probe::step(&health, BIG).unwrap();
    assert!(report
        .stopped_because
        .as_deref()
        .unwrap()
        .contains("went away"));
    assert_eq!(
        drive.reads_in(13 * MIB, 13 * MIB + block),
        1,
        "the slow read before the reset was read"
    );
    assert_eq!(drive.reads_in(14 * MIB, 14 * MIB + block), 1);

    let back = FakeDrive::new(data, faults);
    health.rebind(back.clone());
    let report = run_grid(&health);
    assert_eq!(report.phase, probe::Phase::Edges, "{report:?}");
    assert_eq!(
        report.bands, 1,
        "the islands and the second culprit are inside one band"
    );
    assert_eq!(
        back.reads_in(14 * MIB, 19 * MIB),
        0,
        "nothing after the jump read the band, so the second culprit never cost a reset"
    );
    assert_eq!(health.resets(), 1);
    let (state, _) = probe::step(&health, BIG).unwrap();
    // The band starts after the last clean read, which is before the slow one, so it holds the slow
    // read too; it ends at the first clean read after the jump.
    assert_eq!(
        (state.bands[0].start, state.bands[0].end),
        (12 * MIB + block, 19 * MIB)
    );
    assert_eq!(
        (state.bands[0].first_bad, state.bands[0].resets),
        (14 * MIB, 1)
    );
    // The islands are not read, nor is the band's inside by any other reader.
    for inside in [13 * MIB, 15 * MIB, 16 * MIB, 17 * MIB] {
        assert_eq!(
            known_bad(&read(&health, inside, 4096).unwrap_err())
                .unwrap()
                .state,
            RangeState::Suspect
        );
    }
    assert!(read(&health, 12 * MIB, 4096).is_ok() && read(&health, 19 * MIB, 4096).is_ok());
}

#[test]
fn a_probe_pauses_when_the_drive_has_reset_twice_and_reading_becomes_careful() {
    let root = Root::new();
    let store = root.store();
    let data = patterned(48 * MIB as usize, 13);
    let faults = vec![
        (10 * MIB, 10 * MIB + 4096, Fault::Reset(40)),
        (30 * MIB, 30 * MIB + 4096, Fault::Reset(40)),
    ];
    let first = FakeDrive::new(data.clone(), faults.clone());
    let health = health_of(&first, &store);
    probe::start(&health, options(1, 4, 500)).unwrap();
    let (_, report) = probe::step(&health, BIG).unwrap();
    assert!(report
        .stopped_because
        .as_deref()
        .unwrap()
        .contains("went away"));

    let second = FakeDrive::new(data.clone(), faults.clone());
    health.rebind(second.clone());
    let (_, report) = probe::step(&health, BIG).unwrap();
    assert!(
        report
            .stopped_because
            .as_deref()
            .unwrap()
            .contains("went away"),
        "{report:?}"
    );
    assert!(
        report
            .stopped_because
            .as_deref()
            .unwrap()
            .contains("failing"),
        "the second reset says the drive is failing: {report:?}"
    );
    assert_eq!(
        (report.resets, health.resets(), health.failing()),
        (2, 2, true)
    );

    // The third drive: the probe will not go on by itself.
    let third = FakeDrive::new(data.clone(), faults.clone());
    health.rebind(third.clone());
    let (_, paused) = probe::step(&health, BIG).unwrap();
    assert_eq!(paused.reads, 0);
    assert!(
        paused
            .stopped_because
            .as_deref()
            .unwrap()
            .contains("proceed"),
        "{paused:?}"
    );
    assert!(third.reads().is_empty());
    let (_, going) = probe::step_with(&health, BIG, false, true).unwrap();
    assert!(going.reads > 0, "{going:?}");

    // A drive that is failing is read carefully: a long read goes in pieces of 64 KiB, each journaled.
    let before = third.reads().len();
    read(&health, 2 * MIB, MIB as usize).unwrap();
    let pieces: Vec<u64> = third.reads()[before..]
        .iter()
        .map(|(_, length)| *length)
        .collect();
    assert_eq!(pieces, vec![64 * 1024; 16]);
    let rows = store.recent(health.key(), 100).unwrap();
    assert!(rows.iter().filter(|row| row["length"] == 64 * 1024).count() >= 16);

    // The count is kept for the source: another run of the app starts out careful.
    drop(health);
    let again = health_of(&FakeDrive::new(data, faults), &store);
    assert!(again.failing());
    // Only the person clears it.
    again.clear_resets();
    assert!(!again.failing());
}

#[test]
fn a_suspect_range_is_forgiven_by_the_person_and_read_again() {
    let root = Root::new();
    let drive = FakeDrive::new(patterned(8 * MIB as usize, 14), vec![]);
    let health = health_of(&drive, &root.store());
    health.record(HealthRange::new(
        2 * MIB,
        3 * MIB,
        RangeState::Suspect,
        None,
        "a test",
    ));
    assert!(known_bad(&read(&health, 2 * MIB, 4096).unwrap_err()).is_some());
    assert_eq!(drive.reads_in(2 * MIB, 3 * MIB), 0);
    health.forgive(2 * MIB, 3 * MIB);
    assert!(read(&health, 2 * MIB, 4096).is_ok());
}

#[test]
fn markers_are_taken_from_outside_the_bad_zones_and_the_fingerprint_says_so() {
    let root = Root::new();
    let store = root.store();
    let length = 40 * MIB;
    let data = patterned(length as usize, 9);
    // Nominal markers sit at every length / 19. Put a zone over the fifth.
    let nominal = (u128::from(length - 4096) * 4 / 19) as u64 / 4096 * 4096;
    let zone = (nominal - MIB / 2, nominal + MIB / 2);
    let drive = FakeDrive::new(data, vec![]);
    let disk = RawDisk::with_source_guarded(Path::new("/dev/rdisk9"), drive.clone(), length, store)
        .unwrap();
    disk.health
        .as_ref()
        .unwrap()
        .health
        .record(HealthRange::new(
            zone.0,
            zone.1,
            RangeState::Unreadable,
            None,
            "test",
        ));
    let fingerprint = Fingerprint::read(&disk);
    assert_eq!(fingerprint.moved_for_bad_zones, vec![nominal]);
    assert!(
        fingerprint.samples.len() >= 19,
        "the moved marker still identifies the drive"
    );
    assert!(fingerprint
        .samples
        .iter()
        .all(|sample| sample.offset + sample.length <= zone.0 || sample.offset >= zone.1));
    assert_eq!(drive.reads_in(zone.0, zone.1), 0, "nothing read the zone");
}

#[test]
fn a_comparison_does_not_read_a_marker_in_a_bad_zone_and_calls_it_unknown() {
    let root = Root::new();
    let length = 40 * MIB;
    let data = patterned(length as usize, 10);
    let saved = {
        let disk = RawDisk::with_source(
            Path::new("/x"),
            FakeDrive::new(data.clone(), vec![]),
            length,
        )
        .unwrap();
        Fingerprint::read(&disk)
    };
    let drive = FakeDrive::new(data, vec![]);
    let disk = RawDisk::with_source_guarded(
        Path::new("/dev/rdisk9"),
        drive.clone(),
        length,
        root.store(),
    )
    .unwrap();
    let marker = saved.samples[7].offset;
    disk.health
        .as_ref()
        .unwrap()
        .health
        .record(HealthRange::new(
            marker,
            marker + 4096,
            RangeState::ResetCulprit,
            None,
            "test",
        ));
    let comparison = saved.compare(&mut crate::raw_scan::MarkerReader::new(&disk));
    assert_eq!(comparison.unreadable, 1);
    assert_eq!(
        comparison.changed, 0,
        "a block that is not read is unknown, not changed"
    );
    // Everything that can be read matches, so it is an exact match: the marker in the bad sector says
    // nothing, and is left out of what the match is counted over.
    assert_eq!(comparison.state, crate::raw_scan::Verification::Exact);
    // (A zone where the drive reset makes it a careful open, which reads fewer markers still.)
    assert!(comparison.total >= 1 && comparison.total < saved.samples.len());
    // (A zone where the drive reset makes it a careful open, which reads fewer markers still.)
    assert!(comparison.total >= 1 && comparison.total < saved.samples.len());
    assert!(
        comparison.summary.contains("lie in bad sectors"),
        "{}",
        comparison.summary
    );
    assert_eq!(drive.reads_in(marker, marker + 4096), 0);
}

#[test]
fn a_drive_that_comes_back_is_the_same_source_and_another_drive_is_refused() {
    let root = Root::new();
    let length = 40 * MIB;
    let data = patterned(length as usize, 11);
    let drive = FakeDrive::new(data.clone(), vec![]);
    let disk = RawDisk::with_source_guarded(
        Path::new("/dev/rdisk7s1"),
        drive.clone(),
        length,
        root.store(),
    )
    .unwrap();
    let view = disk.health.as_ref().unwrap();
    view.health.set_identity(Fingerprint::read(&disk));
    let id = disk.id.clone();

    // Still answering: nothing to rebind.
    assert!(format!(
        "{:#}",
        disk.rebound(Path::new("/dev/rdisk12s1")).err().unwrap()
    )
    .contains("still answers"));

    drive.gone.store(true, Ordering::Relaxed);
    assert!(disk.source.is_gone());
    assert!(read_disk(&disk, 0).is_err());

    // Another drive of the same size comes up: refused, and the old device stays where it was.
    let other = FakeDrive::new(patterned(length as usize, 99), vec![]);
    let refusal = disk
        .rebound_with(other, Path::new("/dev/rdisk12s1"))
        .err()
        .unwrap();
    assert!(
        format!("{refusal:#}").contains("does not look like the drive"),
        "{refusal:#}"
    );
    assert!(disk.source.is_gone());

    // The same drive comes back under another name: the same ID, reading again.
    let back = FakeDrive::new(data, vec![]);
    let rebound = disk
        .rebound_with(back.clone(), Path::new("/dev/rdisk12s1"))
        .unwrap();
    assert_eq!(rebound.id, id);
    assert_eq!(rebound.path, Path::new("/dev/rdisk12s1"));
    assert!(read_disk(&rebound, 0).is_ok());
    assert!(
        read_disk(&disk, 0).is_ok(),
        "what holds the old view reads from the new device"
    );
    assert!(back.reads_in(0, 4096) >= 1);
}

fn read_disk(disk: &RawDisk, offset: u64) -> io::Result<()> {
    disk.source.read_exact_at(offset, &mut [0u8; 4096])
}

#[test]
fn journaling_a_read_costs_microseconds_not_a_disk_flush() {
    let root = Root::new();
    let store = root.store();
    let drive = FakeDrive::new(patterned(2 * MIB as usize, 12), vec![]);
    let health = health_of(&drive, &store);
    let _journal = health.journal_scope();
    let started = Instant::now();
    for index in 0..500u64 {
        read(&health, (index % 500) * 4096 % MIB, 4096).unwrap();
    }
    let each = started.elapsed() / 500;
    // An open of a big NTFS source journals thousands of reads: that must not take minutes.
    assert!(
        each < std::time::Duration::from_millis(3),
        "{each:?} a journaled read"
    );
    assert_eq!(store.recent(health.key(), 10_000).unwrap().len(), 500);
}

#[test]
fn a_region_of_a_guarded_source_reports_the_bad_ranges_in_its_own_offsets() {
    let root = Root::new();
    let drive = FakeDrive::new(patterned(8 * MIB as usize, 15), vec![]);
    let health = health_of(&drive, &root.store());
    health.record(HealthRange::new(
        3 * MIB,
        4 * MIB,
        RangeState::Unreadable,
        None,
        "a test",
    ));
    health.record(HealthRange::new(
        MIB / 2,
        2 * MIB,
        RangeState::Slow,
        Some(900),
        "a test",
    ));
    let region = crate::disk::RegionSource::new(health.guard(), 8 * MIB, MIB, 4 * MIB).unwrap();
    let ranges = region.bad_ranges();
    let shown: Vec<_> = ranges
        .iter()
        .map(|range| (range.start, range.end, range.state))
        .collect();
    // The slow range is cut at the region's start, the unreadable one is moved by it.
    assert_eq!(
        shown,
        vec![
            (0, MIB, RangeState::Slow),
            (2 * MIB, 3 * MIB, RangeState::Unreadable)
        ]
    );
    assert!(ranges.iter().all(|range| range.found_millis > 0));
    // The store keeps when it was found.
    let again = health_of(
        &FakeDrive::new(patterned(8 * MIB as usize, 15), vec![]),
        &root.store(),
    );
    assert!(again
        .snapshot()
        .ranges()
        .iter()
        .all(|range| range.found_millis > 0));
}

/// A whole disk of 8 MiB and a partition of it that starts at 2 MiB and is 4 MiB: the partition's bytes are the
/// disk's, as the same drive seen through two device nodes.
fn disk_and_partition(
    store: &Arc<HealthStore>,
    seed: u64,
    disk_faults: Vec<(u64, u64, Fault)>,
    partition_faults: Vec<(u64, u64, Fault)>,
) -> (Arc<FakeDrive>, Arc<Health>, Arc<FakeDrive>, Arc<Health>) {
    let data = patterned(8 * MIB as usize, seed);
    let whole = FakeDrive::new(data.clone(), disk_faults);
    let whole_health = health_of(&whole, store);
    let part = FakeDrive::new(
        data[2 * MIB as usize..6 * MIB as usize].to_vec(),
        partition_faults,
    );
    let part_key = source_key(part.as_ref(), 4 * MIB);
    let part_health = Health::attach_in(
        part.clone(),
        4 * MIB,
        store.clone(),
        part_key,
        Some((whole_health.key().to_string(), 2 * MIB)),
    )
    .unwrap();
    (whole, whole_health, part, part_health)
}

#[test]
fn a_whole_disk_and_its_partitions_share_one_map_one_reset_count_and_one_failing_flag() {
    let root = Root::new();
    let store = root.store();
    let (whole, whole_health, part, part_health) = disk_and_partition(&store, 31, vec![], vec![]);
    assert_eq!(
        part_health.key(),
        whole_health.key(),
        "kept under the physical disk's key"
    );
    assert_ne!(part_health.own_key(), whole_health.key());
    assert_eq!(part_health.base(), 2 * MIB);

    // A zone found through the partition (1 to 2 MiB into it) is the disk's 3 to 4 MiB, and the disk avoids it.
    part_health
        .mark_bad(
            MIB,
            2 * MIB,
            RangeState::ResetCulprit,
            "you",
            "the owner's dd run",
        )
        .unwrap();
    assert!(read(&part_health, MIB + 4096, 4096).is_err());
    whole_health.reload();
    let refused = read(&whole_health, 3 * MIB + 8192, 4096).unwrap_err();
    assert_eq!(
        known_bad(&refused).unwrap().start,
        3 * MIB,
        "in the disk's own offsets"
    );
    assert_eq!(whole.reads_in(3 * MIB, 4 * MIB), 0);
    assert!(read(&whole_health, 3 * MIB - 4096, 4096).is_ok());
    // And the other way: a zone the disk marks is seen from the partition, in the partition's offsets.
    whole_health
        .mark_bad(
            5 * MIB,
            5 * MIB + MIB / 2,
            RangeState::Suspect,
            "you",
            "a probe",
        )
        .unwrap();
    part_health.reload();
    let seen = part_health.snapshot();
    let starts: Vec<(u64, RangeState)> = seen
        .ranges()
        .iter()
        .filter(|r| r.state.blocks())
        .map(|r| (r.start, r.state))
        .collect();
    assert_eq!(
        starts,
        vec![
            (MIB, RangeState::ResetCulprit),
            (3 * MIB, RangeState::Suspect)
        ]
    );
    assert_eq!(
        part.reads_in(MIB, 2 * MIB),
        0,
        "the culprit was read once, before it was marked"
    );
    // Zones outside the partition are not its.
    whole_health
        .mark_bad(
            7 * MIB,
            7 * MIB + 4096,
            RangeState::Unreadable,
            "you",
            "elsewhere",
        )
        .unwrap();
    part_health.reload();
    assert_eq!(
        part_health
            .snapshot()
            .ranges()
            .iter()
            .filter(|r| r.state.blocks())
            .count(),
        2
    );

    // One reset count for the physical disk: a reset through either node counts for both, and the flag follows.
    store.add_resets(whole_health.key(), 1).unwrap();
    part_health.reload();
    whole_health.reload();
    assert_eq!((part_health.resets(), whole_health.resets()), (1, 1));
    assert!(!whole_health.failing());
    store.add_resets(part_health.key(), 1).unwrap();
    whole_health.reload();
    part_health.reload();
    assert!(
        whole_health.failing() && part_health.failing(),
        "two resets, through two nodes, is a failing drive"
    );
}

#[test]
fn what_a_partition_stored_before_it_was_known_to_be_one_moves_into_the_disk_when_they_are_linked()
{
    let root = Root::new();
    let store = root.store();
    let data = patterned(8 * MIB as usize, 32);
    // The partition is opened alone first: its own map, its own reset count, a read that never finished.
    let part = FakeDrive::new(data[2 * MIB as usize..6 * MIB as usize].to_vec(), vec![]);
    let own = source_key(part.as_ref(), 4 * MIB);
    {
        let alone = Health::attach(part.clone(), 4 * MIB, store.clone(), own.clone()).unwrap();
        alone
            .mark_bad(MIB, 2 * MIB, RangeState::ResetCulprit, "you", "found alone")
            .unwrap();
        store.started(&own, &alone.owner, 3 * MIB, 4096).unwrap();
        store.add_resets(&own, 1).unwrap();
    }
    // The whole disk is opened, and says which stretch is the partition.
    let whole = FakeDrive::new(data, vec![]);
    let disk = health_of(&whole, &store);
    store.link(&own, disk.key(), 2 * MIB, 4 * MIB).unwrap();
    disk.reload();
    let blocked: Vec<(u64, u64)> = disk
        .snapshot()
        .ranges()
        .iter()
        .filter(|r| r.state.blocks())
        .map(|r| (r.start, r.end))
        .collect();
    assert_eq!(
        blocked,
        vec![(3 * MIB, 4 * MIB), (5 * MIB, 5 * MIB + 4096)],
        "the zone, and the read that never finished, in the disk's offsets"
    );
    assert_eq!(
        disk.resets(),
        2,
        "the partition's reset, and the read that never finished"
    );
    assert_eq!(
        store.load_map(&own).unwrap().ranges().len(),
        0,
        "nothing is left under the partition's own key"
    );
    // The partition opened now shares all of it.
    let again = Health::attach_in(
        part,
        4 * MIB,
        store.clone(),
        own,
        Some((disk.key().to_string(), 2 * MIB)),
    )
    .unwrap();
    assert_eq!(
        again
            .snapshot()
            .ranges()
            .iter()
            .filter(|r| r.state.blocks())
            .count(),
        2
    );
    assert_eq!(again.resets(), 2);
}

#[test]
fn only_the_read_in_flight_when_the_device_went_away_is_a_culprit() {
    let root = Root::new();
    let store = root.store();
    let data = patterned(8 * MIB as usize, 33);
    // A read that fails at once because the node had already vanished: issued after the device went away.
    let drive = FakeDrive::new(data.clone(), vec![(4096, 8192, Fault::Reset(0))]);
    let health = health_of(&drive, &store);
    assert!(read(&health, 4096, 4096).is_err());
    assert!(health.is_gone());
    assert_eq!(
        health.snapshot().count(RangeState::ResetCulprit),
        0,
        "offset 4096 read fine before: the failure says nothing about it"
    );
    assert_eq!(health.resets(), 1, "the device did go away");
    let rows = store.recent(health.key(), 5).unwrap();
    assert!(rows.iter().all(|row| row["outcome"] != "gone"), "{rows:?}");
    // Reads after that are refused by the dead node without being recorded.
    assert!(read(&health, 16384, 4096).is_err());
    assert_eq!(health.snapshot().count(RangeState::ResetCulprit), 0);
    // One that was in flight for a while is the culprit.
    let slow = FakeDrive::new(data, vec![(MIB, MIB + 4096, Fault::Reset(60))]);
    let second = health_of(&slow, &Root::new().store());
    assert!(read(&second, MIB, 4096).is_err());
    assert_eq!(second.snapshot().count(RangeState::ResetCulprit), 1);
}

#[test]
fn a_probe_that_finds_the_device_already_gone_learns_nothing_and_reads_the_point_again_later() {
    let root = Root::new();
    let store = root.store();
    let data = patterned(16 * MIB as usize, 34);
    let drive = FakeDrive::new(
        data.clone(),
        vec![(4 * MIB, 4 * MIB + 4096, Fault::Reset(0))],
    );
    let health = health_of(&drive, &store);
    probe::start(&health, options(1, 4, 500)).unwrap();
    let (_, report) = probe::step(&health, BIG).unwrap();
    assert!(
        report
            .stopped_because
            .as_deref()
            .unwrap()
            .contains("already gone"),
        "{report:?}"
    );
    assert_eq!(
        health.snapshot().count(RangeState::ResetCulprit)
            + health.snapshot().count(RangeState::Suspect),
        0,
        "no band, no culprit"
    );
    assert_eq!(report.bands, 0);
    health.rebind(FakeDrive::new(data, vec![]));
    let report = run_grid(&health);
    assert_eq!(
        report.phase,
        probe::Phase::Done,
        "the point was read again, and was fine: {report:?}"
    );
}

#[test]
fn culprits_recorded_for_a_read_made_after_the_device_had_gone_are_dropped_when_the_source_is_opened(
) {
    let root = Root::new();
    let store = root.store();
    let drive = FakeDrive::new(patterned(2 * MIB as usize, 35), vec![]);
    let key = source_key(drive.as_ref(), 2 * MIB);
    let mut map = HealthMap::default();
    // As the real catalog held it: offset 0, a latency of 0, the guard's own note.
    map.record(HealthRange::new(
        0,
        4096,
        RangeState::ResetCulprit,
        Some(0),
        "the device went away during a read of this range",
    ));
    // The ones that are evidence: a long read, one the person marked, and one found from a read that never ended.
    map.record(HealthRange::new(
        MIB,
        MIB + 4096,
        RangeState::ResetCulprit,
        Some(108),
        "the device went away during a read of this range",
    ));
    map.record(HealthRange::new(
        MIB + 8192,
        MIB + 12288,
        RangeState::ResetCulprit,
        Some(0),
        "marked by you: dd evidence",
    ));
    map.record(HealthRange::new(
        MIB + 16384,
        MIB + 20480,
        RangeState::ResetCulprit,
        None,
        "a read of this range started and never finished, so the device probably reset during it",
    ));
    store.save_map(&key, None, &map).unwrap();
    let health = health_of(&drive, &store);
    let starts: Vec<u64> = health.snapshot().ranges().iter().map(|r| r.start).collect();
    assert_eq!(
        starts,
        vec![MIB, MIB + 8192, MIB + 16384],
        "only the instant one is gone"
    );
    assert!(read(&health, 0, 4096).is_ok(), "offset 0 reads again");
    // Stored, so it is gone for good.
    assert_eq!(store.load_map(&key).unwrap().ranges().len(), 3);
}

#[test]
fn a_disk_that_has_reset_is_recognised_from_a_few_markers_in_places_known_to_read_and_says_so() {
    let root = Root::new();
    let store = root.store();
    let length = 40 * MIB;
    let data = patterned(length as usize, 36);
    // First opened when nothing was wrong: the full set of markers is stored.
    let saved = {
        let disk = RawDisk::with_source(
            Path::new("/x"),
            FakeDrive::new(data.clone(), vec![]),
            length,
        )
        .unwrap();
        Fingerprint::read(&disk)
    };
    assert!(saved.samples.len() >= 19);
    // Then it reset: a culprit zone, far from any marker.
    let drive = FakeDrive::new(data, vec![]);
    let disk = RawDisk::with_source_guarded(Path::new("/dev/rdisk9"), drive.clone(), length, store)
        .unwrap();
    let view = disk.health.as_ref().unwrap();
    view.health
        .mark_bad(
            37 * MIB,
            38 * MIB,
            RangeState::ResetCulprit,
            "you",
            "found by dd",
        )
        .unwrap();
    assert!(disk.careful_open());
    // Two of the markers lie where an earlier open read well (journaled) and the rest nowhere known.
    let known = [saved.samples[3].offset, saved.samples[12].offset];
    for offset in known {
        view.health
            .store()
            .started(view.health.key(), "0:0", offset, 4096)
            .and_then(|row| view.health.store().done(row, 2, "ok"))
            .unwrap();
    }
    view.health.reload();
    let reads_before = drive.reads().len();
    let mut reader = crate::raw_scan::MarkerReader::new(&disk);
    assert!(reader.is_careful());
    let comparison = saved.compare(&mut reader);
    let reads = drive.reads()[reads_before..].to_vec();
    assert!(
        reads.len() <= crate::raw_scan::CAREFUL_MARKER_READS,
        "{reads:?}"
    );
    assert!(
        reads
            .iter()
            .all(|(offset, _)| *offset < MIB || known.contains(offset)),
        "only the head and what earlier reads found good: {reads:?}"
    );
    assert_eq!(
        comparison.state,
        crate::raw_scan::Verification::Exact,
        "{}",
        comparison.summary
    );
    assert_eq!(comparison.total, reads.len());
    assert!(
        comparison.summary.contains("only") && comparison.summary.contains("to spare it"),
        "{}",
        comparison.summary
    );

    // A fresh reading of the markers reads no further than that either, and in the same places.
    let before = drive.reads().len();
    let fresh = Fingerprint::read(&disk);
    let more = drive.reads()[before..].to_vec();
    assert!(
        more.len() <= crate::raw_scan::CAREFUL_MARKER_READS,
        "{more:?}"
    );
    assert!(
        more.iter()
            .all(|(offset, _)| *offset < MIB || known.contains(offset)),
        "{more:?}"
    );
    assert!(fresh.samples.len() < saved.samples.len());
}

#[test]
fn nothing_is_marked_by_a_person_without_saying_why_and_what_is_marked_is_not_read() {
    let root = Root::new();
    let drive = FakeDrive::new(patterned(4 * MIB as usize, 37), vec![]);
    let health = health_of(&drive, &root.store());
    assert!(health
        .mark_bad(MIB, 2 * MIB, RangeState::ResetCulprit, "you", "  ")
        .is_err());
    assert!(health
        .mark_bad(2 * MIB, MIB, RangeState::ResetCulprit, "you", "why")
        .is_err());
    assert!(
        health
            .mark_bad(MIB, 5 * MIB, RangeState::ResetCulprit, "you", "why")
            .is_err(),
        "past the end"
    );
    assert!(health
        .mark_bad(MIB, 2 * MIB, RangeState::Good, "you", "why")
        .is_err());
    let range = health
        .mark_bad(
            MIB,
            2 * MIB,
            RangeState::ResetCulprit,
            "you",
            "dd from 1 MiB dropped the drive",
        )
        .unwrap();
    assert_eq!(range.note, "marked by you: dd from 1 MiB dropped the drive");
    assert!(range.found_millis > 0);
    assert!(known_bad(&read(&health, MIB + 4096, 4096).unwrap_err()).is_some());
    assert_eq!(drive.reads_in(MIB, 2 * MIB), 0);
}

fn gpt_disk(seed: u64, guid: u8) -> Arc<FakeDrive> {
    let mut data = patterned(2 * MIB as usize, seed);
    // The same protective MBR as every GPT disk, then a header with this disk's GUID.
    data[..512].fill(0);
    data[446 + 4] = 0xEE;
    data[510..512].copy_from_slice(&[0x55, 0xAA]);
    data[512..520].copy_from_slice(b"EFI PART");
    data[512 + 56..512 + 72].fill(guid);
    FakeDrive::new(data, vec![])
}

#[test]
fn two_gpt_disks_of_one_size_are_two_disks_and_what_was_stored_before_the_guid_moves_to_the_one_that_had_it(
) {
    let root = Root::new();
    let store = root.store();
    let (one, two) = (gpt_disk(41, 0xA1), gpt_disk(42, 0xB2));
    let (key_one, key_two) = (
        source_key(one.as_ref(), 2 * MIB),
        source_key(two.as_ref(), 2 * MIB),
    );
    assert_ne!(
        key_one, key_two,
        "the same protective MBR, the same length: the GUID tells them apart"
    );
    assert!(key_one.contains("-g"));
    let legacy = super::legacy_source_key(one.as_ref(), 2 * MIB);
    assert_eq!(
        legacy,
        super::legacy_source_key(two.as_ref(), 2 * MIB),
        "the old key could not"
    );
    // What an earlier version stored under the old key moves to the first disk that is opened, and the
    // second disk, opened after, gets nothing of it.
    let mut map = HealthMap::default();
    map.record(HealthRange::new(
        MIB,
        MIB + 4096,
        RangeState::ResetCulprit,
        Some(108),
        "the device went away during a read of this range",
    ));
    store.save_map(&legacy, None, &map).unwrap();
    store.add_resets(&legacy, 1).unwrap();
    assert!(store.rekey(&legacy, &key_one).unwrap());
    assert!(
        !store.rekey(&legacy, &key_two).unwrap(),
        "nothing left under the old key"
    );
    assert_eq!(store.load_map(&key_one).unwrap().ranges().len(), 1);
    assert_eq!(store.load_map(&key_two).unwrap().ranges().len(), 0);
    assert_eq!(store.load_flags(&key_one).unwrap().0, 1);
    // A key that already has something is not overwritten by another disk's old rows.
    store.save_map(&legacy, None, &map).unwrap();
    assert!(!store.rekey(&legacy, &key_one).unwrap());
}

/// Runs `work` on its own thread and says whether it finished within `seconds`: a deadlock shows as a timeout,
/// not as a test that never ends.
fn finishes_within<T: Send + 'static>(
    seconds: u64,
    work: impl FnOnce() -> T + Send + 'static,
) -> Option<T> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = sender.send(work());
    });
    receiver
        .recv_timeout(std::time::Duration::from_secs(seconds))
        .ok()
}

#[test]
fn a_journaled_read_never_waits_on_a_write_lock_that_the_catalog_has_elsewhere() {
    let root = Root::new();
    let store = root.store();
    let drive = FakeDrive::new(patterned(2 * MIB as usize, 51), vec![]);
    let health = health_of(&drive, &store);
    // Another connection to the catalog, as a raw scan's, holds the write lock for as long as it likes.
    let other = rusqlite::Connection::open(root.0.join("raw-scans.sqlite")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let h = health.clone();
    let done = finishes_within(5, move || {
        let _journal = h.journal_scope();
        let read = read(&h, 8192, 4096);
        // What the status tools ask while the read goes on.
        let probe = probe::load(&h);
        let report = h.report();
        (read.is_ok(), probe.is_ok(), report["length"].as_u64())
    });
    assert_eq!(
        done,
        Some((true, true, Some(2 * MIB))),
        "a read, and the status of the source, waited on the catalog's write lock"
    );
    drop(other);
}

#[test]
fn a_raw_scan_step_does_not_hold_the_catalog_while_it_reads_the_device() {
    // The real freeze: a step took the write lock, then read the disk through the guard, whose journal wanted it too.
    let root = Root::new();
    let store = root.store();
    let length = 2 * MIB;
    let drive = FakeDrive::new(patterned(length as usize, 52), vec![]);
    let disk = RawDisk::with_source_guarded(
        Path::new("/dev/rdisk7"),
        drive.clone(),
        length,
        store.clone(),
    )
    .unwrap();
    // A drive that has reset reads carefully: every read is journaled.
    let key = disk.health.as_ref().unwrap().health.key().to_string();
    store.add_resets(&key, 2).unwrap();
    disk.health.as_ref().unwrap().health.reload();
    assert!(disk.health.as_ref().unwrap().health.failing());
    let mut scans = crate::raw_scan::ScanStore::open_in(&root.0).unwrap();
    let scan = scans.create(&disk, 0, length).unwrap();
    let id = scan.scan_id.clone();
    let done = finishes_within(8, move || {
        let step = scans.step(&disk, &id);
        step.map(|step| step.batch.next_offset)
            .map_err(|error| format!("{error:#}"))
    });
    let next = done
        .expect("the step froze: it held the catalog while the journal wanted it")
        .expect("the step");
    assert!(next > 0);
}

#[test]
fn the_journal_gives_up_quickly_and_goes_on_when_it_cannot_write() {
    let root = Root::new();
    let store = root.store();
    // Whatever holds the journal's own file, a read is not delayed by more than a moment.
    let journal = rusqlite::Connection::open(root.0.join("read-journal.sqlite")).unwrap();
    journal.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE IF NOT EXISTS read_journal (id INTEGER PRIMARY KEY AUTOINCREMENT, source_key TEXT NOT NULL, source_id TEXT, owner TEXT NOT NULL, start INTEGER NOT NULL, length INTEGER NOT NULL, started_millis INTEGER NOT NULL, finished_millis INTEGER, elapsed_ms INTEGER, outcome TEXT); BEGIN IMMEDIATE;").unwrap();
    let began = Instant::now();
    let s = store.clone();
    let wrote = finishes_within(5, move || s.started("k", "1:1", 0, 4096));
    assert!(wrote.is_some(), "writing the journal waited without end");
    assert!(
        began.elapsed() < std::time::Duration::from_secs(2),
        "{:?}",
        began.elapsed()
    );
    // The read is remembered in memory, and written when the file is free again.
    journal.execute_batch("COMMIT").unwrap();
    let row = wrote.unwrap().unwrap();
    store.done(row, 3, "ok").unwrap();
    let rows = store.recent("k", 5).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["outcome"], "ok");
}

#[test]
fn the_end_of_a_read_is_not_lost_when_the_journal_is_busy_so_no_false_open_row_reads_as_a_reset() {
    let root = Root::new();
    let store = root.store();
    let row = store.started("k", "9999999:1", 0, 4096).unwrap();
    assert!(row > 0, "the file was free: a real row");
    // Held by another writer at the moment the read ends.
    let other = rusqlite::Connection::open(root.0.join("read-journal.sqlite")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let s = store.clone();
    assert!(finishes_within(5, move || s.done(row, 4, "ok")).is_some_and(|result| result.is_ok()));
    other.execute_batch("COMMIT").unwrap();
    // Written at the next call, and not left open: a run that started later finds no read to blame.
    assert_eq!(store.recent("k", 3).unwrap()[0]["outcome"], "ok");
    assert!(store.abandoned("k").unwrap().is_empty());
}

#[test]
fn what_is_recorded_about_a_source_waits_for_a_write_lock_held_for_a_while_and_is_not_lost() {
    // Opening a source records its link to the disk, its reset count and its map. Another connection (a scan's
    // step, a second source being opened) can hold the catalog's write lock for longer than the store's own wait,
    // which is short so that a status call always answers. The writes try again, so the open does not fail.
    let root = Root::new();
    let store = root.store();
    let other = rusqlite::Connection::open(root.0.join("raw-scans.sqlite")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        other.execute_batch("COMMIT").unwrap();
    });
    let began = Instant::now();
    let s = store.clone();
    let written = finishes_within(20, move || {
        s.link("child", "parent", 4096, 1 << 20)?;
        let resets = s.add_resets("parent", 2)?;
        let mut map = HealthMap::default();
        map.record(HealthRange::new(
            0,
            4096,
            RangeState::Unreadable,
            None,
            "the test marked it",
        ));
        s.save_map("parent", None, &map)?;
        Ok::<_, anyhow::Error>(resets)
    });
    assert_eq!(
        written.map(|result| result.ok()),
        Some(Some(2)),
        "a write to the catalog failed on a lock that was held for a moment"
    );
    assert!(
        began.elapsed() >= std::time::Duration::from_millis(1000),
        "the writes did not wait for the lock: {:?}",
        began.elapsed()
    );
    release.join().unwrap();
    assert_eq!(
        store.link_of("child").unwrap(),
        Some(("parent".into(), 4096))
    );
    assert_eq!(store.load_flags("parent").unwrap().0, 2);
    assert_eq!(store.load_map("parent").unwrap().ranges().len(), 1);
}

#[test]
fn a_status_read_still_answers_at_once_while_a_write_waits_for_the_catalog() {
    let root = Root::new();
    let store = root.store();
    let other = rusqlite::Connection::open(root.0.join("raw-scans.sqlite")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let writer = store.clone();
    let write = std::thread::spawn(move || writer.set_resets("k", 1));
    std::thread::sleep(std::time::Duration::from_millis(400));
    // The write is retrying: it does not hold the store's mutex between tries, so a read goes through.
    let began = Instant::now();
    let reader = store.clone();
    let read = finishes_within(5, move || reader.load_flags("k"));
    assert!(matches!(read, Some(Ok(_))), "{read:?}");
    assert!(
        began.elapsed() < std::time::Duration::from_secs(1),
        "{:?}",
        began.elapsed()
    );
    other.execute_batch("COMMIT").unwrap();
    write.join().unwrap().unwrap();
    assert_eq!(store.load_flags("k").unwrap().0, 1);
}

#[test]
fn closing_and_moving_journal_rows_waits_for_the_journal_file_and_is_not_lost() {
    // The writes that close or move rows (opening a source does them) are the ones that must not be lost, unlike
    // the journal's write while a read is in flight, which is kept in memory when the file is busy.
    let root = Root::new();
    let store = root.store();
    let row = store.started("k", "9999999:1", 0, 4096).unwrap();
    assert!(row > 0, "the file was free: a real row");
    let other = rusqlite::Connection::open(root.0.join("read-journal.sqlite")).unwrap();
    other.execute_batch("BEGIN IMMEDIATE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        other.execute_batch("COMMIT").unwrap();
    });
    let s = store.clone();
    let done = finishes_within(20, move || {
        s.settle(&[row], "never-finished")?;
        s.adopt("k", "source-1")?;
        s.link("k", "disk", 512, 4096)
    });
    assert!(
        done.is_some_and(|result| result.is_ok()),
        "a write to the journal failed on a lock that was held for a moment"
    );
    release.join().unwrap();
    let rows = store.recent("disk", 5).unwrap();
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["outcome"], "never-finished");
}

#[test]
fn opening_the_store_waits_for_the_journal_file_while_another_store_sets_it_up() {
    // Every store of a process sets the journal's file up (its mode and its tables) when it is opened, and sources
    // are opened at the same moment: one holds the file's lock while the others open. They wait for it, as opening
    // a source waits for any lock, and then give up quickly again for the reads of a device.
    let root = Root::new();
    let other = rusqlite::Connection::open(root.0.join("read-journal.sqlite")).unwrap();
    other.execute_batch("BEGIN EXCLUSIVE").unwrap();
    let release = std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_millis(1200));
        other.execute_batch("COMMIT").unwrap();
    });
    let began = Instant::now();
    let path = root.0.clone();
    let opened = finishes_within(20, move || {
        HealthStore::open(&path).map(|store| store.journal_pending())
    });
    assert!(
        opened.is_some_and(|result| result.is_ok()),
        "opening the store failed on a lock that was held for a moment"
    );
    assert!(
        began.elapsed() >= std::time::Duration::from_millis(1000),
        "it did not wait for the lock: {:?}",
        began.elapsed()
    );
    release.join().unwrap();
}
