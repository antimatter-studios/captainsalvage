"""Saves the start screen, given the made-up disks of start_fixture.py, as PNGs. Never a real disk.

    python3 tests/window/start_screenshot.py <binary> <folder> [prefix]

Writes <folder>/<prefix>-start.png (the disks as a person first sees them) and
<folder>/<prefix>-start-notices.png (the same disks needing an unmount and a permission).
"""
import base64
import json
import os
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import finish, js, start, state, wait_for
from start_fixture import REPORT, calm, needy, use


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


def look(report):
    REPORT.write_text(json.dumps(report))
    js("[...document.querySelectorAll('button')].find((button) => button.textContent.trim() === 'Check again')?.click()")
    wait_for("document.querySelectorAll('.start-volume').length >= %d" % len(report["volumes"]))
    wait_for("!!document.querySelector('.start-volume')")


folder = Path(sys.argv[2])
prefix = sys.argv[3] if len(sys.argv) > 3 else "start"
folder.mkdir(parents=True, exist_ok=True)
use(calm)
start(sys.argv[1])
try:
    wait_for("document.querySelectorAll('.start-volume').length >= 4")
    screenshot(folder / ("%s-start.png" % prefix))
    look(needy)
    screenshot(folder / ("%s-start-notices.png" % prefix))
finally:
    finish()
