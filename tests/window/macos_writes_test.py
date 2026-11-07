"""Window test for #48: macOS's writes can be shown on the map, and a file they touched says so."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'macos.img'))
    check('the overview lists the dates', wait_for("(document.querySelector('.recovery')?.innerText || '').includes('2026-11-04')", 30), True)
    LABEL = "[...document.querySelectorAll('.strips span')].map((n) => n.textContent).find((text) => text.startsWith('Magnified, '))"
    before = probe(LABEL)
    js("[...document.querySelectorAll('.recovery-table tr')].find((r) => r.textContent.includes('2026-11-04')).querySelector('button').click()")
    time.sleep(2)
    after = probe(LABEL)
    print('magnified before:', before, '| after:', after, flush=True)
    check('the map zooms to those clusters', after != before and 'Magnified' in (after or ''), True)
    found = call('search', query='0001')['rows'][0]['id']
    js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'entry', value: %d}})" % found)
    check('a file macOS wrote over says so', wait_for("(document.querySelector('.detail-macos-writes')?.textContent || '').includes('2026-11-04')", 20), True)
    photo = call('search', query='photo.jpg')['rows'][0]['id']
    js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'entry', value: %d}})" % photo)
    time.sleep(3)
    check('the person’s own file does not', probe("document.querySelector('.detail-name')?.textContent === 'photo.jpg' && !document.querySelector('.detail-macos-writes')"), True)
finally:
    finish()
