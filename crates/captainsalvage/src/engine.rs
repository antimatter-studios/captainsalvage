//! The single entry point for every action, from the app or from MCP.

use crate::browse::BrowseQuery;
use crate::content::GROUP_NAMES;
use crate::disk_rebuild::Place;
use crate::disk_rebuild_agent::{AgentConnection, AgentLimits};
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
/// A chain appended to a stream: its head, and its clusters.
type AppendedChain = (u32, Vec<u32>);

/// A stream described by an agent: clusters in an order, and how long the file they make is.
struct Reassembly {
    name: String,
    clusters: Vec<u32>,
    length: u64,
    /// The entry it starts from, when it does.
    entry: Option<i64>,
    /// Where each appended piece starts, and the head of the chain when it was one.
    joins: Vec<(u64, Option<u32>)>,
    chains: Vec<AppendedChain>,
    cut: bool,
}

/// How many bridge candidates `bridge_candidates` tests when asked to, and the most it will.
const DEFAULT_BRIDGE_TESTS: u64 = 5;
const MAX_BRIDGE_TESTS: usize = 20;
/// The bytes all those tests together may read, unless the caller says otherwise.
const DEFAULT_BRIDGE_TEST_BYTES: u64 = 64 * 1024 * 1024;
const DEFAULT_FIND_LIMIT: u64 = 200;
const ACTIVITY_DETAIL_CHARS: usize = 300;
/// Tools called constantly, or only about the window, stay out of the activity log.
const QUIET_TOOLS: &[&str] = &[
    "status",
    "bad_sectors",
    "health_map",
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
    /// Where the read journal and the health maps of the sources are kept, opened on first use.
    health_store: Mutex<Option<Arc<crate::health::HealthStore>>>,
    selection: Mutex<Value>,
    sink: RwLock<Option<UiSink>>,
    screenshot: RwLock<Option<ScreenshotHook>>,
    app_hook: RwLock<Option<AppHook>>,
    media: RwLock<Option<Arc<MediaServer>>>,
    /// Whether file names and contents are kept from agents. Only the person changes it.
    names_hidden: std::sync::atomic::AtomicBool,
    /// How much an agent may change a Disk Rebuild.
    agent_limits: RwLock<AgentLimits>,
}

pub(crate) fn u64_of(args: &Value, key: &str) -> Option<u64> {
    args.get(key).and_then(Value::as_u64)
}

pub(crate) fn need_u64(args: &Value, key: &str) -> Result<u64> {
    u64_of(args, key)
        .with_context(|| format!("argument {key} is required and must be a non-negative integer"))
}

pub(crate) fn need_u32(args: &Value, key: &str) -> Result<u32> {
    u32::try_from(need_u64(args, key)?).with_context(|| format!("argument {key} is too large"))
}

pub(crate) fn need_i64(args: &Value, key: &str) -> Result<i64> {
    args.get(key)
        .and_then(Value::as_i64)
        .with_context(|| format!("argument {key} is required and must be an integer"))
}

/// The Disk Rebuild an argument names: `diskRebuild`.
pub(crate) fn need_rebuild(args: &Value) -> Result<i64> {
    args.get("diskRebuild")
        .and_then(Value::as_i64)
        .context("argument diskRebuild is required and must be an integer")
}

pub(crate) fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    args.get(key)
        .and_then(Value::as_str)
        .with_context(|| format!("argument {key} is required and must be a string"))
}

/// How close to a join a break must be to count as the join's fault: a few frames or chunks. It is
/// also how far either side of the join valid structure must run for a crossing to be clean.
const JOIN_MARGIN_BYTES: u64 = 4096;
/// Below this share of valid bytes, a stream is not valid enough to say anything about a join.
const JOIN_VALID_FLOOR: f64 = 0.05;

/// Whether a stream runs validly across the join between a file's own clusters and the ones appended
/// to it, from what the format walker found.
///
/// A clean crossing means the format runs through the join: valid structure ends at or just before it
/// and continues from it, without a break, for a few frames or chunks either side. A walker that
/// resyncs, as MP3's does, finds valid frames in any audio, so a high valid fraction proves nothing
/// about a bridge, and a stream that is not valid on either side says nothing either way (on a real
/// volume, a file whose data walked as MP3 for 0.0016% of its bytes, with a chain appended, was called
/// clean because no break happened to fall at the join). The answers are:
///
/// - `clean`: valid structure runs unbroken across the join, for the margin either side;
/// - `breaks`: valid structure is there on at least one side, and not through the join;
/// - `invalid-both-sides`: there is no valid structure either side of the join, so the join is not
///   evidence of anything;
/// - `unknown`: the walker stopped before it reached the join and cannot say.
///
/// A walker that notes where its valid structure is (`valid_spans`) is read directly. One that does not
/// is read by its breaks and valid bytes: a break near the join, and no valid bytes to speak of, are
/// not clean, and a clean one needs every byte before the join to have been valid.
pub(crate) fn join_crossing(report: &crate::verify::VerifyReport, join: u64) -> &'static str {
    let total = report.bytes_examined;
    if join == 0 || join >= total {
        return "unknown";
    }
    let before = JOIN_MARGIN_BYTES.min(join);
    let after = JOIN_MARGIN_BYTES.min(total - join);
    if report.has_valid_spans {
        if report
            .valid_spans
            .iter()
            .any(|&(from, to)| from <= join - before && to >= join + after)
        {
            return "clean";
        }
        let valid_in = |from: u64, to: u64| -> u64 {
            report
                .valid_spans
                .iter()
                .map(|&(start, end)| end.min(to).saturating_sub(start.max(from)))
                .sum()
        };
        let left = valid_in(join - before, join);
        let right = valid_in(join, join + after);
        return if left * 2 < before && right * 2 < after {
            "invalid-both-sides"
        } else {
            "breaks"
        };
    }
    let near = |offset: &u64| offset.abs_diff(join) <= JOIN_MARGIN_BYTES;
    if report.breaks.iter().any(near) {
        return "breaks";
    }
    let reached = report.reached_end || report.breaks.iter().any(|&offset| offset > join);
    if !reached {
        return "unknown";
    }
    if report.valid_bytes >= join {
        "clean"
    } else if report.valid_fraction < JOIN_VALID_FLOOR {
        "invalid-both-sides"
    } else {
        "unknown"
    }
}

pub(crate) fn str_of<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(Value::as_str)
}

fn f64_of(args: &Value, key: &str, default: f64) -> f64 {
    args.get(key).and_then(Value::as_f64).unwrap_or(default)
}

pub(crate) fn bool_of(args: &Value, key: &str, default: bool) -> bool {
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
pub(crate) fn place_of(args: &Value) -> Result<Place> {
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

pub(crate) fn path_of_names(args: &Value) -> Result<Vec<String>> {
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

std::thread_local! {
    /// The agent connection the call being run on this thread came in through.
    static AGENT_CONNECTION: std::cell::RefCell<Option<AgentConnection>> = const { std::cell::RefCell::new(None) };
}

/// Puts the agent connection back to none when a call ends, however it ends.
struct ConnectionGuard(Option<AgentConnection>);

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        AGENT_CONNECTION.with(|slot| *slot.borrow_mut() = self.0.take());
    }
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

    /// Sets how much an agent may change at once and in a session. For tests; the app keeps the
    /// defaults, which are constants in `disk_rebuild_agent`.
    pub fn set_agent_limits(&self, limits: AgentLimits) {
        if let Ok(mut slot) = self.agent_limits.write() {
            *slot = limits;
        }
    }

    pub fn agent_limits(&self) -> AgentLimits {
        self.agent_limits
            .read()
            .map(|found| *found)
            .unwrap_or_default()
    }

    /// Runs a tool for an agent that came in through a connection, so what it changes is kept
    /// under that connection's session.
    pub fn call_as_agent(
        &self,
        tool: &str,
        args: &Value,
        author: &str,
        connection: &AgentConnection,
    ) -> Result<Value> {
        let previous = AGENT_CONNECTION.with(|slot| slot.replace(Some(connection.clone())));
        let _restore = ConnectionGuard(previous);
        self.call(tool, args, author)
    }

    fn agent_connection() -> AgentConnection {
        AGENT_CONNECTION
            .with(|slot| slot.borrow().clone())
            .unwrap_or_default()
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

    /// The catalog's health store: the read journal and the health maps.
    pub(crate) fn health_store(&self) -> Result<Arc<crate::health::HealthStore>> {
        let mut slot = self
            .health_store
            .lock()
            .map_err(|_| anyhow!("health store lock poisoned"))?;
        if let Some(store) = slot.as_ref() {
            return Ok(store.clone());
        }
        let store = Arc::new(crate::health::HealthStore::for_user()?);
        *slot = Some(store.clone());
        Ok(store)
    }

    /// Opens a device or image as a raw source behind its health guard.
    pub(crate) fn open_device(&self, path: &std::path::Path) -> Result<crate::disk::RawDisk> {
        let disk = crate::disk::RawDisk::open_guarded(path, self.health_store()?)?;
        // A whole disk tells which of its stretches are its partitions, so they share its health.
        disk.register_partition_links();
        Ok(disk)
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
        // Starting a copy, and the person's own answers to what an agent did, are refused to every
        // agent here, before any argument is read.
        if author != AUTHOR_USER {
            if let Some(reason) = crate::rebuild_tools::person_only_reason(tool) {
                bail!("{reason}");
            }
        }
        if APP_TOOLS.contains(&tool) {
            return self.app_tool(tool, args);
        }
        match tool {
            "open_raw_disk" => {
                let disk =
                    Arc::new(self.open_device(std::path::Path::new(need_str(args, "path")?))?);
                let overview = serde_json::to_value(disk.overview())?;
                *self
                    .raw_disk
                    .write()
                    .map_err(|_| anyhow!("raw disk lock poisoned"))? = Some(disk);
                Ok(overview)
            }
            // Only for driving the window in a test: the open raw source acts as if its device was unplugged.
            #[cfg(feature = "test-hooks")]
            "test_disconnect_raw" => {
                if std::env::var_os("SALVAGE_TEST_UI").is_none() {
                    bail!("this is only for testing");
                }
                self.raw_disk(args)?.source.simulate_disconnect();
                Ok(json!({"ok": true}))
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
            tool if crate::health_tools::TOOLS.contains(&tool) => {
                let disk = self.raw_disk(args)?;
                crate::health_tools::call(tool, &disk, args, author == AUTHOR_USER)
            }
            "rebind_source" => {
                let current = self.raw_disk(args)?;
                let path = std::path::PathBuf::from(need_str(args, "path")?);
                let rebound = Arc::new(current.rebound(&path)?);
                let answer = json!({
                    "rebound": true,
                    "diskId": rebound.id,
                    "path": rebound.path,
                    "health": crate::health_tools::lane(&rebound),
                });
                *self
                    .raw_disk
                    .write()
                    .map_err(|_| anyhow!("raw disk lock poisoned"))? = Some(rebound);
                Ok(answer)
            }
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
                // shows or opens a real one. Only in a test-hooks build, and then only when asked.
                #[cfg(feature = "test-hooks")]
                if std::env::var_os("SALVAGE_TEST_UI").is_some() {
                    if let Some(file) = std::env::var_os("SALVAGE_TEST_STARTUP_DISKS") {
                        return Ok(serde_json::from_slice(&std::fs::read(file)?)?);
                    }
                }
                // With `since` (the fingerprint of the report last shown), the report is built only
                // when the disks or their mounts are not what that report saw.
                match crate::startup::startup_report_since(internal, str_of(args, "since"))? {
                    Some(report) => Ok(serde_json::to_value(report)?),
                    None => Ok(json!({"unchanged": true})),
                }
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
            // Runs script in the window. Only a test-hooks build has it: no other build can.
            #[cfg(feature = "test-hooks")]
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
                let candidates = session.with_database(|database| {
                    database.bridge_candidates(&session.volume, id, &entropy, limit)
                })?;
                let mut rows = serde_json::to_value(&candidates)?;
                if bool_of(args, "test", false) {
                    self.test_bridge_candidates(&session, id, &candidates, &mut rows, args)?;
                }
                Ok(rows)
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
            // Only for driving the window in a test: never offered, refused unless the test
            // environment is set, and not compiled into any build but a test-hooks one.
            #[cfg(feature = "test-hooks")]
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
                let start = f64_of(args, "at", 0.0);
                let media = self
                    .media
                    .read()
                    .map_err(|_| anyhow!("media lock poisoned"))?
                    .clone()
                    .context("the media port did not start")?;
                if Self::describes_a_reassembly(args) {
                    // A file and a chain tried together, as test_stream would: what ffmpeg makes of it.
                    let made = Self::reassembly(&session, args)?;
                    let mut answer = serde_json::to_value(media.check_clusters(
                        made.clusters.clone(),
                        made.length,
                        start,
                    ))?;
                    answer["reassembly"] = json!({
                        "clusters": made.clusters.len(),
                        "length": made.length,
                        "joins": made.joins.iter().map(|&(offset, head)| json!({"offset": offset, "appendedChainHead": head})).collect::<Vec<_>>(),
                    });
                    return Ok(answer);
                }
                let id = args
                    .get("id")
                    .or_else(|| args.get("entryId"))
                    .and_then(Value::as_i64)
                    .context("argument id (or entryId) is required")?;
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
            "disk_rebuild_place_reassembly" => self.place_reassembly(&session, args, author),
            tool if crate::rebuild_tools::changes_a_disk_rebuild(tool) => {
                let result = if author == AUTHOR_USER {
                    session.with_database(|database| {
                        crate::rebuild_tools::apply(database, tool, args).map(|(value, _)| value)
                    })?
                } else {
                    // An agent builds and changes a Disk Rebuild itself: its change is journaled
                    // under its session, shown as made by the agent, and can be undone.
                    let connection = Self::agent_connection();
                    let limits = self.agent_limits();
                    let summary = crate::rebuild_tools::summarize(tool, args);
                    let target = crate::rebuild_tools::target_of(tool, args)?;
                    session.with_database(|database| {
                        database.agent_change(
                            &connection,
                            &limits,
                            tool,
                            &summary,
                            target,
                            |database| crate::rebuild_tools::apply(database, tool, args),
                        )
                    })?
                };
                self.emit(
                    "disk-rebuild-changed",
                    json!({"diskRebuild": args.get("diskRebuild")}),
                );
                Ok(result)
            }
            "disk_rebuild_resolve" => {
                let path = path_of_names(args)?;
                Ok(serde_json::to_value(session.with_database(
                    |database| database.disk_rebuild_resolve(need_rebuild(args)?, &path),
                )?)?)
            }
            "disk_rebuild_agent_changes" => {
                let limits = self.agent_limits();
                Ok(serde_json::to_value(session.with_database(
                    |database| {
                        database.agent_changes(
                            args.get("diskRebuild").and_then(Value::as_i64),
                            str_of(args, "session"),
                            bool_of(args, "includeUndone", false),
                            u64_of(args, "limit").unwrap_or(50),
                            &limits,
                        )
                    },
                )?)?)
            }
            "disk_rebuild_undo" | "disk_rebuild_undo_session" | "disk_rebuild_restore" => {
                // The person's own answer to what an agent did. The gate in dispatch keeps agents out.
                if author != AUTHOR_USER {
                    bail!("Only the person using the app undoes agent changes.");
                }
                let result = session.with_database(|database| -> Result<Value> {
                    let disk_rebuild = args.get("diskRebuild").and_then(Value::as_i64);
                    Ok(match tool {
                        "disk_rebuild_undo" => {
                            serde_json::to_value(database.agent_undo_last(disk_rebuild)?)?
                        }
                        "disk_rebuild_undo_session" => serde_json::to_value(
                            database.agent_undo_session(str_of(args, "session"), disk_rebuild)?,
                        )?,
                        _ => serde_json::to_value(
                            database.agent_restore(need_rebuild(args)?, str_of(args, "session"))?,
                        )?,
                    })
                })?;
                self.emit("disk-rebuild-changed", json!({"diskRebuild": null}));
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
            "bad_sectors" => Ok(crate::health_tools::bad_sectors(&session)),
            "unexplained_summary" => {
                let examples = bool_of(args, "includeExamples", false);
                if examples && author != AUTHOR_USER && self.names_hidden() {
                    bail!("includeExamples shows names and text from the disk, which the person has kept back from agents.");
                }
                session.unexplained_summary(
                    u64_of(args, "top").map(|top| top as usize),
                    u64_of(args, "minClusters").unwrap_or(DEFAULT_MIN_RUN_CLUSTERS) as u32,
                    u64_of(args, "samples")
                        .map_or(crate::regions::DEFAULT_SUMMARY_SAMPLES, |value| {
                            value as usize
                        }),
                    u64_of(args, "maxMillis").unwrap_or(crate::regions::DEFAULT_SUMMARY_MILLIS),
                    bool_of(args, "perRun", false),
                    examples,
                )
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
        if !session.volume.claims().has_links() {
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

    /// The clusters a stream starts from: an entry's (after overlays) or the ones given.
    fn stream_start(
        session: &Arc<Session>,
        args: &Value,
    ) -> Result<(String, Vec<u32>, Option<u64>)> {
        if let Some(id) = args.get("entryId").and_then(Value::as_i64) {
            session.with_database(|database| {
                let row = database
                    .entry(id)?
                    .with_context(|| format!("no entry with id {id}"))?;
                let clusters = database.entry_clusters(&session.volume, &row)?;
                Ok((row.name, clusters, Some(row.length)))
            })
        } else {
            let clusters =
                extents_of(args, "extents")?.context("give either entryId or extents")?;
            Ok((
                str_of(args, "name").unwrap_or("").to_string(),
                clusters,
                None,
            ))
        }
    }

    /// The clusters of the chain that starts at `head`, at most `limit` of them.
    fn chain_clusters(session: &Arc<Session>, head: u32, limit: u64) -> Result<Vec<u32>> {
        if !session.volume.valid_cluster(head) {
            bail!("cluster {head} is outside the volume");
        }
        if !session.volume.claims().has_links() {
            bail!("NTFS keeps no chain of links between clusters, so there is no chain to append. Use appendExtents.");
        }
        let size = session.volume.geometry.cluster_size();
        let wanted = limit.min(u64::from(session.volume.geometry.cluster_count));
        Ok(session
            .volume
            .clusters_for(head, wanted * size, false)
            .clusters)
    }

    /// The chains `appendChain` and `appendChains` name, as (head, clusters), each followed through the
    /// FAT. Together they add at most `MAX_FOLLOW_CLUSTERS` clusters, and the answer says when that cut one.
    fn appended_chains(session: &Arc<Session>, args: &Value) -> Result<(Vec<AppendedChain>, bool)> {
        let mut heads: Vec<u32> = Vec::new();
        let head_of = |value: &Value| -> Result<u32> {
            let head = value
                .get("head")
                .and_then(Value::as_u64)
                .and_then(|head| u32::try_from(head).ok())
                .context("a chain to append is {head: cluster}")?;
            Ok(head)
        };
        if let Some(value) = args.get("appendChain") {
            heads.push(head_of(value)?);
        }
        if let Some(values) = args.get("appendChains") {
            for value in values
                .as_array()
                .context("appendChains must be a list of {head}")?
            {
                heads.push(head_of(value)?);
            }
        }
        let mut left = MAX_FOLLOW_CLUSTERS;
        let mut cut = false;
        let mut chains = Vec::new();
        for head in heads {
            let clusters = Self::chain_clusters(session, head, left)?;
            cut |= clusters.len() as u64 == left;
            left -= clusters.len() as u64;
            chains.push((head, clusters));
        }
        Ok((chains, cut))
    }

    /// The stream an agent describes: an entry's clusters (or `extents`), then `appendExtents`, then
    /// the chains of `appendChain` and `appendChains`, each followed through the FAT. The same
    /// description serves every tool that judges or keeps a possible reassembly.
    fn reassembly(session: &Arc<Session>, args: &Value) -> Result<Reassembly> {
        let size = session.volume.geometry.cluster_size();
        let (name, mut clusters, entry_length) = Self::stream_start(session, args)?;
        let entry = args.get("entryId").and_then(Value::as_i64);
        // Where each appended piece starts in the stream: a join is tested there.
        let mut joins: Vec<(u64, Option<u32>)> = Vec::new();
        let append = extents_of(args, "appendExtents")?.unwrap_or_default();
        if !append.is_empty() {
            joins.push((clusters.len() as u64 * size, None));
            clusters.extend(append);
        }
        let (chains, cut) = Self::appended_chains(session, args)?;
        for (head, chain) in &chains {
            joins.push((clusters.len() as u64 * size, Some(*head)));
            clusters.extend_from_slice(chain);
        }
        let length = u64_of(args, "length")
            .or(entry_length)
            .unwrap_or(clusters.len() as u64 * size);
        let name = str_of(args, "name").map_or(name, str::to_string);
        Ok(Reassembly {
            name,
            clusters,
            length,
            entry,
            joins,
            chains,
            cut,
        })
    }

    /// Whether the arguments describe a stream made of clusters (an entry plus something appended, or
    /// clusters given directly) rather than one entry by itself.
    fn describes_a_reassembly(args: &Value) -> bool {
        ["appendExtents", "appendChain", "appendChains", "extents"]
            .iter()
            .any(|key| args.get(*key).is_some())
    }

    fn test_stream(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let made = Self::reassembly(session, args)?;
        let test = session.test_stream(&made.name, &made.clusters, made.length)?;
        let mut answer = serde_json::to_value(&test)?;
        if let Some(&(first, _)) = made.joins.first() {
            answer["join"] =
                json!({"offset": first, "crossing": join_crossing(&test.report, first)});
        }
        if !made.chains.is_empty() {
            answer["joins"] = made
                .joins
                .iter()
                .map(|&(offset, head)| {
                    json!({"offset": offset, "appendedChainHead": head, "crossing": join_crossing(&test.report, offset)})
                })
                .collect();
            answer["appendedChains"] = made
                .chains
                .iter()
                .map(|(head, chain)| json!({"head": head, "clusters": chain.len()}))
                .collect();
            if made.cut {
                answer["appendedChainsCut"] = json!(MAX_FOLLOW_CLUSTERS);
            }
        }
        Ok(answer)
    }

    /// Keeps a reassembly: the entry's file gets the clusters of the stream as a hypothesis (a
    /// `file_extents` overlay, which is how the map reads the file from then on), and the file is placed
    /// in a Disk Rebuild, so a copy of the Disk Rebuild holds it whole.
    fn place_reassembly(
        &self,
        session: &Arc<Session>,
        args: &Value,
        author: &str,
    ) -> Result<Value> {
        let made = Self::reassembly(session, args)?;
        let entry = made
            .entry
            .context("entryId is required: the reassembly completes that file")?;
        if made.joins.is_empty() {
            bail!("give something to append (appendExtents, appendChain or appendChains): a file with nothing added is already placed with disk_rebuild_place_file");
        }
        let size = session.volume.geometry.cluster_size();
        if made.length > made.clusters.len() as u64 * size {
            bail!(
                "the reassembly holds {} bytes in {} clusters, fewer than the file's {} bytes",
                made.clusters.len() as u64 * size,
                made.clusters.len(),
                made.length
            );
        }
        let note = str_of(args, "note").map(str::to_string).unwrap_or_else(|| {
            format!(
                "Reassembled: {} clusters, joined at {}",
                made.clusters.len(),
                made.joins
                    .iter()
                    .map(|(offset, _)| offset.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        });
        let place = args.get("place").cloned().context("place is required")?;
        let rebuild = need_rebuild(args)?;
        // The placement first: it refuses a file at the top of the tree, and nothing has changed yet.
        let placed = self.call(
            "disk_rebuild_place_file",
            &json!({"diskRebuild": rebuild, "entry": entry, "place": place}),
            author,
        )?;
        let payload = json!({"extents": to_extents(&made.clusters)});
        let overlay = session.with_database(|database| {
            database.add_overlay(
                crate::map::OVERLAY_FILE_EXTENTS,
                Some(entry),
                &payload,
                &note,
                author,
            )
        })?;
        let summary = if bool_of(args, "resolve", true) {
            Some(session.resolve()?)
        } else {
            None
        };
        self.emit("map-changed", json!({"reason": "overlay", "id": overlay}));
        Ok(json!({
            "item": placed.get("id"),
            "overlay": overlay,
            "entryId": entry,
            "clusters": made.clusters.len(),
            "length": made.length,
            "summary": summary,
        }))
    }

    /// Adds to the first candidates of a ranking the verdict of testing each as the continuation of the
    /// entry: the join's crossing and how much of the stream is valid. Reads are bounded: at most
    /// `testLimit` candidates (default 5, at most 20), and a read budget in bytes for all of them
    /// (`testBudgetBytes`, default 64 MiB); a candidate that would go over it is skipped, and says so.
    fn test_bridge_candidates(
        &self,
        session: &Arc<Session>,
        id: i64,
        candidates: &[crate::map::BridgeCandidate],
        rows: &mut Value,
        args: &Value,
    ) -> Result<()> {
        let size = session.volume.geometry.cluster_size();
        let test_limit = (u64_of(args, "testLimit").unwrap_or(DEFAULT_BRIDGE_TESTS) as usize)
            .min(MAX_BRIDGE_TESTS);
        let mut budget = u64_of(args, "testBudgetBytes").unwrap_or(DEFAULT_BRIDGE_TEST_BYTES);
        let (name, own, length) = Self::stream_start(session, &json!({"entryId": id}))?;
        let length = length.unwrap_or(own.len() as u64 * size);
        let join = own.len() as u64 * size;
        for (row, candidate) in rows
            .as_array_mut()
            .into_iter()
            .flatten()
            .zip(candidates)
            .take(test_limit)
        {
            let chain = Self::chain_clusters(
                session,
                candidate.island.head,
                candidate.island.length.min(MAX_FOLLOW_CLUSTERS),
            )?;
            let wanted = length.min((own.len() + chain.len()) as u64 * size);
            if wanted > budget {
                row["test"] = json!({"skipped": "over the read budget (testBudgetBytes)", "wouldRead": wanted});
                continue;
            }
            budget -= wanted;
            let mut clusters = own.clone();
            clusters.extend_from_slice(&chain);
            let test = session.test_stream(&name, &clusters, wanted)?;
            row["test"] = json!({
                "crossing": join_crossing(&test.report, join),
                "joinOffset": join,
                "format": test.report.format,
                "validFraction": test.report.valid_fraction,
                "validFractionAfterJoin": test.report.valid_fraction_after(join),
                "reachedEnd": test.report.reached_end,
                "bytesRead": wanted,
                "breaksNearJoin": test.report.breaks.iter().filter(|&&offset| offset.abs_diff(join) <= JOIN_MARGIN_BYTES).count(),
            });
        }
        Ok(())
    }

    /// Reads clusters in a chosen order as one file, to show it: a picture, or the start of a
    /// text. Nothing is written; the plan is only data until saved as a hypothesis.
    fn materialize(&self, session: &Arc<Session>, args: &Value) -> Result<Value> {
        let size = session.volume.geometry.cluster_size();
        let (clusters, name_owned, length) = if args.get("entryId").is_some()
            || args.get("appendExtents").is_some()
            || args.get("appendChain").is_some()
            || args.get("appendChains").is_some()
        {
            // An entry (with something appended) or clusters with something appended: as test_stream.
            let made = Self::reassembly(session, args)?;
            (made.clusters, made.name, made.length)
        } else {
            let clusters = extents_of(args, "extents")?.context("extents are required")?;
            let length = u64_of(args, "length").unwrap_or(clusters.len() as u64 * size);
            (
                clusters,
                str_of(args, "name").unwrap_or("").to_string(),
                length,
            )
        };
        let name = name_owned.as_str();
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

#[cfg(test)]
mod join_tests {
    use super::join_crossing;
    use crate::verify::{verify_jpeg, verify_mp3, verify_png, verify_stream};

    /// Bytes that nothing repeats in: no walker finds structure in them.
    fn noise(seed: u64, length: usize) -> Vec<u8> {
        let mut state = seed | 1;
        (0..length)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                (state >> 24) as u8
            })
            .collect()
    }

    /// MPEG-1 Layer III frames at 128 kbit/s and 44.1 kHz, each 417 bytes, with payloads that are not
    /// frame headers.
    fn mp3_frames(seed: u64, count: usize) -> Vec<u8> {
        (0..count)
            .flat_map(|n| {
                let mut frame = vec![0xFF, 0xFB, 0x90, 0x64];
                frame.extend(noise(seed + n as u64, 413).into_iter().map(|byte| {
                    if byte == 0xFF {
                        0x7F
                    } else {
                        byte
                    }
                }));
                frame
            })
            .collect()
    }

    fn crc32(parts: &[&[u8]]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for part in parts {
            for &byte in *part {
                crc ^= u32::from(byte);
                for _ in 0..8 {
                    crc = if crc & 1 != 0 {
                        (crc >> 1) ^ 0xEDB8_8320
                    } else {
                        crc >> 1
                    };
                }
            }
        }
        !crc
    }

    fn png_chunk(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut chunk = (body.len() as u32).to_be_bytes().to_vec();
        chunk.extend_from_slice(kind);
        chunk.extend_from_slice(body);
        chunk.extend_from_slice(&crc32(&[kind, body]).to_be_bytes());
        chunk
    }

    /// A PNG whose pixel data is in chunks of 1000 bytes, so a join falls in or beside a short chunk.
    fn png(seed: u64, pieces: usize) -> Vec<u8> {
        let mut file = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut header = vec![0, 0, 0, 64, 0, 0, 0, 64, 8, 2, 0, 0, 0];
        header.truncate(13);
        file.extend(png_chunk(b"IHDR", &header));
        for piece in 0..pieces {
            file.extend(png_chunk(b"IDAT", &noise(seed + piece as u64, 1000)));
        }
        file.extend(png_chunk(b"IEND", &[]));
        file
    }

    /// A JPEG: the markers, and scan data with no 0xFF in it.
    fn jpeg(seed: u64, scan: usize) -> Vec<u8> {
        let mut file = vec![0xFF, 0xD8, 0xFF, 0xE0, 0, 16];
        file.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        file.extend_from_slice(&[0xFF, 0xDA, 0, 12, 3, 1, 0, 2, 0x11, 3, 0x11, 0, 63, 0]);
        file.extend(
            noise(seed, scan)
                .into_iter()
                .map(|byte| if byte == 0xFF { 0x7E } else { byte }),
        );
        file.extend_from_slice(&[0xFF, 0xD9]);
        file
    }

    fn crossing(name: &str, head: &[u8], rest: &[u8]) -> &'static str {
        let mut stream = head.to_vec();
        stream.extend_from_slice(rest);
        join_crossing(&verify_stream(name, &stream), head.len() as u64)
    }

    #[test]
    fn a_stream_of_noise_with_a_join_says_nothing_about_the_join() {
        let report = verify_mp3(&noise(1, 400_000));
        assert_eq!(
            join_crossing(&report, 200_000),
            "invalid-both-sides",
            "{report:?}"
        );
        assert_eq!(
            crossing("x.mp3", &noise(2, 300_000), &noise(3, 300_000)),
            "invalid-both-sides"
        );
        assert_eq!(
            crossing("x.bin", &noise(2, 3_000), &noise(3, 3_000)),
            "unknown"
        );
    }

    #[test]
    fn a_stream_that_is_valid_mp3_in_one_stretch_in_a_hundred_thousand_bytes_is_not_clean() {
        // What the real case looked like: a frame pair here and there, the walker resyncing every 64 KiB,
        // and a join that fell between two breaks.
        let mut stream = Vec::new();
        for piece in 0..16u64 {
            stream.extend(mp3_frames(piece * 50, 2));
            stream.extend(noise(piece + 900, 65_536 - 2 * 417));
        }
        let join = stream.len() as u64 - 10_000;
        stream.extend(noise(77, 30_000));
        let report = verify_mp3(&stream);
        assert!(report.valid_fraction < 0.05, "{report:?}");
        assert_eq!(join_crossing(&report, join), "invalid-both-sides");
    }

    #[test]
    fn valid_mp3_that_stops_at_the_join_is_breaks_and_not_invalid() {
        let mut stream = mp3_frames(1, 30);
        let join = stream.len() as u64 + 200;
        stream.extend(noise(5, 20_000));
        let report = verify_mp3(&stream);
        assert_eq!(join_crossing(&report, join), "breaks");
        // And frames that start where noise ends are valid on one side only.
        let mut stream = noise(6, 20_000);
        let join = stream.len() as u64 - 200;
        stream.extend(mp3_frames(2, 30));
        assert_eq!(join_crossing(&verify_mp3(&stream), join), "breaks");
    }

    #[test]
    fn an_mp3_cut_and_joined_again_is_clean_and_joined_to_other_data_breaks() {
        let song = mp3_frames(5, 1200);
        for cut in [417 * 300, 417 * 300 + 123, 417 * 700 + 400] {
            assert_eq!(
                crossing("a.mp3", &song[..cut], &song[cut..]),
                "clean",
                "cut at {cut}"
            );
        }
        // Another song joined in the middle of a frame: the frame that straddles the join is not one.
        let other = mp3_frames(900, 800);
        for cut in [417 * 300 + 123, 417 * 700 + 400] {
            assert_eq!(
                crossing("a.mp3", &song[..cut], &other),
                "breaks",
                "cut at {cut}"
            );
        }
        // Noise joined to a song that stops being valid at once.
        assert_eq!(
            crossing("a.mp3", &song[..417 * 300 + 5], &noise(3, 40_000)),
            "breaks"
        );
        // A song joined to noise before it, valid on one side only.
        assert_eq!(
            crossing("a.mp3", &noise(4, 40_000), &song[..417 * 300]),
            "breaks"
        );
    }

    #[test]
    fn a_png_cut_and_joined_again_is_clean_and_joined_to_wrong_data_breaks() {
        let file = png(1, 40);
        for cut in [4_000, 20_345, 33_333] {
            let report = verify_png(&file);
            assert!(report.reached_end);
            assert_eq!(
                crossing("a.png", &file[..cut], &file[cut..]),
                "clean",
                "cut at {cut}"
            );
        }
        let other = png(500, 40);
        for cut in [4_000, 20_345, 33_333] {
            let wrong = &other[cut..];
            assert_eq!(
                crossing("a.png", &file[..cut], wrong),
                "breaks",
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn a_jpeg_cut_and_joined_again_is_clean_and_joined_to_wrong_data_breaks() {
        let file = jpeg(1, 60_000);
        assert!(verify_jpeg(&file).reached_end);
        for cut in [2_000, 30_001] {
            assert_eq!(
                crossing("a.jpg", &file[..cut], &file[cut..]),
                "clean",
                "cut at {cut}"
            );
        }
        // Wrong data with a stray marker in it (0xFF 0x01 is not one): the walker stops there. A JPEG's
        // picture data has no checksum, so other picture data would not be told from this one's.
        let stray: Vec<u8> = (0..4_000).flat_map(|_| [0x41, 0xFF, 0x01]).collect();
        for cut in [2_000, 30_001] {
            let mut stream = file[..cut].to_vec();
            stream.extend_from_slice(&stray);
            let report = verify_stream("a.jpg", &stream);
            assert_eq!(
                join_crossing(&report, cut as u64),
                "breaks",
                "cut at {cut}: {report:?}"
            );
        }
    }

    #[test]
    fn a_join_past_what_was_read_or_at_the_start_is_unknown() {
        let song = mp3_frames(5, 100);
        let report = verify_mp3(&song);
        assert_eq!(join_crossing(&report, 0), "unknown");
        assert_eq!(join_crossing(&report, song.len() as u64), "unknown");
        assert_eq!(join_crossing(&report, song.len() as u64 + 5), "unknown");
    }
}
