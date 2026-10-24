// Generates the Spate toolbar/store icons as PNG files with no dependencies:
// a rounded blue square with a white "download" arrow over a tray.
//
//   node scripts/make-icons.mjs            -> writes icons/ next to this script's parent
//
// The drawing is resolution independent (shapes in unit coordinates, 6x6
// supersampling per pixel), so every size is rendered from the same design.

import { mkdirSync, writeFileSync } from 'node:fs';
import { dirname, join, resolve } from 'node:path';
import { fileURLToPath } from 'node:url';
import { deflateSync } from 'node:zlib';

const CRC_TABLE = (() => {
  const table = new Uint32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  return table;
})();

export function crc32(bytes) {
  let c = 0xffffffff;
  for (const b of bytes) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
}

function pngChunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, 'ascii'), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

/** Encode 8-bit RGBA pixels (row-major Buffer of w*h*4) as a PNG file. */
export function encodePng(width, height, rgba) {
  const stride = width * 4;
  const raw = Buffer.alloc((stride + 1) * height);
  for (let y = 0; y < height; y += 1) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(width, 0);
  ihdr.writeUInt32BE(height, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // colour type RGBA
  ihdr[10] = 0;
  ihdr[11] = 0;
  ihdr[12] = 0;
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    pngChunk('IHDR', ihdr),
    pngChunk('IDAT', deflateSync(raw, { level: 9 })),
    pngChunk('IEND', Buffer.alloc(0)),
  ]);
}

// --- shapes in unit coordinates (0..1, y down) -----------------------------

function inRoundRect(x, y, x0, y0, x1, y1, r) {
  const cx = (x0 + x1) / 2;
  const cy = (y0 + y1) / 2;
  const dx = Math.max(Math.abs(x - cx) - ((x1 - x0) / 2 - r), 0);
  const dy = Math.max(Math.abs(y - cy) - ((y1 - y0) / 2 - r), 0);
  return Math.abs(x - cx) <= (x1 - x0) / 2 && Math.abs(y - cy) <= (y1 - y0) / 2 && dx * dx + dy * dy <= r * r;
}

function inTriangle(x, y, [ax, ay], [bx, by], [cx, cy]) {
  const s1 = (bx - ax) * (y - ay) - (by - ay) * (x - ax);
  const s2 = (cx - bx) * (y - by) - (cy - by) * (x - bx);
  const s3 = (ax - cx) * (y - cy) - (ay - cy) * (x - cx);
  return (s1 >= 0 && s2 >= 0 && s3 >= 0) || (s1 <= 0 && s2 <= 0 && s3 <= 0);
}

const PALETTES = {
  on: { from: [56, 189, 248], to: [29, 78, 216] },
  off: { from: [176, 182, 192], to: [87, 94, 107] },
};

function sample(x, y, palette) {
  if (!inRoundRect(x, y, 0.04, 0.04, 0.96, 0.96, 0.22)) return null;
  const glyph =
    inRoundRect(x, y, 0.405, 0.15, 0.595, 0.5, 0.05) ||
    inTriangle(x, y, [0.22, 0.43], [0.78, 0.43], [0.5, 0.71]) ||
    inRoundRect(x, y, 0.22, 0.76, 0.78, 0.87, 0.05);
  if (glyph) return [255, 255, 255];
  const t = Math.min(1, Math.max(0, (x + y) / 2));
  const { from, to } = PALETTES[palette];
  return from.map((c, i) => c + (to[i] - c) * t);
}

/** Render the icon at `size` pixels; `palette` is "on" or "off". */
export function renderIcon(size, palette = 'on') {
  const ss = 6;
  const rgba = Buffer.alloc(size * size * 4);
  for (let py = 0; py < size; py += 1) {
    for (let px = 0; px < size; px += 1) {
      let r = 0;
      let g = 0;
      let b = 0;
      let a = 0;
      for (let sy = 0; sy < ss; sy += 1) {
        for (let sx = 0; sx < ss; sx += 1) {
          const c = sample((px + (sx + 0.5) / ss) / size, (py + (sy + 0.5) / ss) / size, palette);
          if (!c) continue;
          r += c[0];
          g += c[1];
          b += c[2];
          a += 1;
        }
      }
      const o = (py * size + px) * 4;
      if (a) {
        rgba[o] = Math.round(r / a);
        rgba[o + 1] = Math.round(g / a);
        rgba[o + 2] = Math.round(b / a);
        rgba[o + 3] = Math.round((a / (ss * ss)) * 255);
      }
    }
  }
  return encodePng(size, size, rgba);
}

export const ICON_FILES = [
  ['icon-16.png', 16, 'on'],
  ['icon-32.png', 32, 'on'],
  ['icon-48.png', 48, 'on'],
  ['icon-128.png', 128, 'on'],
  ['icon-off-16.png', 16, 'off'],
  ['icon-off-32.png', 32, 'off'],
];

export function writeIcons(outDir) {
  mkdirSync(outDir, { recursive: true });
  for (const [name, size, palette] of ICON_FILES) writeFileSync(join(outDir, name), renderIcon(size, palette));
  return ICON_FILES.map(([name]) => join(outDir, name));
}

if (process.argv[1] && resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  const out = join(dirname(fileURLToPath(import.meta.url)), '..', 'icons');
  for (const file of writeIcons(out)) console.log(`wrote ${file}`);
}
