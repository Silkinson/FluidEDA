#!/usr/bin/env bash
# Fetch real open-PDK GDS for the Phase 1.5 reality check. These files are large
# and MUST NOT be committed (see .gitignore); this script reproduces them.
#
#   1. sky130_fd_sc_hd standard cells -> thousands of DISTINCT cell defs, merged
#      into one GDS. This is the tessellation / cell-count test (H1, H3).
#   2. efabless Caravel full SoC       -> real deep hierarchy, hundreds of MB.
#      This is the parse-throughput / memory test (H2).
#
# Output lands in ./testdata/ (gitignored). Re-running is idempotent.
#
# Requires: git, gunzip, and (for the sky130 merge) klayout.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
OUT="$ROOT/testdata"
mkdir -p "$OUT"
cd "$OUT"

log() { printf '\n=== %s ===\n' "$*"; }

# ---------------------------------------------------------------------------
# 1. sky130_fd_sc_hd standard cells -> one merged GDS with all cell defs.
# ---------------------------------------------------------------------------
fetch_sky130() {
  log "sky130_fd_sc_hd standard cells"
  local repo="https://github.com/google/skywater-pdk-libs-sky130_fd_sc_hd"
  local src="$OUT/sky130_fd_sc_hd"
  if [ ! -d "$src" ]; then
    # Shallow, blobless clone keeps it to the GDS we need without full history.
    git clone --depth 1 --filter=blob:none "$repo" "$src"
  else
    echo "already cloned: $src"
  fi

  # Per-cell GDS live at cells/<cell>/sky130_fd_sc_hd__<cell>.gds
  local count
  count=$(find "$src/cells" -name '*.gds' 2>/dev/null | wc -l | tr -d ' ')
  echo "found $count per-cell GDS files"
  [ "$count" -gt 0 ] || { echo "no sky130 GDS found; skipping merge"; return 1; }

  # Merge them all into one layout so a single file carries every cell def.
  local merged="$OUT/sky130_fd_sc_hd_merged.gds"
  if [ ! -f "$merged" ]; then
    if command -v klayout >/dev/null 2>&1; then
      echo "merging into $merged (klayout)..."
      SKY_SRC="$src" SKY_OUT="$merged" klayout -b -r "$ROOT/scripts/merge_sky130.py"
    else
      echo "klayout not found; leaving per-cell files un-merged at $src/cells"
    fi
  else
    echo "merged file present: $merged"
  fi
}

# ---------------------------------------------------------------------------
# 2. efabless Caravel full SoC GDS (compressed in-repo).
# ---------------------------------------------------------------------------
fetch_caravel() {
  log "efabless Caravel full SoC"
  local repo="https://github.com/efabless/caravel"
  local src="$OUT/caravel"
  if [ ! -d "$src" ]; then
    # Only the gds/ tree; it is the bulk of the repo.
    git clone --depth 1 --filter=blob:none --sparse "$repo" "$src"
    ( cd "$src" && git sparse-checkout set gds )
  else
    echo "already cloned: $src"
  fi

  # Files ship gzip-compressed; the repo's `make uncompress` just gunzips them.
  local gz
  gz=$(find "$src/gds" -name '*.gds.gz' 2>/dev/null | head -20)
  if [ -n "$gz" ]; then
    echo "uncompressing caravel gds..."
    find "$src/gds" -name '*.gds.gz' -print0 | while IFS= read -r -d '' f; do
      [ -f "${f%.gz}" ] || gunzip -k "$f"
    done
  fi
  echo "caravel GDS files:"
  find "$src/gds" -name '*.gds' -exec ls -lh {} \; 2>/dev/null | awk '{print $5, $9}' | sort -rh | head -10
}

fetch_sky130 || echo "sky130 fetch incomplete"
fetch_caravel || echo "caravel fetch incomplete"

log "done — testdata/ contents"
find "$OUT" -maxdepth 2 -name '*.gds' -exec ls -lh {} \; 2>/dev/null | awk '{print $5"\t"$9}' | sort -rh | head -20
