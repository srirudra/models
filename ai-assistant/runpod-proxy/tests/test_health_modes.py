"""POD_HEALTH_MODE readiness bars: any / model / completion.

The production default is "model" — the warmup route's JSON must list the
target model — so readiness does not depend on one server's (vLLM's)
early-refusal behavior of not answering its routes until weights are loaded.
These tests cover the classifier directly, the config defaults, and an
end-to-end pod-mode warmup that stays not-ready until the model appears in
the list (the exact failure the default change hardens against).
"""
import json

import httpx
import pytest

from proxy.config import Config
from proxy.health import classify, completion_url
from proxy.state import State

REST_HOST = "rest.runpod.io"
POD_ID = "testpod"
POD_PORT = 8000
API_KEY = "rk-test-key"
POD_HOST = f"{POD_ID}-{POD_PORT}.proxy.runpod.net"
MODEL = "model-a"


def _resp(status_code: int, **kwargs) -> httpx.Response:
    return httpx.Response(status_code, request=httpx.Request("GET", "http://x/v1/models"), **kwargs)


async def _classify(mode: str, response: httpx.Response, *, pod_mode: bool = True, model: str = MODEL,
                    client: httpx.AsyncClient | None = None) -> tuple[bool, str]:
    return await classify(
        client or httpx.AsyncClient(), mode=mode, pod_mode=pod_mode, response=response,
        base_url="http://x", warmup_path="v1/models", model=model, headers={}, timeout=1.0,
    )


# --- mode "any" (legacy) -----------------------------------------------------

async def test_any_accepts_real_response():
    healthy, reason = await _classify("any", _resp(200, json={"upstream": True}))
    assert healthy and reason == "HTTP 200"


async def test_any_pod_mode_rejects_edge_synthetics():
    for status in (404, 502, 503, 504):
        healthy, reason = await _classify("any", _resp(status))
        assert not healthy
        assert "not ready" in reason


async def test_any_serverless_accepts_edge_statuses_as_genuine():
    # In serverless mode a 503 is a real answer from the endpoint, not a
    # RunPod edge synthetic.
    healthy, _ = await _classify("any", _resp(503), pod_mode=False)
    assert healthy


# --- mode "model" (new default) ---------------------------------------------

async def test_model_requires_target_listed():
    healthy, reason = await _classify("model", _resp(200, json={"data": [{"id": MODEL}]}))
    assert healthy and reason == "model listed"


@pytest.mark.parametrize("listed", [
    MODEL.upper(),  # case-insensitive
    MODEL.replace("-", " "),  # slug match
])
async def test_model_matches_case_and_slug(listed):
    healthy, _ = await _classify("model", _resp(200, json={"data": [{"id": listed}]}))
    assert healthy


async def test_model_rejects_empty_list():
    healthy, reason = await _classify("model", _resp(200, json={"data": []}))
    assert not healthy
    assert "not yet" in reason


async def test_model_rejects_other_model():
    healthy, reason = await _classify("model", _resp(200, json={"data": [{"id": "other"}]}))
    assert not healthy
    assert "not yet" in reason


async def test_model_rejects_not_ready_and_non_json():
    healthy, _ = await _classify("model", _resp(503))
    assert not healthy
    healthy, reason = await _classify("model", _resp(200, text="not json"))
    assert not healthy
    assert "JSON" in reason


async def test_model_without_target_accepts_any_listed_model():
    healthy, reason = await _classify("model", _resp(200, json={"data": [{"id": "whatever"}]}), model="")
    assert healthy and "1 model" in reason


# --- mode "completion" -------------------------------------------------------

async def _completion_classify(completion_status: int, completion_body: dict) -> tuple[bool, str, httpx.Request | None]:
    seen: dict = {}

    def handler(request: httpx.Request) -> httpx.Response:
        seen["request"] = request
        return httpx.Response(completion_status, json=completion_body)

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        healthy, reason = await classify(
            client, mode="completion", pod_mode=True, response=_resp(200, json={"data": []}),
            base_url="http://x", warmup_path="v1/models", model=MODEL, headers={}, timeout=1.0,
        )
    return healthy, reason, seen.get("request")


async def test_completion_requires_successful_inference():
    healthy, reason, request = await _completion_classify(
        200, {"choices": [{"message": {"content": "ok"}}]},
    )
    assert healthy and reason == "completion ok"
    assert request is not None
    assert request.url == "http://x/v1/chat/completions"
    assert request.method == "POST"
    body = json.loads(request.content)
    assert body["model"] == MODEL
    assert body["max_tokens"] == 1


async def test_completion_rejects_failed_probe():
    healthy, reason, _ = await _completion_classify(500, {"error": "engine still loading"})
    assert not healthy
    assert "HTTP 500" in reason


async def test_completion_rejects_missing_choices():
    healthy, reason, _ = await _completion_classify(200, {"id": "x"})
    assert not healthy
    assert "choices" in reason


async def test_completion_pod_mode_skips_probe_on_edge_synthetic():
    # A RunPod edge 503 means the pod is still booting: no completion POST
    # should even be attempted (it would queue behind the loading engine).
    def handler(request: httpx.Request) -> httpx.Response:
        raise AssertionError("completion probe must not run on edge synthetic")

    async with httpx.AsyncClient(transport=httpx.MockTransport(handler)) as client:
        healthy, reason = await classify(
            client, mode="completion", pod_mode=True, response=_resp(503),
            base_url="http://x", warmup_path="v1/models", model=MODEL, headers={}, timeout=1.0,
        )
    assert not healthy
    assert "not ready" in reason


def test_completion_url_derivation():
    assert completion_url("http://p", "v1/models") == "http://p/v1/chat/completions"
    assert completion_url("http://p/", "models") == "http://p/chat/completions"
    assert completion_url("http://p", "openai/v1/models") == "http://p/openai/v1/chat/completions"


# --- config defaults ---------------------------------------------------------

def test_default_health_mode_is_model():
    assert Config(serverless_url="http://x").pod_health_mode == "model"


def test_from_env_defaults_to_model(monkeypatch):
    monkeypatch.delenv("POD_HEALTH_MODE", raising=False)
    assert Config.from_env().pod_health_mode == "model"


def test_from_env_honours_explicit_mode(monkeypatch):
    monkeypatch.setenv("POD_HEALTH_MODE", "completion")
    assert Config.from_env().pod_health_mode == "completion"


# --- end-to-end: pod-mode warmup under the "model" bar ------------------------

def _pod_handler(flips_after: int):
    """Pod warmup route lists the model only after ``flips_after`` empty
    answers (simulating a server that answers HTTP before weights load)."""
    state = {"probes": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={"id": POD_ID})
        if request.url.path == "/v1/models":
            state["probes"] += 1
            if state["probes"] <= flips_after:
                return httpx.Response(200, json={"data": []})
            return httpx.Response(200, json={"data": [{"id": MODEL}]})
        return httpx.Response(200, json={"upstream": True})

    return handler, state


async def test_pod_warmup_waits_until_model_listed(make_app):
    handler, state = _pod_handler(flips_after=3)
    app, proxy, _ = make_app(
        handler, mode="pod", pod_id=POD_ID, pod_port=POD_PORT, api_key=API_KEY,
        model_name=MODEL, pod_health_mode="model",
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={"prompt": "hello"})
    assert r.status_code == 200
    assert proxy.state.state is State.WARM
    # Three empty answers must have been treated as not-ready; the fourth
    # (model listed) ended the warmup.
    assert state["probes"] >= 4


async def test_pod_warmup_times_out_when_model_never_listed(make_app):
    handler, state = _pod_handler(flips_after=10_000)
    app, proxy, _ = make_app(
        handler, mode="pod", pod_id=POD_ID, pod_port=POD_PORT, api_key=API_KEY,
        model_name=MODEL, pod_health_mode="model",
        warmup_timeout_s=0.8, warmup_backoff_max_s=0.2,
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={"prompt": "hello"}, timeout=30.0)
    assert r.status_code == 503
    assert proxy.state.state is State.COLD
    assert state["probes"] >= 1
