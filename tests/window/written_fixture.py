"""A made-up volume whose structures were changed after its baseline, for the "Something wrote to this disk" box.

`written.img` is a copy of jigsaw.img (see the README). The app records a baseline when it first opens a
volume; `change_after_baseline` then sets bits in the volume's allocation bitmap for clusters that were
free, which is what macOS writing to a mounted disk looks like to the app, and the found files a1.jpg
(cluster 22), deep.mp3 (24) and c.mp3 (41) used those clusters. Only this copy is ever touched.
"""
import os, shutil, sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import SCRATCH

SOURCE = os.path.join(SCRATCH, 'jigsaw.img')
IMAGE = os.path.join(SCRATCH, 'written.img')


def fresh_copy():
    if not os.path.isfile(SOURCE):
        raise SystemExit('jigsaw.img is missing: see the README for how to make it')
    shutil.copyfile(SOURCE, IMAGE)
    return IMAGE


def mark_allocated(path, clusters):
    """Sets the allocation bitmap bit of each cluster of an exFAT image, in place."""
    with open(path, 'r+b') as image:
        boot = image.read(512)
        sector = 1 << boot[108]
        cluster_size = sector << boot[109]
        heap = int.from_bytes(boot[88:92], 'little') * sector
        root = int.from_bytes(boot[96:100], 'little')

        def at(cluster):
            return heap + (cluster - 2) * cluster_size

        image.seek(at(root))
        directory = image.read(cluster_size)
        bitmap = next((int.from_bytes(directory[i + 20:i + 24], 'little') for i in range(0, len(directory), 32) if directory[i] == 0x81), None)
        if bitmap is None:
            raise SystemExit('the image has no allocation bitmap entry in its root')
        for cluster in clusters:
            image.seek(at(bitmap) + (cluster - 2) // 8)
            byte = image.read(1)[0]
            image.seek(at(bitmap) + (cluster - 2) // 8)
            image.write(bytes([byte | (1 << ((cluster - 2) % 8))]))


def change_after_baseline(clusters=(22, 24)):
    mark_allocated(IMAGE, clusters)


def screenshot(path):
    """Saves the window as a PNG, through the app's own MCP tool."""
    import base64, json, urllib.request
    from pathlib import Path
    from harness import state
    info = json.load(open(state['cfg']))
    body = json.dumps({'jsonrpc': '2.0', 'id': 1, 'method': 'tools/call', 'params': {'name': 'captainsalvage_screenshot_app', 'arguments': {}}}).encode()
    request = urllib.request.Request(info['url'], body, headers={'Content-Type': 'application/json', 'Authorization': 'Bearer ' + info['token']})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)['result']
    Path(path).write_bytes(base64.b64decode(result['content'][0]['data']))
