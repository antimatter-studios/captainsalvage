### Fixed

- **Opening a source no longer fails with "database is locked" when the catalog is busy for a moment.** The health records of a source (its link to the disk, its reset count and its map) are written again after a short lock clears, instead of the open failing or the record being lost. A read of the device still never waits on the catalog.
