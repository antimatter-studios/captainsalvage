### Added

- **An EROFS image opens, scans and shows its files.** The app reads it through the filesystem's own driver, as it does SquashFS and ext2/3/4: it finds the image by its superblock (at byte 1024), keeps the superblock so the image still opens if the disk's own is written over, lists its folders and files, previews a file and copies it out. The support table has an EROFS column, and the instructions an MCP client is given name it.
