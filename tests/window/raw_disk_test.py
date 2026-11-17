"""Raw sources use the normal workspace and shared graph before a filesystem is opened."""
import base64
import hashlib
import json
import os
import struct
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, js, probe, results, start, state, wait_for


def click(text, scope=".app"):
    js("[...document.querySelectorAll(%s + ' button')].find((button) => button.textContent.trim() === %s)?.click()" % (json.dumps(scope), json.dumps(text)))


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    if result.get("isError"):
        raise RuntimeError(result)
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


fixture = Path(SCRATCH) / "jigsaw.img"
if not fixture.exists():
    raise SystemExit("Generate jigsaw.img as described in tests/window/README.md first.")
offset = 1024 * 1024
payload = bytearray(fixture.read_bytes())
payload.extend(bytes(max(0, 2 * 1024 * 1024 - len(payload))))
struct.pack_into("<Q", payload, 72, len(payload) // 512)
checksum = 0
for position, value in enumerate(payload[:11 * 512]):
    if position not in (106, 107, 112):
        checksum = (((checksum << 31) | (checksum >> 1)) + value) & 0xffffffff
payload[11 * 512:12 * 512] = struct.pack("<I", checksum) * 128
payload[12 * 512:24 * 512] = payload[:12 * 512]
image = bytearray(offset + len(payload))
image[offset:] = payload
image[3:11] = b"NTFS    "
struct.pack_into("<H", image, 11, 512)
image[13] = 8
struct.pack_into("<Q", image, 40, len(image) // 512)
image[510:512] = b"\x55\xaa"
image[450] = 0x07
struct.pack_into("<II", image, 454, 1, (offset - 512) // 512)
source = Path(SCRATCH) / "raw-workspace.img"
source.write_bytes(image)
before = hashlib.sha256(image).hexdigest()

start(sys.argv[1])
try:
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(source)))
    check("raw action becomes available", wait_for("!document.querySelector('.raw-entry-actions button').disabled"), True)
    js("document.querySelector('.start-image input').form.requestSubmit()")
    check("normal Open accepts an unrecognised filesystem", wait_for("!!document.querySelector('.source-strips')"), True)
    check("raw source opens the app workspace", wait_for("!!document.querySelector('.app .explorer')"), True)
    check("the standalone raw screen is gone", probe("!!document.querySelector('.raw-disk-view')"), False)
    check("the normal layout is present", probe("['.sidebar-brand','.toolbar','.explorer','.center-tabs','.recovery','.side .details'].every((selector)=>!!document.querySelector('.app '+selector))"), True)
    check("branding and version remain visible", probe("!!document.querySelector('.sidebar-brand .brand-version')"), True)
    check("both column resize handles remain", probe("document.querySelectorAll('.column-handle').length"), 2)
    check("declared partition is selectable", probe("[...document.querySelectorAll('.explorer .row')].length"), 2)
    check("unknown source does not claim files", probe("[...document.querySelectorAll('.center-tabs button')].find((button)=>button.textContent==='Browse files').disabled"), True)
    click("Investigate", ".mode-switch")
    check("Investigate shows the existing graph", wait_for("document.querySelectorAll('.strips .strip canvas').length === 4"), True)
    check("graph exposes byte-based layers", probe("[...document.querySelectorAll('.strips [role=tab]')].map((button)=>button.textContent)"), ["Source bytes", "Declared partitions", "Filesystem evidence"])
    click("Start scan", ".toolbar")
    check("the whole source can be scanned", wait_for("document.querySelector('.scan-text')?.textContent === 'Scan complete'", 120), True)
    check("NTFS evidence was found", probe("document.querySelector('.explorer').textContent.includes('NTFS')"), True)
    check("exFAT beyond declared partition was found", probe("document.querySelector('.explorer').textContent.includes('exFAT')"), True)
    check("graph painted without a filesystem session", probe("[...document.querySelectorAll('.strip-base')].every((canvas)=>canvas.width>0 && canvas.height===1)"), True)
    check("normal toolbar stays after discovery", probe("document.querySelectorAll('.app .toolbar').length"), 1)
    screenshot("/tmp/captainsalvage-raw-workspace.png")
    click("Bytes", ".center-tabs")
    check("raw bytes use the existing hex layout", wait_for("document.querySelectorAll('.hex-sheet .hex-row').length === 32"), True)
    check("NTFS bytes are visible without its recovery driver", probe("document.querySelector('.hex-sheet').textContent.includes('NTFS')"), True)
    js("[...document.querySelectorAll('.explorer .row')].find((button)=>button.textContent.includes('exFAT'))?.click()")
    check("evidence selects an interpretation", wait_for("document.querySelector('.side select')?.value === 'exFAT' && Number(document.querySelector('.side input').value) === %d" % offset), True)
    click("Try this interpretation", ".side")
    check("interpretation opens the normal recovery workspace", wait_for("!!document.querySelector('.safety-chip')", 120), True)
    check("the shared graph still works for filesystem clusters", wait_for("[...document.querySelectorAll('.strip-base')].some((canvas)=>canvas.height>1)", 30), True)
    check("normal files tab becomes available", probe("![...document.querySelectorAll('.center-tabs button')].find((button)=>button.textContent==='Browse files').disabled"), True)
    click("Source bytes", ".center-tabs")
    check("returning preserves the same workspace", wait_for("!!document.querySelector('.source-strips')"), True)
    check("returning preserves discovery evidence", probe("document.querySelector('.explorer').textContent.includes('NTFS') && document.querySelector('.explorer').textContent.includes('exFAT')"), True)
    click("Simple", ".mode-switch")
    check("Simple mode uses the normal map visibility rule", wait_for("!document.querySelector('.strips')"), True)
    check("source bytes were never modified", hashlib.sha256(source.read_bytes()).hexdigest(), before)
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
