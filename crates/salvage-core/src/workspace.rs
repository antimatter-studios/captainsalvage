//! Where analysis results live. Always on the Mac, never on the examined volume.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// Where the app keeps everything it knows, under the user's Application Support folder.
const APP_FOLDER: &str = "Captain Salvage";
/// The working name's folder, from before the app had its real name.
const OLD_APP_FOLDER: &str = "exfat-lens";

/// The app's own folder: settings, and one workspace per volume with its map and canvases.
///
/// The first time, a folder kept under the working name is moved here whole. A move within one
/// disk changes no file, so a map and weeks of canvas work arrive exactly as they were. The old
/// name is left as a link, so a copy of the app from before still finds the same files.
pub fn app_support() -> Result<PathBuf> {
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    app_folder_in(&Path::new(&home).join("Library/Application Support"))
}

/// The app's folder inside one Application Support folder, moving the old one there first.
pub fn app_folder_in(support: &Path) -> Result<PathBuf> {
    let folder = support.join(APP_FOLDER);
    let old = support.join(OLD_APP_FOLDER);
    let old_is_folder = fs::symlink_metadata(&old).is_ok_and(|found| found.is_dir());
    if old_is_folder && !folder.exists() && fs::rename(&old, &folder).is_ok() {
        let _ = std::os::unix::fs::symlink(APP_FOLDER, &old);
    }
    fs::create_dir_all(&folder).with_context(|| format!("creating {}", folder.display()))?;
    Ok(folder)
}

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

    /// The folder that holds one workspace per volume.
    pub fn root() -> Result<PathBuf> {
        app_support()
    }

    pub fn for_serial(serial: u32) -> Result<Self> {
        Self::at(Self::root()?.join(format!("vol-{serial:08x}")))
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
