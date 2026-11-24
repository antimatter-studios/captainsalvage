### Added

- **Each named data stream of an NTFS file is listed as an entry of its own, named `file:stream`, and can be read and copied.** A copy holds exactly the stream's bytes. This includes streams of a file whose attributes continue in extension records, which were not listed before. A stream that is encrypted, compressed in a way that is not decoded, or incomplete is listed but fails to copy, and is never filled with zeros.

### Fixed

- **Scanning a range again no longer gives a further name or stream of the same file a new entry.** Entries that were already there keep their ids.
