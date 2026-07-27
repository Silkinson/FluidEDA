#!/usr/bin/env python3
"""Generate assets/edit_fixture.gds — a tiny GDS for the Phase 3 edit golden tests.

Unlike hier.gds (whose TOP cell holds only references), this fixture's TOP cell
holds **direct boundary shapes**, because Phase 3 edits TOP-cell shapes and the
golden move/delete tests need a real editable shape at a known index.

    LEAF  -- 2 boundaries (a child cell, to prove editing TOP leaves children alone)
    TOP   -- 5 direct boundaries on layers 0..2, at known indices 0..4,
             + 1 SREF of LEAF off to the side

Shape indices in TOP are file order (a small cell is one chunk, unreordered), so
the tests can move index 0 and delete index 2 deterministically.

Reuses the raw-record encoding from gen_hier.py. Run:  python3 gen_edit_fixture.py
"""
import struct, os

HEADER, BGNLIB, LIBNAME, UNITS = 0x0002, 0x0102, 0x0206, 0x0305
BGNSTR, STRNAME, ENDSTR = 0x0502, 0x0606, 0x0700
BOUNDARY, SREF = 0x0800, 0x0A00
LAYER, DATATYPE, XY, ENDEL = 0x0D02, 0x0E02, 0x1003, 0x1100
SNAME, ENDLIB = 0x1206, 0x0400
DATES = [2024, 1, 1, 0, 0, 0, 2024, 1, 1, 0, 0, 0]


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
    x = abs(x); exp = 64
    while x >= 1.0:
        x /= 16.0; exp += 1
    while x < 1.0 / 16.0:
        x *= 16.0; exp -= 1
    mant = int(x * (1 << 56) + 0.5)
    if mant >= (1 << 56):
        mant >>= 4; exp += 1
    return bytes([sign | (exp & 0x7F)]) + mant.to_bytes(7, "big")


def boundary(layer, dt, x, y, w, h):
    pts = [(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)]
    xy = i4(*[c for p in pts for c in p])
    return rec(BOUNDARY) + rec(LAYER, i2(layer)) + rec(DATATYPE, i2(dt)) + rec(XY, xy) + rec(ENDEL)


def sref(name, x, y):
    return rec(SREF) + rec(SNAME, name.encode() + b"\x00" * (len(name) % 2)) + rec(XY, i4(x, y)) + rec(ENDEL)


def struct_(name, body):
    return rec(BGNSTR, i2(*DATES)) + rec(STRNAME, name.encode() + b"\x00" * (len(name) % 2)) + body + rec(ENDSTR)


# LEAF child: two small boxes on layers 1 and 3.
LEAF = [(1, 0, 0, 0, 200, 200), (3, 0, 60, 60, 80, 80)]

# TOP direct shapes, indices 0..4. Spread out so a move/delete is visually obvious.
TOP_SHAPES = [
    (0, 0, 1000, 1000, 1500, 1500),   # 0: big base tile, layer 0  (the MOVE target)
    (1, 0, 1200, 1200, 400, 400),     # 1: layer-1 box inside it
    (2, 0, 4000, 1000, 800, 2000),    # 2: tall layer-2 box        (the DELETE target)
    (1, 0, 1000, 3200, 3800, 300),    # 3: wide layer-1 strip
    (2, 0, 5200, 3400, 600, 600),     # 4: lone layer-2 box
]


def build():
    leaf = struct_("LEAF", b"".join(boundary(*s) for s in LEAF))
    top_body = b"".join(boundary(*s) for s in TOP_SHAPES)
    top_body += sref("LEAF", 200, 4200)          # child, well away from TOP shapes
    top = struct_("TOP", top_body)

    lib = rec(HEADER, i2(600))
    lib += rec(BGNLIB, i2(*DATES))
    lib += rec(LIBNAME, b"EDIT.DB\x00")
    lib += rec(UNITS, gds_real(1e-3) + gds_real(1e-9))
    lib += leaf + top
    lib += rec(ENDLIB)
    return lib


if __name__ == "__main__":
    data = build()
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "edit_fixture.gds")
    with open(out, "wb") as f:
        f.write(data)
    print(f"wrote {out}: {len(data)} bytes")
    print(f"TOP direct shapes: {len(TOP_SHAPES)} (indices 0..{len(TOP_SHAPES)-1}) + 1 SREF of LEAF")
