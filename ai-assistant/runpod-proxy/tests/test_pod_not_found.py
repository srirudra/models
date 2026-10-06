"""Fail-fast when a pinned RUNPOD_POD_ID no longer exists on RunPod.

A deleted pod's id is never reused, so a warmup that discovers the pod is
gone (GET /pods/{id} -> 404) must fail fast with an actionable 503 instead
of burning the whole WARMUP_TIMEOUT_S on doomed starts.
"""
import time

import httpx
import pytest

from proxy.lifecycle import PodNotFoundError
from proxy.state import State
from tests.test_pod_mode import (
    POD_ID, REST_HOST, START_PATH, STOP_PATH, _pod_app, rest_calls,
)

POD_GET_PATH = f"/v1/pods/{POD_ID}"


def pod_gone_handler(request: httpx.Request) -> httpx.Response:
    """REST host: 404 for GET /v1/pods/{id}; pod host: dead (502)."""
    if request.url.host == REST_HOST:
        if request.method == "GET" and request.url.path == POD_GET_PATH:
            return httpx.Response(404, json={"message": "pod not found"})
        return httpx.Response(200, json={"id": POD_ID})
    return httpx.Response(502)


async def test_start_raises_pod_not_found_on_404(make_app):
    app, proxy, rec = _pod_app(make_app, pod_gone_handler)
    with pytest.raises(PodNotFoundError):
        await proxy.lifecycle.start(budget=5.0)
    # No REST start is ever attempted for a pod that no longer exists.
    assert rest_calls(rec, START_PATH) == []


async def test_stop_is_noop_when_pod_gone(make_app):
    app, proxy, rec = _pod_app(make_app, pod_gone_handler)
    await proxy.lifecycle.stop()  # must not raise
    assert rest_calls(rec, STOP_PATH) == []


async def test_start_still_skips_when_pod_is_running(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            if request.method == "GET" and request.url.path == POD_GET_PATH:
                return httpx.Response(
                    200, json={"id": POD_ID, "desiredStatus": "RUNNING"}
                )
            return httpx.Response(200, json={"id": POD_ID})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = _pod_app(make_app, handler)
    await proxy.lifecycle.start(budget=5.0)  # running: nothing to do
    assert rest_calls(rec, START_PATH) == []


async def test_request_fails_fast_with_actionable_503(make_app):
    app, proxy, rec = _pod_app(make_app, pod_gone_handler, warmup_timeout_s=30.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        started = time.monotonic()
        r = await ac.post("/", json={"prompt": "hello"})
        elapsed = time.monotonic() - started
    assert r.status_code == 503
    error = r.json()["error"]
    assert POD_ID in error
    assert "RUNPOD_POD_ID" in error
    # Fail fast: the 30s warmup budget must not be burned on doomed starts.
    assert elapsed < 5.0
    assert proxy.state.state is State.COLD
    assert rest_calls(rec, START_PATH) == []


async def test_second_request_also_fails_fast(make_app):
    app, proxy, rec = _pod_app(make_app, pod_gone_handler, warmup_timeout_s=30.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r1 = await ac.post("/", json={"prompt": "a"})
        r2 = await ac.post("/", json={"prompt": "b"})
    assert (r1.status_code, r2.status_code) == (503, 503)
    assert "RUNPOD_POD_ID" in r2.json()["error"]
    assert proxy.state.state is State.COLD
