# RunPod Serverless Warm Proxy

A tiny always-on container that makes a **queue-based RunPod Serverless endpoint** behave
like an always-on RunPod pod — for agentic workflows (GitHub Copilot CLI, Claude Code,
OpenAI-SDK scripts, etc.).

Serverless endpoints only bill while a worker runs. That saves money, but the first
request after a scale-down pays the cold start (image pull + model load: ~1–10 minutes
for large models) and can sit in a queue. This proxy masks that:

1. **First request warms the endpoint** and holds your request until the worker is up.
2. **Keepalive heartbeats** keep the worker alive while you're actively working.
3. **Idle give-up** stops the heartbeats after you go quiet, so the worker is recycled
   and billing stops.

It is a transparent, SSE-safe (streaming) reverse proxy: any path, method, and body is
forwarded unchanged, and token streams pass through without buffering.

## Architecture

```
  GitHub Copilot CLI / any OpenAI-compat client
        |   http://localhost:8080   (COPILOT_PROVIDER_BASE_URL)
        v
  +-------------------------------------------+
  |  runpod-proxy  (always-on, ~50 MB RAM)    |
  |  - warmup on first request (single-flight)|
  |  - keepalive while active                 |
  |  - idle give-up -> stop billing           |
  |  - transparent streaming reverse proxy    |
  |  - manages upstream authentication        |
  +-------------------------------------------+
        |   GET {url}/{WARMUP_PATH} (warmup + keepalive probe)
        |   POST /<any path> ... (forwarded, streamed)
        v
  RunPod Serverless endpoint (vLLM worker)
  - billed only while a worker is running
  - workersMin=0  -> scale to zero, cold start on demand
```

## How it works

State machine: `COLD → WARMING → WARM → (DEGRADED) → COLD`

- **Warmup** — the first request while `COLD` triggers `GET {RUNPOD_SERVERLESS_URL}/{WARMUP_PATH}`
  (a plain readiness probe — any OpenAI-compatible worker serves it). Concurrent requests
  join the same single-flight attempt (no stampede). **Any HTTP response — even 4xx/5xx —
  counts as "endpoint up"**, because it proves a worker is answering. Connect errors
  (worker not there yet) are retried with 1s→15s backoff until `WARMUP_TIMEOUT_S`,
  after which the request fails with 503.
- **Keepalive** — while `WARM`, the proxy re-`GET`s the same URL every
  `KEEPALIVE_INTERVAL_S` seconds, but only if real traffic arrived within the last
  `IDLE_GIVEUP_S` seconds. After `IDLE_GIVEUP_S` of silence it stops pinging, marks the
  endpoint `COLD`, and lets RunPod recycle the worker.
- **Degraded** — 3 consecutive keepalive failures mark the endpoint `DEGRADED`; heartbeats
  continue and a single successful probe re-warms it straight back to `WARM`, so a
  recycled worker is detected and replaced in at most 3 intervals.
- **Forwarding** — everything else is proxied upstream with streaming
  (`accept-encoding: identity` is forced so response bytes pass through untouched).

> **RunPod `idleTimeout` must be larger than `KEEPALIVE_INTERVAL_S`.**
> RunPod's default endpoint `idleTimeout` is 5 seconds — with the proxy default interval
> of 25s the worker would be recycled between pings. Set the endpoint's
> **idleTimeout to 30–60s** (or lower `KEEPALIVE_INTERVAL_S`).

## Pod mode (alternative backend)

Set `RUNPOD_MODE=pod` to target a **persistent GPU pod** instead of a serverless
endpoint. The lifecycle is managed explicitly via the RunPod REST API
(`https://rest.runpod.io/v1`). RunPod has deprecated REST API v1 and plans to
retire it on November 15, 2026; `RUNPOD_REST_URL` is configurable for that
reason.

Pod mode has two sub-modes:

- **Pinned** — set `RUNPOD_POD_ID` to retain the existing behavior. The proxy
  starts and stops that pod and uses its derived (or explicitly configured)
  URL.
- **Discovered** — leave `RUNPOD_POD_ID` empty and set `RUNPOD_MODEL_NAME`.
  On the first request, or after idle give-up, the proxy resolves a healthy
  pod on demand. It also revalidates after `POD_REVALIDATE_S` seconds since
  the last successful request, in case the pod was stopped, deleted, or
  reclaimed externally.

Discovery first lists matching `RUNNING` pods and health-probes each, using the
first one that responds. Health is classified by `POD_HEALTH_MODE` (default
`model`: the warmup route's JSON must list the target model) from
`{pod_url}/{WARMUP_PATH}`; a `desiredStatus=RUNNING` result alone is not
health because the model server may still be loading. If no running pod is
healthy, discovery tries matching `EXITED` pods one at a time. A pod's
`start` call is retried with backoff (up to `POD_READY_TIMEOUT_S`) because
RunPod's REST API can return a transient `5xx` for a few seconds right after
a pod was stopped, while the backend is still cleaning up -- retrying avoids
abandoning a perfectly reusable pod and provisioning a costly duplicate.
Once `start` itself succeeds (or keeps failing past the retry budget, e.g.
because GPU capacity is scarce or the pod configuration changed), discovery
moves to the next candidate; successful starts wait for
`desiredStatus=RUNNING` and then probe for up to `POD_READY_TIMEOUT_S` and
`POD_HEALTH_TIMEOUT_S`.

Creation is **opt-in**: only `RUNPOD_ALLOW_POD_CREATE=true` permits discovery
to select a matching non-serverless template (or the template named by
`RUNPOD_TEMPLATE_NAME`) and provision a pod. It uses the GPU and storage
settings in the configuration table. This provisions real, billable GPUs.
With creation disabled (the default), discovery only reuses or resumes
existing pods. At most one pod is created per proxy process; its ID is
remembered and later attempts resume that pod instead of creating another.
Any pod the proxy starts or creates that never becomes healthy is stopped to
prevent silent billing. A pod found already `RUNNING` is never stopped,
because the proxy did not bring it up and another user or process may be
using it.

Once the proxy has created a pod, it is the sole candidate for the rest of
that process's life. If it is permanently broken, discovery retries it
(resume → probe → reclaim) rather than falling back to other pods; restart
the proxy to clear this state. This deliberately trades some availability for
cost safety. If the pod is deleted or force-terminated out-of-band, the proxy
forgets it and falls back to full discovery (which may create a replacement
when creation is allowed). Idle give-up stops the discovered pod. As with
pinned pod mode, a stopped pod still bills for disk/volume storage.

Model matching compares slugs (casefolded, with runs of non-alphanumerics
collapsed to `-`) against a pod or template's name, image, and every
environment value. Thus `Qwen/Qwen3-32B` matches `qwen-qwen3-32b`, an image
tag containing that slug, or `MODEL_NAME=Qwen/Qwen3-32B`; this also accounts
for RunPod pod names not allowing `/`.

Pinned pod behavior:

- **Auto-start** — the first request while `COLD` checks the pod's status via
  `GET /pods/{RUNPOD_POD_ID}` and sends `POST /pods/{RUNPOD_POD_ID}/start` only if it
  is not already running (so a proxy restart against a live pod costs nothing), then
  probes the pod's HTTP endpoint with the usual warmup retry/backoff until it answers.
  A failed start call is retried within the same warmup budget (`WARMUP_TIMEOUT_S`).
- **Pod deleted** — if `GET /pods/{RUNPOD_POD_ID}` returns `404`, the pod is gone
  (RunPod never reuses a deleted id). The warmup fails fast with a 503 whose message
  says to point `RUNPOD_POD_ID` at a live pod or unset it to enable discovery —
  instead of burning `WARMUP_TIMEOUT_S` on doomed starts. `POST .../stop` treats a
  deleted pod as already stopped.
- **Auto-stop** — after `IDLE_GIVEUP_S` of no real traffic, the proxy sends
  `POST /pods/{RUNPOD_POD_ID}/stop` (skipped if the pod already reports stopped)
  and only then marks the endpoint `COLD`. If the
  stop call fails, the state is kept and the stop is retried on the next keepalive
  tick — so a still-billing pod is never silently forgotten.
- **Stop on shutdown** — on graceful shutdown (SIGTERM, `docker compose stop`) the
  proxy also stops the pod best-effort, so a proxy restart does not leave it
  billing until the next session's idle give-up.
- **Upstream URL** — derived as `https://{RUNPOD_POD_ID}-{RUNPOD_POD_PORT}.proxy.runpod.net`,
  or overridden with `RUNPOD_POD_URL`. `RUNPOD_SERVERLESS_URL` is ignored in pod mode.

Env vars: `RUNPOD_MODE=pod`, `RUNPOD_API_KEY` (required),
`RUNPOD_POD_PORT` (default `8000`), `RUNPOD_POD_URL` (optional override),
`RUNPOD_REST_URL` (default `https://rest.runpod.io/v1`, override for tests), and
`RUNPOD_UPSTREAM_API_KEY` (optional model-server credential). Pinned mode also
requires `RUNPOD_POD_ID`; discovered mode instead requires
`RUNPOD_MODEL_NAME`. The RunPod management key is never sent to the pod HTTP
endpoint. Discovery-specific settings are listed below.

> **Cost note:** a stopped pod still bills for its disk/volume storage, and resuming a
> stopped pod is slower than serverless scale-to-zero (and GPU availability on resume
> is not guaranteed). Pod mode trades that for a persistent filesystem and a dedicated
> machine while you work.

## Per-request model routing

By default the proxy serves whatever model the backend is configured for. In
discovered pod mode (`RUNPOD_MODE=pod` with `RUNPOD_POD_ID` empty) it can
instead pick the target model **per request**, from an explicit allowlist, and
switch the underlying pod on demand.

- **Per-request model selection** — the proxy inspects the request body and, if
  it is JSON with a top-level string `model` field, routes to that model. The
  body is only parsed when the `Content-Type` contains `json` and it is at most
  2 MB; anything larger is not inspected. A body that is missing, not JSON,
  unparseable, or has no (or a non-string) `model` field is forwarded
  unchanged and falls back to the **default model** — so a request never fails
  merely because it omitted a model.
- **Config-driven allowlist** — set `RUNPOD_ALLOWED_MODELS` to a comma-separated
  list of the models you allow (whitespace around each entry is trimmed).
  Setting it enables per-request routing. A requested model is matched against
  the list **case-insensitively and by slug** (casefolded, with runs of
  non-alphanumerics collapsed to `-`), so `Qwen/Qwen3-32B` and `qwen-qwen3-32b`
  both match the same allowlist entry. The **canonical spelling from the
  allowlist** is what gets used downstream (pod/template matching, metrics,
  `/_status`), regardless of how the client spelled it — including the `model`
  field of the forwarded body, which the proxy rewrites to the canonical
  spelling before sending it to the pod, since the pod's model server only
  knows the canonical name. The **default model** is
  `RUNPOD_MODEL_NAME` if set, otherwise the first allowlist entry.
- **Disallowed models** — a requested model that matches no allowlist entry is
  rejected with HTTP `400` and this JSON body, without touching the backend:

  ```json
  {"error": "model not allowed", "model": "<requested>", "allowed": ["<model-a>", "<model-b>"]}
  ```

- **Backward compatibility** — when `RUNPOD_ALLOWED_MODELS` is not set, the
  requested `model` field is ignored entirely and the default model is always
  used, so existing single-model and serverless deployments behave exactly as
  before. Pinned pod mode (`RUNPOD_POD_ID` set) never switches models, and
  serverless mode never switches either.

- **One active model at a time** — the proxy serves exactly one model at once. A
  request for a *different* allowed model triggers a switch: it best-effort
  **drains** in-flight requests (up to `MODEL_SWITCH_DRAIN_S`), **stops** the
  current pod, then runs the usual discovery/warmup for the new model. Because
  the switch is serialized, in-flight requests for the current model are never
  cut mid-stream unless the drain budget expires.

  > **Caveat:** interleaved concurrent traffic for two different models will
  > thrash the pod — every alternation pays a full cold start (stop + discover +
  > warmup). Keep `RUNPOD_ALLOWED_MODELS` small and have clients prefer one
  > model at a time; the routing is designed for switching between sessions, not
  > for serving multiple models simultaneously.

- **Drain budget** — `MODEL_SWITCH_DRAIN_S` (default `30`) caps how long a
  switch waits for in-flight requests to finish before stopping the current
  pod. It is best-effort: when the budget expires the switch proceeds anyway,
  which may cut off requests still streaming from the old model.

- **Cost safety across models** — the at-most-one-created-pod guard is tracked
  **per model** (a created pod's ID is remembered per model and resumed rather
  than re-created). If the pod stop during a switch fails, the pod ID is
  recorded and retried by the keepalive loop on every tick, so a switch can
  never silently leak a still-billing pod.

## Declarative model catalogue

Per-request routing and discovery can be driven by an explicit **model
catalogue** instead of the fuzzy slug heuristic. The catalogue makes the
`model → templates → GPU` mapping first-class configuration.

### Why it exists

The legacy heuristic matches a requested model against a pod or template's
name, image, and env values by **substring of slug**. That both false-negatives
and risks the wrong pod:

- **False negative** — a template named `Qwen3.8-27B-FP8` never matches the
  model `Qwen/Qwen3.8-27B-FP8`, because the template slug `qwen3-8-27b-fp8` is
  not a substring of the model slug `qwen-qwen3-8-27b-fp8` (the `qwen/` prefix
  and punctuation differ once collapsed).
- **Wrong pod** — a permissive substring can silently route a request to a
  *different* model's pod that happens to share a slug fragment.

The catalogue replaces guessing with a declared mapping, so matching and
creation are exact and auditable.

### Schema

The catalogue is a **JSON or YAML** file — the file extension picks the parser
(YAML is read with `yaml.safe_load`, so a hand-edited file can never execute
code at boot). The top level is either an object with a `models` list or a
bare array of model objects — both forms are accepted, in either format:

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
    # Optional: suggest specific RunPod datacentre ids for creation. The v2
    # create API has no priority field, so the order is informational —
    # RunPod may pick any listed datacentre that has capacity.
    # datacenters: [US-TX-3, US-KS-3]
  - name: Qwen/Qwen3.8-27B-FP8
    templates: [qwen3-8-27b-fp8-vllm]
    gpus: [{ id: "NVIDIA L40S" }]
```

The same catalogue as JSON:

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
      "port": 8000,
      "container_disk_gb": 50,
      "volume_gb": 100,
      "cloud_type": "SECURE"
    },
    {
      "name": "Qwen/Qwen3.8-27B-FP8",
      "templates": ["qwen3-8-27b-fp8-vllm"],
      "gpus": [{ "id": "NVIDIA L40S" }]
    }
  ]
}
```

Ready-to-edit copies live at [`models.example.yaml`](models.example.yaml) and
[`models.example.json`](models.example.json). In YAML, quote any value
containing `": "` or `"#"`, and note that unquoted `yes`/`no`/`on`/`off` parse
as booleans (YAML 1.1).

Fields per model:

| Field | Required | Default | Meaning |
|---|---|---|---|
| `name` | yes | — | Canonical model name (e.g. `Qwen/Qwen3-32B`). Matched case- and slug-insensitively; this exact spelling is what is used downstream. |
| `templates` | yes | — | Ordered preference list of RunPod **template names** (not ids) to reuse or create from. Must be non-empty. |
| `gpus` | no | `[]` | Ordered list of `{id, min, max}` GPU preferences, tried in order for creation. |
| `gpus[].id` | yes | — | RunPod GPU type id (e.g. `NVIDIA H100 80GB HBM3`). |
| `gpus[].min` | no | `1` | Smallest GPU count to try for this type. |
| `gpus[].max` | no | `= min` | Largest GPU count to try; counts are walked ascending from `min` to `max`. |
| `port` | no | `RUNPOD_POD_PORT` | Per-model override of the pod HTTP port. |
| `container_disk_gb` | no | `RUNPOD_CONTAINER_DISK_GB` | Per-model override of the container disk size on create. |
| `volume_gb` | no | `RUNPOD_VOLUME_GB` | Per-model override of the volume size on create. |
| `cloud_type` | no | `RUNPOD_CLOUD_TYPE` | Per-model override of the cloud type on create. |
| `datacenters` | no | `[]` | Ordered list of RunPod datacentre ids (e.g. `US-TX-3`) the pod may be created in. Sent as `dataCenterIds` on create; empty means RunPod's own default placement. The v2 create API has no priority field, so the declared order is informational only. |
| `datacenter_priority` | no | `availability` | Accepted for compatibility only — never sent to RunPod (the v2 create API has no priority field). |

Each per-model override falls back to its global env var when omitted.

### How it is supplied

Set exactly one of:

- `RUNPOD_MODELS_FILE` — path to a catalogue file, JSON or YAML (the extension
  picks the parser). **Wins if both are set.**
- `RUNPOD_MODELS_JSON` — the catalogue inline as a JSON string (handy for
  docker-compose without mounting a file).

#### Inline catalogue (no mount needed)

For one or two models, `RUNPOD_MODELS_JSON` is the simplest option: it needs no
volume mount and no `MODELS_FILE` compose variable. Author the catalogue
pretty-printed as usual —

```json
{
  "models": [
    {
      "name": "Qwen/Qwen3.8-27B-FP8",
      "templates": ["qwen3-vllm-fp8"],
      "gpus": [{ "id": "NVIDIA H100 80GB HBM3", "min": 1, "max": 2 }]
    }
  ]
}
```

— then collapse it to the single line that goes in `.env`:

```
RUNPOD_MODELS_JSON={"models":[{"name":"Qwen/Qwen3.8-27B-FP8","templates":["qwen3-vllm-fp8"],"gpus":[{"id":"NVIDIA H100 80GB HBM3","min":1,"max":2}]}]}
```

Constraints:

- It must be **one line** — `env_file` does not support multi-line values, so
  the JSON cannot be pretty-printed in place.
- Use **no surrounding quotes and no escaping**: write the raw JSON directly
  after `=`. Compose passes an `env_file` value through byte-for-byte.
- Avoid `$` in any value, because docker-compose would attempt variable
  interpolation on it (model, template and GPU names normally contain none).

Generate the collapsed line from a pretty file with:

```powershell
"RUNPOD_MODELS_JSON=" + (Get-Content models.json -Raw | python -c "import json,sys; print(json.dumps(json.load(sys.stdin),separators=(',',':')))")
```

For docker-compose, mount the file and point the proxy at it. The compose file
mounts `${MODELS_FILE:-./models.yaml}` at `${CONTAINER_MODELS_FILE:-/app/models.yaml}`
read-only:

```yaml
services:
  runpod-proxy:
    build: .
    container_name: runpod-proxy
    env_file:
      - ${ENV_FILE:-.env}
    ports:
      - "8080:8080"
    volumes:
      - ${MODELS_FILE:-./models.yaml}:${CONTAINER_MODELS_FILE:-/app/models.yaml}:ro
    restart: unless-stopped
```

Using a catalogue with Docker:

```powershell
# 1. create your catalogue next to docker-compose.yml
Copy-Item models.example.yaml models.yaml
# 2. (optional) point compose at it — ./models.yaml is the default; both
#    interpolation vars live in the shell or compose .env, NOT in ENV_FILE
$env:MODELS_FILE = "./models.yaml"
# 3. tell the app to load the mounted path (this DOES go in .env)
#    RUNPOD_MODELS_FILE=/app/models.yaml
docker compose up -d --build
```

`MODELS_FILE` (host file to mount) and `CONTAINER_MODELS_FILE` (in-container
target, default `/app/models.yaml`) are **compose interpolation** variables read
by docker-compose; they are *not* read by the app, so they must live in your
shell environment or a compose `.env` file — putting them in `ENV_FILE` will
**not** work, because `env_file` values are only injected into the container.
`RUNPOD_MODELS_FILE=/app/models.yaml` is the *app* setting and belongs in the env
file.

- The catalogue is deliberately **not** baked into the image, so editing
  `models.yaml` and restarting the container picks up changes with no rebuild.
- The mount is **read-only**.
- For simple setups the alternative is `RUNPOD_MODELS_JSON` as single-line JSON
  in the env file (multi-line values are not supported by `env_file`).
- A catalogue path that does not exist is a **boot-time** failure by design
  (fail fast), not a silent fallback.

### How it drives matching

During discovery a pod is a **candidate** if **either**:

1. **(precise)** its `templateId` is one of the active model's resolved
   template ids — the catalogue template names are looked up against the
   account's templates once and cached, **or**
2. **(legacy)** the name/image/env/**launch-args** slug heuristic matches
   (kept for backward compatibility, and for pods not created by this
   proxy's own matrix). The launch args a pod was started with (e.g.
   `Qwen/Qwen3-32B --port 8000 ...`) are checked too, since they carry the
   actual model tag regardless of how the pod or template happens to be
   named — this catches manually-created or legacy pods serving the same
   model that would otherwise go unmatched and cause a needless duplicate,
   billable pod to be created.

The account's template list is fetched **only when a catalogue spec exists**
for the active model, so non-catalogue deployments gain no extra API call on
the discovery path.

Note: since the launch-args heuristic is a substring match against the whole
launch command, a stopped pod running the same model **under a different,
incompatible configuration** (e.g. different GPU count/tensor-parallel
settings) will now also be matched and resumed rather than replaced. If you
have old/experimental pods you don't want reused, terminate them rather than
leaving them stopped.

### How creation walks the matrix

When `RUNPOD_ALLOW_POD_CREATE=true` and no existing pod is reusable, creation
walks the matrix **cheapest viable first**:

```
for each template (in preference order)
  for each gpu (in preference order)
    for count = min .. max (ascending)
      try create_pod(template, gpu, count)
```

A failed attempt (e.g. GPU capacity unavailable) advances to the next
combination. Template selection precedence is:

1. `RUNPOD_TEMPLATE_NAME` override (exact name), then
2. the catalogue `templates` list (declaration order), then
3. the legacy heuristic (first non-serverless template whose slug matches).

A catalogue template name that is **absent from the account** is logged as a
`WARNING` (listing the available non-serverless template names) and skipped —
the matrix continues with the remaining templates.

### Cost safety

The matrix is designed so **at most one billable pod exists per model per
process**:

- The **first** `create_pod` that returns a pod is recorded, increments the
  create counter, and **immediately ends the whole matrix** — no further create
  is attempted.
- A create that **fails** provisioned nothing, so advancing to the next
  combination is safe.
- If the created pod then **fails its readiness/health probe**, it is
  **reclaimed (stopped)** and **not** replaced by another creation attempt. The
  created pod id is remembered, so the next `start()` **resumes that same pod**
  rather than creating a second one.
- `RUNPOD_MAX_CREATE_ATTEMPTS` (default `12`) hard-caps the number of create
  attempts per discovery, bounding a pathological catalogue (many templates ×
  GPUs × counts).

These guards are verified by mutation testing.

### Fail-fast validation

The catalogue is parsed and validated at startup; the process **refuses to
boot** on any of:

- invalid JSON/YAML, or a missing/unreadable `RUNPOD_MODELS_FILE`;
- duplicate model names (compared by slug);
- missing or empty `templates`;
- unknown keys at the model or gpu level (catches typos such as `template` for
  `templates`);
- a GPU `min` < 1, or `max` < `min`;
- non-integer or boolean numerics (a JSON `true`/`false` — or an unquoted YAML
  `yes`/`no`/`on`/`off` — is not an integer);
- duplicate GPU ids within a model;
- non-positive `port`, `container_disk_gb`, or `volume_gb`.

Additionally, `RUNPOD_MODEL_NAME` set to a model **absent from the catalogue**
is a boot-time error (it lists the known model names).

When a catalogue is present it **supersedes `RUNPOD_ALLOWED_MODELS`** as the
per-request allowlist: the allowed set becomes the catalogue's model names, the
default model is `RUNPOD_MODEL_NAME` (if it resolves) otherwise the first
catalogue entry, and per-request routing is enabled. Requested model names
resolve **case- and slug-insensitively** to the catalogue's canonical spelling,
which is what gets used downstream (matching, creation, metrics, `/_status`);
the `model` field of the forwarded body is rewritten to that spelling before
the request is sent to the pod.

### Hot-reloading the catalogue

Edit the mounted file (or the inline source), then:

```bash
curl -s -X POST http://localhost:8080/_reload -H "x-proxy-key: $PROXY_API_KEY"
```

The new catalogue is fully validated before it is swapped in, so a bad edit
cannot take the proxy's model list down. What a successful reload does and
doesn't do:

- **Adds models** — immediately routable; in discovery mode a new model routes
  to its own pod on the next request (normal switch: the current pod is left
  running if the proxy didn't start it, per the cost-safety rules).
- **Drops models** — the model becomes unroutable (`400` on the next request);
  if it happened to be active, the next request falls back to the new default
  (first entry) and switches to it.
- **Does not** restart pods, re-warm the active model, touch in-flight
  requests, or change the `model` a running pod is serving.

## Quickstart

1. **Deploy your serverless endpoint** (vLLM/OpenAI-compatible). Recommended starting
   settings: `workersMin=0`, `workersMax` to your concurrency ceiling, `idleTimeout ≥ 30`.

2. **Configure** the proxy:

   ```bash
   cp .env.example .env
   # edit .env:
   #   RUNPOD_SERVERLESS_URL=https://<endpoint-id>.api.runpod.ai   (no /v1!)
   #   RUNPOD_API_KEY=<your runpod api key>
   ```

3. **Run it:**

   ```bash
   docker compose up -d --build
   ```

4. **Verify:**

   ```bash
   curl -s http://localhost:8080/_status
   # {"state":"COLD","endpoint":"https://…","last_real_traffic_at":null,...}

   # warm it (blocks until the worker answers, then 200)
   curl -s -X POST http://localhost:8080/_warm
   curl -s http://localhost:8080/_status
   # {"state":"WARM",...}

   # then hit any real route through the proxy (path is appended verbatim
   # to RUNPOD_SERVERLESS_URL, so /v1/... lands on the worker's /v1/...):
   curl -s http://localhost:8080/v1/models
   ```

## Local demo (no RunPod credentials needed)

One command runs the full lifecycle against a simulated serverless endpoint
(including a fake 3-second cold start):

```powershell
cd runpod-proxy
.\demo\run-demo.ps1            # run the demo, leave the stack running
.\demo\run-demo.ps1 -Cleanup   # same, but stops everything afterwards
```

It shows: COLD state → first request warms the endpoint (simulated cold start) →
WARM → model list → **SSE streaming chat completion** through the proxy →
non-streaming reply echoing the injected `RUNPOD_API_KEY` → 12s idle → back to
COLD (keepalives stopped, RunPod would recycle the worker and billing stops).

The demo starts `demo/mock_upstream.py` (a small FastAPI app simulating the
endpoint, including a cold start on the first `/v1/*` request) and the proxy
container with `.env.demo` (short keepalive/idle intervals so the lifecycle is
visible in ~15s).

> **Guide:** for a full Copilot CLI walkthrough (discovery, cold start,
> health verification, template creation), see
> [docs/copilot-cli-guide.md](docs/copilot-cli-guide.md).

## Pointing the GitHub Copilot CLI at it (BYOK)

Copilot CLI supports custom OpenAI-compatible providers via environment variables
(verified against the official docs:
[Using your own LLM models in GitHub Copilot CLI](https://docs.github.com/en/copilot/how-tos/copilot-cli/customize-copilot/use-byok-models),
and verified end-to-end with the real CLI through this proxy):

```powershell
# 1) start the proxy (reads .env)
cd C:\path\to\runpod-proxy
docker compose up -d

# 2) point the Copilot CLI at the proxy
$env:COPILOT_PROVIDER_BASE_URL = "http://localhost:8080/v1"
$env:COPILOT_PROVIDER_TYPE     = "openai"          # default; any OpenAI-compatible endpoint
$env:COPILOT_MODEL             = "YOUR-MODEL-NAME" # check: curl http://localhost:8080/v1/models
copilot                                  # interactive session
# copilot -p "your task" --allow-all     # one-shot / scripting
```

`COPILOT_PROVIDER_API_KEY` is optional for RunPod serverless because the proxy replaces
client Authorization with `RUNPOD_API_KEY` — unless you set `PROXY_API_KEY`, in which
case set `COPILOT_PROVIDER_API_KEY` to that value so the CLI can authenticate to the
proxy. In pod mode, client Authorization is
preserved unless `RUNPOD_UPSTREAM_API_KEY` is set; use either variable for the key
configured on the pod's model server.

The base URL ends in `/v1` because OpenAI-style clients append `chat/completions`
directly to it; the proxy forwards that path onto `RUNPOD_SERVERLESS_URL`.

`COPILOT_PROVIDER_TYPE` defaults to `openai` (works for any OpenAI Chat Completions
compatible endpoint, including vLLM). Your model must support **tool calling** and
**streaming** for the agentic experience.

The proxy is a plain local HTTP server: the CLI must be able to reach it. Same machine
→ `http://localhost:8080`. CLI running elsewhere (e.g. on a RunPod pod) → point it at
the proxy wherever you host it (tailnet, VPN, tunnel).

In serverless mode, the proxy injects `RUNPOD_API_KEY` as `Authorization: Bearer ...`.
In pod mode, that key is restricted to lifecycle REST calls; model requests use
`RUNPOD_UPSTREAM_API_KEY` when configured, otherwise client Authorization passes through.

## Cost model (the tradeoff, explicitly)

| Scenario | What you pay |
|---|---|
| You're not working | Nothing (serverless worker stopped; proxy is a ~50 MB container) |
| First request after idle | The cold start — worker boots, model loads, possibly waits in the queue. This is the one thing the proxy cannot avoid; it only *surfaces* it clearly and survives it via retries up to `WARMUP_TIMEOUT_S` |
| While you work (within `IDLE_GIVEUP_S` of last real traffic) | The warm worker, billed as a running serverless worker (same class of billing as an active flex worker) |
| Idle > `IDLE_GIVEUP_S` | Proxy stops heartbeating → worker recycled after RunPod `idleTimeout` → billing stops |

In pod mode, "stop" is the deliberate idle action: RunPod `stop` releases the
GPU (no compute billing) but keeps the pod's volume data (small storage
charge) so the next start is fast. `DELETE` would destroy the pod and its
data and force a full re-provision, so the proxy never deletes — only
stops/restarts.

Compared to the alternatives:

- **`workersMin=1`** = an always-on worker 24/7 (cheaper than a pod, but you pay even
  when idle for days). The proxy + `workersMin=0` gives you the pod-like *experience*
  with pod-like *cost only while you actually work*.
- **No proxy** = you pay the cold start on every session start.

Tuning knobs: lower `KEEPALIVE_INTERVAL_S` (and RunPod `idleTimeout` above it) for
snappier recovery from a recycle; raise `IDLE_GIVEUP_S` if you often take long
thinking breaks; raise `WARMUP_TIMEOUT_S` if your model is huge or the queue is deep.

The compose file sets `stop_grace_period: 30s` (Docker's default is 10s) so
`docker compose down` / image upgrades don't cut a long streaming generation
mid-token.

## Configuration

| Env var | Default | Meaning |
|---|---|---|
| `RUNPOD_SERVERLESS_URL` | *(required in serverless mode)* | Base URL of your endpoint. The proxy appends the client's path verbatim, so do **not** include `/v1`. Direct endpoint: `https://<endpoint-id>.api.runpod.ai`; OpenAI-compat gateway: `https://api.runpod.ai/v2/<endpoint-id>`. Ignored in pod mode. |
| `RUNPOD_API_KEY` | *(empty; required in pod mode)* | RunPod API key. Authenticates upstream requests in serverless mode and pod lifecycle REST calls in pod mode. |
| `RUNPOD_UPSTREAM_API_KEY` | *(empty)* | Optional model-server Bearer key. In pod mode, overrides client Authorization for probes and forwarded requests. |
| `RUNPOD_UPSTREAM_API_KEY_TEMPLATE` | *(empty)* | Template with a `{pod_id}` placeholder (e.g. `sk-{pod_id}`) for pods whose model-server key is derived from the pod's own id (some vLLM templates set `VLLM_API_KEY=sk-$RUNPOD_POD_ID`). Takes precedence over `RUNPOD_UPSTREAM_API_KEY` whenever a pod id is known, so the correct key is derived automatically every time discovery creates or replaces a pod, instead of a static key going stale. **Write the placeholder literally as `{pod_id}`** — a `$RUNPOD_POD_ID` there is expanded by docker-compose at deploy time, freezing the key at the id that was current at last deploy (it would no longer track a replacement pod). |
| `RUNPOD_MODE` | `serverless` | Backend type: `serverless` (queue-based endpoint) or `pod` (persistent pod, started/stopped via the REST API). |
| `RUNPOD_POD_ID` | *(required in pod mode)* | Pod to start/stop via `POST /pods/{id}/start` and `/stop`. |
| `RUNPOD_ON_MIGRATE` | `fail` | Pinned pod mode: what to do when a start is blocked because the pod's original host has no free GPU ("please migrate your pod" prompt, or REST 500 "not enough free GPUs on the host machine" — the host assignment is sticky, so start retries can never succeed). `fail` (default) surfaces the error (with the fix hint) and leaves the pod untouched; `replace` terminates the pod and creates a fresh one via the v2 API with the same spec (same template/GPU, network volume re-attached so on-disk data such as HF model weights is not re-downloaded; a RunPod "no capacity" 400 is retried with backoff within the warmup budget), then keeps serving via the new pod id. After a replace, update `RUNPOD_POD_ID` to the replacement's id so a proxy restart pins the survivor. |
| `RUNPOD_POD_PORT` | `8000` | Port used to derive the pod upstream URL `https://<pod-id>-<port>.proxy.runpod.net`. |
| `RUNPOD_POD_URL` | *(empty)* | Optional explicit pod upstream URL; overrides the derived one. |
| `RUNPOD_REST_URL` | `https://rest.runpod.io/v1` | RunPod REST API base for pod start/stop. |
| `RUNPOD_AVAILABILITY_URL` | `https://api.runpod.io/v2` | RunPod v2 API base for the GPU availability poller (`GET {base}/catalog/gpus?include=AVAILABILITY&product=POD`). |
| `GPU_AVAILABILITY_INTERVAL_S` | `300` | GPU availability poll interval in seconds (`0` disables). Active in pod mode with a RunPod API key and at least one tracked GPU type (the catalogue's, else `RUNPOD_GPU_TYPE_IDS`). Poll failures keep the last-known values and surface the age and last error instead. |
| `RUNPOD_MODEL_NAME` | `""` | Enables pod discovery when `RUNPOD_MODE=pod` and `RUNPOD_POD_ID` is empty. Used to match pod/template names, images, and env values by slug. Also the default model for per-request routing. |
| `RUNPOD_ALLOWED_MODELS` | `""` | Comma-separated allowlist enabling per-request model routing in discovered pod mode. A request's `model` field is matched case-insensitively and by slug; the canonical allowlist spelling is used downstream. The default model is `RUNPOD_MODEL_NAME` if set, else the first entry. Unset means the requested model is ignored. **Superseded by the model catalogue when one is configured.** |
| `RUNPOD_MODELS_FILE` | `""` | Path to a model catalogue file — JSON or YAML (the extension picks the parser; see [Declarative model catalogue](#declarative-model-catalogue)). With Docker, the in-container target of the mount is the `CONTAINER_MODELS_FILE` compose variable (default `/app/models.yaml`). Wins over `RUNPOD_MODELS_JSON` if both are set. When present it defines the per-request allowlist and the model→template→GPU mapping. |
| `RUNPOD_MODELS_JSON` | `""` | Inline JSON model catalogue (same schema), for docker-compose without mounting a file. Ignored when `RUNPOD_MODELS_FILE` is set. |
| `MODEL_SWITCH_DRAIN_S` | `30` | Best-effort budget (seconds) to drain in-flight requests before stopping the current pod on a model switch; the switch proceeds when it expires. |
| `RUNPOD_ALLOW_POD_CREATE` | `false` | Permit discovery to create a pod from a matching template. **Creates real, billable GPU resources; opt in explicitly.** |
| `RUNPOD_GPU_TYPE_IDS` | *(empty)* | Discovery creation GPU types, comma-separated and ordered. |
| `RUNPOD_GPU_TYPE_PRIORITY` | `availability` | Accepted for compatibility only — the v2 create API takes a single GPU id per create and has no priority field, so this value is never sent to RunPod. |
| `RUNPOD_CLOUD_TYPE` | `SECURE` | Cloud type used when discovery creates a pod. |
| `RUNPOD_TEMPLATE_NAME` | `""` | Optional exact template name for discovery creation; otherwise the catalogue `templates` (if configured) or a template matching `RUNPOD_MODEL_NAME` is selected. |
| `RUNPOD_MAX_CREATE_ATTEMPTS` | `12` | Hard cap on create attempts per discovery when walking the catalogue's template × GPU × count matrix. Bounds a pathological catalogue. |
| `POD_REVALIDATE_S` | `300` | In discovery mode, re-resolve after this many seconds since the last successful request. |
| `POD_HEALTH_TIMEOUT_S` | `180` | Discovery health-probe budget after a pod is running. Each response is classified by `POD_HEALTH_MODE`. For large models on a freshly created pod, the first probe must wait out image pull + weight loading — 180s can be too tight; use 300–600. |
| `POD_HEALTH_MODE` | `model` | Pod-mode readiness bar. `model` (default) — the warmup route's JSON must list the target model; `any` — any non-synthetic HTTP response (legacy; only safe for servers that refuse to answer before the model is loaded, e.g. vLLM's `/v1/models`); `completion` — a real 1-token chat completion must succeed (strictest; validates the full inference path, one inference per cold start). Serverless mode always uses `any`. The default protects against backends that answer their HTTP routes while weights are still loading — declaring WARM then would stall the first real request for minutes. |
| `POD_READY_TIMEOUT_S` | `120` | Discovery budget waiting for a started or created pod to report `desiredStatus=RUNNING`. |
| `POD_CIRCUIT_BREAKER_THRESHOLD` | `3` | Consecutive reclaimed (started-but-never-healthy) pods for the active model before the circuit breaker opens. Protects against a crash-looping pod (e.g. bad launch args) burning a full `POD_HEALTH_TIMEOUT_S` on every client retry forever. `0` disables the breaker. |
| `POD_CIRCUIT_BREAKER_COOLDOWN_S` | `300` | How long `start()` fails fast (`LifecycleError`, no RunPod calls) once the breaker opens. After it elapses, one fresh attempt is allowed and the streak resets. Surfaced via `GET /_status` as `circuit_breaker_open_s`. |
| `RUNPOD_CONTAINER_DISK_GB` | *(unset; template default)* | Container disk size when creating a pod. |
| `RUNPOD_VOLUME_GB` | *(unset; template default)* | Volume size when creating a pod. |
| `WARMUP_PATH` | `v1/models` | Path (relative to base) the warmup/keepalive GETs. Serverless mode treats any HTTP response — even 4xx/5xx — as "worker up"; pod mode classifies the response per `POD_HEALTH_MODE` (default: the model must be listed). For the v2 OpenAI-compat gateway use `openai/v1/models`. On vLLM pods, `/v1/models` is the correct probe: it only responds once the weights are loaded, while vLLM's `/health` returns 200 before the model is ready. |
| `WARMUP_TIMEOUT_S` | `600` | Total budget for the first-request warmup, including retries. |
| `WARMUP_BACKOFF_MAX_S` | `15` | Max delay between warmup retries (starts at 1s, doubles). |
| `KEEPALIVE_INTERVAL_S` | `25` | Seconds between keepalive pings. Must be < RunPod endpoint `idleTimeout`. |
| `IDLE_GIVEUP_S` | `300` | After this much without *real* traffic, heartbeats stop and the endpoint is dropped to `COLD`. |
| `REQUEST_TIMEOUT_S` | `300` | Per-request read timeout for forwarded traffic (connect stays at 10s). |
| `MAX_BODY_BYTES` | `52428800` (50 MB) | Max forwarded request body; larger requests are rejected with HTTP `413` and never buffered past the cap. `0` = unlimited. |
| `PREWARM_TIMES` | *(empty = off)* | Comma-separated local times (`08:30,13:00`) at which the proxy warms the **active** model before you send traffic — in discovery mode this discovers/creates the pod, so the first request of the day is a warm hit. Each slot fires once per day (≤120 s after the time); uses the container's local time. |
| `PROXY_API_KEY` | *(empty)* | Shared secret for the proxy itself. When set, every route except `/_health` requires `x-proxy-key: <key>` or `Authorization: Bearer <key>`; the credential is stripped before forwarding. |
| `PORT` | `8080` | Local listen port (non-Docker only; the Docker build always listens on 8080). |
| `LOG_LEVEL` | `INFO` | Python logging level. |
| `LOG_FORMAT` | `text` | `text` (default) or `json` — one JSON object per line (`ts`, `level`, `logger`, `request_id`, `msg`, plus `exc` on exceptions) for log shippers such as Loki/ELK. |

## Operations

- `GET /_status` — current state + details (`COLD`, `WARMING`, `WARM`, `DEGRADED`).
  Always reports the `active_model` and the `model_switches` count. In pod mode
  it reports the resolved `pod_id`; discovery mode also reports `model` (the
  active model). When the GPU availability poller is active, pod mode also
  reports a `gpu_availability` block: last-known availability per tracked GPU
  type and per datacentre (plus a `models` map from each catalogue model to its
  GPUs), with `updated_at`, `age_s` and `last_error` — a failing poll makes the
  data stale, never blank.
- `POST /_warm` — warm on demand; `200` when up, `503` if it times out.
- `POST /_reload` — hot-reload the model catalogue from its configured source
  (`RUNPOD_MODELS_FILE`, else `RUNPOD_MODELS_JSON`). `200` with the new model
  list, `400` if the source is missing, unreadable, invalid, has no models, or
  would drop the configured default model — a rejected reload leaves the
  running catalogue untouched. The swap does not restart the pod or disturb
  in-flight requests: the active model keeps running, a dropped model simply
  stops being routable, and the default becomes the new catalogue's first
  entry. See [Hot-reloading the catalogue](#hot-reloading-the-catalogue).
- `GET /metrics` — Prometheus text exposition: current state, warmups,
  total/warm-hit/cold-hit/failed requests, keepalive failures, pod starts,
  pod creates, discoveries, model switches, and uptime. Discovery adds
  `runpod_proxy_pod_starts`, `runpod_proxy_pod_creates`, and
  `runpod_proxy_discoveries`. Per-request routing adds the
  `runpod_proxy_model_switches` counter and the `runpod_proxy_active_model`
  gauge (labelled with the active `model`). Observability adds a
  `runpod_proxy_request_duration_seconds` histogram (seconds to the
  upstream's response headers, forwarded requests) and a
  `runpod_proxy_requests_by_model{model=...}` counter. In pod mode with the
  GPU availability poller it adds `runpod_proxy_gpu_availability_age_s`
  (seconds since the last successful refresh; grows while polls fail) and
  `runpod_proxy_gpu_availability_level{gpu=...}` (last-known level: HIGH=3,
  MEDIUM=2, LOW=1, NONE=0; absent until the first successful refresh). A
  ready-made Grafana dashboard is at
  [`docs/grafana-dashboard.json`](docs/grafana-dashboard.json).
- `GET /_health` — liveness only (`{"ok": true}`); the one route that stays
  public when `PROXY_API_KEY` is set, so the container healthcheck works
  without embedding the key.
- `PREWARM_TIMES` slots log a `prewarm: HH:MM slot — warming the active model`
  line when they fire (and `already warm; nothing to do` when skipped).
- Logs show warmup attempts, keepalive ticks, and state transitions.
- Every request gets a correlation id: the proxy echoes an incoming
  `X-Request-Id` header when it is well-formed (1–128 chars), otherwise it
  generates one, and returns it in the `X-Request-Id` response header.  The
  id appears on every log line for that request (text format: `[<id>]`;
  JSON format: `request_id` field), so a client can correlate its own trace
  with the proxy's logs and any log shipper can group them.

### Securing the proxy

The proxy attaches your RunPod key to whatever it forwards, so anyone who can
reach its port can spend your credit. Set `PROXY_API_KEY` and pass it from the
client:

```bash
curl -s http://localhost:8080/_status -H "x-proxy-key: $PROXY_API_KEY"
```

For OpenAI-style clients that only speak `Authorization`, use
`Authorization: Bearer <PROXY_API_KEY>` instead — e.g. `COPILOT_PROVIDER_API_KEY`
for the Copilot CLI. The proxy strips its own credential before forwarding, so
it never reaches the upstream model server. Keep the port on localhost, a
tailnet, or a VPN regardless.

> The state machine lives in memory, so run a **single worker**
> (`uvicorn --workers 1`, the default). Multiple workers each keep their own
> state and would heartbeat and give up independently.

Troubleshooting:

- `503 endpoint warmup timeout` — no worker answered within `WARMUP_TIMEOUT_S`
  (deep queue, model too big for the GPU, out-of-memory crash loop). Check RunPod
  worker logs; raise `WARMUP_TIMEOUT_S` for big models.
- `503` with a "no longer exists on RunPod" message (pinned pod mode) —
  `RUNPOD_POD_ID` points at a deleted pod (the id is never reused). Point it at a
  live pod, or unset it and set `RUNPOD_MODEL_NAME` to use pod discovery.
- `503 endpoint warmup timeout` with log lines like
  `pod start returned HTTP 500: ... not enough free GPUs on the host machine`
  (pinned pod mode) — the pod's original host is out of GPUs, and a stopped
  pinned pod resumes onto the **same host**, so plain retries can never
  succeed. Set `RUNPOD_ON_MIGRATE=replace` to have the proxy terminate the pod
  and recreate it from the same template (network volume re-attached, so the
  model weights are not re-downloaded); afterwards point `RUNPOD_POD_ID` at
  the replacement's id.
- `503` in discovered pod mode with no matching pod — check `RUNPOD_MODEL_NAME`
  against pod/template names, images, and environment values (matching is by
  slug). Enable `RUNPOD_ALLOW_POD_CREATE=true` only if automatic billable GPU
  provisioning is intended; otherwise creation is skipped by design.
- `state=DEGRADED` — 3 consecutive keepalive failures (worker recycled or endpoint
  changed). The next real request re-warms automatically.
- `401/403` from serverless upstream — `RUNPOD_API_KEY` missing or wrong.
- `401/403` from a pod upstream — set `RUNPOD_UPSTREAM_API_KEY` to the model server's
  API key, or remove the server's API-key requirement. Do not use the RunPod management key.
- `401` from the proxy itself (`{"error":"unauthorized"}`) — `PROXY_API_KEY` is
  set but the client sent no or a wrong `x-proxy-key` / `Authorization: Bearer`.
- First token slow but eventual success — cold start; that's the queue, working as designed.

Security note: with `PROXY_API_KEY` unset the proxy has no auth — anyone who can
reach its port can use your endpoint (and the proxy attaches your RunPod key).
Set `PROXY_API_KEY` and keep it on localhost, a tailnet, or a VPN.

## Project layout

```
proxy/
  config.py     env-driven frozen Config
  models_config.py  declarative model catalogue parser/validator
  state.py      state machine + EndpointState
  lifecycle.py  backend lifecycle: serverless no-op / pod REST start-stop
  runpod_api.py RunPod REST client
  router.py     per-request model routing + discovery pod switching
  target.py      runtime-resolved upstream
  warmup.py     single-flight warmup with backoff/retry
  keepalive.py  activity-aware heartbeat loop
  prewarm.py    scheduled pre-warm (PREWARM_TIMES) before workday traffic
  gpu_availability.py background RunPod GPU availability poller (last-known values + staleness)
  main.py       FastAPI app: /_status, /_warm, /_reload, /metrics, /_health, catch-all streaming proxy
tests/          264 unit tests (ASGI in-process, mocked upstream + REST API),
                including test_model_routing.py, test_models_config.py,
                test_catalogue_discovery.py, test_pod_replace.py, and test_round2.py
Dockerfile      python:3.12-slim, non-root
docker-compose.yml
.env.example
```

Run the tests: `python -m pytest tests/ -q`

## Enhancement ideas (once basics work)

Done so far: max request body size guard (`MAX_BODY_BYTES`, 413 above the
cap), the pre-warm schedule (`PREWARM_TIMES`), catalogue hot-reload
(`POST /_reload`), per-model GPU datacentre pinning in the catalogue, and
the observability upgrade — `X-Request-Id` correlation on every request and
log line, optional `LOG_FORMAT=json`, a request-duration histogram, per-model
request counters, and a ready-made Grafana dashboard
(`docs/grafana-dashboard.json`), RunPod GPU availability polling in pod mode
(`GPU_AVAILABILITY_INTERVAL_S`) with last-known values and staleness surfaced
in `/_status` and `/metrics`, and startup hardening — pod-mode readiness now
requires the target model to be listed (`POD_HEALTH_MODE=model` default, with
`completion` available) instead of trusting "any HTTP response", warmup progress
logs elapsed/remaining budget while a model loads, and pod-deleted fail-fast — a
pinned `RUNPOD_POD_ID` that is gone on RunPod (`GET /pods/{id}` → 404) now fails
the warmup immediately with an actionable 503 instead of burning
`WARMUP_TIMEOUT_S`, and `stop()` treats a deleted pod as already stopped.
Remaining (parked pending a follow-up questionnaire):

- Multi-endpoint routing: per-user or per-repo routing of one proxy to many
  serverless endpoints
- A pod pool for 3+ concurrently-switched models (one pod per model instead
  of stop/start on every switch)
- Queue position surfacing via RunPod API in `/_status`
