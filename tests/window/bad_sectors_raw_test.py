"""Bad sectors on the raw-disk map: red marks that flash, a legend entry, a tooltip, and amber for slow.

The source is a made-up 24 MiB disk with an MBR and one NTFS boot sector, and a made-up health map written
into the test copy's catalog before it opens: a place the drive reset during a read, a failed read, a band
a probe jumped over, and a slow range. Nothing is read from a real disk. It saves bad-sectors-raw.png in
this folder, of the made-up disk. Needs nothing else.
"""
import json
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bad_sectors_support import LEGEND, seed
from harness import SCRATCH, check, finish, js, probe, results, start, wait_for
from screenshot_helper import screenshot

MIB = 1024 * 1024
FOLDER = Path(SCRATCH)

image = bytearray(24 * MIB)
image[446 + 4] = 0x07
struct.pack_into("<II", image, 446 + 8, 2 * MIB // 512, 20 * MIB // 512)
image[510:512] = b"\x55\xaa"
boot = bytearray(512)
boot[3:11] = b"NTFS    "
struct.pack_into("<H", boot, 11, 512)
boot[13] = 8
struct.pack_into("<Q", boot, 40, 20 * MIB // 512)
boot[510:512] = b"\x55\xaa"
image[2 * MIB:2 * MIB + 512] = boot
source = FOLDER / "bad-sectors-raw.img"
source.write_bytes(image)

start(sys.argv[1])
try:
    seed(source, [
        (3 * MIB, 3 * MIB + 512 * 1024, "slow", 600),
        (10 * MIB, 10 * MIB + 64 * 1024, "reset-culprit", 90),
        (14 * MIB, 14 * MIB + 4096, "unreadable", 7200),
        (17 * MIB, 19 * MIB, "suspect", 86400),
    ])
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(source)))
    check("Open becomes available", wait_for("!document.querySelector('.start-image button[type=submit]').disabled"), True)
    js("document.querySelector('.start-image input').form.requestSubmit()")
    check("the raw source opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    js("[...document.querySelectorAll('.mode-switch button')].find((b) => b.textContent.trim() === 'Investigate')?.click()")
    check("the map is shown", wait_for("document.querySelectorAll('.strips .strip canvas').length === 4"), True)
    check("the bad sectors are drawn on the map", wait_for("document.querySelectorAll('.strips .strip .bad-range').length >= 3"), True)

    marks = probe("document.querySelectorAll('.strips .strip')[0].querySelectorAll('.bad-range').length")
    check("the first strip has one mark for each bad range: reset, failed and the suspect band, not the slow one", marks, 3)
    style = probe("(() => {const mark = document.querySelector('.strips .bad-range'); const style = getComputedStyle(mark); return [style.animationName, style.animationDuration, style.animationIterationCount, style.backgroundColor];})()")
    check("a mark flashes: a pulse of one second, for ever", style[:3], ["bad-pulse", "1s", "infinite"])
    check("a mark is red", style[3], "rgb(229, 32, 43)")
    check("the marks do not take the pointer, so the map still picks", probe("getComputedStyle(document.querySelector('.strips .strip-bad')).pointerEvents"), "none")

    legend = probe("document.querySelector('.strips .legend').textContent")
    check("the legend has an entry for bad sectors", LEGEND in legend, True)
    check("the legend names slow reads on their own", "Slow to read" in legend, True)
    check("the legend's swatch flashes too", probe("getComputedStyle(document.querySelector('.strips .legend .bad-swatch')).animationName"), "bad-pulse")
    check("slow reads are amber and do not flash: nothing but the bad sectors animates in the map", probe("[...document.querySelectorAll('.strips *')].filter((element) => getComputedStyle(element).animationName === 'bad-pulse' && !element.classList.contains('bad-range') && !element.classList.contains('bad-swatch')).length"), 0)

    # The pointer on the mark of the place the drive reset during a read.
    js("(() => {const strip = document.querySelectorAll('.strips .strip')[0]; const box = strip.getBoundingClientRect();"
       "const x = box.left + box.width * (10.02 / 24); const y = box.top + box.height / 2;"
       "strip.dispatchEvent(new PointerEvent('pointermove', {bubbles: true, clientX: x, clientY: y, pointerId: 1}));})()")
    check("the tooltip names the bad sectors", wait_for("document.querySelector('.strip-tip')?.textContent.includes(%s)" % json.dumps(LEGEND)), True)
    tip = probe("document.querySelector('.strip-tip').textContent")
    check("it says where", "MB to" in tip, True)
    check("it says how it was found", "The drive reset during a read here." in tip, True)
    check("it says when", "Found" in tip, True)

    # A failed read, and the band.
    for fraction, sentence in ((14.0001 / 24, "A read here failed."), (18 / 24, "Inside a band the probe jumped over, so it was not read.")):
        js("(() => {const strip = document.querySelectorAll('.strips .strip')[0]; const box = strip.getBoundingClientRect();"
           "const x = box.left + box.width * %f; const y = box.top + box.height / 2;"
           "strip.dispatchEvent(new PointerEvent('pointermove', {bubbles: true, clientX: x, clientY: y, pointerId: 1}));})()" % fraction)
        check("the tooltip says: " + sentence, wait_for("document.querySelector('.strip-tip')?.textContent.includes(%s)" % json.dumps(sentence)), True)
    js("(() => {const strip = document.querySelectorAll('.strips .strip')[0]; const box = strip.getBoundingClientRect();"
       "const x = box.left + box.width * (5 / 24); const y = box.top + box.height / 2;"
       "strip.dispatchEvent(new PointerEvent('pointermove', {bubbles: true, clientX: x, clientY: y, pointerId: 1}));})()")
    check("away from them the tooltip does not name bad sectors", wait_for("!document.querySelector('.strip-tip')?.textContent.includes(%s)" % json.dumps(LEGEND)), True)
    screenshot(FOLDER / "bad-sectors-raw.png")
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
