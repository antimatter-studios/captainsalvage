#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
reference="${1:-$root/crates/salvage-core/tests/parity/reference}"
if [ "$#" -gt 1 ] || [ ! -f "$reference/synthetic.json" ] || [ ! -f "$reference/archives.json" ] || [ ! -f "$reference/boot-sectors.json" ]; then
    echo "Usage: bash scripts/parity.sh [reference-directory]" >&2
    exit 2
fi
reference="$(cd "$reference" && pwd)"
actual="$(mktemp -d "${TMPDIR:-/tmp}/captainsalvage-parity.XXXXXX")"
trap 'rm -rf "$actual"' EXIT

cd "$root"
SALVAGE_PARITY_OUTPUT_DIR="$actual" cargo test --locked -p salvage-core \
    --test synthetic --test archives parity:: -- --test-threads=1

different=0
for name in synthetic.json archives.json boot-sectors.json; do
    if ! diff -u "$reference/$name" "$actual/$name"; then
        different=1
    fi
done
if [ "$different" -ne 0 ]; then
    echo "Parity failed: outputs differ from the pre-driver reference." >&2
    exit 1
fi
echo "Parity passed: synthetic, archive, and boot-sector outputs are byte-identical."
