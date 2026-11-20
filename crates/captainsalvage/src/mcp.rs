//! An MCP server over Streamable HTTP, bound to localhost.
//!
//! Each POST carries a JSON-RPC message and gets a JSON response, which the
//! Streamable HTTP transport allows. Every request needs the bearer token, and
//! browser origins other than the app's own are refused, so a web page cannot
//! reach the server through the browser.

use crate::disk_rebuild_agent::AgentConnection;
use crate::engine::{Engine, AUTHOR_CLAUDE};
use anyhow::{anyhow, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const SERVER_NAME: &str = "captainsalvage";
const PREFERRED_PORT: u16 = 47821;
const WORKER_THREADS: usize = 8;
const MAX_BODY_BYTES: u64 = 8 * 1024 * 1024;
const TOKEN_BYTES: usize = 32;
const OWNER_ONLY: u32 = 0o600;
const SUPPORTED_VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];
const DEFAULT_VERSION: &str = "2025-06-18";
const SCREENSHOT_TOOL: &str = "screenshot_app";
const TOOL_PREFIX: &str = "captainsalvage_";
const SESSION_HEADER: &str = "Mcp-Session-Id";
/// A client that does not send its session id back is taken to start a new session after this
/// long without a call.
const SESSION_IDLE: Duration = Duration::from_secs(30 * 60);
const MAX_SESSION_ID_CHARS: usize = 64;
const MAX_KNOWN_CLIENTS: usize = 64;

pub const INSTRUCTIONS: &str = "\
Every tool name starts with `captainsalvage_`. Use the full prefixed name when calling a tool. \
Captain Salvage examines a damaged exFAT, FAT or NTFS volume, and has no way to write to the disk. Call captainsalvage_capabilities to see what is supported for the open filesystem. Read tools reach the whole disk. \
Overlays and parameters change only how the map on this Mac interprets the disk, so hypotheses cost nothing and can \
be withdrawn. If access is refused, call captainsalvage_disk_access and give the user the exact command it returns. \
Treat damage as something to route around, not a stop sign. Start from the bird's-eye view: captainsalvage_status, \
captainsalvage_map_region, captainsalvage_region_stats, captainsalvage_nameless_chains, captainsalvage_broken_files. \
A failed checksum lowers confidence but does not end a trail, so check what the entry points at. Scores are estimates. \
Test reassemblies with captainsalvage_test_stream, say what is evidence and what is a guess, and record conclusions as \
overlays or with captainsalvage_log_finding so the user can see and reverse them. To find files nothing else accounts for, \
start with captainsalvage_unexplained_runs and captainsalvage_carve_candidates, then check promising candidates with \
captainsalvage_read_bytes, captainsalvage_test_stream and captainsalvage_inspect_cluster, and log what you confirm. \
Use captainsalvage_query_map and captainsalvage_run_script when no fixed tool fits. Call captainsalvage_get_selection to \
see what the user has selected. Disk Rebuilds: when the person asks you to build or change one, do it yourself with the captainsalvage_disk_rebuild_* tools \
(create it, make folders, place found folders and files, choose which layer's copy of a file wins), read the tree back with \
captainsalvage_disk_rebuild_tree and captainsalvage_disk_rebuild_resolve, and say what you changed. Every change shows in the \
window marked as made by the agent, and the person can undo it. A Disk Rebuild is virtual: your changes touch only the app's \
own database, never the disk. When they only asked where something belongs, suggest it with captainsalvage_disk_rebuild_suggest \
and let them decide. Only the person copies files out, accepts a suggestion, undoes your changes, or deletes a scan: plan a copy with \
captainsalvage_export_plan (one file or folder) or captainsalvage_disk_rebuild_copy_plan (a Disk Rebuild), say what it would do, \
and ask them to start it.";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpInfo {
    pub url: String,
    pub token: String,
    pub port: u16,
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({
        "name": format!("{TOOL_PREFIX}{name}"),
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

const SCOPE_NOTE: &str = "partition (default): offsets from the start of the filesystem partition. disk: offsets from the start of the whole disk, which also reaches the NTFS partition and any gaps.";

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
    let mut tools = vec![
        tool("open_raw_disk", "Lower level: opens a whole disk, partition, or raw image read-only as raw bytes only, without requiring a recognised filesystem, and gives the diskId the raw tools take. open_source does this and also applies an interpretation. Returns a source ID, byte length, declared partitions and table warnings. Does not replace the current filesystem interpretation.", json!({"path": string("Raw device or image path.")}), &["path"]),
        tool("discover_raw_disk", "Scans all bytes in a bounded batch for filesystem evidence, including old overlapping formats inside currently allocated partitions. Detection never stops at the first format. Returns hypotheses, isolated structures, unreadable ranges and a continuation offset. No findings does not mean empty. Current probes cover FAT, exFAT boot and directory structures, and NTFS boot metadata; other formats remain unknown.", json!({"diskId": string("ID from open_raw_disk."), "offset": integer("Absolute source byte offset."), "length": integer("1 byte to 16 MiB per batch. Continue from nextOffset.")}), &["diskId", "offset", "length"]),
        tool("list_raw_scans", "Lists saved raw scans, each with a confidence that it is the same source: how many of the fingerprint's markers (4 KiB blocks spread over the source) match, changed or could not be read, and where the changed ones are. States: exact (all match), probable (60% or more match), weak (fewer, but enough to show), different (never listed). A match is sampled, not proof that every byte matches. Disk numbers and hardware identity are not required. Saved scans below 60% are hidden and counted unless showAll is true. Only exact and probable can resume; weak, different and incompatible scanners cannot.", json!({"diskId": string("ID from open_raw_disk."), "showAll": boolean("Also list weak matches, below 60% but above 0%. Different sources are never listed.")}), &["diskId"]),
        tool("create_raw_scan", "Creates a persistent raw scan with a stable scan ID separate from its sampled content fingerprint. Saves only on the Mac, never on the examined source. Continue using scan_raw_batch; discover_raw_disk is stateless.", json!({"diskId": string("ID from open_raw_disk."), "offset": integer("Absolute scan start."), "length": integer("Scan range length.")}), &["diskId", "offset", "length"]),
        tool("resume_raw_scan", "Verifies and loads a saved raw scan against the currently open source, including a clone or raw image. Requires explicit acknowledgement of sampled verification, and for a probable match also of the changed markers; the scan then records that it resumed on a changed source and where, because findings near those markers may be stale. Rewinds to retry previous read failures rather than inheriting them as failures of the new source. Does not start scanning.", json!({"diskId": string("ID from open_raw_disk."), "scanId": string("Permanent saved scan ID."), "acceptSampledMatch": boolean("Acknowledge that matching samples do not prove all source bytes are identical."), "acceptChangedMarkers": boolean("Acknowledge that some markers changed or could not be read, so findings near them may be stale. Required for a probable match; ignored for an exact one.")}), &["diskId", "scanId", "acceptSampledMatch"]),
        tool("raw_scan_status", "Reads the durable findings, current-source errors and checkpoint of a raw scan already bound to this open source. Use after an interrupted response to reconcile with what actually committed. Does not rebind or start scanning.", json!({"diskId": string("ID from open_raw_disk."), "scanId": string("ID from create_raw_scan or resume_raw_scan.")}), &["diskId", "scanId"]),
        tool("scan_raw_batch", "Advances a created or verified raw scan by at most 16 MiB. Findings, read failures and checkpoint commit atomically before returning. Uses the saved range, not caller-supplied offsets. A replaced source or scan taken over elsewhere must be verified again.", json!({"diskId": string("ID from open_raw_disk."), "scanId": string("ID from create_raw_scan or resume_raw_scan.")}), &["diskId", "scanId"]),
        tool("inspect_ntfs_record", "Inspects a surviving NTFS FILE record without requiring a working boot sector, MFT or directory tree. Retains damage diagnostics; a validated record does not prove its data clusters were not reused. Never writes the source.", json!({"diskId": string("ID from open_raw_disk."), "volumeStart": integer("Absolute start of the hypothesised NTFS volume; defaults to zero."), "volumeLength": integer("Bounded volume length; defaults to the remaining source."), "recordOffset": integer("Record byte offset relative to volumeStart."), "recordSize": integer("Record length; defaults to 1024, at most 1 MiB."), "sectorSize": integer("Fixup sector size; defaults to 512.")}), &["diskId", "recordOffset"]),
        tool("read_ntfs_stream", "Reads at most 64 KiB from a surviving or deleted NTFS record using its resident value or validated mapping pairs. Does not require a working filesystem. Refuses torn records, unresolved attribute lists, compressed/encrypted streams and out-of-volume runs. Returns hexadecimal; no source or destination files are written. Surviving metadata is not proof its data clusters were not reused.", json!({"diskId": string("ID from open_raw_disk."), "volumeStart": integer("Absolute volume start; defaults to zero."), "volumeLength": integer("Bounded volume length."), "recordOffset": integer("Record offset relative to volumeStart."), "recordSize": integer("Record size; defaults to 1024."), "sectorSize": integer("Fixup sector size; defaults to 512."), "attribute": integer("Stream attribute index from inspect_ntfs_record."), "clusterSize": integer("Cluster size of the hypothesised old filesystem, not the current partition's filesystem."), "offset": integer("Logical stream offset."), "length": integer("At most 64 KiB.")}), &["diskId", "recordOffset", "attribute", "clusterSize", "offset", "length"]),
        tool("rebind_source", "After the drive went away (a USB reset, an unplug) and came back under another device name, points the open source at the new node, so everything that holds the source (the open volume, the map, the copies) reads again. The vanished node is released first: the app no longer holds it open. The new device must be the same drive: it is recognised by the markers taken when the source was opened (at least a probable match), and refused with the reason when it is not. open_source with the new path does the same when the open source is gone. Only reads.", json!({"diskId": string("ID from open_raw_disk or open_source."), "path": string("The new device, such as /dev/rdisk12s1.")}), &["diskId", "path"]),
        tool("read_raw_disk", "Reads bytes without any filesystem interpretation, as hexadecimal. Does not write or repair the source.", json!({"diskId": string("ID from open_raw_disk."), "offset": integer("Absolute source byte offset."), "length": integer("At most 64 KiB.")}), &["diskId", "offset", "length"]),
        tool("open_disk_region", "Lower level (open_source with an extent does this and links the extent to its disk): tries an explicitly labelled filesystem interpretation of a bounded raw source range, then uses the normal recovery tools. Overlapping interpretations are allowed. A surviving exFAT backup boot, or an NTFS boot sector copied in the volume's last sector, can be used as an in-memory interpretation without modifying any disk bytes. Geometry must fit the selected range. NTFS opens in the full workspace when its boot sector or that backup is valid; inspect_ntfs_record and read_ntfs_stream remain for sources with neither.", json!({"diskId": string("ID from open_raw_disk."), "start": integer("Absolute region start."), "length": integer("Bounded region length."), "format": string("exFAT, FAT12, FAT16, FAT32 or NTFS. A hypothesis, validated against available metadata."), "bootOffset": integer("Absolute boot evidence offset; defaults to start. An exFAT backup must be 12 sectors after start. An NTFS backup must be one sector past the end of its volume."), "label": string("Human label for this interpretation.")}), &["diskId", "start", "length", "format"]),
        tool("disk_access", "Reports what the OS allows on a device: readable, write-denied, mounted. Returns the exact commands the user must run to fix anything. The user grants access; relay the instructions word for word.", json!({"path": string("Device path, such as /dev/rdisk12s2. Defaults to the open volume.")}), &[]),
        tool("list_disks", "Runs read-only diskutil: `diskutil list`, or `diskutil info` for one device. Use it to find the current disk number, which changes when a disk is reconnected.", json!({"device": string("Optional device, such as disk12s2.")}), &[]),
        tool("open_source", "The one way to open a device, a partition, a disk image, one extent of a disk, or a stored scan. It always starts from the raw source and applies an interpretation to it: the one the last scan of a matching stored source used, otherwise the best detection from the boot evidence at the start (exFAT, FAT or NTFS, with a confidence), otherwise none (raw bytes only). Opening starts no scan: stored results load at once, and a scan runs when the person starts it. Give path to open a source, or diskId to act on the open one. extent {start, length} narrows a whole disk to one of its extents, which opens as a source of its own, linked to the disk, so a partition opened alone and the same extent seen from its disk share one interpretation and its scans. A source that starts with a volume is a partition: the filesystem is shown first. One that does not is a whole disk: raw bytes, with its extents to open. Stored sources are found by markers. An exact match loads its latest scan for the last-used interpretation. A probable match is never applied silently: the answer is needsChoice with the matches, their changed markers and their scans, and nothing is made; open one of its scans with scanId and acceptChangedMarkers, or open as new with fresh. Weak matches are hidden unless showAll. Returns the scope (disk or partition), the interpretation applied, the choices of the interpretation picker (exFAT, FAT, NTFS, none; which are detected, available or stored), the scan opened and the source's scans, matches, extents and the volume overview. format switches the interpretation and rescan makes a new scan of the same one; both make a new scan and keep the earlier ones, and only the person using the app may do them. The source is read only.", json!({
            "path": string("Device or image path. Not needed with diskId."),
            "diskId": string("ID of the open source, from open_raw_disk or an earlier open_source."),
            "extent": json!({"type": "object", "description": "Open this extent of the whole disk as a source of its own.", "properties": {"start": {"type": "integer"}, "length": {"type": "integer"}}, "required": ["start", "length"]}),
            "format": string("Switch the interpretation: exFAT, FAT (FAT12, FAT16 or FAT32 as the boot sector says), NTFS or none for raw bytes. Starts a new scan for it, or opens the one stored."),
            "bootOffset": integer("With format: where the boot evidence is, relative to the extent, when it is not at the start (an NTFS boot sector copied in the volume's last sector, or an exFAT backup boot)."),
            "scanId": string("Open this stored scan, from list_scans."),
            "rescan": boolean("Make a new scan of the same source and interpretation. The earlier one stays."),
            "acceptChangedMarkers": boolean("The changed markers of a probable match were read and accepted."),
            "fresh": boolean("Open as a new source and apply nothing stored, even if a probable match exists."),
            "showAll": boolean("Also list weak matches, below 60% but above 0%.")
        }), &[]),
        tool("open_volume", "Lower level: opens an exFAT, FAT12/16/32 or NTFS volume device or image as a filesystem, as open_source does when the detection says so, and makes it the current volume. Prefer open_source, which also finds the stored scan for a matching source and lists the others.", json!({"path": string("Device or image path.")}), &["path"]),
        tool("status", "Safety state and progress: OS write denial, mount state, whether structures are unchanged and, if not, what was written and whether the FAT, bitmap and root are read from the baseline's copy instead, bytes read, read errors, scan progress and a summary of verdicts.", json!({}), &[]),
        tool("volume_overview", "Main and backup boot sector fields and their differences, checksums, geometry, FAT and bitmap statistics, and the names of content classes.", json!({}), &[]),
        tool("verify_unchanged", "Re-reads boot regions, FAT, bitmap and root directory from disk and compares their hash with the baseline. A mismatch means something wrote to the volume.", json!({}), &[]),
        tool("structure_changes", "What was written to the volume since its baseline, when the structures have changed: clusters that were free then and are in use now, how many FAT entries changed, which root entries changed and how, and the found files and folders that claimed a written cluster, so lost data there. Null while nothing has changed, or when no copy of the baseline is kept.", json!({}), &[]),
        tool("map_region", "Bird's-eye view of a cluster range, split into buckets. For each: allocation, dominant content, who explains it, mean entropy, best confidence, whether it holds directories. Zoom by narrowing the range.", json!({"startCluster": integer("Default: first cluster."), "endCluster": integer("Exclusive. Default: end of volume."), "buckets": integer("1-200. Default 64.")}), &[]),
        tool("region_stats", "Exact totals for a cluster range: allocation, content groups, ownership, and entropy and confidence histograms.", json!({"startCluster": integer("Default: first cluster."), "endCluster": integer("Exclusive. Default: end of volume.")}), &[]),
        tool("nameless_chains", "Free FAT chains no entry claims, longest first. These may be files whose directory entries are gone. Each lists head, tail, length, how it ends, head content and mean entropy.", with_page(json!({"headClass": integer("Only chains whose head has this content class number. See classNames in volume_overview.")})), &[]),
        tool("broken_files", "Files whose chain ends before their length says it should, most missing data first. These are where trails stop and a route around may exist.", page.clone(), &[]),
        tool("bridge_candidates", "For a file with a broken chain, ranks nameless chains that might continue it, by size fit, distance from its last cluster and entropy similarity. A lead to test with test_stream, not evidence by itself.", json!({"entryId": integer("Entry id."), "limit": integer("Default 20."), "test": boolean("Also test each of the first candidates as the file's continuation, as test_stream with appendChain does, and put the verdict in the row's test: crossing, validFraction, validFractionAfterJoin. One call then ranks and tests."), "testLimit": integer("How many of the first candidates to test. Default 5, at most 20."), "testBudgetBytes": integer("The most bytes all the tests together read. Default 67108864 (64 MiB); a candidate over it is skipped and says so.")}), &["entryId"]),
        tool("roots", "Top-level folders: the volume root, then orphan folders with the most data first. Each has a hint listing some of its entries.", page.clone(), &[]),
        tool("children", "A folder's subfolders, lost subfolders and a page of files, with verdict, confidence and reason.", with_page(json!({"node": integer("Folder head cluster."), "includeHidden": boolean("Include rename leftovers."), "minConfidence": number("0-1 confidence floor for files.")})), &["node"]),
        tool("node", "One folder: path, totals, and how strongly it is linked to its parent.", json!({"head": integer("Folder head cluster.")}), &["head"]),
        tool("tree_path", "The folder heads from a top-level folder down to a folder, or to the folder holding an entry, top first. Empty when the entry is in no folder.", json!({"folder": integer("Folder head cluster."), "entry": integer("Entry id.")}), &[]),
        tool("capabilities", "What Captain Salvage can and cannot do for the open volume's filesystem, feature by feature: supported, limited with the limit stated, or missing. Check this before promising a recovery: a feature listed missing is not offered, and an unreadable stream is reported, never zero filled.", json!({}), &[]),
        tool("ntfs_entry", "NTFS only: everything the entry's FILE record says. Every name and the folder it was linked in, the sequence number, every data stream (the unnamed one is the file, a named one is an alternate stream and is listed, never merged), whether each is resident, compressed, encrypted or sparse, its runs, whether the record has an attribute list, and any damage found in the record.", json!({"id": integer("Entry id.")}), &["id"]),
        tool("entry", "One file or folder entry: all fields, verdict, confidence, reason, path and the extents it uses.", json!({"id": integer("Entry id.")}), &["id"]),
        tool("search", "Finds entries by name, including damaged ones, most confident first. A FAT entry the driver found by its content only has weak: unchecked, or lone_entry when it is the only entry in its cluster (it may be file bytes that look like an entry), and half the confidence. guessed is set on a FAT file whose clusters the driver guessed (its chain was cleared when it was deleted).", json!({"query": string("Part of a name."), "limit": integer("Default 100."), "minConfidence": number("0-1 confidence floor.")}), &["query"]),
        tool("file_tests_start", "Tests found files in the background: reads each from the disk, walks its format and decodes images, then raises or lowers its score. Only reads the disk. Skips files already tested. Runs at low priority. Give file types, or narrow by entry ids, verdicts, reason codes (see breakdown), extensions and a minimum size; every part given must match.", json!({"groups": {"type": "array", "items": {"type": "string"}, "description": "File types: photos, videos, documents, music, archives, other. Empty means every type."}, "ids": {"type": "array", "items": {"type": "integer"}, "description": "Only these entries."}, "verdicts": {"type": "array", "items": {"type": "string"}, "description": "Only these verdicts."}, "reasons": {"type": "array", "items": {"type": "string"}, "description": "Only these reason codes."}, "extensions": {"type": "array", "items": {"type": "string"}, "description": "Only these extensions, lower case, without the dot."}, "minBytes": integer("Only files at least this large.")}), &[]),
        tool("file_tests_stop", "Stops the background file tests. They resume where they stopped.", json!({}), &[]),
        tool("file_tests_forget", "Forgets stored test results, so those files are tested again after a check has improved. Only changes this Mac's map.", json!({"outcome": string("passed, damaged or untested. Leave out to forget them all."), "formats": {"type": "array", "items": {"type": "string"}, "description": "Only results from these walkers, such as mp3, avi, matroska, mpeg-ps, jpeg. Leave out for every format."}}), &[]),
        tool("file_tests_by_type", "For every file type on the volume, by extension and largest first: files, bytes, and how many passed their test, came out damaged, have no test for their type or are too big to test whole, or are not tested yet. Run file_tests_start first to test them.", json!({}), &[]),
        tool("file_tests_status", "How the background file tests are going: how many passed, are damaged or have no test.", json!({}), &[]),
        tool("zip_contents", "Lists what an archive holds, read from the disk: zip, tar, tar.gz, tar.xz, tar.bz2, tar.zst, gz, xz, bz2, zst, 7z or rar, by its name or else its first bytes; format says which. A damaged zip's list is rebuilt from the headers of the files inside, and source says so. A damaged tar lists as far as it can be read, with complete false and a note.", json!({"id": integer("Entry id of the archive.")}), &["id"]),
        tool("zip_member_check", "Unpacks one file from an archive in memory and checks it against its checksum where the format keeps one (zip, 7z, rar, and the whole stream for gz, xz, bz2 and zst; a tar keeps none). Nothing is saved.", json!({"id": integer("Entry id of the archive."), "index": integer("The file's index from zip_contents.")}), &["id", "index"]),
        tool("zip_member_preview", "Unpacks one file from an archive to show it: kind image gives dataBase64 and mime, kind text gives the start of the text, kind none means it cannot be shown here, too-big means a picture over 24 MB. Includes what unpacking found, as zip_member_check does. Nothing is saved.", json!({"id": integer("Entry id of the archive."), "index": integer("The file's index from zip_contents.")}), &["id", "index"]),
        tool("zip_member_copy", "Copies one file from inside an archive to a folder, under its own name, never over another file. Only the person using the app can do this: tell them which file and its size. The destination defaults to the salvage drive, and the disk being recovered is refused.", json!({"id": integer("Entry id of the archive."), "index": integer("The file's index from zip_contents."), "destination": string("A folder. Default: the salvage drive.")}), &["id", "index"]),
        tool("startup_disks", "The disks attached now, and which volumes this app has seen before, recognised by UUID. Reads nothing from the disks. Every report has a fingerprint of the disks and mounts it saw.", json!({"includeInternal": {"type": "boolean", "description": "Also list internal disks."}, "since": {"type": "string", "description": "The fingerprint of the report last seen. When the disks and mounts are as that report saw them, the answer is just {\"unchanged\": true}."}}), &[]),
        tool("mount_blocks", "The volumes the person asked Captain Salvage to keep unmounted, by volume UUID, or by place for a volume macOS gives no UUID (an NTFS volume, a disk image): the disk's identity with the partition's offset and length. While the app is open, macOS cannot mount them, from Finder, Disk Utility, diskutil or plugging the disk in.", json!({}), &[]),
        tool("mount_blocks_mounted", "The volumes the person asked to keep unmounted that are mounted anyway, because they were mounted before the app started (at boot, at login, or while it was closed). While one is mounted, macOS may write to it. Tell the person.", json!({}), &[]),
        tool("mount_block_unmount", "Unmounts one volume the person asked to keep unmounted, by its UUID: only that volume, never the whole disk. Only the person using the app can do this, because unmounting writes a little to the volume.", json!({"uuid": string("Volume UUID, or the place key of a volume with none, from mount_blocks_mounted.")}), &["uuid"]),
        tool("mount_block_set", "Asks Captain Salvage to keep a volume unmounted while the app is open, or to stop. Blocking only protects a disk, so you may block; only the person can unblock. A blocked volume that is mounted now stays mounted until it is unmounted. Use startup_disks: a volume has a uuid when macOS gives it one, and else a mountBlockKey. Block a volume with a uuid by that uuid; block one without by its device (the volume's device from startup_disks, which is read once for its place). To let a volume mount again, name its uuid or mountBlockKey.", json!({"uuid": string("Volume UUID, from startup_disks; or the volume's mountBlockKey when it has no UUID."), "device": string("The volume's device from startup_disks, such as /dev/rdisk7s1, for a volume with no UUID."), "name": string("The volume's name, to show while it is not attached."), "blocked": boolean("true to keep it unmounted, false to let it mount again.")}), &["blocked"]),
        tool("macos_writes", "Where macOS wrote on this volume, by date: the cluster runs of the files in its .Spotlight-V100 and .fseventsd folders. A lost file running through one may have been overwritten there. The entry tool lists such dates for one file as macosWroteOn.", json!({}), &[]),
        tool("mount_history", "When macOS last wrote to this volume: each date its .Spotlight-V100 and .fseventsd folders changed, with the volume's dirty flag and when the baseline was recorded. Needs a scan, and a baseline cannot see anything before it was recorded.", json!({}), &[]),
        tool("file_copies", "Other found files holding exactly the same bytes as this one. Only knows about files already tested.", json!({"id": integer("Entry id."), "limit": integer("Default 100.")}), &["id"]),
        tool("file_test", "The stored test result for one entry, if it has been tested.", json!({"id": integer("Entry id.")}), &["id"]),
        tool("disk_rebuild_place_reassembly", "Keeps a verified reassembly: completes a found file with clusters appended to it (appendExtents, appendChain or appendChains, as test_stream) and places it in a Disk Rebuild, so a copy of the Disk Rebuild holds it whole. It does two things: saves a hypothesis for the file (a file_extents overlay, so the map reads the file from the reassembled clusters from then on, which delete_overlay undoes) and places the file like disk_rebuild_place_file. Test it first with test_stream and media_check.", json!({"diskRebuild": integer("Disk Rebuild id, from disk_rebuild_list."), "entryId": integer("The file's entry id."), "appendExtents": extents("Clusters to add to the end."), "appendChain": json!({"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"], "description": "Add the FAT chain that starts at this cluster."}), "appendChains": json!({"type": "array", "items": {"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"]}, "description": "Several chains to add in order."}), "length": integer("The file's length. Default the entry's."), "place": json!({"type": "object", "description": "Where it goes: item (a Disk Rebuild item id) and optionally head. Required: a file cannot be the top of the tree.", "properties": {"item": {"type": "integer"}, "head": {"type": "integer"}}}), "note": string("Why: what showed the join is right. Kept with the hypothesis."), "resolve": boolean("Rebuild the map after (default true).")}), &["diskRebuild", "entryId", "place"]),
        tool("media_check", "Tests whether ffmpeg can read a video or sound file, by converting a few seconds and returning its exact errors. Nothing is saved. Damage in a file often shows here. Give id for a found file. To test a reassembly, give entryId (or extents) plus appendExtents, appendChain or appendChains, as for test_stream: ffmpeg then reads the clusters in that order as one file, and the answer says so in reassembly. Every byte is the disk's: a cluster that cannot be read fails the check, it is not filled in.", json!({"id": integer("Entry id."), "entryId": integer("Entry id, to test it with something appended."), "extents": extents("Or the clusters directly."), "appendExtents": extents("Clusters to add to the end, to test a reassembly."), "appendChain": json!({"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"], "description": "Add the FAT chain that starts at this cluster, followed here, as test_stream does."}), "appendChains": json!({"type": "array", "items": {"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"]}, "description": "Several chains to add in order."}), "length": integer("Stream length. Default the entry's length."), "name": string("Name for the file type."), "at": number("Second to test from. Default 0.")}), &[]),
        tool("browse_types", "How many files of each type were found (photos, videos, music, documents, archives, other), with bytes and how many look good.", json!({"folder": integer("Only inside this folder's first cluster."), "minConfidence": number("Ignore entries below this confidence.")}), &[]),
        tool("browse_files", "Lists found files of one type, for browsing. Nothing is copied.", json!({"group": string("photos, videos, music, documents, archives, other or all."), "folder": integer("Only inside this folder's first cluster."), "minConfidence": number("Ignore entries below this confidence."), "sort": string("best, largest or newest. Default best."), "text": string("Only names containing this."), "hideCopies": {"type": "boolean", "description": "Show one file for each set of identical bytes, among files already tested."}, "offset": integer("Skip this many."), "limit": integer("Default 100, at most 1000.")}), &[]),
        tool("owners", "Which entries claim a cluster, whether contiguous or through a FAT chain.", json!({"cluster": integer("Cluster number.")}), &["cluster"]),
        tool("read_bytes", "Hex and ASCII dump of raw bytes, up to 64 KiB. Use run_script for larger reads.", json!({"offset": integer("Byte offset."), "length": integer("Default 512."), "scope": string(SCOPE_NOTE)}), &["offset"]),
        tool("inspect_cluster", "Everything known about one cluster: allocation, FAT value, chain, content class, entropy, who claims it, entry sets inside it, and notes. Optionally includes a hex dump.", json!({"cluster": integer("Cluster number."), "includeHex": boolean("Add a hex dump."), "hexBytes": integer("Dump size. Default 512.")}), &["cluster"]),
        tool("parse_entries", "Parses any bytes as exFAT entry sets, including ones with failed checksums. Works on regions the scan did not class as directories.", json!({"offset": integer("Byte offset."), "length": integer("Bytes, up to 16 MiB."), "scope": string(SCOPE_NOTE)}), &["offset", "length"]),
        tool("find_bytes", "Searches raw bytes for hex or text within a range. Stops at the limit or after 90 seconds. Pass next_offset back as start to continue.", json!({"hex": string("Pattern as hex."), "text": string("Pattern as text."), "utf16": boolean("Encode text as UTF-16LE, as exFAT names are."), "start": integer("Start offset."), "end": integer("End offset. Required for disk scope."), "align": integer("Only hits at multiples of this."), "limit": integer("Default 200."), "scope": string(SCOPE_NOTE)}), &[]),
        tool("follow_chain", "Walks the FAT forward from a cluster, returning extents, why it stopped, and the chain's head.", json!({"cluster": integer("Start cluster."), "max": integer("Maximum clusters.")}), &["cluster"]),
        tool("decode_struct", "Reads bytes through a custom structure, with labelled fields, optionally repeated. Field types: u8, i8, u16le, u16be, u32le, u32be, u64le, u64be, exfat_time, filetime, cluster, ascii, utf16le, bytes.", json!({"definition": {"type": "object", "description": "{fields: [{name, offset, type, length?}], count?, stride?}. length is bytes, or characters for utf16le."}, "offset": integer("Byte offset of the first instance."), "scope": string(SCOPE_NOTE)}), &["definition", "offset"]),
        tool("materialize", "Reads clusters in the order given as one file, to show it: kind image gives dataBase64 and mime (up to 24 MB), kind text gives the start of the text, none means the name's type cannot be shown. Pair it with test_stream on the same description (entryId plus appendChain works here too). Nothing is written.", json!({"extents": extents("The clusters, in order."), "entryId": integer("Or an entry, with something appended."), "appendExtents": extents("Clusters to add to the end, to test a reassembly."), "appendChain": json!({"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"], "description": "Add the FAT chain that starts at this cluster, followed here, as test_stream does."}), "appendChains": json!({"type": "array", "items": {"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"]}, "description": "Several chains to add in order."}), "length": integer("File length. Default: the clusters' size, or the entry's length."), "name": string("Name, for the file type.")}), &[]),
        tool("test_stream", "Tests a possible reassembly. Reads the clusters as one stream and walks the format through it (MP3 frames, PNG chunks with CRC, JPEG markers), reporting the valid fraction and exactly which cluster each break falls in. To test a bridge, give entryId plus appendExtents: the answer then has join.crossing: clean only when valid structure runs unbroken across the join (a few frames or chunks either side); breaks when valid structure is on at least one side and not through the join; invalid-both-sides when neither side is valid (the join then says nothing); unknown when the walker stopped before it. A high valid fraction alone proves nothing for a bridge, because a resyncing walker finds valid MP3 frames in any audio.", json!({"summary": boolean("Leave out the cluster runs and per-run profile, which run to megabytes for a film."), "entryId": integer("Start from this entry's clusters."), "extents": extents("Or give the clusters directly."), "appendExtents": extents("Clusters to add to the end."), "appendChain": json!({"type": "object", "properties": {"head": {"type": "integer", "description": "First cluster of a chain."}}, "required": ["head"], "description": "Add the FAT chain that starts at this cluster to the end, followed here, so a bridge_candidates island can be tested without follow_chain and its extents."}), "appendChains": json!({"type": "array", "items": {"type": "object", "properties": {"head": {"type": "integer"}}, "required": ["head"]}, "description": "Several chains to add in order. The answer has joins, one crossing for each. At most 100000 clusters are added in all."}), "length": integer("Stream length. Defaults to the entry's length, or the clusters' size."), "name": string("Name for choosing the format walker.")}), &[]),
        tool("add_overlay", "Records a hypothesis, then recalculates the tree and scores unless resolve is false. Kinds and payloads: accept_entry (target: entry id; {name?, firstCluster?, length?, noFatChain?, isDir?}); file_extents (target: entry id; {extents: [{first, count}]}); folder_parent (target: folder head; {parent?, name?}); directory_cluster (target: cluster; {head}); note (target: cluster); region_label (target: first cluster; {end, label}: says what an unexplained region is, and moves it to explained by a hypothesis on the map); struct (target: partition byte offset; a decode_struct definition, shown on the hex view); lens (a saved view: {name, allocation?, groups?, ownership?, entropyMin?, entropyMax?, confidenceMin?, startCluster?, endCluster?}).", json!({"kind": string("Overlay kind."), "target": integer("Depends on kind."), "payload": {"type": "object"}, "note": string("Why you believe this, and the evidence. Shown to the user."), "resolve": boolean("Default true.")}), &["kind", "note"]),
        tool("list_overlays", "All hypotheses, active and inactive, with their notes.", json!({"onlyActive": boolean("Skip inactive ones.")}), &[]),
        tool("set_overlay_active", "Turns a hypothesis on or off and recalculates. Compare summaries to see its effect.", json!({"id": integer("Overlay id."), "active": boolean("New state."), "resolve": boolean("Default true.")}), &["id", "active"]),
        tool("delete_overlay", "Removes a hypothesis and recalculates.", json!({"id": integer("Overlay id."), "resolve": boolean("Default true.")}), &["id"]),
        tool("get_parameters", "Current resolve parameters.", json!({}), &[]),
        tool("set_parameters", "Changes how damage is weighed, then recalculates. chainFallbackContiguous: when a chain ends early, assume the rest follows on disk. conflictToleranceClusters: clusters a file may share with other entries and still count as whole. ignoreEntryConflicts: ignore overlaps between found entries, while clusters in current use still count. ignoreReuseOrder: count every shared cluster as a conflict; by default a cluster claimed by a deleted entry and by a file made after that entry was deleted belongs to the later file.", json!({"chainFallbackContiguous": boolean("See description."), "conflictToleranceClusters": integer("See description."), "ignoreEntryConflicts": boolean("See description."), "ignoreReuseOrder": boolean("See description."), "resolve": boolean("Default true.")}), &[]),
        tool("resolve", "Recalculates the folder tree, scores and map layers from everything found so far. Works during a scan.", json!({}), &[]),
        tool("query_map", &format!("Read-only SQL on the map, up to 5000 rows. Try breakdown and chain_summary first: they answer most counting questions without names. sets holds every entry found (o_ columns hold overlay corrections; reason_code is why it got its verdict; type_group and extension come from the name), nodes the folders, islands the FAT chains. Tables and columns: {}.", crate::map::schema_description()), json!({"sql": string("SELECT statement.")}), &["sql"]),
        tool("breakdown", &format!("Counts and sums found files, grouped by any of: {}. Never returns names, so it is safe on a disk with private files. reason is a stable code for why a file got its verdict: {}. Filters narrow it first. Groups come largest first.", crate::summaries::BREAKDOWN_DIMENSIONS.iter().map(|(id, _)| *id).collect::<Vec<_>>().join(", "), crate::resolve::REASON_CODES.join(", ")), json!({
            "by": {"type": "array", "items": {"type": "string"}, "description": "Dimensions to group by, in order."},
            "verdict": {"type": "array", "items": {"type": "string"}, "description": "Only these verdicts."},
            "reason": {"type": "array", "items": {"type": "string"}, "description": "Only these reason codes."},
            "type": {"type": "array", "items": {"type": "string"}, "description": "Only these type groups: photos, videos, music, documents, archives, other."},
            "extension": {"type": "array", "items": {"type": "string"}, "description": "Only these extensions, lower case, without the dot."},
            "deleted": boolean("Only deleted entries, or only entries still in use."),
            "minBytes": integer("Only files at least this large."),
            "limit": integer("Groups to return. Default 50, at most 1000.")
        }), &["by"]),
        tool("agent_privacy", "Whether the person keeps file names and contents from agents. While they do, names and paths in every answer read [hidden] with the extension kept, previews and screenshots are refused, and SQL reads name columns as NULL. Only the person can change it, in the window.", json!({}), &[]),
        tool("set_agent_privacy", "Keeps file names and contents from agents, or lets them see again. Only the person using the app can do this: an agent asking is refused.", json!({"namesHidden": boolean("True to keep names and previews from agents.")}), &["namesHidden"]),
        tool("conflicts", "Who else claims a file's clusters: each other entry with its verdict, whether it is deleted, when it was made, its size and the runs of clusters it shares, and who holds them by the map's rule (holds: this-file when the other was deleted before this file was made, the-other the reverse, unsettled otherwise). Also how many shared clusters hold directory entries, and what the shared clusters hold now, with their mean entropy.", json!({"id": integer("Entry id.")}), &["id"]),
        tool("resolve_preview", "What a rebuild would change, without saving anything: give changed parameters (as set_parameters takes them) and hypotheses (as add_overlay takes them), and get the files that would move from one verdict to another, largest first, with counts, bytes and a few entry ids each. Try a rule here before keeping it.", json!({"parameters": {"type": "object", "description": "Parameter changes, for example {\"ignoreReuseOrder\": true}."}, "overlays": {"type": "array", "items": {"type": "object"}, "description": "Hypotheses to try: each {kind, target, payload, note}."}}), &[]),
        tool("chain_summary", "FAT chains no entry claims, grouped by what their first cluster holds and how the chain ends (eof, broken, or joins another chain), with chain counts, clusters and bytes. No names.", json!({}), &[]),
        tool("run_script", &format!("Runs a sandboxed Rhai script, for any analysis no other tool covers: custom decoders, carving passes, statistics. It can read the disk and map, and cannot write to either. {}", crate::script::API_REFERENCE), json!({"source": string("Rhai source."), "timeoutSeconds": integer("Default 60, maximum 1800.")}), &["source"]),
        tool("carve_candidates", "Files found in space nothing on the disk explains: every cluster there that starts like a known file (JPEG, PNG, GIF, PDF, MP3, MP4/MOV, AVI, WAV, zip, 7z, gz, rar, Office, executable), in disk order, with where it ends. The end is read from the file's own structure where the format allows (end_by structure, confidence 0.8); cut short where the structure runs past the space (0.3); or guessed from where similar clusters stop (0.4). Carving assumes the file's clusters follow each other, so a fragmented file comes out partial. Only the page asked for is measured, because it reads the disk. Check a candidate's bytes with read_bytes and test_stream, and its kind with inspect_cluster. On a volume whose allocation is unknown the answer has allocationBasis and allocationWarning: the whole volume is searched, and a candidate may be a file that is in use now.", with_page(json!({})), &[]),
        tool("unexplained_runs", "Regions of free space that no entry or FAT chain explains, largest first, with their main content and entropy. Rebuild the map first for current results. These shrink as the scan finds more directories, so treat them as provisional until the scan completes. On a volume whose allocation is unknown (its bitmap could not be read) the answer is {allocationBasis, allocationWarning, runs}: the regions are searched as if the whole volume, or what the file records leave over, were free space, which is not known to be free. Say so when you report them.", with_page(json!({"minClusters": integer("Smallest region to list. Default 64.")})), &[]),
        tool("bad_sectors", "The ranges of the open volume's source that the health map calls slow, suspect, unreadable or reset-culprit, in the volume's own bytes and clusters (numbered as the map numbers them, from 2), with when each was found. The window draws them on the volume map as flashing red. Nothing is read. Use health_map on the raw source for the full map.", json!({}), &[]),
        tool("unexplained_summary", "One call over the whole unexplained space (free clusters no entry or chain explains): how many regions, how many bytes, bytes by main content, and what a sample of the clusters holds (with allocationBasis and allocationWarning when the volume's allocation is unknown, because then the whole volume is searched as if it were free), so a conclusion about all of it does not take a call per region. The sample (samples clusters in all, default 2048) is spread over the biggest regions in proportion to their size, every region getting some, and stops at maxMillis (default 20000). It counts clusters holding valid exFAT/FAT directory entry sets, clusters that mix random and plain data (older data under newer), file signatures at a cluster start and inside, NTFS records, zero clusters and read errors, as counts, shares of the sample and estimates for the considered regions. No names or text unless includeExamples is true (and the person lets agents see names).", json!({"top": integer("Only the biggest N regions. Default all."), "minClusters": integer("Ignore regions smaller than this. Default 64."), "samples": integer("Clusters to read in all, 1-20000. Default 2048."), "maxMillis": integer("Time limit in milliseconds, at most 120000. Default 20000."), "perRun": boolean("Also give the counts for each region."), "includeExamples": boolean("Also give up to 20 directory names, NTFS names and text excerpts. Refused while names are kept back from agents.")}), &[]),
        tool("explain_region", "Samples clusters across a region and reports evidence about what it is: file signatures at the cluster start and at 4 KiB boundaries inside it, NTFS file records with their names, exFAT entries, readable text, and clusters that mix random and plain data (a sign of older data under newer). Record a conclusion with add_overlay kind region_label.", json!({"startCluster": integer("Start."), "endCluster": integer("End, exclusive."), "samples": integer("Clusters to read, 1-256. Default 64.")}), &["startCluster", "endCluster"]),
        tool("export_plan", "Works out what exporting one file or folder would do, without writing: file count, size, free space, what falls below the floor, what would be skipped as partial, and whether the destination is refused. An export copies it directly to a folder the person chooses, never over an existing file, and checks each copy as it is written. Starting it is the person's decision, from the file's right-click menu in the app, so recommend a plan and ask them.", json!({"kind": string("file or folder."), "target": integer("File entry id, or folder head cluster, including everything beneath it."), "destination": string("Full path of the destination folder. Defaults to the salvage drive."), "minConfidence": number("0-1 floor. Default 0."), "includePartial": boolean("Include files with missing or reused clusters. Default true.")}), &["target"]),
        tool("get_salvage_drive", "The folder the user has chosen to receive recovered files, with its free space, if one is set. Only the user can change it, in the export window.", json!({}), &[]),
        tool("disk_rebuild_list", "A Disk Rebuild is a virtual file tree built from segments of a scan, overlaid read only. The Disk Rebuilds of the open scan: each is a rebuild of the folder tree, made by placing the pieces the scan found (top-level found folders) and single found files inside one another, merged, or inside folders made up. The person and agents both build them: madeByAgent marks one an agent made or renamed. To change one, use the other disk_rebuild_* tools, when the person asked you to.", json!({}), &[]),
        tool("disk_rebuild_items", "Every item on a Disk Rebuild: made-up folders, pieces (found folders placed as folders), merges (found folders whose contents join the folder they are in) and single files, with where each sits. Items are the layers: a later item lies over an earlier one. madeByAgent marks items an agent made or changed.", json!({"diskRebuild": integer("Disk Rebuild id.")}), &["diskRebuild"]),
        tool("disk_rebuild_tree", "One folder of a Disk Rebuild's rebuilt tree, by the folder names from its root. Where pieces hold files of the same name, the best copy is chosen and every copy is listed, so clashes show.", with_page(json!({"diskRebuild": integer("Disk Rebuild id."), "path": {"type": "array", "items": {"type": "string"}, "description": "Folder names from the root. Empty for the root."}})), &["diskRebuild"]),
        tool("disk_rebuild_clashes", "Every file on a Disk Rebuild whose copies clash, because pieces laid over one place hold different files of the same name. Each says which copy was picked and what decided it: person, quality (tested or judged good, all clusters there, a valid entry checksum, the size the entry says), date (the newer), or layer (the later drop, when all else ties). needsYou marks copies that differ but measure the same, which the person should look at; those come first. Copies at the same clusters, or tested and found to hold the same bytes, are one copy and never clash.", with_page(json!({"diskRebuild": integer("Disk Rebuild id."), "needYou": boolean("Only the clashes the person should look at.")})), &["diskRebuild"]),
        tool("disk_rebuild_suggest", "Suggests where a piece belongs on a Disk Rebuild, without placing it: the person accepts or dismisses it in the Rebuild tab. Use it when they asked where something belongs, or did not ask you to build; when they asked you to build or change the Disk Rebuild, use disk_rebuild_place and the other changing tools instead. Use disk_rebuild_unused for pieces, disk_rebuild_items and disk_rebuild_tree for places, and similar_pieces, names, dates and contents for evidence. Put the evidence in note, in a sentence they can judge. Leave place out to suggest the first piece, the top of the tree.", json!({"diskRebuild": integer("Disk Rebuild id."), "head": integer("First cluster of an unused top-level found folder."), "place": {"type": "object", "description": "Where it goes: item (a Disk Rebuild item id) and optionally head (a found folder inside that item's piece).", "properties": {"item": integer("Disk Rebuild item id."), "head": integer("Found folder inside that item.")}}, "merge": boolean("Join its contents into that folder rather than placing it as a folder of its own."), "note": string("Why it belongs there, in a sentence the person can judge.")}), &["diskRebuild", "head", "note"]),
        tool("disk_rebuild_suggestions", "Suggestions on a Disk Rebuild still waiting for the person to accept or dismiss.", json!({"diskRebuild": integer("Disk Rebuild id.")}), &["diskRebuild"]),
        tool("copy_report", "What a copy did, file by file, from the manifest at its destination: each file's target, size, whether it was copied complete or partial and why, and what the last check of the copies found (same, changed or missing). Totals cover the whole copy. destination defaults to the last copy's.", with_page(json!({"destination": string("The copy's destination folder."), "filter": string("all, partial, changed, missing or unchecked.")})), &[]),
        tool("copy_check", "Reads every copied file at a destination again, in the background, and compares it with the hash recorded when it was copied, to catch copies changed or lost since. It reads only the destination, never the disk being recovered. Read the results with copy_report.", json!({"destination": string("The copy's destination folder. Defaults to the last copy's.")}), &[]),
        tool("disk_rebuild_copy_plan", "Works out what copying a Disk Rebuild's rebuilt tree would do, without writing: each file goes to its rebuilt path, in a folder named after the Disk Rebuild inside the destination, using the chosen copy of each file. Starting it is the person's decision in the Rebuild tab.", json!({"diskRebuild": integer("Disk Rebuild id."), "destination": string("Defaults to the salvage drive."), "minConfidence": number("0-1 floor."), "includePartial": boolean("Include files with missing or reused clusters.")}), &["diskRebuild"]),
        tool("similar_pieces", "Other pieces (top-level found folders) holding many of the same files as this one, by name and size, most in common first, with how much of each other they cover and the share of each one's bytes likely intact. Near copies often come from backups or moves; the one with better data is the one to place.", json!({"head": integer("First cluster of a top-level found folder."), "limit": integer("Most to list. Default 20.")}), &["head"]),
        tool("disk_rebuild_unused", "Pieces not yet placed on a Disk Rebuild, largest first, with how many there are in all and how much they hold.", with_page(json!({"diskRebuild": integer("Disk Rebuild id.")})), &["diskRebuild"]),
        tool(
            "copy_status", "Progress and results of the current or last copy.", json!({}), &[]),
        tool("list_volumes", "Mounted volumes that could receive a copy, with free space and whether each is refused.", json!({}), &[]),
        tool("start_scan", "Starts or resumes the full scan. Refused if the volume is mounted or another process is scanning.", json!({}), &[]),
        tool("stop_scan", "Pauses the scan after the current chunk. It can resume later.", json!({}), &[]),
        tool(SCREENSHOT_TOOL, "Returns a picture of the app window: the same map, list and bytes the user is looking at. Use it to read the map's overall shape, or to check what they mean.", json!({}), &[]),
        tool("finder_status", "Whether the Captain Salvage Finder helper is installed, and which Disk Rebuilds of the open disk are shown in Finder.", json!({}), &[]),
        tool("finder_show", "Shows a Disk Rebuild in Finder as a read-only folder, and opens it there. Nothing done in Finder reaches the disk.", json!({"diskRebuild": integer("Disk Rebuild id.")}), &["diskRebuild"]),
        tool("finder_hide", "Takes a Disk Rebuild out of Finder.", json!({"diskRebuild": integer("Disk Rebuild id.")}), &["diskRebuild"]),
        tool("app_settings", "The window's language, appearance and mode.", json!({}), &[]),
        tool("set_mode", "Changes the window's mode: simple shows what recovering files needs, with no cluster numbers; investigate adds the map, bytes, hypotheses and raw tools.", json!({"mode": string("simple or investigate.")}), &["mode"]),
        tool("set_language", "Changes the window's language: en, fr, de or es.", json!({"language": string("en, fr, de or es.")}), &["language"]),
        tool("set_theme", "Changes the window's appearance: system, light or dark.", json!({"theme": string("system, light or dark.")}), &["theme"]),
        tool("get_selection", "What the user currently has selected in the app, and the view they are in.", json!({}), &[]),
        tool("show_in_app", "Moves the user's view to show something. kind: cluster, entry, folder, range ({start, end}) or lens.", json!({"kind": string("What to show."), "value": integer("Cluster, entry id, folder head or lens overlay id."), "start": integer("Range start."), "end": integer("Range end.")}), &["kind"]),
        tool("log_finding", "Posts a conclusion to the app's activity log, so the user can read it later.", json!({"text": string("The finding, and its evidence.")}), &["text"]),
        tool("activity", "Recent tool calls by anyone, newest first.", json!({"afterId": integer("Only items after this id."), "limit": integer("Default 100.")}), &[]),
    ];
    tools.extend(
        crate::rebuild_tools::definitions()
            .into_iter()
            .map(|definition| {
                tool(
                    definition.name,
                    definition.description,
                    definition.properties,
                    definition.required,
                )
            }),
    );
    tools.extend(
        crate::catalog_tools::definitions()
            .into_iter()
            .map(|definition| {
                tool(
                    definition.name,
                    definition.description,
                    definition.properties,
                    definition.required,
                )
            }),
    );
    tools.extend(
        crate::health_tools::definitions()
            .into_iter()
            .map(|definition| {
                tool(
                    definition.name,
                    definition.description,
                    definition.properties,
                    definition.required,
                )
            }),
    );
    tools.extend(
        crate::range_tools::definitions()
            .into_iter()
            .map(|definition| {
                tool(
                    definition.name,
                    definition.description,
                    definition.properties,
                    definition.required,
                )
            }),
    );
    tools
}

pub(crate) fn base64(data: &[u8]) -> String {
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

/// The agent sessions the server knows. A session is one agent connection: what the client opens
/// with `initialize`, and names in the `Mcp-Session-Id` header after. A client that does not send
/// the header back belongs to the latest session, until it has been quiet for a while.
#[derive(Default)]
struct Sessions {
    latest: Option<(String, Instant)>,
    clients: Vec<(String, String)>,
}

static SESSIONS: Mutex<Sessions> = Mutex::new(Sessions {
    latest: None,
    clients: Vec::new(),
});

fn new_session_id() -> String {
    let mut bytes = [0u8; 8];
    let filled = fs::File::open("/dev/urandom").and_then(|mut file| file.read_exact(&mut bytes));
    if filled.is_err() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|time| time.as_nanos())
            .unwrap_or_default();
        bytes = (now as u64).to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn usable_session_id(id: &str) -> bool {
    !id.is_empty()
        && id.chars().count() <= MAX_SESSION_ID_CHARS
        && id
            .chars()
            .all(|found| found.is_ascii_alphanumeric() || found == '-' || found == '_')
}

/// Starts a session for a client that just said hello.
fn start_session(client: Option<&str>) -> String {
    let id = new_session_id();
    if let Ok(mut sessions) = SESSIONS.lock() {
        sessions.latest = Some((id.clone(), Instant::now()));
        if let Some(client) = client {
            if sessions.clients.len() >= MAX_KNOWN_CLIENTS {
                sessions.clients.remove(0);
            }
            sessions
                .clients
                .push((id.clone(), client.chars().take(80).collect()));
        }
    }
    id
}

/// The connection a call belongs to: the session its header names, or the latest one.
fn connection_for(header: Option<&str>) -> AgentConnection {
    let Ok(mut sessions) = SESSIONS.lock() else {
        return AgentConnection::default();
    };
    let session = match header.filter(|id| usable_session_id(id)) {
        Some(id) => id.to_string(),
        None => match &sessions.latest {
            Some((id, last)) if last.elapsed() < SESSION_IDLE => id.clone(),
            _ => {
                let id = new_session_id();
                sessions.latest = Some((id.clone(), Instant::now()));
                id
            }
        },
    };
    if let Some((id, last)) = &mut sessions.latest {
        if *id == session {
            *last = Instant::now();
        }
    }
    let client = sessions
        .clients
        .iter()
        .find(|(id, _)| *id == session)
        .map(|(_, name)| name.clone());
    AgentConnection { session, client }
}

/// Handles one JSON-RPC message. Returns `None` for notifications.
pub fn handle_message(engine: &Engine, message: &Value) -> Option<Value> {
    handle_in_session(engine, message, None).0
}

/// Handles one message for the agent session a header names, or the latest one. Also returns the
/// session id to tell the client, when the message was `initialize` and so began a session.
pub fn handle_in_session(
    engine: &Engine,
    message: &Value,
    session_header: Option<&str>,
) -> (Option<Value>, Option<String>) {
    let started = (message.get("method").and_then(Value::as_str) == Some("initialize")
        && message.get("id").is_some())
    .then(|| {
        start_session(
            message
                .pointer("/params/clientInfo/name")
                .and_then(Value::as_str),
        )
    });
    let connection = match &started {
        Some(id) => connection_for(Some(id)),
        None => connection_for(session_header),
    };
    (handle_with(engine, message, &connection), started)
}

fn handle_with(engine: &Engine, message: &Value, connection: &AgentConnection) -> Option<Value> {
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
            let Some(tool_name) = name.strip_prefix(TOOL_PREFIX) else {
                return Some(rpc_result(
                    id,
                    json!({
                        "content": [{"type": "text", "text": "MCP tool names must start with captainsalvage_."}],
                        "isError": true,
                    }),
                ));
            };
            if tool_name == SCREENSHOT_TOOL {
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
            let result = match engine.call_as_agent(tool_name, &args, AUTHOR_CLAUDE, connection) {
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
    let session = header_value(&request, SESSION_HEADER).map(str::to_string);
    let mut announced = None;
    let reply = match &message {
        Value::Array(batch) => {
            let replies: Vec<Value> = batch
                .iter()
                .filter_map(|item| {
                    let (reply, started) = handle_in_session(engine, item, session.as_deref());
                    announced = announced.take().or(started);
                    reply
                })
                .collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        single => {
            let (reply, started) = handle_in_session(engine, single, session.as_deref());
            announced = started;
            reply
        }
    };
    match reply {
        Some(value) => {
            let content_type = tiny_http::Header::from_bytes("Content-Type", "application/json")
                .expect("valid header");
            let mut response =
                tiny_http::Response::from_string(value.to_string()).with_header(content_type);
            if let Some(id) = announced {
                if let Ok(header) = tiny_http::Header::from_bytes(SESSION_HEADER, id) {
                    response = response.with_header(header);
                }
            }
            let _ = request.respond(response);
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
