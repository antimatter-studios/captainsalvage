### Fixed

- **A copy now includes files that have no bytes.** A file that is live on the volume but empty, such as `__init__.py`, `.gitkeep` or a lock file, was left out of every copy and Export, and the status did not say so. It is now made at its destination, empty, with its name and date, counted in the plan and the report, and never over a file that is already there. This holds for exFAT, FAT and NTFS. An empty entry that was deleted is still not made into a file.
