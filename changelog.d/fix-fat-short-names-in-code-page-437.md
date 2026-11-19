### Fixed

- **A file whose short DOS name has accented letters shows them correctly.** On FAT volumes, a file made by DOS, a camera or another old device, with no long name and letters such as é, ä or ö in its 8.3 name, was listed with replacement characters (`���X.TXT`). The name is now read in the DOS code page 437, so it shows as `éäöX.TXT`.
