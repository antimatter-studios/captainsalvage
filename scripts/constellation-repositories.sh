#!/usr/bin/env bash
# The repositories that make up Captain Salvage. This file has no side effects and is
# sourced by every tool that needs the repository set (am-overview, am-prs, am-queue,
# am-issues).
# The drivers are the filesystems Captain Salvage reads, rust-fs-core is the shared
# interface they all implement, and the two test harnesses host the oracles the app and
# the drivers are checked against, so their defects block work here too.
CONSTELLATION_REPOSITORIES=(
    "captainsalvage:antimatter-studios/captainsalvage"
    "rust-fs-core:antimatter-studios/rust-fs-core"
    "rust-fs-fat:antimatter-studios/rust-fs-fat"
    "rust-fs-exfat:antimatter-studios/rust-fs-exfat"
    "rust-fs-ntfs:antimatter-studios/rust-fs-ntfs"
    "rust-fs-ext4:antimatter-studios/rust-fs-ext4"
    "rust-fs-xfs:antimatter-studios/rust-fs-xfs"
    "rust-fs-btrfs:antimatter-studios/rust-fs-btrfs"
    "rust-fs-bcachefs:antimatter-studios/rust-fs-bcachefs"
    "rust-fs-erofs:antimatter-studios/rust-fs-erofs"
    "rust-fs-squashfs:antimatter-studios/rust-fs-squashfs"
    "fs-linux-test-harness:antimatter-studios/fs-linux-test-harness"
    "fs-windows-test-harness:antimatter-studios/fs-windows-test-harness"
)
