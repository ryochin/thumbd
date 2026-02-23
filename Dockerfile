# ─────────────────────────────────────────────
# Stage 1: builder
# ─────────────────────────────────────────────
FROM rust:1.93-slim-trixie AS builder
ARG TARGETARCH

# Install protoc (required by tonic-build at compile time) and download grpc-health-probe
RUN apt-get update && apt-get install -y --no-install-recommends \
        protobuf-compiler \
        cmake \
        make \
        nasm \
        gcc \
        g++ \
        curl \
    && GRPC_PROBE_VERSION=v0.4.45 \
    && BINARY="grpc_health_probe-linux-${TARGETARCH}" \
    && curl -fsSL \
       "https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_PROBE_VERSION}/${BINARY}" \
       -o /usr/local/bin/grpc-health-probe \
    && curl -fsSL \
       "https://github.com/grpc-ecosystem/grpc-health-probe/releases/download/${GRPC_PROBE_VERSION}/checksums.txt" \
       -o /tmp/checksums.txt \
    && grep "${BINARY}" /tmp/checksums.txt \
       | awk '{print $1 "  /usr/local/bin/grpc-health-probe"}' \
       | sha256sum -c - \
    && chmod +x /usr/local/bin/grpc-health-probe \
    && rm -f /tmp/checksums.txt \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /build

# Cache dependencies separately from source
# Copy manifests and build scripts first
COPY Cargo.toml Cargo.lock ./
COPY server/Cargo.toml server/Cargo.toml
COPY client/Cargo.toml client/Cargo.toml
COPY server/build.rs  server/build.rs
COPY client/build.rs  client/build.rs
COPY proto/            proto/

# Create stub src files so `cargo build` can resolve the workspace graph
RUN mkdir -p server/src client/src \
 && echo 'fn main() {}' > server/src/main.rs \
 && echo 'fn main() {}' > client/src/main.rs \
 && echo '' > server/src/service.rs \
 && echo '' > server/src/convert.rs

# Pre-build deps (this layer is cached as long as Cargo.toml/lock don't change)
RUN cargo build --release --bin thumbd --bin thumbd-client \
 && rm -rf server/src client/src

# Build the real binaries
COPY server/src/ server/src/
COPY client/src/ client/src/
# Touch main.rs to force rebuild of the crates (not just deps)
RUN touch server/src/main.rs client/src/main.rs \
 && cargo build --release --bin thumbd --bin thumbd-client

# Create socket directory owned by the nonroot user (uid=65532) used in the runtime stage
RUN mkdir -p /run/thumbd && chown 65532:65532 /run/thumbd

# ─────────────────────────────────────────────
# Stage 2: runtime
# ─────────────────────────────────────────────
# distroless/cc contains only glibc + libstdc++.
# libwebp is statically linked (bundled by libwebp-sys), so no extra packages needed.
FROM gcr.io/distroless/cc-debian12

COPY --from=builder /build/target/release/thumbd /usr/local/bin/thumbd
COPY --from=builder /usr/local/bin/grpc-health-probe    /usr/local/bin/grpc-health-probe
COPY --from=builder /run/thumbd                          /run/thumbd

# Run as non-root (distroless nonroot user, uid=65532)
USER nonroot:nonroot

ENTRYPOINT ["/usr/local/bin/thumbd"]
CMD ["--addr", "unix:/run/thumbd/thumbd.sock"]
