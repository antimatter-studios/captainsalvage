"""Window test for #19: a legend click isolates a category, and hovering a file lights its pieces."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
# Counts how many pixels of the whole-volume strip are fully drawn, and how many are faint.
PIXELS = "(() => { const c = document.querySelector('.strips canvas'); const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data; let solid = 0, faint = 0; for (let i = 3; i < d.length; i += 4) { if (d[i] === 255) solid += 1; else if (d[i] > 0) faint += 1; } return [solid, faint]; })()"
# How much of the marks layer above the strips is lit.
MARKS = "(() => { const cs = [...document.querySelectorAll('.strips canvas')]; let lit = 0; for (const c of cs) { const d = c.getContext('2d').getImageData(0, 0, c.width, c.height).data; for (let i = 3; i < d.length; i += 4) if (d[i] > 0) lit += 1; } return lit; })()"
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    time.sleep(2)
    before = probe(PIXELS)
    print('before:', before, flush=True)
    js("document.querySelector('.strips .legend .legend-button').click()")
    time.sleep(2)
    isolated = probe(PIXELS)
    print('isolated:', isolated, 'pressed:', probe("document.querySelector('.strips .legend .legend-button').getAttribute('aria-pressed')"), flush=True)
    check('other categories are drawn faint', isolated[1] > 0 and isolated[0] < before[0], True)
    js("document.querySelector('.strips .legend .legend-button').click()")
    time.sleep(2)
    check('clicking again shows everything', probe(PIXELS)[1], 0)
    # Hover a file in the sidebar.
    js("[...document.querySelectorAll('.explorer-rows [aria-expanded=false]')].forEach((b) => b.click())")
    time.sleep(2)
    unlit = probe(MARKS)
    js("[...document.querySelectorAll('.explorer-rows .row')].find((r) => r.querySelector('.row-name')?.textContent.endsWith('.mp3'))?.dispatchEvent(new MouseEvent('mouseover', {bubbles: true}))")
    time.sleep(2)
    lit = probe(MARKS)
    print('marks before hover:', unlit, 'during:', lit, flush=True)
    check('hovering a file lights its pieces', lit > unlit, True)
    js("[...document.querySelectorAll('.explorer-rows .row')].find((r) => r.querySelector('.row-name')?.textContent.endsWith('.mp3'))?.dispatchEvent(new MouseEvent('mouseout', {bubbles: true}))")
    time.sleep(2)
    check('leaving the row puts the map back', probe(MARKS), unlit)
finally:
    finish()
