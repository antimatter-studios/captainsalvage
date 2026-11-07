"""Does the mount block refuse a flagged test image now? Never the real disk."""
import os, subprocess, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, start
image = os.path.join(SCRATCH, 'blocktest.dmg')
subprocess.run(['rm', '-f', image])
subprocess.run(['hdiutil', 'create', '-size', '5m', '-fs', 'ExFAT', '-layout', 'NONE', '-volname', 'BlockTest', image, '-quiet'], check=True)
attached = subprocess.run(['hdiutil', 'attach', image, '-nobrowse', '-nomount'], capture_output=True, text=True, check=True).stdout
device = attached.split()[0]
uuid = [line.split(':', 1)[1].strip() for line in subprocess.run(['diskutil', 'info', device], capture_output=True, text=True).stdout.splitlines() if 'Volume UUID' in line][0]
print('test image', device, uuid, flush=True)
try:
    began = time.time()
    start(sys.argv[1])
    print('app ready after %.1fs' % (time.time() - began), flush=True)
    print('set:', call('mount_block_set', uuid=uuid, name='BlockTest', blocked=True), flush=True)
    result = subprocess.run(['diskutil', 'mount', device], capture_output=True, text=True)
    print('mount said:', (result.stdout + result.stderr).strip()[:160], flush=True)
    mounted = 'Mounted: Yes' in subprocess.run(['diskutil', 'info', device], capture_output=True, text=True).stdout
    check('the flagged image is refused', mounted, False)
finally:
    finish()
    subprocess.run(['hdiutil', 'detach', device, '-force', '-quiet'])
