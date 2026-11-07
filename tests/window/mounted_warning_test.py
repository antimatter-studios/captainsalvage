"""Window test: a flagged test image mounted before the app starts is shown on every screen, and
the person's button unmounts it. Only a throwaway image; the real disk is not flagged in this home."""
import json, os, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, start, wait_for
image = os.path.join(SCRATCH, 'warntest.dmg')
subprocess.run(['rm', '-f', image])
subprocess.run(['hdiutil', 'create', '-size', '5m', '-fs', 'ExFAT', '-layout', 'MBRSPUD', '-volname', 'WarnTest', image, '-quiet'], check=True)
attached = subprocess.run(['hdiutil', 'attach', image, '-nobrowse'], capture_output=True, text=True, check=True).stdout
whole = attached.split()[0]
device = [line.split()[0] for line in attached.splitlines() if 'WarnTest' in line][0]
print('whole', whole, 'volume', device, flush=True)
uuid = [line.split(':', 1)[1].strip() for line in subprocess.run(['diskutil', 'info', device], capture_output=True, text=True).stdout.splitlines() if 'Volume UUID' in line][0]
home = tempfile.mkdtemp(dir=SCRATCH, prefix='warnhome.')
folder = os.path.join(home, 'Library/Application Support/Captain Salvage')
os.makedirs(folder)
json.dump([{'uuid': uuid, 'name': 'WarnTest'}], open(os.path.join(folder, 'mount-blocks.json'), 'w'))
os.environ['SALVAGE_TEST_HOME'] = home
def mounted():
    return 'Mounted:                   Yes' in subprocess.run(['diskutil', 'info', device], capture_output=True, text=True).stdout
try:
    check('the test image starts mounted', mounted(), True)
    start(sys.argv[1])
    check('an agent cannot unmount it', 'Only the person' in str(call('mount_block_unmount', uuid=uuid)), True)
    print('blocks:', call('mount_blocks'), flush=True)
    disks = call('startup_disks', includeInternal=True)
    print('startup volumes:', [(v.get('name'), v.get('device'), v.get('uuid'), v.get('mountBlocked'), v.get('mountPoint')) for v in disks.get('volumes', [])], flush=True)
    check('the engine lists it', [item['name'] for item in call('mount_blocks_mounted')['mounted']], ['WarnTest'])
    check('the start screen warns', wait_for("(document.querySelector('.mounted-warning')?.innerText || '').includes('WarnTest is mounted')", 30), True)
    js("document.querySelector('.mounted-warning button').click()")
    check('the warning goes once it is unmounted', wait_for("!document.querySelector('.mounted-warning')", 30), True)
    check('the test image is unmounted', mounted(), False)
    check('it is still attached, not ejected', os.path.exists(device), True)
finally:
    finish()
    subprocess.run(['hdiutil', 'detach', whole, '-force', '-quiet'])
    subprocess.run(['rm', '-rf', home])
