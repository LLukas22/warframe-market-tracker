#!/usr/bin/env bash
# Downloads PaddleOCR PP-OCRv6 models (Apache-2.0) into ocr/models/<tier>_{det,rec}/.
# Usage: ocr/scripts/fetch-models.sh [tiny|small|medium]...   (default: tiny, what the app uses)
set -euo pipefail
dir="$(cd "$(dirname "$0")/.." && pwd)/models"
tiers=("$@")
[ ${#tiers[@]} -eq 0 ] && tiers=(tiny)
for tier in "${tiers[@]}"; do
  for part in det rec; do
    mkdir -p "$dir/${tier}_$part"
    for file in inference.onnx inference.yml; do
      out="$dir/${tier}_$part/$file"
      [ -s "$out" ] && continue
      echo "PP-OCRv6_${tier}_$part/$file"
      curl -fL --retry 3 -o "$out.part" \
        "https://huggingface.co/PaddlePaddle/PP-OCRv6_${tier}_${part}_onnx/resolve/main/$file"
      mv "$out.part" "$out"
    done
  done
done
