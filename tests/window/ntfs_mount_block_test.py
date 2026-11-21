"""Window test: an NTFS volume that macOS gives no UUID is kept unmounted by where it is (issue #171).

macOS gives a synthetic NTFS volume no `Volume UUID`, so the Block mount checkbox of the start screen
must name the volume another way: the identity of the disk it is on, and the offset and length of its
partition. This builds throwaway disk images of its own (`hdiutil create`, then the synthetic NTFS
volume `ntfs.img` written into partition 1 of each, with `dd`, on the attached image's own node), one
with an MBR partition map and one with a GPT, and for each:

- the volume is listed as NTFS with no UUID, and the row offers the Block mount checkbox;
- a volume that is mounted when it is blocked is shown as mounted anyway, and the app's own Unmount
  button, which only the person can press, unmounts it;
- once it is blocked, `diskutil mount` is refused, and the volume is still not mounted;
- the image is ejected and attached again, as macOS does by itself when a disk is plugged in (under a
  new disk number, mounted by default): the volume is still not mounted, so the block knows the volume
  by where it is and not by its disk number;
- another disk of the same size and layout, with another NTFS volume on it, mounts as usual, so the
  block holds that disk only;
- on the start screen the attached volume's checkbox is checked, the person's click clears the block,
  and then the volume mounts again.

Every command names the identifier that `attach` printed for an image of this test and nothing else; a
real disk is never listed, read, mounted or unmounted. Needs ntfs.img (see the README), hdiutil and
diskutil. Saves nothing but its images, in this folder, and removes them.
"""
import os, re, shutil, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, start, wait_for

NTFS = os.path.join(SCRATCH, 'ntfs.img')
OTHER = os.path.join(SCRATCH, 'ntfs-block-other.img')
DISK_FILE = os.path.join(SCRATCH, 'ntfs-block-disk.txt')
NAME = 'SALVAGE'  # the volume name inside ntfs.img
# (layout, where partition 1 starts on the disk)
CASES = [('MBRSPUD', 512), ('GPTSPUD', 1048576)]
attached = []  # the disks this test attached, to eject whatever is left
made = []


def run(*command):
    return subprocess.run(command, capture_output=True, text=True)


def disks_now():
    return set(re.findall(r'^/dev/(disk\d+)\b', run('diskutil', 'list').stdout, re.M))


def info(device):
    return run('diskutil', 'info', device).stdout


def is_mounted(device):
    return re.search(r'^\s*Mounted:\s+Yes', info(device), re.M) is not None


def field(device, name):
    found = re.search(r'^\s*%s:\s*(.*)$' % re.escape(name), info(device), re.M)
    return found.group(1).strip() if found else None


def attach(image, mount):
    """Attaches an image of this test and returns the identifier of its whole disk. Mounted by macOS
    itself (not shown in Finder) or not at all."""
    before = disks_now()
    command = ['hdiutil', 'attach', image, '-nobrowse'] if mount else ['hdiutil', 'attach', image, '-nomount']
    result = run(*command)
    found = re.findall(r'^/dev/(disk\d+)\b', result.stdout, re.M)
    if not found:
        raise SystemExit('could not attach %s: %s %s' % (image, result.stdout, result.stderr))
    if found[0] in before:
        raise SystemExit('the attached image has the identifier of a disk that was there: ' + found[0])
    attached.append(found[0])
    return found[0]


def attach_refused(image):
    """Attaches the image as macOS does by itself, and mounts its volume. When every volume of an image is
    refused its mount, `hdiutil attach` fails with "no mountable file systems" and detaches the image
    again. Returns that failure, or None when the image was attached (which is then ejected)."""
    before = disks_now()
    result = run('hdiutil', 'attach', image, '-nobrowse')
    new = sorted(disks_now() - before, key=lambda name: int(name[4:]))
    for disk in new:
        eject(disk)
    if result.returncode == 0:
        return None
    return {'said': (result.stdout + result.stderr).strip(), 'left attached': new}


def eject(disk):
    run('diskutil', 'eject', '/dev/%s' % disk)
    if disk in attached:
        attached.remove(disk)


def make_image(layout, path, volume):
    """A 24 MB image with a partition map of this layout, with the NTFS volume `volume` (a file) written
    into partition 1."""
    if os.path.exists(path):
        os.remove(path)
    created = run('hdiutil', 'create', '-size', '24m', '-fs', 'ExFAT', '-layout', layout, '-volname', 'BLOCKME', path, '-quiet')
    if created.returncode != 0:
        raise SystemExit('could not make a synthetic image: ' + created.stderr)
    made.append(path)
    disk = attach(path, mount=False)
    run('diskutil', 'unmountDisk', '/dev/%s' % disk)
    written = run('dd', 'if=%s' % volume, 'of=/dev/r%ss1' % disk, 'bs=512')
    if written.returncode != 0:
        raise SystemExit('could not write the NTFS volume into the image: ' + written.stderr)
    eject(disk)


def row_js(body):
    return "(() => { const li = [...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r)); %s })()" % (NAME, body)


def row_there():
    return wait_for(row_js('return !!li;'), 60)


def volume_in_report():
    report = call('startup_disks', includeInternal=False)
    return next((volume for volume in report['volumes'] if volume['name'] == NAME), None)


def mounts_within(device, seconds):
    """Whether the volume gets mounted within this long."""
    end = time.time() + seconds
    while time.time() < end:
        if is_mounted(device):
            return True
        time.sleep(0.5)
    return False


for leftover in (OTHER, DISK_FILE):
    if os.path.exists(leftover):
        os.remove(leftover)
if not os.path.exists(NTFS):
    raise SystemExit('ntfs.img is missing: see the README for how to make it')
# Another NTFS volume of the same size: only its serial number differs.
data = bytearray(open(NTFS, 'rb').read())
data[0x48:0x50] = bytes.fromhex('0f1e2d3c4b5a6978')
open(OTHER, 'wb').write(data)
open(DISK_FILE, 'w').write('')
os.environ['SALVAGE_TEST_STARTUP_DISK_FILE'] = DISK_FILE
start(sys.argv[1])
try:
    for layout, offset in CASES:
        print('== %s' % layout, flush=True)
        image = os.path.join(SCRATCH, 'ntfs-block-%s.dmg' % layout.lower())
        other = os.path.join(SCRATCH, 'ntfs-block-%s-other.dmg' % layout.lower())
        make_image(layout, image, NTFS)
        make_image(layout, other, OTHER)

        # 1. The volume is listed as NTFS with no UUID, and the row offers the checkbox.
        disk = attach(image, mount=False)
        partition = '/dev/%ss1' % disk
        open(DISK_FILE, 'w').write(disk)
        check('%s: the attached volume is on the start screen' % layout, row_there(), True)
        found = volume_in_report()
        check('%s: it is read as NTFS' % layout, found['format'], 'NTFS')
        check('%s: macOS gives it no volume UUID' % layout, found['uuid'], None)
        key = found['mountBlockKey']
        check('%s: it is named by its place' % layout, (key or '').startswith('PLACE:'), True)
        check('%s: the place says where the partition starts' % layout, key.split('@')[1].split('+')[0], str(offset))
        check('%s: the row offers Block mount' % layout, probe(row_js("return !!li?.querySelector('.mount-block-inline input');")), True)
        check('%s: nothing is blocked yet' % layout, [block['uuid'] for block in call('mount_blocks')['blocks']], [])

        # 2. Blocked while mounted: shown as mounted anyway, and the person's Unmount button unmounts it.
        mounted = run('diskutil', 'mount', partition)
        check('%s: it mounts before it is blocked' % layout, mounted.returncode, 0)
        js(row_js("li.querySelector('.mount-block-inline input').click();"))
        check('%s: the checkbox blocks it' % layout, wait_for(row_js("return !!li?.querySelector('.mount-block-inline input')?.checked;"), 10), True)
        blocks = call('mount_blocks')['blocks']
        check('%s: one block is saved, by place' % layout, [block['uuid'] for block in blocks], [key])
        check('%s: with the partition\'s offset and length' % layout, (blocks[0]['placement']['offset'], blocks[0]['placement']['length']), (offset, int(field(partition, 'Disk Size').split('(')[1].split(' ')[0])))
        check('%s: the volume is reported blocked' % layout, volume_in_report()['mountBlocked'], True)
        check('%s: it is listed as mounted anyway' % layout, [item['uuid'] for item in call('mount_blocks_mounted')['mounted']], [key])
        check('%s: the app shows the Unmount warning' % layout, wait_for("!!document.querySelector('.mounted-warning button.primary')", 20), True)
        js("document.querySelector('.mounted-warning button.primary').click()")
        check('%s: the person\'s Unmount button unmounts it' % layout, wait_for("!document.querySelector('.mounted-warning')", 30), True)
        check('%s: it is not mounted' % layout, is_mounted(partition), False)

        # 3. A mount is refused while the app is open.
        refused = run('diskutil', 'mount', partition)
        print('mount said:', (refused.stdout + refused.stderr).strip()[:160], flush=True)
        check('%s: diskutil mount is refused' % layout, refused.returncode != 0, True)
        check('%s: and the volume is not mounted' % layout, is_mounted(partition), False)

        # 4. Ejected and attached again, which mounts it by default under a new disk number: still refused.
        eject(disk)
        open(DISK_FILE, 'w').write('')
        check('%s: the screen drops the ejected disk' % layout, wait_for(row_js('return !li;'), 30), True)
        refused = attach_refused(image)
        print('attached again, macOS said:', refused, flush=True)
        check('%s: attached again, macOS is refused the mount of the volume' % layout, bool(refused) and 'no mountable file systems' in refused['said'], True)
        check('%s: and nothing is left mounted under its name' % layout, [line for line in run('mount').stdout.splitlines() if '/Volumes/%s ' % NAME in line], [])

        # 5. Another disk of the same layout and size, with another NTFS volume, mounts as usual.
        lookalike = attach(other, mount=True)
        check('%s: another disk of the same size mounts as usual' % layout, mounts_within('/dev/%ss1' % lookalike, 30), True)
        run('diskutil', 'unmount', '/dev/%ss1' % lookalike)
        eject(lookalike)

        # 6. The block is still there, on the screen, for the volume under its new disk number, and
        #    the person's click lets it mount again.
        disk = attach(image, mount=False)
        partition = '/dev/%ss1' % disk
        open(DISK_FILE, 'w').write(disk)
        check('%s: the volume is on the screen again' % layout, row_there(), True)
        check('%s: its checkbox is checked, so the block knows it by its place' % layout, wait_for(row_js("return !!li?.querySelector('.mount-block-inline input')?.checked;"), 10), True)
        check('%s: the report says blocked' % layout, volume_in_report()['mountBlocked'], True)
        check('%s: still refused' % layout, run('diskutil', 'mount', partition).returncode != 0, True)
        js(row_js("li.querySelector('.mount-block-inline input').click();"))
        check('%s: the person\'s click clears the block' % layout, wait_for(row_js("return !li?.querySelector('.mount-block-inline input')?.checked;"), 10), True)
        check('%s: nothing is blocked' % layout, call('mount_blocks')['blocks'], [])
        mounted = run('diskutil', 'mount', partition)
        check('%s: the volume mounts again' % layout, mounted.returncode, 0)
        check('%s: and is mounted' % layout, is_mounted(partition), True)
        run('diskutil', 'unmount', partition)
        eject(disk)
        open(DISK_FILE, 'w').write('')
finally:
    finish()
    for disk in list(attached):
        run('diskutil', 'unmountDisk', 'force', '/dev/%s' % disk)
        eject(disk)
    for leftover in made + [OTHER, DISK_FILE]:
        if os.path.exists(leftover):
            os.remove(leftover)
