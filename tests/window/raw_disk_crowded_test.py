"""A raw scan that finds many filesystems keeps the window responsive and the lists short.

The source is a sparse synthetic image with NTFS boot sectors every 128 KiB, so the scan reports
well over a thousand findings. While it runs, a 20 ms timer in the window records its worst
delay: a frozen window shows up as one long gap. The evidence list must also stay one page long
until more is asked for. The image lives in this folder and is never a real disk.
"""
import json
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, js, probe, results, start, wait_for

FINDINGS = 1500
STRIDE = 128 * 1024
WORST_GAP_MILLIS = 400


def click(text, scope=".app"):
    js("[...document.querySelectorAll(%s + ' button')].find((button) => button.textContent.trim() === %s)?.click()" % (json.dumps(scope), json.dumps(text)))


source = Path(SCRATCH) / "crowded.img"
length = (FINDINGS + 1) * STRIDE
with source.open("wb") as image:
    image.truncate(length)
    for index in range(FINDINGS):
        boot = bytearray(512)
        boot[3:11] = b"NTFS    "
        struct.pack_into("<H", boot, 11, 512)
        boot[13] = 8
        struct.pack_into("<Q", boot, 40, STRIDE // 512)
        boot[510:512] = b"\x55\xaa"
        image.seek(index * STRIDE + 4096)
        image.write(boot)

start(sys.argv[1])
try:
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(source)))
    check("Open becomes available", wait_for("!document.querySelector('.start-image button[type=submit]').disabled"), True)
    js("document.querySelector('.start-image input').form.requestSubmit()")
    check("the raw source opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    click("Investigate", ".mode-switch")
    check("the graph is shown", wait_for("document.querySelectorAll('.strips .strip canvas').length === 4"), True)
    js("(() => {window.__lag = {worst: 0}; let last = performance.now();"
       "setInterval(() => {const now = performance.now(); window.__lag.worst = Math.max(window.__lag.worst, now - last - 20); last = now;}, 20);})()")
    click("Start scan", ".toolbar")
    check("the scan finishes", wait_for("/complete|Display limit/.test(document.querySelector('.app').textContent)", 240), True)
    worst = probe("window.__lag.worst")
    print("worst timer delay during the scan: %.0f ms" % worst, flush=True)
    check("the window never froze for %d ms during the scan" % WORST_GAP_MILLIS, worst < WORST_GAP_MILLIS, True)
    found = probe("Number(document.querySelector('.source-summary tr:nth-child(2) td.numeric').textContent.replace(/\\D/g, ''))")
    check("the scan found over a thousand filesystems", found > 1000, True)
    check("the evidence list shows one page", probe("document.querySelectorAll('.explorer .row').length"), 1 + 0 + 100 + 1)
    js("[...document.querySelectorAll('.explorer button')].find((button) => button.textContent.startsWith('Show more'))?.click()")
    check("more evidence is shown on request", probe("document.querySelectorAll('.explorer .row').length"), 1 + 0 + 200 + 1)
    probe("(() => {const worst = window.__lag.worst; window.__lag.worst = 0; return worst;})()")
    js("[...document.querySelectorAll('.strips .strip')].at(-1)?.dispatchEvent(new WheelEvent('wheel', {deltaY: -120, bubbles: true}))")
    check("the graph answers after the scan", probe("window.__lag.worst < %d" % WORST_GAP_MILLIS), True)
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
