# Using RunPod GPU Pods with GitHub Copilot CLI via the RunPod Proxy

This guide describes how to use GitHub Copilot CLI (and any other
OpenAI-compatible client) against your own RunPod GPU pods through the
`runpod-proxy` service in this repository. The proxy runs locally (or on your
tailnet/VPN) and presents a single, stable OpenAI-compatible endpoint; behind
the scenes it finds, starts, health-checks, or creates a pod that serves the
requested model.

```
  GitHub Copilot CLI
        |   COPILOT_PROVIDER_BASE_URL=http://localhost:8080/v1
        v
  +---------------------------------------------------------------+
  |  runpod-proxy (this repo)                                     |
  |                                                               |
  |  1. Read `model` from the request body                        |
  |  2. Find a RUNNING pod serving that model  --> proxy it       |
  |  3. Else resume an EXITED (stopped) pod    --> wait, then     |
  |     proxy it                                                  |
  |  4. Else create a pod from an allowed template (opt-in)       |
  |  5. Health-check the pod before serving (RUNNING != healthy)  |
  +---------------------------------------------------------------+
        |   https://<pod-id>-<port>.proxy.runpod.net/v1/...
        v
  RunPod pod (vLLM or any OpenAI-compatible server)
```

---

## 1. Prerequisites

| Requirement | Notes |
|---|---|
| RunPod account + API key | https://console.runpod.io/user/settings |
| Docker (or Python 3.12+) | The proxy runs as a ~50 MB container |
| GitHub Copilot CLI | With BYOK provider support (`COPILOT_PROVIDER_*` env vars) |
| A pod template per model | Runs an OpenAI-compatible server (e.g. vLLM) on a known port |

Your model **must support tool calling and streaming** for the agentic Copilot
CLI experience. vLLM serving an instruct model with `--enable-auto-tool-choice`
is the typical setup.

## 2. How requests flow

The sections below follow one request end to end. All of this is implemented
and tested in `proxy/` — this guide explains the *behavior contract* you rely
on as an operator.

### 2.1 First request: per-request model routing

When Copilot CLI sends a chat completion, the body contains a top-level
`"model"` field. The proxy inspects it (JSON bodies up to 2 MB only) and
resolves it against your allowlist:

- **Catalogue (recommended):** `RUNPOD_MODELS_FILE` / `RUNPOD_MODELS_JSON`
  define the allowed models, their templates, and GPU preferences.
  See [Declarative model catalogue](../README.md#declarative-model-catalogue).
- **Simple allowlist:** `RUNPOD_ALLOWED_MODELS=Qwen/Qwen3-32B,Qwen/Qwen3.8-27B-FP8`.

Matching is case- and slug-insensitive (`Qwen/Qwen3-32B` ≡ `qwen-qwen3-32b`).
A request with no usable `model` field falls back to the default model
(`RUNPOD_MODEL_NAME`, else the first allowlist/catalogue entry) — a request
never fails merely for omitting a model. A model outside the allowlist is
rejected with HTTP 400 *before* any pod is touched:

```json
{"error": "model not allowed", "model": "gpt-4o", "allowed": ["Qwen/Qwen3-32B"]}
```

### 2.2 Resolving a running pod — and the "which model is this pod?" problem

RunPod's pod metadata does **not** reliably record which model a pod serves.
The proxy therefore matches a requested model against each pod using two rules,
in order:

1. **Precise (catalogue):** the pod's `templateId` is one of the template IDs
   declared for the model in the model catalogue (`models.yaml` or
   `models.json`). This is exact and auditable.
2. **Heuristic (legacy):** the model's slug appears in the pod's **name**,
   **image**, or **any environment value** (e.g. `MODEL_NAME=Qwen/Qwen3-32B`).
   This exists for backward compatibility and can false-positive/negative —
   prefer the catalogue.

> **Operational guidance:** name your templates and pods after the model
> (`qwen3-32b-vllm-h100`) and/or set an env var like `MODEL_NAME=...` in the
> template. Then both rules agree.

A *matching* pod is not yet a *serving* pod — see 2.4.

### 2.3 Happy path: pod is running and healthy

Discovery lists `RUNNING` pods, filters by the match rules above, and
health-probes each candidate (`GET {pod_url}/{WARMUP_PATH}`, default
`/v1/models`). The **first pod that answers with any HTTP response** is
selected. From then on the proxy is a transparent streaming reverse proxy:

- any path/method/body is forwarded verbatim to the pod;
- SSE token streams pass through unbuffered (`accept-encoding: identity` is
  forced upstream);
- your Copilot CLI session behaves exactly as if the pod were local.

### 2.4 Cold start: no healthy running pod

If no running pod matches (or none is healthy), discovery tries **stopped
(`EXITED`) pods** one at a time: `POST /pods/{id}/start`, wait for
`desiredStatus=RUNNING` (`POD_READY_TIMEOUT_S`, default 120s), then probe HTTP
health (`POD_HEALTH_TIMEOUT_S`, default 180s). Real-world resume takes
~3 minutes including model load — comfortably inside the defaults.

**Keeping the client busy during the wait.** The proxy never replies early and
never idles the connection: the request that triggered the cold start is held
inside a *single-flight warmup* (`proxy/warmup.py`) until the pod answers, with
retries at 1s→15s backoff up to `WARMUP_TIMEOUT_S` (default **600s**).
Concurrent requests join the same warmup — no stampede. To the client this is
simply a slow first response; no bytes are required to flow to keep it alive,
but you should still:

- set the client-side timeout generously. For Copilot CLI there is nothing to
  tune — it tolerates long first-token latency — but for raw `openai` SDK
  scripts pass e.g. `timeout=httpx.Timeout(600, connect=10)`;
- optionally **pre-warm** before starting a session:
  `curl -X POST http://localhost:8080/_warm` blocks until the pod serves, so
  the first real request is instant.

A failed start (GPU capacity, changed pod config) does **not** fail the
request — discovery advances to the next stopped candidate. Only when all
candidates are exhausted does the request fail with `503 endpoint warmup
timeout`.

### 2.5 Verification: RUNNING ≠ serving

A pod can report `desiredStatus=RUNNING` while the vLLM process inside is
still loading the model — or has crash-looped. The proxy never trusts the
RunPod status alone:

- **Health = an HTTP response** from `GET {pod_url}/{WARMUP_PATH}`. Even a
  4xx/5xx counts (the server is answering); a connect error does not.
- A pod the proxy *started or created* that never becomes healthy is
  **stopped again** ("reclaimed") so it cannot bill silently — and the resume
  budget was not wasted on a second attempt later, because the created pod ID
  is remembered and resumed, not re-created.
- A pod found **already running** when the proxy started is *never* stopped by
  the proxy — you (or a teammate) may be using it.
- Long-lived sessions are guarded by **revalidation**: after
  `POD_REVALIDATE_S` (default 300s) since the last successful request, the
  next request re-probes before forwarding, catching pods that were stopped,
  reclaimed, or deleted outside the proxy.

### 2.6 Last resort: create a pod from an allowed template

If no stopped pod can be revived, creation is attempted — **only** when you
have opted in with `RUNPOD_ALLOW_POD_CREATE=true` (default off; it provisions
real, billable GPUs).

Creation walks your declared matrix **cheapest-viable first**:

```
for each template (catalogue preference order)
  for each GPU (catalogue preference order)
    for count = min .. max (ascending)
      try create_pod(template, gpu, count)
```

Template selection precedence: `RUNPOD_TEMPLATE_NAME` override → catalogue
`templates` list → legacy slug heuristic. Catalogue template names not present
in your RunPod account are logged (`WARNING`) and skipped. A failed create
(e.g. no capacity) advances to the next combination; `RUNPOD_MAX_CREATE_ATTEMPTS`
(default 12) hard-caps the walk.

Cost-safety invariants (enforced and mutation-tested):

- **At most one created pod per model per proxy process.** The first
  successful `create_pod` ends the matrix; its ID is remembered, and future
  discoveries *resume* it rather than creating a second pod.
- A created pod that fails readiness/health is reclaimed (stopped) and the
  process will not create a replacement.
- A created pod that is permanently broken becomes the *sole* candidate for
  that model for the life of the process (retry = resume → probe → reclaim).
  Restart the proxy to clear this deliberate pin.

### 2.7 Steady state: keepalive and idle give-up

While you work, the proxy pings the pod every `KEEPALIVE_INTERVAL_S` (default
25s) — but only if real traffic arrived within `IDLE_GIVEUP_S` (default 300s).
After 5 idle minutes it stops pinging, **stops the discovered pod**, and drops
to `COLD`, so GPU billing ends. (A stopped pod still bills for its
disk/volume — delete it if you're done for good.)

Three consecutive keepalive failures mark the pod `DEGRADED`; the next real
request re-runs discovery automatically.

### 2.8 Switching models mid-session

One pod, one model at a time. Asking for a *different* allowed model triggers
a serialized switch: drain in-flight requests (up to `MODEL_SWITCH_DRAIN_S`,
default 30s) → stop the current pod → run discovery/warmup for the new model.
Interleaving two models alternately will thrash (each alternation is a full
cold start) — prefer one model per session.

---

## 3. Setup

### 3.1 Model catalogue (`models.yaml`)

The catalogue is a JSON or YAML file (the extension picks the parser; YAML is
read with `safe_load`). Example:

```yaml
models:
  - name: Qwen/Qwen3.8-27B-FP8
    templates: [rainy_green_ferret]
    gpus: [{ id: "NVIDIA A40", min: 1, max: 1 }]
    port: 8000
```

`templates` are RunPod **template names** (not IDs), in preference order. The
template must start an OpenAI-compatible server on `port` (default 8000).
Validation is fail-fast at boot: unknown keys, duplicate models, empty
`templates`, bad GPU ranges all refuse to start the proxy.

### 3.2 Environment (`.env`)

```ini
RUNPOD_MODE=pod
RUNPOD_API_KEY=<your runpod api key>        # lifecycle calls only; never sent to the pod
RUNPOD_MODELS_FILE=/app/models.yaml         # catalogue mounted read-only
RUNPOD_ALLOW_POD_CREATE=true                # opt in to billable creation
WARMUP_TIMEOUT_S=600                        # ≥ worst-case pod resume + model load
POD_READY_TIMEOUT_S=120
POD_HEALTH_TIMEOUT_S=180
PROXY_API_KEY=<random shared secret>        # strongly recommended
```

Also useful: `RUNPOD_UPSTREAM_API_KEY` if your model server requires its own
key, `IDLE_GIVEUP_S`, `POD_REVALIDATE_S`. Full table: [Configuration](../README.md#configuration).

### 3.3 Run the proxy

```powershell
cd runpod-proxy
$env:MODELS_FILE = "./models.yaml"   # compose interpolation var (host path to mount)
docker compose up -d --build

curl -s http://localhost:8080/_status -H "x-proxy-key: $env:PROXY_API_KEY"
```

### 3.4 Point Copilot CLI at the proxy (BYOK)

```powershell
$env:COPILOT_PROVIDER_BASE_URL = "http://localhost:8080/v1"
$env:COPILOT_PROVIDER_TYPE     = "openai"
$env:COPILOT_MODEL             = "Qwen/Qwen3.8-27B-FP8"   # or any catalogue name
$env:COPILOT_PROVIDER_API_KEY  = "$env:PROXY_API_KEY"     # only if PROXY_API_KEY is set
copilot
```

The base URL ends in `/v1`: OpenAI-style clients append `chat/completions`
directly, and the proxy forwards that path verbatim onto the pod.

Typical first session against a stopped pod:

```
copilot -p "summarize this repo"     # ~3 min cold start, then streamed answer
curl -s http://localhost:8080/_status -H "x-proxy-key: ..."   # state=WARM, pod_id=...
# 5 min after you stop working: state=COLD, pod stopped, GPU billing ends
```

### 3.5 Verifying end to end

```bash
curl http://localhost:8080/_health                       # proxy liveness (no auth)
curl -X POST http://localhost:8080/_warm -H "x-proxy-key: $K"   # block until pod serves
curl http://localhost:8080/v1/models -H "x-proxy-key: $K"       # through the pod
curl -N http://localhost:8080/v1/chat/completions -H "x-proxy-key: $K" \
  -H 'content-type: application/json' \
  -d '{"model":"Qwen/Qwen3.8-27B-FP8","stream":true,
       "messages":[{"role":"user","content":"hello"}]}'          # SSE stream
```

Then ask Copilot CLI a question that requires a **tool call** (e.g. "list the
files here") — that exercises the tool-calling path the agent depends on.

## 4. Operations reference

| Endpoint | Auth | Purpose |
|---|---|---|
| `GET /_health` | no | Liveness only (`{"ok":true}`); safe for container healthchecks |
| `GET /_status` | yes | State (`COLD/WARMING/WARM/DEGRADED`), active model, resolved `pod_id`, `last_discovery_error` |
| `POST /_warm` | yes | Pre-warm; 200 when serving, 503 on warmup timeout |
| `GET /metrics` | yes | Prometheus: warmups, warm/cold hits, pod starts/creates, discoveries, model switches, `active_model` gauge |

Troubleshooting quick map:

| Symptom | Likely cause / fix |
|---|---|
| `503 endpoint warmup timeout` | Pod never served within budget. Check the pod's logs in the RunPod console (vLLM crash-loop? OOM?); raise `WARMUP_TIMEOUT_S`/`POD_HEALTH_TIMEOUT_S` for big models. `/_status.last_discovery_error` shows the last failure. |
| 503 immediately, discovery found nothing | Model name doesn't match any pod/template by slug or templateId. Check `models.yaml` template names against the RunPod console. |
| 400 `model not allowed` | Requested model not in the catalogue/allowlist. |
| 401 from the proxy | `PROXY_API_KEY` set but missing/wrong client credential. |
| 401/403 from the pod | The model server wants its own key — set `RUNPOD_UPSTREAM_API_KEY` (do **not** use the RunPod management key). |
| Pod keeps billing after failure | Should not happen (reclaim path); check logs for `reclaiming unhealthy pod` and confirm the pod is stopped in the console. |

Security: with `PROXY_API_KEY` unset, anyone who can reach the port can spend
your RunPod credit. Keep the proxy on localhost, a tailnet, or a VPN, and set
`PROXY_API_KEY`. The RunPod management key is used only for lifecycle REST
calls and is never forwarded to the pod.

## 5. Further enhancements (WIP)

Tracked ideas, roughly in priority order:

- **Progressive status streaming during cold start** — emit SSE comment
  heartbeats (`: warming pod…`) so even aggressive clients see bytes while a
  pod resumes. Needs care: not all OpenAI clients tolerate comment-only frames
  before the first data event.
- **Concurrent multi-model serving** — pool one warm pod per model instead of
  the serialized stop/switch, with a total-GPU budget cap.
- **Queue-position / ETA surfacing** in `/_status` during creation (GPU
  capacity waits are currently opaque).
- **Per-client routing** (API-key → model/pod affinity) so two developers can
  share one proxy without thrashing each other's pod.
- **Scheduled pre-warm** (wake the model before the workday) and a
  `/_stop` admin endpoint for immediate give-up.
- **Request body size guard** — bodies are buffered in memory today.
- **RunPod REST v2 migration** — pod lifecycle currently targets REST v1
  (`RUNPOD_REST_URL` is configurable); v1 retires 2026-11-15.

See also the backlog in [README § Enhancement ideas](../README.md#enhancement-ideas-once-basics-work).
