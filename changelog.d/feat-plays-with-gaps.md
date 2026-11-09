### Added

- **Plays with gaps.** An MP3 that fails its test but is still at least 80% valid frames is now marked "Plays with gaps" rather than "Failed test", because a player skips each damaged stretch and carries on. On a real damaged volume this was most failed MP3s whose clusters had been reused at their start by small files: everything after each overwritten stretch was intact.
