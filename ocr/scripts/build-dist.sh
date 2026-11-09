#!/usr/bin/env bash
# Builds everything the site serves under /ocr/ into ocr/dist: the loader and worker, the wasm
# module, the PP-OCRv6 tiny models and the attribute/weapon name lists, plus .gz copies the
# server sends to browsers that accept gzip.
# Needs the wasm32-unknown-unknown target, wasm-bindgen-cli 0.2.129 and curl.
set -euo pipefail
here="$(cd "$(dirname "$0")/.." && pwd)"
dist="$here/dist"
"$here/scripts/build-web.sh"
"$here/scripts/fetch-models.sh" tiny
rm -rf "$dist"
mkdir -p "$dist/pkg" "$dist/models"
cp "$here/web/scanner.js" "$here/web/worker.js" "$dist/"
cp "$here/web/pkg/wfmt_ocr.js" "$here/web/pkg/wfmt_ocr_bg.wasm" "$dist/pkg/"
cp -r "$here/models/tiny_det" "$here/models/tiny_rec" "$dist/models/"
# The server adds these names to the scraper's current weapon list.
cp "$here/tests/fixtures/riven_weapons.json" "$here/tests/fixtures/riven_attributes.json" "$dist/"
find "$dist" -type f ! -name '*.gz' -exec gzip -k -9 {} +
du -sh "$dist"
