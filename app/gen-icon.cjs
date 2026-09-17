// 生成 src-tauri/icons/icon.png（512x512 绿色圆角块 + 白 C）与 icon.ico（PNG-in-ICO）
const zlib = require("node:zlib");
const fs = require("node:fs");
const path = require("node:path");

const SIZE = 512;
const RADIUS = 96;
const BG = [7, 193, 96, 255]; // #07C160

function inRoundedRect(x, y, s, r) {
  const rx = Math.min(x, s - 1 - x);
  const ry = Math.min(y, s - 1 - y);
  if (rx >= r || ry >= r) return true;
  const dx = r - rx, dy = r - ry;
  return dx * dx + dy * dy <= r * r;
}

function makePng(size) {
  const raw = Buffer.alloc(size * (size * 4 + 1));
  const scale = size / SIZE;
  const r = RADIUS * scale;
  // 简单白“C”：环带区域
  for (let y = 0; y < size; y++) {
    const row = y * (size * 4 + 1);
    raw[row] = 0;
    for (let x = 0; x < size; x++) {
      const o = row + 1 + x * 4;
      if (!inRoundedRect(x, y, size, r)) {
        raw.fill(0, o, o + 4);
        continue;
      }
      // 画一个白色圆环开口向右 = 粗略的 C
      const cx = x - size / 2, cy = y - size / 2;
      const d = Math.sqrt(cx * cx + cy * cy);
      const ring = d > size * 0.22 && d < size * 0.34;
      const gap = cx > 0 && Math.abs(cy) < size * 0.16;
      const isC = ring && !gap;
      const px = isC ? [255, 255, 255, 255] : BG;
      raw[o] = px[0]; raw[o + 1] = px[1]; raw[o + 2] = px[2]; raw[o + 3] = px[3];
    }
  }
  const idat = zlib.deflateSync(raw, { level: 9 });
  return pngWrap(size, idat);
}

function crc32(buf) {
  if (zlib.crc32) return zlib.crc32(buf) >>> 0;
  let c, table = [];
  for (let n = 0; n < 256; n++) {
    c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c >>> 0;
  }
  let crc = 0xffffffff;
  for (const b of buf) crc = table[(crc ^ b) & 0xff] ^ (crc >>> 8);
  return (crc ^ 0xffffffff) >>> 0;
}

function chunk(type, data) {
  const len = Buffer.alloc(4);
  len.writeUInt32BE(data.length);
  const td = Buffer.concat([Buffer.from(type), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(td));
  return Buffer.concat([len, td, crc]);
}

function pngWrap(size, idat) {
  const sig = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  return Buffer.concat([
    sig,
    chunk("IHDR", ihdr),
    chunk("IDAT", idat),
    chunk("IEND", Buffer.alloc(0)),
  ]);
}

function makeIco(pngBuf, size) {
  // ICO: ICONDIR + ICONDIRENTRY + PNG data
  const dir = Buffer.alloc(6);
  dir.writeUInt16LE(0, 0);
  dir.writeUInt16LE(1, 2); // ico
  dir.writeUInt16LE(1, 4); // 1 image
  const ent = Buffer.alloc(16);
  ent[0] = size >= 256 ? 0 : size; // width (0 = 256)
  ent[1] = size >= 256 ? 0 : size;
  ent[2] = 0; ent[3] = 0;
  ent.writeUInt16LE(1, 4); // planes
  ent.writeUInt16LE(32, 6); // bpp
  ent.writeUInt32LE(pngBuf.length, 8);
  ent.writeUInt32LE(22, 12);
  return Buffer.concat([dir, ent, pngBuf]);
}

const out = path.join(__dirname, "src-tauri", "icons");
fs.mkdirSync(out, { recursive: true });
const png256 = makePng(256);
fs.writeFileSync(path.join(out, "icon.png"), png256);
fs.writeFileSync(path.join(out, "32x32.png"), makePng(32));
fs.writeFileSync(path.join(out, "128x128.png"), makePng(128));
fs.writeFileSync(path.join(out, "icon.ico"), makeIco(png256, 256));
console.log("icons written to", out);
