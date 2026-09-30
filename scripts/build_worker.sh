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

# The wasm-bindgen CLI must match the wasm-bindgen crate in Cargo.lock EXACTLY
# (its ABI changes between patch versions). Resolve the locked version, then use,
# in order: $WASM_BINDGEN if set; Trunk's cached CLI for that version; a PATH
# wasm-bindgen of that version; else install that exact version into
# target/tools (local to the repo — never touches ~/.cargo/bin).
VER="$(awk '/^name = "wasm-bindgen"$/ { getline; gsub(/version = |"/, ""); print; exit }' Cargo.lock)"
if [ -z "${VER}" ]; then
  echo "could not read the wasm-bindgen version from Cargo.lock" >&2
  exit 1
fi
TRUNK_CACHE="${TRUNK_CACHE:-$HOME/Library/Caches/dev.trunkrs.trunk}"
if [ -z "${WASM_BINDGEN:-}" ]; then
  if [ -x "$TRUNK_CACHE/wasm-bindgen-$VER/wasm-bindgen" ]; then
    WASM_BINDGEN="$TRUNK_CACHE/wasm-bindgen-$VER/wasm-bindgen"
  elif command -v wasm-bindgen >/dev/null && wasm-bindgen --version | grep -q " $VER"; then
    WASM_BINDGEN="$(command -v wasm-bindgen)"
  else
    if [ ! -x "target/tools/bin/wasm-bindgen" ] || ! target/tools/bin/wasm-bindgen --version | grep -q " $VER"; then
      echo "installing wasm-bindgen-cli $VER into target/tools…"
      cargo install -q wasm-bindgen-cli --version "$VER" --locked --root target/tools
    fi
    WASM_BINDGEN="target/tools/bin/wasm-bindgen"
  fi
fi
echo "wasm-bindgen $VER: $WASM_BINDGEN"

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
