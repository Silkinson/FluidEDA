#!/usr/bin/env bash
# Build the FluidEDA parse worker into assets/worker/ (served by Trunk's copy-dir).
#
# Produces (all gitignored — regenerated here):
#   assets/worker/worker.js       wasm-bindgen no-modules glue (defines wasm_bindgen)
#   assets/worker/worker_bg.wasm  the core-only parse wasm
# The hand-written driver assets/worker/parse_worker.js (committed) importScripts()
# worker.js and runs the parse protocol.
#
# Run standalone, or automatically as a Trunk pre_build hook (see Trunk.toml).
set -euo pipefail
cd "$(dirname "$0")/.."

# Use Trunk's cached wasm-bindgen so the version matches the workspace exactly.
WASM_BINDGEN="${WASM_BINDGEN:-/Users/keyman/Library/Caches/dev.trunkrs.trunk/wasm-bindgen-0.2.126/wasm-bindgen}"
if [ ! -x "$WASM_BINDGEN" ]; then
  # Fall back to a PATH wasm-bindgen (e.g. cargo-installed) if the cache moved.
  WASM_BINDGEN="$(command -v wasm-bindgen)"
fi

echo "building fluideda-worker (wasm32)…"
cargo build --release -p fluideda-worker --target wasm32-unknown-unknown

echo "running wasm-bindgen (no-modules)…"
mkdir -p assets/worker
"$WASM_BINDGEN" \
  --target no-modules \
  --no-typescript \
  --out-dir assets/worker \
  --out-name worker \
  target/wasm32-unknown-unknown/release/fluideda_worker.wasm

echo "worker built -> assets/worker/worker.js + worker_bg.wasm"
