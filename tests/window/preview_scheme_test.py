"""Window test: files reach the window through the captainsalvage:// scheme. A copy of the
jigsaw image gets a real 64 x 64 JPEG in each JPEG cluster, so a preview and a thumbnail decode."""
import os, subprocess, sys
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, probe, scan_and_reopen, start, wait_for

image_path = os.path.join(SCRATCH, 'scheme.img')
ppm, jpg = os.path.join(SCRATCH, 'gradient.ppm'), os.path.join(SCRATCH, 'gradient.jpg')
pixels = bytes(v for y in range(64) for x in range(64) for v in (x * 4, y * 4, 160))
open(ppm, 'wb').write(b'P6\n64 64\n255\n' + pixels)
subprocess.run(['sips', '-s', 'format', 'jpeg', ppm, '--out', jpg], check=True, capture_output=True)
jpeg = open(jpg, 'rb').read()
image = bytearray(open(os.path.join(SCRATCH, 'jigsaw.img'), 'rb').read())
sector_size = 1 << image[108]
cluster_size = sector_size << image[109]
heap_offset = int.from_bytes(image[88:92], 'little') * sector_size
assert len(jpeg) <= cluster_size
for offset in range(heap_offset, len(image), cluster_size):
    if image[offset:offset + 3] == b'\xff\xd8\xff':
        image[offset:offset + cluster_size] = jpeg.ljust(cluster_size, b'\0')
open(image_path, 'wb').write(image)

LOAD = ("new Promise((done) => {const i = new Image(); i.onload = () => done(i.naturalWidth);"
        " i.onerror = () => done('error'); i.src = %r; setTimeout(() => done('timeout'), 15000);})")
start(sys.argv[1])
try:
    scan_and_reopen(image_path)
    entry = next(row for row in call('search', query='a1.jpg')['rows'] if row['name'] == 'a1.jpg')
    check('a file loads through the scheme', probe(LOAD % ('captainsalvage://localhost/entry/%d' % entry['id'])), 64)
    check('a thumbnail loads through the scheme', probe(LOAD % ('captainsalvage://localhost/thumb/%d' % entry['id'])) not in ('error', 'timeout'), True)
    check('the old scheme is gone', probe(LOAD % ('lens://localhost/entry/%d' % entry['id'])) in ('error', 'timeout'), True)
finally:
    finish()
    for path in (image_path, ppm, jpg):
        if os.path.exists(path):
            os.remove(path)
