-- The map's schema as it was before the Disk Rebuild rename (main at 0bc01fc), kept so a test
-- can build a database the way an older build did. The later columns follow as ALTER TABLE.
CREATE TABLE IF NOT EXISTS scan_state (key TEXT PRIMARY KEY, value TEXT);
CREATE TABLE IF NOT EXISTS sets (
    id INTEGER PRIMARY KEY,
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    name TEXT NOT NULL,
    in_use INTEGER NOT NULL,
    checksum_ok INTEGER NOT NULL,
    attributes INTEGER NOT NULL,
    is_dir INTEGER NOT NULL,
    first_cluster INTEGER NOT NULL,
    length INTEGER NOT NULL,
    valid_length INTEGER NOT NULL,
    no_fat_chain INTEGER NOT NULL,
    created TEXT,
    modified TEXT,
    accessed TEXT,
    node INTEGER,
    verdict TEXT,
    reason TEXT,
    clusters_needed INTEGER,
    clusters_found INTEGER,
    clusters_allocated INTEGER,
    clusters_conflict INTEGER,
    UNIQUE (source_cluster, source_offset)
);
CREATE INDEX IF NOT EXISTS sets_first ON sets (first_cluster);
CREATE INDEX IF NOT EXISTS sets_node ON sets (node);
CREATE TABLE IF NOT EXISTS ntfs_records (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    record_number INTEGER,
    sequence INTEGER NOT NULL,
    parent_record INTEGER,
    parent_sequence INTEGER,
    storage TEXT NOT NULL,
    PRIMARY KEY (source_cluster, source_offset)
);
CREATE TABLE IF NOT EXISTS ntfs_extents (
    source_cluster INTEGER NOT NULL,
    source_offset INTEGER NOT NULL,
    vcn INTEGER NOT NULL,
    lcn INTEGER,
    count INTEGER NOT NULL,
    PRIMARY KEY (source_cluster, source_offset, vcn)
);
CREATE INDEX IF NOT EXISTS ntfs_extents_lcn ON ntfs_extents (lcn);
CREATE TABLE IF NOT EXISTS file_tests (
    id INTEGER PRIMARY KEY,
    first_cluster INTEGER NOT NULL,
    length INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    format TEXT NOT NULL,
    valid_fraction REAL,
    first_break INTEGER,
    decoded INTEGER,
    note TEXT NOT NULL,
    sha256 TEXT,
    tested_at_millis INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS file_tests_sha256 ON file_tests (sha256);
CREATE TABLE IF NOT EXISTS read_errors (cluster INTEGER PRIMARY KEY, message TEXT);
CREATE TABLE IF NOT EXISTS nodes (
    head INTEGER PRIMARY KEY,
    parent INTEGER,
    name TEXT,
    kind TEXT NOT NULL,
    modified TEXT,
    files INTEGER NOT NULL,
    bytes INTEGER NOT NULL,
    intact_files INTEGER NOT NULL,
    intact_bytes INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS nodes_parent ON nodes (parent);
CREATE TABLE IF NOT EXISTS overlays (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL,
    target INTEGER,
    payload TEXT NOT NULL,
    note TEXT NOT NULL,
    author TEXT NOT NULL,
    active INTEGER NOT NULL,
    created_at_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS islands (
    head INTEGER PRIMARY KEY,
    tail INTEGER NOT NULL,
    length INTEGER NOT NULL,
    allocated INTEGER NOT NULL,
    end_kind TEXT NOT NULL,
    owners INTEGER NOT NULL,
    head_class INTEGER NOT NULL,
    mean_entropy INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS islands_length ON islands (length);
CREATE TABLE IF NOT EXISTS canvases (
    id INTEGER PRIMARY KEY,
    name TEXT NOT NULL,
    created_at_millis INTEGER NOT NULL,
    updated_at_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS canvas_items (
    id INTEGER PRIMARY KEY,
    canvas INTEGER NOT NULL,
    kind TEXT NOT NULL,
    head INTEGER,
    name TEXT,
    parent_item INTEGER,
    parent_head INTEGER,
    position INTEGER NOT NULL,
    created_at_millis INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS canvas_items_canvas ON canvas_items (canvas, parent_item);
CREATE TABLE IF NOT EXISTS canvas_suggestions (
    id INTEGER PRIMARY KEY,
    canvas INTEGER NOT NULL,
    head INTEGER NOT NULL,
    place_item INTEGER,
    place_head INTEGER,
    merge INTEGER NOT NULL,
    note TEXT NOT NULL,
    author TEXT NOT NULL,
    state TEXT NOT NULL,
    created_at_millis INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS canvas_choices (
    canvas INTEGER NOT NULL,
    path TEXT NOT NULL,
    entry INTEGER NOT NULL,
    PRIMARY KEY (canvas, path)
);
CREATE TABLE IF NOT EXISTS activity (
    id INTEGER PRIMARY KEY,
    at_millis INTEGER NOT NULL,
    author TEXT NOT NULL,
    action TEXT NOT NULL,
    detail TEXT NOT NULL
);
ALTER TABLE canvas_items ADD COLUMN entry INTEGER;
ALTER TABLE sets ADD COLUMN o_name TEXT;
ALTER TABLE sets ADD COLUMN o_first INTEGER;
ALTER TABLE sets ADD COLUMN o_length INTEGER;
ALTER TABLE sets ADD COLUMN o_no_fat INTEGER;
ALTER TABLE sets ADD COLUMN o_is_dir INTEGER;
ALTER TABLE sets ADD COLUMN o_ok INTEGER;
ALTER TABLE sets ADD COLUMN confidence REAL;
ALTER TABLE nodes ADD COLUMN link_confidence REAL;
ALTER TABLE sets ADD COLUMN reason_code TEXT;
ALTER TABLE sets ADD COLUMN type_group TEXT;
ALTER TABLE sets ADD COLUMN extension TEXT;
