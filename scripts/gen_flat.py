#!/usr/bin/env python3
"""Generate a FLAT GDS with N distinct boundary polygons (no hierarchy) to probe
the wasm parse-memory ceiling. Unlike hier.gds (AREF replication), every polygon
is a real record, so parse footprint scales with N -- the pathological worst case
for memory. Reuses the record encoder from gen_hier.py.

Usage: python3 gen_flat.py N_MILLIONS   (e.g. 5  -> ~5,000,000 polys)
Output: testdata/flat_<N>M.gds  (gitignored)
"""
import struct, os, sys

# --- GDSII record encoding (same as gen_hier.py) ---
HEADER, BGNLIB, LIBNAME, UNITS = 0x0002, 0x0102, 0x0206, 0x0305
BGNSTR, STRNAME, ENDSTR = 0x0502, 0x0606, 0x0700
BOUNDARY, LAYER, DATATYPE, XY, ENDEL = 0x0800, 0x0D02, 0x0E02, 0x1003, 0x1100
ENDLIB = 0x0400
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
    while x >= 1.0: x /= 16.0; exp += 1
    while x < 1/16.0: x *= 16.0; exp -= 1
    m = int(x * (1 << 56) + 0.5)
    if m >= (1 << 56): m >>= 4; exp += 1
    return bytes([sign | (exp & 0x7F)]) + m.to_bytes(7, "big")


def main():
    n = int(float(sys.argv[1]) * 1_000_000) if len(sys.argv) > 1 else 5_000_000
    out = os.path.join(os.path.dirname(os.path.abspath(__file__)), "..", "testdata",
                       f"flat_{sys.argv[1] if len(sys.argv)>1 else '5'}M.gds")
    out = os.path.abspath(out)
    os.makedirs(os.path.dirname(out), exist_ok=True)

    # A grid of small rects across 5 layers. Precompute the closing-point template.
    W, H = 100, 100          # nm per cell
    PITCH = 200
    side = int(n ** 0.5) + 1

    with open(out, "wb") as f:
        f.write(rec(HEADER, i2(600)))
        f.write(rec(BGNLIB, i2(*DATES)))
        f.write(rec(LIBNAME, b"FLAT.DB\x00"))
        f.write(rec(UNITS, gds_real(1e-3) + gds_real(1e-9)))
        f.write(rec(BGNSTR, i2(*DATES)))
        f.write(rec(STRNAME, b"TOP\x00"))

        # Stream boundaries directly to disk in chunks to keep the generator light.
        buf = bytearray()
        for idx in range(n):
            r, c = divmod(idx, side)
            x, y = c * PITCH, r * PITCH
            layer = idx % 5
            pts = i4(x, y, x + W, y, x + W, y + H, x, y + H, x, y)
            buf += rec(BOUNDARY) + rec(LAYER, i2(layer)) + rec(DATATYPE, i2(0)) + rec(XY, pts) + rec(ENDEL)
            if len(buf) > (8 << 20):
                f.write(buf); buf = bytearray()
        if buf:
            f.write(buf)

        f.write(rec(ENDSTR))
        f.write(rec(ENDLIB))

    sz = os.path.getsize(out)
    print(f"wrote {out}: {n:,} polys, {sz/1e6:.0f} MB")


if __name__ == "__main__":
    main()
