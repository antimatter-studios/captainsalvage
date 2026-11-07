"""#7: clicks through every centre view and details tab on synthetic images, recording page
errors, failed tool calls shown as errors, and empty panels. Never the real disk."""
import json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
TABS = ['Selected', 'Rebuild', 'Chat with an agent', 'Treasure list', 'Hypotheses', 'Lens', 'Volume', 'Activity', 'Connect']
CENTRE = ['Overview', 'Browse files', 'Bytes', 'Inside archive']
INSTALL = """(() => { window.__errors = []; window.addEventListener('error', (e) => window.__errors.push(String(e.message))); window.addEventListener('unhandledrejection', (e) => window.__errors.push('rejected: ' + String(e.reason))); })()"""
SHOWN_ERRORS = "[...document.querySelectorAll('.error, .detail-error, .bad, [role=alert]')].filter((n) => n.offsetParent !== null).map((n) => n.textContent.trim()).filter(Boolean)"
start(sys.argv[1], seconds=900)
problems = []
try:
    for image in ('jigsaw.img',):
        scan_and_reopen(os.path.join(SCRATCH, image))
        js(INSTALL)
        files = call('browse_files', group='all', limit=50)['rows'] or call('search', query='.mp3')['rows']
        print(image, 'files:', len(files), flush=True)
        archive = next((row for row in files if row['name'].endswith(('.zip', '.tar.gz'))), None)
        target = archive or files[0]
        js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'entry', value: %d}})" % target['id'])
        time.sleep(2)
        js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'cluster', value: %d}})" % max(target['firstCluster'], 2))
        time.sleep(2)
        js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'entry', value: %d}})" % target['id'])
        time.sleep(2)
        for name in CENTRE:
            enabled = probe("(() => { const b = [...document.querySelectorAll('.center-tabs [role=tab]')].find((b) => b.textContent.trim() === %s); if (!b || b.disabled) return false; b.click(); return true; })()" % json.dumps(name))
            time.sleep(3)
            shown = probe(SHOWN_ERRORS)
            body = probe("(document.querySelector('.center-stack, .center-panel')?.innerText || '').trim().length")
            print(f'{image} centre {name}: enabled={enabled} text={body} errors={shown}', flush=True)
            if enabled and shown:
                problems.append((image, name, shown))
        for name in TABS:
            probe("(() => { const b = [...document.querySelectorAll('[role=tab]')].find((b) => (b.getAttribute('aria-label') || b.textContent).trim() === %s); b?.click(); return !!b; })()" % json.dumps(name))
            time.sleep(3)
            shown = probe(SHOWN_ERRORS)
            body = probe("(document.querySelector('.details .panel-body:not([hidden]), .details-agent:not([hidden]), .details')?.innerText || '').trim().length")
            print(f'{image} tab {name}: text={body} errors={shown}', flush=True)
            if shown:
                problems.append((image, name, shown))
        errors = probe("window.__errors")
        print(f'{image} page errors:', errors, flush=True)
        if errors:
            problems.append((image, 'page', errors))
    check('no screen showed an error', problems, [])
finally:
    finish()
