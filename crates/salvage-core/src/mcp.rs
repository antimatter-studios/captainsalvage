//! An MCP server over Streamable HTTP, bound to localhost.
//!
//! Each POST carries a JSON-RPC message and gets a JSON response, which the
//! Streamable HTTP transport allows. Every request needs the bearer token, and
//! browser origins other than the app's own are refused, so a web page cannot
//! reach the server through the browser.

use crate::engine::{Engine, AUTHOR_CLAUDE};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::Arc;

pub const SERVER_NAME: &str = "captain-salvage";
const PREFERRED_PORT: u16 = 47821;
const WORKER_THREADS: usize = 8;
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
const TOKEN_BYTES: usize = 32;
const OWNER_ONLY: u32 = 0o600;
const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const DEFAULT_VERSION: &str = "2025-06-18";
const SCREENSHOT_TOOL: &str = "screenshot_app";

pub const INSTRUCTIONS: &str = "\
Captain Salvage examines a damaged exFAT volume, and has no way to write to the disk. Read tools reach the whole disk. \
Overlays and parameters change only how the map on this Mac interprets the disk, so hypotheses cost nothing and can \
be withdrawn. If access is refused, call disk_access and give the user the exact command it returns. \
Treat damage as something to route around, not a stop sign. Start from the bird's-eye view: status, map_region, \
region_stats, nameless_chains, broken_files. A failed checksum lowers confidence but does not end a trail, so check \
what the entry points at. Scores are estimates. Test reassemblies with test_stream, say what is evidence and what is \
a guess, and record conclusions as overlays or with log_finding so the user can see and reverse them. \
Use query_map and run_script when no fixed tool fits. call get_selection to see what the user has selected.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInfo {
    pub url: String,
    pub token: String,
    pub port: u16,
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": name,
        "description": description,
        "inputSchema": {"type": "object", "properties": properties, "required": required},
    })
}

fn integer(description: &str) -> Value {
    json!({"type": "integer", "description": description})
}

fn string(description: &str) -> Value {
    json!({"type": "string", "description": description})
}

fn boolean(description: &str) -> Value {
    json!({"type": "boolean", "description": description})
}

fn number(description: &str) -> Value {
    json!({"type": "number", "description": description})
}

fn extents(description: &str) -> Value {
    json!({
        "type": "array",
        "description": description,
        "items": {
            "type": "object",
            "properties": {"first": {"type": "integer"}, "count": {"type": "integer"}},
            "required": ["first", "count"],
        },
    })
}

const SCOPE_NOTE: &str = "partition (default): offsets from the start of the exFAT partition. disk: offsets from the start of the whole disk, which also reaches the NTFS partition and any gaps.";

/// Every tool, with enough description for an AI to choose and use it unaided.
pub fn tool_definitions() -> Vec<Value> {
    let page = json!({"offset": integer("Rows to skip."), "limit": integer("Rows to return.")});
    let with_page = |extra: Value| {
        let mut merged = page.clone();
        if let (Some(target), Some(source)) = (merged.as_object_mut(), extra.as_object()) {
            target.extend(source.clone());
        }
        merged
    };
    vec![
        tool("disk_access", "Reports what the OS allows on a device: readable, write-denied, mounted. Returns the exact commands the user must run to fix anything. The user grants access; relay the instructions word for word.", json!({"path": string("Device path, such as /dev/rdisk12s2. Defaults to the open volume.")}), &[]),
        tool("list_disks", "Runs read-only diskutil: `diskutil list`, or `diskutil info` for one device. Use it to find the current disk number, which changes when a disk is reconnected.", json!({"device": string("Optional device, such as disk12s2.")}), &[]),
        tool("open_volume", "Opens an exFAT partition, or a disk image, read-only and makes it the current volume.", json!({"path": string("Device or image path.")}), &["path"]),
        tool("status", "Safety state and progress: OS write denial, mount state, whether structures are unchanged, bytes read, read errors, scan progress and a summary of verdicts.", json!({}), &[]),
        tool("volume_overview", "Main and backup boot sector fields and their differences, checksums, geometry, FAT and bitmap statistics, and the names of content classes.", json!({}), &[]),
        tool("verify_unchanged", "Re-reads boot regions, FAT, bitmap and root directory from disk and compares their hash with the baseline. A mismatch means something wrote to the volume.", json!({}), &[]),
        tool("map_region", "Bird's-eye view of a cluster range, split into buckets. For each: allocation, dominant content, who explains it, mean entropy, best confidence, whether it holds directories. Zoom by narrowing the range.", json!({"startCluster": integer("Default: first cluster."), "endCluster": integer("Exclusive. Default: end of volume."), "buckets": integer("1-200. Default 64.")}), &[]),
        tool("region_stats", "Exact totals for a cluster range: allocation, content groups, ownership, and entropy and confidence histograms.", json!({"startCluster": integer("Default: first cluster."), "endCluster": integer("Exclusive. Default: end of volume.")}), &[]),
        tool("nameless_chains", "Free FAT chains no entry claims, longest first. These may be files whose directory entries are gone. Each lists head, tail, length, how it ends, head content and mean entropy.", with_page(json!({"headClass": integer("Only chains whose head has this content class number. See classNames in volume_overview.")})), &[]),
        tool("broken_files", "Files whose chain ends before their length says it should, most missing data first. These are where trails stop and a route around may exist.", page.clone(), &[]),
        tool("bridge_candidates", "For a file with a broken chain, ranks nameless chains that might continue it, by size fit, distance from its last cluster and entropy similarity. A lead to test with test_stream, not evidence by itself.", json!({"entryId": integer("Entry id."), "limit": integer("Default 20.")}), &["entryId"]),
        tool("roots", "Top-level folders: the volume root, then orphan folders with the most data first. Each has a hint listing some of its entries.", page.clone(), &[]),
        tool("children", "A folder's subfolders, lost subfolders and a page of files, with verdict, confidence and reason.", with_page(json!({"node": integer("Folder head cluster."), "includeHidden": boolean("Include rename leftovers."), "minConfidence": number("0-1 confidence floor for files.")})), &["node"]),
        tool("node", "One folder: path, totals, and how strongly it is linked to its parent.", json!({"head": integer("Folder head cluster.")}), &["head"]),
        tool("entry", "One file or folder entry: all fields, verdict, confidence, reason, path and the extents it uses.", json!({"id": integer("Entry id.")}), &["id"]),
        tool("search", "Finds entries by name, including damaged ones, most confident first.", json!({"query": string("Part of a name."), "limit": integer("Default 100."), "minConfidence": number("0-1 confidence floor.")}), &["query"]),
        tool("file_tests_start", "Tests found files in the background: reads each from the disk, walks its format and decodes images, then raises or lowers its score. Only reads the disk. Skips files already tested. Runs at low priority.", json!({"groups": {"type": "array", "items": {"type": "string"}, "description": "File types: photos, documents, music, archives, other."}}), &["groups"]),
        tool("file_tests_stop", "Stops the background file tests. They resume where they stopped.", json!({}), &[]),
        tool("file_tests_forget", "Forgets stored test results, so those files are tested again after a check has improved. Only changes this Mac's map.", json!({"outcome": string("passed, damaged or untested. Leave out to forget them all.")}), &[]),
        tool("file_tests_status", "How the background file tests are going: how many passed, are damaged or have no test.", json!({}), &[]),
        tool("zip_contents", "Lists what an archive holds, read from the disk: zip, tar, tar.gz, tar.xz, tar.bz2, tar.zst, gz, xz, bz2, zst or 7z, by its name or else its first bytes; format says which. A damaged zip's list is rebuilt from the headers of the files inside, and source says so. A damaged tar lists as far as it can be read, with complete false and a note.", json!({"id": integer("Entry id of the archive.")}), &["id"]),
        tool("zip_member_check", "Unpacks one file from an archive in memory and checks it against its checksum where the format keeps one (zip, 7z, and the whole stream for gz, xz, bz2 and zst; a tar keeps none). Nothing is saved.", json!({"id": integer("Entry id of the archive."), "index": integer("The file's index from zip_contents.")}), &["id", "index"]),
        tool("startup_disks", "The disks attached now, and which volumes this app has seen before, recognised by UUID. Reads nothing from the disks.", json!({"includeInternal": {"type": "boolean", "description": "Also list internal disks."}}), &[]),
        tool("mount_blocks", "The volumes the person asked Captain Salvage to keep unmounted, by volume UUID. While the app is open, macOS cannot mount them, from Finder, Disk Utility, diskutil or plugging the disk in.", json!({}), &[]),
        tool("mount_block_set", "Asks Captain Salvage to keep a volume unmounted while the app is open, or to stop. Blocking only protects a disk, so you may block; only the person can unblock. A blocked volume that is mounted now stays mounted until it is unmounted. Use startup_disks for UUIDs.", json!({"uuid": string("Volume UUID, from startup_disks."), "name": string("The volume's name, to show while it is not attached."), "blocked": boolean("true to keep it unmounted, false to let it mount again.")}), &["uuid", "blocked"]),
        tool("mount_history", "When macOS last wrote to this volume: each date its .Spotlight-V100 and .fseventsd folders changed, with the volume's dirty flag and when the baseline was recorded. Needs a scan, and a baseline cannot see anything before it was recorded.", json!({}), &[]),
        tool("file_copies", "Other found files holding exactly the same bytes as this one. Only knows about files already tested.", json!({"id": integer("Entry id."), "limit": integer("Default 100.")}), &["id"]),
        tool("file_test", "The stored test result for one entry, if it has been tested.", json!({"id": integer("Entry id.")}), &["id"]),
        tool("media_check", "Tests whether ffmpeg can read a video or sound file, by converting a few seconds and returning its exact errors. Nothing is saved. Damage in a file often shows here.", json!({"id": integer("Entry id."), "at": number("Second to test from. Default 0.")}), &["id"]),
        tool("browse_types", "How many files of each type were found (photos, videos, music, documents, archives, other), with bytes and how many look good.", json!({"folder": integer("Only inside this folder's first cluster."), "minConfidence": number("Ignore entries below this confidence.")}), &[]),
        tool("browse_files", "Lists found files of one type, for browsing. Nothing is copied.", json!({"group": string("photos, videos, music, documents, archives, other or all."), "folder": integer("Only inside this folder's first cluster."), "minConfidence": number("Ignore entries below this confidence."), "sort": string("best, largest or newest. Default best."), "text": string("Only names containing this."), "hideCopies": {"type": "boolean", "description": "Show one file for each set of identical bytes, among files already tested."}, "offset": integer("Skip this many."), "limit": integer("Default 100, at most 1000.")}), &[]),
        tool("owners", "Which entries claim a cluster, whether contiguous or through a FAT chain.", json!({"cluster": integer("Cluster number.")}), &["cluster"]),
        tool("read_bytes", "Hex and ASCII dump of raw bytes, up to 64 KiB. Use run_script for larger reads.", json!({"offset": integer("Byte offset."), "length": integer("Default 512."), "scope": string(SCOPE_NOTE)}), &["offset"]),
        tool("inspect_cluster", "Everything known about one cluster: allocation, FAT value, chain, content class, entropy, who claims it, entry sets inside it, and notes. Optionally includes a hex dump.", json!({"cluster": integer("Cluster number."), "includeHex": boolean("Add a hex dump."), "hexBytes": integer("Dump size. Default 512.")}), &["cluster"]),
        tool("parse_entries", "Parses any bytes as exFAT entry sets, including ones with failed checksums. Works on regions the scan did not class as directories.", json!({"offset": integer("Byte offset."), "length": integer("Bytes, up to 16 MiB."), "scope": string(SCOPE_NOTE)}), &["offset", "length"]),
        tool("find_bytes", "Searches raw bytes for hex or text within a range. Stops at the limit or after 90 seconds. Pass next_offset back as start to continue.", json!({"hex": string("Pattern as hex."), "text": string("Pattern as text."), "utf16": boolean("Encode text as UTF-16LE, as exFAT names are."), "start": integer("Start offset."), "end": integer("End offset. Required for disk scope."), "align": integer("Only hits at multiples of this."), "limit": integer("Default 200."), "scope": string(SCOPE_NOTE)}), &[]),
        tool("follow_chain", "Walks the FAT forward from a cluster, returning extents, why it stopped, and the chain's head.", json!({"cluster": integer("Start cluster."), "max": integer("Maximum clusters.")}), &["cluster"]),
        tool("decode_struct", "Reads bytes through a custom structure, with labelled fields, optionally repeated. Field types: u8, i8, u16le, u16be, u32le, u32be, u64le, u64be, exfat_time, filetime, cluster, ascii, utf16le, bytes.", json!({"definition": {"type": "object", "description": "{fields: [{name, offset, type, length?}], count?, stride?}. length is bytes, or characters for utf16le."}, "offset": integer("Byte offset of the first instance."), "scope": string(SCOPE_NOTE)}), &["definition", "offset"]),
        tool("test_stream", "Tests a possible reassembly. Reads the clusters as one stream and walks the format through it (MP3 frames, PNG chunks with CRC, JPEG markers), reporting the valid fraction and exactly which cluster each break falls in. To test a bridge, give entryId plus appendExtents.", json!({"entryId": integer("Start from this entry's clusters."), "extents": extents("Or give the clusters directly."), "appendExtents": extents("Clusters to add to the end."), "length": integer("Stream length. Defaults to the entry's length, or the clusters' size."), "name": string("Name for choosing the format walker.")}), &[]),
        tool("add_overlay", "Records a hypothesis, then recalculates the tree and scores unless resolve is false. Kinds and payloads: accept_entry (target: entry id; {name?, firstCluster?, length?, noFatChain?, isDir?}); file_extents (target: entry id; {extents: [{first, count}]}); folder_parent (target: folder head; {parent?, name?}); directory_cluster (target: cluster; {head}); note (target: cluster); region_label (target: first cluster; {end, label}: says what an unexplained region is, and moves it to explained by a hypothesis on the map); struct (target: partition byte offset; a decode_struct definition, shown on the hex view); lens (a saved view: {name, allocation?, groups?, ownership?, entropyMin?, entropyMax?, confidenceMin?, startCluster?, endCluster?}).", json!({"kind": string("Overlay kind."), "target": integer("Depends on kind."), "payload": {"type": "object"}, "note": string("Why you believe this, and the evidence. Shown to the user."), "resolve": boolean("Default true.")}), &["kind", "note"]),
        tool("list_overlays", "All hypotheses, active and inactive, with their notes.", json!({"onlyActive": boolean("Skip inactive ones.")}), &[]),
        tool("set_overlay_active", "Turns a hypothesis on or off and recalculates. Compare summaries to see its effect.", json!({"id": integer("Overlay id."), "active": boolean("New state."), "resolve": boolean("Default true.")}), &["id", "active"]),
        tool("delete_overlay", "Removes a hypothesis and recalculates.", json!({"id": integer("Overlay id."), "resolve": boolean("Default true.")}), &["id"]),
        tool("get_parameters", "Current resolve parameters.", json!({}), &[]),
        tool("set_parameters", "Changes how damage is weighed, then recalculates. chainFallbackContiguous: when a chain ends early, assume the rest follows on disk. conflictToleranceClusters: clusters a file may share with other entries and still count as whole. ignoreEntryConflicts: ignore overlaps between found entries, while clusters in current use still count.", json!({"chainFallbackContiguous": boolean("See description."), "conflictToleranceClusters": integer("See description."), "ignoreEntryConflicts": boolean("See description."), "resolve": boolean("Default true.")}), &[]),
        tool("resolve", "Recalculates the folder tree, scores and map layers from everything found so far. Works during a scan.", json!({}), &[]),
        tool("query_map", "Read-only SQL on the map, up to 5000 rows. Tables: sets (every entry found; o_ columns hold overlay corrections), nodes (folders), islands (FAT chains), overlays, activity, read_errors, scan_state.", json!({"sql": string("SELECT statement.")}), &["sql"]),
        tool("run_script", &format!("Runs a sandboxed Rhai script, for any analysis no other tool covers: custom decoders, carving passes, statistics. It can read the disk and map, and cannot write to either. {}", crate::script::API_REFERENCE), json!({"source": string("Rhai source."), "timeoutSeconds": integer("Default 60, maximum 1800.")}), &["source"]),
        tool("unexplained_runs", "Regions of free space that no entry or FAT chain explains, largest first, with their main content and entropy. Rebuild the map first for current results. These shrink as the scan finds more directories, so treat them as provisional until the scan completes.", with_page(json!({"minClusters": integer("Smallest region to list. Default 64.")})), &[]),
        tool("explain_region", "Samples clusters across a region and reports evidence about what it is: file signatures at the cluster start and at 4 KiB boundaries inside it, NTFS file records with their names, exFAT entries, readable text, and clusters that mix random and plain data (a sign of older data under newer). Record a conclusion with add_overlay kind region_label.", json!({"startCluster": integer("Start."), "endCluster": integer("End, exclusive."), "samples": integer("Clusters to read, 1-256. Default 64.")}), &["startCluster", "endCluster"]),
        tool("copy_plan", "Works out what copying a selection would do, without writing: file count, size, free space, what falls below the floor, what would be skipped as partial, and whether the destination is refused. Starting a copy is the user's decision in the app's Copy out tab, so recommend a plan and ask them.", json!({"destination": string("Full path of the destination folder. Defaults to the salvage drive."), "entryIds": {"type": "array", "items": {"type": "integer"}}, "folders": {"type": "array", "items": {"type": "integer"}, "description": "Folder head clusters, including everything beneath them."}, "minConfidence": number("0-1 floor."), "includePartial": boolean("Include files with missing or reused clusters.")}), &[]),
        tool("get_salvage_drive", "The folder the user has chosen to receive recovered files, with its free space, if one is set. Only the user can change it, in the Copy out tab.", json!({}), &[]),
        tool("canvas_list", "The person's canvases: each is their own rebuild of the folder tree, made by placing the pieces the scan found (top-level found folders) inside one another, merged, or inside folders they made up. Only the person changes a canvas.", json!({}), &[]),
        tool("canvas_items", "Every item on a canvas: made-up folders, pieces (found folders placed as folders) and merges (found folders whose contents join the folder they are in), with where each sits.", json!({"canvas": integer("Canvas id.")}), &["canvas"]),
        tool("canvas_tree", "One folder of a canvas's rebuilt tree, by the folder names from its root. Where pieces hold files of the same name, the best copy is chosen and every copy is listed, so clashes show.", with_page(json!({"canvas": integer("Canvas id."), "path": {"type": "array", "items": {"type": "string"}, "description": "Folder names from the root. Empty for the root."}})), &["canvas"]),
        tool("canvas_suggest", "Suggests where a piece belongs on one of the person's canvases, without placing it: they accept or dismiss it in the Rebuild tab. Use canvas_unused for pieces, canvas_items and canvas_tree for places, and similar_pieces, names, dates and contents for evidence. Put the evidence in note, in a sentence they can judge. Leave place out to suggest the first piece, the top of the tree.", json!({"canvas": integer("Canvas id."), "head": integer("First cluster of an unused top-level found folder."), "place": {"type": "object", "description": "Where it goes: item (a canvas item id) and optionally head (a found folder inside that item's piece).", "properties": {"item": integer("Canvas item id."), "head": integer("Found folder inside that item.")}}, "merge": boolean("Join its contents into that folder rather than placing it as a folder of its own."), "note": string("Why it belongs there, in a sentence the person can judge.")}), &["canvas", "head", "note"]),
        tool("canvas_suggestions", "Suggestions on a canvas still waiting for the person to accept or dismiss.", json!({"canvas": integer("Canvas id.")}), &["canvas"]),
        tool("canvas_copy_plan", "Works out what copying a canvas's rebuilt tree would do, without writing: each file goes to its rebuilt path, in a folder named after the canvas inside the destination, using the chosen copy of each file. Starting it is the person's decision in the Rebuild tab.", json!({"canvas": integer("Canvas id."), "destination": string("Defaults to the salvage drive."), "minConfidence": number("0-1 floor."), "includePartial": boolean("Include files with missing or reused clusters.")}), &["canvas"]),
        tool("similar_pieces", "Other pieces (top-level found folders) holding many of the same files as this one, by name and size, most in common first, with how much of each other they cover and the share of each one's bytes likely intact. Near copies often come from backups or moves; the one with better data is the one to place.", json!({"head": integer("First cluster of a top-level found folder."), "limit": integer("Most to list. Default 20.")}), &["head"]),
        tool("canvas_unused", "Pieces not yet placed on a canvas, largest first, with how many there are in all and how much they hold.", with_page(json!({"canvas": integer("Canvas id.")})), &["canvas"]),
        tool("basket_list",
            "The person's treasure list (the basket tools), in copy order, with each item's size.",
            json!({}),
            &[],
        ),
        tool(
            "basket_add",
            "Adds a file or folder to the treasure list, for the user to review. Only the user can start the copy.",
            json!({"kind": string("entry or folder."), "target": integer("Entry id, or folder head cluster.")}),
            &["kind", "target"],
        ),
        tool(
            "basket_remove",
            "Removes an item from the treasure list.",
            json!({"id": integer("Basket item id.")}),
            &["id"],
        ),
        tool(
            "basket_move",
            "Moves a basket item to a new place in the copy order. Position 0 is first.",
            json!({"id": integer("Basket item id."), "position": integer("New position.")}),
            &["id", "position"],
        ),
        tool(
            "basket_clear",
            "Empties the treasure list. This removes nothing from any disk.",
            json!({}),
            &[],
        ),
        tool(
            "basket_plan",
            "Works out what copying the basket would do, without writing: exact file count and size with overlaps removed, and whether it fits.",
            json!({"destination": string("Defaults to the salvage drive."), "minConfidence": number("0-1 floor."), "includePartial": boolean("Include files with missing or reused clusters.")}),
            &[],
        ),
        tool(
            "copy_status", "Progress and results of the current or last copy.", json!({}), &[]),
        tool("list_volumes", "Mounted volumes that could receive a copy, with free space and whether each is refused.", json!({}), &[]),
        tool("start_scan", "Starts or resumes the full scan. Refused if the volume is mounted or another process is scanning.", json!({}), &[]),
        tool("stop_scan", "Pauses the scan after the current chunk. It can resume later.", json!({}), &[]),
        tool(SCREENSHOT_TOOL, "Returns a picture of the app window: the same map, list and bytes the user is looking at. Use it to read the map's overall shape, or to check what they mean.", json!({}), &[]),
        tool("finder_status", "Whether the Captain Salvage Finder helper is installed, and which canvases of the open disk are shown in Finder.", json!({}), &[]),
        tool("finder_show", "Shows a canvas in Finder as a read-only folder, and opens it there. Nothing done in Finder reaches the disk.", json!({"canvas": integer("Canvas id.")}), &["canvas"]),
        tool("finder_hide", "Takes a canvas out of Finder.", json!({"canvas": integer("Canvas id.")}), &["canvas"]),
        tool("app_settings", "The window's language and appearance.", json!({}), &[]),
        tool("set_language", "Changes the window's language: en, fr, de or es.", json!({"language": string("en, fr, de or es.")}), &["language"]),
        tool("set_theme", "Changes the window's appearance: system, light or dark.", json!({"theme": string("system, light or dark.")}), &["theme"]),
        tool("get_selection", "What the user currently has selected in the app, and the view they are in.", json!({}), &[]),
        tool("show_in_app", "Moves the user's view to show something. kind: cluster, entry, folder, range ({start, end}) or lens.", json!({"kind": string("What to show."), "value": integer("Cluster, entry id, folder head or lens overlay id."), "start": integer("Range start."), "end": integer("Range end.")}), &["kind"]),
        tool("log_finding", "Posts a conclusion to the app's activity log, so the user can read it later.", json!({"text": string("The finding, and its evidence.")}), &["text"]),
        tool("activity", "Recent tool calls by anyone, newest first.", json!({"afterId": integer("Only items after this id."), "limit": integer("Default 100.")}), &[]),
    ]
}

fn base64(data: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for group in data.chunks(3) {
        let value = u32::from(group[0]) << 16
            | u32::from(group.get(1).copied().unwrap_or(0)) << 8
            | u32::from(group.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[(value >> 18) as usize & 63] as char);
        out.push(ALPHABET[(value >> 12) as usize & 63] as char);
        out.push(if group.len() > 1 {
            ALPHABET[(value >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if group.len() > 2 {
            ALPHABET[value as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

fn rpc_result(id: &Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: &Value, code: i64, message: &str) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}})
}

/// Handles one JSON-RPC message. Returns `None` for notifications.
pub fn handle_message(engine: &Engine, message: &Value) -> Option<Value> {
    let id = message.get("id")?;
    let method = message.get("method").and_then(Value::as_str).unwrap_or("");
    let params = message.get("params").cloned().unwrap_or_else(|| json!({}));
    Some(match method {
        "initialize" => {
            let requested = params
                .get("protocolVersion")
                .and_then(Value::as_str)
                .unwrap_or(DEFAULT_VERSION);
            let version = if SUPPORTED_VERSIONS.contains(&requested) {
                requested
            } else {
                DEFAULT_VERSION
            };
            rpc_result(
                id,
                json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}},
                    "serverInfo": {"name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION")},
                    "instructions": INSTRUCTIONS,
                }),
            )
        }
        "ping" => rpc_result(id, json!({})),
        "tools/list" => rpc_result(id, json!({"tools": tool_definitions()})),
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params
                .get("arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            if name == SCREENSHOT_TOOL {
                let result = match engine.screenshot() {
                    Ok(png) => {
                        json!({"content": [{"type": "image", "data": base64(&png), "mimeType": "image/png"}]})
                    }
                    Err(error) => {
                        json!({"content": [{"type": "text", "text": format!("{error:#}")}], "isError": true})
                    }
                };
                return Some(rpc_result(id, result));
            }
            let result = match engine.call(name, &args, AUTHOR_CLAUDE) {
                Ok(value) => json!({
                    "content": [{"type": "text", "text": serde_json::to_string_pretty(&value).unwrap_or_default()}],
                }),
                Err(error) => {
                    json!({"content": [{"type": "text", "text": format!("{error:#}")}], "isError": true})
                }
            };
            rpc_result(id, result)
        }
        other => rpc_error(id, -32601, &format!("method not found: {other}")),
    })
}

/// Where the running app tells clients how to reach it: its address and its token.
pub fn published_info() -> Result<McpInfo> {
    let path = config_dir()?.join("mcp.json");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("reading {}. Is Captain Salvage open?", path.display()))?;
    Ok(serde_json::from_str(&text)?)
}

fn config_dir() -> Result<PathBuf> {
    crate::workspace::app_support()
}

fn write_private(path: &PathBuf, contents: &str) -> Result<()> {
    fs::write(path, contents)?;
    fs::set_permissions(path, fs::Permissions::from_mode(OWNER_ONLY))?;
    Ok(())
}

/// Returns the saved token, creating one from the OS random source the first time.
fn load_or_create_token() -> Result<String> {
    let path = config_dir()?.join("mcp-token");
    if let Ok(token) = fs::read_to_string(&path) {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return Ok(token);
        }
    }
    let mut bytes = [0u8; TOKEN_BYTES];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let token: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    write_private(&path, &token)?;
    Ok(token)
}

fn origin_allowed(origin: &str) -> bool {
    let host = origin.split("://").nth(1).unwrap_or("");
    let bare = host.split(':').next().unwrap_or("");
    matches!(bare, "localhost" | "127.0.0.1" | "tauri.localhost")
}

fn header_value<'a>(request: &'a tiny_http::Request, name: &'static str) -> Option<&'a str> {
    request
        .headers()
        .iter()
        .find(|header| header.field.equiv(name))
        .map(|header| header.value.as_str())
}

fn respond_status(request: tiny_http::Request, code: u16, text: &str) {
    let _ = request.respond(tiny_http::Response::from_string(text).with_status_code(code));
}

fn serve_one(engine: &Engine, token: &str, mut request: tiny_http::Request) {
    if request.url().split('?').next() != Some("/mcp") {
        return respond_status(request, 404, "not found");
    }
    if let Some(origin) = header_value(&request, "Origin") {
        if !origin_allowed(origin) {
            return respond_status(request, 403, "origin not allowed");
        }
    }
    let expected = format!("Bearer {token}");
    if header_value(&request, "Authorization") != Some(expected.as_str()) {
        return respond_status(request, 401, "missing or wrong bearer token");
    }
    if request.method() != &tiny_http::Method::Post {
        return respond_status(request, 405, "this server only accepts POST");
    }

    let mut body = String::new();
    if request
        .as_reader()
        .take(MAX_BODY_BYTES)
        .read_to_string(&mut body)
        .is_err()
    {
        return respond_status(request, 400, "body is not valid UTF-8");
    }
    let Ok(message) = serde_json::from_str::<Value>(&body) else {
        return respond_status(request, 400, "body is not valid JSON");
    };
    let reply = match &message {
        Value::Array(batch) => {
            let replies: Vec<Value> = batch
                .iter()
                .filter_map(|item| handle_message(engine, item))
                .collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        single => handle_message(engine, single),
    };
    match reply {
        Some(value) => {
            let content_type = tiny_http::Header::from_bytes("Content-Type", "application/json")
                .expect("valid header");
            let _ = request.respond(
                tiny_http::Response::from_string(value.to_string()).with_header(content_type),
            );
        }
        None => respond_status(request, 202, ""),
    }
}

/// Starts the server on background threads. Uses the preferred port when free,
/// and writes the address and token to a file only this account can read.
pub fn start(engine: Arc<Engine>) -> Result<McpInfo> {
    let server = tiny_http::Server::http(("127.0.0.1", PREFERRED_PORT))
        .or_else(|_| tiny_http::Server::http(("127.0.0.1", 0)))
        .map_err(|error| anyhow!("cannot start the MCP server: {error}"))?;
    let port = server
        .server_addr()
        .to_ip()
        .context("server has no IP address")?
        .port();
    let token = load_or_create_token()?;
    let info = McpInfo {
        url: format!("http://127.0.0.1:{port}/mcp"),
        token: token.clone(),
        port,
    };
    write_private(
        &config_dir()?.join("mcp.json"),
        &serde_json::to_string_pretty(&info)?,
    )?;

    let server = Arc::new(server);
    for _ in 0..WORKER_THREADS {
        let server = server.clone();
        let engine = engine.clone();
        let token = token.clone();
        std::thread::spawn(move || {
            while let Ok(request) = server.recv() {
                serve_one(&engine, &token, request);
            }
        });
    }
    Ok(info)
}
