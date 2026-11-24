"""Window test: the change journal and $LogFile of an NTFS volume are shown in the window (issue #169).

The image is a synthetic NTFS volume whose `$Extend\\$UsnJrnl` holds 28 records: a file made, written, closed, renamed,
another deleted, and twenty more made. Its `$LogFile` is the one a formatted volume has, with nothing to replay. The test opens and
scans it, finds the panel in the volume's facts, checks that nothing is read until the button is pressed, then that the
evidence is labelled as a separate interpretation, lists the records with their reasons by name, says the log has nothing to replay,
and that the image's hash is the same afterwards (the volume is only ever read).

Needs ntfs-journal.img (see the README). The volume is never mounted or attached: it is read as a file."""
import hashlib, os, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, investigate, js, probe, scan_and_reopen, start, wait_for

IMAGE = os.path.join(SCRATCH, 'ntfs-journal.img')


def sha(path):
    digest = hashlib.sha256()
    with open(path, 'rb') as handle:
        for block in iter(lambda: handle.read(1 << 20), b''):
            digest.update(block)
    return digest.hexdigest()


before = sha(IMAGE)
start(sys.argv[1])
try:
    scan_and_reopen(IMAGE)
    check('the volume is read as NTFS', call('volume_overview')['filesystem'].get('kind'), 'ntfs')
    investigate()
    js("document.querySelector('[role=tab][aria-label=Volume]')?.click()")
    check('the panel is in the volume facts', wait_for("!!document.querySelector('.ntfs-journal button')"), True)
    check('nothing is listed before the button is pressed', probe("document.querySelectorAll('.ntfs-journal table').length"), 0)
    js("document.querySelector('.ntfs-journal button')?.click()")
    check('the change journal lists its records', wait_for("document.querySelectorAll('.ntfs-journal .journal-usn tbody tr').length === 28"), True)
    check('the evidence is labelled and says it is read only',
          probe("document.querySelector('.ntfs-journal .hint')?.textContent.startsWith('NTFS change journal and $LogFile evidence. Read only')"), True)
    names = probe("[...document.querySelectorAll('.ntfs-journal .journal-usn tbody tr')].map((r) => r.children[2].textContent)")
    check('the first records are the first file\'s', names[:6], ['report.txt'] * 4 + ['report-final.txt'] * 2)
    reasons = probe("[...document.querySelectorAll('.ntfs-journal .journal-usn tbody tr')].map((r) => r.children[4].textContent)")
    check('a made file says FILE_CREATE', reasons[0], 'FILE_CREATE')
    check('a rename says both of its names', (reasons[3], reasons[4]), ('RENAME_OLD_NAME', 'RENAME_NEW_NAME'))
    check('a deleted file says FILE_DELETE', reasons[6], 'FILE_DELETE')
    check('the journal is said to be active', probe("[...document.querySelectorAll('.ntfs-journal p')].some((p) => p.textContent === 'The change journal is active.')"), True)
    log_said = probe("[...document.querySelectorAll('.ntfs-journal p')].map((p) => p.textContent).filter((t) => t.startsWith('$LogFile'))")
    check('the log with nothing in it is said to hold nothing to replay (empty, or clean)',
          [t for t in log_said if t in ('$LogFile is empty.', '$LogFile holds nothing to replay.')] != [], True)
    print('the log said:', log_said)
    check('no more records are offered once all are read', probe("document.querySelectorAll('.ntfs-journal button').length"), 0)
    check('the volume is the same bytes afterwards', sha(IMAGE), before)
finally:
    finish()
