### Added

- **The raw-disk map shows partitions as bands and findings as lanes.** The top map is one stable view of the bytes: declared partitions are broad background bands, and each filesystem format that was found has its own thin lane. A disk with many findings stays readable, because dense findings are summarised by format and count under the map, and the list can be narrowed to one format.
- **An inspector explains what you pick.** Click a band or a lane on the map, or an item in the list, and the side panel gives its format, byte range, confidence and status: recognised, needs related metadata, or decodable. The second graph magnifies the selected range, and clicking it shows that place in the main area.

### Changed

- **The map's layer switch is gone.** Source bytes, declared partitions and filesystem evidence used to be three views to switch between; they are now drawn together.
