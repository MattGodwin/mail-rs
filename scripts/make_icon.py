"""Generate assets/icon.ico (16-256 px) from the app's envelope design.

Pure standard library: draws each size with supersampling, encodes PNG,
and packs the PNGs into a multi-resolution ICO. Run: python scripts/make_icon.py
"""
import os
import struct
import zlib

BG = (0xC0, 0x26, 0xD3)
FG = (0xFF, 0xFF, 0xFF)


def coverage(x, y):
    """Colour at (x, y) in the 64x64 design space, or None for transparent."""
    r = 14.0
    cx, cy = min(max(x, r), 64 - r), min(max(y, r), 64 - r)
    if (x - cx) ** 2 + (y - cy) ** 2 > r * r:
        return None
    l, rt, t, b = 14.0, 50.0, 20.0, 44.0
    if l <= x <= rt and t <= y <= b:
        edge = x - l < 2.5 or rt - x < 2.5 or y - t < 2.5 or b - y < 2.5
        mid = (l + rt) / 2
        flap_y = t + (mid - abs(x - mid) - l) * (12.0 / (mid - l))
        if edge or (abs(y - flap_y) < 1.8 and y <= t + 13):
            return FG
    return BG


def render(size, ss=4):
    rows = []
    for py in range(size):
        row = bytearray([0])  # PNG filter type
        for px in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(ss):
                for sx in range(ss):
                    x = (px + (sx + 0.5) / ss) * 64 / size
                    y = (py + (sy + 0.5) / ss) * 64 / size
                    c = coverage(x, y)
                    if c:
                        acc[0] += c[0]; acc[1] += c[1]; acc[2] += c[2]; acc[3] += 255
            n = ss * ss
            a = acc[3] // n
            if a:
                k = acc[3] // 255
                row += bytes([acc[0] // k, acc[1] // k, acc[2] // k, a])
            else:
                row += bytes(4)
        rows.append(bytes(row))
    return rows


def png(size):
    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    raw = b"".join(render(size))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b""))


def main():
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = [png(s) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries, data = b"", b""
    for s, img in zip(sizes, images):
        entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(img), offset + len(data))
        data += img
    out = os.path.join(os.path.dirname(__file__), "..", "assets", "icon.ico")
    with open(out, "wb") as f:
        f.write(header + entries + data)
    print("wrote", os.path.normpath(out), len(header + entries + data), "bytes")


if __name__ == "__main__":
    main()
