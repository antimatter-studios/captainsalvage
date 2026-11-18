"""Window test: a test copy of the app never offers or accepts a real drive for copies. The Copy
to field starts empty, no /Volumes folder is listed, and a folder outside the test's own is refused.
The Export window, opened from a file's right-click menu, shows no /Volumes path either.
Nothing is copied: only plans and checks are asked for."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    volumes = call('list_volumes')
    check('no /Volumes folder is offered', [v['path'] for v in volumes if v['path'].startswith('/Volumes/')], [])
    js("[...document.querySelectorAll('.center-tabs [role=tab]')].find((b) => b.textContent.trim() === 'Browse files')?.click()")
    time.sleep(3)
    js("document.querySelector('.tile-wrap, .list-row')?.dispatchEvent(new MouseEvent('contextmenu', {bubbles: true, cancelable: true, clientX: 200, clientY: 200}))")
    time.sleep(1)
    check('the right-click menu offers Export', probe("[...document.querySelectorAll('.context-menu [role=menuitem]')].map((b) => b.textContent)"), ['Export'])
    js("document.querySelector('.context-menu [role=menuitem]')?.click()")
    time.sleep(3)
    check('the Export window opens', probe("!!document.querySelector('.export-dialog')"), True)
    check('the window shows no /Volumes path', probe("document.body.innerText.includes('/Volumes/')"), False)
    file = call('browse_files', group='all', limit=1)['rows'][0]
    outside = call('export_plan', kind='file', target=file['id'], destination='/Users/Shared')
    check('a folder outside the test is refused', 'Window tests may only copy' in str(outside.get('refused')), True)
    inside = call('export_plan', kind='file', target=file['id'], destination=SCRATCH)
    check('the test folder is allowed', 'Window tests may only copy' in str(inside.get('refused')), False)
    checked = call('copy_check', destination='/Users/Shared')
    check('checking copies outside the test is refused', 'Window tests may only copy' in str(checked), True)
finally:
    finish()
