"""Verify raw scan ETA, durable checkpoints, marker-based clone recognition and the rescan of the
regions a probable match marks as out of date, in an isolated app. Synthetic images only."""
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


MIB = 1024 * 1024


def click(text, scope=".app"):
    js("[...document.querySelectorAll(%s + ' button')].find((button) => button.textContent.trim() === %s)?.click()" % (json.dumps(scope), json.dumps(text)))


def open_raw(path, wait=True):
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(path)))
    if not wait_for("!document.querySelector('.start-image button[type=submit]').disabled"):
        raise RuntimeError("Open never became available")
    js("document.querySelector('.start-image input').form.requestSubmit()")
    if wait and not wait_for("!!document.querySelector('.recovery-sub') && document.querySelector('.recovery-sub').textContent === 'Raw disk investigation'"):
        raise RuntimeError("raw source did not open: " + str(probe("JSON.stringify({sub: document.querySelector('.recovery-sub')?.textContent, saved: !!document.querySelector('.saved-scans'), start: !!document.querySelector('.start'), warn: [...document.querySelectorAll('.warn')].map((n) => n.textContent), interp: document.querySelector('.interp')?.textContent})")))


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
    # A second boot sector in the last mebibyte but one, which a later "mount" overwrites. It is
    # inside the region a changed last block makes stale, and the one at the start is not.
    second = 47 * MIB + MIB // 2
    with source.open("r+b") as stream:
        stream.seek(second)
        stream.write(header)
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
        check("an exact match opens its latest scan at once, with no question", wait_for("document.querySelector('.recovery')?.textContent.includes(%s)" % json.dumps("Scan " + scan_id + ".")), True)
        check("no dialog is in the way", probe("!document.querySelector('.saved-scans')"), True)
        check("saved scans survive app restart and a different path", probe("document.querySelector('.explorer').textContent.includes('NTFS')"), True)
        check("opening restores the committed byte position", probe("document.querySelector('.recovery').textContent.includes(%s)" % json.dumps("Examined through byte %d." % checkpoint)), True)
        check("opening restored the findings without scanning", probe("document.querySelector('.scan-text')?.textContent.startsWith('Scan paused at')"), True)
        click("Saved scans (1)", ".interp")
        check("the saved scans are listed on request", wait_for("!!document.querySelector('.saved-scans')"), True)
        check("the match is shown as a confidence", probe("document.querySelector('.saved-scans').textContent.includes('100% match: 20 match, 0 changed, 0 unreadable')"), True)
        check("it is an exact match", probe("document.querySelector('.saved-scans .saved-state')?.textContent"), "exact")
        check("the sampled limitation is visible", probe("document.querySelector('.saved-scans').textContent.includes('not proof that every byte is identical')"), True)
        check("the scan says it is open now", probe("document.querySelector('.saved-scan-now')?.textContent"), "Open now")
        click("Close", ".saved-scans")
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
        open_raw(clone, wait=False)
        check("a changed block leaves a probable match, which asks before anything opens", wait_for("document.querySelector('.saved-scans')?.textContent.includes('This drive looks like one seen before')"), True)
        check("the confidence is shown", probe("document.querySelector('.saved-scans').textContent.includes('95% match: 19 match, 1 changed, 0 unreadable')"), True)
        check("it is a probable match", probe("document.querySelector('.saved-scans .saved-state')?.textContent"), "probable")
        check("the changed marker is explained", probe("document.querySelector('.saved-scans').textContent.includes('1 of 20 markers changed.')"), True)
        check("nothing was applied: no workspace is behind the question", probe("!document.querySelector('.recovery')"), True)
        check("Open waits for the person to say they read what changed", probe("[...document.querySelectorAll('.saved-scans .saved-scan button')].every((button) => button.disabled)"), True)
        js("document.querySelector('.saved-ack input').click()")
        check("ticking that they read it allows Open", wait_for("[...document.querySelectorAll('.saved-scans .saved-scan button')].every((button) => !button.disabled)"), True)
        click("Open", ".saved-scans .saved-scan")
        check("a probable match opens once acknowledged", wait_for("document.querySelector('.recovery')?.textContent.includes('resumed on a source that had changed')"), True)
        opened = call("open_raw_disk", path=str(clone))
        refused = call("resume_raw_scan", diskId=opened["id"], scanId=scan_id, acceptSampledMatch=True)
        check("backend refuses a probable match without acknowledging the changed markers", "error" in refused, True)
        accepted = call("resume_raw_scan", diskId=opened["id"], scanId=scan_id, acceptSampledMatch=True, acceptChangedMarkers=True)
        check("backend resumes a probable match once the changed markers are acknowledged", len(accepted.get("changedSource", {}).get("changedOffsets", [])), 1)
        click("Change disk", ".toolbar")
        check("return to disk selection again", wait_for("!!document.querySelector('.start-image input')"), True)

        # A mount also rewrote the structure in the last mebibyte but one, inside the region the
        # changed last block makes stale. Reopening marks it, and the person decides to scan it again.
        with clone.open("r+b") as stream:
            stream.seek(second)
            stream.write(random.Random(9).randbytes(512))
        open_raw(clone, wait=False)
        check("the changed marker asks again", wait_for("document.querySelector('.saved-scans')?.textContent.includes('This drive looks like one seen before')"), True)
        check("the question says what opening does about the changed area", probe("document.querySelector('.saved-scans').textContent.includes('you can scan just those again')"), True)
        js("document.querySelector('.saved-ack input').click()")
        check("ticking that they read it allows Open again", wait_for("[...document.querySelectorAll('.saved-scans .saved-scan button')].every((button) => !button.disabled)"), True)
        click("Open", ".saved-scans .saved-scan")
        check("it opens once acknowledged", wait_for("document.querySelector('.recovery')?.textContent.includes('resumed on a source that had changed')"), True)
        check("the window says how many regions may be out of date", wait_for("document.querySelector('.interp-stale')?.textContent.includes('One region may be out of date.')"), True)
        check("it offers to scan them again, and nothing else", probe("[...document.querySelectorAll('.interp-stale button')].map((button) => button.textContent)"), ["Rescan changed regions"])
        with sqlite3.connect(database) as connection:
            regions = connection.execute("SELECT start, end, cleared_at_millis FROM scan_stale_regions").fetchall()
            findings = [row[0] for row in connection.execute("SELECT offset FROM raw_findings ORDER BY offset")]
            next_before = connection.execute("SELECT next_offset FROM raw_scans").fetchone()[0]
            catalog_scan = connection.execute("SELECT id FROM scans WHERE kind = 'raw'").fetchone()[0]
        # The changed last block makes its own 4 KiB and one mebibyte before it stale.
        check("one stale region around the changed marker, not cleared", regions, [(48 * MIB - 4096 - MIB, 48 * MIB, None)])
        check("both findings are still stored: opening scanned nothing", findings, [0, second])
        check("only the finding in the stale region is marked", probe("[...document.querySelectorAll('.explorer .row-name')].map((name) => !!name.querySelector('.stale-mark'))"), [offset == second for offset in findings])
        check("the mark says what it means", probe("document.querySelector('.explorer .stale-mark')?.textContent"), "May be out of date")
        check("an agent can list the region, read only", call("list_stale_regions", scanId=catalog_scan)["stale"]["stale"], 1)
        click("Rescan changed regions", ".interp-stale")
        check("the rescan finishes and says the markers were recorded again", wait_for("document.querySelector('.interp-stale')?.textContent.includes('recorded again')", 120), True)
        check("no finding is marked now", probe("document.querySelectorAll('.explorer .stale-mark').length"), 0)
        check("the button and the count are gone", probe("document.querySelectorAll('.interp-stale button').length"), 0)
        check("the scan no longer says it was resumed on a changed source", probe("!document.querySelector('.recovery').textContent.includes('resumed on a source that had changed')"), True)
        with sqlite3.connect(database) as connection:
            regions = connection.execute("SELECT cleared_at_millis IS NOT NULL, done_to = end FROM scan_stale_regions").fetchall()
            findings = [row[0] for row in connection.execute("SELECT offset FROM raw_findings ORDER BY offset")]
            history = connection.execute("SELECT count(*) FROM source_marker_history").fetchone()[0]
            next_after = connection.execute("SELECT next_offset FROM raw_scans").fetchone()[0]
        check("the region is cleared and was read to its end", regions, [(1, 1)])
        check("the structure the mount overwrote is gone, and the one at the start stays", findings, [0])
        check("the window lists what the rescan found", probe("document.querySelectorAll('.explorer .row-name').length"), len(findings))
        check("the scan's own position did not move", next_after, next_before)
        check("the markers the source had before are kept", history, 1)
        click("Change disk", ".toolbar")
        check("return to disk selection after the rescan", wait_for("!!document.querySelector('.start-image input')"), True)
        open_raw(clone)
        check("the same drive is an exact match again: it opens at once, with no question", wait_for("document.querySelector('.recovery')?.textContent.includes(%s)" % json.dumps("Scan " + scan_id + ".")), True)
        check("no dialog is in the way", probe("!document.querySelector('.saved-scans')"), True)
        check("nothing is marked, and there is nothing to rescan", probe("!document.querySelector('.interp-stale') && document.querySelectorAll('.stale-mark').length === 0"), True)
        click("Change disk", ".toolbar")
        check("return to disk selection once more", wait_for("!!document.querySelector('.start-image input')"), True)

        # Everything but the first block rewritten: a different drive, which is never offered.
        with clone.open("r+b") as stream:
            stream.seek(4096)
            stream.write(random.Random(8).randbytes(48 * 1024 * 1024 - 4096))
        open_raw(clone)
        check("a different drive is not offered, so it opens as a new source", wait_for("document.querySelector('.interp-scans')?.textContent === 'Saved scans (0)'"), True)
        click("Saved scans (0)", ".interp")
        check("the saved scans open", wait_for("!!document.querySelector('.saved-show-all input')"), True)
        js("document.querySelector('.saved-show-all input').click()")
        check("only the new source is listed, with nothing stored", wait_for("document.querySelectorAll('.saved-source').length === 1 && document.querySelector('.saved-scans').textContent.includes('No scans are stored for it.')"), True)
        check("the other drive's scan is not offered, even with Show all", probe("document.querySelectorAll('.saved-scan').length"), 0)
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
