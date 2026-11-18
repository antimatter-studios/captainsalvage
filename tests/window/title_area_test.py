"""The title area: one row across the top of the window, left and right.

The left half holds the app's icon, its name as a title and the version. The right half is the
workspace toolbar: the open device as a title, then its size, Change disk and the interpretation
picker. Both sit beside the macOS window buttons and drag the window. This opens the synthetic
jigsaw.img, and a symlink to it with a very long name, and checks that:

- the icon is the app icon's file, byte for byte, and large enough to stay sharp on a retina screen;
- the app's name and the device name are titles: the same size (18 px or more) and a heavy weight;
- the two halves have the same height and top, and their titles start on the same line;
- both halves drag the window, and the icon starts clear of the window buttons;
- a long device name is cut short with its full path as a tooltip, and the row still shows Change
  disk, the picker and the safety chip, with nothing pushed out of the row;
- the row keeps its height at the window's smallest width, and the theme and language switches work.

Synthetic images only. It needs jigsaw.img (see README.md) and saves title-area.png,
title-area-long.png and title-area-dark.png here.
"""
import base64
import hashlib
import json
import os
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, js, open_image, probe, start, state, wait_for

ICON = Path(SCRATCH).parent.parent / "src-tauri/icons/128x128@2x.png"
LONG = "a-very-long-device-name-that-keeps-going-" * 3 + "jigsaw.img"


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


def reopen(path):
    js("location.reload()")
    if not wait_for("!!document.querySelector('.start-image input')"):
        raise SystemExit("the start screen never came back")
    open_image(path)


MEASURE = """(() => {
  const box = (selector) => { const e = document.querySelector(selector); if (!e) return null; const r = e.getBoundingClientRect(); return {top: r.top, left: r.left, right: r.right, bottom: r.bottom, height: r.height, width: r.width}; };
  const style = (selector) => { const e = document.querySelector(selector); const s = e && getComputedStyle(e); return s && {size: parseFloat(s.fontSize), weight: parseInt(s.fontWeight, 10)}; };
  const row = document.querySelector('.toolbar');
  return {
    left: box('.sidebar-drag'), right: box('.toolbar'),
    leftTitle: box('.sidebar-brand .title-name'), rightTitle: box('.brand-device'),
    leftStyle: style('.sidebar-brand .title-name'), rightStyle: style('.brand-device'),
    icon: box('.sidebar-brand-icon'), version: document.querySelector('.brand-version')?.textContent ?? null,
    change: box('.brand-change'), picker: box('.brand-meta .interp-pick select'), safety: box('.safety-chip'), meta: box('.brand-meta'),
    metaCut: (() => { const m = document.querySelector('.brand-meta'); return m ? m.scrollWidth > m.clientWidth + 1 : null; })(),
    deviceCut: (() => { const d = document.querySelector('.brand-device'); return d ? d.scrollWidth > d.clientWidth + 1 : null; })(),
    deviceTip: document.querySelector('.brand-device')?.title ?? null,
    rowOverflow: row ? row.scrollWidth > row.clientWidth + 1 : null,
    leftDrag: document.querySelector('.sidebar-drag')?.hasAttribute('data-tauri-drag-region'),
    rightDrag: row?.hasAttribute('data-tauri-drag-region'),
  };
})()"""


def sha_of_icon_in_window():
    return probe("""(async () => {
      const img = document.querySelector('.sidebar-brand-icon');
      const bytes = await (await fetch(img.src)).arrayBuffer();
      const digest = await crypto.subtle.digest('SHA-256', bytes);
      return [...new Uint8Array(digest)].map((b) => b.toString(16).padStart(2, '0')).join('') + ':' + img.naturalWidth;
    })()""")


image = os.path.join(SCRATCH, "jigsaw.img")
long_image = os.path.join(SCRATCH, LONG)
if os.path.lexists(long_image):
    os.remove(long_image)
os.symlink(image, long_image)
start(sys.argv[1])
try:
    open_image(image)
    wait_for("!!document.querySelector('.brand-version')")
    seen = probe(MEASURE)
    screenshot(os.path.join(SCRATCH, "title-area.png"))
    digest, natural = sha_of_icon_in_window().split(":")
    check("the header icon is the app icon file, byte for byte", digest, hashlib.sha256(ICON.read_bytes()).hexdigest())
    check("it is sharp on a retina screen (at least 4 times its drawn size)", int(natural) >= 4 * round(seen["icon"]["width"]), True)
    check("the icon is drawn between 28 and 36 px", 28 <= seen["icon"]["width"] <= 36 and 28 <= seen["icon"]["height"] <= 36, True)
    check("the icon starts clear of the window buttons", seen["icon"]["left"] >= 78, True)
    check("the version is a label under the name", (seen["version"] or "").startswith("v") and seen["leftTitle"]["bottom"] <= probe("document.querySelector('.brand-version').getBoundingClientRect().top") + 1, True)
    check("the app's name is a title: 18 px or more, heavy", seen["leftStyle"]["size"] >= 18 and seen["leftStyle"]["weight"] >= 600, True)
    check("the device name is a title in the same size and weight", seen["rightStyle"], seen["leftStyle"])
    check("both halves have the same height", round(seen["left"]["height"]), round(seen["right"]["height"]))
    check("the row is compact: 52 px", round(seen["left"]["height"]), 52)
    check("both halves start at the top of the window", (seen["left"]["top"], seen["right"]["top"]), (0, 0))
    check("the two titles start on the same line", abs(seen["leftTitle"]["top"] - seen["rightTitle"]["top"]) <= 1, True)
    check("both halves drag the window", (seen["leftDrag"], seen["rightDrag"]), (True, True))
    check("the size, Change disk and the picker fit in the row", seen["metaCut"] is False and seen["rowOverflow"] is False and seen["change"] is not None and seen["picker"] is not None, True)
    check("the controls are still in the row", seen["safety"] is not None and seen["safety"]["right"] <= seen["right"]["right"], True)

    reopen(long_image)
    wait_for("!!document.querySelector('.brand-version')")
    long = probe(MEASURE)
    screenshot(os.path.join(SCRATCH, "title-area-long.png"))
    check("a long device name is cut short", long["deviceCut"], True)
    check("and gives its full path as a tooltip", (long["deviceTip"] or "").endswith(LONG), True)
    check("the row keeps its height", round(long["right"]["height"]), 52)
    check("Change disk, the picker and the controls are still in the row", long["change"] is not None and long["picker"] is not None and long["safety"] is not None and long["safety"]["right"] <= long["right"]["right"], True)
    check("nothing is pushed out of the row", (long["metaCut"], long["rowOverflow"]), (False, False))

    # The smallest window is 1360 px wide.
    js("document.getElementById('root').style.width = '1360px'; document.getElementById('root').style.overflow = 'hidden'")
    small = probe(MEASURE)
    check("at the smallest window the row is still one height", round(small["right"]["height"]), 52)
    check("and Change disk and the controls are still in the row", small["change"] is not None and small["change"]["right"] <= small["right"]["right"] and small["safety"] is not None and small["safety"]["right"] <= small["right"]["right"], True)
    js("document.getElementById('root').style.width = ''")

    check("the theme and language switches are in the row", probe("!!document.querySelector('.toolbar .theme-switch') && !!document.querySelector('.toolbar .language-switch')"), True)
    before = probe("getComputedStyle(document.querySelector('.toolbar')).backgroundColor")
    for choice in ("Dark", "Light"):
        js("document.querySelector('.toolbar .theme-switch [aria-label=%s]')?.click()" % choice)
        wait_for("document.querySelector('.theme-switch [aria-label=%s]').getAttribute('aria-checked') === 'true'" % choice, 15)
    pick = probe("getComputedStyle(document.querySelector('.toolbar')).backgroundColor")
    js("document.querySelector('.toolbar .theme-switch [aria-label=Dark]')?.click()")
    check("the theme switch changes the title row's colour", wait_for("getComputedStyle(document.querySelector('.toolbar')).backgroundColor !== %s" % json.dumps(pick), 15), True)
    screenshot(os.path.join(SCRATCH, "title-area-dark.png"))
    js("document.querySelector('.toolbar .language-switch [lang=fr]')?.click()")
    check("the language switch translates Change disk", wait_for("document.querySelector('.brand-change')?.textContent !== 'Change disk'", 15), True)
    js("document.querySelector('.toolbar .language-switch [lang=en]')?.click()")
finally:
    if os.path.lexists(long_image):
        os.remove(long_image)
    finish()
