//! Tests found files in the background, so a score rests on the file's real bytes.
//!
//! Each file is read once from the disk, walked by its format and, for images, decoded.
//! The result and a hash of the bytes go in the map database on this Mac. Nothing is
//! written to the disk. A file that passes is trusted, and one that fails is marked,
//! whatever its directory entry and clusters suggested. The run is low priority, and it
//! stops and resumes without repeating a file.

use crate::browse::{group_id_of, make_decode_check};
use crate::session::Session;
use crate::verify::verify_stream;
use anyhow::{anyhow, bail, Result};
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Testing waits this long after anything the person looks at, so previews stay quick.
const QUIET_AFTER_VIEWING: Duration = Duration::from_secs(2);
const QUIET_CHECK: Duration = Duration::from_millis(250);
/// macOS keeps file details in a companion named `._name`, which is not a copy of the file.
const COMPANION_PREFIX: &str = "._";
/// A macOS alias. It points at a file, whatever its own name says.
const ALIAS_SIGNATURE: &[u8] = b"book\0\0\0\0mark";

pub const OUTCOME_PASSED: &str = "passed";
pub const OUTCOME_DAMAGED: &str = "damaged";
pub const OUTCOME_UNTESTED: &str = "untested";
/// Formats a player reads past damage in: it finds the next frame and carries on, so a file
/// that is mostly valid still plays, with a gap where each break is.
const RESYNC_FORMATS: &[&str] = &["mp3"];
/// How much of such a file must be valid frames for it to play with gaps rather than fail.
/// On a real volume, files failed by small files that reused the starts of some of their
/// clusters were 80 to 99% valid, and the frames carried on after each overwritten stretch.
pub const GAPS_VALID_FRACTION: f64 = 0.8;

/// Whether a failed test still leaves a file that plays, with gaps where it is damaged.
pub fn plays_with_gaps(outcome: &str, format: &str, valid_fraction: Option<f64>) -> bool {
    outcome == OUTCOME_DAMAGED
        && RESYNC_FORMATS.contains(&format)
        && valid_fraction.is_some_and(|valid| valid >= GAPS_VALID_FRACTION)
}
/// Larger files are tested only if their type can be walked as it is read, which needs no
/// more than one piece of the file in memory at a time.
pub const MAX_TESTED_BYTES: u64 = 64 * 1024 * 1024;
/// How much of a large file is read at a time.
const STREAM_PIECE_BYTES: u64 = 4 * 1024 * 1024;
/// A walker can fall short of every byte on a good file, such as a tag at the end.
const PASS_VALID_FRACTION: f64 = 0.99;
const SAVE_EVERY_FILES: usize = 64;
const SAVE_EVERY: Duration = Duration::from_secs(3);
const STATE_IDLE: &str = "idle";
const STATE_RUNNING: &str = "running";
const STATE_STOPPED: &str = "stopped";
const STATE_FINISHED: &str = "finished";
const STATE_FAILED: &str = "failed";
/// Image types with no format walker. JPEG and PNG have strong walkers, and walking one
/// costs a small part of decoding it, which matters across hundreds of thousands of files.
const DECODE_ONLY_EXTENSIONS: &[&str] = &["gif", "bmp", "webp", "tif", "tiff"];
/// Walker formats that a decoder can also read.
const PICTURE_FORMATS: &[&str] = &["jpeg", "png"];

/// What one file's bytes turned out to be.
#[derive(Clone, Debug, PartialEq)]
pub struct Judgement {
    pub outcome: &'static str,
    pub format: String,
    pub valid_fraction: Option<f64>,
    pub first_break: Option<u64>,
    pub decoded: Option<bool>,
    pub note: String,
}

fn needs_decoding(name: &str) -> bool {
    let extension = name
        .rsplit_once('.')
        .map(|(_, extension)| extension.to_ascii_lowercase())
        .unwrap_or_default();
    DECODE_ONLY_EXTENSIONS.contains(&extension.as_str())
}

/// Judges a file's bytes, by walking its format, or by decoding it if it has no walker.
pub fn judge(name: &str, data: &[u8]) -> Judgement {
    let not_the_file = if name.starts_with(COMPANION_PREFIX) {
        Some((
            "companion",
            "This is a macOS companion file, not a copy of the file it names.",
        ))
    } else if data.starts_with(ALIAS_SIGNATURE) {
        Some((
            "alias",
            "This is a macOS alias, which points at a file and holds none of it.",
        ))
    } else {
        None
    };
    if let Some((format, note)) = not_the_file {
        return Judgement {
            outcome: OUTCOME_UNTESTED,
            format: format.into(),
            valid_fraction: None,
            first_break: None,
            decoded: None,
            note: note.into(),
        };
    }
    let report = verify_stream(name, data);
    let has_walker = report.format != "unknown";
    let valid = has_walker.then_some(report.valid_fraction);
    let first_break = report.breaks.first().copied();
    let walk_failed = has_walker
        && (report.valid_fraction < PASS_VALID_FRACTION
            || !report.reached_end
            || first_break.is_some());
    // A picture that fails its walk can still show most of itself, so a decoder gives a
    // second opinion. Only failures pay for it, which keeps the test fast.
    let picture_failed = walk_failed && PICTURE_FORMATS.contains(&report.format.as_str());
    let decode_error = (needs_decoding(name) || picture_failed)
        .then(|| make_decode_check(data).err())
        .map(|error| error.map(|e| format!("{e:#}")));
    let decoded = decode_error.as_ref().map(Option::is_none);
    let format = if has_walker {
        report.format.clone()
    } else {
        "image".to_string()
    };

    if !has_walker && decoded.is_none() {
        return Judgement {
            outcome: OUTCOME_UNTESTED,
            format: "unknown".into(),
            valid_fraction: None,
            first_break: None,
            decoded: None,
            note: "There is no test for this file type yet.".into(),
        };
    }
    if walk_failed {
        let label = report.format.to_uppercase();
        let percent = (report.valid_fraction * 100.0).floor();
        let place = match first_break {
            Some(offset) => format!(", and it breaks at byte {offset}"),
            None => ", and the file stops before its end".to_string(),
        };
        let opens = match decoded {
            Some(true) => " It still opens as a picture, so some of it may be fine.",
            Some(false) => " It will not open as a picture.",
            None => "",
        };
        return Judgement {
            outcome: OUTCOME_DAMAGED,
            format,
            valid_fraction: valid,
            first_break,
            decoded,
            note: format!("Tested: {percent}% of its {label} structure is valid{place}.{opens}"),
        };
    }
    if let Some(Some(error)) = &decode_error {
        return Judgement {
            outcome: OUTCOME_DAMAGED,
            format,
            valid_fraction: valid,
            first_break,
            decoded,
            note: format!("Tested: it would not decode as an image ({error})."),
        };
    }
    let label = format.to_uppercase();
    let note = match decoded {
        Some(true) if has_walker => {
            format!("Tested: its {label} structure is whole, and it decodes.")
        }
        Some(true) => "Tested: it decodes as an image.".to_string(),
        _ => format!("Tested: its {label} structure is whole."),
    };
    Judgement {
        outcome: OUTCOME_PASSED,
        format,
        valid_fraction: valid,
        first_break,
        decoded,
        note,
    }
}

/// Judges a large file from a walk made as it was read. There is no decoding for these types.
pub fn judge_walk(report: &crate::verify::VerifyReport) -> Judgement {
    if report.format == "unknown" {
        return Judgement {
            outcome: OUTCOME_UNTESTED,
            format: "unknown".into(),
            valid_fraction: None,
            first_break: None,
            decoded: None,
            note: "There is no test for this file's contents yet.".into(),
        };
    }
    let first_break = report.breaks.first().copied();
    let failed =
        report.valid_fraction < PASS_VALID_FRACTION || !report.reached_end || first_break.is_some();
    let label = report.format.to_uppercase();
    let note = if failed {
        let percent = (report.valid_fraction * 100.0).floor();
        let place = match first_break {
            Some(offset) => format!(", and it breaks at byte {offset}"),
            None => ", and the file stops before its end".to_string(),
        };
        format!("Tested: {percent}% of its {label} structure is valid{place}.")
    } else {
        format!("Tested: its {label} structure is whole.")
    };
    Judgement {
        outcome: if failed {
            OUTCOME_DAMAGED
        } else {
            OUTCOME_PASSED
        },
        format: report.format.clone(),
        valid_fraction: Some(report.valid_fraction),
        first_break,
        decoded: None,
        note,
    }
}

/// One stored result.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTest {
    pub id: i64,
    pub first_cluster: u32,
    pub length: u64,
    pub outcome: String,
    pub format: String,
    pub valid_fraction: Option<f64>,
    pub first_break: Option<u64>,
    pub decoded: Option<bool>,
    pub note: String,
    pub sha256: Option<String>,
    pub tested_at_millis: u64,
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileTestStatus {
    pub state: String,
    pub groups: Vec<String>,
    pub total: u64,
    pub done: u64,
    pub bytes_done: u64,
    pub current: String,
    pub error: Option<String>,
    pub passed: u64,
    pub damaged: u64,
    pub untested: u64,
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis() as u64)
}

/// Narrows a test run beyond file types: these entries, verdicts, reasons, extensions or sizes.
/// Every part given must match; a part left empty matches everything.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct TestFilter {
    pub ids: Vec<i64>,
    pub verdicts: Vec<String>,
    pub reasons: Vec<String>,
    pub extensions: Vec<String>,
    pub min_bytes: Option<u64>,
}

impl TestFilter {
    pub fn is_empty(&self) -> bool {
        self.ids.is_empty()
            && self.verdicts.is_empty()
            && self.reasons.is_empty()
            && self.extensions.is_empty()
            && self.min_bytes.is_none()
    }

    fn admits(&self, candidate: &crate::map::FileTestCandidate) -> bool {
        let listed = |list: &[String], value: Option<&str>| {
            list.is_empty() || value.is_some_and(|value| list.iter().any(|item| item == value))
        };
        let extension = crate::browse::extension_of(&candidate.name);
        (self.ids.is_empty() || self.ids.contains(&candidate.id))
            && listed(&self.verdicts, candidate.verdict.as_deref())
            && listed(&self.reasons, candidate.reason_code.as_deref())
            && listed(&self.extensions, Some(extension.as_str()))
            && self.min_bytes.is_none_or(|min| candidate.length >= min)
    }
}

impl Session {
    pub fn file_test_status(&self) -> Result<FileTestStatus> {
        let mut status = self
            .file_tests
            .status
            .lock()
            .map_err(|_| anyhow!("file test lock poisoned"))?
            .clone();
        if status.state.is_empty() {
            status.state = STATE_IDLE.into();
            let counts = self.with_database(|database| database.file_test_counts())?;
            status.passed = counts.0;
            status.damaged = counts.1;
            status.untested = counts.2;
            status.done = counts.0 + counts.1 + counts.2;
        }
        Ok(status)
    }

    pub fn file_test_stop(&self) {
        self.file_tests.stop.store(true, Ordering::Relaxed);
    }

    fn update_file_tests(&self, change: impl FnOnce(&mut FileTestStatus)) {
        if let Ok(mut status) = self.file_tests.status.lock() {
            change(&mut status);
        }
    }

    /// Marks that the person is looking at something on the disk, so testing gives way.
    pub fn note_viewing(&self) {
        self.file_tests
            .last_viewed
            .store(now_millis(), Ordering::Relaxed);
    }

    fn viewed_recently(&self, within: Duration) -> bool {
        now_millis().saturating_sub(self.file_tests.last_viewed.load(Ordering::Relaxed))
            < within.as_millis() as u64
    }

    /// Forgets results so files are tested again, after a check has improved. Scores go
    /// back at the rebuild this does. With no outcome given, it forgets every result.
    pub fn file_tests_forget(&self, outcome: Option<&str>) -> Result<u64> {
        if self.file_test_status()?.state == STATE_RUNNING {
            bail!("Pause the file tests first.");
        }
        let forgotten = self.with_database(|database| database.forget_file_tests(outcome))?;
        self.resolve()?;
        if let Ok(mut status) = self.file_tests.status.lock() {
            *status = FileTestStatus::default();
        }
        Ok(forgotten)
    }

    /// Starts testing these types of file, skipping any already tested.
    pub fn file_test_start(
        self: &Arc<Self>,
        groups: Vec<String>,
        filter: TestFilter,
    ) -> Result<FileTestStatus> {
        if groups.is_empty() && filter.is_empty() {
            bail!("Choose at least one type of file to test.");
        }
        let mut slot = self
            .file_tests
            .thread
            .lock()
            .map_err(|_| anyhow!("file test lock poisoned"))?;
        if slot.as_ref().is_some_and(|handle| !handle.is_finished()) {
            bail!("Files are already being tested.");
        }
        self.file_tests.stop.store(false, Ordering::Relaxed);
        let chosen = self.with_database(|database| database.file_test_candidates(u64::MAX >> 1))?;
        let chosen: Vec<_> = chosen
            .into_iter()
            .filter(|candidate| {
                candidate.length <= MAX_TESTED_BYTES || crate::stream_walk::streams(&candidate.name)
            })
            .filter(|candidate| {
                groups.is_empty()
                    || groups
                        .iter()
                        .any(|group| group == group_id_of(&candidate.name))
            })
            .filter(|candidate| filter.admits(candidate))
            .collect();
        let counts = self.with_database(|database| database.file_test_counts())?;
        let status = FileTestStatus {
            state: STATE_RUNNING.into(),
            groups,
            total: chosen.len() as u64,
            passed: counts.0,
            damaged: counts.1,
            untested: counts.2,
            ..FileTestStatus::default()
        };
        *self
            .file_tests
            .status
            .lock()
            .map_err(|_| anyhow!("file test lock poisoned"))? = status.clone();
        let session = Arc::clone(self);
        *slot = Some(std::thread::spawn(move || {
            let outcome = session.run_file_tests(chosen);
            // The map, the folder totals and the summary pick up the results here.
            let rebuilt = session.resolve().map(|_| ());
            session.update_file_tests(|status| {
                status.current.clear();
                match outcome.and_then(|finished| rebuilt.map(|()| finished)) {
                    Ok(true) => status.state = STATE_FINISHED.into(),
                    Ok(false) => status.state = STATE_STOPPED.into(),
                    Err(error) => {
                        status.state = STATE_FAILED.into();
                        status.error = Some(format!("{error:#}"));
                    }
                }
            });
        }));
        Ok(status)
    }

    /// Tests a large file by walking it as it is read, a piece at a time, hashing as it goes.
    /// None when the tests were stopped part way through.
    fn test_in_pieces(
        &self,
        candidate: &crate::map::FileTestCandidate,
    ) -> Result<Option<(Judgement, Vec<u8>, u64)>> {
        let mut walk = crate::stream_walk::StreamWalk::new(&candidate.name, candidate.length)
            .ok_or_else(|| anyhow!("{} cannot be walked in pieces", candidate.name))?;
        let mut hasher = Sha256::new();
        let mut at = 0u64;
        while at < candidate.length {
            if self.file_tests.stop.load(Ordering::Relaxed) {
                return Ok(None);
            }
            while self.viewed_recently(QUIET_AFTER_VIEWING)
                && !self.file_tests.stop.load(Ordering::Relaxed)
            {
                std::thread::sleep(QUIET_CHECK);
            }
            let part = self.read_entry_range(candidate.id, at, STREAM_PIECE_BYTES)?;
            if part.data.is_empty() {
                break;
            }
            walk.feed(&part.data);
            hasher.update(&part.data);
            at += part.data.len() as u64;
            self.update_file_tests(|status| status.bytes_done += part.data.len() as u64);
        }
        Ok(Some((
            judge_walk(&walk.finish()),
            hasher.finalize().to_vec(),
            at,
        )))
    }

    fn run_file_tests(&self, chosen: Vec<crate::map::FileTestCandidate>) -> Result<bool> {
        let mut waiting: Vec<FileTest> = Vec::new();
        let mut last_save = Instant::now();
        for candidate in chosen {
            if self.file_tests.stop.load(Ordering::Relaxed) {
                self.with_database(|database| database.save_file_tests(&waiting))?;
                return Ok(false);
            }
            self.update_file_tests(|status| status.current = candidate.name.clone());
            while self.viewed_recently(QUIET_AFTER_VIEWING)
                && !self.file_tests.stop.load(Ordering::Relaxed)
            {
                std::thread::sleep(QUIET_CHECK);
            }
            let tested = if candidate.length > MAX_TESTED_BYTES {
                self.test_in_pieces(&candidate)
            } else {
                self.read_entry_range(candidate.id, 0, candidate.length)
                    .map(|part| {
                        let hash = Sha256::digest(&part.data);
                        Some((
                            judge(&candidate.name, &part.data),
                            hash.to_vec(),
                            part.data.len() as u64,
                        ))
                    })
            };
            let (judgement, hash, bytes_read) = match tested {
                Ok(Some(found)) => found,
                // Stopped part way through a large file: nothing is kept for it.
                Ok(None) => {
                    self.with_database(|database| database.save_file_tests(&waiting))?;
                    return Ok(false);
                }
                Err(error) => {
                    self.with_database(|database| database.save_file_tests(&waiting))?;
                    return Err(error);
                }
            };
            let sha256: String = hash.iter().map(|byte| format!("{byte:02x}")).collect();
            self.update_file_tests(|status| {
                status.done += 1;
                if candidate.length <= MAX_TESTED_BYTES {
                    status.bytes_done += bytes_read;
                }
                match judgement.outcome {
                    OUTCOME_PASSED => status.passed += 1,
                    OUTCOME_DAMAGED => status.damaged += 1,
                    _ => status.untested += 1,
                }
            });
            waiting.push(FileTest {
                id: candidate.id,
                first_cluster: candidate.first_cluster,
                length: candidate.length,
                outcome: judgement.outcome.into(),
                format: judgement.format,
                valid_fraction: judgement.valid_fraction,
                first_break: judgement.first_break,
                decoded: judgement.decoded,
                note: judgement.note,
                sha256: Some(sha256),
                tested_at_millis: now_millis(),
            });
            if waiting.len() >= SAVE_EVERY_FILES || last_save.elapsed() >= SAVE_EVERY {
                self.with_database(|database| database.save_file_tests(&waiting))?;
                waiting.clear();
                last_save = Instant::now();
            }
        }
        self.with_database(|database| database.save_file_tests(&waiting))?;
        Ok(true)
    }
}

#[cfg(test)]
mod gap_tests {
    use super::*;

    #[test]
    fn only_a_mostly_valid_stream_that_resyncs_plays_with_gaps() {
        assert!(plays_with_gaps(OUTCOME_DAMAGED, "mp3", Some(0.96)));
        assert!(!plays_with_gaps(OUTCOME_DAMAGED, "mp3", Some(0.4)));
        assert!(
            !plays_with_gaps(OUTCOME_DAMAGED, "mp4", Some(0.96)),
            "an MP4 needs its index whole"
        );
        assert!(!plays_with_gaps(OUTCOME_PASSED, "mp3", Some(1.0)));
        assert!(!plays_with_gaps(OUTCOME_DAMAGED, "mp3", None));
    }
}
