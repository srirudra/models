"""Simulated RunPod serverless vLLM endpoint for local demos and testing.

Runs on the host (no Docker) so the proxy container can reach it via
http://host.docker.internal:9999/v1. No GPU or RunPod account needed.

Routes (under /v1, mirroring a real vLLM/OpenAI-compatible endpoint):
  POST /initialize        warmup/keepalive hook (accepts the proxy's pings)
  GET  /models            OpenAI-style model list
  POST /chat/completions  OpenAI-style; stream=true -> SSE, else JSON.
                          The non-streaming reply echoes the Authorization
                          header it received, proving the proxy injected
                          RUNPOD_API_KEY.

The FIRST /v1/* request simulates a cold start (worker boot + model load)
of COLD_START_SECONDS - like a real scale-from-zero serverless endpoint.
Every request is logged (stdout + demo/mock-upstream.log) with its
Authorization header and response status.
"""

import asyncio
import json
import os
import time

import uvicorn
from fastapi import FastAPI, Request
from fastapi.responses import StreamingResponse

PORT = int(os.environ.get("MOCK_PORT", "9999"))
COLD_START_S = float(os.environ.get("COLD_START_SECONDS", "3"))
LOG_PATH = os.path.join(os.path.dirname(os.path.abspath(__file__)), "mock-upstream.log")

app = FastAPI()
state = {"booted": False}


def log(line: str) -> None:
    stamped = f"[{time.strftime('%H:%M:%S')}] {line}"
    print(stamped, flush=True)
    with open(LOG_PATH, "a", encoding="utf-8") as f:
        f.write(stamped + "\n")


@app.middleware("http")
async def cold_start_and_log(request: Request, call_next):
    kind = ""
    if request.url.path.startswith("/v1/") and not state["booted"]:
        await asyncio.sleep(COLD_START_S)  # simulate worker boot + model load
        state["booted"] = True
        kind = " [cold-start]"
    response = await call_next(request)
    auth = request.headers.get("authorization", "-")
    log(f"{request.method} {request.url.path} auth={auth} -> {response.status_code}{kind}")
    return response


@app.on_event("startup")
async def _startup() -> None:
    open(LOG_PATH, "w", encoding="utf-8").close()  # fresh log per run
    print(f"mock upstream on :{PORT} (cold start simulation: {COLD_START_S}s)", flush=True)


@app.get("/")
async def root():
    return {"ok": True}


@app.post("/v1/initialize")
async def initialize():
    return {"status": "ok", "model": "demo-qwen-7b"}


@app.get("/v1/models")
async def models():
    # Also the proxy warmup/keepalive probe target (GET). 200 = "worker is up".
    return {"object": "list", "data": [{"id": "demo-qwen-7b", "object": "model"}]}


@app.post("/v1/chat/completions")
async def chat(request: Request):
    body = await request.json()
    model = body.get("model", "demo-qwen-7b")
    if body.get("stream"):
        words = ["Hello", " from", " the", " mock", " vLLM", " endpoint", "!"]

        async def gen():
            for i, word in enumerate(words):
                chunk = {
                    "id": f"demo-{i}",
                    "object": "chat.completion.chunk",
                    "model": model,
                    "choices": [{"index": 0, "delta": {"content": word}}],
                }
                yield f"data: {json.dumps(chunk)}\n\n"
            # OpenAI protocol: final chunk carries finish_reason (required by strict clients)
            final = {
                "id": f"demo-{len(words)}",
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            }
            yield f"data: {json.dumps(final)}\n\n"
            yield "data: [DONE]\n\n"

        return StreamingResponse(gen(), media_type="text/event-stream")
    auth = request.headers.get("authorization", "-")
    return {
        "id": "demo-1",
        "object": "chat.completion",
        "model": model,
        "choices": [{
            "index": 0,
            "message": {"role": "assistant",
                        "content": f"Hello from the mock vLLM endpoint! (saw auth: {auth})"},
            "finish_reason": "stop",
        }],
        "usage": {"prompt_tokens": 5, "completion_tokens": 9, "total_tokens": 14},
    }


if __name__ == "__main__":
    uvicorn.run(app, host="127.0.0.1", port=PORT, log_level="warning")
