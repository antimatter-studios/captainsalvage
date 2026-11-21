### Fixed

- **The instructions an MCP client is given name SquashFS.** They said the app reads a damaged exFAT, FAT or NTFS volume and left SquashFS out, so an agent was told the app could not open an image it does open. They now name every filesystem the app reads.
