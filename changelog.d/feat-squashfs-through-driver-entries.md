### Added

- **A SquashFS image opens, scans and shows its files.** The app reads it through the filesystem's own driver: its folders and files are listed, a file is previewed from any place in it, and a copy writes the bytes the driver decodes (every compressor SquashFS has: gzip, xz, lzo, lz4 and zstd), with each copy hashed and marked complete or partial as for the other filesystems. No allocation is read from the image, so its map shows every cluster as unknown rather than free. The support table has a SquashFS column.
