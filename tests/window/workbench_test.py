"""Window test for #23: gather pieces in the workbench, order them, test, show and keep."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
SHOW = "window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: %r, value: %d}})"
BUTTON = "[...document.querySelectorAll('.workbench button')].find((b) => b.textContent.trim().startsWith(%r))"
LABELS = "[...document.querySelectorAll('.workbench-piece-label')].map((n) => n.textContent)"
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    file = next(row for row in call('search', query='a1.jpg')['rows'] if row['name'] == 'a1.jpg')
    js(SHOW % ('entry', file['id']))
    time.sleep(2)
    js("[...document.querySelectorAll('[role=tab]')].find((b) => b.getAttribute('aria-label') === 'Workbench').click()")
    check('the workbench opens', wait_for("!!document.querySelector('.workbench')", 10), True)
    js("(%s).click()" % (BUTTON % 'Add the selection'))
    check('the file is added', wait_for("%s.length === 1" % LABELS, 10), True)
    check('its name fills in', probe("document.querySelector('.workbench-fields input').value"), 'a1.jpg')
    js(SHOW % ('cluster', 60))
    time.sleep(2)
    js("(%s).click()" % (BUTTON % 'Add the selection'))
    wait_for("%s.length === 2" % LABELS, 10)
    check('a cluster is added after it', probe(LABELS), ['a1.jpg', 'Cluster 60'])
    js("[...document.querySelectorAll('.workbench-piece')][1].querySelector('[aria-label=\"Move up\"]').click()")
    time.sleep(1)
    check('pieces can be reordered', probe(LABELS), ['Cluster 60', 'a1.jpg'])
    js("[...document.querySelectorAll('.workbench-piece')][0].querySelector('[aria-label=\"Remove\"]').click()")
    time.sleep(1)
    check('a piece can be removed', probe(LABELS), ['a1.jpg'])
    js("(%s).click()" % (BUTTON % 'Test it'))
    check('testing reports the structure', wait_for("/structure is valid|no test for this type/.test(document.querySelector('.workbench-report')?.textContent || '')", 20), True)
    print('report:', probe("document.querySelector('.workbench-report')?.textContent"), flush=True)
    js("(%s).click()" % (BUTTON % 'Show it'))
    check('showing it reads the picture', wait_for("!!document.querySelector('.workbench .member-preview img')", 20), True)
    js("(%s).click()" % (BUTTON % 'Keep as the clusters of'))
    check('it can be kept as a hypothesis', wait_for("/Saved as the clusters of a1.jpg/.test(document.querySelector('.workbench')?.innerText || '')", 20), True)
    overlays = call('list_overlays')
    rows = overlays if isinstance(overlays, list) else overlays.get('overlays', overlays)
    check('the hypothesis is recorded for that file', any(o.get('kind') == 'file_extents' and o.get('target') == file['id'] for o in rows), True)
finally:
    finish()
