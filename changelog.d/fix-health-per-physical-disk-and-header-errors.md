### Fixed

- **An error under the source header is readable.** An error such as "The disk has been disconnected; scan not started." was squeezed into the narrow column of the progress bar beside "Saved scans", one or two words to a line, and ran off the top of the window. It now has a line of its own under the title row, the whole width of the window, and a very long one wraps and scrolls inside itself.

### Added

- **A zone found outside the probe can be marked.** `probe_health` has a `mark_bad` action, and the raw-disk view a "Mark bad sectors" form, to record a zone found some other way, such as by reading the drive with `dd`: its first and last byte, what happened (the drive reset, a read failed, or keep out) and why. It is kept with who marked it and when, and nothing reads it afterwards.
- **A disk that has reset is opened carefully.** Once a disk has a zone where the drive reset, a zone a probe jumped over, or has reset twice, opening it does not read fresh markers across the whole disk and does not run a range analysis: it is recognised from at most five of its stored markers that lie where earlier reads went well, and the window says so. Analysing a range stays the person's to ask for.
- **The bad zones and the reset count belong to the physical disk.** A whole disk and its partitions share one map, one reset count and one failing flag, so a zone found through a partition is avoided from the whole disk and the other way round, and a drive that reset once through each is failing. A GPT disk's own GUID is part of its key, so two disks of one model are two disks. A read that failed because the device had already gone no longer marks the place it was asked for as the cause: only the read in flight when the device went away can.
