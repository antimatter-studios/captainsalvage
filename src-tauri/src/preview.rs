//! Serves files to the window live from the disk, through the `lens://` scheme.
//!
//! `lens://localhost/entry/<id>` honours byte ranges, so images, audio, video and PDFs
//! load straight from a file's clusters without a copy anywhere.
//! `lens://localhost/thumb/<id>` returns a small JPEG decoded from the file's real bytes.

use lens_core::browse::{content_type, parse_range, Thumb};
use lens_core::engine::Engine;
use lens_core::media::MediaServer;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use tauri::http::{header, Request, Response, StatusCode};
use tauri::UriSchemeResponder;

const THUMB_WORKERS: usize = 3;
/// Fast scrolling can ask for far more than anyone will look at. The oldest requests go.
const MAX_WAITING_THUMBS: usize = 200;
/// Longest answer to one range request. Players ask again for the rest.
const MAX_RANGE_BYTES: u64 = 8 * 1024 * 1024;
const MAX_WHOLE_FILE_BYTES: u64 = 256 * 1024 * 1024;

type Answer = Response<Vec<u8>>;

struct ThumbJob {
    id: i64,
    responder: UriSchemeResponder,
}

#[derive(Clone)]
pub struct ThumbQueue {
    shared: Arc<(Mutex<VecDeque<ThumbJob>>, Condvar)>,
}

impl ThumbQueue {
    pub fn start(engine: Arc<Engine>, media: Option<Arc<MediaServer>>) -> Self {
        let queue = Self {
            shared: Arc::new((Mutex::new(VecDeque::new()), Condvar::new())),
        };
        for _ in 0..THUMB_WORKERS {
            let shared = queue.shared.clone();
            let engine = engine.clone();
            let media = media.clone();
            std::thread::spawn(move || loop {
                let job = {
                    let Ok(mut waiting) = shared.0.lock() else {
                        return;
                    };
                    loop {
                        // Newest first: what is on screen now matters most.
                        if let Some(job) = waiting.pop_back() {
                            break job;
                        }
                        let Ok(next) = shared.1.wait(waiting) else {
                            return;
                        };
                        waiting = next;
                    }
                };
                job.responder
                    .respond(thumb_answer(&engine, media.as_deref(), job.id));
            });
        }
        queue
    }

    fn push(&self, job: ThumbJob) {
        let Ok(mut waiting) = self.shared.0.lock() else {
            job.responder
                .respond(plain(StatusCode::SERVICE_UNAVAILABLE, "busy"));
            return;
        };
        waiting.push_back(job);
        if waiting.len() > MAX_WAITING_THUMBS {
            if let Some(oldest) = waiting.pop_front() {
                oldest
                    .responder
                    .respond(plain(StatusCode::SERVICE_UNAVAILABLE, "busy"));
            }
        }
        self.shared.1.notify_one();
    }
}

fn builder(status: StatusCode) -> tauri::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::ACCESS_CONTROL_ALLOW_ORIGIN, "*")
}

fn plain(status: StatusCode, text: &str) -> Answer {
    builder(status)
        .header(header::CONTENT_TYPE, "text/plain; charset=utf-8")
        .body(text.as_bytes().to_vec())
        .unwrap_or_default()
}

fn thumb_answer(engine: &Engine, media: Option<&MediaServer>, id: i64) -> Answer {
    let frame = |id: i64| match media {
        Some(media) => media.frame(id),
        None => anyhow::bail!("the media port did not start"),
    };
    let installed = media.is_some_and(|media| media.tools().is_some());
    let thumb = engine.session().and_then(|session| {
        session.entry_thumbnail_using(
            id,
            installed.then_some(&frame as &dyn Fn(i64) -> anyhow::Result<Vec<u8>>),
        )
    });
    match thumb {
        Ok(Thumb::Image(image)) => builder(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, "max-age=3600")
            .body(image.as_ref().clone())
            .unwrap_or_default(),
        Ok(Thumb::Failed(reason)) => plain(StatusCode::UNPROCESSABLE_ENTITY, &reason),
        Err(error) => plain(StatusCode::NOT_FOUND, &format!("{error:#}")),
    }
}

fn entry_answer(engine: &Engine, id: i64, range: Option<&str>) -> anyhow::Result<Answer> {
    let session = engine.session()?;
    let (total, name) = session.entry_total(id)?;
    let kind = content_type(&name);
    let answer = match range {
        Some(text) => {
            let Some((start, wanted)) = parse_range(text, total) else {
                return Ok(builder(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_RANGE, format!("bytes */{total}"))
                    .body(Vec::new())?);
            };
            let part = session.read_entry_range(id, start, wanted.min(MAX_RANGE_BYTES))?;
            let last = part.start + part.data.len().max(1) as u64 - 1;
            builder(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, kind)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {}-{last}/{}", part.start, part.total),
                )
                .header(header::CONTENT_LENGTH, part.data.len())
                .header(header::CACHE_CONTROL, "no-store")
                .body(part.data)?
        }
        None if total > MAX_WHOLE_FILE_BYTES => {
            plain(StatusCode::PAYLOAD_TOO_LARGE, "ask for a byte range")
        }
        None => {
            let whole = session.read_entry_range(id, 0, total)?;
            builder(StatusCode::OK)
                .header(header::CONTENT_TYPE, kind)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_LENGTH, whole.data.len())
                .header(header::CACHE_CONTROL, "no-store")
                .body(whole.data)?
        }
    };
    Ok(answer)
}

/// Answers one request from the window. Disk reads happen off the calling thread.
pub fn handle(
    engine: Arc<Engine>,
    thumbs: ThumbQueue,
    request: Request<Vec<u8>>,
    responder: UriSchemeResponder,
) {
    let path = request.uri().path().trim_matches('/').to_string();
    let mut parts = path.split('/');
    let route = parts.next().unwrap_or("").to_string();
    let Some(id) = parts.next().and_then(|text| text.parse::<i64>().ok()) else {
        responder.respond(plain(StatusCode::NOT_FOUND, "no such path"));
        return;
    };
    match route.as_str() {
        "thumb" => thumbs.push(ThumbJob { id, responder }),
        "entry" => {
            let range = request
                .headers()
                .get(header::RANGE)
                .and_then(|value| value.to_str().ok())
                .map(str::to_string);
            std::thread::spawn(move || {
                let answer = entry_answer(&engine, id, range.as_deref()).unwrap_or_else(|error| {
                    plain(StatusCode::INTERNAL_SERVER_ERROR, &format!("{error:#}"))
                });
                responder.respond(answer);
            });
        }
        _ => responder.respond(plain(StatusCode::NOT_FOUND, "no such path")),
    }
}
