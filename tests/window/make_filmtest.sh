#!/bin/sh
# Builds filmtest.img: an exFAT image with two made-up 70 MB MP4 films, one whole and one
# with a hole of zeros in its media data, for testing large files in pieces.
set -e
cd "$(dirname "$0")"
rm -rf films && mkdir films
python3 - <<'EOF'
import os, random, struct
random.seed(11)
def box(kind, body):
    return struct.pack('>I', len(body) + 8) + kind + body
def film(path, hole):
    media = bytearray(random.getrandbits(8) for _ in range(1024 * 1024)) * 70
    for i in range(0, len(media), 4093):
        if media[i] == 0:
            media[i] = 1
    if hole:
        media[40 * 1024 * 1024:40 * 1024 * 1024 + 256 * 1024] = bytes(256 * 1024)
    data = box(b'ftyp', b'isom\0\0\x02\0isomiso2') + box(b'moov', b'\x01' * 200) + box(b'mdat', bytes(media))
    open(path, 'wb').write(data)
film('films/whole.mp4', False)
film('films/holed.mp4', True)
EOF
rm -f filmtest.dmg filmtest.cdr filmtest.img
hdiutil create -size 200m -fs ExFAT -layout NONE -volname FilmTest filmtest.dmg -quiet
ATTACHED=$(hdiutil attach filmtest.dmg -nobrowse)
M=$(echo "$ATTACHED" | awk '/FilmTest/ {print $NF}')
DEV=$(echo "$ATTACHED" | awk '/^\/dev\/disk/ {print $1; exit}')
cp films/*.mp4 "$M/"
ls -la "$M"
hdiutil detach "$DEV" -quiet
hdiutil convert filmtest.dmg -format UDTO -o filmtest -quiet && mv filmtest.cdr filmtest.img && rm -f filmtest.dmg
rm -rf films
ls -la filmtest.img
