"""Window test: a synthetic NTFS image is opened, scanned, browsed, previewed and copied in the real
window (issue #171). The image has folders (docs/deep), a resident note, a picture, a file in two
runs (frag.bin, 20000 bytes whose second part was written after another file took the clusters
beside the first) and two deleted files. The test checks that the volume is called NTFS, that the
Explorer tree opens the root and a folder, that the fragmented file previews with the bytes the
volume holds (read through the captainsalvage:// scheme, the way the window reads a preview), that
the Export window plans and makes a copy of it, and that the copy is byte for byte the file.

Needs ntfs.img (see the README). Copies only into this folder, and removes what it copied. The
volume is never mounted or attached: it is read as a file."""
import hashlib, json, os, shutil, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for

OUT = os.path.join(SCRATCH, 'ntfs-flow-out')
shutil.rmtree(OUT, ignore_errors=True)
os.makedirs(OUT)


def pattern(seed, length):
    """tests/ntfs_support's pattern: seed plus the index modulo 251, a byte at a time."""
    return bytes((seed + index % 251) & 255 for index in range(length))


FRAG = pattern(17, 8192) + pattern(19, 11808)
DOCS_BIG = pattern(7, 40000)


def row_names():
    return probe("[...document.querySelectorAll('button.row .row-name')].map((n) => n.textContent)")


def open_row(name):
    """Opens a folder row of the Explorer tree that is closed."""
    js("[...document.querySelectorAll('button.row')].find((b) => b.getAttribute('aria-expanded') === 'false' "
       "&& b.querySelector('.row-name')?.textContent === %s)?.click()" % json.dumps(name))


start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'ntfs.img'))
    overview = call('volume_overview')
    check('the volume is read as NTFS', overview['filesystem'].get('kind'), 'ntfs')
    check('the scan is complete', (call('status').get('scan') or {}).get('status'), 'complete')

    # Browse: the root, then a folder inside it.
    js("[...document.querySelectorAll('button.row')].find((b) => b.getAttribute('aria-expanded') === 'false' && b.textContent.includes('Volume root'))?.click()")
    check('the root lists its folder and files', wait_for("!![...document.querySelectorAll('button.row .row-name')].find((n) => n.textContent === 'docs')"), True)
    names = row_names()
    check('the root shows frag.bin and pic.png', [n for n in names if n in ('frag.bin', 'pic.png')], ['frag.bin', 'pic.png'])
    open_row('docs')
    check('the folder lists its files', wait_for("!![...document.querySelectorAll('button.row .row-name')].find((n) => n.textContent === 'big.bin')"), True)
    names = row_names()
    check('docs holds big.bin, note.txt and the folder deep', sorted(n for n in names if n in ('big.bin', 'note.txt', 'deep')), ['big.bin', 'deep', 'note.txt'])

    # Preview: the fragmented file, read through the scheme the window uses, in full and in a range.
    entry = next(row for row in call('search', query='frag.bin')['rows'] if row['name'] == 'frag.bin')
    check('the fragmented file is live and 20000 bytes', (entry['length'], entry['inUse'], entry['verdict']), (20000, True, 'live'))
    # The window reads a preview through the engine's range command, which is what is called here.
    def read_range(entry_id, start, length):
        return ("window.__TAURI_INTERNALS__.invoke('read_entry_range', {id: %d, start: %d, length: %d})"
                ".then((b) => new Uint8Array(b))" % (entry_id, start, length))

    def digest_of(entry_id, length):
        return ("(%s).then((b) => crypto.subtle.digest('SHA-256', b))"
                ".then((d) => [...new Uint8Array(d)].map((x) => x.toString(16).padStart(2, '0')).join(''))"
                ".catch((e) => 'error ' + e)" % read_range(entry_id, 0, length))

    check('the preview holds the bytes of both runs', probe(digest_of(entry['id'], 20000)), hashlib.sha256(FRAG).hexdigest())
    check('a range across the join of the two runs reads right',
          probe("(%s).then((b) => [...b]).catch((e) => 'error ' + e)" % read_range(entry['id'], 8000, 400)), list(FRAG[8000:8400]))

    # A file in a folder reads whole the same way.
    big = next(row for row in call('search', query='big.bin')['rows'] if row['name'] == 'big.bin')
    check('a file in a folder has its size', big['length'], len(DOCS_BIG))
    check('a file in a folder reads whole', probe(digest_of(big['id'], len(DOCS_BIG))), hashlib.sha256(DOCS_BIG).hexdigest())

    # Copy: the Export window plans one file and writes it.
    js("[...document.querySelectorAll('button.row')].find((b) => b.querySelector('.row-name')?.textContent === 'frag.bin')"
       "?.dispatchEvent(new MouseEvent('contextmenu', {bubbles: true, cancelable: true, clientX: 200, clientY: 200}))")
    time.sleep(1)
    check('the right-click menu offers Export', probe("[...document.querySelectorAll('.context-menu [role=menuitem]')].map((b) => b.textContent)"), ['Export'])
    js("document.querySelector('.context-menu [role=menuitem]')?.click()")
    check('the Export window opens', wait_for("!!document.querySelector('.export-dialog')"), True)
    js("(() => {const i=document.querySelector('.export-dialog input[spellcheck=\"false\"]');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%s);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));})()" % json.dumps(OUT))
    check('the plan counts one file', wait_for("document.querySelector('.export-dialog .copy-total-line strong')?.textContent.startsWith('1 ')"), True)
    js("document.querySelector('.export-dialog .primary')?.click()")
    check('the copy finishes', wait_for("document.querySelector('.export-dialog .copy-progress strong')?.textContent === 'Copy finished'"), True)
    status = call('copy_status')
    check('one file made, none failed or partial', (status['filesDone'], status['filesFailed'], status['filesPartial']), (1, 0, 0))
    made = os.path.join(OUT, 'frag.bin')
    check('the copy is the file, byte for byte', open(made, 'rb').read() if os.path.isfile(made) else None, FRAG)
    check('nothing else arrived', sorted(n for n in os.listdir(OUT) if not n.endswith('manifest.jsonl')), ['frag.bin'])
finally:
    finish()
    shutil.rmtree(OUT, ignore_errors=True)
