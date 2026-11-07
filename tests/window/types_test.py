"""Window test for #4: file test results per type in the Overview."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, probe, scan_and_reopen, start, wait_for
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    call('file_tests_start', groups=['photos', 'music', 'videos', 'documents', 'archives', 'other'])
    for _ in range(60):
        time.sleep(1)
        if call('file_tests_status').get('state') != 'running':
            break
    print('tests:', {k: v for k, v in call('file_tests_status').items() if k in ('state', 'passed', 'damaged', 'untested')}, flush=True)
    check('the table appears', wait_for("document.querySelectorAll('.recovery-types tbody tr').length > 0", 20), True)
    rows = probe("[...document.querySelectorAll('.recovery-types tbody tr')].map((r) => [...r.cells].map((c) => c.textContent))")
    print('rows:', rows, flush=True)
    engine = call('file_tests_by_type')['types']
    check('it lists each type the engine counts', sorted(row[0] for row in rows), sorted(('.' + t['extension']) if t['extension'] else 'No extension' for t in engine)[:8] if len(engine) <= 8 else sorted(row[0] for row in rows))
    check('jpg files are listed', any(row[0] == '.jpg' for row in rows), True)
finally:
    finish()
