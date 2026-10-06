# RUST-001 Plan: RunPod Warm Proxy in Rust (parallel, enhanced)

**Status:** Complete (parity + cost-safety verified; closed per D-09 on 2026-09-19) — live view in `status.md`
**Owner:** Principal Engineer
**Date:** 2026-09-14

## 1. Problem and Outcome

The Python `runpod-proxy` (in `C:\Users\vdkapoor\projects\ai-assistant\runpod-proxy`) masks
RunPod cold starts for agentic LLM workflows: it warms the backend on first request
(single-flight), keeps it alive while the user works, and stops billing when idle. It also
manages pod lifecycle (pinned + discovery + opt-in creation), per-request model routing,
a declarative model catalogue, pre-warming, and full observability.

**Outcome:** a Rust implementation in `C:\Users\vdkapoor\projects\ai-assist-rust` that:

1. Is a **drop-in replacement** — same env-var configuration surface, same HTTP surface
   (`/_health`, `/_status`, `/metrics`, `/_warm`, `/_reload`, transparent proxy), same
   behavior per the normative spec (`docs/specification.md` in the Python repo, v1.0),
   same Docker/compose deployment shape. Clients (Copilot CLI, OpenAI SDKs) need zero changes.
2. Is **enhanced** where Rust gives real, low-risk wins (footprint, startup, streaming
   efficiency, static analysis, binary distribution) — see §9. Enhancements never change
   the wire contract or the cost-safety invariants.

**Why Rust (rationale, recorded for the record):**
- Single static binary: no interpreter, no dependency layer → smaller image, faster boot,
  no supply-chain of Python packages at runtime.
- Memory: target < 20 MB RSS vs ~50 MB for the Python container (spec N2).
- True concurrency for the background loops (keepalive, prewarm, GPU poller) + streaming
  relay without GIL or per-chunk Python overhead.
- Compile-time guarantees (ownership, exhaustive matching) for the state machine and the
  cost-safety invariants (§11 of the spec) — the highest-risk logic in the system.

## 2. Verified Current-System Observations (evidence from the Python repo)

- Stack: FastAPI + httpx + uvicorn[standard] + PyYAML; pytest (264 tests, `asyncio_mode=auto`).
- Modules (spec §13 map): `config.py` (12 KB), `state.py`, `warmup.py`, `keepalive.py`,
  `router.py`, `prewarm.py`, `lifecycle.py` (42 KB — serverless/pinned/discovery),
  `runpod_api.py` (21 KB), `models_config.py` (11 KB), `main.py` (25 KB — HTTP surface +
  pipeline), `target.py`, `gpu_availability.py`, `health.py`.
- The spec is explicitly normative for ports: "Status: Normative reference for recreating
  this system in any language/runtime" and §14 is a porting checklist. The test suite is
  described as "the executable spec" with two seams (HTTP client, RunPod API).
- Deployment: `python:3.12-slim` image, unprivileged user, `/_health` healthcheck,
  compose with read-only catalogue mount and `stop_grace_period: 30s`.
- Config: ~40 env vars (spec §5), model catalogue as JSON/YAML file or inline JSON,
  hot-reload via `/_reload` with full re-validation.
- Security-relevant behaviors: constant-time proxy-key compare; RunPod management key
  never sent to pod endpoints; pod `env` values never logged; `x-proxy-key`/client
  `Authorization` stripped before forwarding; upstream key injection per mode/pod
  (incl. `{pod_id}` template keys).
- Cost-safety invariants (spec §11) are the non-negotiable core: at most one created pod
  per model per process; reclaim on every failure path *including warmup-timeout
  cancellation* (shielded cleanup); failed stops retried every keepalive tick; found-
  RUNNING pods never stopped; shutdown stops the backend.
- RunPod REST v1 is deprecated (retirement planned 2026-11-15); creation already uses the
  v2 API. `RUNPOD_REST_URL` is configurable for the migration.
- Local demo exists (`demo/mock_upstream.py` + `run-demo.ps1`) — a Rust demo equivalent
  is required for no-credential verification.

## 3. Assumptions

- A1: The Python spec v1.0 is the complete behavioral contract; where README and spec
  disagree, the spec wins (it is marked normative).
- A2: The Rust proxy runs on the same host/container topology (Docker on Windows or
  Linux; x86-64). No new deployment targets are in scope.
- A3: The corporate Zscaler root CA (baked into the Python image) is an environment
  concern; the Rust image will support the same `SSL_CERT_FILE`/CA-bundle mechanism
  (rustls with webpki-roots + custom root, or native certs) — verified in research.
- A4: "Parallel" means the Rust proxy can run alongside the Python one (different port
  or host) during validation; cutover is a client base-URL change.
- A5: The Rust toolchain is not yet installed on the dev machine (verified 2026-09-14:
  no `rustup`/`cargo`/`rustc` on PATH) — WI-03 covers bootstrap.

## 4. Non-Goals (parity with spec §1.4, plus port-specific)

- No rate limiting, TLS termination, caching, response transformation, multi-tenancy.
- No concurrent multi-model serving (one active model; switching is serialized).
- No changes to the Python repo (read-only reference).
- No new RunPod API surface beyond what the spec uses (v1 read/start/stop/delete + v2
  create + v2 GPU availability).
- No Kubernetes/Helm; Docker + compose only (matches current deployment).

## 5. Architecture (Rust)

Single binary, single crate (the system is one process; a workspace is overkill at this
size, but the scaffold uses a workspace so test/demo crates can be added without churn).

```
ai-assist-rust/
├── Cargo.toml                  # workspace
├── rust-toolchain.toml         # pin stable channel
├── crates/
│   └── runpod-proxy/
│       ├── src/
│       │   ├── main.rs         # bootstrap: config → app → run; graceful shutdown
│       │   ├── config.rs       # §5 env parsing, derived values, boot validation
│       │   ├── catalogue.rs    # §7.2 JSON/YAML catalogue, strict validation, reload
│       │   ├── state.rs        # §4 state machine + counters (Arc<Mutex<...>>)
│       │   ├── warmup.rs       # §6.1 single-flight warmup
│       │   ├── keepalive.rs    # §6.2 probe loop + idle give-up
│       │   ├── router.rs       # §6.3 model resolution, leases, serialized switches
│       │   ├── prewarm.rs      # §6.4 scheduled warmups (injectable clock)
│       │   ├── lifecycle/
│       │   │   ├── mod.rs      # trait Lifecycle { start, stop, retry_pending_stops }
│       │   │   ├── serverless.rs
│       │   │   ├── pinned.rs   # §6.7 incl. RUNPOD_ON_MIGRATE=replace
│       │   │   └── discovery.rs# §6.8 discovery, creation matrix, cost safety
│       │   ├── runpod_api.rs   # §9 REST client (v1 + v2), typed errors
│       │   ├── health.rs       # §6.8a health classification (any/model/completion)
│       │   ├── proxy/
│       │   │   ├── mod.rs      # §8.4 request pipeline
│       │   │   ├── headers.rs  # §8.3 hop-by-hop filter, accept-encoding: identity
│       │   │   └── body.rs     # streaming body read with MAX_BODY_BYTES cap
│       │   ├── auth.rs         # §8.2 constant-time key gate
│       │   ├── request_id.rs   # X-Request-Id echo/generate + tracing field
│       │   ├── metrics.rs      # §8.1 Prometheus registry
│       │   ├── status.rs       # /_status, /_warm, /_reload handlers
│       │   └── gpu_poller.rs   # §10 GPU availability poller
│       └── tests/              # integration tests (wiremock upstream + RunPod API)
│           ├── warmup.rs  keepalive.rs  forwarding.rs  auth.rs  routing.rs
│           ├── pod_mode.rs  discovery.rs  catalogue.rs  metrics.rs  ...
├── demo/
│   └── mock_upstream/          # Rust (axum) mock of the serverless endpoint
├── Dockerfile                  # multi-stage (see §11)
├── docker-compose.yml          # same shape as Python compose
├── models.example.yaml / .json # copied from Python repo (same schema)
├── .env.example                # same variables, same defaults
└── docs/
    ├── specification.md        # copy of the normative spec (version-pinned)
    └── copilot-cli-guide.md    # client pointing guide (same as Python)
```

Key design decisions (to be confirmed against research in §6):

- **Async runtime:** tokio (multi-thread) — background loops are spawned tasks;
  cancellation via `CancellationToken` (tokio-util) for warmup budgets and shutdown.
- **Server:** axum over hyper 1.x — handler per route; the catch-all proxy handler
  streams the upstream response body straight into the client response
  (`http-body-util`), no buffering.
- **Client:** one shared reqwest/hyper client (connection pooling) for probes,
  forwarding, and RunPod REST; `accept-encoding: identity` forced; rustls TLS with
  configurable root certs (corporate CA support).
- **Single-flight warmup:** `Arc<Mutex<Option<JoinHandle<Result<()>>>>>` — first caller
  spawns the warmup task, all others await the same handle; the task settles the
  shared result exactly once on every path (spec §6.1 "must settle the future exactly
  once").
- **Leases:** atomic in-flight counter + `tokio::sync::Notify` (idle event) — the
  switch waits on the notify with a `MODEL_SWITCH_DRAIN_S` timeout.
- **Multi-session concurrency (N9, D-07):** the proxy must bear many concurrent
  client sessions (multiple Copilot CLI sessions streaming at once) and degrade
  gracefully. Design: tokio multi-thread runtime (already scaffolded); one shared
  pooled client (N2); a `tokio::sync::Semaphore` (`PROXY_MAX_CONCURRENT_REQUESTS`,
  default 100) acquired at the top of the proxy pipeline — `try_acquire` on
  saturation → `503` + `Retry-After: 1` + JSON error (no unbounded queueing, no
  dropped established connections); `/_health`/`/_status` bypass the bound;
  shutdown flips a draining flag → new requests 503, in-flight streams drain
  within the N6 grace. Load test (≥ 50 concurrent SSE streams) is a WI-13 AC.
- **Clocks:** `std::time::Instant` (monotonic) for budgets; `SystemTime` for
  timestamps/prewarm; injectable `now_fn`/`clock` trait for tests (spec §6.4 test seam).
- **Errors:** `thiserror` enums (`LifecycleError`, `RunpodApiError`, `WarmupTimeout`,
  `ModelConfigError`) + `anyhow` at the `main` boundary.
- **Config:** manual env parsing into a typed `Config` (matches the spec's exact
  parsing rules: trim, bool spellings, empty=unset) — a generic config crate would
  fight the spec's idiosyncratic rules; serde for catalogue JSON/YAML with
  `deny_unknown_fields`.
- **Observability:** `tracing` + `tracing-subscriber` (text/JSON per `LOG_FORMAT`),
  request-id as a tracing field; `prometheus` crate for `/metrics`.
- **Auth:** `subtle` crate for constant-time comparison.
- **TLS/CA (D-05):** reqwest/rustls does not read the OS trust store, so the
  Zscaler CA (baked into the image at `/etc/runpod-proxy/zscaler-root-ca.crt`,
  same as the Python image) must be loaded explicitly: `RootCertStore` =
  webpki-roots + the baked-in file, overridable by an env var for local dev.
  The Python repo's `certs/zscaler-root-ca.crt` (public CA cert) is copied into
  this repo.
- **Dev toolchain (D-06):** this Windows box has no MSVC linker, so local dev
  uses `stable-x86_64-pc-windows-gnu` (rustup default on this machine).
  `rust-toolchain.toml` stays host-agnostic (`channel = "stable"`); the
  production image builds on Linux (musl) regardless.

## 6. Specialist Findings — Rust Ecosystem Research

> **Status: complete** (research agent, verified live 2026-09-14). All versions
> below were checked against crates.io / docs.rs at research time.

### 6.1 Toolchain

- **Rust 1.98.1** stable; **Edition 2024** (RFC 3501, stable since 1.85.0).
- **Project MSRV: 1.85** — driven by reqwest 0.13 (rustls/aws-lc). Pin via `rust-toolchain.toml`.
- Windows dev → Linux container: BuildKit cache mounts or musl cross-compile (see §11).

### 6.2 Verified crate selections

| Concern | Crate | Version | Notes |
|---------|-------|---------|-------|
| Async runtime | tokio | 1.53.1 | features: rt-multi-thread, macros, net, time, signal, sync, io-util |
| Cancellation | tokio-util | 0.7.19 | `CancellationToken` (dropping a `JoinHandle` does NOT cancel) |
| HTTP server | axum | 0.8.9 | `/{param}` path syntax; handlers must be `Sync`; `fallback` + `any` for the proxy route |
| HTTP core | hyper / hyper-util | 1.11.1 / 0.1.20 | via axum |
| Middleware | tower / tower-http | 0.5.3 / 0.7.1 | request-id, trace, timeout |
| HTTP client | reqwest | 0.13.5 | **rustls default** (aws-lc provider); `tls_certs_only(RootCertStore)` for the corporate CA; `query`/`form` are opt-in features |
| (De)serialization | serde / serde_json | 1.0.229 / 1.0.151 | `deny_unknown_fields` on config structs |
| YAML | yaml-rust2 | 0.13.0 | **serde_yaml is officially deprecated**; serde_yaml_ng 0.10.0 is stagnant |
| Observability | tracing / tracing-subscriber | 0.1.44 / 0.3.23 | JSON + EnvFilter, request-id spans |
| Metrics | prometheus | 0.14.0 | IntCounter/Histogram/Gauge, TextEncoder |
| Errors | thiserror / anyhow | 2.0.20 / 1.0.104 | typed enums per domain; anyhow only at the `main` boundary |
| Constant-time auth | subtle / sha2 | 2.6.1 / 0.11.0 | `ConstantTimeEq`; hash both keys to fixed length first (length is not hidden) |
| Testing | httpmock / proptest / tokio-test | 0.8.3 / 1.11.0 / 0.4.5 | **httpmock over wiremock (unmaintained)**; `tokio::time::pause()` for clock tests |
| Docker | cargo-chef | 0.1.78 | dependency-layer caching |

### 6.3 Key patterns (streaming reverse proxy)

- **Single-flight warmup**: `std::sync::Mutex<Option<watch::Receiver<WarmupResult>>>` — first caller spawns the task and sends the result into the watch; all callers clone the receiver and await. The task must settle the result exactly once on every path (success, error, timeout, cancellation).
- **Streaming relay**: axum `fallback` route + `any` method; `body.into_stream()` → `reqwest::Body::wrap_stream` (NEVER `.collect()`); response via `Body::from_stream(resp.bytes_stream())`. Force `accept-encoding: identity`; filter hop-by-hop headers both directions; byte-counting stream adapter for the 413 cap.
- **Graceful shutdown**: `with_graceful_shutdown` raced against a grace-period sleep (SSE streams never end on their own); cancel the token tree first, then drain, then force-close.
- **Clocks**: monotonic `Instant` for durations/budgets; `SystemTime` only for timestamps; inject a `now_fn` seam for tests.

### 6.4 "Obvious choice is wrong" list

1. Don't use **serde_yaml** (deprecated) — use yaml-rust2.
2. Don't use **wiremock** (unmaintained) — use httpmock.
3. Don't **buffer bodies** — stream `Bytes` end-to-end.
4. Don't enable **reqwest decompression** — force `accept-encoding: identity`.
5. Don't assume **native-tls** — reqwest 0.13 defaults to rustls; use `tls_certs_only` for the corporate CA.
6. Don't use **SystemTime for durations** — use `Instant` (monotonic).
7. Don't let **`with_graceful_shutdown` run unbounded** — bound it with a grace-period timeout.
8. Don't use **figment** (stagnant) — manual env parsing or config 0.15.

### 6.5 Gaps / follow-ups

- **RUSTSEC advisory sweep not completed** (GitHub rate limit) — run `cargo audit` in-project (WI-03).
- Edition 2024 full itemized change list: verify against the official edition guide during WI-03 bootstrap.
- axum 0.9 is in development (unreleased) with `serve` behavior changes — **pin strictly to 0.8.9** (decision recorded here; revisit only as a deliberate upgrade work item).

## 7. Work Items (atomic, dependency-ordered)

See `status.md` Progress table for the live view. Summary with acceptance criteria:

- **WI-03 Toolchain bootstrap** — rustup + stable MSVC toolchain + clippy/rustfmt on
  Windows; `rust-toolchain.toml` pins the channel. *AC:* `cargo --version`,
  `cargo clippy --version`, `rustfmt --version` all succeed; a hello-world builds.
- **WI-04 Scaffold** — workspace, crate, lints (`clippy::pedantic` where sane,
  `#![warn(missing_docs)]` optional), `cargo fmt` config, empty server that answers
  `/_health`. *AC:* `cargo build` + `cargo clippy` clean; `/_health` returns
  `{"ok":true}`.
- **WI-05 Config + catalogue** — full §5 env surface with exact parsing rules and boot
  validation; §7.2 catalogue (JSON+YAML, strict, all rejection rules, error messages
  naming model index + key). *AC:* unit tests mirror every rule in spec §5.5 and §7.2
  (the Python `test_models_config.py` / config tests are the scenario source).
- **WI-06 State + warmup + keepalive** — §4 state machine, §6.1 single-flight (N
  concurrent waiters, exactly one warmup chain, timeout wakes all), §6.2 tick logic
  (degrade at 3, heal on 1, idle give-up with stop-retry). *AC:* tests for every
  transition in §4 + the "future settled exactly once" property.
- **WI-07 HTTP surface + pipeline** — §8.1 routes, §8.2 auth gate, §8.3 header
  filtering, §8.4 pipeline in normative order (traffic stamp → headers → bounded body
  read → model extraction ≤2 MiB JSON → resolve → canonicalize → lease → revalidation →
  forward → relay). *AC:* SSE pass-through test (chunk-by-chunk, no buffering, no
  added Content-Length), 413 over cap, 401 without key, 502/503 JSON errors,
  `x-proxy-key` stripping, model-field rewrite; N9 semaphore: saturation returns
  503 + `Retry-After: 1` while established streams keep flowing.
- **WI-08 RunPod API + pinned lifecycle** — §9 client (typed errors, 404→None,
  capacity-400 detection), §6.7 pinned start/stop/replace (migrate policy),
  fail-fast on deleted pod. *AC:* httpmock tests for every §6.7 branch incl.
  `RUNPOD_ON_MIGRATE=replace` volume re-attach.
- **WI-09 Discovery + creation matrix + cost safety** — §6.8 algorithm (created-pod
  first, RUNNING-probe, EXITED-resume, create), §6.8a health modes, §6.8c/d/e matching
  and matrix walk, §11 invariants incl. **shielded reclaim under cancellation**.
  *AC:* tests for each §11 invariant; a cancellation test proving a started-but-
  unhealthy pod is still stopped.
- **WI-10 Model router** — §6.3 resolution (slug equality, canonical spelling),
  lease/switch protocol (drain ≤ budget, serialized), §8.4 steps 4–8 integration.
  *AC:* switch test with in-flight request; drain-expiry test; 400 disallowed model
  with `allowed` list.
- **WI-11 Observability** — §8.1 metrics (exact names/labels/buckets), `/_status`
  shape, `/_reload` (validate-then-swap, rejection rules), §6.4 prewarm (fake clock),
  §10 GPU poller (stale-not-blank). *AC:* metrics text matches the Python proxy's
  output for the same event sequence (diff test).
- **WI-12 Docker + compose + demo + docs** — multi-stage Dockerfile (static binary,
  unprivileged user, `/_health` healthcheck, CA support), compose (same shape,
  `stop_grace_period: 30s`), Rust mock-upstream demo (cold start simulation, SSE),
  `.env.example`, README, copilot-cli guide. *AC:* `docker compose up -d --build` →
  healthy; demo script shows COLD→WARM→stream→COLD lifecycle.
- **WI-13 Parity verification + enhancements** — run the spec §14 porting checklist
  end-to-end (mock upstream + httpmock RunPod), diff `/metrics` and `/_status`
  outputs against the Python proxy on identical scenarios, then implement the
  D-02-approved enhancements. *AC:* every §14 box checked with evidence; parity
  report in `completion.md`; N9 load test (≥ 50 concurrent SSE streams, zero
  proxy-side errors) recorded in the parity report.

## 8. Test Strategy

- **Unit:** config parsing, slug matching, catalogue validation, state transitions,
  header filtering, health classification — pure functions, fast.
- **Integration (tokio::test + httpmock):** the two seams from the spec — a mock
  upstream (SSE, cold-start delay, edge 404/502/503/504 responses) and a mock RunPod
  REST API (v1 + v2). Scenario source: the Python test suite (264 tests) — each Python
  test module maps to a Rust test module of the same name.
- **Property-style invariants:** single-flight (N tasks, one warmup), lease released
  exactly once on every path (counted in tests), at-most-one-create (counter asserted
  after matrix walks).
- **Load (N9):** tokio-based load test — ≥ 50 concurrent SSE streams through the
  proxy against the mock upstream (mixed model names to exercise routing), assert
  zero proxy-side errors, all streams complete, and semaphore saturation (small
  `PROXY_MAX_CONCURRENT_REQUESTS`) yields 503 + `Retry-After` without dropping
  established connections.
- **Clocks:** `tokio::time::pause()` + auto-advance for keepalive/prewarm/idle tests;
  injectable `now_fn` for prewarm (spec §6.4).
- **Parity diff:** run identical request sequences against Python and Rust proxies
  (both against the same mock upstream) and diff `/_status` + `/metrics` + response
  bytes (SSE included).
- **Coverage:** `cargo-llvm-cov` on the lifecycle + router + pipeline modules; target
  ≥ 90% line coverage on `lifecycle/`, `router.rs`, `proxy/` (the cost-safety and
  billing-adjacent code), ≥ 80% overall.
- **Static analysis:** clippy (pedantic, with justified allows) + rustfmt in CI;
  `cargo audit` for dependency advisories.

## 9. Enhancements (beyond parity — require D-02 sign-off)

Ordered by value/risk. None change the wire contract or §11 invariants.

1. **Footprint & boot** (free with Rust): static binary, < 20 MB RSS, < 50 ms boot.
   No action needed beyond the port itself — measured in WI-13.
2. **OpenTelemetry export** (optional feature flag): `tracing-opentelemetry` + OTLP
   metrics/traces export alongside Prometheus. Zero cost when disabled.
3. **Config file support** (optional): TOML config file as an alternative to env vars
   (same precedence: env wins), for operators who prefer files. Env-only remains the
   documented default (parity).
4. **`/_status` enrichment**: add `next_prewarm_at`, `in_flight_requests`,
   `last_revalidation_at` (additive fields; existing clients unaffected).
5. **Readiness vs liveness split**: `/_ready` reporting state-machine readiness
   (additive; `/_health` unchanged for the container healthcheck).
6. **HTTP/2 to upstream** (feature flag): if the RunPod edge supports it, h2 upstream
   connections reduce head-of-line effects for concurrent probes. Off by default.
7. **Binary hardening**: `#[deny(unsafe_code)]`, `cargo-udeps` to prune deps,
   reproducible builds (fixed timestamps) for image integrity.
8. **Cross-compile from Windows** to `x86_64-unknown-linux-musl` for the Docker image
   without a Linux builder (or BuildKit cache mounts — decided in §6 research).

## 10. Release and Rollback

- **Artifact:** Docker image `runpod-proxy-rust:<version>` (multi-stage, static
  binary, unprivileged user). Version via Cargo package version + git tag.
- **Rollout:** run the Rust container on a different port (e.g. 8081) alongside the
  Python one; point a canary client (one Copilot CLI session) at it; validate the
  §12.4 smoke test; then cutover by changing the client base URL / compose service.
- **Rollback:** revert the client base URL to the Python proxy (still running) — no
  data migration, no state to migrate (the proxy is stateless across restarts by
  design; created-pod memory is per-process in both implementations).
- **Cost-safety on cutover:** before stopping the Python proxy, confirm it is `COLD`
  (idle give-up done) so no pod is left billing; the Rust proxy's shutdown handler
  stops its own backend (spec §11.5).

## 11. Docker (research-confirmed)

Multi-stage with **cargo-chef 0.1.78** + BuildKit cache mounts
(`--mount=type=cache,target=/usr/local/cargo` and `.../registry`) for
dependency-layer caching. Two final-stage options:

- **Preferred**: musl static binary (`x86_64-unknown-linux-musl`) +
  `gcr.io/distroless/static` → 5–15 MB image. rustls means **no OpenSSL dep**.
  Caveat: distroless has no shell/wget, so a `HEALTHCHECK` instruction won't work —
  bundle the CA certs into the image and rely on the orchestrator's probe (or a
  static curl binary) for `/_health`.
- **Fallback**: gnu target + `debian:bookworm-slim` with `ca-certificates`,
  unprivileged user, `HEALTHCHECK` on `/_health` (same 30 s/5 s/5 s/3 as the
  Python image).

`EXPOSE 8080`, `stop_grace_period: 30s` in compose (matches the Python service).
Target image size: < 25 MB (vs ~150 MB+ for the Python image).

## 12. Risks

| # | Risk | Likelihood | Impact | Mitigation |
|---|------|-----------|--------|------------|
| R1 | Subtle behavior drift from the Python reference (the spec is detailed but the tests are the real contract) | Medium | High (billing leaks, broken streams) | Port test-by-test (module-name parity); parity diff of `/metrics`+`/_status`+SSE bytes; §14 checklist as gate |
| R2 | Cancellation semantics differ (asyncio cancel vs tokio drop) — reclaim-on-cancel is a billing invariant | Medium | High (silent billing) | Dedicated cancellation tests (WI-09); `CancellationToken` + shielded cleanup; QA review of every `drop` path |
| R3 | Streaming edge cases (chunked + SSE + client disconnect mid-stream) | Medium | Medium (broken agent UX) | httpmock SSE tests incl. abort; integration demo with real Copilot CLI session |
| R4 | RunPod REST v1 retirement (2026-11-15) lands mid-port | Low | Medium | `RUNPOD_REST_URL` already configurable; v2 client isolated in `runpod_api.rs` |
| R5 | Corporate TLS (Zscaler CA) breaks rustls | Low | Medium (no upstream connectivity) | Research-verified root-cert mechanism; test with the CA bundle in CI demo |
| R6 | Windows dev → Linux container cross-build friction | Low | Low | BuildKit cache mounts or musl cross-compile (research §6) |
| R7 | Scope creep into "enhancements" before parity | Medium | Medium (schedule) | D-01: parity gate first; enhancements only after WI-13 parity evidence |

## 13. Human Decisions Required

- **D-02 (Scope):** approve the enhancement list in §9 (which items, in what order)
  before implementation. Parity (WI-03..WI-13) does not need approval to proceed.
- **D-03 (Approval, at release time):** cutover of the live client base URL from the
  Python proxy to the Rust proxy (production-adjacent action; human approves).
- **D-04 (Approval, at release time):** first real RunPod pod creation via the Rust
  proxy (`RUNPOD_ALLOW_POD_CREATE=true`) — billable GPU action.

## 14. Knowledge to Capture (at finalization)

- Repo: `docs/` — README, spec copy, copilot-cli guide, demo guide.
- Personal knowledge (`~/.copilot/knowledge/repositories.md`): add ai-assist-rust row
  (path, purpose, ledger pointer).
- Generalized lessons: Rust porting patterns (single-flight, shielded cleanup,
  streaming relay) — only if non-obvious and reusable.
