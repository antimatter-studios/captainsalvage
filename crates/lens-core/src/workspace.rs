//! Where analysis results live. Always on the Mac, never on the examined volume.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[derive(Clone, Debug)]
pub struct Workspace {
    dir: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Baseline {
    pub structure_hash: String,
    pub recorded_at_millis: u64,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BaselineCheck {
    pub unchanged: bool,
    pub first_recording: bool,
    pub recorded_at_millis: u64,
    pub baseline_hash: String,
    pub current_hash: String,
}

impl Workspace {
    pub fn at(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
        Ok(Self { dir })
    }

    pub fn for_serial(serial: u32) -> Result<Self> {
        let home = std::env::var_os("HOME").context("HOME is not set")?;
        Self::at(
            Path::new(&home)
                .join("Library/Application Support/exfat-lens")
                .join(format!("vol-{serial:08x}")),
        )
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn database(&self) -> PathBuf {
        self.dir.join("map.sqlite")
    }

    pub fn classes(&self) -> PathBuf {
        self.dir.join("class.bin")
    }

    pub fn entropy(&self) -> PathBuf {
        self.dir.join("entropy.bin")
    }

    pub fn text_heads(&self) -> PathBuf {
        self.dir.join("texthead.bin")
    }

    pub fn confidence(&self) -> PathBuf {
        self.dir.join("confidence.bin")
    }

    pub fn ownership(&self) -> PathBuf {
        self.dir.join("ownership.bin")
    }

    pub fn lock(&self) -> PathBuf {
        self.dir.join("scan.lock")
    }

    fn baseline(&self) -> PathBuf {
        self.dir.join("baseline.json")
    }

    /// Compares the volume's structures with the hash first recorded here. A
    /// change means something wrote to the boot regions, FAT, bitmap or root.
    pub fn check_baseline(&self, current_hash: &str) -> Result<BaselineCheck> {
        if let Ok(text) = fs::read_to_string(self.baseline()) {
            let baseline: Baseline =
                serde_json::from_str(&text).context("parsing baseline.json")?;
            return Ok(BaselineCheck {
                unchanged: baseline.structure_hash == current_hash,
                first_recording: false,
                recorded_at_millis: baseline.recorded_at_millis,
                baseline_hash: baseline.structure_hash,
                current_hash: current_hash.to_string(),
            });
        }
        let baseline = Baseline {
            structure_hash: current_hash.to_string(),
            recorded_at_millis: now_millis(),
        };
        fs::write(self.baseline(), serde_json::to_string_pretty(&baseline)?)?;
        Ok(BaselineCheck {
            unchanged: true,
            first_recording: true,
            recorded_at_millis: baseline.recorded_at_millis,
            baseline_hash: baseline.structure_hash,
            current_hash: current_hash.to_string(),
        })
    }

    /// Reads a per-cluster map, or an all-zero one if the scan has not started.
    pub fn read_cluster_map(&self, path: &Path, cluster_count: u32) -> Vec<u8> {
        let mut data = fs::read(path).unwrap_or_default();
        data.resize(cluster_count as usize, 0);
        data
    }
}
