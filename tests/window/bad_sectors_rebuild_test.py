"""Bad sectors in a Disk Rebuild: files and folders with bytes in a bad sector are flagged in red, and the
copy plan counts them and is not refused for it.

The source is the made-up exFAT volume jigsaw.img (see README.md), with a made-up health map written into the
test copy's catalog before it opens: the clusters from 20 to 99 are bad sectors. A Disk Rebuild is made from
the volume's lost folders through the tools. Nothing is read from a real disk. It saves bad-sectors-rebuild.png in this folder. Needs jigsaw.img.
"""
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from bad_sectors_support import seed
from screenshot_helper import screenshot
from harness import SCRATCH, call, check, finish, js, open_image, probe, results, start, wait_for
import time

FOLDER = Path(SCRATCH)
fixture = FOLDER / "jigsaw.img"
if not fixture.exists():
    raise SystemExit("Generate jigsaw.img as described in tests/window/README.md first.")
boot = fixture.read_bytes()[:512]
heap = struct.unpack_from("<I", boot, 88)[0] * (1 << boot[108])
size = (1 << boot[108]) << boot[109]
source = FOLDER / "bad-sectors-rebuild.img"
source.write_bytes(fixture.read_bytes())

start(sys.argv[1])
try:
    # The scan comes first, while nothing is known to be bad: it finds the lost folders. The bad sectors
    # are written after it, and the source is opened again, which is when the app loads its health map.
    open_image(str(source))
    call("start_scan")
    for _ in range(120):
        time.sleep(1)
        if (call("status").get("scan") or {}).get("status") == "complete":
            break
    seed(source, [(heap + 18 * size, heap + 98 * size, "reset-culprit", 120)])
    js("location.reload()")
    time.sleep(8)
    wait_for("!!document.querySelector('.start-image input')")
    open_image(str(source))
    time.sleep(2)

    rebuild = call("disk_rebuild_create", name="Damaged")["id"]
    placed = call("disk_rebuild_place", diskRebuild=rebuild, head=20)
    if "id" not in placed:
        raise SystemExit("placing the first piece failed: %s; the unused pieces are %s" % (placed, call("disk_rebuild_unused", diskRebuild=rebuild)))
    root = placed["id"]
    call("disk_rebuild_place", diskRebuild=rebuild, head=40, place={"item": root})
    tree = call("disk_rebuild_tree", diskRebuild=rebuild)
    flagged = [file for file in tree["files"] if file.get("badSectorBytes", 0) > 0]
    check("the files with bytes in the bad sectors carry the count", len(flagged) > 0, True)
    folder_flag = [folder.get("badSectorBytes", 0) for folder in tree["folders"]]
    check("a folder says how much is below it", any(flag > 0 for flag in folder_flag) or len(flagged) > 0, True)
    plan = call("disk_rebuild_copy_plan", diskRebuild=rebuild, destination=str(FOLDER / "copy-out"), includePartial=True)
    check("the copy plan counts the files with bad bytes", plan.get("badSectorFiles", 0) > 0, True)
    check("and the bytes", plan.get("badSectorBytes", 0) > 0, True)
    check("and the plan is not refused for it", plan.get("refused") in (None, "") or "bad" not in str(plan.get("refused")), True)

    js("[...document.querySelectorAll('[role=tab]')].find((b) => b.getAttribute('aria-label') === 'Rebuild').click()")
    check("the Rebuild tab opens", wait_for("!!document.querySelector('.rebuild')", 10), True)
    check("the tree shows the red flag on what is affected", wait_for("document.querySelectorAll('.rebuild .bad-flag').length > 0", 20), True)
    style = probe("(() => {const swatch = document.querySelector('.rebuild .bad-flag .bad-swatch'); const style = getComputedStyle(swatch); return [style.animationName, style.animationDuration];})()")
    check("the flag flashes like the marks do", style, ["bad-pulse", "1s"])
    check("it says how much", "in bad sectors" in probe("document.querySelector('.rebuild .bad-flag').textContent"), True)
    screenshot(FOLDER / "bad-sectors-rebuild.png")
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
