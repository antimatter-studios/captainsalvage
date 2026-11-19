### Fixed

- **An exFAT file whose entry runs across two folder blocks is no longer left out.** When a file's directory entry started at the end of one block of a folder and finished at the start of the next, and that next block was not the one right after it on the disk, the file was missing from the listing. The scan now reads the rest of the entry from the folder's next block, wherever it is.
