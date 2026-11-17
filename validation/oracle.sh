#!/usr/bin/env bash
# Independent oracle output for the NTFS flows, built by ntfs-3g's tools and the Linux kernel's
# ntfs3 driver. Run as root INSIDE the Linux test VM (scripts/vm-setup.sh installs the tools and
# the ntfs3 run profile boots a kernel that has the driver). Nothing is mounted on the host.
#
#   oracle.sh SHARE
#
# SHARE/in/NAME.img      images the Rust test wrote with the rust-fs-ntfs driver, and for each
#                        an optional NAME.streams of `path<TAB>stream` lines to hash
# SHARE/out/             everything this script produced, rebuilt on every run:
#   tools.txt            kernel and ntfs-3g versions
#   NAME.ntfsfix         `exit=N`, then `ntfsfix -n` output (no action is ever taken)
#   NAME.ntfs3g.tsv      what ntfsls and ntfscat report: `d<TAB>path`, `f<TAB>size<TAB>sha256<TAB>path`,
#                        and `s<TAB>size<TAB>sha256<TAB>path:stream`
#   NAME.ntfs3.tsv       what the kernel's ntfs3, mounted read-only, reports, in the same form
# and, built here by mkntfs and filled through a read-write ntfs3 mount:
#   mkntfs-512.img, mkntfs-4096.img   the images, with the same four files beside each, and
#   NAME.deleted.tsv     `f<TAB>size<TAB>sha256<TAB>path` of files removed before the image was kept
# The two oracles are compared with each other first: a difference between them fails this script,
# because an expectation neither agrees on says nothing about the app.
set -euo pipefail

share="${1:?usage: oracle.sh SHARE}"
in="$share/in"
out="$share/out"
rm -rf "${out:?}"
mkdir -p "$out"
work="$(mktemp -d /var/tmp/ntfsoracle.XXXXXX)"
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
modprobe ntfs3
modprobe loop || true

{
    echo "kernel $(uname -r)"
    ntfsfix --version 2>&1 | head -1
    dpkg-query -W -f='${Package} ${Version}\n' ntfs-3g
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

# ntfs3g_report IMAGE STREAMS_FILE: the tree as ntfs-3g's own tools read it. System files are
# not listed, as a mount does not show them either.
ntfs3g_report() {
    local img="$1" streams="$2"
    walk() {
        local dir="$1" name path size sum
        ntfsls -F -p "$dir" "$img" | while IFS= read -r name; do
            case "$name" in
            ./ | ../) ;;
            */)
                path="${dir%/}/${name%/}"
                printf 'd\t%s\n' "${path#/}"
                walk "$path"
                ;;
            *)
                path="${dir%/}/$name"
                ntfscat "$img" "$path" >"$work/cat"
                size="$(stat -c %s "$work/cat")"
                sum="$(sha256sum "$work/cat")"
                printf 'f\t%s\t%s\t%s\n' "$size" "${sum%% *}" "${path#/}"
                ;;
            esac
        done
    }
    walk /
    if [ -s "$streams" ]; then
        local file stream
        while IFS=$'\t' read -r file stream; do
            ntfscat -n "$stream" "$img" "/$file" >"$work/cat"
            printf 's\t%s\t%s\t%s\n' "$(stat -c %s "$work/cat")" "$(sha256sum "$work/cat" | cut -d' ' -f1)" "$file:$stream"
        done <"$streams"
    fi
}

# ntfs3_report IMAGE: the tree as the kernel's ntfs3 reads it, mounted read-only from a copy.
ntfs3_report() {
    cp --sparse=always "$1" "$work/ro.img"
    loop="$(losetup --find --show --read-only "$work/ro.img")"
    mount -t ntfs3 -o ro "$loop" "$mnt"
    (
        cd "$mnt"
        find . -mindepth 1 -printf '%y\t%P\n' | while IFS=$'\t' read -r kind path; do
            case "$kind" in
            d) printf 'd\t%s\n' "$path" ;;
            f) printf 'f\t%s\t%s\t%s\n' "$(stat -c %s "$path")" "$(sha256sum "$path" | cut -d' ' -f1)" "$path" ;;
            *) printf 'other\t%s\t%s\n' "$kind" "$path" ;;
            esac
        done
    )
    umount "$mnt"
    losetup -d "$loop"
    loop=""
    rm -f "$work/ro.img"
}

# Both oracles on one image, compared with each other before anything else uses them.
record() { # NAME IMAGE STREAMS_FILE
    local name="$1" img="$2" streams="$3" status=0
    {
        ntfsfix -n "$img" >"$work/fix" 2>&1 || status=$?
        echo "exit=$status"
        cat "$work/fix"
    } >"$out/$name.ntfsfix"
    ntfs3g_report "$img" "$streams" | LC_ALL=C sort >"$out/$name.ntfs3g.tsv"
    ntfs3_report "$img" | LC_ALL=C sort >"$out/$name.ntfs3.tsv"
    if ! diff <(grep -v '^s' "$out/$name.ntfs3g.tsv") "$out/$name.ntfs3.tsv" >"$work/diff"; then
        echo "oracle.sh: ntfs-3g and ntfs3 disagree about $name:" >&2
        cat "$work/diff" >&2
        exit 1
    fi
}

shopt -s nullglob
for image in "$in"/*.img; do
    name="$(basename "$image" .img)"
    cp --sparse=always "$image" "$work/$name.img"
    streams="$in/$name.streams"
    [ -f "$streams" ] || streams=/dev/null
    record "$name" "$work/$name.img" "$streams"
    rm -f "$work/$name.img"
done

# Images this script makes: mkntfs formats them and the kernel's ntfs3 fills them, so neither the
# formatter nor the writer is the rust-fs-ntfs driver or the app.
populate() { # CLUSTER_BYTES
    cd "$mnt"
    mkdir -p docs/deep/er many frag
    : >empty
    put small.txt small 100
    put docs/odd.bin odd 12345
    put docs/deep/er/cluster.bin cluster 65536
    put docs/big.bin big 3145728
    put "Mixed Case.Txt" mixed 2222
    put "résumé — naïve.txt" unicode 3333
    for i in $(seq 0 149); do put "many/entry-$(printf %03d "$i").txt" "entry$i" $((10 + i)); done
    # A sparse file: a hole between two blocks of data.
    truncate -s 1M sparse.bin
    prng sparse-head 4096 | dd of=sparse.bin conv=notrunc 2>/dev/null
    prng sparse-tail 4096 | dd of=sparse.bin bs=4096 seek=255 conv=notrunc 2>/dev/null
    # A fragmented file. The volume is filled with small files and every other one of the first
    # hundred removed, so the only free clusters are small and apart and a file written next must
    # be split across them.
    mkdir fill
    i=0
    while dd if=/dev/zero of="fill/f$i" bs=8192 count=1 2>/dev/null; do i=$((i + 1)); done
    sync
    for j in $(seq 0 2 99); do rm "fill/f$j"; done
    sync
    put fragmented.bin fragmented 327680
    # Room for the files removed later, in clusters nothing else wants.
    for j in $(seq 100 109); do rm "fill/f$j"; done
    sync
    cd /
}
for cluster in 512 4096; do
    name="mkntfs-$cluster"
    img="$work/$name.img"
    truncate -s 24M "$img"
    # mkntfs says a lot about a file that is not a disk. What it says is kept for a failure.
    mkntfs -F -Q -s 512 -c "$cluster" -L "MKNTFS $cluster" "$img" >"$work/mkntfs.log" 2>&1 \
        || { cat "$work/mkntfs.log" >&2; exit 1; }
    loop="$(losetup --find --show "$img")"
    mount -t ntfs3 -o rw "$loop" "$mnt"
    populate "$cluster"
    umount "$mnt"
    losetup -d "$loop"
    loop=""
    # Files removed after they were written, for the deleted-file flows. The kernel's ntfs3
    # scrubs a deleted file's name, so these are written and removed through ntfs-3g, which
    # leaves the record as it was but for the in-use flag. Their clusters are not written again.
    ntfs-3g -o rw "$img" "$mnt"
    : >"$out/$name.deleted.tsv"
    put "$mnt/deleted-keepsake.bin" keepsake 20000
    put "$mnt/deleted-small.txt" deletedsmall 300
    sync
    for file in deleted-keepsake.bin deleted-small.txt; do
        printf 'f\t%s\t%s\t%s\n' "$(stat -c %s "$mnt/$file")" "$(sha256sum "$mnt/$file" | cut -d' ' -f1)" "$file" >>"$out/$name.deleted.tsv"
        rm "$mnt/$file"
    done
    sync
    umount "$mnt"
    cp --sparse=always "$img" "$out/$name.img"
    record "$name" "$img" /dev/null
    rm -f "$img"
done
echo "oracle.sh: done"
