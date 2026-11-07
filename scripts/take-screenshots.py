#!/usr/bin/env python3
"""Captures the app's main screens for the README, with file names hidden.

Run this for each new version:

    scripts/take-screenshots.py media/changelog/v3

It starts a second copy of the built app off screen, so a window you are working
in is left alone. It drives that copy through its test hook and saves PNGs.

Names on a real disk are private, so the copy blurs them. Always look at each image
before committing it: bytes and chat answers can still hold names.
"""
import base64
import json
import os
import shutil
import signal
import subprocess
import sys
import time
import urllib.request
from pathlib import Path

CONFIG = Path.home() / "Library/Application Support/Captain Salvage"
BINARY = Path(os.environ.get(
    "SALVAGE_BINARY",
    Path(__file__).resolve().parent.parent / "target/release/bundle/macos/Captain Salvage.app/Contents/MacOS/captain-salvage",
))
OUTPUT_WIDTH = "1600"
HIDE_NAMES = """
(function () {
  const style = document.createElement('style');
  style.textContent = `
    .row-name:not([data-safe]), .row-path, .detail-name, .detail-path, .chat-about, .chat-context,
    .test-report .muted, .strip-tip, .volume-list, .salvage {
      filter: blur(6px);
    }
  `;
  document.head.appendChild(style);
  const mark = () => document.querySelectorAll('.row-name').forEach((element) => {
    if (/^(Lost folder|Volume root)/.test(element.textContent)) element.dataset.safe = '1';
    else delete element.dataset.safe;
  });
  new MutationObserver(mark).observe(document.body, { subtree: true, childList: true, characterData: true });
  mark();
})();
"""


class App:
    """A copy of the app, reached through its MCP server."""

    def __init__(self, dark=False):
        self.saved_config = (CONFIG / "mcp.json").read_bytes() if (CONFIG / "mcp.json").exists() else None
        before = self.saved_config
        environment = {
            "HOME": os.environ["HOME"],
            "USER": os.environ.get("USER", ""),
            "PATH": "/usr/bin:/bin:/usr/sbin:/sbin",
            "SALVAGE_TEST_UI": "1",
            "SALVAGE_TEST_HIDDEN": "1",
        }
        if dark:
            environment["SALVAGE_TEST_DARK"] = "1"
        self.process = subprocess.Popen([str(BINARY)], env=environment, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        for _ in range(60):
            time.sleep(0.5)
            current = (CONFIG / "mcp.json").read_bytes() if (CONFIG / "mcp.json").exists() else None
            # With the normal app open, this copy gets its own port and rewrites the file.
            if current and (current != before or self._port_free()):
                break
        self.info = json.loads((CONFIG / "mcp.json").read_text())

    @staticmethod
    def _port_free():
        return False

    def close(self):
        self.process.send_signal(signal.SIGTERM)
        self.process.wait(timeout=15)
        if self.saved_config is not None:
            (CONFIG / "mcp.json").write_bytes(self.saved_config)
            os.chmod(CONFIG / "mcp.json", 0o600)

    def raw(self, name, **arguments):
        message = {"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": arguments}}
        request = urllib.request.Request(
            self.info["url"],
            json.dumps(message).encode(),
            {"Content-Type": "application/json", "Authorization": "Bearer " + self.info["token"]},
        )
        with urllib.request.urlopen(request, timeout=300) as response:
            return json.loads(response.read().decode())["result"]

    def tool(self, name, **arguments):
        result = self.raw(name, **arguments)
        text = result["content"][0]["text"]
        if result.get("isError"):
            raise RuntimeError(f"{name}: {text}")
        return json.loads(text)

    def wait_until_open(self):
        for _ in range(60):
            try:
                return self.tool("status")
            except RuntimeError:
                time.sleep(2)
        raise SystemExit("The volume did not open. Check its device permission.")

    def script(self, code):
        self.tool("ui_eval", code=code)

    def click(self, text):
        self.script(f"[...document.querySelectorAll('button')].find(b => b.textContent.trim() === {json.dumps(text)})?.click()")

    def click_label(self, label):
        self.script(f"document.querySelector({json.dumps(f'[aria-label={json.dumps(label)}]')})?.click()")

    def shot(self, path):
        item = self.raw("screenshot_app")["content"][0]
        if item["type"] != "image":
            raise RuntimeError(f"No picture for {path}: {item.get('text')}")
        Path(path).write_bytes(base64.b64decode(item["data"]))
        subprocess.run(["sips", "--resampleWidth", OUTPUT_WIDTH, str(path)], capture_output=True, check=True)
        print("saved", path)


def first_id(app, where):
    rows = app.tool("query_map", sql=f"SELECT id FROM sets WHERE is_dir = 0 AND {where} ORDER BY length DESC LIMIT 1")["rows"]
    return rows[0]["id"] if rows else None


def ask(app, question):
    code = """
    (function () {
      const box = document.querySelector('.composer textarea');
      Object.getOwnPropertyDescriptor(HTMLTextAreaElement.prototype, 'value').set.call(box, %s);
      box.dispatchEvent(new Event('input', { bubbles: true }));
      setTimeout(() => document.querySelector('.composer button.primary').click(), 200);
    })();
    """ % json.dumps(question)
    app.script(code)
    time.sleep(4)
    for _ in range(90):
        running = subprocess.run(["pgrep", "-f", "mcp-client.json"], capture_output=True, text=True).stdout.strip()
        if not running:
            return
        time.sleep(2)


def main():
    output = Path(sys.argv[1])
    output.mkdir(parents=True, exist_ok=True)

    app = App()
    try:
        app.wait_until_open()
        app.tool("resolve")
        time.sleep(4)
        app.script(HIDE_NAMES)
        time.sleep(2)
        app.shot(output / "overview.png")

        app.click("Content")
        time.sleep(3)
        app.click("What explains it")
        time.sleep(1)

        video = first_id(app, "verdict = 'likely-intact' AND lower(name) LIKE '%.avi'")
        if video:
            app.tool("show_in_app", kind="entry", value=video)
            time.sleep(4)
            app.shot(output / "file-and-bytes.png")
            ask(app, "What kind of data is at this cluster, and does its header fit a healthy file? "
                     "Check with your tools and answer in two short sentences. Do not name the file or any other file.")
            time.sleep(2)
            app.shot(output / "ask-claude.png")

        archive = first_id(app, "verdict = 'likely-intact' AND lower(name) LIKE '%.zip' AND length BETWEEN 1000000 AND 80000000")
        if archive:
            app.tool("show_in_app", kind="entry", value=archive)
            time.sleep(3)
            app.click_label("Next cluster")
            time.sleep(2)
            app.click("Test format")
            time.sleep(12)
            app.script("document.querySelector('.test-report')?.scrollIntoView()")
            time.sleep(1)
            app.shot(output / "format-test.png")

        if video:
            app.tool("show_in_app", kind="entry", value=video)
            time.sleep(3)
            app.click_label("Copy out")
            time.sleep(10)
            app.script("[...document.querySelectorAll('.volume-list button')].find(b => !b.disabled && b.textContent.startsWith('sdcard'))?.click()")
            time.sleep(1)
            app.click("Check")
            time.sleep(4)
            app.shot(output / "copy-out.png")

        app.click("Unexplained")
        time.sleep(6)
        app.script("document.querySelector('.explorer .panel-body .row')?.click()")
        time.sleep(5)
        app.shot(output / "unexplained-region.png")
    finally:
        app.close()

    time.sleep(1)
    dark = App(dark=True)
    try:
        dark.wait_until_open()
        time.sleep(5)
        dark.script(HIDE_NAMES)
        time.sleep(2)
        dark.shot(output / "overview-dark.png")
    finally:
        dark.close()


if __name__ == "__main__":
    if not shutil.which("sips"):
        raise SystemExit("This script needs macOS.")
    main()
