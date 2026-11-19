### Fixed

- **A file whose long name was stored across two folder blocks is listed under its real name.** On FAT12, FAT16 and FAT32, when a long file name began at the end of one block of a folder and the rest of the entry was at the start of the next, the file was listed under its short 8.3 name, such as `ENTRYF~4.TXT`, instead of its real name. Its size and contents were right. The scan now joins the end of the block before it, following the folder's chain, so the real name is shown.
