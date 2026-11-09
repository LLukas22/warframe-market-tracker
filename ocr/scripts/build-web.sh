#!/usr/bin/env bash
# Builds the browser module into ocr/web/pkg.
# Needs the wasm32-unknown-unknown target and wasm-bindgen-cli 0.2.129 (the crate's pin).
# Uses its own target directory so the SIMD flag doesn't invalidate the app's build cache.
set -euo pipefail
root="$(cd "$(dirname "$0")/../.." && pwd)"
target="$root/target/ocr-wasm"
# SIMD roughly halves inference time. Chrome 91+, Firefox 89+ and Safari 16.4+ support it.
RUSTFLAGS="-C target-feature=+simd128" cargo build \
  --manifest-path "$root/Cargo.toml" -p wfmt-ocr --lib --features wasm \
  --target wasm32-unknown-unknown --profile ocr-wasm --target-dir "$target"
wasm-bindgen --target web --no-typescript --out-dir "$root/ocr/web/pkg" \
  "$target/wasm32-unknown-unknown/ocr-wasm/wfmt_ocr.wasm"
ls -l "$root/ocr/web/pkg"
