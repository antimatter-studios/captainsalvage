"""Window test: an NTFS volume whose `$Bitmap` cannot be read opens with its allocation unknown, and a torn
`$MFT` record 0 is reported as damage (issue #167).

It makes two copies of the synthetic volume ntfs.img with one MFT record's update-sequence check broken
(record 6, `$Bitmap`; record 0, `$MFT`), and opens each as a person does. For the first it checks that the
volume is not refused: it is read as NTFS; the overview, which the window and the tools both show, says
"allocation unknown" and why; the map has a state of its own for it (no cluster is free or in use); the
unexplained-space total does not count unknown clusters as free; the files are listed; and a file stored in
two runs is read whole, and copied with the right-click Export, byte for byte as in the undamaged volume. For
the second it checks that the damage is told, in words, and that the allocation is still known.

Needs ntfs.img (see the README). Copies only into this folder, and removes what it copied. The volume is
never mounted or attached: it is read as a file."""
import hashlib, json, os, shutil, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for, investigate

OUT = os.path.join(SCRATCH, 'ntfs-unknown-out')
shutil.rmtree(OUT, ignore_errors=True)
os.makedirs(OUT)
SOURCE = os.path.join(SCRATCH, 'ntfs.img')
NO_BITMAP = os.path.join(SCRATCH, 'ntfs-no-bitmap.img')
NO_MFT = os.path.join(SCRATCH, 'ntfs-no-mft-record.img')


def pattern(seed, length):
    return bytes((seed + index % 251) & 255 for index in range(length))


FRAG = pattern(17, 8192) + pattern(19, 11808)


def torn_copy(path, record):
    """A copy of ntfs.img with the check at the end of one MFT record's first sector broken."""
    data = bytearray(open(SOURCE, 'rb').read())
    sector = int.from_bytes(data[0x0B:0x0D], 'little')
    cluster = sector * data[0x0D]
    mft = int.from_bytes(data[0x30:0x38], 'little') * cluster
    data[mft + record * 1024 + 510] ^= 0xFF
    open(path, 'wb').write(data)


torn_copy(NO_BITMAP, 6)
torn_copy(NO_MFT, 0)
start(sys.argv[1])
try:
    scan_and_reopen(NO_BITMAP)
    overview = call('volume_overview')
    check('the volume is not refused: it is read as NTFS', overview['filesystem'].get('kind'), 'ntfs')
    check('the overview says the allocation is unknown', overview.get('allocationKnown'), False)
    damage = overview.get('structureDamage', [])
    check('and why, in words', (len(damage), damage[0].startswith('$Bitmap could not be read (')), (1, True))
    check('every cluster is counted as unknown', overview['allocation'].get('unknown'), overview['geometry']['clusterCount'])
    check('and none as in use or free', (overview['allocation']['allocated'], overview['allocation']['freeZeroFat']), (0, 0))
    check('the scan completes', (call('status').get('scan') or {}).get('status'), 'complete')
    stats = call('region_stats', startCluster=2, endCluster=overview['geometry']['clusterCount'] + 2)
    check('the map counts every cluster as unknown, none free', (stats['allocation']['unknown'], stats['allocation']['freeZeroFat']), (overview['geometry']['clusterCount'], 0))
    summary = call('resolve')
    check('unexplained space does not count unknown clusters as free', summary['unexplainedFreeClusters'], 0)
    check('it is counted apart, so it is not hidden', summary.get('unexplainedUnknownClusters', 0) > 0, True)

    # The window shows it: the overview panel says so, and the map's legend has a state for it.
    investigate()
    js("document.querySelector('.details [role=tab][aria-label=Volume]')?.click()")
    check('the window says allocation unknown', wait_for("document.body.innerText.includes('Allocation unknown')", 20), True)
    check('and why', wait_for("document.body.innerText.includes('$Bitmap could not be read')", 20), True)

    # Files are listed, read and copied as in the undamaged volume.
    js("[...document.querySelectorAll('button.row')].find((b) => b.getAttribute('aria-expanded') === 'false' && b.textContent.includes('Volume root'))?.click()")
    check('the root lists its files', wait_for("!![...document.querySelectorAll('button.row .row-name')].find((n) => n.textContent === 'frag.bin')"), True)
    entry = next(row for row in call('search', query='frag.bin')['rows'] if row['name'] == 'frag.bin')
    check('the fragmented file is live and 20000 bytes', (entry['length'], entry['inUse'], entry['verdict']), (20000, True, 'live'))
    digest = ("window.__TAURI_INTERNALS__.invoke('read_entry_range', {id: %d, start: 0, length: 20000})"
              ".then((b) => crypto.subtle.digest('SHA-256', new Uint8Array(b)))"
              ".then((d) => [...new Uint8Array(d)].map((x) => x.toString(16).padStart(2, '0')).join(''))"
              ".catch((e) => 'error ' + e)" % entry['id'])
    check('the file reads whole, byte for byte', probe(digest), hashlib.sha256(FRAG).hexdigest())
    js("[...document.querySelectorAll('button.row')].find((b) => b.querySelector('.row-name')?.textContent === 'frag.bin')"
       "?.dispatchEvent(new MouseEvent('contextmenu', {bubbles: true, cancelable: true, clientX: 200, clientY: 200}))")
    time.sleep(1)
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


    # The tools that look in free space search the whole volume while allocation is unknown, and say so, in the window
    # and in the tool's answer; the volume's own answers still do not call anything free.
    answer = call('unexplained_runs', limit=20)
    check('the tool says where its free space came from', (answer.get('allocationBasis'), isinstance(answer.get('runs'), list)), ('whole-volume', True))
    check('and what that means', 'Nothing here is known to be free' in answer.get('allocationWarning', ''), True)
    check('carving says it too', call('carve_candidates').get('allocationBasis'), 'whole-volume')
    js("[...document.querySelectorAll('button')].find((b) => b.textContent.trim() === 'Unexplained')?.click()")
    check('the Unexplained list warns that the whole volume is searched', wait_for("document.body.innerText.includes('the whole volume is searched as if it were free space')", 20), True)
    check('and that nothing in it is known to be free', probe("document.body.innerText.includes('Nothing here is known to be free')"), True)
finally:
    finish()
    shutil.rmtree(OUT, ignore_errors=True)

# A torn $MFT record 0 is reported, and the allocation is still known.
start(sys.argv[1])
try:
    scan_and_reopen(NO_MFT)
    overview = call('volume_overview')
    check('a volume with a torn $MFT record 0 opens as NTFS', overview['filesystem'].get('kind'), 'ntfs')
    damage = overview.get('structureDamage', [])
    check('its damage is told, once', (len(damage), damage[0].startswith('$MFT record 0 failed its update-sequence check')), (1, True))
    check('its allocation is known', overview.get('allocationKnown'), None)
    investigate()
    js("document.querySelector('.details [role=tab][aria-label=Volume]')?.click()")
    check('the window tells the damage', wait_for("document.body.innerText.includes('$MFT record 0 failed its update-sequence check')", 20), True)
    check('and does not say the allocation is unknown', probe("document.body.innerText.includes('Allocation unknown')"), False)
finally:
    finish()
    shutil.rmtree(OUT, ignore_errors=True)
    for leftover in (NO_BITMAP, NO_MFT):
        if os.path.exists(leftover):
            os.remove(leftover)
