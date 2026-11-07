"""Window test: films over 64 MB are tested in pieces; a hole of zeros is found."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, scan_and_reopen, start
start(sys.argv[1], seconds=900)
try:
    scan_and_reopen(os.path.join(SCRATCH, 'filmtest.img'))
    began = time.time()
    started = call('file_tests_start', groups=['videos'])
    print('started:', {k: started.get(k) for k in ('total', 'state')}, flush=True)
    check('both films are picked for testing', started.get('total'), 2)
    for _ in range(300):
        time.sleep(1)
        status = call('file_tests_status')
        if status.get('state') != 'running':
            break
    print('status:', {k: status.get(k) for k in ('state', 'passed', 'damaged', 'bytesDone')}, 'in %.0fs' % (time.time() - began), flush=True)
    results = {}
    for name in ('whole.mp4', 'holed.mp4'):
        rows = call('search', query=name)['rows']
        print(name, 'search:', [(row['name'], row['id']) for row in rows], flush=True)
        entry = next(row['id'] for row in rows if row['name'] == name)
        results[name] = call('file_test', id=entry) or {}
        print(name, results[name].get('outcome'), results[name].get('firstBreak'), results[name].get('note'), flush=True)
    check('the whole film passes', results['whole.mp4'].get('outcome'), 'passed')
    check('the holed film is damaged', results['holed.mp4'].get('outcome'), 'damaged')
    expected_hole = 8 + 16 + 208 + 8 + 40 * 1024 * 1024
    check('the hole is found where it is', results['holed.mp4'].get('firstBreak'), expected_hole)
    check('a full hash is kept', len(results['whole.mp4'].get('sha256') or ''), 64)
finally:
    finish()
