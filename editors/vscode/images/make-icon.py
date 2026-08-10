#!/usr/bin/env python3
"""Generate the cmakedb extension icon as a PNG, no external deps.

Design: a provenance chain — three nodes joined top-to-bottom by edges,
the shape `why-links` prints — sitting on a dark rounded square. The mark
is original; it deliberately does not echo CMake's logo, which is
Kitware's trademark.

Rendered at 4x and box-downsampled for anti-aliasing.
"""
import math
import struct
import zlib

S = 512          # final size
SS = 4           # supersample factor
W = S * SS

BG = (0x1B, 0x22, 0x2B)      # slate
ACCENT = (0x4E, 0xC9, 0xB0)  # teal (reads on both marketplace themes)
EDGE = (0x8A, 0x9B, 0xA8)    # muted steel
NODE_HI = (0xE8, 0xF0, 0xF4)

buf = bytearray()
for _ in range(W * W):
    buf += bytes((0, 0, 0, 0))


def px(x, y, rgb, a=1.0):
    if not (0 <= x < W and 0 <= y < W) or a <= 0:
        return
    i = (y * W + x) * 4
    sr, sg, sb, sa = buf[i], buf[i + 1], buf[i + 2], buf[i + 3] / 255.0
    out_a = a + sa * (1 - a)
    if out_a <= 0:
        return
    for k, c in enumerate(rgb):
        buf[i + k] = int(round((c * a + (sr, sg, sb)[k] * sa * (1 - a)) / out_a))
    buf[i + 3] = int(round(out_a * 255))


def rounded_rect(x0, y0, x1, y1, r, rgb):
    for y in range(int(y0), int(y1)):
        for x in range(int(x0), int(x1)):
            dx = max(x0 + r - x, 0, x - (x1 - r - 1))
            dy = max(y0 + r - y, 0, y - (y1 - r - 1))
            if dx * dx + dy * dy <= r * r:
                px(x, y, rgb, 1.0)


def disc(cx, cy, r, rgb):
    for y in range(int(cy - r) - 1, int(cy + r) + 2):
        for x in range(int(cx - r) - 1, int(cx + r) + 2):
            if (x - cx) ** 2 + (y - cy) ** 2 <= r * r:
                px(x, y, rgb, 1.0)


def ring(cx, cy, r, thick, rgb):
    inner = r - thick
    for y in range(int(cy - r) - 1, int(cy + r) + 2):
        for x in range(int(cx - r) - 1, int(cx + r) + 2):
            d2 = (x - cx) ** 2 + (y - cy) ** 2
            if inner * inner <= d2 <= r * r:
                px(x, y, rgb, 1.0)


def thick_line(x0, y0, x1, y1, w, rgb):
    n = int(math.hypot(x1 - x0, y1 - y0)) + 1
    for i in range(n + 1):
        t = i / n
        disc(x0 + (x1 - x0) * t, y0 + (y1 - y0) * t, w / 2, rgb)


# Backplate
rounded_rect(0, 0, W, W, int(0.18 * W), BG)

# Provenance chain: root, middle, leaf — offset to suggest a walk.
cx = W * 0.355
nodes = [
    (cx + W * 0.10, W * 0.24, W * 0.070, ACCENT),
    (cx, W * 0.50, W * 0.070, ACCENT),
    (cx + W * 0.20, W * 0.76, W * 0.070, ACCENT),
]
for (x0, y0, _, _), (x1, y1, _, _) in zip(nodes, nodes[1:]):
    thick_line(x0, y0, x1, y1, W * 0.030, EDGE)
for x, y, r, c in nodes:
    disc(x, y, r, c)
    disc(x, y, r * 0.42, BG)

# A second branch off the middle node: propagation fans out.
mx, my = nodes[1][0], nodes[1][1]
bx, by = cx + W * 0.30, W * 0.50
thick_line(mx, my, bx, by, W * 0.030, EDGE)
ring(bx, by, W * 0.060, W * 0.024, NODE_HI)

# Downsample (box filter) onto an opaque canvas.
out = bytearray()
for y in range(S):
    out.append(0)
    for x in range(S):
        acc = [0, 0, 0, 0]
        for dy in range(SS):
            for dx in range(SS):
                i = ((y * SS + dy) * W + (x * SS + dx)) * 4
                a = buf[i + 3] / 255.0
                for k in range(3):
                    acc[k] += buf[i + k] * a
                acc[3] += a
        n = SS * SS
        a = acc[3] / n
        if a > 0:
            out += bytes(int(round(acc[k] / acc[3])) for k in range(3))
        else:
            out += bytes((0, 0, 0))
        out.append(int(round(a * 255)))


def chunk(tag, data):
    return (
        struct.pack(">I", len(data))
        + tag
        + data
        + struct.pack(">I", zlib.crc32(tag + data) & 0xFFFFFFFF)
    )


png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", S, S, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(bytes(out), 9))
png += chunk(b"IEND", b"")

import sys

open(sys.argv[1], "wb").write(png)
print(f"wrote {sys.argv[1]} ({S}x{S}, {len(png)} bytes)")
