"""Window test: a live file with no bytes is exported from the right-click menu, and an empty file
arrives. The Explorer row offers Export for it (it used to be hidden), the Export window plans one
file, and the copy makes it empty, beside nothing that was deleted. Needs emptyfiles.img (see the
README). Copies only into this folder."""
import json, os, shutil, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
OUT = os.path.join(SCRATCH, 'empty-export-out')
shutil.rmtree(OUT, ignore_errors=True)
os.makedirs(OUT)
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'emptyfiles.img'))
    rows = call('search', query='empty.txt')['rows']
    live = [row for row in rows if row['name'] == 'empty.txt']
    check('the scan lists the live empty file', [(r['length'], r['inUse'], r['verdict']) for r in live], [(0, True, 'empty')])
    check('a deleted empty file is listed as deleted', [r['inUse'] for r in rows if r['name'] == 'gone-empty.txt'], [False])
    # Open the volume root in the Explorer tree, then right-click the empty file's row.
    js("[...document.querySelectorAll('button.row')].find((b) => b.getAttribute('aria-expanded') === 'false' && b.textContent.includes('Volume root'))?.click()")
    wait_for("!![...document.querySelectorAll('button.row .row-name')].find((n) => n.textContent === 'empty.txt')")
    js("[...document.querySelectorAll('button.row')].find((b) => b.querySelector('.row-name')?.textContent === 'empty.txt')"
       "?.dispatchEvent(new MouseEvent('contextmenu', {bubbles: true, cancelable: true, clientX: 200, clientY: 200}))")
    time.sleep(1)
    check('the right-click menu on the empty file offers Export', probe("[...document.querySelectorAll('.context-menu [role=menuitem]')].map((b) => b.textContent)"), ['Export'])
    js("document.querySelector('.context-menu [role=menuitem]')?.click()")
    check('the Export window opens', wait_for("!!document.querySelector('.export-dialog')"), True)
    js("(() => {const i=document.querySelector('.export-dialog input[spellcheck=\"false\"]');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%s);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(OUT))
    check('the plan counts one file', wait_for("document.querySelector('.export-dialog .copy-total-line strong')?.textContent.startsWith('1 ')"), True)
    check('Export is offered for one file', probe("[...document.querySelectorAll('.export-dialog .primary')].map((b) => [b.textContent, b.disabled])"), [['Export 1 files', False]])
    js("document.querySelector('.export-dialog .primary')?.click()")
    check('the copy finishes', wait_for("document.querySelector('.export-dialog .copy-progress strong')?.textContent === 'Copy finished'"), True)
    status = call('copy_status')
    check('one file made, none failed or partial', (status['filesDone'], status['filesFailed'], status['filesPartial']), (1, 0, 0))
    made = os.path.join(OUT, 'empty.txt')
    check('an empty file arrived', (os.path.isfile(made), os.path.getsize(made) if os.path.isfile(made) else None), (True, 0))
    check('nothing else arrived', sorted(n for n in os.listdir(OUT) if not n.endswith('manifest.jsonl')), ['empty.txt'])
finally:
    finish()
    shutil.rmtree(OUT, ignore_errors=True)
