### Fixed

- **A raw scan of a drive that has reset no longer freezes the app.** A raw scan step held a write lock on the catalog while it read the disk, and every read it journaled waited on that same lock, half a minute at a time, while holding what every status call needs: the scan sat at 0.0% and the window answered nothing. A step now reads the disk without the lock and takes it only to write; the read journal has a file of its own and gives up after a moment, keeping a write it cannot make in memory until the file is free, so no read is left open to be blamed for a reset.
