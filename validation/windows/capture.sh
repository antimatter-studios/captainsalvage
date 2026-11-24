#!/usr/bin/env bash
# Have Windows format and fill NTFS volumes, and bring their images and manifests back.
#
#   capture.sh OUT [NAME...]
#
# OUT/NAME.img            the volume, as Windows left it
# OUT/NAME.manifest.tsv   what Windows reported about it (make-volume.ps1 explains the lines)
# OUT/NAME.info.txt       Windows version, geometry and chkdsk's verdict
#
# NAMEs are from volumes.json; with none, every volume is made.
#
# Where Windows runs:
#   default            the Windows guest of fs-windows-test-harness (QEMU, `fswth-local`), over ssh and scp.
#                      The guest is started if it is not running, when the harness checkout is found.
#   FSWTH_LOCAL=1      this machine is Windows (a hosted runner), and Windows PowerShell runs here.
#
# Environment: FSWTH_SSH_CONFIG (default: the harness's ssh config), FSWTH_HARNESS (the harness
# checkout, default ../fs-windows-test-harness), FSWTH_WORKDIR (guest folder, default
# C:/fswth/captainsalvage-win-oracle).
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
out="${1:?usage: capture.sh OUT [NAME...]}"
shift || true
mkdir -p "$out"
out="$(cd "$out" && pwd)"

state="${XDG_DATA_HOME:-$HOME/.local/share}/fs-windows-test-harness/arm64"
config="${FSWTH_SSH_CONFIG:-$state/ssh_config}"
harness="${FSWTH_HARNESS:-$here/../../../fs-windows-test-harness}"
workdir="${FSWTH_WORKDIR:-C:/fswth/captainsalvage-win-oracle}"
local_windows="${FSWTH_LOCAL:-0}"

fail() {
    echo "capture: $*" >&2
    exit 1
}

python="$(command -v python3 || command -v python || true)"
[ -n "$python" ] || fail "python 3 is needed to read volumes.json"

# What the specs say, as `name sizeMB clusterBytes sectorBytes style scale` lines. Python on Windows ends its lines with CRLF,
# and the carriage return would stay on the last field (`small\r` is not in make-volume.ps1's ValidateSet), so it is dropped.
specs() {
    "$python" -I - "$here/volumes.json" "$@" <<'PY' | tr -d '\r'
import json, sys
wanted = set(sys.argv[2:])
rows = json.load(open(sys.argv[1]))
known = {row["name"] for row in rows}
missing = wanted - known
if missing:
    sys.exit("unknown volume(s): " + ", ".join(sorted(missing)))
for row in rows:
    if not wanted or row["name"] in wanted:
        print(row["name"], row["sizeMB"], row["clusterBytes"], row["sectorBytes"], row["partitionStyle"], row["scale"])
PY
}

if [ "$local_windows" != 1 ]; then
    [ -f "$config" ] || fail "no ssh config for the Windows guest at $config. 'chore windows:up' starts it."
    guest() { ssh -n -F "$config" fswth-local "$1"; }
    if ! guest 'echo up' >/dev/null 2>&1; then
        if [ -f "$harness/scripts/local-vm.py" ]; then
            echo "capture: starting the Windows guest" >&2
            python3 "$harness/scripts/local-vm.py" up >&2
            python3 "$harness/scripts/local-vm.py" wait >&2
        fi
        guest 'echo up' >/dev/null 2>&1 || fail "the Windows guest does not answer. 'chore windows:up' starts it."
    fi
fi

if [ "$local_windows" = 1 ]; then
    script="$(cygpath -w "$here/make-volume.ps1")"
    stage="$out/stage"
    mkdir -p "$stage"
    specs "$@" | while read -r name size cluster sector style scale; do
        echo "capture: $name ($size MiB, $cluster-byte clusters, $sector-byte sectors)" >&2
        powershell.exe -NoProfile -ExecutionPolicy Bypass -File "$script" -Name "$name" -SizeMB "$size" \
            -ClusterBytes "$cluster" -SectorBytes "$sector" -PartitionStyle "$style" -Scale "$scale" \
            -Out "$(cygpath -w "$stage")" </dev/null >&2 || fail "make-volume.ps1 failed for $name"
        gunzip -c "$stage/$name.img.gz" >"$out/$name.img"
        mv "$stage/$name.manifest.tsv" "$stage/$name.info.txt" "$out/"
        rm -f "$stage/$name.img.gz"
    done
    rmdir "$stage" 2>/dev/null || true
    exit 0
fi

guest "New-Item -ItemType Directory -Force '$workdir/out' | Out-Null"
scp -F "$config" -q "$here/make-volume.ps1" "fswth-local:$workdir/make-volume.ps1"
specs "$@" | while read -r name size cluster sector style scale; do
    echo "capture: $name ($size MiB, $cluster-byte clusters, $sector-byte sectors)" >&2
    guest "Remove-Item -Force '$workdir/out/$name.*' -ErrorAction SilentlyContinue; powershell -NoProfile -ExecutionPolicy Bypass -File '$workdir/make-volume.ps1' -Name $name -SizeMB $size -ClusterBytes $cluster -SectorBytes $sector -PartitionStyle $style -Scale $scale -Out '$workdir/out'; if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }" >&2 ||
        fail "make-volume.ps1 failed for $name"
    for ext in img.gz manifest.tsv info.txt; do
        scp -F "$config" -q "fswth-local:$workdir/out/$name.$ext" "$out/$name.$ext" </dev/null
    done
    gunzip -f "$out/$name.img.gz"
    guest "Remove-Item -Force '$workdir/out/$name.*' -ErrorAction SilentlyContinue" >&2
done
