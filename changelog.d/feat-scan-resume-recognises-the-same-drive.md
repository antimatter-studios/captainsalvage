### Added

- **A drive you opened before is recognised as the same drive, so its scan resumes at once.** This holds even when its blocks read differently now or the drive was opened carefully after a reset. A mount that rewrote blocks still marks the regions around them as possibly out of date.
- **A drive is recognised by its model and serial number when the system reports one.** A damaged drive whose first sector cannot be read is still the same drive, and a copy with another serial number is not.
- **When a saved scan cannot be matched to the drive, you are asked.** You can choose Resume anyway or open the drive as a new source. This includes a drive that cannot be recognised at all, for which the saved scans of the same size are offered. A scan is never started again silently.
