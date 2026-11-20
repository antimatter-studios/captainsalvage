"""A long error appears on a line of its own under the source header, the whole width of it, and is readable.

The source is a made-up 24 MiB disk (a partition table and one NTFS boot sector). The test copy's device is made to
act as if it was unplugged, and the scan is started: the engine's error, "The disk has been disconnected; scan not started.", used to appear in the narrow column of the progress bar, a word to a line, running off the top of the
window. Then the text is made much longer to see that the banner wraps and keeps inside the window. It saves
header-error.png of the made-up disk. Needs nothing else.
"""
import json
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, results, start, wait_for
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
source = FOLDER / "header-error.img"
source.write_bytes(image)

GEOMETRY = """(() => {
  const header = document.querySelector('.toolbar').getBoundingClientRect();
  const banner = document.querySelector('.toolbar-error');
  const box = banner.getBoundingClientRect();
  return {headerLeft: header.left, headerRight: header.right, headerBottom: header.bottom, left: box.left, right: box.right, top: box.top,
          bottom: box.bottom, height: box.height, scrollWidth: banner.scrollWidth, clientWidth: banner.clientWidth, viewport: window.innerHeight,
          lines: Math.round(box.height / parseFloat(getComputedStyle(banner).lineHeight))};
})()"""

start(sys.argv[1])
try:
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(source)))
    check("Open becomes available", wait_for("!document.querySelector('.start-image button[type=submit]').disabled"), True)
    js("document.querySelector('.start-image input').form.requestSubmit()")
    check("the raw source opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    check("there is no error banner while nothing is wrong", probe("!document.querySelector('.toolbar-error')"), True)

    call("test_disconnect_raw", diskId=probe("document.querySelector('[data-disk-id]').dataset.diskId"))
    js("[...document.querySelectorAll('.toolbar button')].find((b) => b.textContent.trim() === 'Start scan')?.click()")
    check("the error appears", wait_for("!!document.querySelector('.toolbar-error')", 30), True)
    text = probe("document.querySelector('.toolbar-error').textContent")
    check("it is the engine's sentence, whole", text.startswith("The disk has been disconnected;") and text.endswith("."), True)
    print("the error reads: %r" % text, flush=True)
    box = probe(GEOMETRY)
    check("it is as wide as the header: from its left edge to its right", abs(box["left"] - box["headerLeft"]) < 1.5 and abs(box["right"] - box["headerRight"]) < 1.5, True)
    check("it hangs below the header, not above it: nothing is clipped off the top", box["top"] >= box["headerBottom"] - 1 and box["top"] >= 0, True)
    check("it fits on one or two lines at this width", box["lines"] <= 2, True)
    check("its text is not cut off sideways", box["scrollWidth"] <= box["clientWidth"] + 1, True)
    screenshot(FOLDER / "header-error.png")

    # A long one wraps inside the banner, keeps the window's width, and scrolls inside itself when very long.
    js("document.querySelector('.toolbar-error').textContent = %s" % json.dumps(("The disk has been disconnected; analysis stopped. " + "The device went away while the range was being read, and every read after it failed. " * 12)))
    long = probe(GEOMETRY)
    check("a long error keeps the header's width", abs(long["left"] - long["headerLeft"]) < 1.5 and abs(long["right"] - long["headerRight"]) < 1.5, True)
    check("and wraps onto several lines", long["lines"] >= 4, True)
    check("and stays under the header", long["top"] >= long["headerBottom"] - 1, True)
    check("and inside the window: at most 30% of it tall", long["bottom"] <= long["headerBottom"] + 0.31 * long["viewport"], True)
    check("and is not cut off sideways", long["scrollWidth"] <= long["clientWidth"] + 1, True)
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
