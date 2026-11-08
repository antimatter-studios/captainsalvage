### Added

- **Keep file names and previews from agents.** A switch in the chat panel, which only the person can change, keeps file names and contents from any agent working through the tools. Names and paths in every answer read `[hidden]` with the extension kept, previews and window screenshots are refused, and SQL from `query_map` and scripts reads name columns as empty. Counts, sizes, types and verdicts stay visible. A script can still read raw bytes, so the switch prevents accidents rather than stopping a determined agent.
