//! The single entry point for every action, from the app or from MCP.

use crate::browse::BrowseQuery;
use crate::content::GROUP_NAMES;
use crate::disk_rebuild::Place;
use crate::export::{list_volumes, CopyRequest};
use crate::exporting::{ExportOptions, RebuildCopyOptions};
use crate::map::{from_extents, to_extents, Extent, ResolveParams};
use crate::media::MediaServer;
use crate::script::{run_script, DEFAULT_TIMEOUT_SECONDS};
use crate::session::{disk_access_report, list_disks, whole_disk_path, Layer, Scope, Session};
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};

mod opening;
mod rescan;

/// Receives events the app's window should react to.
pub type UiSink = Arc<dyn Fn(&str, Value) + Send + Sync>;

/// Captures the app window as PNG bytes.
pub type ScreenshotHook = Arc<dyn Fn() -> Result<Vec<u8>> + Send + Sync>;
/// Runs a tool that belongs to the app rather than the engine, such as Finder or the window's
/// language, so MCP clients can do what the window does.
pub type AppHook = Arc<dyn Fn(&str, &Value) -> Result<Value> + Send + Sync>;
/// The tools the app hook answers.
pub const APP_TOOLS: &[&str] = &[
    "finder_status",
    "finder_show",
    "finder_hide",
    "app_settings",
    "set_language",
    "set_theme",
    "set_mode",
];

pub const AUTHOR_CLAUDE: &str = "claude";
pub const AUTHOR_USER: &str = "you";

const MAX_HEX_BYTES: u64 = 64 * 1024;
const MAX_MAP_REGION_BUCKETS: usize = 200;
const DEFAULT_PAGE: u64 = 100;
/// What the workbench shows of a reassembly: a whole picture up to this, or a text's start.
const MAX_MATERIALIZE_IMAGE_BYTES: u64 = 24 * 1024 * 1024;
const MAX_MATERIALIZE_TEXT_BYTES: u64 = 512 * 1024;
const MAX_FOLLOW_CLUSTERS: u64 = 100_000;
const DEFAULT_FIND_LIMIT: u64 = 200;
const ACTIVITY_DETAIL_CHARS: usize = 300;
/// Tools called constantly, or only about the window, stay out of the activity log.
const QUIET_TOOLS: &[&str] = &[
    "status",
    "get_selection",
    "set_selection",
    "activity",
    "list_overlays",
    "copy_status",
    "browse_types",
    "browse_files",
    "file_test",
    "file_copies",
    "file_tests_status",
    "mount_history",
    "disk_access",
    "startup_disks",
    "zip_contents",
    "zip_member_check",
];
const DEFAULT_REGION_SAMPLES: usize = 64;
const DEFAULT_MIN_RUN_CLUSTERS: u64 = 64;

#[derive(Default)]
pub struct Engine {
    session: RwLock<Option<Arc<Session>>>,
    raw_disk: RwLock<Option<Arc<crate::disk::RawDisk>>>,
    /// The stored source of the whole disk that is open, as (raw source ID, source ID).
    source_of_disk: RwLock<Option<(String, String)>>,
    selection: Mutex<Value>,
    sink: RwLock<Option<UiSink>>,
    screenshot: RwLock<Option<ScreenshotHook>>,
    app_hook: RwLock<Option<AppHook>>,
    media: RwLock<Option<Arc<MediaServer>>>,
    /// Whether file names and contents are kept from agents. Only the person changes it.
    names_hidden: std::sync::atomic::AtomicBool,
}

fn u64_of(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

fn need_u64(args: &Value, key: &str) -> Result<u64> {
    u64_of(args, key)
        .with_context(|| format!("argument {key} is required and must be a non-negative integer"))
}

fn need_u32(args: &Value, key: &str) -> Result<u32> {
    u32::try_from(need_u64(args, key)?).with_context(|| format!("argument {key} is too large"))
}

fn need_i64(args: &Value, key: &str) -> Result<i64> {
    args.get(key)
        .and_then(Value::as_i64)
        .with_context(|| format!("argument {key} is required and must be an integer"))
}

/// The Disk Rebuild an argument names: `diskRebuild`.
fn need_rebuild(args: &Value) -> Result<i64> {
    args.get("diskRebuild")
        .and_then(Value::as_i64)
        .context("argument diskRebuild is required and must be an integer")
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("argument {key} is required and must be a string"))
}

/// How close to a join a break must be to count as the join's fault: a few frames or chunks.
const JOIN_MARGIN_BYTES: u64 = 4096;

/// Whether a stream runs straight across the join between a file's own clusters and the ones
/// appended to it. A walker that resyncs, as MP3's does, finds valid frames in any audio, so a
/// high valid fraction proves nothing about a bridge: on a real volume, every MP3 bridge that
/// scored 98% valid broke at or just after the join. Only a clean crossing is evidence.
fn join_crossing(join: u64, breaks: &[u64], reached_end: bool) -> &'static str {
    let near = |offset: &u64| offset.abs_diff(join) <= JOIN_MARGIN_BYTES;
    if breaks.iter().any(near) {
        "breaks"
    } else if reached_end || breaks.iter().any(|&offset| offset > join) {
        "clean"
    } else {
        "unknown"
    }
}

fn str_of<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn f64_of(args: &Value, key: &str, default: f64) -> f64 {
    args.get(key).and_then(Value::as_f64).unwrap_or(default)
}

fn bool_of(args: &Value, key: &str, default: bool) -> bool {
    args.get(key).and_then(Value::as_bool).unwrap_or(default)
}

fn extents_of(args: &Value, key: &str) -> Result<Option<Vec<u32>>> {
    let Some(value) = args.get(key) else {
        return Ok(None);
    };
    let extents: Vec<Extent> = serde_json::from_value(value.clone())
        .with_context(|| format!("{key} must be a list of {{first, count}}"))?;
    Ok(Some(from_extents(&extents)))
}

/// Where a Disk Rebuild tool puts something: `{"item": id, "head": cluster}`, either or both left out.
fn place_of(args: &Value) -> Result<Place> {
    let Some(place) = args.get("place").filter(|place| !place.is_null()) else {
        return Ok(Place::default());
    };
    Ok(Place {
        item: place.get("item").and_then(Value::as_i64),
        head: place
            .get("head")
            .and_then(Value::as_u64)
            .map(u32::try_from)
            .transpose()
            .context("head is not a cluster number")?,
    })
}

fn path_of_names(args: &Value) -> Result<Vec<String>> {
    match args.get("path") {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(parts)) => parts
            .iter()
            .map(|part| {
                part.as_str()
                    .map(str::to_string)
                    .context("path holds a name that is not text")
            })
            .collect(),
        Some(_) => bail!("path must be a list of folder names"),
    }
}

/// Changing a Disk Rebuild is the person's to do. Claude reads Disk Rebuilds, and suggests.
fn only_the_person(author: &str) -> Result<()> {
    if author != AUTHOR_USER {
        bail!("Only the person using the app changes a Disk Rebuild. Suggest a placement instead.");
    }
    Ok(())
}

fn parse_hex(text: &str) -> Result<Vec<u8>> {
    let clean: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        bail!("hex text has an odd number of digits");
    }
    (0..clean.len())
        .step_by(2)
        .map(|index| {
            u8::from_str_radix(&clean[index..index + 2], 16)
                .context("hex text holds a non-hex digit")
        })
        .collect()
}

/// Formats bytes as a classic offset, hex and ASCII dump.
pub fn hexdump(data: &[u8], base: u64) -> String {
    let mut out = String::with_capacity(data.len() * 5);
    for (index, row) in data.chunks(16).enumerate() {
        out.push_str(&format!("{:012x}  ", base + index as u64 * 16));
        for column in 0..16 {
            match row.get(column) {
                Some(byte) => out.push_str(&format!("{byte:02x} ")),
                None => out.push_str("   "),
            }
            if column == 7 {
                out.push(' ');
            }
        }
        out.push(' ');
        out.extend(row.iter().map(|&byte| {
            if (32..127).contains(&byte) {
                byte as char
            } else {
                '.'
            }
        }));
        out.push('\n');
    }
    out
}

impl Engine {
    pub fn new() -> Arc<Self> {
        let engine = Self::default();
        engine.set_names_hidden(crate::privacy::saved_names_hidden());
        Arc::new(engine)
    }

    /// Keeps file names and contents from agents, or lets them see again, for this engine.
    pub fn set_names_hidden(&self, hidden: bool) {
        self.names_hidden
            .store(hidden, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn names_hidden(&self) -> bool {
        self.names_hidden.load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn set_ui_sink(&self, sink: UiSink) {
        if let Ok(mut slot) = self.sink.write() {
            *slot = Some(sink);
        }
    }

    pub fn set_media(&self, media: Arc<MediaServer>) {
        if let Ok(mut slot) = self.media.write() {
            *slot = Some(media);
        }
    }

    pub fn set_screenshot_hook(&self, hook: ScreenshotHook) {
        if let Ok(mut slot) = self.screenshot.write() {
            *slot = Some(hook);
        }
    }

    pub fn set_app_hook(&self, hook: AppHook) {
        if let Ok(mut slot) = self.app_hook.write() {
            *slot = Some(hook);
        }
    }

    fn app_tool(&self, tool: &str, args: &Value) -> Result<Value> {
        let hook = self
            .app_hook
            .read()
            .map_err(|_| anyhow!("app hook lock poisoned"))?
            .clone();
        hook.context("this needs the Captain Salvage window, which is not running here")?(
            tool, args,
        )
    }

    pub fn screenshot(&self) -> Result<Vec<u8>> {
        let hook = self
            .screenshot
            .read()
            .map_err(|_| anyhow!("screenshot lock poisoned"))?
            .clone();
        hook.context("no app window is available to capture")?()
    }

    pub fn emit(&self, event: &str, payload: Value) {
        if let Ok(slot) = self.sink.read() {
            if let Some(sink) = slot.as_ref() {
                sink(event, payload);
            }
        }
    }

    /// Opens a volume. Asking for the volume already open returns that session.
    pub fn open(&self, path: &str) -> Result<Arc<Session>> {
        let wanted = PathBuf::from(path);
        if let Ok(current) = self.session() {
            // A disk that came back with the same number needs a new handle: the old one is dead.
            if current.path == wanted
                && current.interpretation.is_none()
                && !current.volume.source().is_gone()
            {
                return Ok(current);
            }
        }
        let session = Session::open(&wanted)?;
        *self
            .session
            .write()
            .map_err(|_| anyhow!("session lock poisoned"))? = Some(session.clone());
        self.emit("volume-opened", json!({"path": path}));
        Ok(session)
    }

    fn open_region(
        &self,
        disk: &Arc<crate::disk::RawDisk>,
        region: crate::disk::DiskRegion,
    ) -> Result<Value> {
        let session = Session::open_region(disk, region)?;
        let mut overview = session.overview();
        overview["filesystem"] = serde_json::to_value(&session.volume.filesystem)?;
        self.set_session(session);
        self.emit(
            "volume-opened",
            json!({"path": disk.path, "interpretation": true}),
        );
        Ok(overview)
    }

    /// Opens a stored scan of the open raw source. A filesystem scan becomes the open volume with
    /// the results it stored, and nothing is scanned. A raw scan loads its findings.
    fn open_stored_scan(&self, args: &Value) -> Result<Value> {
        use crate::catalog::{Accept, Catalog, ScanKind};
        let disk = self.raw_disk(args)?;
        let scan_id = need_str(args, "scanId")?;
        let changed = bool_of(args, "acceptChangedMarkers", false);
        let accept = if changed {
            Accept::Probable
        } else {
            Accept::Exact
        };
        let scan = Catalog::for_user()?.scan(scan_id)?;
        match scan.kind {
            ScanKind::Filesystem => {
                let session = Session::open_scan(&disk, scan_id, accept)?;
                let mut overview = session.overview();
                overview["filesystem"] = serde_json::to_value(&session.volume.filesystem)?;
                self.set_session(session);
                self.emit(
                    "volume-opened",
                    json!({"path": disk.path, "interpretation": true}),
                );
                Ok(overview)
            }
            ScanKind::Raw => {
                let raw = scan
                    .raw_scan_id
                    .as_deref()
                    .context("this scan has no saved findings to load")?;
                Ok(serde_json::to_value(
                    crate::raw_scan::ScanStore::for_user()?.resume(
                        &disk,
                        raw,
                        bool_of(args, "acceptSampledMatch", false),
                        changed,
                    )?,
                )?)
            }
        }
    }

    pub fn set_session(&self, session: Arc<Session>) {
        if let Ok(mut slot) = self.session.write() {
            *slot = Some(session);
        }
    }

    fn raw_disk(&self, args: &Value) -> Result<Arc<crate::disk::RawDisk>> {
        let disk = self
            .raw_disk
            .read()
            .map_err(|_| anyhow!("raw disk lock poisoned"))?
            .clone()
            .context("no raw source is open; use open_raw_disk first")?;
        if need_str(args, "diskId")? != disk.id {
            bail!("raw source changed; reopen it before using these offsets");
        }
        Ok(disk)
    }

    pub fn session(&self) -> Result<Arc<Session>> {
        self.session
            .read()
            .map_err(|_| anyhow!("session lock poisoned"))?
            .clone()
            .context("no volume is open. Use open_volume first, and disk_access if it is refused.")
    }

    /// Runs a tool, and records it in the activity log the window shows.
    pub fn call(&self, tool: &str, args: &Value, author: &str) -> Result<Value> {
        // The person can keep file names and contents from agents. The window always sees them.
        let private = author != AUTHOR_USER && self.names_hidden();
        let result = if private && crate::privacy::CONTENT_TOOLS.contains(&tool) {
            Err(anyhow!(
                "The person using the app has kept file names and contents from agents, so {tool} is not available. Counts, sizes, types and verdicts still are."
            ))
        } else {
            self.dispatch(tool, args, author).map(|mut answer| {
                if let Ok(session) = self.session() {
                    session.flag_stale(&mut answer);
                }
                if private {
                    crate::privacy::redact(&mut answer);
                }
                answer
            })
        };
        if !QUIET_TOOLS.contains(&tool) {
            if let Ok(session) = self.session() {
                let mut detail = args.to_string();
                if tool != "log_finding" && detail.chars().count() > ACTIVITY_DETAIL_CHARS {
                    detail = detail
                        .chars()
                        .take(ACTIVITY_DETAIL_CHARS)
                        .collect::<String>()
                        + "…";
                }
                if let Err(error) = &result {
                    detail = format!("{detail} failed: {error:#}");
                }
                let _ =
                    session.with_database(|database| database.log_activity(author, tool, &detail));
                self.emit("activity", json!({"tool": tool, "author": author}));
            }
        }
        result
    }

    fn dispatch(&self, tool: &str, args: &Value, author: &str) -> Result<Value> {
        if APP_TOOLS.contains(&tool) {
            return self.app_tool(tool, args);
        }
        match tool {
            "open_raw_disk" => {
                let disk = Arc::new(crate::disk::RawDisk::open(std::path::Path::new(need_str(
                    args, "path",
                )?))?);
                let overview = serde_json::to_value(disk.overview())?;
                *self
                    .raw_disk
                    .write()
                    .map_err(|_| anyhow!("raw disk lock poisoned"))? = Some(disk);
                Ok(overview)
            }
            "discover_raw_disk" => {
                let disk = self.raw_disk(args)?;
                Ok(serde_json::to_value(disk.discover(
                    need_u64(args, "offset")?,
                    need_u64(args, "length")?,
                )?)?)
            }
            tool if crate::catalog_tools::TOOLS.contains(&tool) => {
                let disk = match args.get("diskId") {
                    Some(_) => Some(self.raw_disk(args)?),
                    None => None,
                };
                crate::catalog_tools::call(tool, disk.as_deref(), args, author == AUTHOR_USER)
            }
            "open_stored_scan" => self.open_stored_scan(args),
            "rescan_changed_regions" => self.rescan_changed_regions(args, author),
            "open_source" => self.open_source(args, author),
            tool if crate::range_tools::TOOLS.contains(&tool) => {
                let disk = self.raw_disk(args)?;
                crate::range_tools::call(tool, &disk, args)
            }
            "list_raw_scans" => {
                let disk = self.raw_disk(args)?;
                Ok(serde_json::to_value(
                    crate::raw_scan::ScanStore::for_user()?.list_matching(
                        &disk,
                        args.get("showAll")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )?,
                )?)
            }
            "create_raw_scan" => {
                let disk = self.raw_disk(args)?;
                Ok(serde_json::to_value(
                    crate::raw_scan::ScanStore::for_user()?.create(
                        &disk,
                        need_u64(args, "offset")?,
                        need_u64(args, "length")?,
                    )?,
                )?)
            }
            "resume_raw_scan" => {
                let disk = self.raw_disk(args)?;
                Ok(serde_json::to_value(
                    crate::raw_scan::ScanStore::for_user()?.resume(
                        &disk,
                        need_str(args, "scanId")?,
                        args.get("acceptSampledMatch")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                        args.get("acceptChangedMarkers")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    )?,
                )?)
            }
            "raw_scan_status" => {
                let disk = self.raw_disk(args)?;
                Ok(serde_json::to_value(
                    crate::raw_scan::ScanStore::for_user()?
                        .current(&disk, need_str(args, "scanId")?)?,
                )?)
            }
            "scan_raw_batch" => {
                let disk = self.raw_disk(args)?;
                Ok(serde_json::to_value(
                    crate::raw_scan::ScanStore::for_user()?
                        .step(&disk, need_str(args, "scanId")?)?,
                )?)
            }
            "inspect_ntfs_record" | "read_ntfs_stream" => {
                let disk = self.raw_disk(args)?;
                let start = u64_of(args, "volumeStart").unwrap_or(0);
                let length = u64_of(args, "volumeLength")
                    .unwrap_or_else(|| disk.length.saturating_sub(start));
                let recovery = crate::filesystem::ntfs::Recovery::open(&disk, start, length)?;
                let record = recovery.inspect(
                    need_u64(args, "recordOffset")?,
                    usize::try_from(u64_of(args, "recordSize").unwrap_or(1024))?,
                    u16::try_from(u64_of(args, "sectorSize").unwrap_or(512))?,
                )?;
                if tool == "inspect_ntfs_record" {
                    return Ok(serde_json::to_value(
                        crate::filesystem::ntfs::Recovery::describe(&record),
                    )?);
                }
                let data = recovery.read_stream(
                    &record,
                    usize::try_from(need_u64(args, "attribute")?)?,
                    need_u64(args, "clusterSize")?,
                    need_u64(args, "offset")?,
                    usize::try_from(need_u64(args, "length")?)?,
                )?;
                Ok(
                    json!({"hex": data.iter().map(|byte| format!("{byte:02x}")).collect::<String>(), "length": data.len(), "warning": "Surviving metadata does not prove its data clusters have not been reused."}),
                )
            }
            "read_raw_disk" => {
                let disk = self.raw_disk(args)?;
                let length = need_u64(args, "length")?;
                anyhow::ensure!(length <= MAX_HEX_BYTES, "raw hex read exceeds 64 KiB limit");
                let offset = need_u64(args, "offset")?;
                let data = disk.read(offset, length)?;
                Ok(
                    json!({"offset": offset, "hex": data.iter().map(|byte| format!("{byte:02x}")).collect::<String>()}),
                )
            }
            "open_disk_region" => {
                let disk = self.raw_disk(args)?;
                let start = need_u64(args, "start")?;
                let region = crate::disk::DiskRegion {
                    start,
                    length: need_u64(args, "length")?,
                    format: need_str(args, "format")?.into(),
                    label: str_of(args, "label")
                        .unwrap_or("Filesystem interpretation")
                        .into(),
                    boot_offset: u64_of(args, "bootOffset").unwrap_or(start),
                };
                self.open_region(&disk, region)
            }
            "open_hypothesis" => {
                let disk = self.raw_disk(args)?;
                let hypothesis = crate::hypotheses::HypothesisStore::for_user()?
                    .get(&disk, need_str(args, "id")?)?;
                let region = crate::disk::DiskRegion {
                    start: hypothesis.start,
                    length: hypothesis.length,
                    format: hypothesis.format.clone(),
                    label: hypothesis.label.clone(),
                    boot_offset: hypothesis.boot_offset.unwrap_or(hypothesis.start),
                };
                self.open_region(&disk, region)
            }
            "disk_access" => {
                let path = match str_of(args, "path") {
                    Some(path) => PathBuf::from(path),
                    None => self.session()?.path.clone(),
                };
                Ok(disk_access_report(&path))
            }
            "list_disks" => Ok(json!({"output": list_disks(str_of(args, "device"))?})),
            "startup_disks" => {
                let internal = args
                    .get("includeInternal")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                // Only for driving the window in a test: a made-up list of disks, so a test never
                // shows or opens a real one. Ignored unless the test hook is on.
                if std::env::var_os("SALVAGE_TEST_UI").is_some() {
                    if let Some(file) = std::env::var_os("SALVAGE_TEST_STARTUP_DISKS") {
                        return Ok(serde_json::from_slice(&std::fs::read(file)?)?);
                    }
                }
                Ok(serde_json::to_value(crate::startup::startup_report(
                    internal,
                )?)?)
            }
            "agent_privacy" => Ok(json!({"namesHidden": self.names_hidden()})),
            "set_agent_privacy" => {
                if author != AUTHOR_USER {
                    bail!("Only the person using the app can decide what agents see.");
                }
                let hidden = args
                    .get("namesHidden")
                    .and_then(Value::as_bool)
                    .context("namesHidden must be true or false")?;
                crate::privacy::save_names_hidden(hidden)?;
                self.set_names_hidden(hidden);
                self.emit("agent-privacy", json!({"namesHidden": hidden}));
                Ok(json!({"namesHidden": hidden}))
            }
            "mount_blocks" => Ok(json!({"blocks": crate::mountblock::list()})),
            "mount_blocks_mounted" => Ok(json!({"mounted": crate::mountblock::mounted_anyway()?})),
            "mount_block_unmount" => {
                let said = crate::mountblock::unmount_flagged(
                    need_str(args, "uuid")?,
                    author == AUTHOR_USER,
                )?;
                Ok(json!({"unmounted": true, "diskutil": said}))
            }
            "mount_block_set" => {
                let blocked = args
                    .get("blocked")
                    .and_then(Value::as_bool)
                    .context("blocked must be true or false")?;
                let blocks = crate::mountblock::set(
                    &crate::workspace::Workspace::root()?,
                    need_str(args, "uuid")?,
                    str_of(args, "name").unwrap_or_default(),
                    blocked,
                    author == AUTHOR_USER,
                )?;
                Ok(json!({"blocks": blocks}))
            }
            "open_volume" => {
                let session = self.open(need_str(args, "path")?)?;
                let mut overview = session.overview();
                overview
                    .as_object_mut()
                    .context("volume overview is not an object")?
                    .insert(
                        "filesystem".into(),
                        serde_json::to_value(&session.volume.filesystem)?,
                    );
                Ok(overview)
            }
            "get_selection" => Ok(self
                .selection
                .lock()
                .map_err(|_| anyhow!("selection lock poisoned"))?
                .clone()),
            "set_selection" => {
                *self
                    .selection
                    .lock()
                    .map_err(|_| anyhow!("selection lock poisoned"))? = args.clone();
                Ok(json!({"ok": true}))
            }
            "ui_eval" => {
                self.emit("test-eval", args.clone());
                Ok(json!({"sent": true}))
            }
            "show_in_app" => {
                self.emit("focus", args.clone());
                Ok(json!({"shown": true}))
            }
            _ => self.dispatch_with_session(tool, args, author),
        }
    }

    fn dispatch_with_session(&self, tool: &str, args: &Value, author: &str) -> Result<Value> {
        let session = self.session()?;
        let page = |args: &Value| {
            (
                u64_of(args, "offset").unwrap_or(0),
                u64_of(args, "limit").unwrap_or(DEFAULT_PAGE),
            )
        };
        match tool {
            "status" => Ok(serde_json::to_value(session.status()?)?),
            "volume_overview" => Ok(session.overview()),
            "capabilities" => Ok(json!({
                "filesystem": session.volume.kind.to_string(),
                "capabilities": session.volume.kind.capabilities(),
            })),
            "ntfs_entry" => Ok(serde_json::to_value(
                session.ntfs_record(need_i64(args, "id")?)?,
            )?),
            "verify_unchanged" => Ok(serde_json::to_value(session.verify_unchanged()?)?),
            "structure_changes" => Ok(serde_json::to_value(session.structure_changes()?)?),

            "region_stats" => {
                let last = session.volume.geometry.last_cluster_exclusive();
                Ok(session.region_stats(
                    u64_of(args, "startCluster").unwrap_or(2) as u32,
                    u64_of(args, "endCluster").unwrap_or(u64::from(last)) as u32,
                ))
            }
            "map_region" => self.map_region(&session, args),
            "nameless_chains" => {
                let (offset, limit) = page(args);
                let class = u64_of(args, "headClass").map(|value| value as u8);
                let (rows, total) = session
                    .with_database(|database| database.nameless_chains(offset, limit, class))?;
                Ok(
                    json!({"chains": rows, "total": total, "clusterSize": session.volume.geometry.cluster_size()}),
                )
            }
            "broken_files" => {
                let (offset, limit) = page(args);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.broken_files(offset, limit),
                )?)?)
            }
            "bridge_candidates" => {
                let id = need_i64(args, "entryId")?;
                let limit = u64_of(args, "limit").unwrap_or(20) as usize;
                let entropy = session.entropy_map();
                Ok(serde_json::to_value(session.with_database(
                    |database| database.bridge_candidates(&session.volume, id, &entropy, limit),
                )?)?)
            }

            "roots" => {
                let (offset, limit) = page(args);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.roots(offset, limit),
                )?)?)
            }
            "children" => {
                let (offset, limit) = page(args);
                let node = need_u32(args, "node")?;
                let hidden = bool_of(args, "includeHidden", false);
                let floor = f64_of(args, "minConfidence", 0.0);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.children(node, offset, limit, hidden, floor),
                )?)?)
            }
            "node" => {
                let head = need_u32(args, "head")?;
                Ok(serde_json::to_value(
                    session.with_database(|database| database.node(head))?,
                )?)
            }
            "tree_path" => {
                let heads = session.with_database(|database| {
                    let folder = match (
                        u64_of(args, "folder").and_then(|head| u32::try_from(head).ok()),
                        args.get("entry").and_then(Value::as_i64),
                    ) {
                        (Some(folder), _) => Some(folder),
                        (None, Some(id)) => database.folder_of_entry(id)?,
                        (None, None) => bail!("tree_path needs folder or entry"),
                    };
                    match folder {
                        Some(head) => database.folder_heads(head),
                        None => Ok(Vec::new()),
                    }
                })?;
                Ok(json!({"heads": heads}))
            }
            "entry" => {
                let id = need_i64(args, "id")?;
                session.with_database(|database| {
                    let row = database.entry(id)?.with_context(|| format!("no entry with id {id}"))?;
                    let clusters = database.entry_clusters(&session.volume, &row)?;
                    let overlay = database.overlay_clusters(id)?.is_some();
                    Ok((row, clusters, overlay))
                })
                .and_then(|(row, clusters, overlay)| {
                    let written = session.macos_wrote_over(&clusters)?;
                    Ok(json!({"entry": row, "extents": to_extents(&clusters), "clusters": clusters.len(), "usesOverlay": overlay, "macosWroteOn": written}))
                })
            }
            "search" => {
                let query = need_str(args, "query")?;
                let limit = u64_of(args, "limit").unwrap_or(DEFAULT_PAGE);
                let floor = f64_of(args, "minConfidence", 0.0);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.search(query, limit, floor),
                )?)?)
            }
            "file_tests_start" => {
                let groups = args
                    .get("groups")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                let filter: crate::filetest::TestFilter =
                    serde_json::from_value(args.clone()).context("invalid test filter")?;
                Ok(serde_json::to_value(
                    session.file_test_start(groups, filter)?,
                )?)
            }
            "file_tests_stop" => {
                session.file_test_stop();
                Ok(json!({"stopRequested": true}))
            }
            "file_tests_forget" => {
                let outcome = args.get("outcome").and_then(Value::as_str);
                let formats: Vec<String> = args
                    .get("formats")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .filter_map(Value::as_str)
                            .map(str::to_string)
                            .collect()
                    })
                    .unwrap_or_default();
                Ok(json!({"forgotten": session.file_tests_forget(outcome, &formats)?}))
            }
            "file_tests_status" => Ok(serde_json::to_value(session.file_test_status()?)?),
            "file_tests_by_type" => Ok(
                json!({"types": session.with_database(|database| database.file_tests_by_type())?}),
            ),
            "file_copies" => {
                let id = need_i64(args, "id")?;
                let limit = u64_of(args, "limit").unwrap_or(DEFAULT_PAGE);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.file_copies(id, limit),
                )?)?)
            }
            "macos_writes" => Ok(json!({"dates": session.macos_writes()?})),
            "mount_history" => Ok(serde_json::to_value(session.mount_history()?)?),
            // Only for driving the window in a test: never offered, and refused otherwise.
            "test_disconnect" => {
                if std::env::var_os("SALVAGE_TEST_UI").is_none() {
                    bail!("this is only for testing");
                }
                session.volume.source().simulate_disconnect();
                Ok(json!({"ok": true}))
            }
            "zip_contents" => {
                let id = need_i64(args, "id")?;
                Ok(serde_json::to_value(session.zip_contents(id)?)?)
            }
            "zip_member_check" => {
                let id = need_i64(args, "id")?;
                let index = u64_of(args, "index").context("index is required")? as u32;
                Ok(serde_json::to_value(session.zip_member_check(id, index)?)?)
            }
            "zip_member_preview" => {
                let id = need_i64(args, "id")?;
                let index = u64_of(args, "index").context("index is required")? as u32;
                Ok(serde_json::to_value(
                    session.zip_member_preview(id, index)?,
                )?)
            }
            "zip_member_copy" => {
                if author != AUTHOR_USER {
                    bail!("Only the person using the app copies files out. Tell them which file, and its size.");
                }
                let id = need_i64(args, "id")?;
                let index = u64_of(args, "index").context("index is required")? as u32;
                let destination = match args.get("destination").and_then(Value::as_str) {
                    Some(destination) if !destination.trim().is_empty() => destination.to_string(),
                    _ => session
                        .salvage_drive()?
                        .filter(|drive| !drive.is_empty())
                        .context("Choose a destination, or set a salvage drive, first.")?,
                };
                Ok(serde_json::to_value(session.zip_member_copy(
                    id,
                    index,
                    std::path::Path::new(&destination),
                )?)?)
            }
            "file_test" => {
                let id = need_i64(args, "id")?;
                Ok(serde_json::to_value(
                    session.with_database(|database| database.file_test(id))?,
                )?)
            }
            "media_check" => {
                let id = need_i64(args, "id")?;
                let start = f64_of(args, "at", 0.0);
                let media = self
                    .media
                    .read()
                    .map_err(|_| anyhow!("media lock poisoned"))?
                    .clone()
                    .context("the media port did not start")?;
                Ok(serde_json::to_value(media.check(id, start))?)
            }
            "browse_types" => {
                let folder = u64_of(args, "folder").map(|head| head as u32);
                let floor = f64_of(args, "minConfidence", 0.0);
                Ok(serde_json::to_value(session.browse_types(folder, floor)?)?)
            }
            "browse_files" => {
                let query = BrowseQuery {
                    group: args
                        .get("group")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    folder: u64_of(args, "folder").map(|head| head as u32),
                    min_confidence: f64_of(args, "minConfidence", 0.0),
                    sort: args
                        .get("sort")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    text: args
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .into(),
                    hide_copies: args
                        .get("hideCopies")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                };
                let offset = u64_of(args, "offset").unwrap_or(0);
                let limit = u64_of(args, "limit").unwrap_or(DEFAULT_PAGE);
                Ok(serde_json::to_value(
                    session.browse_files(&query, offset, limit)?,
                )?)
            }
            "owners" => {
                let cluster = need_u32(args, "cluster")?;
                Ok(serde_json::to_value(session.with_database(
                    |database| database.owners(&session.volume, cluster),
                )?)?)
            }

            "read_bytes" => {
                let scope = Scope::parse(str_of(args, "scope"))?;
                let offset = need_u64(args, "offset")?;
                let length = u64_of(args, "length").unwrap_or(512);
                if length > MAX_HEX_BYTES {
                    bail!("length is limited to {MAX_HEX_BYTES} bytes here. Use run_script for larger reads.");
                }
                let data = session.read(scope, offset, length)?;
                Ok(json!({"dump": hexdump(&data, offset)}))
            }
            "inspect_cluster" => {
                let cluster = need_u32(args, "cluster")?;
                let mut info = serde_json::to_value(session.inspect_cluster(cluster)?)?;
                if bool_of(args, "includeHex", false) {
                    let offset = session.volume.cluster_offset(cluster);
                    let length = u64_of(args, "hexBytes").unwrap_or(512).min(MAX_HEX_BYTES);
                    info["dump"] = json!(hexdump(
                        &session.read(Scope::Partition, offset, length)?,
                        offset
                    ));
                }
                Ok(info)
            }
            "parse_entries" => {
                let scope = Scope::parse(str_of(args, "scope"))?;
                let data =
                    session.read(scope, need_u64(args, "offset")?, need_u64(args, "length")?)?;
                Ok(serde_json::to_value(
                    session.volume.find_sets(&data, data.len()),
                )?)
            }
            "find_bytes" => self.find_bytes(&session, args),
            "follow_chain" => self.follow_chain(&session, args),
            "decode_struct" => {
                let scope = Scope::parse(str_of(args, "scope"))?;
                let definition = args
                    .get("definition")
                    .context("argument definition is required")?;
                Ok(serde_json::to_value(session.decode_struct(
                    definition,
                    scope,
                    need_u64(args, "offset")?,
                )?)?)
            }

            "conflicts" => Ok(serde_json::to_value(
                session.conflicts(need_i64(args, "id")?)?,
            )?),
            "resolve_preview" => {
                let proposed = args
                    .get("overlays")
                    .and_then(Value::as_array)
                    .map(|items| {
                        items
                            .iter()
                            .enumerate()
                            .map(|(index, item)| crate::map::Overlay {
                                id: -1 - index as i64,
                                kind: str_of(item, "kind").unwrap_or_default().to_string(),
                                target: item.get("target").and_then(Value::as_i64),
                                payload: item.get("payload").cloned().unwrap_or_else(|| json!({})),
                                note: str_of(item, "note").unwrap_or_default().to_string(),
                                author: author.to_string(),
                                active: true,
                                created_at_millis: 0,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                let changes = args.get("parameters").cloned().unwrap_or_else(|| json!({}));
                Ok(serde_json::to_value(
                    session.resolve_preview(&changes, proposed)?,
                )?)
            }
            "test_stream" => self.test_stream(&session, args).map(|mut answer| {
                // The cluster runs and the per-run profile can run to megabytes for a film.
                if bool_of(args, "summary", false) {
                    if let Some(map) = answer.as_object_mut() {
                        map.remove("extents");
                        map.remove("profile");
                    }
                }
                answer
            }),
            "materialize" => self.materialize(&session, args),
            "add_overlay" => {
                let kind = need_str(args, "kind")?;
                let note = need_str(args, "note")?;
                let payload = args.get("payload").cloned().unwrap_or_else(|| json!({}));
                let target = args.get("target").and_then(Value::as_i64);
                let id = session.with_database(|database| {
                    database.add_overlay(kind, target, &payload, note, author)
                })?;
                let summary = if bool_of(args, "resolve", true) {
                    Some(session.resolve()?)
                } else {
                    None
                };
                self.emit("map-changed", json!({"reason": "overlay", "id": id}));
                Ok(json!({"id": id, "summary": summary}))
            }
            "list_overlays" => Ok(serde_json::to_value(session.with_database(
                |database| database.overlays(bool_of(args, "onlyActive", false)),
            )?)?),
            "set_overlay_active" => {
                let id = need_i64(args, "id")?;
                let active = args
                    .get("active")
                    .and_then(Value::as_bool)
                    .context("argument active is required")?;
                let found =
                    session.with_database(|database| database.set_overlay_active(id, active))?;
                let summary = if bool_of(args, "resolve", true) {
                    Some(session.resolve()?)
                } else {
                    None
                };
                self.emit("map-changed", json!({"reason": "overlay", "id": id}));
                Ok(json!({"found": found, "summary": summary}))
            }
            "delete_overlay" => {
                let id = need_i64(args, "id")?;
                let found = session.with_database(|database| database.delete_overlay(id))?;
                let summary = if bool_of(args, "resolve", true) {
                    Some(session.resolve()?)
                } else {
                    None
                };
                self.emit("map-changed", json!({"reason": "overlay", "id": id}));
                Ok(json!({"found": found, "summary": summary}))
            }
            "get_parameters" => Ok(serde_json::to_value(
                session.with_database(|database| database.params())?,
            )?),
            "set_parameters" => {
                let current = session.with_database(|database| database.params())?;
                let mut merged = serde_json::to_value(current)?;
                if let (Some(target), Some(changes)) = (merged.as_object_mut(), args.as_object()) {
                    for (key, value) in changes {
                        if key != "resolve" {
                            target.insert(key.clone(), value.clone());
                        }
                    }
                }
                let params: ResolveParams =
                    serde_json::from_value(merged).context("invalid parameter value")?;
                session.with_database(|database| database.set_params(&params))?;
                let summary = if bool_of(args, "resolve", true) {
                    Some(session.resolve()?)
                } else {
                    None
                };
                self.emit("map-changed", json!({"reason": "parameters"}));
                Ok(json!({"parameters": params, "summary": summary}))
            }
            "resolve" => {
                let summary = session.resolve()?;
                self.emit("map-changed", json!({"reason": "resolve"}));
                Ok(serde_json::to_value(summary)?)
            }

            "query_map" => session.query_map_private(
                need_str(args, "sql")?,
                author != AUTHOR_USER && self.names_hidden(),
            ),
            "breakdown" => {
                let request: crate::summaries::BreakdownRequest =
                    serde_json::from_value(args.clone()).context("invalid breakdown request")?;
                session.with_database(|database| crate::summaries::breakdown(database, &request))
            }
            "chain_summary" => {
                let size = session.volume.geometry.cluster_size();
                let groups = session
                    .with_database(|database| crate::summaries::chain_summary(database, size))?;
                Ok(json!({"groups": groups}))
            }
            "run_script" => {
                let timeout = u64_of(args, "timeoutSeconds").unwrap_or(DEFAULT_TIMEOUT_SECONDS);
                let private = author != AUTHOR_USER && self.names_hidden();
                run_script(&session, need_str(args, "source")?, timeout, private)
            }

            "export_plan" => {
                let options: ExportOptions =
                    serde_json::from_value(args.clone()).context("invalid export options")?;
                Ok(serde_json::to_value(session.export_plan(
                    str_of(args, "kind").unwrap_or("file"),
                    need_i64(args, "target")?,
                    &options,
                )?)?)
            }
            "export_start" => {
                // Writing files is the person's decision. An AI can plan an export, but not start one.
                if author != AUTHOR_USER {
                    bail!("Only the person using the app can start an export. Use export_plan, and ask them to start it.");
                }
                let options: ExportOptions =
                    serde_json::from_value(args.clone()).context("invalid export options")?;
                Ok(serde_json::to_value(session.export_start(
                    str_of(args, "kind").unwrap_or("file"),
                    need_i64(args, "target")?,
                    &options,
                )?)?)
            }
            "copy_stop" => {
                if author != AUTHOR_USER {
                    bail!("Only the person using the app can stop a copy.");
                }
                session.copy_stop();
                Ok(json!({"stopRequested": true}))
            }
            "copy_status" => Ok(serde_json::to_value(session.copy_status()?)?),
            "copy_report" | "copy_check" => {
                let destination = match str_of(args, "destination").filter(|path| !path.is_empty())
                {
                    Some(path) => PathBuf::from(path),
                    None => PathBuf::from(session.copy_status()?.destination),
                };
                if destination.as_os_str().is_empty() {
                    bail!("Nothing has been copied yet.");
                }
                if tool == "copy_check" {
                    crate::report::check_copies(&destination)?;
                    return Ok(json!({"checking": true}));
                }
                let (offset, limit) = page(args);
                let filter = str_of(args, "filter").unwrap_or("all");
                Ok(serde_json::to_value(crate::report::copy_report(
                    &destination,
                    filter,
                    offset as usize,
                    limit as usize,
                )?)?)
            }
            "disk_rebuild_list" => Ok(serde_json::to_value(
                session.with_database(|database| database.disk_rebuilds())?,
            )?),
            "disk_rebuild_items" => {
                let disk_rebuild = need_rebuild(args)?;
                Ok(serde_json::to_value(session.with_database(
                    |database| database.disk_rebuild_items(disk_rebuild),
                )?)?)
            }
            "disk_rebuild_tree" => {
                let disk_rebuild = need_rebuild(args)?;
                let path = path_of_names(args)?;
                let offset = u64_of(args, "offset").unwrap_or(0);
                let limit = u64_of(args, "limit").unwrap_or(DEFAULT_PAGE);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.disk_rebuild_list(disk_rebuild, &path, offset, limit),
                )?)?)
            }
            "disk_rebuild_clashes" => {
                let disk_rebuild = need_rebuild(args)?;
                let need_you = args
                    .get("needYou")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let (offset, limit) = page(args);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.disk_rebuild_clashes(disk_rebuild, need_you, offset, limit),
                )?)?)
            }
            "disk_rebuild_suggestions" => {
                let disk_rebuild = need_rebuild(args)?;
                Ok(serde_json::to_value(session.with_database(
                    |database| database.disk_rebuild_suggestions(disk_rebuild),
                )?)?)
            }
            "disk_rebuild_suggest" => {
                let id = session.with_database(|database| {
                    database.disk_rebuild_suggest(
                        need_rebuild(args)?,
                        need_u32(args, "head")?,
                        place_of(args)?,
                        bool_of(args, "merge", false),
                        need_str(args, "note")?,
                        author,
                    )
                })?;
                self.emit(
                    "disk-rebuild-changed",
                    json!({"diskRebuild": args.get("diskRebuild")}),
                );
                Ok(json!({"id": id}))
            }
            "similar_pieces" => {
                let head = need_u32(args, "head")?;
                let limit = u64_of(args, "limit").unwrap_or(20) as usize;
                Ok(serde_json::to_value(session.with_database(
                    |database| database.similar_pieces(head, limit),
                )?)?)
            }
            "disk_rebuild_unused" => {
                let disk_rebuild = need_rebuild(args)?;
                let offset = u64_of(args, "offset").unwrap_or(0);
                let limit = u64_of(args, "limit").unwrap_or(DEFAULT_PAGE);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.disk_rebuild_unused(disk_rebuild, offset, limit),
                )?)?)
            }
            "disk_rebuild_create"
            | "disk_rebuild_rename"
            | "disk_rebuild_delete"
            | "disk_rebuild_place"
            | "disk_rebuild_place_file"
            | "disk_rebuild_folder"
            | "disk_rebuild_move"
            | "disk_rebuild_rename_item"
            | "disk_rebuild_remove"
            | "disk_rebuild_leftovers"
            | "disk_rebuild_choose"
            | "disk_rebuild_swap"
            | "disk_rebuild_accept"
            | "disk_rebuild_dismiss" => {
                only_the_person(author)?;
                let result = session.with_database(|database| -> Result<Value> {
                    Ok(match tool {
                        "disk_rebuild_create" => {
                            json!({"id": database.disk_rebuild_create(need_str(args, "name")?)?})
                        }
                        "disk_rebuild_rename" => {
                            database.disk_rebuild_rename(
                                need_rebuild(args)?,
                                need_str(args, "name")?,
                            )?;
                            json!({"ok": true})
                        }
                        "disk_rebuild_delete" => {
                            database.disk_rebuild_delete(need_rebuild(args)?)?;
                            json!({"ok": true})
                        }
                        "disk_rebuild_place" => json!({"id": database.disk_rebuild_place(
                            need_rebuild(args)?,
                            need_u32(args, "head")?,
                            place_of(args)?,
                            bool_of(args, "merge", false),
                        )?}),
                        "disk_rebuild_place_file" => json!({"id": database.disk_rebuild_place_file(
                            need_rebuild(args)?,
                            need_i64(args, "entry")?,
                            place_of(args)?,
                        )?}),
                        "disk_rebuild_folder" => json!({"id": database.disk_rebuild_folder(
                            need_rebuild(args)?,
                            need_str(args, "name")?,
                            place_of(args)?,
                        )?}),
                        "disk_rebuild_move" => {
                            database.disk_rebuild_move(
                                need_rebuild(args)?,
                                need_i64(args, "item")?,
                                place_of(args)?,
                                args.get("merge").and_then(Value::as_bool),
                            )?;
                            json!({"ok": true})
                        }
                        "disk_rebuild_rename_item" => {
                            database.disk_rebuild_rename_item(
                                need_rebuild(args)?,
                                need_i64(args, "item")?,
                                str_of(args, "name"),
                            )?;
                            json!({"ok": true})
                        }
                        "disk_rebuild_accept" => json!({"id": database.disk_rebuild_accept(
                            need_rebuild(args)?,
                            need_i64(args, "suggestion")?,
                        )?}),
                        "disk_rebuild_dismiss" => {
                            database.disk_rebuild_dismiss(
                                need_rebuild(args)?,
                                need_i64(args, "suggestion")?,
                            )?;
                            json!({"ok": true})
                        }
                        "disk_rebuild_swap" => {
                            database.disk_rebuild_swap(
                                need_rebuild(args)?,
                                need_i64(args, "item")?,
                                need_u32(args, "head")?,
                            )?;
                            json!({"ok": true})
                        }
                        "disk_rebuild_remove" => json!({"removed": database.disk_rebuild_remove(
                            need_rebuild(args)?,
                            need_i64(args, "item")?,
                        )?}),
                        "disk_rebuild_leftovers" => {
                            let (folder, pieces) = database.disk_rebuild_leftovers(
                                need_rebuild(args)?,
                                need_str(args, "name")?,
                                u64_of(args, "maxFiles"),
                            )?;
                            json!({"folder": folder, "pieces": pieces})
                        }
                        _ => {
                            database.disk_rebuild_choose(
                                need_rebuild(args)?,
                                &path_of_names(args)?,
                                args.get("entry").and_then(Value::as_i64),
                            )?;
                            json!({"ok": true})
                        }
                    })
                })?;
                self.emit(
                    "disk-rebuild-changed",
                    json!({"diskRebuild": args.get("diskRebuild")}),
                );
                Ok(result)
            }
            "disk_rebuild_copy_plan" => {
                let options: RebuildCopyOptions =
                    serde_json::from_value(args.clone()).context("invalid copy options")?;
                Ok(serde_json::to_value(
                    session.disk_rebuild_copy_plan(need_rebuild(args)?, &options)?,
                )?)
            }
            "disk_rebuild_copy_start" => {
                if author != AUTHOR_USER {
                    bail!("Only the person using the app can start copying. Use disk_rebuild_copy_plan, and ask them to start it.");
                }
                let options: RebuildCopyOptions =
                    serde_json::from_value(args.clone()).context("invalid copy options")?;
                Ok(serde_json::to_value(
                    session.disk_rebuild_copy_start(need_rebuild(args)?, &options)?,
                )?)
            }
            "get_salvage_drive" => match session.salvage_drive()? {
                Some(destination) => {
                    let request = CopyRequest {
                        destination: destination.clone(),
                        ..CopyRequest::default()
                    };
                    let plan = session.copy_plan(&request)?;
                    Ok(
                        json!({"destination": destination, "freeBytes": plan.free_bytes, "refused": plan.refused}),
                    )
                }
                None => Ok(json!({"destination": null})),
            },
            "set_salvage_drive" => {
                // Where recovered files get written is the person's decision, like starting a copy.
                if author != AUTHOR_USER {
                    bail!("Only the person using the app can set the salvage drive. Suggest one, and ask them to set it.");
                }
                match str_of(args, "destination").filter(|path| !path.trim().is_empty()) {
                    Some(destination) => {
                        let plan = session.set_salvage_drive(destination)?;
                        Ok(
                            json!({"destination": destination, "freeBytes": plan.free_bytes, "refused": null}),
                        )
                    }
                    None => {
                        session.clear_salvage_drive()?;
                        Ok(json!({"destination": null}))
                    }
                }
            }
            "list_volumes" => Ok(json!(list_volumes(Some(&session.path))?)),
            "carve_candidates" => {
                let (offset, limit) = page(args);
                Ok(serde_json::to_value(
                    session.carve_candidates(offset as usize, limit as usize)?,
                )?)
            }
            "unexplained_runs" => {
                let (offset, limit) = page(args);
                let minimum =
                    u64_of(args, "minClusters").unwrap_or(DEFAULT_MIN_RUN_CLUSTERS) as u32;
                Ok(serde_json::to_value(session.unexplained_runs(
                    offset as usize,
                    limit as usize,
                    minimum,
                ))?)
            }
            "explain_region" => {
                let samples =
                    u64_of(args, "samples").map_or(DEFAULT_REGION_SAMPLES, |value| value as usize);
                session.explain_region(
                    need_u32(args, "startCluster")?,
                    need_u32(args, "endCluster")?,
                    samples,
                )
            }

            "start_scan" => {
                session.start_scan()?;
                Ok(json!({"started": true}))
            }
            "stop_scan" => {
                session.stop_scan();
                Ok(json!({"stopRequested": true}))
            }
            "activity" => {
                let after = args.get("afterId").and_then(Value::as_i64).unwrap_or(0);
                let limit = u64_of(args, "limit").unwrap_or(DEFAULT_PAGE);
                Ok(serde_json::to_value(session.with_database(
                    |database| database.activity(after, limit),
                )?)?)
            }
            "log_finding" => {
                let text = need_str(args, "text")?;
                self.emit("finding", json!({"text": text, "author": author}));
                Ok(json!({"logged": true}))
            }
            other => bail!("unknown tool {other}"),
        }
    }

    fn map_region(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let last = session.volume.geometry.last_cluster_exclusive();
        let start = u64_of(args, "startCluster").unwrap_or(2) as u32;
        let end = u64_of(args, "endCluster").map_or(last, |value| (value as u32).min(last));
        let buckets =
            (u64_of(args, "buckets").unwrap_or(64) as usize).clamp(1, MAX_MAP_REGION_BUCKETS);
        const ALLOCATION: [&str; 3] = [
            "allocated",
            "free, FAT chain remains",
            "free, FAT entry zero",
        ];
        const OWNERSHIP: [&str; 6] = [
            "unexplained",
            "claimed by one entry",
            "claimed by several entries",
            "directory",
            "nameless FAT chain",
            "explained by a hypothesis",
        ];
        let allocation = session.map_bytes(Layer::Allocation, start, end, buckets, 0.0, None);
        let content = session.map_bytes(Layer::Content, start, end, buckets, 0.0, None);
        let ownership = session.map_bytes(Layer::Ownership, start, end, buckets, 0.0, None);
        let entropy = session.map_bytes(Layer::Entropy, start, end, buckets, 0.0, None);
        let confidence = session.map_bytes(Layer::Confidence, start, end, buckets, 0.0, None);
        let folders = session.map_bytes(Layer::Folders, start, end, buckets, 0.0, None);
        let span = f64::from(end.saturating_sub(start));
        let rows: Vec<Value> = (0..buckets)
            .map(|index| {
                json!({
                    "startCluster": start + (span * index as f64 / buckets as f64) as u32,
                    "allocation": ALLOCATION[usize::from(allocation[index].saturating_sub(1)).min(2)],
                    "content": GROUP_NAMES[usize::from(content[index]).min(GROUP_NAMES.len() - 1)],
                    "ownership": OWNERSHIP[usize::from(ownership[index].saturating_sub(1)).min(5)],
                    "meanEntropyBits": f64::from(entropy[index]) * 8.0 / 255.0,
                    "bestConfidence": f64::from(confidence[index]) / 255.0,
                    "holdsDirectories": folders[index] > 0,
                })
            })
            .collect();
        Ok(json!({"clustersPerBucket": span / buckets as f64, "buckets": rows}))
    }

    fn find_bytes(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let scope = Scope::parse(str_of(args, "scope"))?;
        let pattern = match (str_of(args, "hex"), str_of(args, "text")) {
            (Some(hex), _) => parse_hex(hex)?,
            (None, Some(text)) if bool_of(args, "utf16", false) => {
                text.encode_utf16().flat_map(u16::to_le_bytes).collect()
            }
            (None, Some(text)) => text.as_bytes().to_vec(),
            (None, None) => bail!("give either hex or text"),
        };
        let start = u64_of(args, "start").unwrap_or(0);
        let end = match u64_of(args, "end") {
            Some(end) => end,
            None if scope == Scope::Partition => session.volume.geometry.volume_size,
            None => bail!("end is required when the scope is disk"),
        };
        let align = u64_of(args, "align").unwrap_or(1);
        let limit = u64_of(args, "limit").unwrap_or(DEFAULT_FIND_LIMIT) as usize;
        Ok(serde_json::to_value(
            session.find_bytes(&pattern, scope, start, end, align, limit)?,
        )?)
    }

    fn follow_chain(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let start = need_u32(args, "cluster")?;
        if !session.volume.valid_cluster(start) {
            bail!("cluster {start} is outside the volume");
        }
        if session.volume.kind == crate::filesystem::FilesystemKind::Ntfs {
            bail!("NTFS keeps no chain of links between clusters. Use entry to see the runs a file's record names.");
        }
        let limit = u64_of(args, "max")
            .unwrap_or(MAX_FOLLOW_CLUSTERS)
            .min(MAX_FOLLOW_CLUSTERS);
        let length = limit.min(u64::from(session.volume.geometry.cluster_count))
            * session.volume.geometry.cluster_size();
        let clusters = session.volume.clusters_for(start, length, false).clusters;
        let current = clusters
            .last()
            .map(|&cluster| session.volume.fat_value(cluster))
            .unwrap_or(start);
        let stopped = if clusters.len() as u64 == limit {
            "limit"
        } else if !session.volume.valid_cluster(current) {
            "chain left the valid cluster range"
        } else {
            "chain loops back on itself"
        };
        let allocated = clusters
            .iter()
            .filter(|&&cluster| session.volume.is_allocated(cluster))
            .count();
        Ok(json!({
            "start": start,
            "chainHead": session.volume.chain_head(start),
            "predecessor": session.volume.predecessor(start),
            "clusters": clusters.len(),
            "allocatedClusters": allocated,
            "extents": to_extents(&clusters),
            "stoppedBecause": stopped,
            "lastFatValue": session.fat_meaning(current),
        }))
    }

    fn test_stream(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let append = extents_of(args, "appendExtents")?.unwrap_or_default();
        let (name, mut clusters, default_length) =
            if let Some(id) = args.get("entryId").and_then(Value::as_i64) {
                session.with_database(|database| {
                    let row = database
                        .entry(id)?
                        .with_context(|| format!("no entry with id {id}"))?;
                    let clusters = database.entry_clusters(&session.volume, &row)?;
                    Ok((row.name, clusters, row.length))
                })?
            } else {
                let clusters =
                    extents_of(args, "extents")?.context("give either entryId or extents")?;
                let size = session.volume.geometry.cluster_size();
                (
                    str_of(args, "name").unwrap_or("").to_string(),
                    clusters.clone(),
                    clusters.len() as u64 * size,
                )
            };
        let join = (!append.is_empty())
            .then(|| clusters.len() as u64 * session.volume.geometry.cluster_size());
        clusters.extend(append);
        let length = u64_of(args, "length").unwrap_or(default_length);
        let name = str_of(args, "name").map_or(name, str::to_string);
        let mut answer = serde_json::to_value(session.test_stream(&name, &clusters, length)?)?;
        if let Some(join) = join {
            let report = &answer["report"];
            let breaks: Vec<u64> = report["breaks"]
                .as_array()
                .map(|items| items.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default();
            let reached_end = report["reachedEnd"].as_bool().unwrap_or(false);
            answer["join"] =
                json!({"offset": join, "crossing": join_crossing(join, &breaks, reached_end)});
        }
        Ok(answer)
    }

    /// Reads clusters in a chosen order as one file, to show it: a picture, or the start of a
    /// text. Nothing is written; the plan is only data until saved as a hypothesis.
    fn materialize(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let clusters = extents_of(args, "extents")?.context("extents are required")?;
        let name = str_of(args, "name").unwrap_or("");
        let size = session.volume.geometry.cluster_size();
        let length = u64_of(args, "length").unwrap_or(clusters.len() as u64 * size);
        let Some((kind, mime)) = crate::archive::preview_kind(name) else {
            return Ok(json!({"kind": crate::archive::PREVIEW_NONE, "bytes": length}));
        };
        let limit = if kind == crate::archive::PREVIEW_IMAGE {
            MAX_MATERIALIZE_IMAGE_BYTES
        } else {
            MAX_MATERIALIZE_TEXT_BYTES
        };
        if kind == crate::archive::PREVIEW_IMAGE && length > limit {
            return Ok(json!({"kind": crate::archive::PREVIEW_TOO_BIG, "bytes": length}));
        }
        let data = session.read_clusters(&clusters, length, limit)?;
        Ok(if kind == crate::archive::PREVIEW_TEXT {
            json!({"kind": kind, "mime": mime, "text": String::from_utf8_lossy(&data), "bytes": length, "truncated": length > limit})
        } else {
            json!({"kind": kind, "mime": mime, "dataBase64": crate::mcp::base64(&data), "bytes": length, "truncated": false})
        })
    }

    /// Whole-disk device path of the open volume, for the window to display.
    pub fn whole_disk(&self) -> Option<String> {
        let session = self.session().ok()?;
        whole_disk_path(&session.path).map(|path| path.display().to_string())
    }
}
