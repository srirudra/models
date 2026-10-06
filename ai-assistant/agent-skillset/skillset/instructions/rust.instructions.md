---
name: 'Rust Engineering'
description: 'Use when editing Rust, Cargo.toml, rust-toolchain.toml, rustfmt.toml, clippy.toml, or async/streaming proxy code in Rust. Covers tokio, axum/hyper, reqwest, serde, tracing, testing, and 2026 ecosystem pitfalls.'
applyTo: '**/*.rs, **/Cargo.toml, **/rust-toolchain.toml, **/rustfmt.toml, **/clippy.toml'
---

# Rust Engineering Standards

- Match the existing project's edition (2024 for new code), MSRV, dependency versions, and module layout before introducing new patterns or crates.
- Prefer the established stack for this workspace: tokio (rt-multi-thread) + axum 0.8 over hyper 1.x + tower-http for servers; reqwest 0.13 (rustls default) for clients; serde/serde_json for (de)serialization; tracing + prometheus for observability; thiserror for domain errors, anyhow at the binary boundary.
- Never buffer streaming bodies: relay `Bytes` end-to-end via `Body::wrap_stream` / `Body::from_stream`; `.collect()`/`.text()` on a proxied body is a defect. Force `accept-encoding: identity` upstream for byte-exact pass-through (SSE included).
- Async hygiene: no blocking calls (`std::fs`, `thread::sleep`, sync DNS) in async contexts — use `tokio::fs`/`tokio::time`/`spawn_blocking`; never hold a `std::sync::MutexGuard` across `.await`; cancel background loops with `CancellationToken` (dropping a `JoinHandle` does not cancel the task).
- Single-flight pattern: guard `Option<watch::Receiver<Result>>` with a `std::sync::Mutex`; the first caller spawns the task and sends the result into the watch; all callers await the receiver. The task must settle the result exactly once on every path (success, error, timeout, cancellation).
- Clocks: `std::time::Instant` (monotonic) for durations and budgets; `SystemTime` only for timestamps; inject a clock/`now_fn` seam for tests; use `tokio::time::pause()` for timer tests.
- Config: manual env parsing into a typed struct with explicit rules (trim, bool spellings, empty=unset); serde with `deny_unknown_fields` for file-based config; YAML via yaml-rust2 (serde_yaml is deprecated); invalid config is a boot failure with a message naming the offending key.
- Security: constant-time key comparison (hash both sides to fixed length, then `subtle::ConstantTimeEq`); never log secrets, tokens, or pod env values; rustls with `tls_certs_only`/custom `RootCertStore` for corporate CAs; strip client credentials before forwarding.
- Errors: typed `thiserror` enums per domain (config, lifecycle, API, warmup) with `#[from]` sources; map to HTTP status via `IntoResponse`; `anyhow` only in `main`/tests.
- Graceful shutdown: `with_graceful_shutdown` must be bounded by a grace-period timeout (long-lived streams never end); cancel the token tree first, then drain, then force-close.
- Testing: `#[tokio::test]` + httpmock (not wiremock — unmaintained) for HTTP seams; byte-exact SSE equality tests; proptest for invariants (single-flight, header filtering, body caps); `tokio::time::pause` for keepalive/prewarm/idle logic.
- Lints: keep `cargo clippy` (pedantic, with justified allows) and `cargo fmt` clean; `#[deny(unsafe_code)]` unless a documented exception; run `cargo audit` after dependency changes.
- 2026 ecosystem facts (verified 2026-09-14): reqwest 0.13 defaults to rustls/aws-lc (MSRV 1.85); axum 0.8 uses `/{param}` path syntax and requires `Sync` handlers; thiserror 2.x is current; avoid serde_yaml, figment, and wiremock (deprecated/stagnant).
- Docker: multi-stage with cargo-chef + BuildKit cache mounts; musl static binary + distroless/static (debian:slim fallback); unprivileged user; `/_health` healthcheck.
- Validate after changes: `cargo check` → `cargo clippy --all-targets` → focused `cargo test <filter>` → full suite when blast radius requires (see the rust-build-test skill).
