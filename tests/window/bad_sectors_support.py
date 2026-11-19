"""What the bad-sectors window tests share: a health map written into the test copy's catalog before it
opens a made-up disk, as a probe or a reset would have left it. Never a real disk, and never real data:
the images are made here, and the ranges are chosen here."""
import hashlib
import os
import sqlite3
import time
from pathlib import Path

from harness import state

SCHEMA = """
CREATE TABLE IF NOT EXISTS health_ranges (
    source_key TEXT NOT NULL, source_id TEXT,
    start INTEGER NOT NULL, end INTEGER NOT NULL,
    state TEXT NOT NULL, latency_ms INTEGER, note TEXT NOT NULL DEFAULT '',
    updated_millis INTEGER NOT NULL,
    PRIMARY KEY (source_key, start)
);
"""


def source_key(path):
    """The key the app gives a source in its read journal: its length and the start of its first sector,
    with the bytes a mount rewrites left out."""
    data = Path(path).read_bytes()
    head = bytearray(data[:512])
    head[104:120] = bytes(16)
    return "%d-%s" % (len(data), hashlib.sha256(bytes(head)).hexdigest()[:16])


def seed(path, ranges):
    """Writes (start, end, state, found_seconds_ago) ranges as the health map of the image at `path`."""
    folder = os.path.join(state["home"], "Library/Application Support/Captain Salvage")
    os.makedirs(folder, exist_ok=True)
    connection = sqlite3.connect(os.path.join(folder, "raw-scans.sqlite"), timeout=30)
    connection.execute("PRAGMA journal_mode = WAL")
    connection.executescript(SCHEMA)
    key = source_key(path)
    now = int(time.time() * 1000)
    for start, end, health, ago in ranges:
        connection.execute(
            "INSERT OR REPLACE INTO health_ranges (source_key, source_id, start, end, state, latency_ms, note, updated_millis) VALUES (?, NULL, ?, ?, ?, NULL, 'made up by the test', ?)",
            (key, start, end, health, now - ago * 1000),
        )
    connection.commit()
    connection.close()


# What a probe's tooltip and legend say, as the window writes them.
LEGEND = "Bad sectors: reading here drops the drive"
