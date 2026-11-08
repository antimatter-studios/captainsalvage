### Added

- **MKV and WebM films are checked element by element.** A new Matroska walker follows every element down to each block in each cluster, including clusters of unknown size, so an overwritten or unwritten cluster shows where it starts. It runs as large films stream, and for small ones in memory.

### Fixed

- **AVI films are checked frame by frame.** The AVI walker treated each list, including the one holding every frame, as one opaque chunk, so damage in the middle of a film passed. It now walks inside lists to each video and audio chunk. Padding left after the last RIFF form no longer counts as a break.
