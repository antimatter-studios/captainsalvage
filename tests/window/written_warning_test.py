"""Window test: the "Something wrote to this disk" box can be closed, and what it said moves to the safety panel.

It copies jigsaw.img to written.img (see the README), opens and scans that copy, then sets bits in the copy's
allocation bitmap, after the baseline was recorded, for clusters that were free (written_fixture.py). Only that
copy is touched. It checks that:

- the box has a close button in its top-right corner, named for a screen reader, and no "Fold away" link;
- closing it removes the box and leaves no red strip anywhere, while the Check disk safety button stays red;
- the safety panel then shows the same information (what changed, how many clusters and files, the advice about
  macOS) with Show the files, Rebuild the map now and Check again, and they work;
- looking again at the same change does not bring the box back;
- a new change (a different current hash) brings it back once, and Escape closes it;
- a disk that is no longer changed shows no box and no details.

It saves written-box.png, written-closed.png and written-panel.png here.
"""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
from written_fixture import change_after_baseline, fresh_copy, screenshot


def box():
    return probe("!!document.querySelector('.written-warning')")


def panel_text():
    return probe("document.querySelector('.safety-list')?.innerText || ''")


def open_panel():
    js("(() => { const b = document.querySelector('.safety-button'); if (b && b.getAttribute('aria-expanded') !== 'true') b.click(); })()")
    wait_for("!!document.querySelector('.safety-list')", 5)


start(sys.argv[1])
try:
    scan_and_reopen(fresh_copy())
    check('a disk that was not changed shows no box', box(), False)
    open_panel()
    check('and the safety panel says the structures are unchanged', 'are unchanged since' in panel_text(), True)

    change_after_baseline((22, 24))
    call('verify_unchanged')
    check('writing to the disk after the baseline shows the box', wait_for("!!document.querySelector('.written-warning')", 30), True)
    time.sleep(1)
    screenshot(os.path.join(SCRATCH, 'written-box.png'))
    check('the box says what it always said', probe("document.querySelector('.written-warning strong').textContent.startsWith('Something wrote to this disk after')"), True)
    check('it has no Fold away link', probe("[...document.querySelectorAll('.written-warning button')].some((b) => b.textContent.trim() === 'Fold away')"), False)
    check('it has a close button named for a screen reader', probe("document.querySelector('.written-warning button.written-close')?.getAttribute('aria-label')"), 'Close')
    corner = probe("""(() => {
      const box = document.querySelector('.written-warning').getBoundingClientRect();
      const close = document.querySelector('.written-close').getBoundingClientRect();
      return {right: box.right - close.right, top: close.top - box.top, size: close.height};
    })()""")
    print('the close button is %.0f px from the right edge and %.0f px from the top, %.0f px tall' % (corner['right'], corner['top'], corner['size']), flush=True)
    check('the close button is in the top-right corner of the box', 0 <= corner['right'] < 24 and 0 <= corner['top'] < 24, True)
    check('and big enough to hit', corner['size'] >= 24, True)
    check('the safety button is red while the disk is changed', probe("!!document.querySelector('.safety-chip-bad')"), True)
    first = probe("document.querySelector('.written-warning p')?.textContent")

    # Close it: no box, no strip, and the safety button stays red.
    js("document.querySelector('.written-close').click()")
    check('closing removes the box', wait_for("!document.querySelector('.written-warning')", 5), True)
    check('and leaves no strip on the screen', probe("!document.querySelector('.written-strip') && ![...document.querySelectorAll('button')].some((b) => b.textContent.includes('Show what changed'))"), True)
    check('the safety button is still red', probe("!!document.querySelector('.safety-chip-bad')"), True)
    time.sleep(1)
    screenshot(os.path.join(SCRATCH, 'written-closed.png'))

    # What the box said is in the panel.
    open_panel()
    check('the safety panel shows the same findings', wait_for("(document.querySelector('.safety-list')?.innerText || '').includes('clusters')", 10), True)
    text = panel_text()
    check('it names the change', 'Structures have changed since the baseline' in text, True)
    check('it says how many clusters and files', '2 clusters' in text and '2 found files' in text, True)
    check('it repeats the advice about macOS', 'macOS writes like this when it mounts the disk' in text, True)
    check('it is the same sentence the box had', probe("document.querySelector('.safety-list .written-details p')?.textContent") == first, True)
    check('it offers Show the files, Rebuild the map now and Check again', probe("[...document.querySelectorAll('.safety-list .written-details button')].map((b) => b.textContent.trim())"),
          ['Show the files', 'Rebuild the map now', 'Check again'])
    screenshot(os.path.join(SCRATCH, 'written-panel.png'))
    js("[...document.querySelectorAll('.safety-list .written-details button')].find((b) => b.textContent.trim() === 'Show the files').click()")
    check('Show the files lists the files', wait_for("document.querySelectorAll('.safety-list .written-files tbody tr').length === 2", 10), True)
    check('they are the two files that used the clusters', probe("[...document.querySelectorAll('.safety-list .written-files tbody tr td:first-child')].map((c) => c.textContent.split('/').pop()).sort()"), ['a1.jpg', 'deep.mp3'])

    # Looking again at the same change does not bring the box back.
    js("[...document.querySelectorAll('.safety-list .written-details button')].find((b) => b.textContent.trim() === 'Check again').click()")
    time.sleep(3)
    call('verify_unchanged')
    time.sleep(4)
    check('the same change does not bring the box back', box(), False)
    check('the panel still shows it', 'clusters' in panel_text(), True)

    # A new change brings it back once; Escape closes it.
    change_after_baseline((22, 24, 41))
    call('verify_unchanged')
    check('a new change brings the box back', wait_for("!!document.querySelector('.written-warning')", 20), True)
    check('it counts the new clusters', probe("document.querySelector('.written-warning p')?.textContent.startsWith('3 clusters')"), True)
    check('and while it is shown the panel does not repeat it', probe("!document.querySelector('.safety-list .written-details')"), True)
    js("document.dispatchEvent(new KeyboardEvent('keydown', {key: 'Escape', bubbles: true}))")
    check('Escape closes the box', wait_for("!document.querySelector('.written-warning')", 5), True)
    check('and leaves no strip', probe("!document.querySelector('.written-strip')"), True)
    call('verify_unchanged')
    time.sleep(4)
    check('it stays closed', box(), False)
finally:
    finish()
