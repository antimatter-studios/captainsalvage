#!/usr/bin/env bash
# Independent oracle output for the FAT12, FAT16, FAT32 and exFAT flows. mkfs.fat and mkfs.exfat
# format the images, the Linux kernel's vfat and exfat drivers fill them through a read-write loop
# mount, and the same kernel drivers (and, for FAT, mtools) read them back. Run as root INSIDE the
# Linux test VM (scripts/vm-setup.sh installs the tools). Nothing is mounted on the host.
#
#   fat-oracle.sh SHARE
#
# SHARE/out/             everything this script produced, rebuilt on every run:
#   tools.txt            kernel, dosfstools, exfatprogs and mtools versions
#   cases.tsv            one line per image: name, kind, sector bytes, cluster bytes, byte offset of
#                        the volume inside the image, volume bytes, and whether mtools also read it
#   NAME.img             the image: the formatter's and the kernel's work, not the app's
#   NAME.fsck            `exit=N`, then fsck.fat -n or fsck.exfat -n output (nothing is repaired)
#   NAME.kernel.tsv      what the kernel, mounted read-only, reports: `d<TAB>path` and
#                        `f<TAB>size<TAB>sha256<TAB>path`
#   NAME.mtools.tsv      the same from mtools, for the FAT images it can read
#   NAME.deleted.tsv     `f<TAB>size<TAB>sha256<TAB>path` of files written last and then removed
# and, for a few images, damaged copies (validation/damage.py):
#   NAME-RECIPE.img      the damaged copy, with NAME-RECIPE.fsck
#   damage.tsv           one line per damaged copy: name, parent, recipe, the files it can make
#                        read wrong (`*` for any)
# The oracles are compared with each other first: a difference between the kernel and mtools fails
# this script, because an expectation neither agrees on says nothing about the app.
set -euo pipefail

share="${1:?usage: fat-oracle.sh SHARE}"
here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="$share/out"
rm -rf "${out:?}"
mkdir -p "$out"
work="$(mktemp -d /var/tmp/fatoracle.XXXXXX)"
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
modprobe vfat || true
modprobe exfat || true
for filesystem in vfat exfat; do
    grep -qw "$filesystem" /proc/filesystems || {
        echo "fat-oracle.sh: the running kernel has no $filesystem driver; chore test:oracle boots the Linux test VM that has it (scripts/vm-setup.sh)" >&2
        exit 1
    }
done

{
    echo "kernel $(uname -r)"
    dpkg-query -W -f='${Package} ${Version}\n' dosfstools exfatprogs mtools
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

# attach IMAGE OFFSET SIZE SECTOR [ro]: a loop device over the volume inside the image, whose
# logical sector size is SECTOR. exfatprogs and the kernel take the sector size from the block
# device, so a 4096-byte-sector image is made on a loop device of 4096-byte sectors, and the size
# the device ended up with is checked: a silent 512 would make a 512-byte image under a 4096 name.
attach() {
    local args=(--offset "$2" --sizelimit "$3" --sector-size "$4") got
    [ "${5:-}" = ro ] && args+=(--read-only)
    loop="$(losetup --find --show "${args[@]}" "$1")"
    got="$(blockdev --getss "$loop")"
    if [ "$got" != "$4" ]; then
        echo "fat-oracle.sh: $1 was attached with sector size $got, not $4 (case ${case_name:-?})" >&2
        exit 1
    fi
}
detach() {
    losetup -d "$loop"
    loop=""
}

# tree_report DIR: `d` and `f` lines for everything below DIR.
tree_report() {
    (
        cd "$1"
        find . -mindepth 1 -printf '%y\t%P\n' | while IFS=$'\t' read -r kind path; do
            case "$kind" in
            d) printf 'd\t%s\n' "$path" ;;
            f) printf 'f\t%s\t%s\t%s\n' "$(stat -c %s "$path")" "$(sha256sum "$path" | cut -d' ' -f1)" "$path" ;;
            *) printf 'other\t%s\t%s\n' "$kind" "$path" ;;
            esac
        done
    ) | LC_ALL=C sort
}

# mount_options KIND: how the kernel is asked to show names. shortname=winnt shows a name that is
# all lower case as such, which is what the flags a kernel or Windows writes mean.
mount_options() {
    case "$1" in
    exfat) echo "" ;;
    *) echo "iocharset=utf8,shortname=winnt" ;;
    esac
}
mount_type() { case "$1" in exfat) echo exfat ;; *) echo vfat ;; esac; }

# populate CLUSTER_BYTES: the tree the kernel writes into a new volume, then two files written
# last and removed, so their clusters stay as they were.
populate() {
    local cs="$1" i n=24
    cd "$mnt"
    put readme.txt readme 11
    put lower.txt lower 1500
    put UPPER.TXT upper 700
    put mixed.TXT mixedbase 33
    put UPEXT.txt upperext 44
    put ntlower.txt ntlower 21
    put ntbase.TXT ntbase 22
    put NTEXT.txt ntext 23
    : >empty.dat
    put "Mixed Case.Txt" mixedcase 2222
    put "Ünïcödé — résumé with a very long name.txt" unicode 3333
    put "a name that is much longer than eight dot three but not unicode.data" longname $((cs * 3 + 17))
    put exact.bin exact $((cs * 4))
    put onebyte.bin onebyte 1
    put deepfile.bin deep $((cs * 4 + 5))
    mkdir -p docs/nested/deeper many
    put docs/guide.txt guide 5000
    put docs/nested/deep.bin deeper $((cs * 6 + 5))
    put "docs/nested/Long Nested Name.txt" nestedlong 1234
    put docs/nested/deeper/leaf.bin leaf $((cs * 2))
    # Fragmentation: two files grown one cluster at a time, in turn, so each cluster of one sits
    # between clusters of the other.
    put "$work/fa.src" fraga $((cs * n))
    put "$work/fb.src" fragb $((cs * n))
    : >fraga.bin
    : >fragb.bin
    for ((i = 0; i < n; i++)); do
        dd if="$work/fa.src" of=fraga.bin bs="$cs" skip="$i" seek="$i" count=1 conv=notrunc status=none
        dd if="$work/fb.src" of=fragb.bin bs="$cs" skip="$i" seek="$i" count=1 conv=notrunc status=none
    done
    rm -f "$work/fa.src" "$work/fb.src"
    # Enough long-named files in one folder to need several clusters of entries.
    for ((i = 0; i < 120; i++)); do
        printf 'entry %04d\n' "$i" >"$(printf 'many/entry file with a long name %04d.txt' "$i")"
    done
    sync
    put gone-big.bin gonebig 40000
    put gone-small.txt gonesmall 300
    sync
    for file in gone-big.bin gone-small.txt; do
        printf 'f\t%s\t%s\t%s\n' "$(stat -c %s "$file")" "$(sha256sum "$file" | cut -d' ' -f1)" "$file" >>"$deleted"
        rm "$file"
    done
    sync
    cd "$work"
}

# fsck_record KIND DEVICE FILE: what the formatter's own checker says, with nothing repaired.
fsck_record() {
    local status=0
    {
        case "$1" in
        exfat) fsck.exfat -n "$2" >"$work/fsck" 2>&1 || status=$? ;;
        *) fsck.fat -n "$2" >"$work/fsck" 2>&1 || status=$? ;;
        esac
        echo "exit=$status"
        cat "$work/fsck"
    } >"$3"
}

# One damaged copy per recipe, each checked by the formatter's own checker (which must object, or
# the recipe damaged nothing the checker looks at: the Rust test asserts it).
damage() { # NAME IMAGE KIND SECTOR RECIPES...
    local name="$1" img="$2" kind="$3" sector="$4" recipe touched
    shift 4
    for recipe in "$@"; do
        cp --sparse=always "$img" "$work/damaged.img"
        touched="$(python3 "$here/damage.py" "$work/damaged.img" "$recipe" | cut -f2)"
        attach "$work/damaged.img" 0 "$(stat -c %s "$work/damaged.img")" "$sector" ro
        fsck_record "$kind" "$loop" "$out/$name-$recipe.fsck"
        detach
        cp --sparse=always "$work/damaged.img" "$out/$name-$recipe.img"
        rm -f "$work/damaged.img"
        printf '%s-%s\t%s\t%s\t%s\n' "$name" "$recipe" "$name" "$recipe" "$touched" >>"$out/damage.tsv"
    done
}

# build NAME KIND SECTOR CLUSTER SIZE_MIB OFFSET_MIB
build() {
    local name="$1" kind="$2" sector="$3" cluster="$4" size_mib="$5" offset_mib="$6"
    local img="$work/$name.img" offset size total mtools=0 type
    case_name="$name"
    offset=$((offset_mib * 1048576))
    size=$((size_mib * 1048576))
    total=$((offset + size))
    # A volume inside a disk leaves a megabyte after it, as a partitioned disk does.
    if [ "$offset" -gt 0 ]; then total=$((total + 1048576)); fi
    deleted="$out/$name.deleted.tsv"
    : >"$deleted"
    truncate -s "$total" "$img"
    attach "$img" "$offset" "$size" "$sector"
    case "$kind" in
    exfat)
        mkfs.exfat -L ORACLE -c "$cluster" "$loop" >"$work/mkfs.log" 2>&1 || {
            cat "$work/mkfs.log" >&2
            exit 1
        }
        type=7
        ;;
    *)
        local hidden=()
        [ "$offset" -gt 0 ] && hidden=(-h $((offset / 512)))
        mkfs.fat -I -F "${kind#fat}" -S "$sector" -s $((cluster / sector)) -n ORACLE -i CA550001 "${hidden[@]}" "$loop" >"$work/mkfs.log" 2>&1 || {
            cat "$work/mkfs.log" >&2
            exit 1
        }
        case "$kind" in fat12) type=1 ;; fat16) type=e ;; *) type=c ;; esac
        # mtools reads what is on 512-byte sectors.
        [ "$sector" = 512 ] && mtools=1
        ;;
    esac
    local options
    options="$(mount_options "$kind")"
    mount -t "$(mount_type "$kind")" ${options:+-o "$options"} "$loop" "$mnt"
    populate "$cluster"
    umount "$mnt"
    detach
    if [ "$offset" -gt 0 ]; then
        # The partition table is the oracle's: sfdisk writes it, and the app reads it back.
        printf 'label: dos\nstart=%s, size=%s, type=%s\n' "$((offset / 512))" "$((size / 512))" "$type" \
            | sfdisk --quiet "$img" >"$work/sfdisk.log" 2>&1 || {
            cat "$work/sfdisk.log" >&2
            exit 1
        }
    fi
    attach "$img" "$offset" "$size" "$sector" ro
    fsck_record "$kind" "$loop" "$out/$name.fsck"
    mount -t "$(mount_type "$kind")" -o "ro${options:+,$options}" "$loop" "$mnt"
    tree_report "$mnt" >"$out/$name.kernel.tsv"
    umount "$mnt"
    detach
    if [ "$mtools" = 1 ]; then
        rm -rf "${work:?}/mtools"
        mkdir "$work/mtools"
        MTOOLS_SKIP_CHECK=1 LC_ALL=C.UTF-8 mcopy -i "$img@@$offset" -s -n -m ::/ "$work/mtools"/
        tree_report "$work/mtools" >"$out/$name.mtools.tsv"
        rm -rf "${work:?}/mtools"
        if ! diff "$out/$name.kernel.tsv" "$out/$name.mtools.tsv" >"$work/diff"; then
            echo "fat-oracle.sh: the kernel and mtools disagree about $name:" >&2
            head -20 "$work/diff" >&2
            exit 1
        fi
    fi
    printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$name" "$kind" "$sector" "$cluster" "$offset" "$size" "$mtools" >>"$out/cases.tsv"
    cp --sparse=always "$img" "$out/$name.img"
    case "$name" in
    fat12-512 | fat16-512 | fat32-512)
        damage "$name" "$img" "$kind" "$sector" fat-zeroed chain-cut chain-loop chain-crosslinked differing-fats
        ;;
    exfat-512)
        damage "$name" "$img" "$kind" "$sector" chain-cut chain-loop chain-crosslinked entry-checksum
        ;;
    esac
    rm -f "$img"
}

: >"$out/cases.tsv"
: >"$out/damage.tsv"
#     name          kind   sector cluster size_mib offset_mib
build fat12-512     fat12  512    512     2        0
build fat12-big     fat12  512    16384   60       0
build fat12-4096    fat12  4096   4096    8        0
build fat16-512     fat16  512    1024    32       0
build fat16-4096    fat16  4096   4096    64       0
build fat16-offset  fat16  512    1024    32       1
build fat32-512     fat32  512    512     64       0
build fat32-offset  fat32  512    512     40       1
build fat32-4096    fat32  4096   4096    272      0
build exfat-512     exfat  512    4096    64       0
build exfat-4096    exfat  4096   4096    64       0
build exfat-offset  exfat  512    4096    32       1
echo "fat-oracle.sh: done"
