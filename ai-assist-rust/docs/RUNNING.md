# Running the RunPod Warm Proxy

A production-grade Rust reverse proxy that keeps a RunPod serverless endpoint or
GPU pod warm and transparently proxies OpenAI-compatible traffic (chat
completions, streaming SSE, model listing). This guide covers building, running,
configuring, and verifying it.

## 1. Prerequisites

- **Run the prebuilt binary:** nothing extra — `target/release/runpod-proxy` is
  self-contained (a baked-in Zscaler CA plus webpki roots; no OS trust store
  dependency).
- **Rebuild from source:** Rust 1.98+ (`rustup`). On this Windows box the default
  toolchain is `stable-x86_64-pc-windows-gnu`.

```powershell
cargo build --release -p runpod-proxy
# binary: target\release\runpod-proxy.exe
```

## 2. Configuration model

All configuration is via **environment variables** (there are no CLI flags). The
process **fails fast on invalid configuration** at boot. It listens on
`0.0.0.0:PORT` (default **8080**).

### Modes (`RUNPOD_MODE`)

| Mode | Purpose | Required env |
| ---- | ------- | ------------ |
| `serverless` (default) | Proxy to any OpenAI-compatible upstream (RunPod serverless, or a local mock) | `RUNPOD_SERVERLESS_URL` (default `https://api.runpod.io/v2`) |
| `pod` | Discover / resume / create a RunPod GPU pod for a model | `RUNPOD_API_KEY` **and** (`RUNPOD_POD_ID` **or** `RUNPOD_MODEL_NAME` / `RUNPOD_ALLOWED_MODELS`) |

### Most-used variables

| Variable | Default | Meaning |
| -------- | ------- | ------- |
| `PORT` | `8080` | Listen port |
| `RUNPOD_MODE` | `serverless` | `serverless` or `pod` |
| `PROXY_API_KEY` | *(empty)* | Client bearer token. **Empty = all proxied routes are open.** |
| `RUNPOD_MODEL_NAME` | — | Default/served model |
| `RUNPOD_ALLOWED_MODELS` | — | Comma-separated allowlist; enables model routing |
| `RUNPOD_MODELS_JSON` / `RUNPOD_MODELS_YAML` / `RUNPOD_MODELS_FILE` | — | Model catalogue (inline JSON, inline YAML, or file path) |
| `WARMUP_PATH` | `v1/models` | Health-probe path appended to the upstream URL |
| `POD_HEALTH_MODE` | `model` | `model` (checks `/v1/models` ids) or `text` |
| `PROXY_MAX_CONCURRENT_REQUESTS` | `100` | Concurrency semaphore; returns `503 + Retry-After` on saturation |
| `REQUEST_TIMEOUT_S` | — | Per-request upstream timeout |
| `RUNPOD_ALLOW_POD_CREATE` | `false` | Allow creating pods (billable) — pod mode |
| `RUNPOD_UPSTREAM_API_KEY` | — | Bearer token forwarded **to** the upstream |
| `RUNPOD_ON_MIGRATE` | `fail` | `fail` or `replace` on a migration-blocked pod |
| `LOG_LEVEL` / `LOG_FORMAT` | `INFO` / `text` | Tracing verbosity / format |

> Full variable set: see `crates/runpod-proxy/src/config.rs`.

### Control-plane endpoints (exempt from client auth)

- `GET /_health` — liveness (`{"ok":true}`)
- `GET /_status` — state (COLD/WARMING/WARM), counters, circuit-breaker
- `GET /metrics` — Prometheus exposition
- `POST /_warm` — force a warmup; returns the resulting state
- `POST /_reload` — hot-reload the model catalogue

Every other path is proxied to the warmed upstream.

## 3. Quick local smoke test (bundled mock upstream)

The repo ships a functional OpenAI-compatible mock (`demo/mock_upstream`) with
model listing, chat completions, and real SSE streaming. Run from the repo root:

```powershell
cargo build --release -p runpod-proxy -p mock_upstream

# Terminal A — mock upstream
$env:MOCK_PORT="9137"; $env:MOCK_MODEL="qwen"
.\target\release\mock_upstream.exe

# Terminal B — the proxy, serverless mode pointed at the mock
$env:RUNPOD_MODE="serverless"
$env:RUNPOD_SERVERLESS_URL="http://127.0.0.1:9137"
$env:RUNPOD_MODEL_NAME="qwen"
$env:PORT="8137"
.\target\release\runpod-proxy.exe
```

Verify (Terminal C):

```powershell
curl.exe http://127.0.0.1:8137/_health     # {"ok":true}
curl.exe http://127.0.0.1:8137/_status     # state -> WARM after the first probe
curl.exe http://127.0.0.1:8137/v1/models   # proxied model list

# Chat / SSE. Write the body to a file — PowerShell mangles inline JSON.
'{"model":"qwen","stream":true,"messages":[{"role":"user","content":"hi"}]}' |
  Out-File -Encoding ascii body.json
curl.exe -N -X POST http://127.0.0.1:8137/v1/chat/completions `
  -H "Content-Type: application/json" --data-binary "@body.json"
# streams: role delta -> content deltas -> [DONE]
```

Mock tuning: `MOCK_STREAM_CHUNKS` (default 5), `MOCK_STREAM_DELAY_MS` (default 40).

## 4. Real RunPod (pod mode)

```powershell
$env:RUNPOD_MODE="pod"
$env:RUNPOD_API_KEY="<your-runpod-key>"
$env:RUNPOD_MODEL_NAME="llama-3"
$env:RUNPOD_MODELS_JSON='{"models":[{"name":"llama-3","templates":["llama-3"],"gpus":[{"id":"A","min":1,"max":1}]}]}'
$env:RUNPOD_ALLOW_POD_CREATE="true"   # only if auto-create is wanted (billable)
$env:PROXY_API_KEY="<token-your-clients-send>"
.\target\release\runpod-proxy.exe
```

The proxy discovers a matching RUNNING pod, resumes an EXITED one, or (when
allowed) creates one from the catalogue matrix. Cost-safety invariants apply: at
most one created pod per model, and a created-but-never-healthy pod is always
reclaimed (stopped) on every failure path, including warmup-timeout cancellation.

## 5. Point a client at it

Set the OpenAI base URL to `http://<host>:<PORT>/v1` and the API key to your
`PROXY_API_KEY` (any value works if it is unset). Compatible with the Copilot CLI
and OpenAI SDKs unchanged.

## 6. Graceful shutdown

`Ctrl+C` (all platforms) or `SIGTERM` (Linux/orchestrated deployments) drains
in-flight requests before exiting.

## 7. Tests and checks

```powershell
cargo test --workspace          # full suite
cargo clippy --all-targets
cargo fmt --all -- --check
```
