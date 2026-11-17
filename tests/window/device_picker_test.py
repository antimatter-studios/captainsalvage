"""The start screen lists volumes compactly and offers one chooser for what to inspect as raw bytes.

The test gives the window a made-up list of disks (SALVAGE_TEST_STARTUP_DISKS), so no real disk is named or opened. Its
"devices" are small synthetic image files in this folder: a whole disk with no partition table,
and a disk with two partitions. Choosing a scope must open exactly those bytes. It saves
picker-start.png in this folder.
"""
import base64
import json
import os
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, js, probe, results, start, state, wait_for

MIB = 1024 * 1024
FOLDER = Path(SCRATCH)


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


def image(name, size):
    path = FOLDER / name
    data = bytearray(size)
    data[:16] = name.encode().ljust(16, b".")[:16]
    path.write_bytes(data)
    return str(path)


def volume(device, whole, name, size, fmt, known=False):
    return {"device": device, "wholeDisk": whole, "name": name, "bytes": size, "format": fmt, "external": True,
            "mountPoint": None, "uuid": None, "canOpen": True, "readable": True, "known": None,
            "fstabLine": None, "protected": False, "mountBlocked": False}


whole = image("picker-whole.img", 5 * MIB)
second = image("picker-second.img", 8 * MIB)
first_part = image("picker-part-a.img", 3 * MIB)
second_part = image("picker-part-b.img", 2 * MIB)
report = {
    "disks": [
        {"device": whole, "name": "UNREADABLE TABLE DRIVE", "bytes": 5 * MIB, "external": True, "scheme": None, "readable": True},
        {"device": second, "name": "STORAGE DEVICE", "bytes": 8 * MIB, "external": True, "scheme": "GUID_partition_scheme", "readable": True},
    ],
    "volumes": [volume(first_part, second, "Holiday", 3 * MIB, "ExFAT"), volume(second_part, second, "Backup", 2 * MIB, "NTFS")],
    "elsewhere": [], "diskSleepMinutes": None,
}

(FOLDER / "picker-report.json").write_text(json.dumps(report))
os.environ["SALVAGE_TEST_STARTUP_DISKS"] = str(FOLDER / "picker-report.json")


def choose(device):
    js("(() => {const select = document.querySelector('.start-raw select');"
       "Object.getOwnPropertyDescriptor(HTMLSelectElement.prototype, 'value').set.call(select, %s);"
       "select.dispatchEvent(new Event('change', {bubbles: true}));})()" % json.dumps(device))


def inspect():
    js("[...document.querySelectorAll('.start-raw button')].find((button) => button.textContent === 'Inspect raw bytes')?.click()")


start(sys.argv[1])
try:
    check("the first look finishes", wait_for("document.querySelector('.start-heading button')?.textContent.trim() === 'Check again'"), True)
    check("each volume is one row", wait_for("document.querySelectorAll('.start-volume').length === 2"), True)
    check("a row says what differs: name, size, format and device", probe("[...document.querySelectorAll('.start-volume-title')].map((row) => row.textContent)"),
          ["Holiday3.1 MB, ExFAT, %s" % first_part, "Backup2.1 MB, NTFS, %s" % second_part])
    check("no row repeats the raw buttons", probe("document.querySelectorAll('.start-volume button:not(.primary)').length"), 0)
    check("raw inspection is offered once", probe("[...document.querySelectorAll('.start-raw button')].length"), 1)
    check("the chooser lists both disks, the whole disk first in each", probe("[...document.querySelectorAll('.start-raw optgroup')].map((group) => group.querySelector('option').textContent)"),
          ["Whole disk, 5.2 MB", "Whole disk, 8.4 MB"])
    check("the disk with no partition is still there to choose", probe("document.querySelectorAll('.start-raw optgroup')[0].querySelectorAll('option').length"), 1)
    height = probe("Math.round(document.querySelector('.start-volumes').getBoundingClientRect().height + document.querySelector('.start-raw').getBoundingClientRect().height)")
    print("volume rows and chooser together: %d px" % height, flush=True)
    screenshot(FOLDER / "picker-start.png")

    choose(whole)
    check("a whole disk with no partition table is chosen", wait_for("document.querySelector('.start-raw-scope')?.textContent.includes('all 5.2 MB of the disk')"), True)
    inspect()
    check("the whole disk opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    check("the map covers the whole disk", probe("[...document.querySelectorAll('.side input')][1].value"), str(5 * MIB))

    js("[...document.querySelectorAll('.toolbar button')].find((button) => button.textContent === 'Change disk')?.click()")
    check("the start screen comes back", wait_for("!!document.querySelector('.start-raw select')"), True)
    choose(second_part)
    check("a partition is chosen", wait_for("document.querySelector('.start-raw-scope')?.textContent.includes('only this partition, 2.1 MB')"), True)
    inspect()
    check("the partition opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    check("the map covers only that partition", probe("[...document.querySelectorAll('.side input')][1].value"), str(2 * MIB))
    js("[...document.querySelectorAll('.mode-switch button')].find((button) => button.textContent.trim() === 'Investigate')?.click()")
    check("the graph's last mark is the partition's end", wait_for("!!document.querySelector('.ruler span:last-child')"), True)
    check("no byte outside it is shown", probe("document.querySelector('.ruler span:last-child').textContent"), probe("document.querySelector('.explorer .row-size').textContent"))
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
