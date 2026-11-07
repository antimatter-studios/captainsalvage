"""How long after launch does the mount block start refusing? A flagged test image is mounted
in a loop from the moment the app starts. Never the real disk."""
import json, os, subprocess, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, renamed_copy
image = os.path.join(SCRATCH, 'racetest.dmg')
subprocess.run(['rm', '-f', image])
subprocess.run(['hdiutil', 'create', '-size', '5m', '-fs', 'ExFAT', '-layout', 'NONE', '-volname', 'RaceTest', image, '-quiet'], check=True)
device = subprocess.run(['hdiutil', 'attach', image, '-nobrowse', '-nomount'], capture_output=True, text=True, check=True).stdout.split()[0]
uuid = [line.split(':', 1)[1].strip() for line in subprocess.run(['diskutil', 'info', device], capture_output=True, text=True).stdout.splitlines() if 'Volume UUID' in line][0]
home = tempfile.mkdtemp(dir=SCRATCH, prefix='racehome.')
folder = os.path.join(home, 'Library/Application Support/Captain Salvage')
os.makedirs(folder)
json.dump([{'uuid': uuid, 'name': 'RaceTest'}], open(os.path.join(folder, 'mount-blocks.json'), 'w'))
program = renamed_copy(sys.argv[1])
env = {'HOME': home, 'USER': os.environ.get('USER', ''), 'PATH': '/usr/bin:/bin:/usr/sbin:/sbin', 'SALVAGE_TEST_HIDDEN': '1'}
child = None
try:
    began = time.time()
    child = subprocess.Popen([program, '-NSAppSleepDisabled', 'YES'], env=env, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    mounted_at = []
    refused = None
    while time.time() - began < 60:
        result = subprocess.run(['diskutil', 'mount', device], capture_output=True, text=True)
        text = result.stdout + result.stderr
        at = time.time() - began
        if 'keeping this volume unmounted' in text:
            refused = at
            break
        if 'mounted' in text.lower() and 'failed' not in text.lower():
            mounted_at.append(round(at, 1))
            subprocess.run(['diskutil', 'unmount', device], capture_output=True)
        time.sleep(0.2)
    print('mounts let through before the block: %d (at %s s)' % (len(mounted_at), mounted_at[:5]), flush=True)
    print('first refusal %.1f s after launch' % refused if refused is not None else 'never refused in 60 s', flush=True)
finally:
    if child:
        child.terminate()
        child.wait(timeout=15)
    subprocess.run(['hdiutil', 'detach', device, '-force', '-quiet'])
    subprocess.run(['rm', '-rf', home])
