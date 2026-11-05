FROM rust:1-slim-trixie AS build
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates curl && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
# Must match the dioxus version in Cargo.lock.
RUN curl -fsSL "https://github.com/DioxusLabs/dioxus/releases/download/v0.7.10/dx-$(uname -m)-unknown-linux-gnu.tar.gz" | tar -xz -C /usr/local/bin dx
WORKDIR /src
COPY . .
RUN dx bundle --web --release

FROM debian:trixie-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=build /src/target/dx/warframe-market-tracker/release/web /app
ENV IP=0.0.0.0 PORT=8080 WFMT_DATA_DIR=/data
EXPOSE 8080
ENTRYPOINT ["/app/server"]
