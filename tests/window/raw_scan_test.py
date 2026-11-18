"""Verify raw scan ETA, durable checkpoints and marker-based clone recognition in an isolated app."""
import hashlib
import json
import os
import random
import shutil
import sqlite3
import struct
import sys
import tempfile
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, results, start, state, wait_for


def click(text, scope=".app"):
    js("[...document.querySelectorAll(%s + ' button')].find((button) => button.textContent.trim() === %s)?.click()" % (json.dumps(scope), json.dumps(text)))


def open_raw(path):
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(path)))
    if not wait_for("!document.querySelector('.raw-entry-actions button').disabled"):
        raise RuntimeError("the raw action never became available")
    js("document.querySelector('.start-image input').form.requestSubmit()")
    if not wait_for("!!document.querySelector('.recovery-sub') && document.querySelector('.recovery-sub').textContent === 'Raw disk investigation'"):
        raise RuntimeError("raw source did not open")


with tempfile.TemporaryDirectory(dir=SCRATCH, prefix="fakehome.rawscan.") as home:
    previous_home = os.environ.get("SALVAGE_TEST_HOME")
    os.environ["SALVAGE_TEST_HOME"] = home
    source = Path(home) / "original.img"
    header = bytearray(512)
    header[3:11] = b"NTFS    "
    struct.pack_into("<H", header, 11, 512)
    header[13] = 8
    struct.pack_into("<Q", header, 40, 48 * 1024 * 1024 // 512)
    header[510:512] = b"\x55\xaa"
    # Every block holds bytes that tell this drive from another (blank blocks are not markers),
    # and none repeats a short pattern. Block 0 carries the NTFS boot sector the scan finds.
    block = random.Random(7).randbytes(4096)
    with source.open("wb") as stream:
        stream.write(header + block[512:])
        for _ in range(48 * 1024 * 1024 // 4096 - 1):
            stream.write(block)
    original_hash = hashlib.sha256(source.read_bytes()).hexdigest()
    database = Path(home) / "Library/Application Support/Captain Salvage/raw-scans.sqlite"
    try:
        start(sys.argv[1])
        open_raw(source)
        js("(() => {const callbacks=window.__TAURI_INTERNALS__.callbacks;"
           "const get=callbacks.get.bind(callbacks); let batches=0;"
           "const delayed=function(identifier){const callback=get(identifier);"
           "return callback && function(data){"
           "if(data && data.scanId && typeof data.nextOffset==='number' && !('scanStart' in data)){"
           "batches++; const deliver=()=>callback(data);"
           "if(batches===2) window.__rawNextBatch=deliver; else setTimeout(deliver,200);"
           "}else callback(data);};};"
           "callbacks.get=delayed; window.__rawTimingInstalled=callbacks.get===delayed;})()")
        if probe("window.__rawTimingInstalled===true") is not True:
            raise RuntimeError("the deterministic batch-response gate was not installed")
        click("Start scan", ".toolbar")
        check("raw toolbar shows speed and ETA before completion", wait_for("document.querySelector('.scan-text')?.textContent.includes('/s,') && document.querySelector('.scan-text').textContent.includes('left')"), True)
        check("ETA never displays invalid numbers", probe("!/NaN|Infinity/.test(document.querySelector('.scan-text').textContent)"), True)
        click("Pause scan", ".toolbar")
        js("window.__rawNextBatch?.()")
        check("pause finishes and saves the in-flight batch", wait_for("document.querySelector('.scan-text')?.textContent.startsWith('Scan paused at')"), True)
        with sqlite3.connect(database) as connection:
            scan_id, checkpoint, scan_end, evidence_count = connection.execute(
                "SELECT id,next_offset,scan_end,(SELECT count(*) FROM raw_findings WHERE scan_id=raw_scans.id) FROM raw_scans"
            ).fetchone()
        check("a partial checkpoint is durable", 0 < checkpoint < scan_end, True)
        check("pause displays the committed in-flight checkpoint", probe("document.querySelector('.recovery').textContent.includes(%s)" % json.dumps("Examined through byte %d." % checkpoint)), True)
        check("findings were committed with the checkpoint", evidence_count > 0, True)
        check("permanent scan ID is visible", probe("document.querySelector('.recovery').textContent.includes(%s)" % json.dumps(scan_id)), True)
        finish()
        Path(state["cfg"]).rename(Path(home) / "previous-mcp.json")
        clone = Path(home) / "renamed-clone.img"
        shutil.copyfile(source, clone)
        start(sys.argv[1])
        open_raw(clone)
        check("saved scans survive app restart and a different path", wait_for("!![...document.querySelectorAll('.recovery button')].find(button=>button.textContent==='Load saved scan' && !button.disabled)"), True)
        check("the match is shown as a confidence", probe("document.querySelector('.recovery').textContent.includes('100% match: 20 match, 0 changed, 0 unreadable')"), True)
        check("the sampled limitation is visible", probe("document.querySelector('.recovery').textContent.includes('not proof that every byte is identical')"), True)
        check("an exact match loads without a second question", probe("!document.querySelector('.recovery [role=alertdialog]')"), True)
        click("Load saved scan", ".recovery")
        check("loading restores the same stable scan ID", wait_for("document.querySelector('.recovery').textContent.includes(%s)" % json.dumps("Scan " + scan_id + ".")), True)
        check("loading restores findings without rescanning", probe("document.querySelector('.explorer').textContent.includes('NTFS')"), True)
        check("loading restores the committed byte position", probe("document.querySelector('.recovery').textContent.includes(%s)" % json.dumps("Examined through byte %d." % checkpoint)), True)
        click("Resume scan", ".toolbar")
        check("resumed clone reaches completion", wait_for("document.querySelector('.scan-text')?.textContent==='Scan complete'", 120), True)
        with sqlite3.connect(database) as connection:
            saved_id, saved_next, count = connection.execute("SELECT id,next_offset,(SELECT count(*) FROM raw_scans) FROM raw_scans").fetchone()
        check("resuming does not create another scan", (saved_id, saved_next, count), (scan_id, scan_end, 1))
        click("Change disk", ".toolbar")
        check("return to disk selection", wait_for("!!document.querySelector('.start-image input')"), True)
        # A mount rewrote the last block: one marker of twenty changed, so the drive is probable.
        with clone.open("r+b") as stream:
            stream.seek(-1, 2)
            stream.write(b"\x01")
        open_raw(clone)
        check("a changed block leaves a probable match with its confidence", wait_for("document.querySelector('.recovery').textContent.includes('95% match: 19 match, 1 changed, 0 unreadable')"), True)
        check("the changed marker is explained", probe("document.querySelector('.recovery').textContent.includes('1 of 20 markers changed.')"), True)
        click("Load saved scan", ".recovery")
        check("the window asks before loading a probable match", wait_for("!!document.querySelector('.recovery [role=alertdialog]')"), True)
        click("Load anyway", ".recovery")
        check("a probable match loads once acknowledged", wait_for("document.querySelector('.recovery').textContent.includes('resumed on a source that had changed')"), True)
        opened = call("open_raw_disk", path=str(clone))
        refused = call("resume_raw_scan", diskId=opened["id"], scanId=scan_id, acceptSampledMatch=True)
        check("backend refuses a probable match without acknowledging the changed markers", "error" in refused, True)
        accepted = call("resume_raw_scan", diskId=opened["id"], scanId=scan_id, acceptSampledMatch=True, acceptChangedMarkers=True)
        check("backend resumes a probable match once the changed markers are acknowledged", len(accepted.get("changedSource", {}).get("changedOffsets", [])), 1)
        click("Change disk", ".toolbar")
        check("return to disk selection again", wait_for("!!document.querySelector('.start-image input')"), True)
        # Everything but the first block rewritten: a different drive, which is never offered.
        with clone.open("r+b") as stream:
            stream.seek(4096)
            stream.write(random.Random(8).randbytes(48 * 1024 * 1024 - 4096))
        open_raw(clone)
        check("a different drive is not offered, even with Show all", wait_for("!document.querySelector('.recovery').textContent.includes('Saved scans')"), True)
        opened = call("open_raw_disk", path=str(clone))
        refused = call("resume_raw_scan", diskId=opened["id"], scanId=scan_id, acceptSampledMatch=True, acceptChangedMarkers=True)
        check("backend independently refuses a different source", "error" in refused, True)
        check("the original source was never modified", hashlib.sha256(source.read_bytes()).hexdigest(), original_hash)
    finally:
        finish()
        if previous_home is None:
            os.environ.pop("SALVAGE_TEST_HOME", None)
        else:
            os.environ["SALVAGE_TEST_HOME"] = previous_home

if not all(passed for _, passed in results):
    raise SystemExit(1)
