#!/usr/bin/env bash
# Builds the WebAssembly core into the JS SDK (sdk/js/wasm), where the
# Cloudflare entry imports it. Needs the wasm32-unknown-unknown target and
# wasm-bindgen-cli at the same version as the `wasm-bindgen` crate.
#
#   crates/unsworn-wasm/build.sh [--check-size]
set -euo pipefail

root="$(cd "$(dirname "$0")/../.." && pwd)"
out="$root/sdk/js/wasm"
# Cloudflare's free plan allows 3 MB compressed per Worker, handlers
# included. Keep the core well under it.
max_gzip=$((1600 * 1024))

cd "$root"
cargo build -p unsworn-wasm --target wasm32-unknown-unknown --profile wasm
rm -rf "$out"
wasm-bindgen --target web --out-dir "$out" --out-name unsworn \
  --remove-name-section --remove-producers-section \
  target/wasm32-unknown-unknown/wasm/unsworn_wasm.wasm

raw=$(wc -c <"$out/unsworn_bg.wasm")
gz=$(gzip -9 -c "$out/unsworn_bg.wasm" | wc -c)
echo "unsworn_bg.wasm: $((raw / 1024)) KiB, $((gz / 1024)) KiB gzipped"
if [[ "${1:-}" == "--check-size" && $gz -gt $max_gzip ]]; then
  echo "error: the wasm core is over $((max_gzip / 1024)) KiB gzipped" >&2
  exit 1
fi
