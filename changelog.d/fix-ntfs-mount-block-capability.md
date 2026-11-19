### Fixed

- **The app no longer says it can keep an NTFS volume unmounted.** The Block mount switch works by the volume's UUID, and macOS reports no volume UUID for an NTFS volume, so the checkbox never appears for one and nothing is blocked. The support table (in the window, `captainsalvage filesystem capabilities` and the README) said "Supported"; it now says "Missing" and why. macOS mounts NTFS read-only by itself, so only a third-party NTFS driver on the Mac would write to such a volume.
