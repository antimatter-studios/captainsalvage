"""Bad sectors on the volume map and in a preview: red marks that flash over the clusters that are bad.

The source is the made-up exFAT volume jigsaw.img (see README.md), with a made-up health map written into the
test copy's catalog before it opens: four clusters the drive reset during a read on. Nothing is read from a
real disk. It saves bad-sectors-volume.png in this folder. Needs jigsaw.img.
"""
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bad_sectors_support import LEGEND, seed
from harness import SCRATCH, check, finish, investigate, js, open_image, probe, results, start, wait_for
from screenshot_helper import screenshot

FOLDER = Path(SCRATCH)
fixture = FOLDER / "jigsaw.img"
if not fixture.exists():
    raise SystemExit("Generate jigsaw.img as described in tests/window/README.md first.")
boot = fixture.read_bytes()[:512]
heap = struct.unpack_from("<I", boot, 88)[0] * (1 << boot[108])
size = (1 << boot[108]) << boot[109]
source = FOLDER / "bad-sectors-volume.img"
source.write_bytes(fixture.read_bytes())

start(sys.argv[1])
try:
    # Clusters 20 to 23 (the map numbers them from 2).
    seed(source, [(heap + 18 * size, heap + 22 * size, "reset-culprit", 300)])
    open_image(str(source))
    investigate()
    check("the volume map has the bad clusters drawn on both strips", wait_for("document.querySelectorAll('.strips .strip .bad-range').length >= 2"), True)
    check("one mark on each of the two strips", probe("[...document.querySelectorAll('.strips .strip')].map((strip) => strip.querySelectorAll('.bad-range').length)"), [1, 1])
    check("a mark flashes once a second, for ever", probe("(() => {const style = getComputedStyle(document.querySelector('.strips .bad-range')); return [style.animationName, style.animationDuration, style.animationIterationCount];})()"), ["bad-pulse", "1s", "infinite"])
    check("the volume map's legend names bad sectors", LEGEND in probe("document.querySelector('.strips .legend').textContent"), True)
    # The pointer on the whole-volume strip, where the bad clusters are.
    js("(() => {const strip = document.querySelectorAll('.strips .strip')[0]; const box = strip.getBoundingClientRect();"
       "const bad = strip.querySelector('.bad-range').getBoundingClientRect();"
       "const x = bad.left + bad.width / 2; const y = box.top + box.height / 2;"
       "strip.dispatchEvent(new PointerEvent('pointermove', {bubbles: true, clientX: x, clientY: y, pointerId: 1}));})()")
    check("the tooltip names the clusters", wait_for("/Clusters 2\\d to 2\\d/.test(document.querySelector('.strip-tip')?.textContent || '')"), True)
    check("it says how it was found", "The drive reset during a read here." in probe("document.querySelector('.strip-tip').textContent"), True)
    screenshot(FOLDER / "bad-sectors-volume.png")
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
