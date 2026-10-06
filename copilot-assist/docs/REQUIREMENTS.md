# Copilot Assist — Model Routing Proxy

**Requirements Document** · Status: Ready for implementation · Language: Rust

---

## 1. Overview

### 1.1 Problem

The GitHub Copilot CLI offers two mutually exclusive model sources:

- **Copilot (GitHub subscription) models** — the default. Routed to GitHub's endpoints, billed as AI credits.
- **BYOK (Bring Your Own Key) models** — configured via `COPILOT_PROVIDER_*` env vars. When active, routing is **all-or-nothing**: *every* request goes to the custom provider and GitHub auth is bypassed.

There is **no way to mix them**. You cannot, for example, use Copilot's GPT-5.4 for most work while routing one specific model (a local Llama, a vLLM endpoint, an OpenRouter key, a corporate OpenAI-compatible gateway) to your own provider — all within the same, unmodified CLI.

### 1.2 Goal

Build a **Rust** tool that intercepts the Copilot CLI's model requests/responses and:

- When the requested model matches a **user-configured model→provider mapping** (an OpenAI-compatible provider: endpoint + key + model name), direct the request/response **to/from that provider**.
- Otherwise, let **normal Copilot processing continue** (request/response to/from Copilot's endpoint).

The tool must work with the **unmodified interactive Copilot CLI (TUI)** and the non-interactive (`-p`) mode.

### 1.3 Chosen Approach — Local MITM Proxy

A **local man-in-the-middle (MITM) proxy** in Rust that:

1. Sits between the CLI and `api.business.githubcopilot.com`, engaged via `HTTPS_PROXY` + `NODE_EXTRA_CA_CERTS`.
2. Intercepts `GET /models` and **injects** the user's custom models into the model list.
3. Intercepts inference requests (`POST /responses`, …) and **routes** matched models to the custom provider, **translating** the wire format.
4. **Passes through** everything else unchanged (OAuth, telemetry, MCP, non-matched models).

**Why MITM (and not the alternatives):**

| Option | Verdict |
|--------|---------|
| **A. MITM proxy (chosen)** | Works with the unmodified TUI. The CLI already honors `HTTPS_PROXY` and `NODE_EXTRA_CA_CERTS`, and TLS pinning is relaxed when an extra CA is configured (**verified live**). No CLI modification. |
| B. Native SDK multi-provider (`SessionOpenOptions.providers`/`models`) | The Rust runtime *does* support named providers with per-model routing, but **only via the SDK protocol** — the interactive TUI never passes `providers`/`models`. Would require building a custom UI (loses the TUI). |
| C. Patch `app.js` to pass `providers`/`models` | Fragile: overwritten on every CLI update; requires deep SDK-protocol knowledge. Rejected. |
| D. Built-in BYOK env vars | All-or-nothing; cannot mix with Copilot models. Rejected. |

---

## 2. Research Findings (all verified by live capture)

### 2.1 CLI Architecture

- `copilot.exe` = Node.js v24 **SEA** (Single Executable Application).
- `app.js` (~7.7 MB) = Node.js **Ink TUI** — UI layer only.
- `prebuilds/win32-x64/runtime.node` (~87 MB) = **Rust** native agent runtime (tokio / hyper / rustls) — **performs all model inference**.
- The Rust runtime is the component that talks to the model endpoints and is the one that honors proxy/TLS env vars.

### 2.2 Endpoints

Base URL: **`https://api.business.githubcopilot.com`** (overridable via `COPILOT_API_URL`).

| Endpoint | Method | Purpose |
|----------|--------|---------|
| `/models` | GET | Model list (≈42 KB JSON) |
| `/responses` | POST | **Inference** — OpenAI Responses API, SSE stream |
| `/responses` | GET | WebSocket upgrade (`101 Switching Protocols`) — `ws:` transport |
| `api.github.com/copilot_internal/user` | GET | OAuth user login |
| `api.github.com/copilot/mcp_registry` | GET | MCP server registry |
| `telemetry.business.githubcopilot.com` | POST | Product telemetry |
| `dc.services.visualstudio.com` | POST | App Insights telemetry |

### 2.3 Auth & Headers

- GitHub OAuth (MSAL) + HMAC key derived from the user ID.
- Notable request headers: `authorization` (Bearer), `x-github-api-version: 2026-08-01`, `user-agent: copilot/1.0.86 (…)`, `x-client-machine-id`, `x-client-session-id`, `x-interaction-id`, `copilot-integration-id: copilot-developer-cli`, `copilot-harness-id: copilot-sdk`, `openai-intent: conversation-agent`, `x-stainless-helper-method: stream`.

### 2.4 Proxy / TLS (the MITM enabler)

- The Rust runtime honors `HTTP_PROXY` / `HTTPS_PROXY` / `NO_PROXY` (case-insensitive) and the `proxyUrl` setting.
- The Rust runtime loads extra trust roots from `NODE_EXTRA_CA_CERTS`, `SSL_CERT_FILE`, `CURL_CA_BUNDLE`.
- **Verified live:** a full model turn completed through a local MITM proxy with **no certificate-pinning failure** when our CA was supplied via `NODE_EXTRA_CA_CERTS`.
- The CLI uses **HTTP/1.1** (not HTTP/2) with `Transfer-Encoding: chunked` for responses.

### 2.5 Wire Format

#### 2.5.1 Model list — `GET /models`

Response body: `{"data": [Model, …]}`. Each `Model`:

```jsonc
{
  "id": "gpt-5.4-mini",                 // selection id; appears in inference request `model`
  "name": "GPT-5.4 mini",               // display name
  "vendor": "OpenAI",                   // OpenAI | Anthropic | Google
  "version": "gpt-5.4-mini",
  "capabilities": {
    "family": "gpt-5.4-mini",
    "limits": {
      "max_context_window_tokens": 400000,
      "max_output_tokens": 128000,
      "max_prompt_tokens": 272000,
      "vision": { "max_prompt_images": 1, "supported_media_types": ["image/png", "…"] }
    },
    "supports": {
      "streaming": true, "tool_calls": true, "vision": true,
      "parallel_tool_calls": true, "structured_outputs": true,
      "reasoning_effort": ["none","low","medium","high","xhigh"]
    },
    "tokenizer": "o200k_base", "type": "chat"
  },
  "billing": { "token_prices": { "…": "…" }, "restricted_to": ["pro","…"] },
  "model_picker_category": "lightweight",   // powerful | versatile | lightweight
  "model_picker_enabled": true,
  "model_picker_price_category": "low",     // low | medium | high
  "is_chat_default": false,
  "is_chat_fallback": false,
  "preview": false,
  "supported_endpoints": ["/responses","ws:/responses"],  // wire APIs this model accepts
  "policy": { "state": "enabled", "terms": "…" }
}
```

`supported_endpoints` reveals the wire API per vendor: OpenAI → `["/responses","ws:/responses"]` (and `/chat/completions` for some), Anthropic → `["/v1/messages","/chat/completions"]`, Google → `["/chat/completions"]`.

#### 2.5.2 Inference request — `POST /responses` (OpenAI **Responses** API)

```jsonc
{
  "model": "gpt-5.4-mini",
  "instructions": "<system prompt, ~44 KB>",
  "input": [
    { "role": "user",
      "content": [ { "type": "input_text", "text": "<current_datetime>…</current_datetime>\n\n<user prompt>" } ],
      "type": "message" }
    // later turns add assistant messages, function_call, function_call_output items
  ],
  "tools": [
    { "name": "powershell", "description": "…", "parameters": { "type":"object", "properties": { "…":"…" } },
      "strict": true, "type": "function" }
    // 17 tools: powershell, read_powershell, stop_powershell, list_powershell, apply_patch,
    //           view, web_fetch, fetch_copilot_cli_documentation, skill, sql, session_store_sql,
    //           read_agent, list_agents, write_agent, rg, glob, task
  ],
  "reasoning": { "effort": "medium", "summary": "auto" },
  "store": false,
  "stream": true,
  "include": ["reasoning.encrypted_content"],
  "parallel_tool_calls": true
}
```

#### 2.5.3 Inference response — SSE stream

Observed event sequence for a simple text answer:

| # | event | data shape |
|---|-------|-----------|
| 1 | `response.created` | `{"response": {…}}` |
| 2 | `response.in_progress` | `{"response": {…}}` |
| 3 | `response.output_item.added` | `{"item": {…}}` — reasoning item (has `encrypted_content`) |
| 4 | `response.output_item.done` | `{"item": {…}}` — reasoning item |
| 5 | `response.output_item.added` | `{"item": {…}}` — message item |
| 6 | `response.content_part.added` | `{"content_index":0,"item_id":"…","part":{…}}` |
| 7 | `response.output_text.delta` | `{"content_index":0,"delta":"P","item_id":"…"}` |
| 8 | `response.output_text.delta` | `{"content_index":0,"delta":"ONG","item_id":"…"}` |
| 9 | `response.output_text.done` | `{"content_index":0,"item_id":"…","text":"PONG"}` |
| 10 | `response.content_part.done` | `{"content_index":0,"item_id":"…","part":{…}}` |
| 11 | `response.output_item.done` | `{"item": {…}}` — message item |
| 12 | `response.completed` | `{"response": {…}, "copilot_usage": {…}}` |

Tool-call turns additionally emit `response.function_call_arguments.delta` / `.done` and `function_call` output items.

**Copilot-specific:** `response.completed` carries a `copilot_usage` object:

```jsonc
"copilot_usage": {
  "token_details": [
    { "batch_size":1000000, "cost_per_batch":75000000000, "model":"gpt-5.4-mini", "token_count":15722, "token_type":"input" },
    { "…": "…", "token_type":"cache_read" },
    { "…": "…", "token_type":"cache_write" },
    { "…": "…", "token_type":"output" }
  ]
}
```

The CLI uses this to render **AI Credits** and token stats.

---

## 3. Functional Requirements

- **FR-1 · Model-list injection.** Intercept `GET /models` responses and append the user's configured custom models to `data`. Each injected model must carry a valid `id`, `name`, `capabilities`, `supported_endpoints`, and `model_picker_enabled: true`. The injected `id` is what the user selects and what later appears in the inference request's `model` field.

- **FR-2 · Request routing.** Intercept inference requests (`POST /responses`, and any other inference endpoint the CLI uses). Parse `model` from the body. If it matches a configured model→provider mapping, route to that provider; otherwise forward to the real Copilot endpoint **unchanged**.

- **FR-3 · Wire-format translation.** Translate the Copilot Responses API request into the provider's OpenAI-compatible format (`/chat/completions` by default, `/responses` if the provider supports it) and translate the provider's response back into the Copilot Responses API **SSE** format. Must support **text completion**, **tool calls (function calling)**, and **streaming**. Must populate `copilot_usage` in `response.completed` (from the provider's `usage`, or a safe default).

- **FR-4 · Pass-through.** All non-matched traffic (OAuth, telemetry, MCP, non-matched models) is forwarded to the real endpoint byte-for-byte. The proxy must be transparent for pass-through.

- **FR-5 · Configuration.** A config file (TOML) defining providers (name, base URL, API key, wire API), models (CLI-visible `id`, target `provider`, `wire_model`, display `name`, token limits), and proxy (listen address, CA paths).

- **FR-6 · Local-only.** Bind to `127.0.0.1` only. CA and leaf certs are local artifacts.

- **FR-7 · Observability.** Optional request/response logging (off by default) with `authorization` and API keys redacted. A concise per-request log line (model, route decision, status, latency) on by default.

---

## 4. Configuration Schema

```toml
[proxy]
listen      = "127.0.0.1:8931"
ca_cert     = "certs/ca.pem"     # our MITM CA (PEM)
ca_key      = "certs/ca.key"
upstream    = "https://api.business.githubcopilot.com"   # real Copilot base URL
log         = "off"              # off | summary | full

[[provider]]
name     = "my-local"
base_url = "http://127.0.0.1:11434/v1"   # OpenAI-compatible endpoint
api_key  = "sk-…"                        # or { env = "MY_KEY" }
wire_api = "chat"                         # "chat" (/chat/completions) | "responses" (/responses)

[[model]]
id              = "my-local-model"        # id shown in the CLI model picker
provider        = "my-local"              # route to this provider
wire_model      = "llama3.1:8b"           # actual model name sent to the provider
name            = "My Local Model"        # display name
max_prompt_tokens  = 128000
max_output_tokens  = 8192
```

A model may also **shadow** an existing Copilot model id (e.g. `id = "gpt-5.4-mini"`) to redirect that id to a custom provider; the routing decision is purely "does `model` match a configured mapping".

---

## 5. Wire-Format Translation Spec

### 5.1 Request: Copilot Responses API → provider `chat/completions`

| Copilot field | → chat/completions |
|---------------|--------------------|
| `instructions` | `messages[0] = {"role":"system","content": instructions}` |
| `input[]` (message) | `messages[]` — `{"type":"message","role":R,"content":[{"type":"input_text","text":T}]}` → `{"role":R,"content":T}` |
| `input[]` (function_call) | `{"role":"assistant","content":null,"tool_calls":[{"id":call_id,"type":"function","function":{"name":name,"arguments":arguments}}]}` |
| `input[]` (function_call_output) | `{"role":"tool","tool_call_id":call_id,"content":output}` |
| `tools[]` | `tools[]` — nest each tool's `name/description/parameters/strict` under a `function` key |
| `model` | `wire_model` (from config) |
| `stream` | `stream` |
| `parallel_tool_calls` | `parallel_tool_calls` |
| `reasoning`, `store`, `include` | dropped (or mapped to provider-specific extras if supported) |

### 5.2 Response: provider `chat/completions` SSE → Copilot Responses API SSE

Reconstruct the full Responses API event sequence from `chat.completion.chunk` events:

1. `response.created` → `response.in_progress`
2. Per output item:
   - `response.output_item.added`
   - **text**: `response.content_part.added` → `response.output_text.delta` (per chunk) → `response.output_text.done` → `response.content_part.done`
   - **tool call**: `response.function_call_arguments.delta` (per chunk) → `response.function_call_arguments.done`
   - `response.output_item.done`
3. `response.completed` with `copilot_usage` built from the provider's `usage` (`prompt_tokens`→input, `completion_tokens`→output; cache fields zero).

`id` / `item_id` / `response.id` values may be generated (e.g. random base64url). The reasoning item may be omitted if the provider returns no reasoning (see R-3).

---

## 6. Security Considerations

- **Local-only binding** (`127.0.0.1`); no external exposure.
- **CA handling:** the MITM CA is a local artifact, clearly labeled, **never committed** to source control. The user opts in by pointing `NODE_EXTRA_CA_CERTS` at it.
- **No secret leakage:** the provider API key is read from config/env and sent only to the configured provider; it is never logged. The upstream `authorization` header is passed through to GitHub only and redacted in any logs.
- **Upstream trust:** the proxy verifies the real GitHub certificate using the system CA bundle on the upstream leg (no `danger_accept_invalid_certs`).
- **Blast radius:** because the proxy can see all Copilot traffic, it must run only on the user's machine and bind to loopback.

---

## 7. Risks / Open Questions

| # | Risk / Question | Mitigation |
|---|-----------------|------------|
| R-1 | **Toolchain:** `rustls`+`ring` needs a C compiler (gcc/cl), which is not currently installed. | (a) Install a C toolchain (MSVC Build Tools or TDM-GCC), or (b) use `native-tls` (SChannel) + `rcgen` (pure-Rust cert gen) to avoid a C compiler. Decide in Phase 1. |
| R-2 | **`copilot_usage`:** the CLI may require it in `response.completed`. | Populate it from provider `usage`; verify the CLI tolerates zero/absent values. |
| R-3 | **Reasoning / `encrypted_content`:** Copilot responses include encrypted reasoning a custom provider can't produce. | Omit the reasoning item; verify the CLI tolerates its absence. |
| R-4 | **WebSocket transport:** the CLI can use `ws:/responses`. | The captured session used HTTP POST; handle WebSocket upgrades only if/when the CLI selects that transport. |
| R-5 | **HTTP/2:** CLI currently uses HTTP/1.1. | Build the proxy as a byte-level tunnel so it is version-agnostic; parse HTTP/1.1 for routing decisions. |
| R-6 | **Model-picker behavior** for injected models. | Verify in Phase 2 that injected models appear and are selectable. |
| R-7 | **Multi-turn + tool calls** in `input`. | Cover with the translation spec (§5.1) and integration tests. |

---

## 8. Verification Plan

1. **Unit tests** — wire-format translation (request and response) against captured fixtures.
2. **Integration test** — run the CLI through the proxy against a **mock OpenAI-compatible server**; assert: custom model appears in the list; selecting it routes to the mock; the response is translated and displayed; tool calls round-trip; non-matched models still hit Copilot.
3. **Live test** — run the CLI through the proxy against a real provider (Ollama / vLLM / OpenRouter) end-to-end.

---

## 9. Implementation Plan (Phased)

- **Phase 0 — Research & capture (done).** Mapped architecture, endpoints, auth, proxy/TLS, and captured the exact wire format.
- **Phase 1 — Proxy skeleton.** Rust TCP server + TLS termination + transparent pass-through. *Gate: CLI works normally through the proxy.*
- **Phase 2 — Model-list injection.** *Gate: custom model appears in the CLI picker.*
- **Phase 3 — Routing + text translation.** *Gate: a simple prompt on the custom model routes to the provider and displays correctly.*
- **Phase 4 — Tool-call support.** *Gate: CLI tools (view/powershell/edit) work through the provider.*
- **Phase 5 — Config, security, logging, error handling.**
- **Phase 6 — Docs, packaging, full verification.**

---

## Appendix A — Evidence

- Live captures: `capture-proxy/captures/` (model list, inference request/response, OAuth, MCP).
- Capture harness: `capture-proxy/capture.py` (Python MITM proxy, port 8931) + `certs/ca.{pem,key}`.
- Analysis scripts: `capture-proxy/analyze.py`, `capture-proxy/analyze2.py`.
- Rust runtime binary: `%LOCALAPPDATA%\copilot\pkg\win32-x64\1.0.86\prebuilds\win32-x64\runtime.node`.
- SDK schema: `%LOCALAPPDATA%\copilot\pkg\win32-x64\1.0.86\schemas\api.schema.json`.
