#!/usr/bin/env bash
# Downloads the riven screenshots tests/samples/expected.json describes, pinned to the commit it
# names. They come from github.com/Endermy/wm-riven-ocr, which has no license, so they're
# fetched for local testing instead of being committed.
set -euo pipefail
dir="$(cd "$(dirname "$0")/.." && pwd)/tests/samples"
commit=85632610f6835c1dc37693583a5a1591f978c730
base="https://raw.githubusercontent.com/Endermy/wm-riven-ocr/$commit/test/samples"
for file in card1.png card2.png mods/r1.png mods/r2.png mods/r3.png mods/r4.png mods/r5.png mods/r6.png mods/r7.png mods/m1.png; do
  out="$dir/images/$file"
  [ -s "$out" ] && continue
  mkdir -p "$(dirname "$out")"
  echo "$file"
  curl -fL --retry 3 -o "$out" "$base/$file"
done
