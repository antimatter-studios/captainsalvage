"""Window test for #19: the legend lists only what the map shows, and keeps read errors."""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, js, probe, scan_and_reopen, start, wait_for
FULL_CONTENT = ['Compressed or media data', 'Text', 'Audio start', 'Image start', 'Video start', 'Document or archive start', 'Binary', 'exFAT directory', 'Empty', 'Not scanned yet', 'Read error']
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    js("[...document.querySelectorAll('.strips [role=tab]')].find((b) => b.textContent.trim() === 'Content').click()")
    time.sleep(3)
    shown = probe("[...document.querySelectorAll('.strips .legend .legend-item')].map((i) => i.textContent.trim())")
    print('content legend:', shown, flush=True)
    check('it lists fewer than every category', 0 < len(shown) < len(FULL_CONTENT), True)
    check('everything it lists is a content category', all(item in FULL_CONTENT for item in shown), True)
    check('read errors stay listed', 'Read error' in shown, True)
    check('a finished scan leaves nothing unscanned', 'Not scanned yet' in shown, False)
    js("[...document.querySelectorAll('.strips [role=tab]')].find((b) => b.textContent.trim() === 'Allocation').click()")
    time.sleep(3)
    allocation = probe("[...document.querySelectorAll('.strips .legend .legend-item')].map((i) => i.textContent.trim())")
    print('allocation legend:', allocation, flush=True)
    check('switching layer lists that layer', all(item not in FULL_CONTENT for item in allocation) and len(allocation) > 0, True)
finally:
    finish()
