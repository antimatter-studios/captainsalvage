### Added

- **An exFAT volume with 1024, 2048 or 4096-byte sectors opens.** It was refused with `unsupported sector size 4096`, which left drives with 4096-byte sectors (4Kn disks, some SD cards and flash media) formatted exFAT showing nothing. The app now lists their files and folders and copies them out, with the same SHA-256 as the Linux kernel reports for a volume `mkfs.exfat` made on a 4096-byte-sector device.
