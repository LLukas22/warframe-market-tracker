# wfmt-ocr

Extracts weapon names, riven names, stats, mastery rank, and reroll counts from screenshots using [pure-onnx-ocr](https://github.com/siska-tech/pure-onnx-ocr) with PaddleOCR PP-OCRv6 tiny. It runs lazily in a browser Web Worker through WebAssembly, keeping OCR off the UI thread.

Install Rust, the `wasm32-unknown-unknown` target, and `wasm-bindgen-cli` 0.2.129, then run `ocr/scripts/fetch-models.sh && ocr/scripts/build-web.sh` from the repository root. Run `python3 -m http.server -d ocr 8000` and open `http://localhost:8000/web/` to scan a screenshot.
