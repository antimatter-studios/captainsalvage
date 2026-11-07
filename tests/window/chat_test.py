"""Window test for #22: suggested questions, cluster links in answers, and folded tool calls.
No agent runs: the answer is played into the window as the chat's own events."""
import json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
EMIT = "window.__TAURI_INTERNALS__.invoke('plugin:event|emit', {event: 'chat-event', payload: %s})"
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    js("[...document.querySelectorAll('[role=tab]')].find((b) => (b.getAttribute('aria-label') || b.textContent).trim() === 'Chat with an agent')?.click()")
    check('suggestions are offered', wait_for("document.querySelectorAll('.chat-suggestion').length >= 2", 15), True)
    first = probe("document.querySelector('.chat-suggestion').textContent")
    js("document.querySelector('.chat-suggestion').click()")
    time.sleep(1)
    check('a suggestion fills the question', probe("document.querySelector('.chat-input textarea').value"), first)
    js("(() => { const t = document.querySelector('.chat-input textarea'); Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(t, ''); t.dispatchEvent(new Event('input', {bubbles: true})); })()")
    js(EMIT % json.dumps({'type': 'tool', 'name': 'entry', 'input': '{"id": 3}'}))
    js(EMIT % json.dumps({'type': 'tool', 'name': 'follow_chain', 'input': '{}'}))
    js(EMIT % json.dumps({'type': 'text', 'text': 'Its data starts at cluster 40, and **clusters 52, 53** hold the rest.'}))
    check('the answer shows', wait_for("(document.querySelector('.chat-log')?.innerText || '').includes('hold the rest')", 15), True)
    check('cluster numbers are links', probe("[...document.querySelectorAll('.chat-cluster')].map((b) => b.textContent)"), ['40', '52'])
    check('tool calls are folded', probe("(() => { const d = document.querySelector('.chat-tools-fold'); return d ? [d.open, d.querySelector('summary').textContent] : null; })()"), [False, '2 tools used'])
    js("[...document.querySelectorAll('.chat-cluster')][0].click()")
    time.sleep(3)
    selection = call('get_selection')
    print('selection after click:', json.dumps(selection)[:400], flush=True)
    check('a cluster link selects that cluster', (selection.get('selection') or {}).get('cluster') == 40 or selection.get('hexCluster') == 40, True)
finally:
    finish()
