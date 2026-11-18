"""Saves the header of an opened partition, from a synthetic image. Never a real disk.

    python3 tests/window/header_screenshot.py <binary> <image> <png>

Opens the image through the start screen's form, as a person would, and saves the window once the
workspace is up. It works with any build, so the same image can be shown before and after a change.
"""
import base64
import json
import os
import sys
import urllib.request
from pathlib import Path

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import finish, js, start, state, wait_for


def screenshot(path):
    info = json.load(open(state["cfg"]))
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": "captainsalvage_screenshot_app", "arguments": {}}}).encode()
    request = urllib.request.Request(info["url"], body, headers={"Content-Type": "application/json", "Authorization": "Bearer " + info["token"]})
    with urllib.request.urlopen(request, timeout=60) as response:
        result = json.load(response)["result"]
    Path(path).write_bytes(base64.b64decode(result["content"][0]["data"]))


start(sys.argv[1])
try:
    js("(() => {const i=document.querySelector('.start-image input');"
       "Object.getOwnPropertyDescriptor(HTMLInputElement.prototype,'value').set.call(i,%s);"
       "i.dispatchEvent(new Event('input',{bubbles:true}));setTimeout(()=>i.form.requestSubmit(),300);})()" % json.dumps(sys.argv[2]))
    wait_for("!!document.querySelector('.safety-chip')", 90)
    wait_for("true")
    screenshot(sys.argv[3])
finally:
    finish()
