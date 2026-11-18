"""One door: Open takes a whole disk, a partition and an image the same way.

Everything opens from the raw source with an interpretation applied. This opens a synthetic whole
disk (an MBR, one exFAT partition from 1 MiB) and checks that:

- a whole disk opens as raw bytes with its extents to open, and no filesystem picker;
- Open on its partition opens that extent as its own source, with the filesystem applied and the
  interpretation picker in the header (exFAT, FAT, NTFS, no filesystem), the detected one marked;
- changing the picker keeps the earlier scan, says so, and choosing it again scans nothing;
- a choice with no evidence behind it is refused with a reason, and the picker stays where it was;
- the saved scans list names its scan and lets the person name it;
- the same partition opened alone, from a file holding only its bytes, is the same source and the
  same scan, with the results already there.

Synthetic images only. It needs jigsaw.img (see README.md) and saves door-whole-disk.png, door-header.png and
door-saved-scans.png here.
"""
import base64
import hashlib
import json
import os
import random
import sqlite3
import struct
import sys
import time
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, results, start, state, wait_for

MIB = 1024 * 1024


def click(text, scope=".app"):
    js("[...document.querySelectorAll(%s + ' button')].find((button) => button.textContent.trim() === %s)?.click()" % (json.dumps(scope), json.dumps(text)))


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


def open_path(path):
    js("(() => {const i=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%s);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));setTimeout(()=>i.form.requestSubmit(),300);})()" % json.dumps(str(path)))


def pick(value):
    js("(() => {const select = document.querySelector('.interp select');"
       "Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(select, %s);"
       "select.dispatchEvent(new Event('change', {bubbles: true}));})()" % json.dumps(value))


def database():
    return sqlite3.connect(Path(state["home"]) / "Library/Application Support/Captain Salvage/raw-scans.sqlite")


fixture = Path(SCRATCH) / "jigsaw.img"
if not fixture.exists():
    raise SystemExit("Generate jigsaw.img as described in tests/window/README.md first.")
rng = random.Random(21)
# The jigsaw's boot region is made to describe a volume of its own size, as raw_disk_test does.
payload = bytearray(fixture.read_bytes())
payload.extend(bytes(max(0, 2 * MIB - len(payload))))
struct.pack_into("<Q", payload, 72, len(payload) // 512)
checksum = 0
for position, value in enumerate(payload[:11 * 512]):
    if position not in (106, 107, 112):
        checksum = (((checksum << 31) | (checksum >> 1)) + value) & 0xffffffff
payload[11 * 512:12 * 512] = struct.pack("<I", checksum) * 128
payload[12 * 512:24 * 512] = payload[:12 * 512]
volume = bytes(payload)
partition = volume + rng.randbytes(2 * MIB)
disk = bytearray(rng.randbytes(MIB)) + partition + rng.randbytes(64 * 1024)
disk[:512] = bytes(512)
disk[446:462] = bytes(16)
disk[446 + 4] = 0x07
disk[446 + 8:446 + 12] = (MIB // 512).to_bytes(4, "little")
disk[446 + 12:446 + 16] = (len(partition) // 512).to_bytes(4, "little")
disk[510:512] = b"\x55\xaa"
whole = Path(SCRATCH) / "door-whole.img"
alone = Path(SCRATCH) / "door-partition.img"
whole.write_bytes(disk)
alone.write_bytes(partition)
before = hashlib.sha256(bytes(disk)).hexdigest()

start(sys.argv[1])
try:
    open_path(whole)
    check("a whole disk opens", wait_for("!!document.querySelector('.source-strips')", 90), True)
    check("it is raw bytes, and says its extents are what to open", probe("document.querySelector('.interp-whole')?.textContent"), "Whole disk: raw bytes. Open one of its extents to read it as a filesystem.")
    check("a whole disk has no filesystem picker", probe("!!document.querySelector('.interp select')"), False)
    check("its partition has an Open of its own", probe("[...document.querySelectorAll('.row-with-open .row-open')].map((button) => button.textContent)"), ["Open"])
    check("nothing is stored for it yet", probe("document.querySelector('.interp-scans').textContent"), "Saved scans (0)")
    screenshot(Path(SCRATCH) / "door-whole-disk.png")

    js("document.querySelector('.row-with-open .row-open').click()")
    check("Open on the partition opens its filesystem", wait_for("!!document.querySelector('.safety-chip')", 90), True)
    check("the picker is in the header, exFAT applied", probe("document.querySelector('.interp select')?.value"), "exFAT")
    check("it offers exFAT, FAT, NTFS and raw bytes", probe("[...document.querySelectorAll('.interp select option')].map((option) => option.value)"), ["exFAT", "FAT", "NTFS", "none"])
    check("the detected filesystem is marked with its confidence", probe("/^exFAT \\(detected, \\d+%\\)$/.test(document.querySelector('.interp select option').textContent)"), True)
    check("raw bytes read as no filesystem", probe("document.querySelector('.interp select option[value=none]').textContent"), "No filesystem (raw bytes)")
    check("the scan is stored and listed", probe("document.querySelector('.interp-scans').textContent"), "Saved scans (1)")
    check("opening started no scan", call("status")["scan"]["status"] != "complete" and not call("status")["scanInThisApp"], True)
    screenshot(Path(SCRATCH) / "door-header.png")

    call("start_scan")
    check("the scan runs when it is asked for", wait_for("document.querySelector('.scan-text')?.textContent === 'Scan complete'", 180), True)

    pick("none")
    check("choosing no filesystem shows the raw bytes", wait_for("!!document.querySelector('.source-strips') && !document.querySelector('.safety-chip')", 60), True)
    check("it says the earlier scans stay", probe("document.querySelector('.interp-note')?.textContent"), "Showing the raw bytes. Your earlier scans stay.")
    pick("exFAT")
    check("choosing exFAT again opens the filesystem", wait_for("!!document.querySelector('.safety-chip')", 60), True)
    check("it opens the earlier scan and says nothing was scanned", probe("document.querySelector('.interp-note')?.textContent"), "Opened your earlier exFAT scan. Nothing was scanned.")
    check("its results are there, scanned once", call("status")["scan"]["status"], "complete")

    pick("NTFS")
    check("a choice with no evidence is refused, with a reason", wait_for("document.querySelector('.interp-note.warn')?.textContent.startsWith('No NTFS boot evidence was found')", 30), True)
    check("the picker stays on what is open", probe("document.querySelector('.interp select').value"), "exFAT")

    click("Saved scans (1)", ".interp")
    check("the saved scans open", wait_for("!!document.querySelector('.saved-scans')"), True)
    check("the scan is listed with what it reads, when, and how much", probe("document.querySelector('.saved-scan-facts').textContent.startsWith('exFAT, ')"), True)
    check("it shows the size it stores", probe("/Stores \\d/.test(document.querySelector('.saved-scan-facts').textContent)"), True)
    check("it is the one open now", probe("document.querySelector('.saved-scan-now')?.textContent"), "Open now")
    screenshot(Path(SCRATCH) / "door-saved-scans.png")
    js("(() => {const input = document.querySelector('.saved-scan-label'); input.focus();"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype, 'value').set.call(input, 'Holiday disk, exFAT');"
       "input.dispatchEvent(new Event('input', {bubbles: true})); setTimeout(() => input.blur(), 400);})()")
    named = []
    for _ in range(20):
        time.sleep(0.5)
        with database() as connection:
            named = connection.execute("SELECT label FROM scans WHERE kind = 'filesystem'").fetchall()
        if named != [("",)]:
            break
    check("the name is stored with the scan", named, [("Holiday disk, exFAT",)])
    click("Close", ".saved-scans")

    click("Change disk", ".toolbar")
    check("the start screen comes back", wait_for("!!document.querySelector('.start-image input')"), True)
    open_path(alone)
    check("the partition opened alone opens its filesystem", wait_for("!!document.querySelector('.safety-chip')", 90), True)
    check("it is the same scan, with its results already there", probe("document.querySelector('.scan-text')?.textContent"), "Scan complete")
    check("and it is still the only one", probe("document.querySelector('.interp-scans').textContent"), "Saved scans (1)")
    with database() as connection:
        scans = connection.execute("SELECT count(*) FROM scans WHERE kind = 'filesystem'").fetchone()[0]
        links = connection.execute("SELECT count(*) FROM source_extents").fetchone()[0]
        sources = connection.execute("SELECT count(*) FROM sources").fetchone()[0]
    check("one scan, one link between the disk and its extent", (scans, links, sources), (1, 1, 2))
    check("the source image was never modified", hashlib.sha256(whole.read_bytes()).hexdigest(), before)
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
