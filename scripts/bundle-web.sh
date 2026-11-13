#!/bin/sh
# Builds the web app into target/dx/warframe-market-tracker/release/web, for the Dockerfile and CI.
# The browser build uses the size-optimized wasm-release profile (see Cargo.toml). Static assets
# are then precompressed with brotli: the server sends a .br file next to an asset when the
# browser accepts it, which beats on-the-fly gzip by a good margin for the WASM.
set -eu
dx bundle --web --release @client --profile wasm-release
find target/dx/warframe-market-tracker/release/web/public \
    -type f \( -name '*.wasm' -o -name '*.js' -o -name '*.css' -o -name '*.svg' \) \
    -exec brotli --best --keep --force {} +
