"""Window test for #201: an agent builds a Disk Rebuild, and the person sees and undoes it.

The agent's side is the MCP tools, as an agent calls them. The person's side is the window: the
Rebuild tab's Agent marks, the bar with Undo, Undo all agent changes and Restore to before the
agent. It opens only the synthetic jigsaw.img, never a real disk.
"""
import os, sys, time
sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH, call, check, finish, js, probe, scan_and_reopen, start, wait_for

PERSON = "window.__TAURI_INTERNALS__.invoke('engine_call', {tool: %r, args: %s})"
BAR = "document.querySelector('.agent-bar')"
BUTTON = "[...document.querySelectorAll('.agent-bar button')].find((b) => b.textContent.trim() === %r)"


def person(tool, args='{}'):
    js(PERSON % (tool, args))
    time.sleep(1)


def items(rebuild):
    found = call('disk_rebuild_items', diskRebuild=rebuild)
    return found if isinstance(found, list) else []


start(sys.argv[1])
try:
    scan_and_reopen(os.path.join(SCRATCH, 'jigsaw.img'))
    js("[...document.querySelectorAll('[role=tab]')].find((b) => b.getAttribute('aria-label') === 'Rebuild').click()")
    check('the Rebuild tab opens', wait_for("!!document.querySelector('.rebuild')", 10), True)
    check('no bar before an agent changes anything', probe("!%s" % BAR), True)

    # The agent builds a Disk Rebuild through the tools.
    rebuild = call('disk_rebuild_create', name='Agent build')['id']
    root = call('disk_rebuild_place', diskRebuild=rebuild, head=20)['id']
    photos = call('disk_rebuild_folder', diskRebuild=rebuild, name='Photos', place={'item': root})['id']
    call('disk_rebuild_place', diskRebuild=rebuild, head=40, place={'item': photos})
    call('disk_rebuild_place', diskRebuild=rebuild, head=30, place={'item': root}, merge=True)
    check('the agent made four items in its Disk Rebuild', len(items(rebuild)), 4)

    # The person sees it straight away, marked as made by the agent.
    check('the bar appears', wait_for("!!%s" % BAR, 15), True)
    check('the Disk Rebuild is named as made by the agent', 'made by the agent' in probe("document.querySelector('.rebuild-bar select').selectedOptions[0].textContent"), True)
    check('the folders carry the Agent mark', wait_for("document.querySelectorAll('.rebuild .agent-mark').length >= 2", 15), True)
    check('the bar names what the agent did last', 'merged the folder at cluster 30' in probe("%s.textContent" % BAR).lower(), True)
    check('it is in the activity log as the agent\'s', any(entry['action'] == 'disk_rebuild_place' and entry['author'] == 'claude' for entry in call('activity')), True)

    # Starting a copy and undoing are the person's: an agent that tries is refused.
    check('an agent cannot start a copy', 'Only the person' in call('disk_rebuild_copy_start', diskRebuild=rebuild, destination='/tmp/nowhere').get('error', ''), True)
    check('an agent cannot undo', 'Only the person' in call('disk_rebuild_undo').get('error', ''), True)

    # Undo takes back one step: the merge.
    js("(%s).click()" % (BUTTON % 'Undo'))
    check('Undo takes the last change back', wait_for("document.querySelector('.agent-bar-text')?.textContent.toLowerCase().includes('placed the folder at cluster 40')", 15), True)
    check('one item is gone', len(items(rebuild)), 3)

    # Undo all takes back everything the agent session did, the Disk Rebuild included.
    js("(%s).click()" % (BUTTON % 'Undo all agent changes'))
    check('the bar goes away', wait_for("!%s" % BAR, 15), True)
    check('the Disk Rebuild the agent made is gone', call('disk_rebuild_list'), [])
    check('the window offers to start a Disk Rebuild again', wait_for("!!document.querySelector('.disk-rebuild-start')", 15), True)

    # Restore: the person's own Disk Rebuild is put back as it was before the agent began.
    person('disk_rebuild_create', '{name: "Mine"}')
    mine = call('disk_rebuild_list')[0]['id']
    person('disk_rebuild_place', '{diskRebuild: %d, head: 20}' % mine)
    before = [item['id'] for item in items(mine)]
    check('the person placed one piece', len(before), 1)
    folder = call('disk_rebuild_folder', diskRebuild=mine, name='Agent folder', place={'item': before[0]})['id']
    call('disk_rebuild_place', diskRebuild=mine, head=40, place={'item': folder})
    check('the agent added two items', len(items(mine)), 3)
    check('the person\'s own Disk Rebuild is not marked as the agent\'s', call('disk_rebuild_list')[0]['madeByAgent'], False)
    check('the bar offers Restore to before the agent', wait_for("!!(%s)" % (BUTTON % 'Restore to before the agent'), 15), True)
    js("(%s).click()" % (BUTTON % 'Restore to before the agent'))
    check('it asks first', wait_for("!!(%s)" % (BUTTON % 'Restore'), 10), True)
    js("(%s).click()" % (BUTTON % 'Restore'))
    check('the Disk Rebuild is as it was', wait_for("!document.querySelector('.agent-bar')", 15), True)
    check('only the person\'s piece is left', [item['id'] for item in items(mine)], before)
    check('the source is unchanged after all of it', call('verify_unchanged').get('unchanged'), True)
finally:
    finish()
