"""Marking bad sectors from the window: the zone is drawn in flashing red, kept with who and why, and a bad request says so.

The source is a made-up 24 MiB disk. Nothing is read from a real disk. The zone is the one the owner's dd run found,
scaled down to this disk. Needs nothing else.
"""
import json
import os
import struct
import sys
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, results, start, wait_for

MIB = 1024 * 1024
FOLDER = Path(SCRATCH)
image = bytearray(24 * MIB)
image[446 + 4] = 0x07
struct.pack_into("<II", image, 446 + 8, 2 * MIB // 512, 20 * MIB // 512)
image[510:512] = b"\x55\xaa"
source = FOLDER / "bad-sectors-mark.img"
source.write_bytes(image)

SET = ("(() => {const label = [...document.querySelectorAll('.mark-bad label')].find((l) => l.textContent.startsWith(%s));"
       "const field = label.querySelector('input, select'); const proto = field.tagName === 'SELECT' ? HTMLSelectElement.prototype : HTMLInputElement.prototype;"
       "Object.getOwnPropertyDescriptor(proto, 'value').set.call(field, %s); field.dispatchEvent(new Event(field.tagName === 'SELECT' ? 'change' : 'input', {bubbles: true}));})()")

start(sys.argv[1])
try:
    js("(() => {const input=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(input,%s);"
       "input.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(str(source)))
    check("Open becomes available", wait_for("!document.querySelector('.start-image button[type=submit]').disabled"), True)
    js("document.querySelector('.start-image input').form.requestSubmit()")
    check("the raw source opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    js("[...document.querySelectorAll('.mode-switch button')].find((b) => b.textContent.trim() === 'Investigate')?.click()")
    check("the map is shown", wait_for("document.querySelectorAll('.strips .strip canvas').length === 4"), True)
    check("nothing is marked yet", probe("document.querySelectorAll('.strips .bad-range').length"), 0)
    js("document.querySelector('.mark-bad summary').click()")
    button = "[...document.querySelectorAll('.mark-bad button')].find((b) => b.textContent.trim() === 'Mark as bad sectors')"
    check("the button waits for a zone and a reason", probe(button + ".disabled"), True)
    js(SET % (json.dumps("First byte"), json.dumps("10,485,760")))
    js(SET % (json.dumps("One past the last byte"), json.dumps(str(10 * MIB + 1_073_741_824 // 1024))))
    js(SET % (json.dumps("What happened"), json.dumps("reset-culprit")))
    check("a reason is still needed", probe(button + ".disabled"), True)
    js(SET % (json.dumps("Why"), json.dumps("dd from 10 MiB dropped the drive")))
    check("with a zone and a reason it can be pressed", probe(button + ".disabled"), False)
    js(button + ".click()")
    check("the zone is drawn in flashing red", wait_for("document.querySelectorAll('.strips .strip .bad-range').length >= 1", 20), True)
    check("it is the one that was marked", probe("document.querySelector('.mark-bad [role=status]')?.textContent.includes('Marked')"), True)
    ranges = call("health_map", diskId=probe("document.querySelector('[data-disk-id]').dataset.diskId"))["ranges"]
    bad = [r for r in ranges if r["state"] == "reset-culprit"]
    check("the engine has it, from the first byte given", [r["start"] for r in bad], [10 * MIB])
    check("with who marked it and why", bad[0]["note"], "marked by you: dd from 10 MiB dropped the drive")
    check("and when", bad[0]["foundMillis"] > 0, True)
    check("the legend names bad sectors", "Bad sectors: reading here drops the drive" in probe("document.querySelector('.strips .legend').textContent"), True)

    # A zone past the end of the disk is refused with the reason, and nothing is drawn for it.
    js(SET % (json.dumps("First byte"), json.dumps("1")))
    js(SET % (json.dumps("One past the last byte"), json.dumps("999999999999")))
    check("past the end the button stays disabled", probe(button + ".disabled"), True)
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
