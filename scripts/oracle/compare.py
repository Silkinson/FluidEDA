#!/usr/bin/env python3
"""Compare two layout dumps (KLayout's vs FluidEDA's) cell by cell.

  python3 scripts/oracle/compare.py <a.json> <b.json>

Both inputs are JSON lines, one cell per line. Polygons are normalized before
comparison — closing/duplicate/collinear points removed, counter-clockwise,
started at the smallest vertex — so readers that store the same shape with a
different vertex order still agree; each cell's polygons are compared as a
sorted-multiset digest (so a 2.5 M-polygon cell streams). Placements are
compared as multisets with arrays canonicalized (see canon_inst).
Exits non-zero on any difference, printing the first few.
"""
import hashlib, json, sys
from collections import Counter

def norm(pts):
    p = [tuple(q) for q in pts]
    if len(p) > 1 and p[0] == p[-1]:
        p.pop()
    out = []
    for q in p:
        if not out or out[-1] != q:
            out.append(q)
    if len(out) > 1 and out[0] == out[-1]:
        out.pop()
    changed = True
    while changed and len(out) >= 3:
        changed = False
        for i in range(len(out)):
            a, b, c = out[i - 1], out[i], out[(i + 1) % len(out)]
            if (b[0] - a[0]) * (c[1] - b[1]) - (b[1] - a[1]) * (c[0] - b[0]) == 0:
                out.pop(i)
                changed = True
                break
    area = sum(out[i - 1][0] * out[i][1] - out[i][0] * out[i - 1][1] for i in range(len(out)))
    if area < 0:
        out.reverse()
    k = out.index(min(out)) if out else 0
    return tuple(out[k:] + out[:k])

def canon_inst(i):
    """(child, x, y, rot, mirror, mag, na, nb, ax, ay, bx, by) with the array
    canonicalized: a dimension of count 1 has no step, and the two (count,
    step) pairs are sorted — KLayout and FluidEDA may assign an AREF's
    rows/columns to the other axis for the same set of placements."""
    child, x, y, rot, mirror, mag, na, nb, ax, ay, bx, by = i
    dims = sorted(((na, (ax, ay)) if na > 1 else (1, (0, 0)), (nb, (bx, by)) if nb > 1 else (1, (0, 0))))
    return (child, x, y, round(rot, 3), mirror, round(mag, 3), dims[0], dims[1])

def cells(path):
    """cell name -> (polygon count, polygon digest, placement multiset)."""
    res = {}
    with open(path) as f:
        for line in f:
            c = json.loads(line)
            polys = sorted(repr((l, dt, norm(p))) for l, dt, p in c["polygons"])
            h = hashlib.sha1("\n".join(polys).encode()).hexdigest()
            insts = Counter(canon_inst(i) for i in c["instances"])
            res[c["cell"]] = (len(polys), h, insts)
    return res

a, b = cells(sys.argv[1]), cells(sys.argv[2])
bad = []
for name in sorted(set(a) | set(b)):
    if name not in a or name not in b:
        bad.append(f"cell {name}: only in {'A' if name in a else 'B'}")
        continue
    (na_, ha, ia), (nb_, hb, ib) = a[name], b[name]
    if ha != hb:
        bad.append(f"cell {name}: polygons differ ({na_} in A, {nb_} in B)")
    if ia != ib:
        bad.append(f"cell {name}: placements differ; A-only {list((ia - ib))[:2]} B-only {list((ib - ia))[:2]}")
total_polys = sum(n for n, _, _ in a.values())
total_insts = sum(sum(i.values()) for _, _, i in a.values())
if bad:
    print(f"MISMATCH in {len(bad)} place(s):")
    for x in bad[:12]:
        print("  " + x)
    sys.exit(1)
print(f"MATCH: {len(a)} cells, {total_polys} polygons, {total_insts} placements identical")
