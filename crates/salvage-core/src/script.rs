//! A sandboxed scripting tool, for analysis no fixed tool anticipated.
//!
//! Scripts run in Rhai, which has no file, network or process access. The only
//! way out is the functions registered below, and all of them read. So a script
//! can define its own decoder or carving pass, and cannot change the disk or the map.

use crate::content::{entropy_bits, CLASS_NAMES};
use crate::exfat::{find_sets, FIRST_CLUSTER};
use crate::session::{Scope, Session};
use crate::verify::verify_stream;
use anyhow::{anyhow, Result};
use rhai::serde::{from_dynamic, to_dynamic};
use rhai::{Array, Blob, Dynamic, Engine, EvalAltResult, Scope as RhaiScope, INT};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const DEFAULT_TIMEOUT_SECONDS: u64 = 60;
pub const MAX_TIMEOUT_SECONDS: u64 = 1800;
const MAX_PRINTED_BYTES: usize = 1024 * 1024;
const PROGRESS_CHECK_INTERVAL: u64 = 4096;

pub const API_REFERENCE: &str = "\
Functions available to scripts. Offsets are bytes from the start of the exFAT partition unless noted. \
read(offset, length) -> blob; read_disk(offset, length) -> blob, measured from the start of the whole disk; \
read_cluster(cluster) -> blob; cluster_offset(cluster); cluster_size(); cluster_count(); first_cluster(); \
fat(cluster) -> next value; allocated(cluster) -> bool; predecessor(cluster) -> cluster or 0; \
class_of(cluster) -> class name; entropy_of(cluster) -> bits per byte; ownership_of(cluster) -> 0 unexplained, \
1 one entry, 2 several entries, 3 directory, 4 nameless chain; confidence_of(cluster) -> 0.0-1.0; \
chain(first, max) -> array of clusters; \
u16le(blob, i); u32le(blob, i); u64le(blob, i); u16be(blob, i); u32be(blob, i); ascii(blob, i, n); \
utf16le(blob, i, chars); hex(blob); entropy(blob) -> bits per byte; find(blob, pattern_blob) -> array of positions; \
parse_sets(blob) -> array of maps, including damaged sets; verify(blob, name) -> format walk report; \
sql(query) -> array of maps, read-only. print() output and the final expression are both returned.";

fn fail(error: impl std::fmt::Display) -> Box<EvalAltResult> {
    error.to_string().into()
}

fn slice(blob: &Blob, position: INT, length: usize) -> Result<&[u8], Box<EvalAltResult>> {
    let start = usize::try_from(position).map_err(|_| fail("negative position"))?;
    blob.get(start..start + length).ok_or_else(|| {
        fail(format!(
            "position {start} plus {length} is outside the blob"
        ))
    })
}

fn cluster_argument(session: &Session, cluster: INT) -> Result<u32, Box<EvalAltResult>> {
    u32::try_from(cluster)
        .ok()
        .filter(|&value| session.volume.valid_cluster(value))
        .ok_or_else(|| fail(format!("cluster {cluster} is outside the volume")))
}

fn register_reading(engine: &mut Engine, session: &Arc<Session>) {
    let s = session.clone();
    engine.register_fn(
        "read",
        move |offset: INT, length: INT| -> Result<Blob, Box<EvalAltResult>> {
            s.read(Scope::Partition, offset as u64, length as u64)
                .map_err(fail)
        },
    );
    let s = session.clone();
    engine.register_fn(
        "read_disk",
        move |offset: INT, length: INT| -> Result<Blob, Box<EvalAltResult>> {
            s.read(Scope::WholeDisk, offset as u64, length as u64)
                .map_err(fail)
        },
    );
    let s = session.clone();
    engine.register_fn(
        "read_cluster",
        move |cluster: INT| -> Result<Blob, Box<EvalAltResult>> {
            let value = cluster_argument(&s, cluster)?;
            s.volume.read_cluster(value).map_err(fail)
        },
    );
    let s = session.clone();
    engine.register_fn(
        "cluster_offset",
        move |cluster: INT| -> Result<INT, Box<EvalAltResult>> {
            Ok(s.volume.cluster_offset(cluster_argument(&s, cluster)?) as INT)
        },
    );
    let s = session.clone();
    engine.register_fn("cluster_size", move || s.volume.boot.cluster_size() as INT);
    let s = session.clone();
    engine.register_fn("cluster_count", move || {
        INT::from(s.volume.boot.cluster_count)
    });
    engine.register_fn("first_cluster", || INT::from(FIRST_CLUSTER));
}

fn register_structures(engine: &mut Engine, session: &Arc<Session>) {
    let s = session.clone();
    engine.register_fn(
        "fat",
        move |cluster: INT| -> Result<INT, Box<EvalAltResult>> {
            Ok(INT::from(
                s.volume.fat[cluster_argument(&s, cluster)? as usize],
            ))
        },
    );
    let s = session.clone();
    engine.register_fn(
        "allocated",
        move |cluster: INT| -> Result<bool, Box<EvalAltResult>> {
            Ok(s.volume.is_allocated(cluster_argument(&s, cluster)?))
        },
    );
    let s = session.clone();
    engine.register_fn(
        "predecessor",
        move |cluster: INT| -> Result<INT, Box<EvalAltResult>> {
            Ok(INT::from(
                s.volume
                    .predecessor(cluster_argument(&s, cluster)?)
                    .unwrap_or(0),
            ))
        },
    );
    let s = session.clone();
    engine.register_fn(
        "chain",
        move |first: INT, max: INT| -> Result<Array, Box<EvalAltResult>> {
            let mut out = Array::new();
            let mut current = cluster_argument(&s, first)?;
            let mut seen = std::collections::HashSet::new();
            while s.volume.valid_cluster(current)
                && (out.len() as INT) < max
                && seen.insert(current)
            {
                out.push(Dynamic::from(INT::from(current)));
                current = s.volume.fat[current as usize];
            }
            Ok(out)
        },
    );

    // The per-cluster maps are read once, so a script sees one consistent snapshot.
    let classes = Arc::new(session.class_map());
    let entropy = Arc::new(session.entropy_map());
    let ownership = Arc::new(session.ownership_map());
    let confidence = Arc::new(session.confidence_map());
    let s = session.clone();
    engine.register_fn(
        "class_of",
        move |cluster: INT| -> Result<String, Box<EvalAltResult>> {
            let index = (cluster_argument(&s, cluster)? - FIRST_CLUSTER) as usize;
            Ok(CLASS_NAMES[usize::from(classes[index])].to_string())
        },
    );
    let s = session.clone();
    engine.register_fn(
        "entropy_of",
        move |cluster: INT| -> Result<f64, Box<EvalAltResult>> {
            let index = (cluster_argument(&s, cluster)? - FIRST_CLUSTER) as usize;
            Ok(f64::from(entropy[index]) * 8.0 / 255.0)
        },
    );
    let s = session.clone();
    engine.register_fn(
        "ownership_of",
        move |cluster: INT| -> Result<INT, Box<EvalAltResult>> {
            let index = (cluster_argument(&s, cluster)? - FIRST_CLUSTER) as usize;
            Ok(INT::from(ownership[index]))
        },
    );
    let s = session.clone();
    engine.register_fn(
        "confidence_of",
        move |cluster: INT| -> Result<f64, Box<EvalAltResult>> {
            let index = (cluster_argument(&s, cluster)? - FIRST_CLUSTER) as usize;
            Ok(f64::from(confidence[index]) / 255.0)
        },
    );
}

fn register_decoding(engine: &mut Engine, session: &Arc<Session>, names_hidden: bool) {
    engine.register_fn(
        "u16le",
        |blob: &mut Blob, i: INT| -> Result<INT, Box<EvalAltResult>> {
            let b = slice(blob, i, 2)?;
            Ok(INT::from(u16::from_le_bytes([b[0], b[1]])))
        },
    );
    engine.register_fn(
        "u16be",
        |blob: &mut Blob, i: INT| -> Result<INT, Box<EvalAltResult>> {
            let b = slice(blob, i, 2)?;
            Ok(INT::from(u16::from_be_bytes([b[0], b[1]])))
        },
    );
    engine.register_fn(
        "u32le",
        |blob: &mut Blob, i: INT| -> Result<INT, Box<EvalAltResult>> {
            Ok(INT::from(u32::from_le_bytes(
                slice(blob, i, 4)?.try_into().map_err(fail)?,
            )))
        },
    );
    engine.register_fn(
        "u32be",
        |blob: &mut Blob, i: INT| -> Result<INT, Box<EvalAltResult>> {
            Ok(INT::from(u32::from_be_bytes(
                slice(blob, i, 4)?.try_into().map_err(fail)?,
            )))
        },
    );
    engine.register_fn(
        "u64le",
        |blob: &mut Blob, i: INT| -> Result<INT, Box<EvalAltResult>> {
            Ok(u64::from_le_bytes(slice(blob, i, 8)?.try_into().map_err(fail)?) as INT)
        },
    );
    engine.register_fn(
        "ascii",
        |blob: &mut Blob, i: INT, n: INT| -> Result<String, Box<EvalAltResult>> {
            Ok(String::from_utf8_lossy(slice(blob, i, n as usize)?).into_owned())
        },
    );
    engine.register_fn(
        "utf16le",
        |blob: &mut Blob, i: INT, chars: INT| -> Result<String, Box<EvalAltResult>> {
            let units: Vec<u16> = slice(blob, i, chars as usize * 2)?
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect();
            Ok(String::from_utf16_lossy(&units))
        },
    );
    engine.register_fn("hex", |blob: &mut Blob| {
        blob.iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>()
    });
    engine.register_fn("entropy", |blob: &mut Blob| entropy_bits(blob));
    engine.register_fn("find", |blob: &mut Blob, pattern: Blob| -> Array {
        if pattern.is_empty() {
            return Array::new();
        }
        blob.windows(pattern.len())
            .enumerate()
            .filter(|(_, window)| *window == pattern.as_slice())
            .map(|(index, _)| Dynamic::from(index as INT))
            .collect()
    });
    let s = session.clone();
    engine.register_fn(
        "parse_sets",
        move |blob: &mut Blob| -> Result<Dynamic, Box<EvalAltResult>> {
            to_dynamic(find_sets(blob, blob.len(), s.volume.limits()))
        },
    );
    engine.register_fn(
        "verify",
        |blob: &mut Blob, name: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            to_dynamic(verify_stream(name, blob))
        },
    );
    let s = session.clone();
    engine.register_fn(
        "sql",
        move |query: &str| -> Result<Dynamic, Box<EvalAltResult>> {
            let result = s.query_map_private(query, names_hidden).map_err(fail)?;
            to_dynamic(result.get("rows").cloned().unwrap_or(Value::Null))
        },
    );
}

/// Runs a script and returns its printed output and final value.
/// With `names_hidden`, its SQL reads name columns as NULL; the caller masks names in the answer.
pub fn run_script(
    session: &Arc<Session>,
    source: &str,
    timeout_seconds: u64,
    names_hidden: bool,
) -> Result<Value> {
    let limit = Duration::from_secs(timeout_seconds.clamp(1, MAX_TIMEOUT_SECONDS));
    let started = Instant::now();
    let mut engine = Engine::new();
    engine.set_max_expr_depths(128, 64);
    engine.on_progress(move |operations| {
        if operations % PROGRESS_CHECK_INTERVAL == 0 && started.elapsed() > limit {
            Some(Dynamic::from("time limit reached"))
        } else {
            None
        }
    });
    let printed = Arc::new(Mutex::new(String::new()));
    let sink = printed.clone();
    engine.on_print(move |text| {
        if let Ok(mut buffer) = sink.lock() {
            if buffer.len() < MAX_PRINTED_BYTES {
                buffer.push_str(text);
                buffer.push('\n');
            }
        }
    });
    register_reading(&mut engine, session);
    register_structures(&mut engine, session);
    register_decoding(&mut engine, session, names_hidden);

    let mut scope = RhaiScope::new();
    let outcome = engine.eval_with_scope::<Dynamic>(&mut scope, source);
    let output = printed
        .lock()
        .map_err(|_| anyhow!("print buffer poisoned"))?
        .clone();
    let seconds = started.elapsed().as_secs_f64();
    match outcome {
        Ok(value) => {
            let result: Value = from_dynamic(&value).unwrap_or_else(|_| json!(value.to_string()));
            Ok(json!({"result": result, "printed": output, "seconds": seconds}))
        }
        Err(error) => {
            let message = if matches!(*error, EvalAltResult::ErrorTerminated(..)) {
                format!("stopped: time limit of {} seconds reached", limit.as_secs())
            } else {
                error.to_string()
            };
            Ok(json!({"error": message, "printed": output, "seconds": seconds}))
        }
    }
}
