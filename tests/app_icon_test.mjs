// The icon in the window's title area and every desktop icon file come from one source,
// src-tauri/icons/source.png. They went stale once: the app icon was redrawn and the header
// kept showing the old one. This fails when any generated icon stops looking like the source.
// To fix it, run `npx tauri icon src-tauri/icons/source.png` and delete the android and ios
// folders it makes.
import assert from "node:assert/strict";
import { readFileSync, readdirSync } from "node:fs";
import { test } from "node:test";
import { inflateSync } from "node:zlib";

const icons = new URL("../src-tauri/icons/", import.meta.url);

/** Reads an 8-bit, non-interlaced RGBA PNG, which is what the Tauri icon tool writes. */
function decode(file) {
  const png = readFileSync(file);
  assert.equal(png.subarray(1, 4).toString(), "PNG", `${file} is not a PNG`);
  let at = 8;
  let width = 0;
  let height = 0;
  const parts = [];
  while (at < png.length) {
    const size = png.readUInt32BE(at);
    const kind = png.subarray(at + 4, at + 8).toString();
    const body = png.subarray(at + 8, at + 8 + size);
    if (kind === "IHDR") {
      width = body.readUInt32BE(0);
      height = body.readUInt32BE(4);
      assert.equal(body[8], 8, "8 bits per channel");
      assert.equal(body[9], 6, "RGBA");
      assert.equal(body[12], 0, "not interlaced");
    } else if (kind === "IDAT") parts.push(body);
    at += 12 + size;
  }
  const raw = inflateSync(Buffer.concat(parts));
  const stride = width * 4;
  const out = Buffer.alloc(stride * height);
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    for (let x = 0; x < stride; x++) {
      const value = raw[y * (stride + 1) + 1 + x];
      const left = x >= 4 ? out[y * stride + x - 4] : 0;
      const up = y > 0 ? out[(y - 1) * stride + x] : 0;
      const upLeft = x >= 4 && y > 0 ? out[(y - 1) * stride + x - 4] : 0;
      let predicted = 0;
      if (filter === 1) predicted = left;
      else if (filter === 2) predicted = up;
      else if (filter === 3) predicted = (left + up) >> 1;
      else if (filter === 4) {
        const p = left + up - upLeft;
        const pa = Math.abs(p - left);
        const pb = Math.abs(p - up);
        const pc = Math.abs(p - upLeft);
        predicted = pa <= pb && pa <= pc ? left : pb <= pc ? up : upLeft;
      }
      out[y * stride + x] = (value + predicted) & 255;
    }
  }
  return { width, height, data: out };
}

/** Averages a square image down to size x size with a box filter, in premultiplied colour. */
function shrink(image, size) {
  const out = new Float64Array(size * size * 4);
  const scale = image.width / size;
  for (let y = 0; y < size; y++) {
    for (let x = 0; x < size; x++) {
      let r = 0, g = 0, b = 0, a = 0, n = 0;
      for (let sy = Math.floor(y * scale); sy < Math.ceil((y + 1) * scale); sy++) {
        for (let sx = Math.floor(x * scale); sx < Math.ceil((x + 1) * scale); sx++) {
          const i = (sy * image.width + sx) * 4;
          const alpha = image.data[i + 3] / 255;
          r += image.data[i] * alpha; g += image.data[i + 1] * alpha; b += image.data[i + 2] * alpha; a += alpha; n++;
        }
      }
      const o = (y * size + x) * 4;
      out[o] = r / n; out[o + 1] = g / n; out[o + 2] = b / n; out[o + 3] = (a / n) * 255;
    }
  }
  return out;
}

/** The mean difference, per colour value, between an icon and the source shrunk to its size. */
function distance(icon, source) {
  const expected = shrink(source, icon.width);
  let total = 0;
  for (let y = 0; y < icon.height; y++) {
    for (let x = 0; x < icon.width; x++) {
      const i = (y * icon.width + x) * 4;
      const alpha = icon.data[i + 3] / 255;
      for (let c = 0; c < 3; c++) total += Math.abs(icon.data[i + c] * alpha - expected[i + c]);
      total += Math.abs(icon.data[i + 3] - expected[i + 3]);
    }
  }
  return total / (icon.width * icon.height * 4);
}

const source = decode(new URL("source.png", icons));
const files = readdirSync(icons).filter((name) => name.endsWith(".png") && name !== "source.png");

test("the source is a square image", () => {
  assert.equal(source.width, source.height);
  assert.ok(source.width >= 1024);
});

for (const name of files) {
  test(`${name} shows the current icon`, () => {
    const icon = decode(new URL(name, icons));
    assert.equal(icon.width, icon.height, "square");
    const off = distance(icon, source);
    assert.ok(off < 4, `${name} differs from source.png by ${off.toFixed(1)} per value. Regenerate it: npx tauri icon src-tauri/icons/source.png`);
  });
}

test("the title area draws the icon from the generated files, large enough to stay sharp", () => {
  const app = readFileSync(new URL("../src/App.tsx", import.meta.url), "utf8");
  const imported = [...app.matchAll(/import \w+ from "\.\.\/src-tauri\/icons\/([^"]+)"/g)].map((match) => match[1]);
  assert.ok(imported.length > 0, "App.tsx imports no icon");
  for (const name of imported) {
    assert.ok(files.includes(name), `${name} is not one of the generated icon files`);
    assert.ok(decode(new URL(name, icons)).width >= 128, `${name} is too small to stay sharp on a retina screen`);
  }
});

test("the app and its installer use the files that are checked here", () => {
  const config = JSON.parse(readFileSync(new URL("../src-tauri/tauri.conf.json", import.meta.url), "utf8"));
  for (const path of config.bundle.icon.filter((icon) => icon.endsWith(".png"))) {
    assert.ok(files.includes(path.replace("icons/", "")), `${path} is not checked`);
  }
});
