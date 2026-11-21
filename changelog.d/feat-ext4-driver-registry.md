### Added

- **An ext2, ext3 or ext4 volume opens and says what it is.** The app reads it through the filesystem's own driver, as it does SquashFS: it finds the volume by its superblock (at byte 1024, past the first sector), keeps the superblock so the volume still opens if the disk's own is written over, and shows its block size, label and serial number. The support table has an ext2/3/4 column, and the instructions an MCP client is given name it.
