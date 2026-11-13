# Builds the app and the screenshot scanner from source by default. CI compiles them outside
# Docker, where its Cargo cache persists, and sets ARTIFACTS=prebuilt (see the `prebuilt` stage).
ARG ARTIFACTS=source

FROM rust:1-slim-trixie AS base
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
WORKDIR /src

FROM base AS build
# Must match the dioxus version in Cargo.lock (and DX_VERSION in .github/workflows/docker.yml).
RUN curl -fsSL "https://github.com/DioxusLabs/dioxus/releases/download/v0.7.10/dx-$(uname -m)-unknown-linux-gnu.tar.gz" | tar -xz -C /usr/local/bin dx
RUN apt-get update && apt-get install -y --no-install-recommends brotli && rm -rf /var/lib/apt/lists/*
COPY . .
RUN scripts/bundle-web.sh

# The screenshot scanner, served from /app/ocr. Builds in parallel with the app.
FROM base AS ocr
# Must match the wasm-bindgen version in Cargo.lock (and WASM_BINDGEN_VERSION in the workflow).
RUN curl -fsSL "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.129/wasm-bindgen-0.2.129-$(uname -m)-unknown-linux-musl.tar.gz" \
    | tar -xz --strip-components=1 -C /usr/local/bin "wasm-bindgen-0.2.129-$(uname -m)-unknown-linux-musl/wasm-bindgen"
COPY . .
RUN ocr/scripts/build-dist.sh

FROM scratch AS source
COPY --from=build /src/target/dx/warframe-market-tracker/release/web /app
COPY --from=ocr /src/ocr/dist /app/ocr

# Takes a build context holding app/ laid out like /app: the `dx bundle` output, plus ocr/dist in
# app/ocr. The build and ocr stages are skipped then.
FROM scratch AS prebuilt
COPY app /app

FROM ${ARTIFACTS} AS artifacts

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=artifacts /app /app
# The Hugging Face upload keeps a chunk cache here; the container user may have no home folder.
ENV IP=0.0.0.0 PORT=8080 WFMT_DATA_DIR=/data HF_HOME=/tmp/huggingface
EXPOSE 8080
ENTRYPOINT ["/app/server"]
