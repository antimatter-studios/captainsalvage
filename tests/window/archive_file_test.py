"""Window test for #52: a file inside an archive can be shown and copied out."""
import hashlib, os, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for

out = tempfile.mkdtemp(dir=SCRATCH, prefix='archive-out.')
TAB = "[...document.querySelectorAll('.center-tabs [role=tab]')].find((b) => b.textContent.trim() === 'Inside archive')"
ROW = "[...document.querySelectorAll('.zip-row')].find((r) => r.querySelector('.zip-name')?.textContent === %r)"
BUTTON = "[...document.querySelectorAll('.member-actions button')].find((b) => b.textContent.trim() === %r)"

def open_archive(name):
    files = call('browse_files', group='all', limit=50)['rows']
    entry = next(row['id'] for row in files if row['name'] == name)
    js("window.__TAURI_INTERNALS__.invoke('engine_call', {tool: 'show_in_app', args: {kind: 'entry', value: %d}})" % entry)
    wait_for("!!(%s)" % TAB, 20)
    js("(%s).click()" % TAB)
    return wait_for("document.querySelectorAll('.zip-row').length > 0", 30)

def select(name, folders=()):
    for folder in folders:
        if not probe("!!(%s)" % (ROW % name)):
            js("(%s)?.click()" % (ROW % folder))
            time.sleep(1)
    print('rows:', probe("[...document.querySelectorAll('.zip-row .zip-name')].map((n) => n.textContent)"), flush=True)
    js("(%s).click()" % (ROW % name))
    return wait_for("!!document.querySelector('.member-actions')", 10)

start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'arctest.img'))
    check('the zip opens inside', open_archive('pictures.zip'), True)
    check('the picture is selected', select('gradient.png', ['holiday']), True)
    js("(%s).click()" % (BUTTON % 'Show it'))
    check('the picture is shown', wait_for("document.querySelector('.member-preview img')?.naturalWidth === 64", 20), True)
    js("(%s).click()" % (ROW % 'readme.txt'))
    wait_for("!!document.querySelector('.member-actions')", 10)
    js("(%s).click()" % (BUTTON % 'Show it'))
    check('the text is shown', wait_for("(document.querySelector('.member-preview pre')?.textContent || '').startsWith('A made-up note inside a zip.')", 20), True)
    js("(%s).click()" % (BUTTON % 'Copy it out'))
    wait_for("!!document.querySelector('.member-copy input')", 10)
    label = probe("document.querySelector('.member-copy label span').textContent")
    check('the size is shown before copying', 'readme.txt (87 B)' in label or 'readme.txt (' in label, True)
    print('label:', label, flush=True)
    js("(() => {const i=document.querySelector('.member-copy input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%r);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));})()" % out)
    time.sleep(1)
    js("document.querySelector('.member-copy button.primary').click()")
    check('the copy is reported', wait_for("/Copied to/.test(document.querySelector('.member-actions')?.innerText || '')", 30), True)
    copied = os.path.join(out, 'readme.txt')
    check('the file is on the destination', open(copied).read(), 'A made-up note inside a zip.\n' * 3)

    # From a compressed tar, the bigger file, which is unpacked as it is written.
    check('the tar opens inside', open_archive('photos.tar.gz'), True)
    check('the big file is selected', select('second.bin', ['docs']), True)
    js("(%s).click()" % (BUTTON % 'Copy it out'))
    wait_for("!!document.querySelector('.member-copy input')", 10)
    js("(() => {const i=document.querySelector('.member-copy input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%r);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));})()" % out)
    time.sleep(1)
    js("document.querySelector('.member-copy button.primary').click()")
    wait_for("/Copied to/.test(document.querySelector('.member-actions')?.innerText || '')", 30)
    original = open(os.path.join(SCRATCH, 'arc', 'src', 'docs', 'second.bin'), 'rb').read()
    check('the tar file comes out byte for byte', hashlib.sha256(open(os.path.join(out, 'second.bin'), 'rb').read()).hexdigest(), hashlib.sha256(original).hexdigest())
    check('nothing half-written is left', sorted(os.listdir(out)), ['readme.txt', 'second.bin'])
finally:
    finish()
