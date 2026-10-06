# RunPod Serverless Warm Proxy — Implementation Specification

**Version:** 1.0 (spec of the Python reference implementation in `proxy/`)
**Status:** Normative reference for recreating this system in any language/runtime.

This document defines the problem the solution addresses, what the solution does, and —
with enough behavioral precision (algorithms, state machines, wire formats, error
contracts, edge cases) — how to recreate it from scratch in this or another language.
The Python layout in section 13 maps every requirement to a file in the reference
implementation; porters may use any decomposition that preserves the behaviors.

---

## 1. Problem statement

### 1.1 Context

Large language model (LLM) inference workloads — especially agentic coding tools such as
GitHub Copilot CLI, Claude Code, or scripts using OpenAI-compatible SDKs — are commonly
served by self-hosted model servers (e.g. vLLM) on rented GPUs. RunPod offers two
deployment shapes:

1. **Serverless endpoints** — billed only while a worker is running. When no requests
   arrive, RunPod scales the worker to zero; the next request pays a **cold start**
   (image pull + model weight load: roughly 1–10 minutes for large models) and may sit
   in a **queue** while capacity is allocated.
2. **Persistent pods** — billed continuously while running, with a persistent
   filesystem, but they keep billing even when idle.

### 1.2 The problems

**P1. Cold-start latency breaks agent UX.** An agent (or a human) issues a request, and
the first one after a scale-down blocks for minutes while the model loads. Agentic
workflows often fire the first "hello" request early and then continue working; that
request must not fail or hang.

**P2. Billing vs. latency trade-off is manual.** Keeping a pod running 24/7 avoids cold
starts but wastes money during idle periods. Stopping it saves money but reintroduces
P1. The user wants: *pay only while actively working, but be warm when I work.*

**P3. Nothing in RunPod's serverless API lets you "hold" a worker.** The platform has
an `idleTimeout`, but an interactive session is a bursty pattern: short requests
separated by seconds of thinking/typing, then long silence. No single idleTimeout value
matches that pattern.

**P4 (pod mode).** With pods, the start/stop lifecycle is available via REST API but is
manual. An assistant that works 9–17h wants its pod started shortly before the workday
and stopped after, without a cron on the user's machine, and without the user hunting
for which pod serves which model when several models/pods exist.

### 1.3 Goal

A tiny, always-on, cheap (~50 MB RAM) **reverse proxy** that sits between the client and
the billable GPU backend and **masks cold starts**:

- The first request after a cold period **waits for the backend to become ready**
  instead of failing, sharing that wait across all concurrent requests.
- While the user is actively working, **heartbeats keep the backend alive** so
  subsequent requests are instant.
- When the user goes quiet, the proxy **stops the heartbeats** (and, in pod mode,
  explicitly stops the pod) so billing stops.

Secondary goals: **per-request model routing** across multiple models/pods with
cost-safe switching, a **declarative model catalogue** (model → templates → GPUs) for
exact matching/creation instead of name heuristics, **scheduled pre-warming**, and
**operational visibility** (status/metrics endpoints).

### 1.4 Non-goals

- Not a general-purpose API gateway (no rate limiting, auth providers, TLS termination
  beyond what the host provides, caching, or response transformation).
- Not a multi-tenant product: one proxy instance manages one account's backends and
  serves a set of trusted clients.
- Not a model *scheduler*: exactly **one** model is active at a time; interleaved
  concurrent traffic across two models is explicitly not supported (it would thrash).
- Does not proxy or store conversation state; request bodies are at most inspected for
  the `model` field (bounded, see §8.5) and passed through.

---

## 2. Solution overview

```
 GitHub Copilot CLI / Claude Code / any OpenAI-compatible client
     |   http://localhost:8080   (client base URL)
     v
 +-----------------------------------------------------------+
 | runpod-proxy  (always-on, tiny)                           |
 |                                                           |
 |  HTTP surface   auth middleware (PROXY_API_KEY)           |
 |  /_health /_status /metrics /_warm /_reload + proxy       |
 |                                                           |
 |  ModelRouter      per-request model selection,           |
 |                   serialized model switches,             |
 |                   in-flight request leases               |
 |                                                           |
 |  WarmupManager    single-flight warmup: first request    |
 |                   triggers the backend warmup; all       |
 |                   concurrent requests share it           |
 |                                                           |
 |  KeepaliveLoop    periodic probes while active;          |
 |                   idle give-up -> COLD (stop billing)    |
 |                                                           |
 |  Lifecycle        what "warm up" / "give up" mean for    |
 |                   the backend:                           |
 |                   serverless: no-op (scale on demand)    |
 |                   pinned pod: REST start/stop            |
 |                   discovery: find/resume/create pod      |
 |                                                           |
 |  PrewarmScheduler optional scheduled warm-ups            |
 |  State            COLD/WARMING/WARM/DEGRADED + counters  |
 +-----------------------------------------------------------+
     |  GET {upstream}/{WARMUP_PATH}        (probe: warmup + keepalive)
     |  *  {upstream}/<any path> (verbatim, streamed)
     v
 RunPod serverless endpoint (queue-based)   OR   persistent GPU pod
 (vLLM / any OpenAI-compatible worker)
```

**How it solves the problems:**

- **P1** — warmup: the first request while `COLD` triggers the backend to come up and
  *holds the client's HTTP request open* until the worker answers a readiness probe.
  Concurrent requests join the same warmup (single-flight; no stampede of probes).
- **P2/P3** — keepalive + idle give-up: while `WARM`, the proxy probes every
  `KEEPALIVE_INTERVAL_S`; but only if real client traffic arrived within
  `IDLE_GIVEUP_S`. After `IDLE_GIVEUP_S` of silence the proxy stops probing (and stops
  the pod in pod mode) — billing stops, and the next session pays one cold start.
- **P4** — pod-mode lifecycle management: explicit REST start/stop, plus *discovery*
  mode that resolves "which pod serves model X" on demand, with optional creation,
  scheduled pre-warm, and cost-safe reclamation of pods it brought up.

---

## 3. Requirements

### 3.1 Functional

- **F1 Transparent proxying.** Forward any path, method (GET/POST/PUT/DELETE/PATCH/
  HEAD/OPTIONS), query string, and body to the upstream **unchanged** (except the
  documented header and `model`-field rewrites in §8.5). Responses — including SSE
  token streams — must pass through **without buffering**: proxy one chunk at a time.
- **F2 Warmup on demand.** A request arriving while the backend is not up must cause
  the backend to become up, and the request must be held (not failed) until it does or
  until `WARMUP_TIMEOUT_S` elapses, after which the request fails with HTTP 503.
- **F3 Single-flight warmup.** N concurrent requests during a cold period cause exactly
  one warmup attempt chain; all N wait on it and proceed together.
- **F4 Keepalive.** While warm and the client is active (traffic within
  `IDLE_GIVEUP_S`), probe the backend every `KEEPALIVE_INTERVAL_S` so the platform's
  own idle timeout never fires mid-session.
- **F5 Idle give-up.** After `IDLE_GIVEUP_S` of no real client traffic, stop probing and
  transition to `COLD`. In pod mode this also sends a stop to the backend (pod mode
  only; serverless scales down on its own).
- **F6 Degraded detection.** 3 consecutive failed probes mark the endpoint `DEGRADED`;
  probing continues and one successful probe returns it to `WARM`. (A recycled/crashed
  backend is detected within 3 intervals and replaced on the next request.)
- **F7 Pod lifecycle (pinned).** With `RUNPOD_POD_ID` set: idempotent start on warmup
  (skip start if already RUNNING), stop on idle give-up (skip if already EXITED), stop
  on graceful shutdown (best effort). A `GET /pods/{id}` 404 means the pod was deleted
  (RunPod never reuses a deleted id): warmup **fails fast** — the in-flight warmup is
  aborted with an actionable 503 ("point `RUNPOD_POD_ID` at a live pod, or unset it
  to enable pod discovery") instead of burning `WARMUP_TIMEOUT_S` on doomed starts —
  and `stop()` treats the deleted pod as already stopped. If a start is blocked
  because the pod's original host has no free GPU (RunPod's "please migrate your
  pod" prompt, or the REST 500 "not enough free GPUs on the host machine" — a
  pinned pod keeps its machine assignment, so start retries can never succeed),
  the `RUNPOD_ON_MIGRATE` policy applies: `fail` (default) surfaces the error with
  the fix hint and leaves the pod untouched; `replace` terminates the pod, creates
  a fresh one with the same spec (same template/GPUs; network volume re-attached
  so on-disk data such as HF model weights is not re-downloaded), adopts the new
  id and URL (per-pod upstream keys derived via
  `RUNPOD_UPSTREAM_API_KEY_TEMPLATE` follow the new id), and waits for RUNNING
  within the remaining warmup budget. Non-2xx lifecycle responses log and surface
  RunPod's error message from the response body.
- **F8 Pod discovery.** With pod mode and no `RUNPOD_POD_ID` but a default model: on
  warmup, find a pod that serves the active model — prefer `RUNNING` pods (health
  probed), then resume `EXITED` pods — health-probe each candidate; adopt the first
  healthy one. A pod is healthy iff its warmup-route response is classified ready by
  `POD_HEALTH_MODE` (§6.8a); `desiredStatus = RUNNING` alone is **not** health (the
  model server may still be loading weights).
- **F9 Pod creation (opt-in).** Only when `RUNPOD_ALLOW_POD_CREATE=true`: if no
  existing pod can be reused, create one from a matching template. At most **one pod is
  created per model per proxy process**; its id is remembered and later attempts resume
  it, never create another. Any pod the proxy started/created that never becomes
  healthy **must be stopped** (reclaimed) to prevent silent billing. A pod found already
  RUNNING is **never** stopped by the proxy.
- **F10 Per-request model routing.** When an allowlist is configured (via
  `RUNPOD_ALLOWED_MODELS` or a model catalogue), inspect JSON request bodies for a
  top-level string `model` field and route to that model; reject non-allowed models
  with 400 **without touching the backend**; fall back to the default model when the
  body has no usable `model` field (a request must never fail merely for omitting it).
- **F11 Model switching.** Serving model B when model A is active must: best-effort
  drain in-flight requests for A (bounded by `MODEL_SWITCH_DRAIN_S`), stop A's pod,
  then run discovery/warmup for B. Switching is serialized (one switch at a time).
- **F12 Model catalogue.** Accept a declarative JSON catalogue (file or inline) mapping
  model → ordered template names → ordered GPU preferences (+ per-model port/disk/
  volume/cloud overrides). The catalogue becomes the allowlist, drives *exact*
  template-id matching during discovery, and drives creation matrix walking. Invalid
  catalogues are a **boot failure**.
- **F13 Pre-warming.** Optional list of local times (HH:MM) at which the proxy warms
  the *active* model ahead of time (e.g. before the workday). One fire per slot per
  calendar day; missed slots fire late within a bounded window; the scheduler never
  triggers a model switch.
- **F14 Revalidation.** In discovery mode, a warm endpoint whose last successful
  upstream response is older than `POD_REVALIDATE_S` is re-probed (forced warmup)
  before forwarding, to catch pods stopped/deleted/reclaimed externally.
- **F15 Operations surface.** `/_health` (public liveness), `/_status` (state machine
  view), `/metrics` (Prometheus text), `/_warm` (manual warm of the active model,
  which also anchors the idle window), `/_reload` (hot-reload the model catalogue
  from its configured source; validated first, atomic swap, rejected reloads are
  a no-op — §8.1).
- **F16 Auth gate.** When `PROXY_API_KEY` is set, every route except `/_health`
  requires the key as `x-proxy-key: <key>` or `Authorization: Bearer <key>`; other
  requests get 401.

### 3.2 Non-functional

- **N1 Streaming safety.** No response buffering: bytes are relayed as they arrive
  (SSE-compatible). Force `accept-encoding: identity` upstream so the response is
  never compressed mid-relay.
- **N2 Small memory footprint.** One shared async HTTP client; bounded request-body
  buffering (reject with 413 above `MAX_BODY_BYTES`, default 50 MiB; streaming read —
  never buffer an unbounded upload).
- **N3 Cost safety (invariants).** See §11 — no silent billing leaks: created pods are
  remembered per model, failed stops are retried, shutdown stops the backend.
- **N4 Credential separation.** The RunPod management key is used only for lifecycle
  REST calls, never sent to the pod's HTTP endpoint. The upstream (model-server) key
  and the client-facing proxy key are separate and are stripped/managed per §9.
- **N5 Secrets in logs.** Pod `env` values may contain secrets and must never be
  logged; non-matching pods log id/name/image/desiredStatus only.
- **N6 Graceful shutdown.** On SIGTERM: stop prewarm + keepalive, stop the pod-mode
  backend (best effort), close the HTTP client. In-flight streams are given a 30 s
  stop grace (deployment concern, §12.3).
- **N7 Fail fast on bad config.** Invalid mode, invalid catalogue, missing required
  env per mode → the process fails to start with a clear error.
- **N8 Idempotent lifecycle.** Start/stop must be safe to call on an already-started/
  stopped backend (RunPod rejects starting an already-running pod).

---

## 4. Backend state machine

One global state per proxy process (the proxy serves one active model at a time):

```
                  classified ready
    (serverless: any HTTP response; pod
     mode: POD_HEALTH_MODE, default model)
                               |
                               v
   COLD =====> WARMING =====> WARM <=======> DEGRADED
     ^             |             |  ^             |  ^
     |             |             |  |             |  |
     |             |             |  | one successful
     |             |             |  | probe (heals)
     |             |             |  |             |  |
     |             |             +---------- 3 consecutive
     |             |                               failed
     |             |                               probes
     |             |
     |             +-- warmup timeout / aborted / forced model switch
     |
     +-- idle > IDLE_GIVEUP_S, reached from WARM or DEGRADED
         (pod mode: only after the backend stop succeeds;
          serverless: stop is a no-op, so unconditionally)
```

States:

| State | Meaning | Probes sent? | Requests forwarded? |
|---|---|---|---|
| `COLD` | Backend assumed down; let it idle (billing stopped). | No | Yes — but each first triggers warmup |
| `WARMING` | A shared warmup attempt is in progress. | Yes (the warmup's own) | Yes — but each joins the in-flight warmup first |
| `WARM` | Backend confirmed answering. | Yes, on keepalive ticks | Yes, directly |
| `DEGRADED` | Backend may be down (3 consecutive probe failures); still being probed so it can self-heal. | Yes | Yes — but each re-warms first (join or start) |

Transition rules (normative):

1. **COLD/WARMING/DEGRADED → WARM**: only when a readiness probe is classified ready —
   serverless: an HTTP response from the backend (§6.3; in pod mode synthetic edge
   statuses do not count); pod mode: per `POD_HEALTH_MODE` (§6.8a), default `model`.
2. **WARM → DEGRADED**: `consecutive_keepalive_failures >= 3`.
3. **DEGRADED → WARM**: one successful keepalive probe (reset failure counter).
4. **WARM/DEGRADED → COLD**: idle give-up — `now - last_real_traffic_at > IDLE_GIVEUP_S`
   *and* (pod mode) the backend stop succeeded. If the stop fails, stay in the current
   state and retry the stop on the next tick. (Serverless: stop is a no-op, so the
   transition happens unconditionally.)
5. **WARMING → COLD**: warmup timeout, warmup aborted by an unexpected error, or forced
   model switch (§6.3) — any of these must wake all waiters with a failure.
6. **COLD → WARMING**: when `ensure_warm()` starts an attempt (first request, re-warm
   from DEGRADED, forced revalidation, `/_warm`, or prewarm).

`consecutive_keepalive_failures` resets to 0 on any successful probe (warmup or
keepalive). `last_real_traffic_at` is updated by **any** request that reaches the
proxying route (before warmup), by `/_warm`, and is what the idle give-up measures.

---

## 5. Configuration reference

All configuration is via environment variables (a config object is built once at
startup; nothing is hot-reloaded except the mounted catalogue file on container
restart). Parsing rules: strings are trimmed; numbers parse as float-then-int where
noted; booleans accept `1/true/yes/on` (case-insensitive), anything else is the
default; empty string = unset.

### 5.1 Backend selection

| Variable | Default | Applies to | Meaning |
|---|---|---|---|
| `RUNPOD_MODE` | `serverless` | all | `serverless` or `pod`. Any other value → boot error. |
| `RUNPOD_SERVERLESS_URL` | — (required in serverless mode) | serverless | Base URL of the endpoint, **no trailing slash**. Client paths are appended verbatim, so do not include `/v1` here. E.g. `https://<endpoint-id>.api.runpod.ai` (raw) or `https://api.runpod.ai/v2/<endpoint-id>` (OpenAI-compatible). |
| `RUNPOD_API_KEY` | `""` (required in pod mode) | all | RunPod API key. In **serverless** mode it also authenticates upstream requests (injected as `Authorization: Bearer` unless `RUNPOD_UPSTREAM_API_KEY` overrides). In **pod** mode it is used **only** for lifecycle REST calls — never sent to the pod's HTTP endpoint. |
| `RUNPOD_POD_ID` | `""` | pod | Pinned pod id. Empty → **discovery mode** (requires a default model, see `RUNPOD_MODEL_NAME`). |
| `RUNPOD_POD_PORT` | `8000` | pod | Port the model server listens on inside the pod; used to derive `https://<pod-id>-<port>.proxy.runpod.net` and to pick among the pod's exposed HTTP ports during discovery. |
| `RUNPOD_POD_URL` | `""` | pod | Explicit upstream URL override (pinned mode; beats the derived URL). |
| `RUNPOD_REST_URL` | `https://rest.runpod.io/v1` | pod | RunPod REST API base (overridable because RunPod is retiring REST v1 on 2026-11-15). |
| `RUNPOD_AVAILABILITY_URL` | `https://api.runpod.io/v2` | pod | RunPod v2 API base for the GPU availability poller (§10, "GPU availability polling"). |
| `GPU_AVAILABILITY_INTERVAL_S` | `300` | pod | Seconds between GPU availability polls; `0` disables the poller. |

### 5.2 Model routing & catalogue

| Variable | Default | Meaning |
|---|---|---|
| `RUNPOD_MODEL_NAME` | `""` | Default/active model name. In discovery mode this is required (it is what pods are discovered by). It is also the default routing target. If a catalogue is present, this must be one of the catalogue's models (else boot error); the default model is otherwise the first catalogue entry. |
| `RUNPOD_ALLOWED_MODELS` | `""` | Comma-separated model allowlist. Non-empty → per-request routing enabled (§6.3, §8.4). Superseded by the catalogue when a catalogue is configured. |
| `RUNPOD_MODELS_FILE` | `""` | Path to the catalogue file — JSON or YAML, dispatched on extension (wins if both set). Missing/unreadable file, invalid JSON/YAML, or schema violation → **boot failure**. |
| `RUNPOD_MODELS_JSON` | `""` | Catalogue inline as a single-line JSON string (no multi-line values in `env_file`). Same validation. |
| `MODEL_SWITCH_DRAIN_S` | `30` | Best-effort seconds to wait for in-flight requests before stopping the pod during a model switch. Expiry proceeds anyway (may cut streams). |
| `RUNPOD_ALLOW_POD_CREATE` | `false` | Discovery may create a pod (billable!). Default is reuse/resume only. |
| `RUNPOD_GPU_TYPE_IDS` | `""` | Comma-separated GPU type ids, tried in order (used when the catalogue defines no `gpus` for the model). |
| `RUNPOD_GPU_TYPE_PRIORITY` | `availability` | Accepted for compatibility only — the v2 create API takes a single gpu id and has no priority field, so this value is never sent to RunPod. |
| `RUNPOD_CLOUD_TYPE` | `SECURE` | Cloud type for created pods (per-model override via catalogue). |
| `RUNPOD_TEMPLATE_NAME` | `""` | Exact template name (case-insensitive) to create from; beats catalogue/legacy template selection. |
| `RUNPOD_MAX_CREATE_ATTEMPTS` | `12` | Hard cap on create attempts per discovery when walking the template × GPU × count matrix. |
| `RUNPOD_CONTAINER_DISK_GB` | unset | Container disk (GiB) for created pods; template default when unset (per-model override via catalogue). |
| `RUNPOD_VOLUME_GB` | unset | Volume size (GiB) for created pods; template default when unset. |
| `POD_REVALIDATE_S` | `300` | Discovery mode: if the last *successful upstream response* is older than this, force a re-warmup (revalidation probe) before forwarding. |
| `POD_HEALTH_TIMEOUT_S` | `180` | Budget for HTTP health-probing a (re)started pod. |
| `POD_READY_TIMEOUT_S` | `120` | Budget for waiting until the pod reports `desiredStatus = RUNNING`. |
| `POD_HEALTH_MODE` | `model` | Pod-mode readiness bar classifying the warmup-route response (§6.8a). `model` (default) — the warmup route's JSON must list the target model (case/slug-insensitive); `any` — any non-synthetic HTTP response (legacy; only safe for servers that refuse to answer before the model is loaded, e.g. vLLM's `/v1/models`); `completion` — a real 1-token `POST /chat/completions` must succeed (strictest; validates the full inference path). Serverless mode always uses `any`. The default guards against backends that answer HTTP while weights are still loading. |

### 5.3 Timing & proxy behavior

| Variable | Default | Meaning |
|---|---|---|
| `WARMUP_PATH` | `v1/models` | Path (relative to base URL) used for warmup/keepalive probes: `GET {upstream}/{path}`. Any HTTP response counts as "up" (serverless). Must be a route the worker actually serves — `GET /v1/models` on any OpenAI-compatible worker. |
| `WARMUP_TIMEOUT_S` | `600` | Total budget for one warmup (including all backoff sleeps and lifecycle start). |
| `WARMUP_BACKOFF_MAX_S` | `15` | Backoff cap; retries start at 1 s and double. |
| `KEEPALIVE_INTERVAL_S` | `25` | Probe period while WARM/DEGRADED and active. **Must be smaller than the platform's idle timeout** (RunPod serverless default is 5 s — set the endpoint's idleTimeout to 30–60 s, or lower this). |
| `IDLE_GIVEUP_S` | `300` | Silence threshold after which keepalive stops and (pod mode) the pod is stopped. |
| `REQUEST_TIMEOUT_S` | `300` | Per-request upstream timeout (read/write/pool). Connect timeout is fixed at 10 s. |
| `MAX_BODY_BYTES` | `52428800` (50 MiB) | Max request body; over → HTTP 413. `0` = unlimited (discouraged: unbounded-upload OOM vector). |
| `PREWARM_TIMES` | `""` | Comma-separated local `HH:MM` slots to pre-warm the active model. Invalid parts are dropped with a warning. Uses the host/container local time (set `TZ` to pin a zone). |
| `PROXY_API_KEY` | `""` | Shared secret gating all routes except `/_health` (§8.2). Strongly recommended if the proxy is reachable beyond localhost. |
| `PORT` | `8080` | Port for direct `uvicorn` runs. Ignored by the Docker image (always 8080). |
| `LOG_LEVEL` | `INFO` | `DEBUG` / `INFO` / `WARNING`. |
| `LOG_FORMAT` | `text` | `text` (default) or `json` — one JSON object per line (`ts`, `level`, `logger`, `request_id`, `msg`, plus `exc` on exceptions) for log shippers. |

### 5.4 Upstream credentials

| Variable | Default | Meaning |
|---|---|---|
| `RUNPOD_UPSTREAM_API_KEY` | `""` | Static model-server API key. When set, injected as `Authorization: Bearer <key>` into probes **and** forwarded requests (both modes). In serverless mode, when unset, `RUNPOD_API_KEY` is used instead; in pod mode, when unset, no auth header is injected (unauthenticated model server). |
| `RUNPOD_UPSTREAM_API_KEY_TEMPLATE` | `""` | Template with a `{pod_id}` placeholder (e.g. `sk-{pod_id}`) for pods whose model-server key is derived from the pod id (e.g. `VLLM_API_KEY=sk-$RUNPOD_POD_ID` in some vLLM templates). When set and a pod id is known, it takes precedence over the static key — a static key would go stale on every new pod. Write the placeholder literally as `{pod_id}`: a `$RUNPOD_POD_ID` is expanded by docker-compose at deploy time and would freeze the key at the id current at last deploy. |

### 5.5 Derived configuration (must be computed the same way)

```
upstream_url:
  pod mode:  RUNPOD_POD_URL
             | (if empty and RUNPOD_POD_ID set) "https://{pod_id}-{RUNPOD_POD_PORT}.proxy.runpod.net"
             | (discovery, no pod yet) ""        # honest empty until first discovery
  serverless: RUNPOD_SERVERLESS_URL
warmup_url   = upstream_url + "/" + WARMUP_PATH (with WARMUP_PATH's leading "/" stripped)
discovery_enabled = (mode == pod) AND (RUNPOD_POD_ID == "") AND (default_model != "")
default_model:
  catalogue present:  RUNPOD_MODEL_NAME if in catalogue, else first catalogue name
  else:               RUNPOD_MODEL_NAME or first RUNPOD_ALLOWED_MODELS entry or ""
effective_allowed_models:
  catalogue present:  catalogue model names (in file order)
  else:               RUNPOD_ALLOWED_MODELS, or (RUNPOD_MODEL_NAME,) if set, else ()
allowlist_configured  = catalogue non-empty OR RUNPOD_ALLOWED_MODELS non-empty
upstream_auth_headers = {"authorization": "Bearer " + key} where
  key = RUNPOD_UPSTREAM_API_KEY or (RUNPOD_API_KEY if serverless mode else "")
  (empty key → no header)
auth_headers_for_pod(pod_id):
  if RUNPOD_UPSTREAM_API_KEY_TEMPLATE and pod_id: Bearer template.format(pod_id)
  else: upstream_auth_headers
```

**Startup validation (boot failure if violated):**
- `RUNPOD_MODE` ∈ {`serverless`, `pod`}.
- Catalogue loads and validates (§7.2); if `RUNPOD_MODEL_NAME` is set it must match a
  catalogue model by slug.
- Serverless mode requires `RUNPOD_SERVERLESS_URL`.
- Pod mode requires `RUNPOD_API_KEY`; if `RUNPOD_POD_ID` is empty it requires a
  non-empty `default_model`.

---

## 6. Components

The system is composed of these components (names are normative for behavior; a port
may restructure freely as long as the interactions below hold):

| Component | Responsibility | Key state it owns |
|---|---|---|
| **Config** | Env parsing, derived values, boot validation (§5). | Immutable after startup. |
| **State** | State machine + all counters/timestamps (§4). | `state`, `last_*_at`, `consecutive_keepalive_failures`, counters. |
| **UpstreamTarget** | The *mutable* upstream (url + pod_id) adopted by discovery/switches. | `(url, pod_id)`. |
| **Lifecycle** | What "bring backend up" / "put backend down" mean, per mode (§6.6–6.8). | Pod mode: `_created_pod_ids[model]`, `_pending_stops`, template-id cache. |
| **WarmupManager** | Single-flight warmup driving state to WARM (§6.1). | One in-flight future + a lock. |
| **KeepaliveLoop** | Background probe task + idle give-up (§6.2). | Task handle. |
| **ModelRouter** | Per-request model resolution, switch serialization, request leases (§6.3). | `active_model`, in-flight request counter + idle event. |
| **PrewarmScheduler** | Scheduled warmups at local times (§6.4). | Fired (day, slot) set. |
| **Proxy (app)** | HTTP surface, auth middleware, request pipeline (§8), shutdown. | Owns all of the above. |
| **RunPod API client** | Async REST client for pod/template management (§9). | Stateless (shares one HTTP client). |

A single shared async HTTP client is used by everything (probes, forwarding, REST
calls). Time source: monotonic clock for budgets, wall clock for timestamps/prewarm.

### 6.1 WarmupManager — single-flight warmup (F2, F3)

API: `ensure_warm(force = false) -> None` (raises `WarmupTimeout` on failure).

Semantics (normative):

```
ensure_warm(force):
  if state == WARM and not force: return
  under lock:
    if state == WARM and not force: return          # double-check: someone else warmed it
    if in_flight is None or in_flight.done():
      in_flight = new Future
      spawn task warm_up(in_flight)
  await in_flight                                  # every caller shares the future
```

`warm_up(future)` — drives the attempt and **must settle the future exactly once on
every path** (an unsettled future would hang this and every later request forever):

```
state = WARMING
deadline = now + WARMUP_TIMEOUT_S
backoff = 1.0
backend_started = false
loop:
  remaining = deadline - now
  if remaining <= 0:
    state = COLD; future.set_error(WarmupTimeout); return
  try:
    if not backend_started:
      await lifecycle.start() with timeout(remaining)   # must succeed once per warmup
      backend_started = true
    resp = GET target.warmup_url(WARMUP_PATH)
           headers = auth_headers_for_pod(target.pod_id)
           with timeout(remaining)
    if mode == pod and resp.status in {404, 502, 503, 504}:
      raise "not ready"        # RunPod edge proxy's synthetic responses (§6.5);
                               # never ready in any mode
    ready, reason = classify(resp, POD_HEALTH_MODE, model)    # §6.8a
    if not ready:
      raise "not ready"        # e.g. "model X not yet in warmup response"
    # classified ready (serverless: ANY other HTTP response — 2xx/3xx/4xx/5xx —
    # means a worker is answering; pod mode: per POD_HEALTH_MODE, default model):
    state = WARM
    last_warmup_at = now; consecutive_keepalive_failures = 0; warmups += 1
    future.set_result(); return
  catch (lifecycle error, HTTP/transport error, timeout):
    sleep(min(backoff, max(remaining, 0))); backoff = min(backoff*2, WARMUP_BACKOFF_MAX_S)
```

Additional requirements:
- Unexpected internal errors (bugs) must also settle the future (with an error),
  return state to COLD, and log. If the task is cancelled, re-raise after settling.
- A defensive `finally` may re-settle if somehow not done (no path may leave waiters
  hanging).
- `GET` is the probe method deliberately: on the real serverless gateway a GET
  readiness route is routed to ready workers, while POST probes can sit in the LB
  queue and hang.

### 6.2 KeepaliveLoop (F4, F5, F6)

A background task running forever at `KEEPALIVE_INTERVAL_S` cadence. Each tick:

```
tick():
  lifecycle.retry_pending_stops()                      # see §11 cost safety
  if state not in (WARM, DEGRADED): return             # COLD = let it idle; no pinging
  if last_real_traffic_at is None: return              # never had traffic: nothing to keep alive
  idle_for = now - last_real_traffic_at
  if idle_for > IDLE_GIVEUP_S:
    try: await lifecycle.stop()                        # pod mode: REST stop; serverless: no-op
    catch LifecycleError:
      warn "stop failed; retry next tick"; return      # stay in current state (cost safety)
    state = COLD; return
  timeout = clamp(KEEPALIVE_INTERVAL_S, min=1, max=30)
  try:
    resp = GET target.warmup_url(WARMUP_PATH)
         headers = auth_headers_for_pod(target.pod_id), timeout
    if mode == pod and resp.status in {404,502,503,504}: raise   # §6.5
    if state == DEGRADED: state = WARM                   # one good probe heals
    consecutive_keepalive_failures = 0; last_keepalive_at = now
  catch (HTTP error, timeout, OS error):
    consecutive_keepalive_failures += 1
    keepalive_failures_total += 1
    if consecutive_keepalive_failures >= 3: state = DEGRADED
```

Start/stop: `start()` is idempotent (restart if task absent/done); `stop()` sets a stop
event, cancels the task, and awaits it.

### 6.3 ModelRouter (F10, F11)

Responsibilities: resolve the requested model against the allowlist; serialize model
switches; hold a *lease* per request so a switch can drain in-flight traffic.

**Resolution** `resolve(requested) -> (model | None, rejected | None)`:
- `model = requested or default_model`.
- If no allowlist configured: return `(default_model, None)` — the requested field is
  ignored entirely (backward compatibility: single-model deployments unchanged).
- Otherwise match each allowlist entry by **case-insensitive exact** or by
  **slug equality** (§7.1). Return `(canonical_allowlist_entry, None)` on a match —
  the *canonical spelling* is used downstream, not the client's spelling.
- No match: return `(None, requested)` — the caller rejects with 400 (§8.4).

**Lease + switch protocol** (context manager `request(model)` around each proxied
request, and around `/_warm`):

```
acquire:
  under switch_lock:
    if lifecycle is DiscoveryPodLifecycle and model != active_model:
      switch(model)                    # §below; includes a fresh warmup
    acquire_lease()                    # in_flight += 1; clear idle event if now 1
    # the lease is taken under the lock so no request starts against a pod
    # that is about to be swapped
release:
  in_flight -= 1; set idle event if now 0     # lock-free: a draining switch must
  # observe the last completion promptly
```

**Switch** `switch(model)`:
```
if in_flight > 0:
  wait for idle event with timeout MODEL_SWITCH_DRAIN_S   # best-effort; expiry proceeds
lifecycle.stop()            # failures are swallowed here: the discovery lifecycle
                            # records them in _pending_stops for keepalive retries
target.set(upstream_url, "")   # forget the old pod
state = COLD
lifecycle.active_model = model
active_model = model
model_switches += 1
ensure_warm()               # full discovery+warmup for the new model; may raise WarmupTimeout
```

Notes:
- The in-flight counter is mutated without the lock (single-threaded event loop; no
  await between read and write); only the *ordering after a switch* needs the lock.
- `/_warm` and the prewarm scheduler warm the **active** model only — never the
  default — so an explicit prewarm cannot yank the pod away from the model traffic
  is on.

### 6.4 PrewarmScheduler (F13)

- Parses `PREWARM_TIMES` ("HH:MM,HH:MM"; invalid parts dropped with a warning).
- Ticks every 20 s. A slot is *due* when the current local time is within
  `max(120 s, 2 × tick_s)` **after** the slot (modulo 86400 for midnight wrap);
  a missed tick therefore fires late by at most that window.
- Fires at most once per (calendar day, slot). A process restart may re-fire a
  still-in-window slot — harmless (warm stays warm). The fired set is pruned to the
  current day.
- On fire: if already WARM, log and do nothing; else `ensure_warm()` on the active
  model. A failed slot is logged and dropped; retried the next day.
- The scheduler never triggers a model switch.
- `now_fn` is injectable (test seam): the implementation must be testable with a
  fake clock.

### 6.5 Pod "not ready" edge statuses (F8) — critical pod-mode detail

RunPod's edge/ingress proxy (`https://<pod>-<port>.proxy.runpod.net`) answers on the
pod's behalf **before the container inside is ready**: `404` when nothing is listening
on the port yet, and `502/503/504` once the port is bound but the app hasn't finished
starting. These are synthetic "not ready" responses, **not** answers from the model
server.

Normative rule: in pod mode, probe status codes `{404, 502, 503, 504}` are treated as
transport failures everywhere a probe result is interpreted (warmup, keepalive,
discovery health check). Treating them as "healthy" would declare the pod ready while
it is still loading weights, and the first real request would then get a confusing
404/502 from the same edge layer instead of a clean warmup wait. In **serverless**
mode any HTTP response counts as "up" (the gateway only answers when a worker is
serving).

### 6.6 Lifecycle: serverless (F2)

`start()` and `stop()` are no-ops. Serverless endpoints scale on demand; the proxy's
probe *is* the scale signal (a probe that lands in the queue gets served once a worker
comes up, which is exactly what warmup waits for).

### 6.7 Lifecycle: pinned pod (F7)

Stateless apart from config. REST base `RUNPOD_REST_URL`, auth
`Authorization: Bearer RUNPOD_API_KEY`.

```
desired_status():                     # None when undeterminable
  GET /pods/{pod_id}
  on HTTP 404: raise PodNotFoundError        # deleted id is never reused -> fail fast
  on transport/HTTP/JSON failure: return None
  return (body.desiredStatus or body.status).upper() or None

start(budget):
  if desired_status() == "RUNNING": return        # idempotency: RunPod rejects a
                                                  # start on a running pod, which
                                                  # would otherwise burn the whole
                                                  # warmup budget on a healthy pod
  try:
    POST /pods/{pod_id}/start
  except PodMigrationRequired:                    # host out of free GPUs: the
    # "please migrate your pod" prompt, or the REST 500
    # "not enough free GPUs on the host machine" — the
    # host assignment is sticky, so retries can never succeed
    if RUNPOD_ON_MIGRATE != "replace":
      raise LifecycleError(error with the RUNPOD_ON_MIGRATE=replace hint)
    _replace_pod(budget)
  (non-2xx -> LifecycleError carrying RunPod's error message from the body,
   which is also logged; transport error -> LifecycleError)
  (PodNotFoundError propagates -> warmup fails fast with an actionable 503)

_replace_pod(budget):
  old = GET /v1/pods/{pod_id}     # creation spec; the network volume arrives as
                                  # networkVolumeId (volumeId stays null in the GET
                                  # shape) and is re-attached so pod data survives
  DELETE /v1/pods/{old.id}        # RunPod's "terminate"
  fresh = POST /v2/pods           # v2 base RUNPOD_AVAILABILITY_URL (default
                                  # https://api.runpod.io/v2)
        (name, template or image, gpu = {id, count}, ports, env,
         container disk, cloud SECURE,
         mounts.network = [old's volume] when the old pod had one)
                                  # v2 create takes a SINGLE gpu id (old.gpu_type
                                  # else first RUNPOD_GPU_TYPE_IDS) — the old pod's
                                  # datacenter is NOT pinned; a re-attached volume
                                  # is datacenter-locked and pins placement by
                                  # itself, and a volume-less create is left
                                  # unpinned so RunPod can place it wherever the
                                  # GPU type has capacity
  on RunPod capacity 400: back off (2s, 4s, 8s, ... capped 30s) and retry
                          the create until the budget is exhausted
  adopt fresh.id + URL, pods_replaced += 1
                                # adoption happens BEFORE the RUNNING wait, so a
                                # budget timeout targets the replacement (an
                                # idempotent start on a booting pod), not the
                                # deleted original
  wait for GET /pods/{fresh.id} == RUNNING within budget
  (create failure -> LifecycleError; the old pod is already gone, so the
   operator recreates via the RunPod console)

stop():
  if desired_status() == "EXITED": return         # already stopped
  if PodNotFoundError: return                     # deleted pod: nothing to stop
  POST /pods/{pod_id}/stop    (non-2xx or transport error -> LifecycleError)
```

`retry_pending_stops()` is a no-op (pinned mode never records pending stops).

### 6.8 Lifecycle: discovery pod (F8, F9, F14)

Owns: `_created_pod_ids: {model -> pod_id}` (per-model memory of pods **this process
created**; survives reclamation — a retry resumes the same pod, never creates a
second), `_pending_stops: {pod_id}` (failed stops to retry), `active_model`,
`last_error`, and a lazily-built template-name→id cache.

**Pod URL derivation:** `https://{pod.id}-{port}.proxy.runpod.net` where port is the
catalogue per-model port (else `RUNPOD_POD_PORT`); if the pod's exposed HTTP ports
(`ports` entries of form `NNN/http`) include it use that, else the pod's first HTTP
port, else the configured port.

**`start()` — discovery algorithm (in priority order):**

1. **Previously-created pod first.** If `_created_pod_ids[active_model]` exists:
   - `GET /pods/{id}`.
   - If the pod is gone (404) or `TERMINATED`: forget the id (so stops/retries can't
     target a dead pod) and fall through to full discovery (creation allowed again).
   - If `RUNNING`: health-probe (§6.8a). Healthy → adopt (set target, `discoveries +=
     1`). Unhealthy → reclaim (§11) then fail with `LifecycleError`.
   - Else: resume-and-probe (§6.8b); if it fails, fail with `LifecycleError`.
   - **No fall-through to other pods** in this branch: a created pod is the sole
     candidate for the life of the process (deliberate cost-safety trade: retry/resume
     the same pod rather than provision another).
2. **Matching RUNNING pods.** `GET /pods?desiredStatus=RUNNING`; for each pod with a
   match reason (§6.8c), health-probe; first healthy one is adopted.
3. **Matching EXITED pods.** `GET /pods?desiredStatus=EXITED`; for each matching pod,
   resume-and-probe; first success is adopted.
4. **Creation (only if `RUNPOD_ALLOW_POD_CREATE`)**, else fail with a message telling
   the operator to enable it:
   - `GET /templates`; select candidate templates (§6.8d); walk the creation matrix
     (§6.8e). A successful create is recorded in `_created_pod_ids` **immediately**
     (the pod now exists and bills), `pod_creates += 1`, then wait for RUNNING
     (`POD_READY_TIMEOUT_S`) and health-probe (`POD_HEALTH_TIMEOUT_S`).
   - If it never becomes healthy: reclaim (§11) and fail.
5. If nothing worked: `LifecycleError` with the last error (or "no matching pod").

A `list_pods`/`get_pod` failure on the *lookup* paths degrades to an empty list /
failure rather than crashing the loop (the last error is recorded).

**`start()` must also survive cancellation by the warmup timeout** (it runs under
`asyncio.wait_for`): any pod it started/created but that did not end up healthy must
still be stopped — the cleanup must be shielded from the cancelling cancellation.

**`stop()`:**
```
pod_id = target.pod_id or _created_pod_ids.get(active_model)
if not pod_id: return
try:    POST /pods/{pod_id}/stop
catch:  _pending_stops.add(pod_id); raise LifecycleError
        _pending_stops.discard(pod_id)
retry_pending_stops(): for each pending pod id, retry stop; drop on success.
```

**6.8a — Health probe of a pod:** loop `GET {pod_url}/{WARMUP_PATH}` (with per-pod
auth headers) until the deadline `POD_HEALTH_TIMEOUT_S`, classifying each response by
`POD_HEALTH_MODE` (default `model`):
- `any` — success = any response *other than* the edge "not ready" set §6.5 (legacy;
  only safe for servers that refuse to answer before the model is loaded, e.g. vLLM's
  `/v1/models`).
- `model` — success = a `2xx` whose JSON `data[]` lists the target model
  (case-insensitive and slug-insensitive; with no known model, any non-empty list).
- `completion` — the GET must first clear the §6.5 edge set, then a real
  `POST {prefix}/chat/completions` (`max_tokens=1`, the resolved model) must return a
  `2xx` with `choices[]`. This is the only bar that proves inference works, at the cost
  of one 1-token inference per cold start.
On a not-ready classification, back off 1 s → ×2 (cap `WARMUP_BACKOFF_MAX_S`), record
`last_error` (e.g. "HTTP <code> (not ready)", "model <id> not yet in warmup response"),
and — so a multi-minute model load is observable without reading the pod's own logs —
log `warmup: not ready yet (<reason>) — <elapsed>s elapsed, <remaining>s budget left`.

**6.8b — Resume-and-probe:** `POST /pods/{id}/start` (`pod_starts += 1` on success),
then wait for `desiredStatus == RUNNING` for up to `POD_READY_TIMEOUT_S` (poll every
1 s; `get_pod` failures recorded), then health-probe. Success → adopt. **In all
failure paths, if the start had succeeded, reclaim** (even under cancellation).

**6.8c — Match rules** (a pod is a candidate if **either**):
- (a) **precise**: `pod.templateId` ∈ the active model's resolved catalogue template
  ids (catalogue template *names* resolved to ids once via `GET /templates`, cached;
  on a miss the map is refetched once; on API failure → empty set). This call happens
  **only when a catalogue spec exists** for the active model — non-catalogue
  deployments must not gain an extra HTTP request.
- (b) **legacy heuristic**: slug substring match (§7.1) of the active model against
  the pod's name, image, and every env *value*.

**6.8d — Template selection for creation (ordered, first match wins):**
1. `RUNPOD_TEMPLATE_NAME` exact (case-insensitive) among non-serverless templates —
   alone, even if the catalogue lists others.
2. Else if the catalogue has a spec: the spec's template names in declared order,
   resolved case-insensitively; unknown names are logged (available templates listed)
   and skipped.
3. Else legacy: first non-serverless template whose name/image/env slug-matches the
   model.

**6.8e — Creation matrix walk (cheapest viable first):**
```
for template in candidates (in order):
  combos = (catalogue spec.gpus) ? for each gpu: (gpu.id, count for count in min..max)
                                 : [(None, 1)]
  for (gpu_id, count) in combos:
    if attempts >= RUNPOD_MAX_CREATE_ATTEMPTS: abort with cap error
    attempts += 1
    pod = POST /v2/pods         # v2 base, see §9
                      name = model_slug(active_model)
                      templateId = template.id
                      gpu = { id: gpu_id or first RUNPOD_GPU_TYPE_IDS,
                              count: count }
                      cloud (per-model override)
                      dataCenterIds (per-model list, when set)
                      containerDiskGb / volume (per-model override, when set)
    on RunpodApiError (incl. RunPod capacity 400): record last_error;
       continue to next combo (a failed create provisioned nothing → no cleanup)
    on success: record _created_pod_ids[model]; pod_creates += 1; STOP the matrix
       (a pod exists and bills — never attempt another create)
```

### 6.9 Idle give-up, revalidation and the "one active model" rule (F5, F11, F14)

- Keepalive's idle give-up (§6.2) calls `lifecycle.stop()`: discovery mode stops the
  adopted pod (or the created-pod id if the target was cleared); pinned mode stops the
  pinned pod; serverless does nothing. Only after a **successful** stop does state go
  COLD; a failed stop stays put and is retried next tick (§6.2) — a silently
  still-billing pod is worse than one more ping.
- **Revalidation (F14):** in discovery mode, a request arriving while WARM but whose
  `last_success_at` (last successful *forwarded* response) is older than
  `POD_REVALIDATE_S` triggers `ensure_warm(force=true)` before forwarding — catching
  pods stopped/deleted/reclaimed out-of-band without waiting for 3 failed keepalives.
  (A running pod left by another user is never stopped by the proxy, §11.)
- **One active model:** the proxy adopts exactly one upstream at a time; switching
  replaces it. Interleaved traffic across models is out of contract (§1.4).

## 7. Model naming, slugs, and the model catalogue

### 7.1 Slug matching (legacy heuristic)

`model_slug(s) = re.sub(r"[^a-z0-9]+", "-", s.casefold()).strip("-")`
(i.e. casefold, collapse every run of non-alphanumerics to a single `-`, trim
leading/trailing `-`).

`matches_model(name, image, env, model)`: let `wanted = model_slug(model)`; empty
`wanted` → false. True iff `wanted` is a **substring** of `model_slug(name)`, or of
`model_slug(image)`, or of `model_slug(v)` for any string env value. This is why
`Qwen/Qwen3-32B` matches a pod named `qwen-qwen3-32b`, an image tag containing the
slug, or a template with `MODEL_NAME=Qwen/Qwen3-32B` (RunPod pod names disallow `/`).
Allowlist resolution (§6.3) uses slug **equality** instead of substring — stricter,
so two distinct models cannot alias each other.

Known limits of the legacy heuristic (motivating the catalogue): false negatives
(`qwen3-8-27b-fp8` template vs model `Qwen/Qwen3.8-27B-FP8` — slug `qwen-qwen3-8-27b-fp8`
does not contain `qwen3-8-27b-fp8` as a substring of its *template* slug — note the
direction: matching checks `wanted in model_slug(name)`) and false positives
(permissive substrings can route to a different model's pod sharing a slug fragment).

### 7.2 Model catalogue (F12)

**Sources:** `RUNPOD_MODELS_FILE` (path; wins) or `RUNPOD_MODELS_JSON` (inline
JSON string); both unset → empty catalogue (no-op). File format is dispatched on
extension: `.json` → strict JSON, `.yaml`/`.yml` → YAML via `yaml.safe_load`
(never the unrestricted loader — the file can't execute code at boot), any other
extension → JSON first, then YAML (YAML is a superset of JSON). Unreadable file,
invalid JSON/YAML, or schema violation → `ModelConfigError` → **boot failure**
(fail fast, no silent fallback).

**Top level:** either `{"models": [ ... ]}` or a bare array `[ ... ]`.

**Per-model object** (unknown keys rejected):

| Field | Required | Type | Validation |
|---|---|---|---|
| `name` | yes | string | non-empty; **unique by slug** across the catalogue (duplicate slug → error naming both). |
| `templates` | yes | string[] | non-empty; every entry a non-empty string. Template *names* (not ids), in preference order. |
| `gpus` | no | array | default `[]`. Each entry an object with exactly keys from `{id, min, max}`; `id` non-empty string, **unique** (case-insensitive) within the model; `min` int ≥ 1 (default 1); `max` int ≥ `min` (default `min`). The creation matrix walks `min..max` inclusive, ascending. |
| `port` | no | int | ≥ 1. Per-model override of the pod HTTP port. |
| `container_disk_gb` | no | int | ≥ 1. Per-model override for creation. |
| `volume_gb` | no | int | ≥ 1. Per-model override for creation. |
| `cloud_type` | no | string | non-empty. Per-model override for creation. |
| `datacenters` | no | string[] | default `[]`. Ordered list of RunPod datacentre ids (e.g. `US-TX-3`); every entry a non-empty string, **unique** (exact match) within the model. |
| `datacenter_priority` | no | string | `availability` (default) or `custom`. Accepted for compatibility only — the v2 create API has no priority field, so this value is never sent to RunPod (see §Datacentre pinning). |

**Semantics:**
- `get(name)` resolves by slug equality (case/insensitive, punctuation-insensitive);
  the **exact spelling declared in the catalogue** is canonical downstream.
- Catalogue presence ⇒ it is the allowlist (`effective_allowed_models` = catalogue
  names) and `allowlist_configured = true`.
- `RUNPOD_MODEL_NAME`, when set, must exist in the catalogue (else boot error).
- Catalogue fields fall back to the corresponding global env vars when omitted
  (§5.2: `port`→`RUNPOD_POD_PORT`, `container_disk_gb`→`RUNPOD_CONTAINER_DISK_GB`,
  `volume_gb`→`RUNPOD_VOLUME_GB`, `cloud_type`→`RUNPOD_CLOUD_TYPE`).
- Validation must reject: wrong top-level shape, non-object model entries, missing/
  empty `name`/`templates`, non-string template entries, non-list `gpus`, non-object
  gpu entries, unknown keys at model or gpu level, non-integer or out-of-range
  `min`/`max`/`port`/disk/volume, duplicate model slugs, duplicate gpu ids,
  `max < min`, non-list `datacenters`, empty/non-string datacentre entries,
  duplicate datacentre ids, and an unknown `datacenter_priority`. Errors must
  identify the model (index + name) and the offending key — operators edit
  these files by hand.
- **Datacentre pinning:** when a model declares `datacenters`, every create-pod
  attempt for it sends `dataCenterIds` (the list, in declared order). The v2
  create API has **no priority field**: `datacenter_priority` is accepted for
  compatibility, but the declared order is informational only — RunPod may place
  the pod in any listed DC that has capacity. Omitted/empty `datacenters` sends
  no `dataCenterIds`, so RunPod's default placement applies.
  A pod replaced after a migration prompt is recreated by re-attaching the old
  pod's network volume; v2 volumes are datacenter-locked, which pins placement
  to the volume's datacentre without any explicit pin.

**Example catalogue** (`models.example.yaml`; JSON twin `models.example.json`):
```yaml
models:
  - name: Qwen/Qwen3-32B
    templates: [qwen3-32b-vllm-h100, qwen3-32b-vllm-a100]
    gpus:
      - { id: "NVIDIA H100 80GB HBM3", min: 1, max: 1 }
      - { id: "NVIDIA A100 80GB PCIe", min: 1, max: 2 }
    port: 8000
    container_disk_gb: 50
    volume_gb: 100
    cloud_type: SECURE
  - name: Qwen/Qwen3.8-27B-FP8
    templates: [qwen3-8-27b-fp8-vllm]
    gpus: [{ id: "NVIDIA L40S" }]
```

```json
{
  "models": [
    {
      "name": "Qwen/Qwen3-32B",
      "templates": ["qwen3-32b-vllm-h100", "qwen3-32b-vllm-a100"],
      "gpus": [
        { "id": "NVIDIA H100 80GB HBM3", "min": 1, "max": 1 },
        { "id": "NVIDIA A100 80GB PCIe", "min": 1, "max": 2 }
      ],
      "port": 8000, "container_disk_gb": 50, "volume_gb": 100, "cloud_type": "SECURE"
    },
    { "name": "Qwen/Qwen3.8-27B-FP8",
      "templates": ["qwen3-8-27b-fp8-vllm"],
      "gpus": [{ "id": "NVIDIA L40S" }] }
  ]
}
```

**Deployment note:** the catalogue is deliberately *not* baked into the container
image; the compose file mounts `${MODELS_FILE:-./models.yaml}` at
`${CONTAINER_MODELS_FILE:-/app/models.yaml}` read-only so edits apply on restart
without a rebuild. A mounted-but-missing host file becomes a directory and the
catalogue load fails loudly at startup (by design, fail fast).

---

## 8. HTTP surface and request pipeline

### 8.1 Routes

| Route | Auth | Behavior |
|---|---|---|
| `GET /_health` | public | `200 {"ok": true}` — liveness only (no state read, no config); safe to expose to container healthchecks without the proxy key. |
| `GET /_status` | gated | JSON: `state`, `endpoint` (current target URL), `last_warmup_at`, `last_keepalive_at`, `last_real_traffic_at`, `consecutive_keepalive_failures`, `uptime_s`, `pod_starts`, `pod_creates`, `discoveries`, `model_switches`, `mode`, `active_model`; pod mode adds `pod_id` (and `model` = active model when discovery is enabled); discovery adds `last_discovery_error` (or null); pod mode with the GPU availability poller adds `gpu_availability` (`updated_at`, `age_s`, `last_error`, last-known `availability` + per-datacentre availability per tracked GPU type, and a `models` map; see §10 "GPU availability polling"). |
| `GET /metrics` | gated | Prometheus text format (`text/plain; version=0.4`). Gauge `runpod_proxy_state{state="COLD|WARMING|WARM|DEGRADED"}` (1 for the current state, 0 for others); counters `runpod_proxy_{warmups,requests_total,requests_warm_hit,requests_cold_hit,requests_failed,keepalive_failures_total,pod_starts,pod_creates,discoveries,model_switches}`; gauge `runpod_proxy_uptime_s`; gauge `runpod_proxy_active_model{model="<escaped>"}` (label value escaped: `\` → `\\`, `"` → `\"`, newline → `\n`); counter `runpod_proxy_requests_by_model{model="<escaped>"}` (forwarded requests per resolved model — a distinct name from the unlabelled `requests_total` because Prometheus labels are per-family); histogram `runpod_proxy_request_duration_seconds` (buckets `0.05 … 120` plus `+Inf`, plus `_sum`/`_count`) measuring seconds from receiving the client request to the upstream's response headers, forwarded requests only; pod mode with the GPU availability poller adds gauge `runpod_proxy_gpu_availability_age_s` (seconds since the last successful refresh — grows while polls fail; absent until the first success) and gauge `runpod_proxy_gpu_availability_level{gpu="<escaped>"}` (last-known availability: HIGH=3, MEDIUM=2, LOW=1, NONE=0, unknown spelling=-1; no series until the first successful refresh). |
| `POST /_warm` | gated | Warms the **active** model via the router (lease + `ensure_warm`). On `WarmupTimeout` → `503 {"error":"endpoint warmup timeout","state":...}`. On success sets `last_real_traffic_at = now` (anchors the idle window: the endpoint stays warm for `IDLE_GIVEUP_S` so the user can start working immediately) and returns `{"state": ...}`. |
| `POST /_reload` | gated | Re-reads the catalogue source recorded at boot (`RUNPOD_MODELS_FILE`, else `RUNPOD_MODELS_JSON`) and re-validates it with the same rules as startup, plus: a reloaded catalogue with **no models** is rejected, and one that drops `RUNPOD_MODEL_NAME` is rejected (lists known names). On success the new catalogue is swapped onto the shared `Config` — visible to the router, discovery, and lifecycle on the very next request. The swap never restarts a pod, re-warms, or touches in-flight leases: the active model keeps running, dropped models become unroutable (`400`), and the default model becomes the new catalogue's first entry. On any rejection → `400 {"error": ...}` and the running catalogue is untouched. Response: `200 {"reloaded": true, "source": <path|"inline">, "models": [...]}`. With no source configured → `400 {"error": "no model catalogue source configured ..."}`. |
| `* /{path:path}` | gated | The proxy: GET/POST/PUT/DELETE/PATCH/HEAD/OPTIONS, any path. See §8.4 pipeline. |

### 8.2 Auth middleware (F16)

When `PROXY_API_KEY` is set, **every** route except `/_health` is gated. The presented
key is the `x-proxy-key` header, else the token after `Bearer ` in `Authorization`
(case-insensitive prefix, trimmed). Comparison is **constant-time** (timing-safe
compare). Missing/mismatch → `401 {"error":"unauthorized"}`. When no proxy key is
configured, all routes are open (documented risk: the proxy is an open relay for the
RunPod credit — it attaches your RunPod key to what it forwards).

### 8.3 Hop-by-hop header filtering

Request headers forwarded: all except hop-by-hop set
`{connection, keep-alive, proxy-authenticate, proxy-authorization, te, trailer,
transfer-encoding, upgrade, host, content-length}` (matched case-insensitively).
Response headers relayed: same filter. Additionally on requests:
`accept-encoding` is **forced to `identity`** so upstream responses are never
compressed (pass-through stays byte-exact, SSE included). `content-length` must not be
forwarded because the body may be rewritten (model canonicalization) — the underlying
HTTP client recomputes it.

### 8.4 Proxy pipeline (normative order)

```
1. state.last_real_traffic_at = now; state.requests_total += 1
   if state == WARM: state.requests_warm_hit += 1
2. Build forwarded headers (§8.3), then:
   - strip `x-proxy-key` (the proxy credential is never the upstream's)
   - if the client presented its proxy key via `Authorization`: strip `authorization`
     (else a client's proxy key would be leaked to the model server as its API key)
   - merge upstream auth headers: auth_headers_for_pod(target.pod_id)
     (serverless: upstream_auth_headers — key = UPSTREAM_API_KEY or RUNPOD_API_KEY)
3. Read the body **streaming with a limit** MAX_BODY_BYTES:
   - over limit -> 413 {"error":"request body too large","max_bytes":N}; requests_failed += 1
     (the stream is read until it exceeds the limit, then rejected — never buffered whole)
4. Model extraction (F10): only if body non-empty AND Content-Type contains "json"
   (case-insensitive) AND len(body) <= 2 MiB:
   - parse JSON; if top-level object with string "model": requested = that value
   - unparseable / not an object / missing or non-string model -> requested = None
     (a request must never fail for omitting a model)
5. (model, rejected) = router.resolve(requested)
   rejected != None -> 400 {"error":"model not allowed","model":<requested>,
                            "allowed":[<allowlist>]} ; requests_failed += 1
6. Model canonicalization: if allowlist_configured AND requested != None AND
   model != requested: set parsed["model"] = model and re-serialize the body
   (compact JSON, non-ASCII preserved). Rationale: the pod's model server (vLLM)
   only knows the canonical name; a client's case/slug variant must not reach it.
7. Acquire the router lease for `model` (may trigger a switch + its warmup):
   WarmupTimeout -> 503 warmup-timeout JSON; requests_failed += 1; release lease
8. Before forwarding:
   - if discovery_enabled AND was_warm AND last_success_at set AND
     now - last_success_at > POD_REVALIDATE_S:  ensure_warm(force=true)
   - if state != WARM:  ensure_warm()          # join the in-flight attempt
   WarmupTimeout in either -> 503; requests_failed += 1; release lease
9. Forward: build upstream request to "{target.url}/{path}" preserving the original
   method and full query string (multi-values), headers from step 2, body from step 6,
   timeout = REQUEST_TIMEOUT_S for read/write/pool, connect = 10 s fixed.
   send(stream=True):
   - transport failure -> 502 {"error":"upstream connection error","detail":<type>};
     requests_failed += 1; release lease
10. state.last_success_at = now   (set on *response received*, before streaming)
11. Relay: StreamingResponse with the upstream status and filtered headers;
    stream chunks as they arrive (no buffering); on stream end (or client
    disconnect), close the upstream and release the lease in a finally.
```

Edge behaviors to preserve:
- `was_warm` is captured **before** `ensure_warm` (it classifies warm vs cold hits).
- The lease is released exactly once on every path (success, 502, 503, stream end,
  client abort) — the release must happen in the streaming `finally`, because that
  is when the in-flight request actually finishes.
- Streaming responses (SSE `text/event-stream`, `chunked` bodies) must work with no
  intermediate buffering and no added `Content-Length`.
- The proxy adds no response headers of its own (beyond what the framework requires
  for streaming) — clients must see the upstream response as if direct.
- `HEAD`/`OPTIONS`/etc. are forwarded like any method; the pipeline (warmup, lease,
  model extraction) applies uniformly.

### 8.5 Why bodies are only *sometimes* parsed (N2)

Body inspection is bounded by design: only JSON, only ≤ 2 MiB (the model field is
tiny; anything larger is passed through unparsed and unrouted — falls back to the
default model). The separate 50 MiB hard cap (`MAX_BODY_BYTES`) protects the proxy's
memory from unbounded uploads; it is enforced by *streaming* the body and aborting
once the limit is crossed.

---

## 9. RunPod REST API client (pod mode only)

Base URL `RUNPOD_REST_URL` (default `https://rest.runpod.io/v1`); every call sends
`Authorization: Bearer RUNPOD_API_KEY`. Errors: transport failure or non-2xx →
`RunpodApiError` (non-2xx messages include the API's `message` field when the body is
a JSON object with one). A `404` on `GET /pods/{id}` returns *None* (pod gone), not an
error — discovery relies on this to forget dead pods.

| Operation | Call | Notes |
|---|---|---|
| List pods | `GET /pods?desiredStatus=RUNNING\|EXITED` | response: JSON **array** of pod objects. |
| Get pod | `GET /pods/{id}` | 404 → None. |
| Start pod | `POST /pods/{id}/start` | 2xx only means success. |
| Stop pod | `POST /pods/{id}/stop` | 2xx only. |
| List templates | `GET /templates` | array of template objects; optional params `includePublicTemplates`, `includeRunpodTemplates` (not needed by default discovery). |
| Create pod | `POST /v2/pods` (v2 base `RUNPOD_AVAILABILITY_URL`) | body: `name` (the model **slug** — pod names disallow `/`), `templateId` **or** `image`, `gpu` = `{ id (required — v2 has no default), count }`, `cloud` (default `SECURE`), and optionally `dataCenterIds[]`, `ports[]`, `env{}`, `disk`, `mounts: { network: [{ volumeId, path }] }` (re-attach an existing volume). Response: v2 pod object. A `400` covers both rule violations and **no capacity**; the capacity wording raises `RunpodCapacityError`, which the replace flow retries with backoff (§6.7). |

Pod object fields used: `id`, `name`, `desiredStatus`, `image`, `templateId`, `env`
(map), `ports` (list of `"NNN/protocol"` strings; HTTP ports = entries whose protocol
is `http`, parsed to int). Template object fields used: `id`, `name`, `imageName`,
`env`, `ports`, `isServerless`.

Statuses: `RUNNING`, `EXITED`, `TERMINATED`. A pod the proxy **found** already
RUNNING is never stopped by the proxy (another user/process may be using it); only
pods the proxy started or created are candidates for stop/reclaim.

> **API version note:** RunPod deprecated REST v1 (retirement planned 2026-11-15).
> Pod **creation** already uses the v2 API (`POST /v2/pods` on the
> `RUNPOD_AVAILABILITY_URL` base); `RUNPOD_REST_URL` points the remaining
> read/start/stop/delete calls at a future base without code changes; the v1 call
> shapes above are the contract for those operations.

---

## 10. Observability and logging

- **Logs** (single logger, levels from `LOG_LEVEL`): warmup start/attempt-failure with
  backoff, warmup success (with upstream status), keepalive failures (consecutive
  count) and DEGRADED transitions, idle give-up, discovery decisions (how many pods
  listed, how many matched, *why* each candidate matched), non-matching pods at
  **DEBUG only** — and never with `env` contents (secrets) —, create attempts and
  their failures, reclaim events, switch/drain events, forwarded request lines
  (`forwarded METHOD /path -> NNN (NNNms)`).
- **Request correlation id**: every HTTP request carries an `X-Request-Id`. A
  well-formed client-supplied id (1–128 chars, trimmed) is honoured so callers
  can correlate their own traces with the proxy's; otherwise the proxy generates
  a `uuid4().hex`. The id is echoed in the response header and appears on every
  log record for that request (text format: `[<id>]` in the line; JSON format:
  `request_id` field). It is delivered via a pure-ASGI middleware that wraps the
  `send` callback — deliberately not `@app.middleware` (BaseHTTPMiddleware),
  which cannot add headers to a response an inner middleware returns without
  calling `call_next` (e.g. the 401 auth rejection). It is registered *after*
  the auth middleware, because in this Starlette version the last-registered
  user middleware is outermost.
- **Log format** (`LOG_FORMAT`): `text` (default) or `json`. JSON emits one
  object per line: `ts` (UTC ISO-8601), `level`, `logger`, `request_id`
  (`-` outside a request), `msg`, and `exc` (formatted traceback) on
  exceptions. The `request_id` is injected by a filter attached to the root
  *handler* (not a logger): records propagated from child loggers (e.g.
  httpx) skip ancestor loggers' filters, so a logger-level filter would leave
  the field unset and the `%(request_id)s` format would crash on them.
- **GPU availability polling** (pod mode; `proxy/gpu_availability.py`): every
  `GPU_AVAILABILITY_INTERVAL_S` (and once immediately at startup; `0`
  disables) the proxy polls
  `GET {RUNPOD_AVAILABILITY_URL}/catalog/gpus?include=AVAILABILITY&product=POD&cloud={RUNPOD_CLOUD_TYPE}`
  and keeps the last-known availability of the GPU types referenced by the
  catalogue (or `RUNPOD_GPU_TYPE_IDS` without a catalogue). A failing poll
  records the error and keeps the previous values — staleness is visible
  (`age_s`, `last_error`) rather than data going blank. Surface: the
  `gpu_availability` block of `/_status` and the
  `runpod_proxy_gpu_availability_{age_s,level}` gauges of `/metrics` (§8.1).
  The poller is informational and strictly non-fatal: every exception is
  swallowed and logged; it must never take the proxy down or disturb traffic.
- **`/_status`**: human-readable current state (§8.1).
- **`/metrics`**: machine-readable counters (§8.1). Counters are monotonically
  increasing per process (no resets); `uptime_s` is a gauge. The request
  histogram and per-model counters are exposed in §8.1's metric list; a
  ready-made Grafana dashboard is provided at `docs/grafana-dashboard.json`
  (panels for state, active model, request rates, warm/cold/failed split,
  p50/p95/p99 latency, requests by model, warmups/keepalive failures, and pod
  lifecycle).
- **Cost-safety log lines** (operators grep for these when billing looks off):
  "reclaiming unhealthy pod", "pod ... stop failed ... will retry next tick",
  "shutdown: pod-mode backend stopped", "shutdown: could not stop pod-mode backend".

---

## 11. Cost-safety invariants (N3) — the rules a port must not break

1. **At most one created pod per model per process.** The created id is recorded the
   moment `create_pod` returns (before any readiness wait) and the creation matrix
   stops immediately. A later discovery resumes that pod; it never provisions a
   second one for the same model.
2. **Every pod the proxy started or created must end stopped if it never becomes
   healthy.** Cleanup (reclaim) must run on all failure paths *and* survive
   cancellation by the warmup timeout (shielded cleanup), because a successful start
   without cleanup is a billing leak.
3. **Failed stops are never dropped.** A failed `stop` on idle give-up or model
   switch records the pod id in a pending set retried by the keepalive loop every
   tick; the state machine does not go COLD until the stop succeeds.
4. **Pods found already RUNNING (not started by this process) are never stopped** —
   by idle give-up, switches, or shutdown.
5. **Shutdown stops the backend** (best effort, logged on failure) so a proxy
   restart does not leave a discovered/pinned pod billing until the next session's
   idle give-up.
6. **A failed create provisions nothing** (API error) → the matrix continues; only a
   *successful* create is billable and must be tracked.
7. **Idle give-up in serverless mode** simply stops probing — no explicit stop exists
   (the platform scales down after its own idleTimeout), so there is nothing to leak;
   the invariant that matters there is that probing *actually stops* after
   `IDLE_GIVEUP_S`.

---

## 12. Deployment

### 12.1 Container (reference Dockerfile)

```
python:3.12-slim
  env: PYTHONDONTWRITEBYTECODE=1 PYTHONUNBUFFERED=1 PIP_NO_CACHE_DIR=1
  workdir /app
  COPY requirements.txt -> pip install
  COPY proxy/ -> /app/proxy
  run as unprivileged user (useradd appuser; chown /app; USER appuser)
  EXPOSE 8080
  HEALTHCHECK: GET http://127.0.0.1:8080/_health every 30 s (5 s timeout,
               5 s start period, 3 retries) — /_health must stay public
  CMD: uvicorn proxy.main:app --host 0.0.0.0 --port 8080
```

Runtime deps: an async HTTP server framework (FastAPI), an async HTTP client
(httpx), a server (uvicorn). Any equivalents work if §3 behaviors hold.

### 12.2 docker-compose (reference)

```yaml
services:
  runpod-proxy:
    build: .
    env_file: [ ${ENV_FILE:-.env} ]
    ports: [ "8080:8080" ]
    volumes: [ ${MODELS_FILE:-./models.yaml}:${CONTAINER_MODELS_FILE:-/app/models.yaml}:ro ]
    restart: unless-stopped
    stop_grace_period: 30s        # default 10 s cuts long streaming generations
                                  # mid-token on down/upgrade
```

- `MODELS_FILE` (host file to mount) and `CONTAINER_MODELS_FILE` (in-container
  target, default `/app/models.yaml`) are **compose interpolation** variables
  (picked by docker-compose) — they must live in the shell environment or the
  compose `.env`, **not** in `ENV_FILE`; `env_file` values are injected into the
  container only and are never used for interpolation. The *app* setting is
  `RUNPOD_MODELS_FILE=/app/models.yaml` (belongs in the env file).
- The mount is read-only; a missing host file makes Docker create a *directory* at
  the path → catalogue load fails loudly at boot (only when `RUNPOD_MODELS_FILE`
  points at the mounted path).
- Client side: point the agent's base URL at the proxy (e.g. Copilot CLI
  `COPILOT_PROVIDER_BASE_URL=http://localhost:8080`), and set the provider API key to
  the value the proxy expects — the proxy key (`PROXY_API_KEY`) when set, else
  `RUNPOD_API_KEY` (serverless mode, no proxy key set).

### 12.3 Required upstream settings (operator, not code)

- **RunPod serverless endpoint:** `idleTimeout` **must exceed** `KEEPALIVE_INTERVAL_S`
  (RunPod default is 5 s; with the proxy default 25 s the worker would be recycled
  between pings) — set the endpoint idleTimeout to 30–60 s, or lower the interval.
  `workersMin=0` for scale-to-zero (the whole point).
- **Pod mode:** the pod's model server must answer `GET /{WARMUP_PATH}` (any
  OpenAI-compatible worker does, e.g. `/v1/models`) and satisfy `POD_HEALTH_MODE`
  (default `model`: it must list the target model; `completion` additionally requires
  `POST /chat/completions` to work), and its port must match `RUNPOD_POD_PORT` (or the
  catalogue's per-model port) among the pod's exposed `http` ports.

### 12.4 Acceptance smoke test (deployment-level)

1. `docker compose up -d --build` → container healthy (`/_health` = 200).
2. `/_status` (with proxy key if set) shows `state=COLD`, correct `mode`/`endpoint`.
3. A real chat request cold-starts: first request takes up to `WARMUP_TIMEOUT_S` and
   succeeds; `/_status` shows `WARM`, `requests_warm_hit` increments on the next one.
4. Idle beyond `IDLE_GIVEUP_S` → `state=COLD` and (pod mode) the pod is EXITED.
5. In discovery mode with creation enabled: a request for a model with no existing
   pod creates at most one pod, adopts it, and stops it again on idle give-up.
6. Pinned pod mode with `RUNPOD_ON_MIGRATE=replace`: once the pod's original host
   is out of free GPUs, a request terminates the pod, recreates it from the same
   template (network volume re-attached), adopts the new id, and reaches `WARM`;
   `/_status` shows `pods_replaced` incremented.

---

## 13. Reference implementation map (Python)

| Spec section | File(s) in `runpod-proxy/` |
|---|---|
| §5 Config & env | `proxy/config.py` |
| §4 State machine | `proxy/state.py` |
| §6.1 Warmup | `proxy/warmup.py` |
| §6.2 Keepalive | `proxy/keepalive.py` |
| §6.3 Model router | `proxy/router.py` |
| §6.4 Prewarm | `proxy/prewarm.py` |
| §6.6–6.9 Lifecycle (serverless / pinned / discovery) | `proxy/lifecycle.py` |
| §7.1 Slugs, §9 REST client | `proxy/runpod_api.py` |
| §7.2 Catalogue | `proxy/models_config.py` |
| §8 HTTP surface & pipeline | `proxy/main.py` |
| §6 UpstreamTarget | `proxy/target.py` |
| §12 Packaging | `Dockerfile`, `docker-compose.yml`, `requirements.txt` |
| Catalogue example | `models.example.yaml`, `models.example.json` |
| Env reference | `.env.example` |
| Behavior tests (264) | `tests/test_*.py` — one module per area: auth, forwarding, keepalive, warmup, pod mode, pod-not-found fail-fast, pod replace on migration, discovery, model routing, models config, catalogue discovery, runpod_api, regressions, metrics, status, observability, gpu-availability, health modes |
| Local demo (no RunPod) | `demo/mock_upstream.py` + `demo/run-demo.ps1` |
| Client guide | `docs/copilot-cli-guide.md` |

The test suite is the executable spec: a faithful port should be able to run the
same scenarios against it (the HTTP client and RunPod API are the only two
seams; tests inject fakes for both).

---

## 14. Porting checklist

A port is complete when **all** of these hold (each was a bug class or requirement in
the reference implementation):

**Warmup/keepalive**
- [ ] First request while COLD waits (single-flight) and succeeds after backend up.
- [ ] N concurrent cold requests → exactly one warmup chain, all succeed together.
- [ ] Warmup timeout → 503, state back to COLD, all waiters woken.
- [ ] Any HTTP response (incl. 4xx/5xx) = warm **in serverless mode**; pod mode
      classifies by `POD_HEALTH_MODE` (default `model`: target model listed in the
      warmup-route JSON; edge set 404/502/503/504 never ready).
- [ ] 3 consecutive failed probes → DEGRADED; one good probe → WARM.
- [ ] Idle > IDLE_GIVEUP_S → COLD (and pod stopped in pod mode); failed stop →
      state retained, retried next tick.
- [ ] Keepalive probes only when WARM/DEGRADED and real traffic has occurred.

**Proxying**
- [ ] SSE token streams relay without buffering; status/headers preserved (hop-by-hop
      filtered); `accept-encoding: identity` forced upstream.
- [ ] Body > MAX_BODY_BYTES → 413 without buffering the whole body.
- [ ] `x-proxy-key` stripped from forwarded headers; client `Authorization` stripped
      when it carried the proxy key; upstream key injected per mode/pod.
- [ ] Upstream transport error → 502 JSON; warmup timeout → 503 JSON.

**Routing/catalogue**
- [ ] No allowlist → `model` field ignored entirely (backward compatible).
- [ ] Case/slug variants resolve to the canonical allowlist spelling; the forwarded
      body's `model` is rewritten to canonical.
- [ ] Disallowed model → 400 with `allowed` list, backend untouched.
- [ ] Missing/non-JSON/no-model body → default model, never a failure.
- [ ] Switch drains (≤ MODEL_SWITCH_DRAIN_S), stops old pod, warms new, serialized;
      in-flight lease released exactly once on every path.
- [ ] Catalogue: schema validated at boot (fail fast); exact template-id matching;
      creation matrix walks templates × gpus × min..max cheapest-first, stops on first
      success; per-model overrides fall back to env defaults.

**Lifecycle/cost**
- [ ] Pinned pod: start skipped when RUNNING; stop skipped when EXITED; stop on
      graceful shutdown (best effort, logged).
- [ ] Pinned pod deleted (`GET /pods/{id}` → 404): warmup fails fast with an
      actionable 503 (no `WARMUP_TIMEOUT_S` burn, no REST start attempted);
      `stop()` treats the pod as already stopped.
- [ ] Discovery: RUNNING-probe before EXITED-resume before create; `desiredStatus`
      alone is never health; created pod is sole candidate thereafter; deleted/
      terminated created pod is forgotten.
- [ ] Created-never-healthy pods are stopped on every failure path *including*
      warmup-timeout cancellation.
- [ ] Found-already-RUNNING pods are never stopped.
- [ ] Revalidation: warm endpoint idle beyond POD_REVALIDATE_S re-probes before
      forwarding.

**Surface/ops**
- [ ] `/_health` public; `/_status`, `/metrics`, `/_warm`, `/_reload` gated by PROXY_API_KEY
      (constant-time compare; `x-proxy-key` or `Bearer`).
- [ ] Prewarm fires once per slot per day, within the lateness window, active model
      only, survives a failed slot.
- [ ] Boot validation per §5.5 (missing required env / bad mode / bad catalogue →
      clear failure).

---

*End of specification.*
