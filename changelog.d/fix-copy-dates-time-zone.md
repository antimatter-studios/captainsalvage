### Fixed

- **Copied files get the right modified date.** Copies used to read the disk's time as UTC, so dates could be off by your time zone. They now use the zone the disk recorded, or your Mac's zone when it recorded none, and the window shows the recorded zone beside a time.
