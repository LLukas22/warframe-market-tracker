FROM rust:1-slim-trixie AS base
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
WORKDIR /src

FROM base AS build
# Must match the dioxus version in Cargo.lock.
RUN curl -fsSL "https://github.com/DioxusLabs/dioxus/releases/download/v0.7.10/dx-$(uname -m)-unknown-linux-gnu.tar.gz" | tar -xz -C /usr/local/bin dx
COPY . .
RUN dx bundle --web --release

# The screenshot scanner, served from /app/ocr. Builds in parallel with the app.
FROM base AS ocr
# Must match the wasm-bindgen version in Cargo.lock.
RUN curl -fsSL "https://github.com/wasm-bindgen/wasm-bindgen/releases/download/0.2.129/wasm-bindgen-0.2.129-$(uname -m)-unknown-linux-musl.tar.gz" \
    | tar -xz --strip-components=1 -C /usr/local/bin "wasm-bindgen-0.2.129-$(uname -m)-unknown-linux-musl/wasm-bindgen"
COPY . .
RUN ocr/scripts/build-dist.sh

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/dx/warframe-market-tracker/release/web /app
COPY --from=ocr /src/ocr/dist /app/ocr
ENV IP=0.0.0.0 PORT=8080 WFMT_DATA_DIR=/data
EXPOSE 8080
ENTRYPOINT ["/app/server"]
