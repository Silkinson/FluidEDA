#!/usr/bin/env python3
"""Merge every sky130_fd_sc_hd per-cell GDS into one layout (klayout -b).

Produces a single file carrying thousands of DISTINCT cell definitions -- the
input for the tessellation / cell-count reality check. Also wraps them in a
grid-instanced TOP cell so the viewer actually has something to cull/draw and so
the file has a single unambiguous top (otherwise every std cell is its own top).

Env: SKY_SRC = cloned repo dir, SKY_OUT = output .gds path.
"""
import os, glob, math
import pya

src = os.environ["SKY_SRC"]
out = os.environ["SKY_OUT"]

files = sorted(glob.glob(os.path.join(src, "cells", "*", "*.gds")))
print(f"merging {len(files)} per-cell GDS files")

ly = pya.Layout()
ly.dbu = 0.001
top_names = []
for i, f in enumerate(files):
    tmp = pya.Layout()
    tmp.read(f)
    for tc in tmp.top_cells():
        # Copy the whole cell tree into the merged layout.
        name = tc.name
        if ly.cell(name) is not None:
            continue
        dest = ly.create_cell(name)
        dest.copy_tree(tc)
        top_names.append(name)
    if i % 100 == 0:
        print(f"  {i}/{len(files)}")

# Wrap all merged cells into a grid TOP so there is one top + real hierarchy.
top = ly.create_cell("TOP")
n = len(top_names)
side = max(1, int(math.ceil(math.sqrt(n))))
PITCH = 20000  # 20 um grid, comfortably bigger than any std cell
for idx, name in enumerate(top_names):
    c = ly.cell(name)
    if c is None:
        continue
    r, col = divmod(idx, side)
    t = pya.Trans(pya.Vector(col * PITCH, r * PITCH))
    top.insert(pya.CellInstArray(c.cell_index(), t))

ly.write(out)
print(f"wrote {out}: {n} cell defs wrapped into TOP ({side}x{side} grid)")
