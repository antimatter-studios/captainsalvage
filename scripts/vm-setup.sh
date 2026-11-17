#!/usr/bin/env bash
# The Linux test VM's setup, run as root INSIDE the guest by fs-linux-test-harness once per
# boot. Installs ntfs-3g's tools, which validation/oracle.sh uses beside the kernel's ntfs3
# driver, and refuses a guest whose running kernel cannot load ntfs3. The ntfs3 run profile
# (run-profiles/ntfs3.toml) boots the kernel that has it.
set -euo pipefail
export DEBIAN_FRONTEND=noninteractive
running="$(uname -r)"
if ! modinfo -k "$running" ntfs3 >/dev/null 2>&1 || ! modprobe ntfs3; then
    echo "vm-setup: running kernel $running has no loadable ntfs3; select the ntfs3 run profile" >&2
    exit 1
fi
apt-get update -qq
apt-get install -y -qq ntfs-3g openssl python3
modprobe loop || true
for tool in mkntfs ntfsfix ntfsls ntfscat ntfsinfo; do
    command -v "$tool" >/dev/null || { echo "vm-setup: $tool is missing after installing ntfs-3g" >&2; exit 1; }
done
echo "vm-setup: $running has ntfs3; $(ntfsfix --version 2>&1 | head -1)"
