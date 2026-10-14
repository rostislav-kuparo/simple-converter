"""Renders the app icon (dark rounded square with two arrows) into assets/icon.ico and icon.png.

Pure Python, no dependencies: every size is rasterised separately with supersampling,
so the small sizes stay crisp instead of being blurry downscales.
"""
import math
import os
import struct
import zlib

HERE = os.path.dirname(os.path.abspath(__file__))
SIZES = [16, 20, 24, 32, 40, 48, 64, 96, 128, 256]

TOP, BOTTOM = (40, 40, 42), (10, 10, 11)  # background gradient
EDGE = (70, 70, 74)  # hairline so the icon does not vanish on a dark taskbar
FG = (255, 255, 255)

# Two arrows in unit coordinates: (shaft start, shaft end, head end).
ARROWS = [
    ((0.29, 0.39), (0.71, 0.39), (0.59, 0.27)),
    ((0.71, 0.61), (0.29, 0.61), (0.41, 0.73)),
]


def rounded_rect_sd(x, y, x0, y0, x1, y1, r):
    cx, cy = (x0 + x1) / 2, (y0 + y1) / 2
    qx = abs(x - cx) - ((x1 - x0) / 2 - r)
    qy = abs(y - cy) - ((y1 - y0) / 2 - r)
    return math.hypot(max(qx, 0), max(qy, 0)) + min(max(qx, qy), 0) - r


def segment_dist(x, y, ax, ay, bx, by):
    dx, dy = bx - ax, by - ay
    t = max(0.0, min(1.0, ((x - ax) * dx + (y - ay) * dy) / (dx * dx + dy * dy)))
    return math.hypot(x - ax - t * dx, y - ay - t * dy)


def render(size):
    margin = 0.035 if size >= 48 else 0.0
    radius = 0.23
    stroke = max(0.078, 1.7 / size) / 2
    edge = max(0.8 / size, 0.006) if size >= 32 else 0.0
    segments = []
    for start, end, head in ARROWS:
        segments.append((*start, *end))
        segments.append((*end, *head))

    n = 4 if size >= 128 else 8
    pixels = bytearray()
    for py in range(size):
        for px in range(size):
            r = g = b = a = 0.0
            for sy in range(n):
                for sx in range(n):
                    x = (px + (sx + 0.5) / n) / size
                    y = (py + (sy + 0.5) / n) / size
                    sd = rounded_rect_sd(x, y, margin, margin, 1 - margin, 1 - margin, radius - margin)
                    if sd > 0:
                        continue
                    if any(segment_dist(x, y, *s) <= stroke for s in segments):
                        c = FG
                    elif sd > -edge:
                        c = EDGE
                    else:
                        t = (y - margin) / (1 - 2 * margin)
                        c = tuple(TOP[i] + (BOTTOM[i] - TOP[i]) * t for i in range(3))
                    r, g, b, a = r + c[0], g + c[1], b + c[2], a + 1
            k = n * n
            if a:
                pixels += bytes((round(r / a), round(g / a), round(b / a), round(255 * a / k)))
            else:
                pixels += b"\0\0\0\0"
    return pixels


def png(size, rgba):
    raw = b"".join(b"\0" + rgba[y * size * 4:(y + 1) * size * 4] for y in range(size))

    def chunk(tag, data):
        return struct.pack(">I", len(data)) + tag + data + struct.pack(">I", zlib.crc32(tag + data))

    ihdr = struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0)
    return b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", ihdr) + chunk(b"IDAT", zlib.compress(raw, 9)) + chunk(b"IEND", b"")


def dib(size, rgba):
    """Classic BMP entry: the most compatible form for sizes below 256."""
    header = struct.pack("<IiiHHIIiiII", 40, size, size * 2, 1, 32, 0, 0, 0, 0, 0, 0)
    rows = []
    for y in reversed(range(size)):
        row = rgba[y * size * 4:(y + 1) * size * 4]
        rows.append(b"".join(bytes((row[i + 2], row[i + 1], row[i], row[i + 3])) for i in range(0, len(row), 4)))
    mask_row = ((size + 31) // 32) * 4
    return header + b"".join(rows) + b"\0" * mask_row * size


def main():
    images = []
    for size in SIZES:
        rgba = render(size)
        images.append((size, png(size, rgba) if size >= 256 else dib(size, rgba)))
        if size == 256:
            with open(os.path.join(HERE, "icon.png"), "wb") as f:
                f.write(png(size, rgba))

    offset = 6 + 16 * len(images)
    header = struct.pack("<HHH", 0, 1, len(images))
    entries, blobs = b"", b""
    for size, data in images:
        entries += struct.pack("<BBBBHHII", size % 256, size % 256, 0, 0, 1, 32, len(data), offset + len(blobs))
        blobs += data
    with open(os.path.join(HERE, "icon.ico"), "wb") as f:
        f.write(header + entries + blobs)
    print("icon.ico:", ", ".join(str(s) for s in SIZES))


if __name__ == "__main__":
    main()
