### Fixed

- **Files with long entry sets are found.** A file whose directory entry carries more than eighteen secondary entries, as one with vendor extensions can, is now listed and recovered instead of being skipped.
- **Less noise among lost entries.** A candidate entry whose checksum fails and whose reserved bytes are not zero is now treated as noise, not as a damaged file.
