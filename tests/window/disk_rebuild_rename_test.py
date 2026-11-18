"""Window test for the Disk Rebuild rename of the map database: a workspace saved with the old
table names opens, is renamed once, and keeps its Disk Rebuild.

Phase 1 scans the jigsaw image and makes a Disk Rebuild with a folder in it. The app is then
stopped and its map is turned back into what an older build wrote (the old table, column and
index names, `user_version` 0), with a note of every row. Phase 2 starts the app again on the same
throwaway home and opens the image: the map must be renamed, the copy made, the rows all there,
and the Rebuild tab must show the Disk Rebuild. It never touches a real disk or the real app
folder: the harness gives the copy a throwaway HOME.
"""
import glob, os, shutil, sqlite3, sys, tempfile, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import harness
from harness import SCRATCH, call, check, finish, js, open_image, scan_and_reopen, start, state, wait_for

IMAGE = os.path.join(SCRATCH, 'jigsaw.img')
PAIRS = [('disk_rebuilds', 'canvases'), ('disk_rebuild_items', 'canvas_items'),
         ('disk_rebuild_suggestions', 'canvas_suggestions'), ('disk_rebuild_choices', 'canvas_choices')]
PERSON = "window.__TAURI_INTERNALS__.invoke('engine_call', {tool: %r, args: %s})"


def rows(connection, tables):
    return {table: connection.execute(f'SELECT * FROM {table} ORDER BY rowid').fetchall() for table in tables}


def stop_app():
    child = state['child']
    child.terminate()
    child.wait(timeout=30)
    time.sleep(1)


home = tempfile.mkdtemp(dir=SCRATCH, prefix='fakehome.')
os.environ['SALVAGE_TEST_HOME'] = home
binary = sys.argv[1]
try:
    start(binary)
    scan_and_reopen(IMAGE)
    js(PERSON % ('disk_rebuild_create', '{name: "Weeks of work"}'))
    time.sleep(2)
    listed = call('disk_rebuild_list')
    check('a Disk Rebuild was made', [item['name'] for item in listed], ['Weeks of work'])
    rebuild = listed[0]['id']
    js(PERSON % ('disk_rebuild_folder', '{diskRebuild: %d, name: "Photos"}' % rebuild))
    time.sleep(2)
    check('with a folder in it', len(call('disk_rebuild_items', diskRebuild=rebuild)), 1)
    stop_app()

    maps = glob.glob(os.path.join(home, 'Library/Application Support/Captain Salvage/**/map.sqlite'), recursive=True)
    check('one map was saved', len(maps), 1)
    path = maps[0]
    connection = sqlite3.connect(path)
    connection.execute('PRAGMA wal_checkpoint(TRUNCATE)')
    new_rows = rows(connection, [new for new, _ in PAIRS])
    check('the new names are the ones in use', len(new_rows['disk_rebuilds']), 1)
    # Back to what an older build wrote.
    for new, old in PAIRS:
        connection.execute(f'ALTER TABLE {new} RENAME TO {old}')
    for old in ('canvas_items', 'canvas_suggestions', 'canvas_choices'):
        connection.execute(f'ALTER TABLE {old} RENAME COLUMN disk_rebuild TO canvas')
    connection.execute('DROP INDEX IF EXISTS disk_rebuild_items_disk_rebuild')
    connection.execute('CREATE INDEX canvas_items_canvas ON canvas_items (canvas, parent_item)')
    connection.execute('PRAGMA user_version = 0')
    connection.commit()
    old_rows = rows(connection, [old for _, old in PAIRS])
    connection.close()
    for suffix in ('-wal', '-shm'):
        if os.path.exists(path + suffix):
            os.remove(path + suffix)
    before = open(path, 'rb').read()
    check('the map has the old names now', sqlite3.connect(path).execute("SELECT count(*) FROM sqlite_master WHERE name = 'canvas_items'").fetchone()[0], 1)

    # The first run's address is stale: the harness waits for the new one to be written.
    os.remove(state['cfg'])
    start(binary)
    open_image(IMAGE)
    # Opening the image opened the map: renamed, once, after a copy.
    backup = path + '.before-disk-rebuild-rename'
    check('the copy is there', os.path.exists(backup), True)
    check('and is the map as it was', open(backup, 'rb').read() == before, True)
    check('only one copy', len(glob.glob(path + '.before-disk-rebuild-rename*')), 1)
    listed = call('disk_rebuild_list')
    check('the Disk Rebuild is still there', [item['name'] for item in listed], ['Weeks of work'])
    check('with its folder', len(call('disk_rebuild_items', diskRebuild=listed[0]['id'])), 1)
    js("[...document.querySelectorAll('.inspector-tabs [role=tab]')].find((b) => b.getAttribute('aria-label') === 'Rebuild')?.click()")
    check('the Rebuild tab lists it', wait_for("[...document.querySelectorAll('.rebuild select option')].some((o) => o.textContent === 'Weeks of work')", 20), True)
    stop_app()
    connection = sqlite3.connect(path)
    names = ' '.join(f'{name} {sql or ""}' for name, sql in connection.execute('SELECT name, sql FROM sqlite_master'))
    check('no old name is left in the map', 'canvas' in names, False)
    check('the version is recorded', connection.execute('PRAGMA user_version').fetchone()[0], 1)
    check('every row is where it was', rows(connection, [new for new, _ in PAIRS]) == {new: old_rows[old] for new, old in PAIRS}, True)
    check('integrity is ok', connection.execute('PRAGMA integrity_check').fetchone()[0], 'ok')
    connection.close()
finally:
    finish()
    shutil.rmtree(home, ignore_errors=True)
