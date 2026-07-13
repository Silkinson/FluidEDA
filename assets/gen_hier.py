#!/usr/bin/env python3
"""Generate a HIERARCHICAL test GDSII: assets/hier.gds.

Structure (nothing flattened on disk):

    LEAF   -- ~10 small boundaries across layers 0..4 (a stand-cell-like tile)
    ROW    -- AREF of LEAF, cols x 1  (a horizontal strip of leaves)
    TOP    -- AREF of ROW,  1 x rows  (stack the strips into a block)
              + one rotated/reflected SREF of LEAF to exercise STRANS

With cols = rows = 1000 and 10 shapes/leaf the *effective flattened* shape
count is 10 * 1000 * 1000 = 10,000,000, yet the file stays a few hundred bytes
of records: the array references are stored once. The block is placed at a
multi-millimetre offset so f32 world coordinates jitter without origin
rebasing -- exactly the Phase 1 precision test.

Writes raw GDSII records (no third-party deps). Run:  python3 gen_hier.py
"""
import struct, os

# ---- tunables -------------------------------------------------------------
COLS = 1000            # leaves per row (AREF columns in ROW)
ROWS = 1000            # rows per block (AREF rows in TOP)
COL_PITCH = 1000       # nm between leaf columns
ROW_PITCH = 1200       # nm between rows
ORIGIN = (5_000_000, 5_000_000)  # block placed ~5 mm out -> forces rebasing

# ---- GDSII record encoding ------------------------------------------------
HEADER, BGNLIB, LIBNAME, UNITS = 0x0002, 0x0102, 0x0206, 0x0305
BGNSTR, STRNAME, ENDSTR = 0x0502, 0x0606, 0x0700
BOUNDARY, SREF, AREF = 0x0800, 0x0A00, 0x0B00
LAYER, DATATYPE, XY, ENDEL = 0x0D02, 0x0E02, 0x1003, 0x1100
SNAME, COLROW = 0x1206, 0x1302
STRANS, MAG, ANGLE = 0x1A01, 0x1B05, 0x1C05
ENDLIB = 0x0400

# Modification/access timestamps: 12 int2 = {mod y,m,d,h,mn,s, acc y,m,d,h,mn,s}.
# Must be a valid calendar date (gds21 parses these via chrono).
DATES = [2024, 1, 1, 0, 0, 0, 2024, 1, 1, 0, 0, 0]


def rec(rtype, payload=b""):
    if len(payload) % 2:            # records must be even length
        payload += b"\x00"
    return struct.pack(">HH", len(payload) + 4, rtype) + payload


def i2(*vals):
    return b"".join(struct.pack(">h", v) for v in vals)


def i4(*vals):
    return b"".join(struct.pack(">i", v) for v in vals)


def gds_real(x):
    """8-byte GDSII real: sign, 7-bit excess-64 base-16 exponent, 56-bit mantissa."""
    if x == 0:
        return b"\x00" * 8
    sign = 0x80 if x < 0 else 0
    x = abs(x)
    exp = 64
    while x >= 1.0:
        x /= 16.0
        exp += 1
    while x < 1.0 / 16.0:
        x *= 16.0
        exp -= 1
    mant = int(x * (1 << 56) + 0.5)
    if mant >= (1 << 56):           # rounding carried past the top nibble
        mant >>= 4
        exp += 1
    return bytes([sign | (exp & 0x7F)]) + mant.to_bytes(7, "big")


def boundary(layer, dt, x, y, w, h):
    # Closed ring, first point repeated at the end (per spec).
    pts = [(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)]
    xy = i4(*[c for p in pts for c in p])
    return rec(BOUNDARY) + rec(LAYER, i2(layer)) + rec(DATATYPE, i2(dt)) + rec(XY, xy) + rec(ENDEL)


def strans(angle=0.0, reflect=False, mag=1.0):
    flags = 0x8000 if reflect else 0x0000
    out = rec(STRANS, struct.pack(">H", flags))
    if mag != 1.0:
        out += rec(MAG, gds_real(mag))
    if angle != 0.0:
        out += rec(ANGLE, gds_real(angle))
    return out


def sref(name, x, y, angle=0.0, reflect=False, mag=1.0):
    body = rec(SREF) + rec(SNAME, name.encode() + b"\x00" * (len(name) % 2))
    if angle or reflect or mag != 1.0:
        body += strans(angle, reflect, mag)
    return body + rec(XY, i4(x, y)) + rec(ENDEL)


def aref(name, x0, y0, cols, rows, col_pitch, row_pitch, angle=0.0, reflect=False, mag=1.0):
    # Three-point xy: reference, reference+cols*col_pitch (col direction),
    # reference+rows*row_pitch (row direction). The loader recovers per-step
    # vectors by dividing by cols/rows.
    p0 = (x0, y0)
    p1 = (x0 + cols * col_pitch, y0)
    p2 = (x0, y0 + rows * row_pitch)
    body = rec(AREF) + rec(SNAME, name.encode() + b"\x00" * (len(name) % 2))
    if angle or reflect or mag != 1.0:
        body += strans(angle, reflect, mag)
    body += rec(COLROW, i2(cols, rows))
    body += rec(XY, i4(p0[0], p0[1], p1[0], p1[1], p2[0], p2[1]))
    return body + rec(ENDEL)


def struct_(name, body):
    return rec(BGNSTR, i2(*DATES)) + rec(STRNAME, name.encode() + b"\x00" * (len(name) % 2)) + body + rec(ENDSTR)


# ---- LEAF: ~10 boundaries across layers 0..4 ------------------------------
LEAF_SHAPES = [
    (0, 0, 0, 0, 360, 400),      # base tile
    (1, 0, 40, 60, 60, 280),     # poly gate 1
    (1, 0, 200, 60, 60, 280),    # poly gate 2
    (1, 0, 120, 60, 20, 280),    # poly link
    (2, 0, 20, 300, 320, 60),    # metal top
    (2, 0, 20, 40, 320, 60),     # metal bottom
    (2, 0, 160, 180, 40, 40),    # metal patch
    (3, 0, 60, 180, 40, 40),     # contact 1
    (3, 0, 220, 180, 40, 40),    # contact 2
    (4, 0, 150, 150, 60, 100),   # via
]


def build():
    leaf_body = b"".join(boundary(*s) for s in LEAF_SHAPES)
    leaf = struct_("LEAF", leaf_body)

    # ROW: a horizontal strip of COLS leaves.
    row = struct_("ROW", aref("LEAF", 0, 0, COLS, 1, COL_PITCH, ROW_PITCH))

    # TOP: stack ROWS strips, plus one rotated+reflected leaf as a landmark.
    ox, oy = ORIGIN
    top_body = aref("ROW", ox, oy, 1, ROWS, COL_PITCH, ROW_PITCH)
    top_body += sref("LEAF", ox - 3000, oy - 3000, angle=90.0, reflect=True)
    top = struct_("TOP", top_body)

    lib = rec(HEADER, i2(600))
    lib += rec(BGNLIB, i2(*DATES))
    lib += rec(LIBNAME, b"HIER.DB\x00")
    lib += rec(UNITS, gds_real(1e-3) + gds_real(1e-9))   # 1 DB unit = 1 nm
    lib += leaf + row + top
    lib += rec(ENDLIB)
    return lib


if __name__ == "__main__":
    data = build()
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "hier.gds")
    with open(out, "wb") as f:
        f.write(data)
    flat = len(LEAF_SHAPES) * COLS * ROWS
    print(f"wrote {out}: {len(data)} bytes")
    print(f"hierarchy: LEAF({len(LEAF_SHAPES)} shapes) -> ROW(AREF {COLS}x1) -> TOP(AREF 1x{ROWS})")
    print(f"effective flattened shapes: {flat:,}")
