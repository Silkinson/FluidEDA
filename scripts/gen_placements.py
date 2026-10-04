#!/usr/bin/env python3
"""Generate a GDS whose size is in its PLACEMENTS, not its shapes (Phase 7.3):
a TOP cell holding N single placements (SREFs) of four small standard-cell-like
leaf cells, packed in rows like a placed-and-routed block. The leaf cells have
a handful of rects each, so the file is ~26 bytes per placement and the
hierarchy itself is what has to scale: in-core, N instance records must fit
in the browser; streamed, only the placement chunks on screen are fetched.

Usage: python3 scripts/gen_placements.py N_MILLIONS   (e.g. 50)
Output: testdata/place_<N>M.gds  (gitignored)
"""
import os, struct, sys

HEADER, BGNLIB, LIBNAME, UNITS = 0x0002, 0x0102, 0x0206, 0x0305
BGNSTR, STRNAME, ENDSTR = 0x0502, 0x0606, 0x0700
BOUNDARY, LAYER, DATATYPE, XY, ENDEL = 0x0800, 0x0D02, 0x0E02, 0x1003, 0x1100
SREF, SNAME = 0x0A00, 0x1206
ENDLIB = 0x0400
DATES = [2026, 10, 3, 0, 0, 0, 2026, 10, 3, 0, 0, 0]
ROW_H = 2_720  # nm, a sky130 standard-cell row


def rec(rtype, payload=b""):
    if len(payload) % 2:
        payload += b"\x00"
    return struct.pack(">HH", len(payload) + 4, rtype) + payload


def i2(*v):
    return b"".join(struct.pack(">h", x) for x in v)


def i4(*v):
    return b"".join(struct.pack(">i", x) for x in v)


def gds_real(x):
    if x == 0:
        return b"\x00" * 8
    sign = 0x80 if x < 0 else 0
    x = abs(x)
    exp = 64
    while x >= 1.0:
        x /= 16.0
        exp += 1
    while x < 1 / 16.0:
        x *= 16.0
        exp -= 1
    m = int(x * (1 << 56) + 0.5)
    if m >= (1 << 56):
        m >>= 4
        exp += 1
    return bytes([sign | (exp & 0x7F)]) + m.to_bytes(7, "big")


def rect(layer, x0, y0, x1, y1):
    pts = i4(x0, y0, x1, y0, x1, y1, x0, y1, x0, y0)
    return rec(BOUNDARY) + rec(LAYER, i2(layer)) + rec(DATATYPE, i2(20)) + rec(XY, pts) + rec(ENDEL)


def main():
    n = int(float(sys.argv[1]) * 1_000_000) if len(sys.argv) > 1 else 50_000_000
    tag = sys.argv[1] if len(sys.argv) > 1 else "50"
    out = os.path.abspath(os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "testdata", f"place_{tag}M.gds"))
    os.makedirs(os.path.dirname(out), exist_ok=True)

    widths = [460, 920, 1_380, 2_760]  # four leaf cells, in standard-cell sites
    names = [f"C{i}".encode() for i in range(len(widths))]
    avg_w = sum(widths) / len(widths)
    per_row = max(1, int((n * avg_w * ROW_H) ** 0.5 / avg_w))

    with open(out, "wb") as f:
        f.write(rec(HEADER, i2(600)))
        f.write(rec(BGNLIB, i2(*DATES)))
        f.write(rec(LIBNAME, b"PLACE.DB"))
        f.write(rec(UNITS, gds_real(1e-3) + gds_real(1e-9)))
        # Leaf cells: an n-well, a diffusion strip, a poly finger per site, li1 rails.
        for name, w in zip(names, widths):
            f.write(rec(BGNSTR, i2(*DATES)))
            f.write(rec(STRNAME, name))
            f.write(rect(64, 0, 1_360, w, ROW_H))
            f.write(rect(65, 60, 400, w - 60, 2_300))
            for x in range(150, w - 100, 460):
                f.write(rect(66, x, 300, x + 150, 2_420))
            f.write(rect(67, 0, -170, w, 170))
            f.write(rect(67, 0, ROW_H - 170, w, ROW_H + 170))
            f.write(rec(ENDSTR))
        # TOP: N placements in rows, left to right, cycling the four widths.
        f.write(rec(BGNSTR, i2(*DATES)))
        f.write(rec(STRNAME, b"TOP"))
        srefs = [rec(SREF) + rec(SNAME, nm) for nm in names]
        xy_head = struct.pack(">HH", 12, XY)
        tail = rec(ENDEL)
        buf = bytearray()
        x = y = col = 0
        for i in range(n):
            k = i % 4
            buf += srefs[k]
            buf += xy_head
            buf += struct.pack(">ii", x, y)
            buf += tail
            x += widths[k]
            col += 1
            if col == per_row:
                col = 0
                x = 0
                y += ROW_H
            if len(buf) > (16 << 20):
                f.write(buf)
                buf = bytearray()
        f.write(buf)
        f.write(rec(ENDSTR))
        f.write(rec(ENDLIB))

    sz = os.path.getsize(out)
    print(f"wrote {out}: {n:,} placements in rows of {per_row:,}, {sz / 1e6:.0f} MB")


if __name__ == "__main__":
    main()
