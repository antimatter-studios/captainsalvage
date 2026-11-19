"""The start screen is a tree: each physical disk is a parent row with Open, its partitions nested under it.

The test gives the window a made-up list of disks (SALVAGE_TEST_STARTUP_DISKS), so no real disk is named or opened. Its
"devices" are small synthetic image files in this folder: a whole disk with no partition table,
a disk with three partitions, and a card reader with one. It checks that:

- there is one parent row per physical disk, with its own Open, and each volume is one row nested under its disk;
- every row has the same one button, Open, and there is no separate raw control;
- a disk with no readable partition table is still a parent row, with a short note;
- every volume has an inline "Block mount" checkbox that still sets and clears the block, and the
  list has one note about it, not one per row;
- a disk says once what it needs (unmounting, read permission), however many volumes need it;
- under a disk's own row, the partitions' rule, the notices' bar and the "no partitions" note start on one
  vertical line, and partition titles, notice text and notice commands start at one x (within 1px);
- Open on a whole disk opens all its bytes as raw bytes, and Open on a partition opens exactly that partition's bytes;
- three disks with their volumes fit on the screen at the app's default window size, with the
  image form still in view.

It saves picker-start.png in this folder (the screen as a person first sees it) and
picker-start-notices.png (the same disks needing an unmount and a permission).
"""
import base64
import json
import os
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import call, check, finish, js, probe, results, start, state, wait_for
from start_fixture import FOLDER, MIB, REPORT, UUID_A, UUID_B, calm, needy, part_a, part_b, part_c, part_d, reader, second, use, whole

use(calm)


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


def look(report):
    """Gives the window another list of disks and waits for it to show them."""
    REPORT.write_text(json.dumps(report))
    js("[...document.querySelectorAll('.start-heading button')].find((button) => button.textContent.trim() === 'Check again')?.click()")
    wait_for("document.querySelector('.start-heading button')?.textContent.trim() === 'Check again' && document.querySelectorAll('.start-volume').length === %d" % (len(report["volumes"]) + len(report["elsewhere"])))


def click_open(scope, index):
    """Clicks the Open button of the index-th element matching scope."""
    js("document.querySelectorAll(%s)[%d].querySelector('.start-open').click()" % (json.dumps(scope), index))


def fit():
    return probe("""(() => {
      const box = (selector) => document.querySelector(selector).getBoundingClientRect();
      const list = document.querySelector('.start-list');
      return {window: innerHeight, list: list.clientHeight, content: list.scrollHeight, panel: document.querySelector('.start-panel').scrollHeight,
              panelShown: document.querySelector('.start-panel').clientHeight, imageBottom: Math.round(box('.start-image').bottom)};
    })()""")


start(sys.argv[1])
try:
    check("the first look finishes", wait_for("document.querySelector('.start-heading button')?.textContent.trim() === 'Check again'"), True)
    check("each volume is one row", wait_for("document.querySelectorAll('.start-disk .start-volume').length === 4"), True)
    check("there is one card for each physical disk", probe("document.querySelectorAll('.start-disk').length"), 3)
    check("a parent row names the disk, its size and its device", probe("[...document.querySelectorAll('.start-disk-head .start-volume-title')].map((head) => head.textContent)"),
          ["UNREADABLE TABLE DRIVE5.2 MB, whole disk picker-whole.img", "STORAGE DEVICE8.4 MB, whole disk picker-second.img", "CARD READER4.2 MB, whole disk picker-reader.img"])
    check("every parent row has its own Open", probe("[...document.querySelectorAll('.start-disk-head button')].map((button) => button.textContent)"), ["Open", "Open", "Open"])
    check("a partition is nested under its disk, not beside it", probe("[...document.querySelectorAll('.start-disk')].map((card) => card.querySelectorAll(':scope > .start-volumes > .start-volume').length)"), [0, 3, 1])
    check("nested rows are indented under the parent", probe("(() => {const card = document.querySelectorAll('.start-disk')[1]; return card.querySelector('.start-volume strong').getBoundingClientRect().left > card.querySelector('.start-disk-head strong').getBoundingClientRect().left + 8;})()"), True)
    check("each volume is under its own disk", probe("[...document.querySelectorAll('.start-disk')].map((card) => [...card.querySelectorAll('.start-volume strong')].map((name) => name.textContent))"),
          [[], ["Holiday", "Backup", "Camera"], ["Dashcam"]])
    check("a disk with no readable partition table is still a parent row, with a short note", probe("document.querySelectorAll('.start-disk')[0].querySelector('.start-disk-empty')?.textContent"),
          "No partitions found: its partition table may be damaged or missing. Its bytes can still be opened.")
    check("a volume seen before says when, in its one line", probe("document.querySelectorAll('.start-volumes .start-volume-title span')[0].textContent.startsWith('ExFAT, 3.1 MB, picker-part-a.img. Seen before, last opened')"), True)
    check("a volume not seen before says so", probe("document.querySelectorAll('.start-volumes .start-volume-title span')[1].textContent"), "NTFS, 2.1 MB, picker-part-b.img. Not seen before.")
    check("each row has one short status", probe("[...document.querySelectorAll('.start-disk .start-status')].map((status) => status.textContent)"), ["Ready", "Ready", "Ready", "Ready"])
    check("a row's only button is Open, the same word as everywhere", probe("[...document.querySelectorAll('.start-disk .start-volume button')].map((button) => button.textContent)"), ["Open", "Open", "Open", "Open"])
    check("there is no separate raw control", probe("document.querySelectorAll('.start-raw-toggle, .start-raw').length + ([...document.querySelectorAll('button')].filter((button) => /raw/i.test(button.textContent)).length)"), 0)

    check("every volume has an inline Block mount checkbox", probe("[...document.querySelectorAll('.start-disk .mount-block-inline')].map((label) => label.textContent)"), ["Block mount"] * 4)
    check("the long per-row label is gone", probe("document.body.textContent.includes('Block mounting while')"), False)
    check("the list has one note about blocking", probe("[...document.querySelectorAll('.start-block-note')].map((note) => note.textContent)"),
          ["Block mount is to prevent data loss. While Captain Salvage is open, macOS is stopped from mounting a blocked volume, because mounting a damaged volume lets macOS write to it."])
    check("the note is below the list", probe("document.querySelector('.start-block-note').getBoundingClientRect().top >= document.querySelector('.start-list').getBoundingClientRect().bottom - 1"), True)
    check("nothing is blocked yet", [block["uuid"] for block in call("mount_blocks")["blocks"]], [])
    js("document.querySelectorAll('.start-disk .mount-block-inline input')[1].click()")
    check("the checkbox blocks that volume", wait_for("document.querySelectorAll('.start-disk .mount-block-inline input')[1].checked"), True)
    check("the block is saved for that volume only", sorted(block["uuid"].upper() for block in call("mount_blocks")["blocks"]), [UUID_B.upper()])
    js("document.querySelectorAll('.start-disk .mount-block-inline input')[1].click()")
    check("the checkbox lets it mount again", wait_for("!document.querySelectorAll('.start-disk .mount-block-inline input')[1].checked"), True)
    check("the block is gone", call("mount_blocks")["blocks"], [])
    check("a volume seen before, not attached, is a row too", probe("document.querySelectorAll('.start-elsewhere .start-volume').length"), 1)

    fitting = fit()
    print("three disks, four volumes: %s" % fitting, flush=True)
    check("the disks fit without scrolling at the default window size", fitting["content"] <= fitting["list"], True)
    check("the whole panel fits the window", fitting["panel"] <= fitting["panelShown"], True)
    check("the image form is still in view", fitting["imageBottom"] <= fitting["window"], True)
    check("the image form is one line", probe("document.querySelector('.start-image form').getBoundingClientRect().height < 46"), True)
    screenshot(FOLDER / "picker-start.png")

    look(needy)
    check("a disk says once what it needs", probe("[...document.querySelectorAll('.start-disk')].map((card) => [card.querySelectorAll('.start-caution').length, card.querySelectorAll('.start-needs').length])"),
          [[0, 0], [1, 1], [0, 1]])
    check("the permission is asked once for the disk, not for each volume", probe("[...document.querySelectorAll('.start-needs .command code')].map((code) => code.textContent)"),
          ["sudo chgrp staff %s %s" % (part_c, second), "sudo chgrp staff %s %s" % (part_d, reader)])
    check("two mounted volumes on one disk share one unmount command", probe("[...document.querySelectorAll('.start-caution .command code')].map((code) => code.textContent)"), ["diskutil unmountDisk " + second])
    check("no sentence is repeated", probe("(document.body.textContent.match(/Reading needs your permission/g) || []).length"), 2)
    check("each row has one short status again", probe("[...document.querySelectorAll('.start-disk .start-status')].map((status) => status.textContent)"), ["Mounted", "Mounted", "Needs permission", "Needs permission"])
    check("a volume that is mounted or unreadable has no Open button, but its disk does", probe("[document.querySelectorAll('.start-volume .start-open').length, document.querySelectorAll('.start-disk-head .start-open').length]"), [0, 3])
    # One left edge: under the disk's own row, the partitions' rule, the notices' bar and the "no partitions"
    # note start on one vertical line, and the partition titles, the notice text and the notice commands
    # start at one x. Measured, not looked at: a 1px rule against a 2px bar is the easy way to be 1px off.
    edges = probe("""(() => {
      const left = (node) => node.getBoundingClientRect().left;
      return [...document.querySelectorAll('.start-disk')].map((card) => ({
        head: left(card.querySelector('.start-disk-head strong')),
        rule: card.querySelector('.start-volumes') ? left(card.querySelector('.start-volumes')) : null,
        empty: card.querySelector('.start-disk-empty') ? left(card.querySelector('.start-disk-empty')) : null,
        bars: [...card.querySelectorAll('.start-caution, .start-needs')].map(left),
        titles: [...card.querySelectorAll('.start-volume strong')].map(left),
        notices: [...card.querySelectorAll('.start-caution p, .start-needs p')].map(left),
        commands: [...card.querySelectorAll('.start-caution .command, .start-needs .command')].map(left),
      }));
    })()""")
    near = lambda a, b: abs(a - b) <= 1
    for number, card in enumerate(edges):
        name = "disk %d: " % (number + 1)
        line = card["rule"] if card["rule"] is not None else card["empty"]
        check(name + "the partitions and the note sit in from the disk's text", line > card["head"] + 8, True)
        check(name + "every notice bar is on the partitions' line", all(near(bar, line) for bar in card["bars"]), True)
        check(name + "every notice text starts where the partition titles do", all(near(text, title) for text in card["notices"] for title in card["titles"]), True)
        check(name + "every notice command starts where the notice text does", all(near(box, text) for box in card["commands"] for text in card["notices"]), True)
    check("the checks measured a card with partitions and notices, and a card with the note", (len(edges[1]["titles"]), len(edges[1]["notices"]), edges[0]["empty"] is not None), (3, 2, True))
    check("the 'no partitions' note is on the same line as the partitions of another card", near(edges[0]["empty"], edges[1]["rule"]), True)
    needing = fit()
    print("the same disks needing an unmount and a permission: %s" % needing, flush=True)
    check("they fit too", needing["content"] <= needing["list"], True)
    screenshot(FOLDER / "picker-start-notices.png")
    look(calm)

    click_open(".start-disk-head", 0)
    check("Open on a whole disk with no partition table opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    check("it is a whole disk: raw bytes, with no filesystem applied", probe("document.querySelector('.interp-whole')?.textContent"), "Whole disk: raw bytes. Open one of its extents to read it as a filesystem.")
    check("the map covers the whole disk", probe("[...document.querySelectorAll('.side input')][1].value"), str(5 * MIB))

    js("[...document.querySelectorAll('.toolbar button')].find((button) => button.textContent === 'Change disk')?.click()")
    check("the start screen comes back", wait_for("document.querySelectorAll('.start-disk').length === 3"), True)
    click_open(".start-disk:nth-child(2) .start-volume", 1)
    check("Open on a partition opens", wait_for("!!document.querySelector('.app .explorer')"), True)
    check("an image with no filesystem opens as raw bytes", probe("document.querySelector('.interp-whole') !== null && !document.querySelector('.safety-chip')"), True)
    check("the map covers only that partition", probe("[...document.querySelectorAll('.side input')][1].value"), str(2 * MIB))
    js("[...document.querySelectorAll('.mode-switch button')].find((button) => button.textContent.trim() === 'Investigate')?.click()")
    check("the graph's last mark is the partition's end", wait_for("!!document.querySelector('.ruler span:last-child')"), True)
    check("no byte outside it is shown", probe("document.querySelector('.ruler span:last-child').textContent"), probe("document.querySelector('.explorer .row-size').textContent"))
finally:
    finish()
if not all(passed for _, passed in results):
    raise SystemExit(1)
