import os, sys, json
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, finish, probe, scan_and_reopen, start
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    print('types:', json.dumps(call('browse_types'))[:400], flush=True)
    print('all:', len(call('browse_files', group='all', limit=50)['rows']), 'music:', len(call('browse_files', group='music', limit=50)['rows']), 'min0:', len(call('browse_files', group='all', limit=50, minConfidence=0)['rows']), flush=True)
    print('search rows:', [(r['name'], r['verdict'], round(r['confidence'], 2)) for r in call('search', query='.')['rows']][:10], flush=True)
    js_text = probe("document.querySelector('.browse')?.innerText.slice(0, 300) || document.querySelector('.center-panel')?.innerText.slice(0,300)")
    print('browse text:', js_text, flush=True)
finally:
    finish()
