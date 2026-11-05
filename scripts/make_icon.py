"""Generate assets/icon.ico (16-256 px) and assets/icon.svg from the app icon
geometry (mirrors src/appicon.rs): a navy tile, a white envelope with a navy
flap seam, and a light-blue folded corner. Small sizes drop the fold and
thicken the seam. Standard library only. Run: python scripts/make_icon.py
"""
import math
import os
import struct
import zlib

NAVY = (0x17, 0x36, 0x5C)
WHITE = (0xFF, 0xFF, 0xFF)
FOLD = (0x70, 0xA9, 0xED)


def in_round_rect(x, y, l, t, r, b, rad):
    if x < l or x > r or y < t or y > b:
        return False
    cx, cy = min(max(x, l + rad), r - rad), min(max(y, t + rad), b - rad)
    return (x - cx) ** 2 + (y - cy) ** 2 <= rad * rad


def dist_to_segment(x, y, a, b):
    dx, dy = b[0] - a[0], b[1] - a[1]
    t = max(0.0, min(1.0, ((x - a[0]) * dx + (y - a[1]) * dy) / (dx * dx + dy * dy)))
    return math.hypot(x - (a[0] + t * dx), y - (a[1] + t * dy))


def envelope(small):
    return (14.0, 27.0, 86.0, 73.0) if small else (20.0, 32.0, 80.0, 70.0)


def sample(x, y, small):
    if not in_round_rect(x, y, 0, 0, 100, 100, 20):
        return None
    l, t, r, b = envelope(small)
    if not in_round_rect(x, y, l, t, r, b, 4):
        return NAVY
    if not small and x >= 68 and y <= 44 and (x - 68) >= (y - 32):
        return FOLD
    mid = ((l + r) / 2, t + (b - t) * 0.55)
    seam = 3.4 if small else 2.0
    if dist_to_segment(x, y, (l + 1, t + 1), mid) < seam or dist_to_segment(x, y, mid, (r - 1, t + 1)) < seam:
        return NAVY
    return WHITE


def render(size, ss=4):
    small = size <= 24
    rows = []
    for py in range(size):
        row = bytearray([0])
        for px in range(size):
            acc = [0, 0, 0, 0]
            for sy in range(ss):
                for sx in range(ss):
                    c = sample((px + (sx + 0.5) / ss) * 100 / size, (py + (sy + 0.5) / ss) * 100 / size, small)
                    if c:
                        acc[0] += c[0]; acc[1] += c[1]; acc[2] += c[2]; acc[3] += 255
            a = acc[3] // (ss * ss)
            if acc[3]:
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


SVG = """<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 100 100">
  <rect width="100" height="100" rx="20" fill="#17365C"/>
  <rect x="20" y="32" width="60" height="38" rx="4" fill="#FFFFFF"/>
  <path d="M21 33 L50 52.9 L79 33" fill="none" stroke="#17365C" stroke-width="4" stroke-linecap="round" stroke-linejoin="round"/>
  <path d="M68 32 L76 32 Q80 32 80 36 L80 44 Z" fill="#70A9ED"/>
</svg>
"""


def main():
    root = os.path.join(os.path.dirname(__file__), "..", "assets")
    sizes = [16, 24, 32, 48, 64, 128, 256]
    images = [png(s) for s in sizes]
    header = struct.pack("<HHH", 0, 1, len(sizes))
    offset = 6 + 16 * len(sizes)
    entries, data = b"", b""
    for s, img in zip(sizes, images):
        entries += struct.pack("<BBBBHHII", s % 256, s % 256, 0, 0, 1, 32, len(img), offset + len(data))
        data += img
    with open(os.path.join(root, "icon.ico"), "wb") as f:
        f.write(header + entries + data)
    with open(os.path.join(root, "icon.svg"), "w", newline="\n") as f:
        f.write(SVG)
    with open(os.path.join(root, "icon-256.png"), "wb") as f:
        f.write(images[-1])
    print("wrote icon.ico, icon.svg, icon-256.png")


if __name__ == "__main__":
    main()
