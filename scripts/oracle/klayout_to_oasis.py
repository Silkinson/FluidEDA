# KLayout batch script (Phase 6 oracle): convert a layout to OASIS with
# KLayout's own writer — the reference OASIS files FluidEDA's reader is
# checked against.
#
#   klayout -b -r scripts/oracle/klayout_to_oasis.py -rd src=<in.gds> -rd dst=<out.oas> [-rd level=2] [-rd cblocks=1]
#
# Then: dump both readings (klayout_dump.py / the layout_dump example) and
# compare them with compare.py.
import pya

ly = pya.Layout()
ly.read(src)
opt = pya.SaveLayoutOptions()
opt.format = "OASIS"
opt.oasis_compression_level = int(globals().get("level", "2"))
opt.oasis_write_cblocks = globals().get("cblocks", "1") == "1"
opt.oasis_strict_mode = True
ly.write(dst, opt)
print("wrote", dst)
