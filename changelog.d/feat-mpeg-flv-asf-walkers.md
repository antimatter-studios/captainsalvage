### Added

- **MPEG, FLV and WMV films are tested.** New walkers follow MPEG program streams pack by pack and packet by packet, FLV tag by tag with each tag's back-pointer, and ASF (WMV, WMA) object by object and packet by packet, so damage shows where it starts. Raw MPEG video with no packs is left untested rather than called damaged.
- **A file is walked as what its bytes say it is**, whatever its name: a WMV named `.mpg` is walked as ASF. Up to 4 KB of web headers before an MPEG stream's first pack are skipped, as players do.
