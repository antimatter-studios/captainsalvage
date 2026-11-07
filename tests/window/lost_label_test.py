"""Window test for #17: a lost folder shows a guessed label from its files, and keeps its cluster."""
import os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, check, finish, probe, scan_and_reopen, start, wait_for
start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    wait_for("document.querySelectorAll('.explorer-rows .row').length > 1", 20)
    guessed = probe("[...document.querySelectorAll('.explorer-rows .row-name-guessed')].map((n) => n.textContent)")
    print('guessed labels:', guessed, flush=True)
    check('the lost folder of music is labelled', 'Music, 1 file' in guessed, True)
    row = probe("(() => { const n = [...document.querySelectorAll('.explorer-rows .row-name-guessed')].find((n) => n.textContent === 'Music, 1 file'); const r = n.closest('.row'); return {sub: r.querySelector('.row-path')?.textContent, title: n.title}; })()")
    print('row:', row, flush=True)
    check('its cluster name stays beside it', row['sub'], 'Lost folder 40')
    check('it says it is a guess', 'guess' in row['title'], True)
    check('it is shown in italics', probe("getComputedStyle(document.querySelector('.row-name-guessed')).fontStyle"), 'italic')
finally:
    finish()
