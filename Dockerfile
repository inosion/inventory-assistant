# inventory-assistant: Rust (axum) server + Rust/Leptos wasm UI in one small image.
FROM rust:1.100-slim-bookworm AS build
ARG TARGETARCH
RUN apt-get update && apt-get install -y --no-install-recommends curl ca-certificates \
 && rm -rf /var/lib/apt/lists/*
RUN rustup target add wasm32-unknown-unknown
ARG TRUNK_VERSION=0.21.14
RUN case "${TARGETARCH:-amd64}" in arm64) a=aarch64 ;; *) a=x86_64 ;; esac \
 && curl -sSfL "https://github.com/trunk-rs/trunk/releases/download/v${TRUNK_VERSION}/trunk-${a}-unknown-linux-gnu.tar.gz" \
    | tar -xz -C /usr/local/bin trunk
# Must match the wasm-bindgen version in Cargo.lock. Installed from crates.io so the build
# does not depend on trunk downloading it at build time.
ARG WASM_BINDGEN_VERSION=0.2.130
RUN cargo install wasm-bindgen-cli --version "${WASM_BINDGEN_VERSION}" --locked
WORKDIR /src
COPY . .
# Path the UI is served under (must match the reverse proxy route).
ARG PUBLIC_URL=/capture/
RUN cd crates/web && TRUNK_SKIP_VERSION_CHECK=true trunk build --release --public-url "${PUBLIC_URL}"
RUN cargo build --release -p inventory-assistant

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends ca-certificates tzdata \
 && rm -rf /var/lib/apt/lists/* \
 && useradd --system --uid 10001 --home-dir /data assistant && mkdir -p /data && chown assistant /data
COPY --from=build /src/target/release/inventory-assistant /usr/local/bin/inventory-assistant
COPY --from=build /src/crates/web/dist /app/dist
ENV DIST_DIR=/app/dist DATA_DIR=/data LISTEN=0.0.0.0:3000 RUST_LOG=info
USER assistant
VOLUME /data
EXPOSE 3000
HEALTHCHECK --interval=30s --timeout=5s CMD ["bash", "-c", "exec 3<>/dev/tcp/127.0.0.1/3000"]
CMD ["inventory-assistant"]
