# syntax=docker/dockerfile:1

# ---- builder: static musl binary with dependency-layer caching ----
FROM rust:1.98-musl AS builder
RUN cargo install cargo-chef --locked
WORKDIR /app

# Dependency layer: rebuilds only when the manifests change.
COPY Cargo.toml Cargo.lock ./
COPY crates/runpod-proxy/Cargo.toml crates/runpod-proxy/
COPY demo/mock_upstream/Cargo.toml demo/mock_upstream/
RUN cargo-chef prepare > /tmp/plan.json
COPY . .
RUN cargo-chef cook

RUN cargo build --release --bin runpod-proxy

# ---- runtime: distroless static, unprivileged (uid 65532) ----
FROM gcr.io/distroless/static-debian12:nonroot

# Corporate CA (Zscaler) baked in, same as the Python image. rustls does not
# read a system store, so the app loads this file explicitly (config, WI-05).
COPY certs/zscaler-root-ca.crt /etc/runpod-proxy/zscaler-root-ca.crt

COPY --from=builder /app/target/release/runpod-proxy /usr/local/bin/runpod-proxy

# No HEALTHCHECK instruction: distroless has no shell. Rely on the
# orchestrator's probe of /_health (same 30s/5s/5s/3 shape as the Python image).
EXPOSE 8080
ENTRYPOINT ["/usr/local/bin/runpod-proxy"]
