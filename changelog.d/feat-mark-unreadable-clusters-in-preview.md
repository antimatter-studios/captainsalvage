### Added

- **A preview shows where a file's bytes could not be read.** On exFAT and FAT volumes, when a cluster of a file cannot be read from the disk, the text and byte previews now show each of its bytes as `?` and say which bytes they are, instead of showing zeros that look like the file's own data. A copy of such a file keeps its layout, writes zeros where nothing could be read, is marked partial, and its manifest now names the unreadable byte ranges. Images and films still show those places as zeros.
