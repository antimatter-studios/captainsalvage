### Fixed

- **MP3s with tags at the end are no longer marked damaged.** The MP3 test counted the last frame as a break whenever an ID3v1, APE or Lyrics3 tag, another ID3 tag or padding followed it, and also broke on zeros padding out an ID3v2 tag and on an ID3v2.4 footer. Whole MP3s came out as "Plays with gaps" because of it.

### Added

- **Test results can be forgotten for some formats only.** `file_tests_forget` takes `formats`, so an improved check can re-test, say, only MP3s.
