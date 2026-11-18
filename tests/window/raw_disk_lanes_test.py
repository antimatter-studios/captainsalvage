"""The raw map shows partition bands and filesystem lanes, and an inspector explains what is selected.

The source is a synthetic 24 MiB image: an MBR with three back-to-back partitions, an NTFS boot
sector at the start of each, an older exFAT volume inside the third, and sixty more NTFS boot
sectors in a dense run past the end. The map must stay readable, summarise the dense run by format
and count, and show the detail of whatever is picked, on the map or in the list. It saves
raw-lanes.png in this folder. Needs jigsaw.img (see README.md).
"""
import base64
import json
import os
import struct
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, js, probe, results, start, state, wait_for

MIB = 1024 * 1024
FOLDER = Path(SCRATCH)


def click(text, scope=".app"):
    js("[...document.querySelectorAll(%s + ' button')].find((button) => button.textContent.trim() === %s)?.click()" % (json.dumps(scope), json.dumps(text)))


def click_strip(index, fraction):
    """A real pointer click on a strip, at a fraction of its width."""
    js("(() => {Element.prototype.setPointerCapture = () => {}; const strip = document.querySelectorAll('.strips .strip')[%d];"
       "const box = strip.getBoundingClientRect(); const x = box.left + box.width * %f; const y = box.top + box.height / 2;"
       "for (const type of ['pointerdown', 'pointerup']) strip.dispatchEvent(new PointerEvent(type, {bubbles: true, clientX: x, clientY: y, pointerId: 1}));})()" % (index, fraction))


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


def ntfs_boot(volume_bytes):
    boot = bytearray(512)
    boot[3:11] = b"NTFS    "
    struct.pack_into("<H", boot, 11, 512)
    boot[13] = 8
    struct.pack_into("<Q", boot, 40, volume_bytes // 512)
    boot[510:512] = b"\x55\xaa"
    return bytes(boot)


fixture = FOLDER / "jigsaw.img"
if not fixture.exists():
    raise SystemExit("Generate jigsaw.img as described in tests/window/README.md first.")
exfat = bytearray(fixture.read_bytes())
exfat.extend(bytes(max(0, 2 * MIB - len(exfat))))
struct.pack_into("<Q", exfat, 72, len(exfat) // 512)
checksum = 0
for position, value in enumerate(exfat[:11 * 512]):
    if position not in (106, 107, 112):
        checksum = (((checksum << 31) | (checksum >> 1)) + value) & 0xffffffff
exfat[11 * 512:12 * 512] = struct.pack("<I", checksum) * 128
exfat[12 * 512:24 * 512] = exfat[:12 * 512]

image = bytearray(24 * MIB)
starts = [2 * MIB, 8 * MIB, 14 * MIB]
for index, begin in enumerate(starts):
    image[446 + index * 16 + 4] = 0x07
    struct.pack_into("<II", image, 446 + index * 16 + 8, begin // 512, 6 * MIB // 512)
    image[begin:begin + 512] = ntfs_boot(6 * MIB)
image[510:512] = b"\x55\xaa"
image[18 * MIB:18 * MIB + len(exfat)] = exfat
for index in range(60):
    where = 20 * MIB + index * 64 * 1024
    image[where:where + 512] = ntfs_boot(64 * 1024)
source = FOLDER / "raw-lanes.img"
source.write_bytes(image)

start(sys.argv[1])
try:
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(source)))
    check("Open becomes available", wait_for("!document.querySelector('.start-image button[type=submit]').disabled"), True)
    js("document.querySelector('.start-image input').form.requestSubmit()")
    check("the raw source opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    click("Investigate", ".mode-switch")
    check("the map is shown", wait_for("document.querySelectorAll('.strips .strip canvas').length === 4"), True)
    click("Start scan", ".toolbar")
    check("the scan finishes", wait_for("document.querySelector('.scan-text')?.textContent === 'Scan complete'", 180), True)

    rows = probe("document.querySelector('.strip-base').height")
    check("the map has a scan row, three band rows and a lane for each of the two formats", rows, 1 + 3 + 2)
    check("there is no layer to switch", probe("document.querySelectorAll('.strips [role=tab]').length"), 0)
    legend = probe("document.querySelector('.strips .legend').textContent")
    check("dense NTFS findings are summarised by format and count", "NTFS · 63" in legend, True)
    check("exFAT is summarised too", probe("/exFAT · \\d+/.test(document.querySelector('.strips .legend').textContent)"), True)
    check("three declared partitions are in the list", probe("document.querySelectorAll('.explorer .row-path').length > 0 && [...document.querySelectorAll('.explorer .row-name')].filter((name) => name.textContent === 'MBR type 07').length"), 3)

    click_strip(0, 11 / 24)
    check("picking a band shows the partition", wait_for("document.querySelector('.inspected .detail-name')?.textContent === 'MBR type 07'"), True)
    check("it says it is recognised", probe("document.querySelector('.inspected .status').textContent"), "Recognised")
    check("the range is the partition's", probe("/8\\D388\\D608.14\\D680\\D064/.test(document.querySelector('.inspected .facts').textContent)"), True)

    click_strip(0, 2 / 24)
    check("picking a lane shows the finding", wait_for("document.querySelector('.inspected .detail-name')?.textContent.startsWith('NTFS')"), True)
    facts = probe("document.querySelector('.inspected .facts').textContent")
    check("the inspector gives its format, confidence and status", all(word in facts for word in ("Format", "Confidence", "Status", "Decodable")), True)
    screenshot(FOLDER / "raw-lanes.png")
    check("the second graph magnifies the selected range", probe("document.querySelectorAll('.strip-range')[1].textContent"), "2.1 MB to 8.4 MB")
    click_strip(1, 0.5)
    check("clicking the magnified graph shows that place in the main area", wait_for("!!document.querySelector('.hex-sheet')"), True)

    click("Overview", ".center-tabs")
    js("[...document.querySelectorAll('.format-filter button')].find((button) => button.textContent.startsWith('exFAT'))?.click()")
    check("the list can be narrowed to one format", wait_for("[...document.querySelectorAll('.explorer .row-name')].every((name) => !name.textContent.startsWith('NTFS'))"), True)
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
