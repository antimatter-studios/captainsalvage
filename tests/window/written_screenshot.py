"""Saves the "Something wrote to this disk" box, closed, and the safety panel as PNGs, from a made-up volume only.

    python3 tests/window/written_screenshot.py <binary> <folder> [prefix]

Works on a build from before the close button too (it folds the box with the link then), which is how the
"before" pictures of the change that added it were made.
"""
import os, sys, time
from pathlib import Path
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import call, finish, js, probe, scan_and_reopen, start, wait_for
from written_fixture import change_after_baseline, fresh_copy, screenshot

folder = Path(sys.argv[2])
prefix = sys.argv[3] if len(sys.argv) > 3 else 'written'
folder.mkdir(parents=True, exist_ok=True)
start(sys.argv[1])
try:
    scan_and_reopen(fresh_copy())
    change_after_baseline()
    call('verify_unchanged')
    wait_for("!!document.querySelector('.written-warning')", 30)
    time.sleep(1)
    screenshot(folder / ('%s-box.png' % prefix))
    js("(document.querySelector('.written-close') || [...document.querySelectorAll('.written-warning button.link')][0])?.click()")
    time.sleep(1.5)
    screenshot(folder / ('%s-closed.png' % prefix))
    js("(() => { const b = document.querySelector('.safety-button'); if (b && b.getAttribute('aria-expanded') !== 'true') b.click(); })()")
    time.sleep(1.5)
    screenshot(folder / ('%s-panel.png' % prefix))
finally:
    finish()
