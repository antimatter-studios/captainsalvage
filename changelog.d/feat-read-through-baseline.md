### Added

- **The disk's structures are read from the baseline copy once the disk has changed.** When the boot regions, FAT, bitmap or root on the disk no longer match the baseline, the map uses the copy kept on the Mac, so a chain macOS freed or a cluster it took still reads as it was. File data is still read from the disk, and the warning panel says which source the map uses. A baseline recorded before copies were kept gets its copy the next time the disk still matches it.
