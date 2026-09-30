# KLayout batch script (Phase 6 oracle): dump every cell's geometry and
# placements in a normalized, reader-independent form, for comparison with
# FluidEDA's readers (crates/core/examples/layout_dump.rs).
#
#   klayout -b -r scripts/oracle/klayout_dump.py -rd src=<file.gds|.oas> -rd dst=<out.json>
#
# Output: JSON lines, one object per cell {"cell", "polygons", "instances"}.
# Polygons: boxes, polygons and paths (converted with KLayout's own
# Path.polygon()), texts skipped. Each as [layer, datatype, [[x, y], ...]] in
# database units, normalized by scripts/oracle/compare.py. Placements:
# [child, x, y, rot, mirror, mag, na, nb, ax, ay, bx, by] (na = nb = 1 for a
# single placement).
import json
import pya

ly = pya.Layout()
ly.read(src)
f = open(dst, "w")
n = 0
for cell in ly.each_cell():
    polys = []
    for li in ly.layer_indexes():
        info = ly.get_info(li)
        it = cell.shapes(li).each(pya.Shapes.SBoxes | pya.Shapes.SPolygons | pya.Shapes.SPaths)
        for sh in it:
            if sh.is_box():
                p = pya.Polygon(sh.box)
            elif sh.is_path():
                p = sh.path.polygon()
            else:
                p = sh.polygon
            polys.append([info.layer, info.datatype, [[pt.x, pt.y] for pt in p.each_point_hull()]])
    insts = []
    for inst in cell.each_inst():
        t = inst.cplx_trans
        child = ly.cell(inst.cell_index).name
        if inst.is_regular_array():
            a, b = inst.a, inst.b
            insts.append([child, t.disp.x, t.disp.y, t.angle, t.is_mirror(), t.mag, inst.na, inst.nb, a.x, a.y, b.x, b.y])
        else:
            insts.append([child, t.disp.x, t.disp.y, t.angle, t.is_mirror(), t.mag, 1, 1, 0, 0, 0, 0])
    f.write(json.dumps({"cell": cell.name, "polygons": polys, "instances": insts}) + "\n")
    n += 1
f.close()
print("dumped", n, "cells to", dst)
