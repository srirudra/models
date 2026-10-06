# runpod-proxy (Rust)

Rust reimplementation of the Python [runpod-proxy](../ai-assistant/runpod-proxy) —
a warm proxy for RunPod serverless endpoints and GPU pods. It is a **parallel,
drop-in replacement**: same client contract (Copilot CLI / OpenAI-compatible
SDKs), same env surface, same control-plane routes, same cost-safety invariants.

## Status

PBI **RUST-001** — see `docs/engineering/pbis/RUST-001/` (plan, status ledger).
The normative contract is `docs/specification.md` (copied, version-pinned from
the Python repo); its section 14 porting checklist is the acceptance gate.

## Layout

- `crates/runpod-proxy/` — the proxy (single binary).
- `demo/mock_upstream/` — local mock of a serverless endpoint for demos/tests.
- `Dockerfile` — multi-stage (cargo-chef, musl static, distroless runtime).
- `docker-compose.yml` — same shape as the Python service.
- `.env.example` — full env surface (same variables as the Python proxy).
- `models.example.yaml` / `models.example.json` — model catalogue examples.

## Develop

```sh
cargo build            # compile
cargo clippy --all-targets
cargo test             # unit + integration
cargo run -p runpod-proxy   # local run (PORT=8080 default)
```

Toolchain: stable Rust (edition 2024, MSRV 1.85) — see `rust-toolchain.toml`.
On Windows without MSVC build tools, use the GNU toolchain
(`rustup default stable-x86_64-pc-windows-gnu`); the production image is built
on Linux (musl) regardless.

## Run (Docker)

```sh
cp .env.example .env   # fill in RUNPOD_SERVERLESS_URL / RUNPOD_API_KEY
docker compose up -d --build
curl http://localhost:8080/_health   # {"ok":true}
```
