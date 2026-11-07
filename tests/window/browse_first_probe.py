"""Does Browse show files straight after a disk is opened, or only later?"""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    print('engine first answer:', len(call('browse_files', group='all', limit=50)['rows']), flush=True)
    js("[...document.querySelectorAll('.center-tabs [role=tab]')].find((b) => b.textContent.trim() === 'Browse files').click()")
    for second in (1, 3, 6, 10):
        time.sleep(second - (0 if second == 1 else [1, 3, 6, 10][[1, 3, 6, 10].index(second) - 1]))
        tiles = probe("document.querySelectorAll('.browse-tile, .browse-row, [data-browse-id]').length")
        text = probe("(document.querySelector('.browse')?.innerText || '').slice(0, 120).replace(/\\n/g, ' | ')")
        print(f'after {second}s: tiles={tiles} text={text!r}', flush=True)
finally:
    finish()
