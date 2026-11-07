"""Window test for #20: Simple mode by default, with no cluster number anywhere; Investigate
adds the rest; agents can switch through set_mode."""
import os, re, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
TEXT = "document.querySelector('.app')?.innerText || ''"
DETAIL_TABS = "[...document.querySelectorAll('.inspector-tabs [role=tab]')].map((b) => b.getAttribute('aria-label'))"
CLUSTER_NUMBER = re.compile(r'\b[Cc]luster\s+\d|Lost folder\s+\d')
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    check('it starts in Simple', call('app_settings').get('mode'), 'simple')
    check('the map is hidden', probe("!!document.querySelector('.strips')"), False)
    check('the bytes view is hidden', probe("[...document.querySelectorAll('.center-tabs [role=tab]')].some((b) => b.textContent.trim() === 'Bytes')"), False)
    check('the details keep what recovery needs', probe(DETAIL_TABS), ['Selected', 'Rebuild', 'Chat with an agent', 'Treasure list'])
    check('the sidebar keeps the folders', probe("[...document.querySelectorAll('.explorer [role=tab]')].map((b) => b.textContent)"), ['Folders'])
    # Open every folder and pick a file, then read the whole window for a cluster number.
    for _ in range(3):
        js("[...document.querySelectorAll('.explorer-rows [aria-expanded=false]')].forEach((b) => b.click())")
        time.sleep(1)
    file = next(row for row in call('search', query='c.mp3')['rows'] if row['name'] == 'c.mp3')
    js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'entry', value: %d}})" % file['id'])
    time.sleep(3)
    for center in ('Overview', 'Browse files'):
        js("[...document.querySelectorAll('.center-tabs [role=tab]')].find((b) => b.textContent.trim() === %r)?.click()" % center)
        time.sleep(2)
        found = CLUSTER_NUMBER.findall(probe(TEXT))
        if found:
            print('where:', probe("[...document.querySelectorAll('.app *')].filter((n) => n.children.length === 0 && /[Cc]luster\\s+\\d/.test(n.textContent)).map((n) => [n.tagName, n.className, n.textContent.slice(0, 80), n.parentElement?.className])"), flush=True)
        check(f'no cluster number with {center} showing', found, [])
    js("[...document.querySelectorAll('.mode-switch [role=tab]')].find((b) => b.textContent.trim() === 'Investigate').click()")
    check('Investigate shows the map', wait_for("!!document.querySelector('.strips')", 10), True)
    check('and every tab', len(probe(DETAIL_TABS)), 10)
    check('the choice is kept', call('app_settings').get('mode'), 'investigate')
    check('cluster numbers are back for investigating', bool(CLUSTER_NUMBER.search(probe(TEXT))), True)
    call('set_mode', mode='simple')
    check('an agent can switch it back', wait_for("!document.querySelector('.strips')", 10), True)
finally:
    finish()
