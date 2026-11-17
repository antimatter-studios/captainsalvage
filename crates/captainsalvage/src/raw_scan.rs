use crate::disk::{DiscoveryBatch, RawDisk, ReadFailure, DISCOVERY_BATCH_BYTES};
use crate::filesystem::probe::Evidence;
use crate::workspace::{now_millis, Workspace};
use anyhow::{ensure, Context, Result};
use rusqlite::{params, Connection, TransactionBehavior};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

const FINGERPRINT_VERSION: u32 = 1;
const SCANNER_VERSION: u32 = 1;
const SAMPLE_BYTES: u64 = 4096;
const SAMPLE_COUNT: u64 = 16;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Sample {
    pub offset: u64,
    pub length: u64,
    pub sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Fingerprint {
    pub version: u32,
    pub length: u64,
    pub samples: Vec<Sample>,
    pub sampled_sha256: Option<String>,
}

#[derive(Clone, Copy, Debug, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum Verification {
    SampledMatch,
    Mismatch,
    Unverifiable,
    Incompatible,
}

impl Fingerprint {
    pub fn read(disk: &RawDisk) -> Self {
        let length = disk.length.min(SAMPLE_BYTES);
        let last = disk.length - length;
        let offsets: BTreeSet<u64> = (0..SAMPLE_COUNT)
            .map(|index| {
                if index == SAMPLE_COUNT - 1 {
                    last
                } else {
                    (u128::from(last) * u128::from(index) / u128::from(SAMPLE_COUNT - 1)) as u64
                        / SAMPLE_BYTES
                        * SAMPLE_BYTES
                }
            })
            .collect();
        let samples: Vec<Sample> = offsets
            .into_iter()
            .map(|offset| Sample {
                offset,
                length,
                sha256: disk.read(offset, length).ok().map(|data| hash(&data)),
            })
            .collect();
        let sampled_sha256 = samples
            .iter()
            .all(|sample| sample.sha256.is_some())
            .then(|| {
                let mut digest = Sha256::new();
                digest.update(b"Captain Salvage sampled disk fingerprint\0");
                digest.update(FINGERPRINT_VERSION.to_le_bytes());
                digest.update(disk.length.to_le_bytes());
                for sample in &samples {
                    digest.update(sample.offset.to_le_bytes());
                    digest.update(sample.length.to_le_bytes());
                    digest.update(sample.sha256.as_ref().unwrap().as_bytes());
                }
                hex(&digest.finalize())
            });
        Self {
            version: FINGERPRINT_VERSION,
            length: disk.length,
            samples,
            sampled_sha256,
        }
    }

    pub fn verify(&self, current: &Self) -> Verification {
        if self.version != current.version {
            Verification::Incompatible
        } else if self.length != current.length {
            Verification::Mismatch
        } else if self.sampled_sha256.is_none() || current.sampled_sha256.is_none() {
            Verification::Unverifiable
        } else if self.samples == current.samples && self.sampled_sha256 == current.sampled_sha256 {
            Verification::SampledMatch
        } else {
            Verification::Mismatch
        }
    }
}

fn hash(data: &[u8]) -> String {
    hex(&Sha256::digest(data))
}

fn hex(data: &[u8]) -> String {
    data.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn identifier() -> Result<String> {
    let mut bytes = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(hex(&bytes))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSummary {
    pub id: String,
    pub scan_start: u64,
    pub scan_end: u64,
    pub next_offset: u64,
    pub updated_at_millis: u64,
    pub verification: Verification,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SavedScans {
    pub fingerprint: Fingerprint,
    pub scans: Vec<ScanSummary>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanSnapshot {
    pub scan_id: String,
    pub scan_start: u64,
    pub scan_end: u64,
    pub next_offset: u64,
    pub findings: Vec<Evidence>,
    pub errors: Vec<ReadFailure>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScanStep {
    pub scan_id: String,
    #[serde(flatten)]
    pub batch: DiscoveryBatch,
}

pub struct ScanStore {
    connection: Connection,
}

impl ScanStore {
    pub fn for_user() -> Result<Self> {
        Self::open(&Workspace::root()?.join("raw-scans.sqlite"))
    }

    pub fn open(path: &Path) -> Result<Self> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = FULL;
             CREATE TABLE IF NOT EXISTS raw_scans (
                 id TEXT PRIMARY KEY, fingerprint TEXT NOT NULL, source_length INTEGER NOT NULL,
                 source_path TEXT NOT NULL, scanner_version INTEGER NOT NULL,
                 scan_start INTEGER NOT NULL, scan_end INTEGER NOT NULL, next_offset INTEGER NOT NULL,
                 source_token TEXT NOT NULL, attempt_start INTEGER NOT NULL,
                 created_at_millis INTEGER NOT NULL, updated_at_millis INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS raw_scans_length ON raw_scans(source_length);
             CREATE TABLE IF NOT EXISTS raw_findings (
                 scan_id TEXT NOT NULL REFERENCES raw_scans(id), offset INTEGER NOT NULL,
                 format TEXT NOT NULL, structure TEXT NOT NULL, evidence TEXT NOT NULL,
                 PRIMARY KEY(scan_id, offset, format, structure)
             );
             CREATE TABLE IF NOT EXISTS raw_failures (
                 scan_id TEXT NOT NULL REFERENCES raw_scans(id), offset INTEGER NOT NULL,
                 length INTEGER NOT NULL, error TEXT NOT NULL, source_token TEXT NOT NULL,
                 PRIMARY KEY(scan_id, offset)
             );",
        )?;
        Ok(Self { connection })
    }

    pub fn list(&self, disk: &RawDisk) -> Result<SavedScans> {
        let fingerprint = Fingerprint::read(disk);
        let path = disk.path.to_string_lossy();
        let mut statement = self.connection.prepare(
            "SELECT id, fingerprint, source_path, scanner_version, scan_start, scan_end,
                    next_offset, updated_at_millis FROM raw_scans
             WHERE source_length = ?1 OR source_path = ?2 ORDER BY updated_at_millis DESC",
        )?;
        let mut rows = statement.query(params![i64::try_from(disk.length)?, path])?;
        let mut scans = Vec::new();
        while let Some(row) = rows.next()? {
            let saved: Fingerprint = serde_json::from_str(&row.get::<_, String>(1)?)?;
            let verification = if row.get::<_, u32>(3)? != SCANNER_VERSION {
                Verification::Incompatible
            } else {
                saved.verify(&fingerprint)
            };
            if verification != Verification::SampledMatch && row.get::<_, String>(2)? != path {
                continue;
            }
            scans.push(ScanSummary {
                id: row.get(0)?,
                scan_start: row.get::<_, i64>(4)? as u64,
                scan_end: row.get::<_, i64>(5)? as u64,
                next_offset: row.get::<_, i64>(6)? as u64,
                updated_at_millis: row.get::<_, i64>(7)? as u64,
                verification,
            });
        }
        Ok(SavedScans { fingerprint, scans })
    }

    pub fn create(&mut self, disk: &RawDisk, start: u64, length: u64) -> Result<ScanSnapshot> {
        ensure!(
            length > 0
                && start
                    .checked_add(length)
                    .is_some_and(|end| end <= disk.length),
            "scan range is outside the raw source"
        );
        ensure!(
            disk.length <= i64::MAX as u64,
            "source exceeds saved scan offset limits"
        );
        let mut bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let fingerprint = Fingerprint::read(disk);
        ensure!(
            !disk.source.is_gone(),
            "The disk has been disconnected; scan not started."
        );
        let id = identifier()?;
        let token = identifier()?;
        let now = now_millis();
        self.connection.execute(
            "INSERT INTO raw_scans VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?6, ?8, ?6, ?9, ?9)",
            params![
                id,
                serde_json::to_string(&fingerprint)?,
                disk.length as i64,
                disk.path.to_string_lossy(),
                SCANNER_VERSION,
                start as i64,
                (start + length) as i64,
                token,
                now as i64
            ],
        )?;
        bindings.insert(id.clone(), token);
        self.snapshot(&id)
    }

    pub fn resume(
        &mut self,
        disk: &RawDisk,
        id: &str,
        accept_sampled_match: bool,
    ) -> Result<ScanSnapshot> {
        ensure!(accept_sampled_match, "Resuming requires acknowledgement that sampled matches do not prove the entire source is identical.");
        let mut bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let current = Fingerprint::read(disk);
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (fingerprint, scanner_version): (String, u32) = transaction
            .query_row(
                "SELECT fingerprint, scanner_version FROM raw_scans WHERE id = ?1",
                [id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .context("saved scan not found")?;
        ensure!(
            scanner_version == SCANNER_VERSION,
            "Saved scan uses an incompatible scanner; start a new scan."
        );
        let saved: Fingerprint = serde_json::from_str(&fingerprint)?;
        ensure!(
            saved.verify(&current) == Verification::SampledMatch,
            "Source fingerprint differs or could not be verified; saved scan was not resumed."
        );
        let token = identifier()?;
        transaction.execute(
            "UPDATE raw_scans SET next_offset = min(next_offset,
                coalesce((SELECT min(offset) FROM raw_failures WHERE scan_id = ?1), next_offset)),
                source_token = ?2, source_path = ?3, updated_at_millis = ?4 WHERE id = ?1",
            params![id, token, disk.path.to_string_lossy(), now_millis() as i64],
        )?;
        transaction.execute(
            "UPDATE raw_scans SET attempt_start = next_offset WHERE id = ?1",
            [id],
        )?;
        transaction.commit()?;
        bindings.insert(id.into(), token);
        self.snapshot(id)
    }

    pub fn current(&self, disk: &RawDisk, id: &str) -> Result<ScanSnapshot> {
        let bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let token = bindings
            .get(id)
            .context("Verify and load this saved scan before continuing.")?;
        let transaction = self.connection.unchecked_transaction()?;
        let saved_token: String = transaction.query_row(
            "SELECT source_token FROM raw_scans WHERE id = ?1",
            [id],
            |row| row.get(0),
        )?;
        ensure!(
            *token == saved_token,
            "This scan was opened elsewhere; verify and load it again."
        );
        let result = self.snapshot(id)?;
        transaction.commit()?;
        Ok(result)
    }

    pub fn step(&mut self, disk: &RawDisk, id: &str) -> Result<ScanStep> {
        let bindings = disk
            .scan_tokens
            .lock()
            .map_err(|_| anyhow::anyhow!("scan lock poisoned"))?;
        let token = bindings
            .get(id)
            .context("Verify and load this saved scan before continuing.")?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (start, end, saved_token, attempt_start): (i64, i64, String, i64) = transaction.query_row(
            "SELECT next_offset, scan_end, source_token, attempt_start FROM raw_scans WHERE id = ?1", [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        ensure!(
            *token == saved_token,
            "This scan was opened elsewhere; verify and load it again."
        );
        ensure!(start < end, "Scan is already complete.");
        let batch = disk.discover(
            start as u64,
            ((end - start) as u64).min(DISCOVERY_BATCH_BYTES),
        )?;
        ensure!(
            batch.next_offset > start as u64 && batch.next_offset <= end as u64,
            "Discovery did not advance within the saved range."
        );
        transaction.execute(
            "DELETE FROM raw_findings WHERE scan_id = ?1 AND offset >= ?2 AND offset < ?3",
            params![id, start, batch.next_offset as i64],
        )?;
        for evidence in &batch.findings {
            transaction.execute(
                "INSERT OR REPLACE INTO raw_findings VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    evidence.offset as i64,
                    evidence.format,
                    evidence.structure,
                    serde_json::to_string(evidence)?
                ],
            )?;
        }
        transaction.execute(
            "DELETE FROM raw_failures WHERE scan_id = ?1 AND source_token != ?2
            AND offset >= ?3 AND offset + length <= ?4",
            params![id, token, attempt_start, batch.next_offset as i64],
        )?;
        for failure in &batch.errors {
            transaction.execute(
                "INSERT OR REPLACE INTO raw_failures VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    id,
                    failure.offset as i64,
                    failure.length as i64,
                    failure.error,
                    token
                ],
            )?;
        }
        transaction.execute(
            "UPDATE raw_scans SET next_offset = ?2, updated_at_millis = ?3 WHERE id = ?1",
            params![id, batch.next_offset as i64, now_millis() as i64],
        )?;
        transaction.commit()?;
        Ok(ScanStep {
            scan_id: id.into(),
            batch,
        })
    }

    fn snapshot(&self, id: &str) -> Result<ScanSnapshot> {
        let (start, end, next, token): (i64, i64, i64, String) = self.connection.query_row(
            "SELECT scan_start, scan_end, next_offset, source_token FROM raw_scans WHERE id = ?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        let mut findings = Vec::new();
        let mut statement = self.connection.prepare("SELECT evidence FROM raw_findings WHERE scan_id = ?1 ORDER BY offset, format, structure")?;
        let mut rows = statement.query([id])?;
        while let Some(row) = rows.next()? {
            findings.push(serde_json::from_str(&row.get::<_, String>(0)?)?);
        }
        let mut statement = self.connection.prepare(
            "SELECT offset, length, error FROM raw_failures
            WHERE scan_id = ?1 AND source_token = ?2 ORDER BY offset",
        )?;
        let errors = statement
            .query_map(params![id, token], |row| {
                Ok(ReadFailure {
                    offset: row.get::<_, i64>(0)? as u64,
                    length: row.get::<_, i64>(1)? as u64,
                    error: row.get(2)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(ScanSnapshot {
            scan_id: id.into(),
            scan_start: start as u64,
            scan_end: end as u64,
            next_offset: next as u64,
            findings,
            errors,
        })
    }
}

#[cfg(test)]
mod tests;
