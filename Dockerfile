# wreq builds BoringSSL from source, so the builder needs cmake, a C/C++
# toolchain and libclang (for bindgen). The runtime image needs none of that.
FROM rust:1-bookworm AS builder

WORKDIR /build

RUN apt-get update \
    && apt-get install -y --no-install-recommends cmake clang libclang-dev \
    && rm -rf /var/lib/apt/lists/*

# cmake/ is required: .cargo/config.toml points at the shim inside it.
COPY Cargo.toml Cargo.lock ./
COPY .cargo ./.cargo
COPY cmake ./cmake
COPY src ./src

RUN cargo build --release --locked && strip target/release/hls-proxy

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/target/release/hls-proxy /usr/local/bin/hls-proxy

ENV BIND=0.0.0.0 \
    PORT=8080 \
    RUST_LOG=hls_proxy=info

EXPOSE 8080
USER nobody

ENTRYPOINT ["/usr/local/bin/hls-proxy"]
