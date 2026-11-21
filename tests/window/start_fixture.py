"""A made-up list of disks for the start screen, built from small synthetic image files in this folder.

A test gives the window the list through SALVAGE_TEST_STARTUP_DISKS, so no real disk is named or opened.
Three disks: a whole disk with no partition table, a disk with three partitions and a card reader with one.
`calm` is the list as a person first sees it, `needy` the same disks needing an unmount and a permission.
"""
import json
import os
from pathlib import Path

from harness import SCRATCH

MIB = 1024 * 1024
FOLDER = Path(SCRATCH)
OPENED = 1730000000000


def image(name, size):
    path = FOLDER / name
    data = bytearray(size)
    data[:16] = name.encode().ljust(16, b".")[:16]
    path.write_bytes(data)
    return str(path)


def volume(device, whole, name, size, fmt, uuid, known=False, mounted=False, readable=True):
    record = {"uuid": uuid, "name": name, "bytes": size, "lastOpenedMillis": OPENED}
    return {"device": device, "wholeDisk": whole, "name": name, "bytes": size, "format": fmt, "external": True,
            "mountPoint": "/Volumes/" + name if mounted else None, "uuid": uuid, "mountBlockKey": uuid, "canOpen": fmt != "HFS+", "readable": readable,
            "known": {"record": record, "kinds": [], "lostFolders": 0, "scanned": True, "filesFound": 4200, "bytesFound": 2 * MIB, "filesPassed": 30, "filesDamaged": 2} if known else None,
            "fstabLine": None, "protected": False, "mountBlocked": False}


def disk(device, name, size, scheme="GUID_partition_scheme", readable=True):
    return {"device": device, "name": name, "bytes": size, "external": True, "scheme": scheme, "readable": readable}


whole = image("picker-whole.img", 5 * MIB)
second = image("picker-second.img", 8 * MIB)
reader = image("picker-reader.img", 4 * MIB)
part_a = image("picker-part-a.img", 3 * MIB)
part_b = image("picker-part-b.img", 2 * MIB)
part_c = image("picker-part-c.img", 1 * MIB)
part_d = image("picker-part-d.img", 2 * MIB)
UUID_A, UUID_B, UUID_C, UUID_D = ("00000000-0000-4000-8000-00000000000%d" % n for n in range(1, 5))
elsewhere = {"record": {"uuid": "00000000-0000-4000-8000-0000000000ee", "name": "Old Backup", "bytes": 9 * MIB, "lastOpenedMillis": OPENED}, "kinds": [], "lostFolders": 0,
             "scanned": False, "filesFound": 0, "bytesFound": 0, "filesPassed": 0, "filesDamaged": 0}
disks = [disk(whole, "UNREADABLE TABLE DRIVE", 5 * MIB, None), disk(second, "STORAGE DEVICE", 8 * MIB), disk(reader, "CARD READER", 4 * MIB)]

calm = {
    "disks": disks,
    "volumes": [volume(part_a, second, "Holiday", 3 * MIB, "ExFAT", UUID_A, known=True), volume(part_b, second, "Backup", 2 * MIB, "NTFS", UUID_B),
                volume(part_c, second, "Camera", 1 * MIB, "FAT32", UUID_C), volume(part_d, reader, "Dashcam", 2 * MIB, "ExFAT", UUID_D)],
    "elsewhere": [elsewhere], "diskSleepMinutes": None,
}
# The same disks needing something: the disk with three partitions has two mounted and one that
# cannot be read yet, and the card reader's volume cannot be read.
needy = {
    "disks": disks,
    "volumes": [volume(part_a, second, "Holiday", 3 * MIB, "ExFAT", UUID_A, mounted=True), volume(part_b, second, "Backup", 2 * MIB, "NTFS", UUID_B, mounted=True),
                volume(part_c, second, "Camera", 1 * MIB, "FAT32", UUID_C, readable=False), volume(part_d, reader, "Dashcam", 2 * MIB, "ExFAT", UUID_D, readable=False)],
    "elsewhere": [], "diskSleepMinutes": None,
}


REPORT = FOLDER / "picker-report.json"


def use(report):
    """Writes the list the window will be given the next time it looks for disks."""
    REPORT.write_text(json.dumps(report))
    os.environ["SALVAGE_TEST_STARTUP_DISKS"] = str(REPORT)
