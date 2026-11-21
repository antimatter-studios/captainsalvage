"""Window test: a FAT volume is read through the FAT driver's shared layers, and an entry the driver found by its
content only is a weak candidate (the generic adapter of #290).

It opens a synthetic FAT16 image with a long-named file, a folder with a child, a deleted entry, a cluster nobody points
to holding one lone entry (ORPHAN.TXT) and another holding two (TWO.TXT, THREE.TXT), and a cluster of file bytes that
reads like one entry. It checks that the overview says the generic adapter reads the volume and tells of the damage the
driver found in the FAT; that a search shows the weak candidates with a mark in the window, and the tool says why (`weak`,
and a reason that says the driver found it by its content); that the entry a folder names has no mark; and that the
details of a weak candidate say how it was found. Entries in a cluster that is free in the FAT are deleted.

Needs fat.img (see the README). Never mounts or attaches anything."""
import json, os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, investigate, js, probe, scan_and_reopen, start, wait_for


def search(query):
    """Types a search into the Explorer's box, as a person does, and returns the rows it shows."""
    js("(() => {const i=document.querySelector('.sidebar-search input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%s);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));setTimeout(()=>i.form.requestSubmit(),200);})()" % json.dumps(query))
    wait_for("!![...document.querySelectorAll('button.row .row-name')].find((n) => n.textContent.startsWith(%s))" % json.dumps(query.split('.')[0]))
    return probe("[...document.querySelectorAll('button.row')].map((b) => ({name: b.querySelector('.row-name')?.firstChild?.textContent, weak: b.querySelector('.row-weak')?.textContent ?? null, title: b.querySelector('.row-weak')?.title ?? null}))")


start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'fat.img'))
    overview = call('volume_overview')
    check('the volume is read as FAT16', overview['filesystem'].get('variant'), 'FAT16')
    check('by the generic adapter', overview.get('fatAdapter'), 'generic')
    damage = overview.get('structureDamage', [])
    check('the damage the driver found in the FAT is told', bool(damage) and all(text.startswith('FAT structure damage found by the driver: ') for text in damage), True)
    check('the scan is complete', (call('status').get('scan') or {}).get('status'), 'complete')

    lone = next(row for row in call('search', query='ORPHAN.TXT')['rows'] if row['name'] == 'ORPHAN.TXT')
    check('the lone entry is a weak candidate, said so by the tool', lone.get('weak'), 'lone_entry')
    check('it is deleted: its cluster is free in the FAT', lone['inUse'], False)
    check('and its reason says how the driver found it', 'found this entry by its content' in lone['reason'], True)
    two = next(row for row in call('search', query='TWO.TXT')['rows'] if row['name'] == 'TWO.TXT')
    check('an entry of a cluster of two is weak, unchecked', two.get('weak'), 'unchecked')
    named = next(row for row in call('search', query='CHILD.TXT')['rows'] if row['name'] == 'CHILD.TXT')
    check('an entry a folder names has no mark', 'weak' in named, False)
    check('and is in use', named['inUse'], True)

    deleted = next(row for row in call('search', query='_OST.TXT')['rows'] if row['name'] == '_OST.TXT')
    check('a deleted file the FAT cleared the chain of is read whole, as a guess', (deleted['inUse'], deleted.get('guessed'), deleted['clustersFound'], deleted['clustersNeeded']), (False, True, 2, 2))
    check('and its reason says so', 'are a guess' in deleted['reason'], True)

    rows = search('ORPHAN.TXT')
    check('the window marks the weak candidate', [(r['name'], r['weak']) for r in rows if r['name'] == 'ORPHAN.TXT'], [('ORPHAN.TXT', 'Weak')])
    title = next(r['title'] for r in rows if r['name'] == 'ORPHAN.TXT')
    check('and its tooltip says why', 'only entry in its cluster' in title, True)
    rows = search('CHILD.TXT')
    check('the entry a folder names has no mark in the window', [(r['name'], r['weak']) for r in rows if r['name'] == 'CHILD.TXT'], [('CHILD.TXT', None)])

    # The details that say how an entry was found are the Investigate view's.
    investigate()
    search('ORPHAN.TXT')
    js("[...document.querySelectorAll('button.row')].find((b) => b.querySelector('.row-name')?.firstChild?.textContent === 'ORPHAN.TXT')?.click()")
    check('the details say how it was found', wait_for("document.body.innerText.includes('Its content only, a lone entry')", 20), True)
    check('and that it is marked deleted', wait_for("document.body.innerText.includes('Marked deleted')", 5), True)
    search('_OST.TXT')
    js("[...document.querySelectorAll('button.row')].find((b) => b.querySelector('.row-name')?.firstChild?.textContent === '_OST.TXT')?.click()")
    check('the details of a guessed file say its layout is a guess', wait_for("document.body.innerText.includes('A guess: one run from its first cluster')", 20), True)
finally:
    finish()
