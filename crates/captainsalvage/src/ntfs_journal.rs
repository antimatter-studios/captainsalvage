//! What an NTFS volume's change journal and `$LogFile` say, read without writing the volume.
//!
//! NTFS keeps two records of what recently happened to it. The change journal (`$Extend\$UsnJrnl`, its `$J` stream) lists
//! what was created, renamed, changed and deleted, by name and time. `$LogFile` holds the transactions the file system had
//! begun and may not have finished: the work a replay would redo or undo on a volume that was not unmounted cleanly.
//!
//! Both are returned here as one separate, labelled interpretation, never mixed into the entries the scan finds. Each record
//! says where in its stream it was read from, so a person can find it again. The device is only ever read: the driver's
//! evidence functions take a `BlockRead`, which has no write, and nothing here replays the log onto the volume. What the log
//! holds is evidence of what the volume was doing, not proof that it reached the disk, and the journal's names are the names
//! at the time, which may not be the names now.
//!
//! The window and the capability table's `journal` row are not changed by this: that row stays missing until what is read
//! here has been checked against an independent reader on Windows' own volumes.

use fs_core::BlockRead;
use fs_ntfs::evidence::{self, logfile_state, read_usn_records, usn_journal_state};
use fs_ntfs::logfile::LogfileState;
use fs_ntfs::logfile_replay::walk_records;
use fs_ntfs::usn_image::{ImageJournal, JournalState};
use serde::Serialize;

/// How the interpretation is named wherever it is shown.
pub const INTERPRETATION: &str = "NTFS change journal and $LogFile evidence";

/// What a reader must keep in mind about it.
pub const CAUTION: &str = "Read only, from the volume as it is. This is evidence of what the volume recorded, not proof that it reached the disk, and it is not part of the files the scan finds. The log is never replayed onto the volume. A name here is the name at the time of the record.";

/// Bytes of `$J` examined at most in one call, unless the caller asks for more.
pub const DEFAULT_USN_BYTES: usize = 1 << 20;
/// The most `$J` a call may ask to examine.
pub const MAX_USN_BYTES: usize = 64 << 20;
/// `$LogFile` records returned at most in one call, unless the caller asks for more.
pub const DEFAULT_LOG_RECORDS: usize = 200;
/// The most records a call may ask for.
pub const MAX_LOG_RECORDS: usize = 5000;
/// The largest client data of one `$LogFile` record that is read.
const MAX_LOG_RECORD_BYTES: usize = 1 << 20;

/// Where to start and how much to read. A cursor is what a previous call returned as `nextUsn` or `nextLsn`.
#[derive(Clone, Copy, Debug, Default)]
pub struct Request {
    /// The USN to start at: the journal's first when left out.
    pub from_usn: Option<i64>,
    /// How many bytes of `$J` to examine, within [`MAX_USN_BYTES`].
    pub usn_bytes: Option<usize>,
    /// The LSN to start at: where replay's analysis starts when left out.
    pub from_lsn: Option<u64>,
    /// How many log records to return, within [`MAX_LOG_RECORDS`].
    pub log_records: Option<usize>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalEvidence {
    /// What this is, for any place that shows it.
    pub interpretation: &'static str,
    pub read_only: bool,
    pub caution: &'static str,
    pub usn_journal: UsnEvidence,
    pub log_file: LogEvidence,
}

/// The change journal: its state and bounds, and the records read from it.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsnEvidence {
    /// `absent`, `deleting` (Windows was deleting it: no record is to be relied on), `active`, or `unreadable`.
    pub state: &'static str,
    /// Why, in words, when it is not simply active or absent.
    pub detail: Option<String>,
    pub journal: Option<JournalBounds>,
    /// Where the records come from.
    pub provenance: &'static str,
    pub records: Vec<UsnRecord>,
    /// The USN to continue from, when the journal has more than was read.
    pub next_usn: Option<i64>,
    /// Every record up to the journal's end was read.
    pub finished: bool,
    /// Why reading stopped before the end, with everything read before it kept.
    pub stopped: Option<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct JournalBounds {
    /// The MFT record number of `$Extend\$UsnJrnl`.
    pub record: u64,
    pub journal_id: u64,
    pub first_usn: i64,
    pub next_usn: i64,
    pub lowest_valid_usn: i64,
    pub maximum_size: u64,
    pub allocation_delta: u64,
}

impl From<&ImageJournal> for JournalBounds {
    fn from(journal: &ImageJournal) -> Self {
        Self {
            record: journal.record,
            journal_id: journal.journal_id,
            first_usn: journal.first_usn,
            next_usn: journal.next_usn,
            lowest_valid_usn: journal.lowest_valid_usn,
            maximum_size: journal.maximum_size,
            allocation_delta: journal.allocation_delta,
        }
    }
}

/// One `USN_RECORD_V2`.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UsnRecord {
    pub usn: i64,
    /// Where in `$J` the record is: its USN is its byte offset.
    pub stream_offset: i64,
    pub record_length: u32,
    pub name: String,
    pub file_segment: u64,
    pub file_sequence: u16,
    pub parent_segment: u64,
    pub parent_sequence: u16,
    pub reason: u32,
    /// The reason flags by name (`FILE_CREATE`, `RENAME_OLD_NAME`, ...), in bit order.
    pub reasons: Vec<&'static str>,
    pub source_info: u32,
    pub security_id: u32,
    pub file_attributes: u32,
    /// The raw FILETIME: 100 ns since 1601-01-01 UTC.
    pub time: i64,
    pub iso_time: Option<String>,
}

/// `$LogFile`: what it holds as far as replay is concerned, and its records.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogEvidence {
    /// `empty`, `clean`, `pending` (it may hold transactions a replay would redo), or `unreadable`.
    pub state: &'static str,
    /// The driver's one-line account of the state, or why it could not be read.
    pub detail: String,
    /// Discarding the log, or clearing the dirty flag over it, could lose a transaction.
    pub needs_replay: bool,
    /// Where `$LogFile`'s bytes start on the volume.
    pub volume_offset: Option<u64>,
    pub length: Option<u64>,
    pub provenance: &'static str,
    pub records: Vec<LogRecord>,
    /// The LSN to continue from, when the log has more than was returned.
    pub next_lsn: Option<u64>,
    /// The walk reached the log's newest record.
    pub finished: bool,
    pub stopped: Option<String>,
}

/// One `$LogFile` client record, as its header and the start of its data say. Not replay authority, and the operation
/// codes of a restart record are raw words, not operations.
#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LogRecord {
    pub lsn: u64,
    /// Where the record's header is in `$LogFile`.
    pub log_offset: u64,
    /// Where it is on the volume.
    pub volume_offset: u64,
    pub next_lsn: u64,
    pub record_type: u32,
    pub transaction_id: u32,
    pub client_index: u16,
    pub flags: u16,
    pub client_previous_lsn: u64,
    pub client_undo_next_lsn: u64,
    pub client_data_length: u32,
    pub redo_operation: u16,
    pub undo_operation: u16,
    pub redo_length: u16,
    pub undo_length: u16,
}

const USN_PROVENANCE: &str = "$Extend\\$UsnJrnl:$J, at the byte offset the USN names";
const LOG_PROVENANCE: &str = "$LogFile, at the offset of the record's header";

const REASONS: [(u32, &str); 22] = [
    (0x0000_0001, "DATA_OVERWRITE"),
    (0x0000_0002, "DATA_EXTEND"),
    (0x0000_0004, "DATA_TRUNCATION"),
    (0x0000_0010, "NAMED_DATA_OVERWRITE"),
    (0x0000_0020, "NAMED_DATA_EXTEND"),
    (0x0000_0040, "NAMED_DATA_TRUNCATION"),
    (0x0000_0100, "FILE_CREATE"),
    (0x0000_0200, "FILE_DELETE"),
    (0x0000_0400, "EA_CHANGE"),
    (0x0000_0800, "SECURITY_CHANGE"),
    (0x0000_1000, "RENAME_OLD_NAME"),
    (0x0000_2000, "RENAME_NEW_NAME"),
    (0x0000_4000, "INDEXABLE_CHANGE"),
    (0x0000_8000, "BASIC_INFO_CHANGE"),
    (0x0001_0000, "HARD_LINK_CHANGE"),
    (0x0002_0000, "COMPRESSION_CHANGE"),
    (0x0004_0000, "ENCRYPTION_CHANGE"),
    (0x0008_0000, "OBJECT_ID_CHANGE"),
    (0x0010_0000, "REPARSE_POINT_CHANGE"),
    (0x0020_0000, "STREAM_CHANGE"),
    (0x0040_0000, "TRANSACTED_CHANGE"),
    (0x8000_0000, "CLOSE"),
];

/// The names of the `USN_REASON_*` flags set in `reason`.
pub fn reason_names(reason: u32) -> Vec<&'static str> {
    REASONS
        .iter()
        .filter(|(bit, _)| reason & bit != 0)
        .map(|(_, name)| *name)
        .collect()
}

/// Reads the volume's change journal and `$LogFile` through a device that can only be read.
pub fn read<D: BlockRead + ?Sized>(device: &D, request: &Request) -> JournalEvidence {
    JournalEvidence {
        interpretation: INTERPRETATION,
        read_only: true,
        caution: CAUTION,
        usn_journal: read_usn(device, request),
        log_file: read_log(device, request),
    }
}

fn read_usn<D: BlockRead + ?Sized>(device: &D, request: &Request) -> UsnEvidence {
    let mut found = UsnEvidence {
        state: "unreadable",
        detail: None,
        journal: None,
        provenance: USN_PROVENANCE,
        records: Vec::new(),
        next_usn: None,
        finished: false,
        stopped: None,
    };
    let journal = match usn_journal_state(device) {
        Ok(JournalState::Absent) => {
            found.state = "absent";
            found.detail = Some("the volume has no change journal".into());
            found.finished = true;
            return found;
        }
        Ok(JournalState::Deleting) => {
            found.state = "deleting";
            found.detail = Some(
                "Windows was deleting the change journal; its next mount completes that, and no record in it is to be relied on"
                    .into(),
            );
            return found;
        }
        Ok(JournalState::Active(journal)) => journal,
        Err(error) => {
            found.detail = Some(error.to_string());
            return found;
        }
    };
    found.state = "active";
    found.journal = Some(JournalBounds::from(&journal));
    let mut cursor = journal.first_cursor();
    if let Some(from) = request.from_usn {
        cursor.next_usn = from;
    }
    let bytes = request
        .usn_bytes
        .unwrap_or(DEFAULT_USN_BYTES)
        .clamp(1, MAX_USN_BYTES);
    match read_usn_records(device, &journal, cursor, bytes) {
        Ok(batch) => {
            found.records = batch
                .records
                .iter()
                .map(|record| UsnRecord {
                    usn: record.usn,
                    stream_offset: record.usn,
                    record_length: record.record_length,
                    name: record.file_name_lossy(),
                    file_segment: record.file_segment(),
                    file_sequence: record.file_sequence(),
                    parent_segment: record.parent_file_reference & 0x0000_FFFF_FFFF_FFFF,
                    parent_sequence: (record.parent_file_reference >> 48) as u16,
                    reason: record.reason,
                    reasons: reason_names(record.reason),
                    source_info: record.source_info,
                    security_id: record.security_id,
                    file_attributes: record.file_attributes,
                    time: record.timestamp,
                    iso_time: u64::try_from(record.timestamp)
                        .ok()
                        .and_then(crate::session::filetime_to_iso),
                })
                .collect();
            found.finished = batch.finished;
            if !batch.finished {
                found.next_usn = Some(batch.next.next_usn);
            }
        }
        Err(error) => {
            found.stopped = Some(error.to_string());
        }
    }
    found
}

fn read_log<D: BlockRead + ?Sized>(device: &D, request: &Request) -> LogEvidence {
    let mut found = LogEvidence {
        state: "unreadable",
        detail: String::new(),
        needs_replay: false,
        volume_offset: None,
        length: None,
        provenance: LOG_PROVENANCE,
        records: Vec::new(),
        next_lsn: None,
        finished: false,
        stopped: None,
    };
    let state = match logfile_state(device) {
        Ok(state) => state,
        Err(error) => {
            found.detail = error.to_string();
            return found;
        }
    };
    found.detail = state.describe();
    found.needs_replay = state.needs_replay();
    found.state = match state {
        LogfileState::Empty => "empty",
        LogfileState::Clean(_) => "clean",
        LogfileState::Pending(_) => "pending",
    };
    if matches!(state, LogfileState::Empty) {
        found.finished = true;
        return found;
    }
    let log = match evidence::read_logfile(device) {
        Ok(log) => log,
        Err(error) => {
            found.stopped = Some(error.to_string());
            return found;
        }
    };
    found.volume_offset = Some(log.offset);
    found.length = Some(log.bytes.len() as u64);
    let limit = request
        .log_records
        .unwrap_or(DEFAULT_LOG_RECORDS)
        .clamp(1, MAX_LOG_RECORDS);
    match walk_records(&log.bytes, request.from_lsn, limit, MAX_LOG_RECORD_BYTES) {
        Ok(walk) => {
            found.records = walk
                .records
                .iter()
                .map(|walked| LogRecord {
                    lsn: walked.view.lsn,
                    log_offset: walked.offset,
                    volume_offset: log.offset + walked.offset,
                    next_lsn: walked.next_lsn,
                    record_type: walked.view.record_type,
                    transaction_id: walked.view.transaction_id,
                    client_index: walked.view.client_index,
                    flags: walked.view.flags,
                    client_previous_lsn: walked.view.client_previous_lsn,
                    client_undo_next_lsn: walked.view.client_undo_next_lsn,
                    client_data_length: walked.view.client_data_length,
                    redo_operation: walked.view.redo_operation,
                    undo_operation: walked.view.undo_operation,
                    redo_length: walked.view.redo_length,
                    undo_length: walked.view.undo_length,
                })
                .collect();
            found.finished = walk.complete;
            if !walk.complete {
                found.next_lsn = walk.records.last().map(|walked| walked.next_lsn);
            }
            found.stopped = walk.stopped.map(|error| error.to_string());
        }
        Err(error) => found.stopped = Some(error.to_string()),
    }
    found
}
