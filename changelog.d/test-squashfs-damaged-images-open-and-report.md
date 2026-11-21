### Fixed

- **A damaged or truncated SquashFS image now says what is wrong when you open it.** A copy cut short, or one with a zeroed inode or directory table header, used to open as an empty volume with no damage reported. It now lists what it found, and a truncated copy still shows its files.
