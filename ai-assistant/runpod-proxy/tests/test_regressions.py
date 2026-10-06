"""Regression tests for warmup robustness, pod idempotency and timeout wiring."""
import asyncio

import httpx

from proxy.main import CONNECT_TIMEOUT_S
from proxy.state import State
from tests.conftest import ok_handler
from tests.test_pod_mode import POD_ID, POD_HOST, REST_HOST, START_PATH, STOP_PATH, _pod_app, rest_calls

POD_PATH = f"/v1/pods/{POD_ID}"


async def test_unexpected_warmup_error_fails_fast_instead_of_hanging(make_app):
    """A non-HTTP exception must settle the shared future, not brick the proxy."""
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            raise ValueError("boom-not-an-http-error")
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = make_app(handler, warmup_timeout_s=30.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await asyncio.wait_for(ac.get("/"), timeout=5.0)

    assert r.status_code == 503
    assert proxy.state.state is State.COLD


async def test_proxy_recovers_after_unexpected_warmup_error(make_app):
    """The next request must start a fresh warmup, not join a dead future."""
    mode = {"broken": True}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            if mode["broken"]:
                raise ValueError("boom")
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = make_app(handler, warmup_timeout_s=30.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        first = await asyncio.wait_for(ac.get("/"), timeout=5.0)
        assert first.status_code == 503

        mode["broken"] = False
        second = await asyncio.wait_for(ac.get("/"), timeout=5.0)

    assert second.status_code == 200
    assert second.json() == {"upstream": True}
    assert proxy.state.state is State.WARM


async def test_forwarded_request_keeps_short_connect_timeout(make_app):
    """A bare float would clobber the connect timeout; httpx.Timeout must not."""
    seen = {}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path != "/v1/models":
            seen.update(request.extensions.get("timeout", {}))
        return httpx.Response(200, json={})

    app, proxy, rec = make_app(handler, request_timeout_s=300.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        await ac.post("/v1/chat/completions", json={})

    assert seen["connect"] == CONNECT_TIMEOUT_S
    assert seen["read"] == 300.0


def _pod_status_handler(status: str):
    """REST pod GET reports ``status``; start/stop and pod host return 200."""
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            if request.method == "GET" and request.url.path == POD_PATH:
                return httpx.Response(200, json={"id": POD_ID, "desiredStatus": status})
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    return handler


async def test_pod_start_skipped_when_pod_already_running(make_app):
    """Starting a running pod is rejected by RunPod and would burn the warmup budget."""
    app, proxy, rec = _pod_app(make_app, _pod_status_handler("RUNNING"), warmup_timeout_s=5.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await asyncio.wait_for(ac.get("/"), timeout=5.0)

    assert r.status_code == 200
    assert proxy.state.state is State.WARM
    assert rest_calls(rec, START_PATH) == []


async def test_pod_start_still_issued_when_pod_is_stopped(make_app):
    app, proxy, rec = _pod_app(make_app, _pod_status_handler("EXITED"), warmup_timeout_s=5.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")

    assert r.status_code == 200
    assert len(rest_calls(rec, START_PATH)) == 1


async def test_pod_start_issued_when_status_is_unknown(make_app):
    """An unreadable status must not silently skip the start."""
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            if request.method == "GET" and request.url.path == POD_PATH:
                return httpx.Response(500, json={})
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = _pod_app(make_app, handler, warmup_timeout_s=5.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")

    assert r.status_code == 200
    assert len(rest_calls(rec, START_PATH)) == 1


async def test_pod_stop_skipped_when_pod_already_exited(make_app):
    import time

    app, proxy, rec = _pod_app(
        make_app, _pod_status_handler("EXITED"), keepalive_interval_s=0.05, idle_giveup_s=0.1,
    )
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time() - 10
    proxy.keepalive.start()
    await asyncio.sleep(0.25)
    await proxy.keepalive.stop()

    assert proxy.state.state is State.COLD
    assert rest_calls(rec, STOP_PATH) == []
