"""Window test: a test copy of the app never offers or accepts a real drive for copies. The Copy
to field starts empty, no /Volumes folder is listed, and a folder outside the test's own is refused.
Nothing is copied: only plans and checks are asked for."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start
TAB = "[...document.querySelectorAll('.inspector-tabs [role=tab]')].find((b) => b.getAttribute('aria-label') === %r)?.click()"
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    volumes = call('list_volumes')
    check('no /Volumes folder is offered', [v['path'] for v in volumes if v['path'].startswith('/Volumes/')], [])
    js(TAB % 'Treasure list')
    time.sleep(2)
    check('the window shows no /Volumes path', probe("document.body.innerText.includes('/Volumes/')"), False)
    outside = call('copy_plan', destination='/Users/Shared')
    check('a folder outside the test is refused', 'Window tests may only copy' in str(outside.get('refused')), True)
    inside = call('copy_plan', destination=SCRATCH)
    check('the test folder is allowed', 'Window tests may only copy' in str(inside.get('refused')), False)
    checked = call('copy_check', destination='/Users/Shared')
    check('checking copies outside the test is refused', 'Window tests may only copy' in str(checked), True)
finally:
    finish()
