"""Window test: the start screen follows the disks (issue #250).

It makes a small synthetic exFAT disk image of its own (`hdiutil create`), starts the app on the start
screen with a list of disks that names only that one (SALVAGE_TEST_STARTUP_DISK_FILE, a file holding the
identifier of the disk the test attached; honoured in a test-hooks build with SALVAGE_TEST_UI), and then
attaches the image, mounts its volume, unmounts it and detaches it, with `diskutil`. Every command names
the identifier that `attach` printed for this image and nothing else; a real disk is never listed, read,
mounted or unmounted.

After each step the start screen must show it within the check interval (10 s), with no button
pressed. It also clicks the app's own Unmount button (the warning the app shows for a volume the person
asked to keep unmounted) and expects the row to change within a few seconds, from the app reading the state
back when the command returns and 1.5 s later, not from the timer. It also checks that the list never says "Looking" while following, that a row that did not
change is the same element afterwards (nothing was drawn again), and that the engine answers `unchanged`
to a look at the fingerprint it gave, which is how a quiet look avoids rebuilding the report.

Needs only hdiutil and diskutil. Saves nothing but the image (follow.dmg) in this folder.
"""
import os, re, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, start, wait_for

IMAGE = os.path.join(SCRATCH, 'follow.dmg')
DISK_FILE = os.path.join(SCRATCH, 'follow-disk.txt')
NAME = 'FOLLOWME'
FOLLOW_SECONDS = 25  # the screen checks every 10 s on its own


def run(*command):
    return subprocess.run(command, capture_output=True, text=True)


def disks_now():
    return set(re.findall(r'^/dev/(disk\d+)\b', run('diskutil', 'list').stdout, re.M))


def row(status=None):
    """The start screen's row for the test volume as {status, same} read from the window, or None."""
    return probe("""(() => {
      const item = [...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r));
      if (!item) return null;
      return {status: item.querySelector('.start-status')?.textContent.trim(), marked: item.dataset.follow === '1'};
    })()""" % NAME)


def status_is(wanted):
    return wait_for("[...document.querySelectorAll('.start-volume')].some((li) => li.textContent.includes(%r) && li.querySelector('.start-status')?.textContent.trim() === %r)" % (NAME, wanted), FOLLOW_SECONDS)


for leftover in (IMAGE, DISK_FILE):
    if os.path.exists(leftover):
        os.remove(leftover)
open(DISK_FILE, 'w').write('')
made = run('hdiutil', 'create', '-size', '8m', '-fs', 'ExFAT', '-volname', NAME, '-type', 'UDIF', IMAGE)
if made.returncode != 0:
    raise SystemExit('could not make the synthetic image: ' + made.stderr)
os.environ['SALVAGE_TEST_STARTUP_DISK_FILE'] = DISK_FILE
before = disks_now()
disk = None
start(sys.argv[1])
try:
    # A watcher that counts how often the heading's button says "Looking", from now on.
    js("window.__looking = 0; new MutationObserver(() => { const b = document.querySelector('.start-heading button'); if (b && b.textContent.trim() === 'Looking') window.__looking += 1; }).observe(document.body, {subtree: true, childList: true, characterData: true});")
    check('with no disk named the list is empty', wait_for("!!document.querySelector('.start-empty')"), True)

    attached = run('diskutil', 'image', 'attach', '--noMount', IMAGE)
    found = re.findall(r'^/dev/(disk\d+)\b', attached.stdout, re.M)
    if attached.returncode != 0 or not found:
        raise SystemExit('could not attach the synthetic image: ' + attached.stdout + attached.stderr)
    disk = found[0]
    if disk in before:
        raise SystemExit('the attached image has the identifier of a disk that was there: ' + disk)
    open(DISK_FILE, 'w').write(disk)
    check('the attached disk appears, not mounted, by itself', status_is('Ready'), True)
    js("[...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r)).dataset.follow = '1'" % NAME)

    mounted = run('diskutil', 'mount', '/dev/%ss1' % disk)
    check('the volume mounts', mounted.returncode, 0)
    check('the screen shows the mount by itself', status_is('Mounted'), True)
    check('the same row is still there, not drawn again', (row() or {}).get('marked'), True)

    unmounted = run('diskutil', 'unmount', '/dev/%ss1' % disk)
    check('the volume unmounts', unmounted.returncode, 0)
    check('the screen shows the unmount by itself (it used to stay "Mounted")', status_is('Ready'), True)

    # The app's own commands: block this volume with its checkbox, then click Unmount in the warning the app
    # shows for a blocked volume that is mounted. The row must change by itself, soon, with no button for
    # Check again, and the app must have read the state back when the command returned and 1.5 s later.
    mounted = run('diskutil', 'mount', '/dev/%ss1' % disk)
    check('the volume mounts again', mounted.returncode, 0)
    check('the screen shows it mounted', status_is('Mounted'), True)
    # The window sends each engine command through fetch (ipc://localhost/engine_call, a JSON body naming the
    # tool), and `invoke` itself cannot be replaced, so the log wraps fetch.
    js("""window.__log = [];
      const original = window.fetch.bind(window);
      window.fetch = (url, options) => {
        let tool = null;
        try { tool = typeof options?.body === 'string' ? JSON.parse(options.body).tool : null; } catch (error) { tool = null; }
        if (!['startup_disks', 'mount_block_unmount', 'mount_block_set', 'mount_blocks_mounted'].includes(tool)) return original(url, options);
        const entry = {tool, start: performance.now()};
        window.__log.push(entry);
        return original(url, options).finally(() => { entry.end = performance.now(); });
      };
      // When the Unmount button of the app appears, and when the test volume's row first says Ready.
      window.__seen = {};
      new MutationObserver(() => {
        if (!window.__seen.banner && document.querySelector('.mounted-warning button.primary')) window.__seen.banner = performance.now();
        const row = [...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r));
        if (window.__seen.unmounting && !window.__seen.ready && row?.querySelector('.start-status')?.textContent.trim() === 'Ready') window.__seen.ready = performance.now();
      }).observe(document.body, {subtree: true, childList: true, characterData: true});""" % NAME)
    check('the row has its Block mount checkbox', wait_for("!![...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r))?.querySelector('.mount-block-inline input')" % NAME, 5), True)
    js("window.__seen.blocking = performance.now(); [...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r)).querySelector('.mount-block-inline input').click()" % NAME)
    check('blocking a mounted volume shows the Unmount button of the app by itself', wait_for("!!window.__seen.banner", 8), True)
    seen = probe("window.__seen")
    log = probe("window.__log")
    read = next((e for e in log if e['tool'] == 'mount_blocks_mounted' and 'end' in e), None)
    print('the Unmount button appeared %.0f ms after the checkbox was clicked; the app asked which volumes are mounted %.0f ms after it, and that read took %.0f ms' % (
        seen['banner'] - seen['blocking'], read['start'] - seen['blocking'], read['end'] - read['start']), flush=True)
    check('the app asked which volumes are mounted as the block command returned, not on its 15 s timer', -50 < read['start'] - seen['blocking'] < 500, True)
    check('and the button appeared as soon as that read answered', 0 <= seen['banner'] - read['end'] < 500, True)
    # Click just after the timer's own look, so the timer (every 10 s) cannot be what reads the state next:
    # any look in the next two seconds is the app reading back after its command.
    js("""window.__log = []; window.__clicked = false;
      (async () => {
        while (!window.__log.some((entry) => entry.tool === 'startup_disks')) await new Promise((resolve) => setTimeout(resolve, 10));
        window.__clicked = true; window.__seen.unmounting = performance.now();
        document.querySelector('.mounted-warning button.primary').click();
      })()""")
    check('the Unmount button was clicked just after the timer', wait_for('window.__clicked === true', 15), True)
    check('the row changes after the Unmount of the app, without Check again', wait_for("!!window.__seen.ready", 12), True)
    time.sleep(2.5)
    log = probe("window.__log")
    seen = probe("window.__seen")
    unmount = next((entry for entry in log if entry['tool'] == 'mount_block_unmount' and 'end' in entry), None)
    check('the unmount command ran and returned', unmount is not None, True)
    after = sorted(entry['start'] - unmount['end'] for entry in log if entry['tool'] == 'startup_disks' and entry['start'] - unmount['end'] > -50)
    print('looks at the disks after the unmount returned, in ms: %s' % [round(x) for x in after], flush=True)
    print('the row said Ready %.0f ms after the unmount command returned' % (seen['ready'] - unmount['end']), flush=True)
    check('the row changed within 2.5 s of the command returning (the timer was 10 s away)', seen['ready'] - unmount['end'] < 2500, True)
    check('the state was read back as the command returned', any(-50 < x < 600 for x in after), True)
    check('and read once more about 1.5 s later', any(1200 < x < 2400 for x in after), True)
    check('there were two reads, and the timer had just run, so neither was the timer', len([x for x in after if x < 2400]) >= 2, True)
    check('the Unmount button is gone', wait_for("!document.querySelector('.mounted-warning')", 5), True)
    js("[...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r)).querySelector('.mount-block-inline input').click()" % NAME)
    check('the block is lifted again', wait_for("![...document.querySelectorAll('.start-volume')].find((li) => li.textContent.includes(%r)).querySelector('.mount-block-inline input').checked" % NAME, 5), True)

    # A quiet look at the fingerprint of the report on screen is answered with nothing to rebuild.
    first = call('startup_disks', includeInternal=False)
    check('a report has a fingerprint', bool(first.get('fingerprint')), True)
    check('and nothing is rebuilt for a look at that fingerprint', call('startup_disks', includeInternal=False, since=first['fingerprint']), {'unchanged': True})

    # The same row survives a quiet stretch with nothing changing: the follower asks, and nothing is drawn.
    time.sleep(8)
    check('a row that did not change is the same element after several checks', (row() or {}).get('marked'), True)

    ejected = run('diskutil', 'eject', '/dev/%s' % disk)
    check('the disk detaches', ejected.returncode, 0)
    disk = None
    check('the screen drops the detached disk by itself', wait_for("!document.querySelector('.start-volume') && !!document.querySelector('.start-empty')", FOLLOW_SECONDS), True)
    check('the list never said "Looking" while following', probe('window.__looking'), 0)
finally:
    finish()
    if disk and disk in disks_now():
        run('diskutil', 'eject', '/dev/%s' % disk)
    for leftover in (IMAGE, DISK_FILE):
        if os.path.exists(leftover):
            os.remove(leftover)
