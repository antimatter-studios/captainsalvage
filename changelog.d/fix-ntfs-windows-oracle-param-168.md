### Fixed

- **The Windows NTFS oracle accepts volume scales ending in a carriage return.** Windows Git Bash can pass CRLF output from the volume-spec generator to PowerShell; the accepted scale is validated and normalized before use.
