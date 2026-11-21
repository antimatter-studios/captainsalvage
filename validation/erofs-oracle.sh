#!/usr/bin/env bash
# Independent oracle output for the EROFS flows. mkfs.erofs makes the images from a tree and the Linux kernel's
# erofs driver, mounted read-only on a loop device, reads them back. Run as root INSIDE the Linux test
# VM (scripts/vm-setup.sh installs the tools). Nothing is mounted on the host.
#
#   erofs-oracle.sh SHARE
#
# SHARE/out/             everything this script produced, rebuilt on every run:
#   tools.txt            the kernel and erofs-utils versions
#   cases.tsv            one line per image: name, mkfs.erofs options, image bytes
#   NAME.img             the image: mkfs.erofs's work, not the app's
#   NAME.kernel.tsv      what the kernel reports: `d<TAB>path`, `f<TAB>size<TAB>sha256<TAB>path` and
#                        `l<TAB>target<TAB>path` for a symbolic link
# One image per layout the app is held to: uncompressed, LZ4 and LZ4HC compressed (with the tail of a file packed into its inode too), and chunk-based files. A tool that
# cannot build one of them fails this script, so the run never quietly checks fewer.
set -euo pipefail

share="${1:?usage: erofs-oracle.sh SHARE}"
out="$share/out"
rm -rf "${out:?}"
mkdir -p "$out"
work="$(mktemp -d /var/tmp/erofsoracle.XXXXXX)"
mnt="$work/mnt"
mkdir "$mnt"
loop=""
cleanup() {
    cd /
    if mountpoint -q "$mnt"; then umount "$mnt" || true; fi
    if [ -n "$loop" ]; then losetup -d "$loop" 2>/dev/null || true; fi
    rm -rf "${work:?}"
}
trap cleanup EXIT

modprobe loop || true
modprobe erofs || true
grep -qw erofs /proc/filesystems || {
    echo "erofs-oracle.sh: the running kernel has no erofs driver; chore test:oracle boots the Linux test VM that has it (scripts/vm-setup.sh)" >&2
    exit 1
}
{
    echo "kernel $(uname -r)"
    dpkg-query -W -f='${Package} ${Version}\n' erofs-utils
} >"$out/tools.txt"

# prng SEED BYTES: deterministic bytes, so a rebuilt image is identical.
prng() {
    local key
    key="$(printf '%s' "$1" | sha256sum | cut -c1-32)"
    set +o pipefail
    openssl enc -aes-128-ctr -K "$key" -iv 00000000000000000000000000000000 </dev/zero 2>/dev/null | head -c "$2"
    set -o pipefail
}
put() { prng "$2" "$3" >"$1"; }

# text SEED BYTES: bytes that compress well, so the compressed blocks are smaller than the data.
text() {
    prng "$1" 4096 | base64 -w 76 | head -c 4096 | sed 's/[+\/=]/ /g' | tr -d '\n' | head -c 4096 >"$work/text.unit"
    : >"$2"
    local written=0 want="$3"
    while [ "$written" -lt "$want" ]; do
        cat "$work/text.unit" >>"$2"
        written=$((written + 4096))
    done
    truncate -s "$want" "$2"
}

# The tree every image holds: files that are one fragment, several blocks and a tail, a file that
# compresses and one that does not, sparse stretches, an empty file, symbolic links, and names that
# are long and not ASCII.
src="$work/src"
mkdir -p "$src/docs/nested/deeper" "$src/many"
(
    cd "$src"
    put readme.txt readme 11
    put one-block.bin oneblock 100000
    put two-blocks-and-a-tail.bin twotail 300000
    put exact-blocks.bin exact 262144
    text compressible "compressible.txt" 700000
    : >empty.dat
    put onebyte.bin onebyte 1
    put "Mixed Case.Txt" mixedcase 2222
    put "Ünïcödé — résumé with a very long name.txt" unicode 3333
    put "a name that is much longer than eight dot three but not unicode and goes on for a while.data" longname 4444
    put docs/guide.txt guide 5000
    put docs/nested/deep.bin deeper 131075
    put docs/nested/deeper/leaf.bin leaf 77
    # A file of zeros between two stretches of data.
    { prng sparsehead 5000; head -c 393216 /dev/zero; prng sparsetail 777; } >sparse.bin
    # Enough entries in one folder for the directory to need several blocks.
    for ((i = 0; i < 400; i++)); do
        printf 'entry %04d\n' "$i" >"$(printf 'many/entry file with a long name %04d.txt' "$i")"
    done
    ln -s readme.txt link-to-file
    ln -s docs/nested link-to-folder
    ln -s ../../readme.txt docs/nested/up-link
)

# tree_report DIR: `d`, `f` and `l` lines for everything below DIR .
tree_report() {
    (
        cd "$1"
        find . -mindepth 1 -printf '%y\t%P\n' | while IFS=$'\t' read -r kind path; do
            case "$kind" in
            d) printf 'd\t%s\n' "$path" ;;
            f) printf 'f\t%s\t%s\t%s\n' "$(stat -c %s "$path")" "$(sha256sum "$path" | cut -d' ' -f1)" "$path" ;;
            l) printf 'l\t%s\t%s\n' "$(readlink "$path")" "$path" ;;
            *) printf 'other\t%s\t%s\n' "$kind" "$path" ;;
            esac
        done
    ) | LC_ALL=C sort
}

# build NAME OPTIONS...
build() {
    local name="$1" img="$work/$1.img"
    shift
    mkfs.erofs "$@" -T 0 -U 11111111-2222-3333-4444-555555555555 "$img" "$src" >/dev/null
    loop="$(losetup --find --show --read-only "$img")"
    mount -t erofs -o ro "$loop" "$mnt"
    tree_report "$mnt" >"$out/$name.kernel.tsv"
    umount "$mnt"
    losetup -d "$loop"
    loop=""
    cp "$img" "$out/$name.img"
    printf '%s\t%s\t%s\n' "$name" "$*" "$(stat -c %s "$img")" >>"$out/cases.tsv"
}

build plain
build chunked --chunksize=16384
build lz4 -zlz4
build lz4hc -zlz4hc,9 -C 16384
build lz4hc-tailpacking -zlz4hc -Eztailpacking
echo "erofs-oracle: $(wc -l <"$out/cases.tsv") images, $(wc -l <"$out/plain.kernel.tsv") entries each"
