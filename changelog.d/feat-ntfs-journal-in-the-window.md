### Added

- **The overview of an NTFS volume can show its change journal and `$LogFile`.** Press "Read the journal" to see what the volume recorded: each change-journal record with its time, name and what happened (created, renamed, deleted, and so on), and whether `$LogFile` is empty, has nothing to replay or may hold transactions Windows would redo. It is read only evidence, kept apart from the files the scan finds, and nothing is ever written or replayed.
