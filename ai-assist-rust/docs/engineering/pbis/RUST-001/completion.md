# RUST-001: Completion & Parity Report

- PBI: RUST-001 — RunPod Warm Proxy, Rust reimplementation (parallel, enhanced)
- Date: 2026-10-05
- Status: **Complete** (D-09) — parity core + cost-safety verified; CLI shippable
- Reference: Python `runpod-proxy` (normative spec `docs/specification.md`, 264-test suite)
- Companion ledger: `status.md` (live), `plan.md` (plan + §14 gate)

This is the parity report required by the WI-13 acceptance criteria: *"every §14 box
checked with evidence; parity report in `completion.md`; N9 load test (≥ 50 concurrent
SSE streams, zero proxy-side errors) recorded in the parity report."*

## 1. Scope & Method

The Rust proxy is a drop-in replacement for the Python `runpod-proxy`. Parity was
verified three ways:

1. **Test-by-test port** — each Python test module maps to a Rust test module of the
   same name; the suite is the real contract (plan R1).
2. **End-to-end smoke** — real release binaries against a functional OpenAI-compatible
   mock upstream (`demo/mock_upstream`): proxying, SSE pass-through, `/_status` +
   `/metrics` shape, `/_warm`.
3. **§14 checklist walk** — the spec's porting checklist (each item a historical bug
   class) walked line-by-line, with the evidence below.

The suite is proxy-robust: a `#[cfg(test)] test_client()` (`.no_proxy()`) keeps
httpmock (127.0.0.1) traffic from being routed through an ambient `HTTP(S)_PROXY`,
so `cargo test --workspace` passes on machines with a corporate proxy (previously
24/234 failed with 501s).

## 2. §14 Porting Checklist (evidence per box)

**Warmup/keepalive**
- [x] First request while COLD waits (single-flight) and succeeds after backend up.
  — `warmup.rs` single-flight tests; e2e smoke (cold → WARM → 200).
- [x] N concurrent cold requests → exactly one warmup chain, all succeed together.
  — `warmup.rs` single-flight (N tasks, one warmup) invariant tests.
- [x] Warmup timeout → 503, state back to COLD, all waiters woken.
  — `warmup.rs::warmup_timeout_returns_cold`; **new** `proxy/mod.rs::warmup_timeout_returns_503_with_state`
    (asserts the 503 JSON shape + state reset).
- [x] Any HTTP response (incl. 4xx/5xx) = warm in serverless mode; pod mode classifies
  by `POD_HEALTH_MODE` (default `model`); edge set 404/502/503/504 never ready.
  — `health.rs::classify` tests (serverless any-2xx/4xx/5xx warm; pod `model`/`any` modes;
    edge set never ready).
- [x] 3 consecutive failed probes → DEGRADED; one good probe → WARM.
  — `keepalive.rs` degrade/recover tests.
- [x] Idle > IDLE_GIVEUP_S → COLD (pod stopped in pod mode); failed stop → state
  retained, retried next tick.
  — `keepalive.rs` idle-giveup + failed-stop-retry tests.
- [x] Keepalive probes only when WARM/DEGRADED and real traffic has occurred.
  — `keepalive.rs` probe-gating tests.

**Proxying**
- [x] SSE token streams relay without buffering; status/headers preserved (hop-by-hop
  filtered); `accept-encoding: identity` forced upstream.
  — `proxy/mod.rs` SSE pass-through + header-filter tests; e2e smoke (role delta →
    content deltas → `[DONE]`).
- [x] Body > MAX_BODY_BYTES → 413 without buffering the whole body.
  — `proxy/mod.rs` 413 body-cap test.
- [x] `x-proxy-key` stripped from forwarded headers; client `Authorization` stripped
  when it carried the proxy key; upstream key injected per mode/pod.
  — `proxy/mod.rs` header-filter tests (strip/inject per mode).
- [x] Upstream transport error → 502 JSON; warmup timeout → 503 JSON.
  — `proxy/mod.rs` transport-error 502 + warmup-timeout 503 tests.

**Routing/catalogue**
- [x] No allowlist → `model` field ignored entirely (backward compatible).
  — `router.rs` no-allowlist tests.
- [x] Case/slug variants resolve to the canonical allowlist spelling; forwarded body's
  `model` rewritten to canonical.
  — `router.rs` canonicalization tests; `model_slug` parity tests.
- [x] Disallowed model → 400 with `allowed` list, backend untouched.
  — `router.rs` rejection tests; **new** `proxy/mod.rs::disallowed_model_never_reaches_backend`
    (asserts the upstream mock is never hit).
- [x] Missing/non-JSON/no-model body → default model, never a failure.
  — `router.rs` default-model tests.
- [x] Switch drains (≤ MODEL_SWITCH_DRAIN_S), stops old pod, warms new, serialized;
  in-flight lease released exactly once on every path.
  — `router.rs` idle-watch lease tests; **new** `proxy/mod.rs::model_switch_drains_stops_and_rewarms`
    (asserts drain → `stop()` once → re-warm → `model_switches` incremented).
- [x] Catalogue: schema validated at boot (fail fast); exact template-id matching;
  creation matrix walks templates × gpus × min..max cheapest-first, stops on first
  success; per-model overrides fall back to env defaults.
  — `config.rs`/catalogue validation tests; **new** `lifecycle/discovery.rs::test_select_create_templates_follows_spec_order`
    (asserts §6.8d template-selection order).

**Lifecycle/cost**
- [x] Pinned pod: start skipped when RUNNING; stop skipped when EXITED; stop on
  graceful shutdown (best effort, logged).
  — `lifecycle/pinned.rs` start/stop-skip tests; `main.rs` graceful-shutdown stop.
- [x] Pinned pod deleted (`GET /pods/{id}` → 404): warmup fails fast with an actionable
  503 (no `WARMUP_TIMEOUT_S` burn, no REST start); `stop()` treats the pod as already
  stopped.
  — `lifecycle/pinned.rs` pod-not-found fail-fast tests.
- [x] Discovery: RUNNING-probe before EXITED-resume before create; `desiredStatus`
  alone is never health; created pod is sole candidate thereafter; deleted/terminated
  created pod is forgotten.
  — `lifecycle/discovery.rs` candidate-ordering + sole-candidate tests.
- [x] Created-never-healthy pods are stopped on every failure path *including*
  warmup-timeout cancellation.
  — `lifecycle/discovery.rs::test_created_pod_that_never_becomes_ready_is_reclaimed`
    (item #21, P0; RAII `CreatedPodReclaim` drop guard, red→green).
- [x] Found-already-RUNNING pods are never stopped.
  — `lifecycle/discovery.rs` found-running-never-stopped tests.
- [x] Revalidation: warm endpoint idle beyond POD_REVALIDATE_S re-probes before
  forwarding.
  — **new** `proxy/mod.rs::stale_warm_endpoint_is_revalidated`.

**Surface/ops**
- [x] `/_health` public; `/_status`, `/metrics`, `/_warm`, `/_reload` gated by
  PROXY_API_KEY (constant-time compare; `x-proxy-key` or `Bearer`).
  — `auth.rs` constant-time compare tests; **new** `auth.rs::auth_middleware_gates_routes`
    (integration: public vs gated routes).
- [x] Prewarm fires once per slot per day, within the lateness window, active model
  only, survives a failed slot.
  — `prewarm.rs` slot/lateness/active-model tests; **new**
    `prewarm.rs::failed_slot_is_dropped_but_scheduler_survives`.
- [x] Boot validation per §5.5 (missing required env / bad mode / bad catalogue →
  clear failure).
  — `config.rs::validate_boot` tests.

**Result: all §14 boxes checked.** The one P0 gap the walk surfaced (item #21,
cost-safety leak) was fixed and TDD-tested before sign-off.

## 3. N9 Load Test (D-07)

- **60 concurrent SSE streams** through the proxy against the mock upstream (mixed
  model names to exercise routing).
- All HTTP 200; all reached `[DONE]`; **`requests_failed=0`** (zero proxy-side errors).
- Exceeds the ≥ 50-stream AC. Semaphore saturation (small
  `PROXY_MAX_CONCURRENT_REQUESTS`) yields 503 + `Retry-After` without dropping
  established connections (covered by `proxy/mod.rs` N9 saturation tests).

## 4. D-02 Enhancement Benchmarks (plan §9.1 — Footprint & boot)

Measured on the release binary (`cargo build --release`, `stable-x86_64-pc-windows-gnu`),
serverless mode, minimal config, `/_health` (public, no upstream hit):

| Metric | Target (plan §9.1) | Measured | Verdict |
| --- | --- | --- | --- |
| Release binary size | static binary | 11.38 MB (11,928,504 B) | single self-contained `.exe` |
| Idle RSS | < 20 MB | ~16.1 MB (5-run median) | **met** |
| Application boot (config → serve) | < 50 ms | ~30 ms (`starting` → `listening` log delta) | **met** |
| Process start → `/_health` 200 | < 50 ms (Linux musl) | ~2.5 s (Windows) | see caveat |

**Caveat (Windows vs Linux-musl):** the "static binary" and "< 50 ms boot" targets are
for the production **Linux musl** build. On this Windows box the artifact is a PE `.exe`
that loads DLLs, so the end-to-end process-start-to-serving time (~2.5 s) is dominated
by Windows process creation + DLL loading, not the proxy. The proxy's own
application-level boot (config parse → pipeline construction → TCP bind → serving) is
~30 ms, which meets the < 50 ms target. The Linux musl build (D-06) is expected to meet
the full target; it is not built on this box.

**Not implemented (D-02 sign-off pending for implementation):** plan §9 items 2–8
(OpenTelemetry export, TOML config, `/_status` enrichment, `/_ready` split, HTTP/2
upstream, binary hardening, Windows→musl cross-compile). Item 1 (footprint & boot) is
measured above; the rest are out of scope for this PBI.

### 4.1 Native Linux measurement (2026-10-05)

Built and executed locally in **Ubuntu 26.04 under WSL2**, x86-64, Linux
6.18.40.1-microsoft-standard-WSL2, using Rust **1.98.1** and the existing lockfile:

```text
cargo +1.98.1 build --locked --release -p runpod-proxy -p mock-upstream --quiet
```

Build output resides on the Linux filesystem at
`~/.cache/ai-assist-rust-benchmark/target/release/runpod-proxy`; the Windows build
output was not replaced. No application dependencies or source were changed.

| Metric | Measured |
| --- | --- |
| Release executable, unstripped | **8,534,040 bytes (8.14 MiB)** |
| Idle RSS, median of 10 launches | **14.29 MiB** |
| Process launch to successful `/_health`, median | **11.53 ms** |
| Process launch to successful `/_health`, observed range | **10.45-20.58 ms** |
| Model-listing smoke | Passed against the bundled mock |
| SSE smoke/load | 60 requests dispatched with 60 client workers; all HTTP 200 and `[DONE]` |
| Proxy shutdown | SIGTERM followed by exit code 0 |

**Method:** a Python harness running inside Linux used a monotonic clock starting
immediately before `subprocess.Popen`, then polled `/_health` over loopback with
proxies disabled and 1 ms retry sleeps. Each sample launched a fresh process in
serverless mode with a dummy upstream, no API keys, and `RUST_LOG=warn`. RSS was
read from `/proc/<pid>/status` one second after health succeeded. Startup includes
process creation, dynamic loading, initialization, polling delay, and the health
request. These are repeated-launch measurements without filesystem cache flushing,
not cold-disk or bare-metal measurements; GPU/model warmup is not included.

**Linkage and portability:** `file` and `ldd` confirm an ELF **GNU/glibc dynamically
linked** executable, not a static musl build. The existing `reqwest` configuration
uses `native-tls`; runtime dependencies include OpenSSL (`libssl.so.3`,
`libcrypto.so.3`), glibc, and other system libraries. The measurement host has
glibc 2.43 and OpenSSL 3.5.5; the executable references GLIBC symbols through
GLIBC_2.34. Compatible libraries are required on the destination machine, and
other distributions have not been verified. Earlier static/baked-in-CA deployment
descriptions are design intentions, not evidence for this GNU Linux artifact.
The measured footprint and startup meet the numeric targets on WSL2, but do not
validate static musl packaging or performance on another host.

Artifact SHA-256:
`0e2971b71ee7ff8ce34469627c91113d726ff18e2f62875214169ecd96cd6d95`.
Raw samples and the benchmark harness are retained in the local session artifacts;
Linux also retains `~/.cache/ai-assist-rust-benchmark/results.json`.
The subsequent same-host Python comparison is recorded in section 4.2.

### 4.2 Python versus Rust, same Linux environment (2026-10-05)

Both implementations were measured in the same Ubuntu WSL2 environment, using
the same Rust mock upstream and serverless settings. The Rust release artifact
is unchanged from section 4.1. The Python reference source was copied read-only
to the Linux filesystem to avoid mounted-Windows-filesystem import overhead;
neither implementation's application source was modified.

Python ran as one Uvicorn worker, without reload or access logging, using
Python 3.14.4, FastAPI 0.142.2, httpx 0.28.1, Uvicorn 0.54.0, uvloop 0.23.0,
httptools 0.8.0, and PyYAML 6.0.3. Dependencies were resolved from the reference's
`requirements.txt` into an isolated virtual environment; they are not a snapshot
of packages deployed with an earlier Python release. Rust used `RUST_LOG=warn`;
Python used `LOG_LEVEL=WARNING` and Uvicorn `--log-level warning`. Both had no
API keys, model `qwen`, and no ambient HTTP proxies.

| Metric | Rust | Python | Observed difference |
| --- | --- | --- | --- |
| Idle RSS, median | **14.24 MiB** | **63.69 MiB** | Rust uses about **78% less** |
| Process launch to healthy, median | **18.18 ms** | **444.21 ms** | Rust is about **24.4x faster** |
| Warm `GET /v1/models`, median latency | **0.39 ms** | **1.70 ms** | Rust is about **4.4x faster** |
| Mock non-stream chat, 20 clients, observed requests/s | **2,721** | **370** | Rust achieved about **7.4x** the request rate |
| Same chat workload, median request latency | **6.55 ms** | **44.53 ms** | Rust is about **6.8x faster** |
| SSE completion checks | **180/180** | **180/180** | Both HTTP 200 and `[DONE]` |

**Startup/RSS method:** 10 fresh process launches per implementation, alternating
the order each iteration. Time starts before process creation and ends after a
successful public health request; RSS is sampled one second later via
`/proc/<pid>/status`. Python's startup range was 247.70-846.96 ms; Rust's was
10.66-30.31 ms. Like section 4.1, caches were not flushed. These new paired
measurements supersede using separate historical runs to calculate ratios.

**Warm-request method:** three rounds, alternating implementation order. A
Python standard-library `http.client` harness used persistent loopback HTTP/1.1
connections, 20 warmup requests, then 300 sequential model-listing requests.
Chat used 20 synchronized client threads, each issuing 20 non-stream completions
on its own connection (400 requests per round). All responses were checked for
HTTP 200 and the expected model or completion shape. The table reports the
median of the three per-round medians/rates, not a production capacity estimate.
Rust's observed chat rate ranged from 1,977 to 2,878 requests/s; Python's ranged
from 283 to 420 requests/s.

**Control and discarded timing results:** an initial async-httpx client run
showed approximately 18-20 requests/s even directly against the mock, with
large latency outliers. Its timing results cannot isolate proxy performance and
were not used for speed claims. The simpler persistent-connection control
measured the direct mock at 0.18 ms median model-listing latency and about
2,600 chat requests/s. Rust is therefore near the client/mock ceiling here;
its sometimes higher rate than the direct baseline reflects run variation,
not negative proxy overhead. The 180 successful SSE checks per implementation
come from the initial three rounds; their latency measurements are not used.

**Limits:** local plaintext loopback traffic, small mock responses, no TLS,
no real RunPod lifecycle calls or GPU inference. The throughput result does
not imply faster model token generation or a 7.4x end-user speedup. Python
multi-worker tuning, sustained load, CPU consumption, loaded RSS, and production
latency were not measured. Rust's existing GNU/OpenSSL linkage caveat still
applies. There is no directly comparable Python single-binary artifact size.

The harnesses, dependency versions, source hashes, raw launch samples, and
workload results are retained in local session artifacts. Linux retains
`~/.cache/ai-assist-rust-benchmark/comparison-results.json` and
`~/.cache/ai-assist-rust-benchmark/latency-control-results.json`.

## 5. Parity Gaps Found & Fixed During Verification

1. **Discovery template-selection order (spec §6.8d)** — `load_template_map` /
   `template_ids_for_active_model` / `match_reason` / `select_create_templates` did not
   follow the spec's candidate ordering. Rewrote to spec §6.8c/§6.8d; locked with the
   new `test_select_create_templates_follows_spec_order`.
2. **Cost-safety leak (item #21, P0)** — the discovery create path reclaimed a created
   pod only when it reached RUNNING-but-unhealthy; a pod stuck STARTING until the
   deadline, or dropped when the warmup `tokio::time::timeout` cancels `start()`, was
   orphaned (billing until manual `stop()`). Python reclaims in a `finally:
   reclaim_before_cancel` that survives cancellation. Fixed with an RAII
   `CreatedPodReclaim` drop guard (disarmed only on healthy) that spawns the shielded
   stop on every other exit — including cancellation-by-drop. TDD-verified.
3. **Catch-all routing** — route used pre-axum-0.8 `/{path:path}` (single-segment only),
   silently 404-ing every real multi-segment path. Fixed to axum 0.8 `/{*path}` + `/`
   root route; locked with `tests/routing.rs`.
4. **SIGTERM not handled** — graceful shutdown only wired `ctrl_c()`; added a cfg-gated
   Unix SIGTERM arm (Ctrl+C retained on all platforms).
5. **Test-suite proxy fragility** — ambient `HTTP(S)_PROXY` with empty `NO_PROXY`
   routed httpmock traffic through the proxy → 501s (24/234 failed). Added
   `#[cfg(test)] test_client()` (`.no_proxy()`); production client unchanged.

**Finding (no action, documented):** `allow_pod_create` is parsed and validated but not
yet consumed by the discovery create path in a way that changes behavior beyond the
existing matrix; recorded here so the config surface and the implementation stay in
sync. (No wire-contract or parity impact.)

## 6. Test Suite & Coverage

- **244 tests pass** — `cargo test --workspace` (240 unit + 4 `tests/routing.rs`
  integration); 0 failed. Up from the 234-test baseline at D-09. Includes 3
  `validate_reloaded` tests added 2026-10-05 (reload success, empty-catalogue
  rejection, dropped-default-model rejection) covering the `POST /_reload`
  control-plane path.
- **7 coverage-hardening tests added** (the D-09 deferred set, now complete):
  1. `proxy/mod.rs::stale_warm_endpoint_is_revalidated` — §14 revalidation.
  2. `proxy/mod.rs::model_switch_drains_stops_and_rewarms` — §14 switch.
  3. `lifecycle/discovery.rs::test_select_create_templates_follows_spec_order` — §14 creation matrix.
  4. `proxy/mod.rs::warmup_timeout_returns_503_with_state` — §14 warmup timeout.
  5. `proxy/mod.rs::disallowed_model_never_reaches_backend` — §14 disallowed model.
  6. `auth.rs::auth_middleware_gates_routes` — §14 control-plane auth.
  7. `prewarm.rs::failed_slot_is_dropped_but_scheduler_survives` — §14 prewarm.
- **Static analysis:** `cargo clippy --all-targets` 0 warnings (pedantic; pre-existing
  dead-code warnings cleaned up this phase); `cargo fmt --all -- --check` clean.
- **Line coverage:** not measured (`cargo-llvm-cov` not run in this environment); the
  7 added tests target the previously-thin cost-safety / routing / control-plane paths.

## 7. Residual / Deferred

- **Docker/compose packaging (WI-12)** — parked per D-08; the app is shippable as a
  standalone CLI binary. Lift when container deployment is wanted.
- **D-02 enhancements 2–8** — not implemented (sign-off pending for implementation);
  item 1 (footprint & boot) measured in §4 above.
- **Linux musl build** — not built on this Windows box (D-06); the §9.1 static-binary /
  < 50 ms boot targets apply to that build.

## 8. Conclusion

Parity with the Python `runpod-proxy` is achieved: every §14 box is checked with
evidence, the one P0 cost-safety gap (item #21) is fixed and TDD-tested, the N9 load
test (60 concurrent SSE streams) shows zero proxy-side errors, and the D-02 footprint &
boot benchmarks meet the RSS and application-boot targets. The Rust proxy is a
drop-in replacement for the same clients (Copilot CLI / OpenAI-compatible SDKs) and is
shippable as a standalone CLI binary. **PBI RUST-001 is complete (D-09).**
