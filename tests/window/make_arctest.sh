#!/bin/sh
# Builds arctest.img: a small exFAT image of archives, for window tests. Synthetic data only.
set -e
cd "$(dirname "$0")"
rm -rf arc && mkdir -p arc/src/docs arc/pics
python3 - <<'EOF'
import os, tarfile, gzip, random, zipfile, zlib, struct
random.seed(7)
os.chdir('arc')
open('src/docs/first.txt','w').write('the first file, which is short\n')
open('src/docs/second.bin','wb').write(bytes(random.getrandbits(8) for _ in range(150_000)))
for mode, name in [('w:gz','photos.tar.gz'),('w:bz2','music.tar.bz2'),('w:xz','backup.tar.xz'),('w','plain.tar')]:
    with tarfile.open(name, mode) as t: t.add('src/docs', arcname='docs')
open('notes.txt.gz','wb').write(gzip.compress(b'hello from a gzip file\n'*100))
def png(width, height):
    rows = b''.join(b'\x00' + b''.join(bytes((x * 255 // width, y * 255 // height, 160)) for x in range(width)) for y in range(height))
    def chunk(kind, data):
        return struct.pack('>I', len(data)) + kind + data + struct.pack('>I', zlib.crc32(kind + data) & 0xffffffff)
    return b'\x89PNG\r\n\x1a\n' + chunk(b'IHDR', struct.pack('>IIBBBBB', width, height, 8, 2, 0, 0, 0)) + chunk(b'IDAT', zlib.compress(rows)) + chunk(b'IEND', b'')
with zipfile.ZipFile('pictures.zip', 'w', zipfile.ZIP_DEFLATED) as z:
    z.writestr('holiday/gradient.png', png(64, 48))
    z.writestr('holiday/readme.txt', 'A made-up note inside a zip.\n' * 3)
EOF
rm -f arctest.dmg arctest.cdr arctest.img
hdiutil create -size 30m -fs ExFAT -layout NONE -volname ArcTest arctest.dmg -quiet
ATTACHED=$(hdiutil attach arctest.dmg -nobrowse)
M=$(echo "$ATTACHED" | awk '/ArcTest/ {print $NF}')
DEV=$(echo "$ATTACHED" | awk '/^\/dev\/disk/ {print $1; exit}')
cp arc/*.tar* arc/*.gz arc/*.zip "$M/"
ls "$M"
hdiutil detach "$DEV" -quiet
hdiutil convert arctest.dmg -format UDTO -o arctest -quiet && mv arctest.cdr arctest.img && rm -f arctest.dmg
ls -la arctest.img
