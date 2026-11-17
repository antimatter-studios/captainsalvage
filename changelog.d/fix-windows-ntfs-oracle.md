### Fixed

- **A small file in a compressed Windows folder is read.** Windows marks the data of such a file as compressed although it is held, uncompressed, in the file's own record. The app took the mark at its word, called the file partial, refused to show it and left it out of copies. It is now read and copied like any other small file.
- **A file with a very long name keeps its extension when copied.** A name too long for the destination was cut at its end, so `…long name.jpg` became a name with no `.jpg`. The name is cut before the extension instead.
