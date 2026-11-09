use super::*;
use serde_json::Value;

mod support;

fn mcp(engine: &Engine, tool: &str, arguments: Value) -> Value {
    let mut reply = handle_message(
        engine,
        &json!({
            "jsonrpc": "2.0", "id": 29, "method": "tools/call",
            "params": {"name": tool, "arguments": arguments}
        }),
    )
    .expect("MCP reply");
    for content in reply["result"]["content"].as_array_mut().expect("content") {
        if let Some(text) = content["text"].as_str() {
            if let Ok(value) = serde_json::from_str::<Value>(text) {
                content["text"] = json!(serde_json::to_string(&support::canonical(value)).unwrap());
            }
        }
    }
    reply
}

fn capture(label: &str, image: Vec<u8>, resume: bool) -> Value {
    let source_sha256 = support::sha256(&image);
    let (workspace, dir) = temp_workspace(&format!("parity-{label}"));
    let session = Session::with_source(
        Path::new("/test/image"),
        Arc::new(MemorySource::new(image)),
        workspace,
    )
    .expect("synthetic session");
    if resume {
        let stop = AtomicBool::new(false);
        let paused = run_scan(
            &session.volume,
            &session.workspace,
            ScanOptions { chunk_clusters: 16 },
            &stop,
            |_| stop.store(true, Ordering::Relaxed),
        )
        .expect("paused scan");
        assert_eq!(paused.status, "paused");
        assert_eq!(paused.clusters_scanned, 16);
    }
    scan(&session.volume, &session.workspace);
    let engine = Engine::new();
    engine.set_session(session.clone());
    let volume = &session.volume;
    let database = MapDb::open(&session.workspace.database()).expect("map");
    let progress = read_progress(&database, volume).expect("scan progress");
    let mut layers = serde_json::Map::new();
    for (name, layer) in [
        ("allocation", Layer::Allocation),
        ("content", Layer::Content),
        ("entropy", Layer::Entropy),
        ("ownership", Layer::Ownership),
        ("confidence", Layer::Confidence),
        ("folders", Layer::Folders),
    ] {
        layers.insert(
            name.into(),
            json!(session.map_bytes(
                layer,
                2,
                volume.boot.last_cluster_exclusive(),
                CLUSTER_COUNT as usize,
                0.0,
                None
            )),
        );
    }
    let sets = session
        .query_map("SELECT * FROM sets ORDER BY id")
        .expect("sets");
    let entries = sets["rows"]
        .as_array()
        .expect("rows")
        .iter()
        .map(|row| {
            let id = row["id"].as_i64().expect("entry id");
            let detail = call(&engine, "entry", json!({"id": id}));
            let bytes = if row["is_dir"] == 0 {
                session
                    .read_entry(id, 1024 * 1024)
                    .expect("read fixture file")
                    .1
            } else {
                Vec::new()
            };
            json!({"detail": detail, "readBytes": bytes.len(), "sha256": support::sha256(&bytes)})
        })
        .collect::<Vec<_>>();
    let mut tools = serde_json::Map::new();
    for (tool, args) in [
        ("volume_overview", json!({})),
        ("roots", json!({"limit": 1000})),
        ("search", json!({"query": "mp3", "limit": 1000})),
        ("broken_files", json!({"limit": 1000})),
        ("nameless_chains", json!({"limit": 1000})),
        ("chain_summary", json!({})),
        ("breakdown", json!({"by": ["verdict", "reason"]})),
        ("map_region", json!({"buckets": 16})),
        ("resolve_preview", json!({})),
    ] {
        let mut response = call(&engine, tool, args);
        if tool == "volume_overview" {
            let overview = response.as_object_mut().expect("overview");
            for key in ["device", "wholeDisk", "workspace"] {
                assert!(overview.remove(key).is_some(), "overview metadata {key}");
            }
        }
        tools.insert(tool.into(), response);
    }
    let chains = [3, 30, 70, 75, 100]
        .into_iter()
        .map(|cluster| call(&engine, "follow_chain", json!({"cluster": cluster})))
        .collect::<Vec<_>>();
    let mcp_responses = [
        ("roots", json!({"limit": 1000})),
        ("chain_summary", json!({})),
        ("resolve_preview", json!({})),
        ("inspect_cluster", json!({"cluster": 30})),
        ("inspect_cluster", json!({"cluster": 99999999})),
    ]
    .into_iter()
    .map(|(tool, arguments)| json!({"tool": tool, "arguments": arguments, "response": mcp(&engine, tool, arguments.clone())}))
    .collect::<Vec<_>>();
    let result = json!({
        "sourceSha256": source_sha256,
        "scan": progress,
        "classes": session.workspace.read_cluster_map(&session.workspace.classes(), CLUSTER_COUNT),
        "layers": layers,
        "summary": database.summary().expect("summary"),
        "sets": sets,
        "nodes": session.query_map("SELECT * FROM nodes ORDER BY head").expect("nodes"),
        "islands": session.query_map("SELECT * FROM islands ORDER BY head").expect("islands"),
        "volumeIslands": volume.chain_islands(),
        "entries": entries,
        "tools": tools,
        "chains": chains,
        "mcp": mcp_responses
    });
    drop(database);
    drop(session);
    drop(engine);
    std::fs::remove_dir_all(dir).expect("remove parity workspace");
    result
}

#[test]
fn synthetic_outputs_match_the_pre_driver_reference() {
    let mut fixtures = serde_json::Map::new();
    for (name, image) in [
        ("damage", build_volume()),
        ("reuse", reuse_image()),
        ("macos", volume_macos_has_visited()),
        ("jigsaw", build_jigsaw_volume()),
    ] {
        fixtures.insert(name.into(), capture(name, image, false));
    }
    assert_eq!(
        support::canonical(capture("resumed", build_volume(), true)),
        support::canonical(fixtures["damage"].clone()),
        "a resumed scan must have the same complete parity output"
    );
    let (engine, _session, dir) = engine_with_volume("parity-preview");
    let short = id_named(&engine, "short.bin");
    let previews = json!({
        "bridge": call(&engine, "resolve_preview", json!({"overlays": [{"kind": "file_extents", "target": short, "payload": {"extents": [{"first": 70, "count": 1}, {"first": 75, "count": 1}]}, "note": "try the bridge"}]})),
        "parameters": call(&engine, "resolve_preview", json!({"parameters": {"chainFallbackContiguous": true}})),
        "storedEntryAfterPreview": call(&engine, "entry", json!({"id": short})),
        "storedOverlaysAfterPreview": call(&engine, "list_overlays", json!({}))
    });
    let mut zip = stored_zip("a.txt", b"hello world", 0x0D4A_1185);
    let read_zip = |start: u64, length: u64| {
        let end = (start + length).min(zip.len() as u64) as usize;
        Ok(zip[start as usize..end].to_vec())
    };
    let archive =
        salvage_core::archive::list_zip(zip.len() as u64, &read_zip).expect("zip listing");
    let zip_check =
        salvage_core::archive::check_member(&archive.members[0], zip.len() as u64, &read_zip)
            .expect("zip check");
    let zip_walk = salvage_core::verify::verify_zip(&zip);
    zip[40] ^= 1;
    let damaged_zip_walk = salvage_core::verify::verify_zip(&zip);
    std::fs::remove_dir_all(dir).expect("remove preview workspace");
    support::record(
        "synthetic.json",
        json!({"fixtures": fixtures, "previews": previews, "zip": {"listing": archive, "check": zip_check, "walk": zip_walk, "damagedWalk": damaged_zip_walk}}),
        include_str!("reference/synthetic.json"),
    );
}
