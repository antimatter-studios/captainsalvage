### Changed

- **An MPEG film is walked as a player reads it.** After damage, the MPEG walker now picks the stream up again at the next pack, as players do, and counts only the bytes lost, so a film with a few foreign blocks can be marked "Plays with gaps" instead of failing outright. A last packet cut short at the end of the file is a note rather than a failure.
