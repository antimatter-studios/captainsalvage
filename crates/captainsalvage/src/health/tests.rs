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
        vec![(5 * MIB, 5 * MIB + 8192, Fault::Reset(5))],
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
    let back = FakeDrive::new(data, vec![(5 * MIB, 5 * MIB + 8192, Fault::Reset(5))]);
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

#[test]
fn the_probe_finds_the_edges_of_a_zone_from_the_good_side_without_reading_into_it() {
    let root = Root::new();
    let store = root.store();
    let zone = (20_000_000u64, 23_000_000u64);
    let drive = FakeDrive::new(
        patterned(64 * MIB as usize, 6),
        vec![(zone.0, zone.1, Fault::Error)],
    );
    let health = health_of(&drive, &store);
    probe::start(
        &health,
        Options {
            grid_bytes: 4 * MIB,
            edge_bytes: MIB,
            slow_ms: 500,
        },
    )
    .unwrap();
    let (state, report) = probe::step(
        &health,
        Budget {
            max_reads: 500,
            max_millis: 60_000,
        },
    )
    .unwrap();
    assert!(report.finished, "{report:?}");
    assert_eq!(state.zones_found, 1);
    let map = health.snapshot();
    let bad: Vec<_> = map
        .ranges()
        .iter()
        .filter(|range| range.state == RangeState::Unreadable)
        .collect();
    assert_eq!(
        bad.len(),
        1,
        "one bounded zone absorbs the single bad reads: {bad:?}"
    );
    // The zone holds the whole bad area, with a margin, and not much more than that.
    assert!(
        bad[0].start <= zone.0 && bad[0].start >= zone.0 - 3 * MIB,
        "{bad:?}"
    );
    assert!(
        bad[0].end >= zone.1 && bad[0].end <= zone.1 + 3 * MIB,
        "{bad:?}"
    );
    // The drive was asked about the bad area only a few times: the grid point and the read that
    // found the other edge, never a sweep of it.
    assert!(drive.reads_in(zone.0, zone.1) <= 3, "{:?}", drive.reads());
    // Everything inside the zone is now refused, and just outside it still reads.
    assert!(known_bad(&read(&health, 21 * MIB, 4096).unwrap_err()).is_some());
    assert!(read(&health, 8 * MIB, 4096).is_ok());
}

#[test]
fn the_probe_is_bounded_per_call_and_resumable_after_the_app_is_closed() {
    let root = Root::new();
    let store = root.store();
    let length = 40 * MIB;
    let faults = vec![(25 * MIB, 26 * MIB, Fault::Slow(250))];
    let options = Options {
        grid_bytes: MIB,
        edge_bytes: MIB,
        slow_ms: 20,
    };
    let data = patterned(length as usize, 7);

    let drive = FakeDrive::new(data.clone(), faults.clone());
    let health = health_of(&drive, &store);
    probe::start(&health, options).unwrap();
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
        if report.finished {
            break;
        }
        assert!(calls < 50);
    }
    assert!(calls > 1);
    let resumed = health.snapshot();

    // One call to the end gives the same map.
    let root_two = Root::new();
    let drive3 = FakeDrive::new(data, faults);
    let health_three = health_of(&drive3, &root_two.store());
    probe::start(&health_three, options).unwrap();
    let (_, report) = probe::step(
        &health_three,
        Budget {
            max_reads: 500,
            max_millis: 60_000,
        },
    )
    .unwrap();
    assert!(report.finished);
    // The same map, whatever the timings of the reads were.
    let shape = |map: &HealthMap| {
        map.ranges()
            .iter()
            .map(|range| (range.start, range.end, range.state))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&resumed), shape(&health_three.snapshot()));
    assert_eq!(resumed.count(RangeState::Slow), 1);
}

#[test]
fn a_probe_stops_when_the_device_resets_and_goes_on_after_it_is_rebound_without_reading_the_culprit(
) {
    let root = Root::new();
    let store = root.store();
    let data = patterned(32 * MIB as usize, 8);
    let fault = (13 * MIB, 14 * MIB, Fault::Reset(5));
    let drive = FakeDrive::new(data.clone(), vec![fault]);
    let health = health_of(&drive, &store);
    probe::start(
        &health,
        Options {
            grid_bytes: 4 * MIB,
            edge_bytes: MIB,
            slow_ms: 500,
        },
    )
    .unwrap();
    // The grid reaches 12 MiB, then 16 MiB: 13 MiB is not on it. Make the grid land on the fault.
    drop(health);
    let drive = FakeDrive::new(data.clone(), vec![(12 * MIB, 13 * MIB, Fault::Reset(5))]);
    let health = health_of(&drive, &store);
    probe::start(
        &health,
        Options {
            grid_bytes: 4 * MIB,
            edge_bytes: MIB,
            slow_ms: 500,
        },
    )
    .unwrap();
    let (_, report) = probe::step(
        &health,
        Budget {
            max_reads: 100,
            max_millis: 60_000,
        },
    )
    .unwrap();
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
    let error = probe::step(&health, Budget::default()).unwrap_err();
    assert!(format!("{error:#}").contains("gone away"));

    // The drive comes back, found again, and the culprit stays marked.
    let back = FakeDrive::new(data, vec![(12 * MIB, 13 * MIB, Fault::Reset(5))]);
    health.rebind(back.clone());
    assert!(!health.is_gone());
    let (_, report) = probe::step(
        &health,
        Budget {
            max_reads: 500,
            max_millis: 60_000,
        },
    )
    .unwrap();
    assert!(report.finished, "{report:?}");
    assert_eq!(
        back.reads_in(12 * MIB, 13 * MIB),
        0,
        "the culprit was not read again"
    );
    assert!(!back.is_gone());
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
    assert_eq!(comparison.state, crate::raw_scan::Verification::Probable);
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
