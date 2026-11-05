//! Plays formats the window cannot, using an ffmpeg the person installed themselves.
//!
//! ffmpeg stays optional: the app looks for it and works without it. It reads a file
//! through a private local port, from the same clusters a preview uses, and converts
//! it a few seconds at a time as the player asks. Output goes down a pipe and is never
//! saved, so nothing is written anywhere.

use crate::browse::parse_range;
use crate::engine::Engine;
use crate::session::Session;
use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tiny_http::{Header, Response, Server, StatusCode};

pub const SEGMENT_SECONDS: f64 = 6.0;
const TOKEN_BYTES: usize = 32;
/// Apps opened from Finder get a very short PATH, so the usual places are checked first.
const TOOL_DIRS: &[&str] = &[
    "/opt/homebrew/bin",
    "/usr/local/bin",
    "/opt/local/bin",
    "/usr/bin",
];
/// In order of preference. The first is the Mac's hardware encoder, so it costs little.
const VIDEO_ENCODERS: &[&str] = &["h264_videotoolbox", "libx264"];
const MAX_VIDEO_WIDTH: u32 = 1280;
const VIDEO_BITRATE: &str = "3M";
const AUDIO_BITRATE: &str = "160k";
const THUMB_WIDTH: u32 = 256;
/// A frame this far in, or a tenth of the way, whichever comes first, avoids black openings.
const MAX_THUMB_SEEK_SECONDS: f64 = 60.0;
const THUMB_SEEK_SHARE: f64 = 0.1;
const MAX_CHECK_ERRORS: usize = 12;
/// ffmpeg jumps around inside some files, so small reads and a cache beat big reads.
const CACHE_CHUNK_BYTES: u64 = 256 * 1024;
const CACHE_CHUNKS: usize = 512;

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Tools {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    pub encoder: String,
}

/// Finds a program, so a copy installed after the app started is still noticed.
pub fn find_tool(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    dirs.iter()
        .map(|dir| dir.join(name))
        .find(|path| path.is_file())
}

fn tool_dirs() -> Vec<PathBuf> {
    TOOL_DIRS
        .iter()
        .map(PathBuf::from)
        .chain(
            std::env::var_os("PATH")
                .into_iter()
                .flat_map(|paths| std::env::split_paths(&paths).collect::<Vec<_>>()),
        )
        .collect()
}

pub fn detect_tools() -> Option<Tools> {
    let dirs = tool_dirs();
    let ffmpeg = find_tool("ffmpeg", &dirs)?;
    let ffprobe = find_tool("ffprobe", &dirs)?;
    let output = Command::new(&ffmpeg)
        .args(["-hide_banner", "-encoders"])
        .stdin(Stdio::null())
        .output()
        .ok()?;
    let listed = String::from_utf8_lossy(&output.stdout);
    let encoder = VIDEO_ENCODERS.iter().find(|name| {
        listed
            .lines()
            .any(|line| line.split_whitespace().nth(1) == Some(**name))
    })?;
    Some(Tools {
        ffmpeg,
        ffprobe,
        encoder: (*encoder).to_string(),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct Probe {
    pub duration: f64,
    pub has_video: bool,
    pub has_audio: bool,
}

/// Limits what a damaged or odd file can make ffmpeg open: only the input it was given.
fn input_args(command: &mut Command, input: &str) {
    let allowed = if input.starts_with("http://127.0.0.1:") {
        "http,tcp"
    } else {
        "file"
    };
    command.args(["-protocol_whitelist", allowed]);
    command.args(["-i", input]);
}

/// Reads a file's length and streams. Damaged containers often have no duration.
pub fn probe(tools: &Tools, input: &str) -> Result<Probe> {
    let mut command = Command::new(&tools.ffprobe);
    command.args(["-v", "error", "-of", "json"]);
    command.args(["-show_entries", "format=duration:stream=codec_type"]);
    input_args(&mut command, input);
    let output = command
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .context("starting ffprobe")?;
    let report: Value = serde_json::from_slice(&output.stdout).context("reading ffprobe")?;
    let has = |kind: &str| {
        report["streams"]
            .as_array()
            .is_some_and(|streams| streams.iter().any(|stream| stream["codec_type"] == kind))
    };
    let duration = report["format"]["duration"]
        .as_str()
        .and_then(|text| text.parse::<f64>().ok())
        .filter(|seconds| *seconds > 0.0)
        .context("ffmpeg could not find how long this file is")?;
    if !has("video") && !has("audio") {
        bail!("ffmpeg found no sound or picture in this file");
    }
    Ok(Probe {
        duration,
        has_video: has("video"),
        has_audio: has("audio"),
    })
}

/// A player's list of pieces. Each is only converted when the player asks for it.
pub fn playlist(duration: f64, token: &str) -> String {
    let mut out = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n",
        SEGMENT_SECONDS.ceil() as u64
    );
    let count = (duration / SEGMENT_SECONDS).ceil() as u64;
    for index in 0..count {
        let length = (duration - index as f64 * SEGMENT_SECONDS).min(SEGMENT_SECONDS);
        out.push_str(&format!("#EXTINF:{length:.3},\nseg{index}.ts?t={token}\n"));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Converts one piece, to a pipe. Damaged data is skipped rather than ending the piece.
pub fn segment_command(
    tools: &Tools,
    input: &str,
    probe: &Probe,
    start: f64,
    length: f64,
) -> Command {
    let mut command = Command::new(&tools.ffmpeg);
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    command.args([
        "-fflags",
        "+genpts+discardcorrupt",
        "-err_detect",
        "ignore_err",
    ]);
    command.args(["-ss", &format!("{start:.3}"), "-t", &format!("{length:.3}")]);
    input_args(&mut command, input);
    if probe.has_video {
        command.args(["-map", "0:v:0"]);
        command.arg("-vf");
        command.arg(format!(
            "scale='min({MAX_VIDEO_WIDTH},iw)':-2,format=yuv420p"
        ));
        command.args(["-c:v", &tools.encoder, "-b:v", VIDEO_BITRATE]);
    }
    if probe.has_audio {
        command.args([
            "-map",
            "0:a:0",
            "-c:a",
            "aac",
            "-ac",
            "2",
            "-b:a",
            AUDIO_BITRATE,
        ]);
    }
    command.args(["-output_ts_offset", &format!("{start:.3}")]);
    command.args([
        "-muxdelay",
        "0",
        "-muxpreload",
        "0",
        "-f",
        "mpegts",
        "pipe:1",
    ]);
    command
}

/// One still frame as a small JPEG, to a pipe.
pub fn frame_command(tools: &Tools, input: &str, at: f64) -> Command {
    let mut command = Command::new(&tools.ffmpeg);
    command.args(["-hide_banner", "-loglevel", "error", "-nostdin"]);
    if at > 0.0 {
        command.args(["-ss", &format!("{at:.3}")]);
    }
    input_args(&mut command, input);
    command.args(["-frames:v", "1", "-vf"]);
    command.arg(format!("scale={THUMB_WIDTH}:-2"));
    command.args(["-f", "image2pipe", "-c:v", "mjpeg", "-q:v", "5", "pipe:1"]);
    command
}

pub fn thumb_seek(duration: f64) -> f64 {
    (duration * THUMB_SEEK_SHARE).min(MAX_THUMB_SEEK_SECONDS)
}

/// ffmpeg's output as it is made. Stops ffmpeg as soon as nobody is reading it.
pub struct ChildOutput {
    child: Child,
    stdout: ChildStdout,
}

pub fn spawn_output(mut command: Command) -> Result<ChildOutput> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .context("starting ffmpeg")?;
    let stdout = child.stdout.take().context("ffmpeg has no output")?;
    Ok(ChildOutput { child, stdout })
}

impl Read for ChildOutput {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        self.stdout.read(buffer)
    }
}

impl Drop for ChildOutput {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Recent parts of files. ffmpeg opens many short connections, and they share this.
#[derive(Default)]
struct ChunkCache {
    items: HashMap<(i64, u64), Arc<Vec<u8>>>,
    order: VecDeque<(i64, u64)>,
}

impl ChunkCache {
    fn get(&self, key: (i64, u64)) -> Option<Arc<Vec<u8>>> {
        self.items.get(&key).cloned()
    }

    fn insert(&mut self, key: (i64, u64), data: Arc<Vec<u8>>) {
        if self.items.insert(key, data).is_none() {
            self.order.push_back(key);
        }
        while self.order.len() > CACHE_CHUNKS {
            if let Some(oldest) = self.order.pop_front() {
                self.items.remove(&oldest);
            }
        }
    }
}

/// Part of a file, read from its clusters as ffmpeg asks, however much it asks for.
struct EntryReader {
    session: Arc<Session>,
    cache: Arc<Mutex<ChunkCache>>,
    id: i64,
    position: u64,
    end: u64,
}

impl EntryReader {
    fn chunk(&self, index: u64) -> std::io::Result<Arc<Vec<u8>>> {
        let key = (self.id, index);
        if let Some(found) = self.cache.lock().ok().and_then(|cache| cache.get(key)) {
            return Ok(found);
        }
        let part = self
            .session
            .read_entry_range(self.id, index * CACHE_CHUNK_BYTES, CACHE_CHUNK_BYTES)
            .map_err(std::io::Error::other)?;
        let data = Arc::new(part.data);
        if let Ok(mut cache) = self.cache.lock() {
            cache.insert(key, data.clone());
        }
        Ok(data)
    }
}

impl Read for EntryReader {
    fn read(&mut self, out: &mut [u8]) -> std::io::Result<usize> {
        if self.position >= self.end || out.is_empty() {
            return Ok(0);
        }
        let data = self.chunk(self.position / CACHE_CHUNK_BYTES)?;
        let offset = (self.position % CACHE_CHUNK_BYTES) as usize;
        if offset >= data.len() {
            return Ok(0);
        }
        let count = out
            .len()
            .min(data.len() - offset)
            .min((self.end - self.position) as usize);
        out[..count].copy_from_slice(&data[offset..offset + count]);
        self.position += count as u64;
        Ok(count)
    }
}

/// What happened when ffmpeg tried a few seconds of a file.
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaCheck {
    pub ffmpeg: bool,
    pub duration: Option<f64>,
    pub has_video: bool,
    pub has_audio: bool,
    pub output_bytes: u64,
    pub seconds_taken: f64,
    pub file_requests: u64,
    pub errors: Vec<String>,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaInfo {
    pub available: bool,
    pub encoder: Option<String>,
    pub base_url: String,
    pub token: String,
}

/// A private port for ffmpeg and the window's player. Bound to this Mac only, and every
/// request needs a token made fresh at each launch, because these files are personal.
pub struct MediaServer {
    engine: Arc<Engine>,
    token: String,
    port: u16,
    tools: Mutex<Option<Tools>>,
    probes: Mutex<HashMap<i64, Probe>>,
    cache: Arc<Mutex<ChunkCache>>,
    file_requests: AtomicU64,
}

fn new_token() -> Result<String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn header(name: &str, value: &str) -> Result<Header> {
    Header::from_bytes(name, value).map_err(|_| anyhow!("bad header {name}"))
}

type Answer = Response<Box<dyn Read + Send>>;

fn answer(
    status: u16,
    headers: Vec<Header>,
    body: Box<dyn Read + Send>,
    length: Option<usize>,
) -> Answer {
    Response::new(StatusCode(status), headers, body, length, None)
}

fn text_answer(status: u16, text: &str) -> Answer {
    let data = text.as_bytes().to_vec();
    let length = data.len();
    answer(
        status,
        Vec::new(),
        Box::new(std::io::Cursor::new(data)),
        Some(length),
    )
}

impl MediaServer {
    pub fn start(engine: Arc<Engine>) -> Result<Arc<Self>> {
        let server = Server::http(("127.0.0.1", 0))
            .map_err(|error| anyhow!("cannot start the media port: {error}"))?;
        let port = server
            .server_addr()
            .to_ip()
            .context("media port has no address")?
            .port();
        let media = Arc::new(Self {
            engine,
            token: new_token()?,
            port,
            tools: Mutex::new(None),
            probes: Mutex::new(HashMap::new()),
            cache: Arc::new(Mutex::new(ChunkCache::default())),
            file_requests: AtomicU64::new(0),
        });
        let serving = media.clone();
        std::thread::spawn(move || {
            for request in server.incoming_requests() {
                let media = serving.clone();
                std::thread::spawn(move || {
                    let reply = media.reply(request.url(), request.headers());
                    let _ = request.respond(reply);
                });
            }
        });
        Ok(media)
    }

    /// Looks again each time until ffmpeg is found, so installing it needs no restart.
    pub fn tools(&self) -> Option<Tools> {
        let mut slot = self.tools.lock().ok()?;
        if slot
            .as_ref()
            .is_none_or(|tools| !Path::new(&tools.ffmpeg).is_file())
        {
            *slot = detect_tools();
        }
        slot.clone()
    }

    pub fn info(&self) -> MediaInfo {
        let tools = self.tools();
        MediaInfo {
            available: tools.is_some(),
            encoder: tools.map(|tools| tools.encoder),
            base_url: format!("http://127.0.0.1:{}", self.port),
            token: self.token.clone(),
        }
    }

    fn file_url(&self, id: i64) -> String {
        format!("http://127.0.0.1:{}/file/{id}?t={}", self.port, self.token)
    }

    fn probe_entry(&self, tools: &Tools, id: i64) -> Result<Probe> {
        if let Some(found) = self
            .probes
            .lock()
            .ok()
            .and_then(|probes| probes.get(&id).cloned())
        {
            return Ok(found);
        }
        let found = probe(tools, &self.file_url(id))?;
        if let Ok(mut probes) = self.probes.lock() {
            probes.insert(id, found.clone());
        }
        Ok(found)
    }

    /// Tries a few seconds of a file and reports what ffmpeg said, word for word.
    pub fn check(&self, id: i64, start: f64) -> MediaCheck {
        let Some(tools) = self.tools() else {
            return MediaCheck {
                errors: vec!["ffmpeg is not installed".into()],
                ..MediaCheck::default()
            };
        };
        let found = match self.probe_entry(&tools, id) {
            Ok(found) => found,
            Err(error) => {
                return MediaCheck {
                    ffmpeg: true,
                    errors: vec![format!("{error:#}")],
                    ..MediaCheck::default()
                }
            }
        };
        let mut report = MediaCheck {
            ffmpeg: true,
            duration: Some(found.duration),
            has_video: found.has_video,
            has_audio: found.has_audio,
            ..MediaCheck::default()
        };
        let length = (found.duration - start).clamp(0.0, SEGMENT_SECONDS);
        let began = std::time::Instant::now();
        let requests_before = self.file_requests.load(Ordering::Relaxed);
        match segment_command(&tools, &self.file_url(id), &found, start, length)
            .stdin(Stdio::null())
            .output()
        {
            Ok(output) => {
                report.output_bytes = output.stdout.len() as u64;
                report.errors = String::from_utf8_lossy(&output.stderr)
                    .lines()
                    .take(MAX_CHECK_ERRORS)
                    .map(str::to_string)
                    .collect();
            }
            Err(error) => report.errors.push(error.to_string()),
        }
        report.seconds_taken = began.elapsed().as_secs_f64();
        report.file_requests = self.file_requests.load(Ordering::Relaxed) - requests_before;
        report
    }

    /// A still frame from a video, or a picture format the app cannot decode itself.
    pub fn frame(&self, id: i64) -> Result<Vec<u8>> {
        let tools = self.tools().context("ffmpeg is not installed")?;
        let url = self.file_url(id);
        let at = self
            .probe_entry(&tools, id)
            .map(|found| thumb_seek(found.duration))
            .unwrap_or(0.0);
        let mut image = Vec::new();
        spawn_output(frame_command(&tools, &url, at))?.read_to_end(&mut image)?;
        if image.is_empty() {
            bail!("ffmpeg could not get a picture from this file");
        }
        Ok(image)
    }

    fn reply(&self, url: &str, headers: &[Header]) -> Answer {
        let (path, query) = url.split_once('?').unwrap_or((url, ""));
        let authorised = query
            .split('&')
            .any(|pair| pair.strip_prefix("t=") == Some(self.token.as_str()));
        if !authorised {
            return text_answer(403, "not allowed");
        }
        self.route(path, headers)
            .unwrap_or_else(|error| text_answer(422, &format!("{error:#}")))
    }

    fn route(&self, path: &str, headers: &[Header]) -> Result<Answer> {
        let parts: Vec<&str> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["file", id] => self.serve_file(id.parse()?, headers),
            ["hls", id, "index.m3u8"] => {
                let tools = self.tools().context("ffmpeg is not installed")?;
                let found = self.probe_entry(&tools, id.parse()?)?;
                let body = playlist(found.duration, &self.token).into_bytes();
                let length = body.len();
                Ok(answer(
                    200,
                    vec![header("Content-Type", "application/vnd.apple.mpegurl")?],
                    Box::new(std::io::Cursor::new(body)),
                    Some(length),
                ))
            }
            ["hls", id, piece] => {
                let index: u64 = piece
                    .strip_prefix("seg")
                    .and_then(|rest| rest.strip_suffix(".ts"))
                    .context("no such piece")?
                    .parse()?;
                self.serve_segment(id.parse()?, index)
            }
            _ => Ok(text_answer(404, "no such path")),
        }
    }

    fn serve_file(&self, id: i64, headers: &[Header]) -> Result<Answer> {
        let session = self.engine.session()?;
        session.note_viewing();
        let (total, _) = session.entry_total(id)?;
        let range = headers
            .iter()
            .find(|item| item.field.equiv("Range"))
            .map(|item| item.value.as_str().to_string());
        let (start, length, status) = match range {
            Some(text) => match parse_range(&text, total) {
                Some((start, length)) => (start, length, 206),
                None => {
                    return Ok(answer(
                        416,
                        vec![header("Content-Range", &format!("bytes */{total}"))?],
                        Box::new(std::io::empty()),
                        Some(0),
                    ))
                }
            },
            None => (0, total, 200),
        };
        let mut reply_headers = vec![
            header("Accept-Ranges", "bytes")?,
            header("Content-Type", "application/octet-stream")?,
        ];
        if status == 206 {
            let last = start + length.max(1) - 1;
            reply_headers.push(header(
                "Content-Range",
                &format!("bytes {start}-{last}/{total}"),
            )?);
        }
        self.file_requests.fetch_add(1, Ordering::Relaxed);
        let reader = EntryReader {
            session,
            cache: self.cache.clone(),
            id,
            position: start,
            end: start + length,
        };
        Ok(answer(
            status,
            reply_headers,
            Box::new(reader),
            Some(length as usize),
        ))
    }

    fn serve_segment(&self, id: i64, index: u64) -> Result<Answer> {
        let tools = self.tools().context("ffmpeg is not installed")?;
        let found = self.probe_entry(&tools, id)?;
        let start = index as f64 * SEGMENT_SECONDS;
        if start >= found.duration {
            return Ok(text_answer(404, "past the end of the file"));
        }
        let length = (found.duration - start).min(SEGMENT_SECONDS);
        let output = spawn_output(segment_command(
            &tools,
            &self.file_url(id),
            &found,
            start,
            length,
        ))?;
        Ok(answer(
            200,
            vec![header("Content-Type", "video/mp2t")?],
            Box::new(output),
            None,
        ))
    }
}
