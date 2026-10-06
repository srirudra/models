"""Pod mode: REST start before warmup, REST stop on idle give-up, status, config."""
import asyncio
import time

import httpx
import pytest

from proxy.config import Config
from proxy.state import State
from tests.conftest import ok_handler

POD_ID = "testpod"
POD_PORT = 8000
API_KEY = "rk-test-key"
REST_HOST = "rest.runpod.io"
POD_HOST = f"{POD_ID}-{POD_PORT}.proxy.runpod.net"
START_PATH = f"/v1/pods/{POD_ID}/start"
STOP_PATH = f"/v1/pods/{POD_ID}/stop"


def pod_ok_handler(request: httpx.Request) -> httpx.Response:
    """REST host: 200 for start/stop; pod host: 200 JSON everywhere."""
    if request.url.host == REST_HOST:
        return httpx.Response(200, json={"id": POD_ID})
    return httpx.Response(200, json={"upstream": True})


def _pod_app(make_app, handler, **overrides):
    base = dict(mode="pod", pod_id=POD_ID, pod_port=POD_PORT, api_key=API_KEY)
    base.update(overrides)
    return make_app(handler, **base)


def rest_calls(rec, path):
    return [r for r in rec.requests if r.url.host == REST_HOST and r.url.path == path]


async def test_pod_warmup_starts_pod_once_before_probe_then_forwards(make_app):
    app, proxy, rec = _pod_app(make_app, pod_ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={"prompt": "hello"})
    assert r.status_code == 200
    assert r.json() == {"upstream": True}
    assert proxy.state.state is State.WARM

    starts = rest_calls(rec, START_PATH)
    assert len(starts) == 1
    assert starts[0].method == "POST"
    assert starts[0].headers["authorization"] == f"Bearer {API_KEY}"

    # The REST start must precede the first warmup probe against the pod host.
    probes = [r for r in rec.requests if r.url.host == POD_HOST and r.url.path == "/v1/models"]
    assert len(probes) >= 1
    assert "authorization" not in probes[0].headers
    assert rec.requests.index(starts[0]) < rec.requests.index(probes[0])


async def test_pod_management_key_is_not_forwarded_to_model_server(make_app):
    app, proxy, rec = _pod_app(make_app, pod_ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        response = await ac.post("/", headers={"Authorization": "Bearer model-key"})

    assert response.status_code == 200
    pod_requests = [r for r in rec.requests if r.url.host == POD_HOST]
    assert pod_requests[-1].headers["authorization"] == "Bearer model-key"
    assert all(r.headers.get("authorization") != f"Bearer {API_KEY}" for r in pod_requests)


async def test_pod_upstream_key_authenticates_probes_and_forwarded_requests(make_app):
    app, proxy, rec = _pod_app(
        make_app, pod_ok_handler, upstream_api_key="model-server-key"
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        response = await ac.post("/", headers={"Authorization": "Bearer client-key"})

    assert response.status_code == 200
    pod_requests = [r for r in rec.requests if r.url.host == POD_HOST]
    assert all(
        r.headers["authorization"] == "Bearer model-server-key" for r in pod_requests
    )


async def test_upstream_api_key_template_derives_key_from_pod_id(make_app):
    """Some RunPod templates (e.g. vLLM) key the model server's API key off
    the pod's own id. RUNPOD_UPSTREAM_API_KEY_TEMPLATE must derive the right
    value per pod instead of relying on a static, easily stale key."""
    app, proxy, rec = _pod_app(
        make_app, pod_ok_handler,
        upstream_api_key_template="sk-{pod_id}",
        upstream_api_key="stale-key-from-a-different-pod",
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        response = await ac.get("/")

    assert response.status_code == 200
    pod_requests = [r for r in rec.requests if r.url.host == POD_HOST]
    assert pod_requests
    expected = "Bearer sk-" + POD_ID
    assert all(r.headers["authorization"] == expected for r in pod_requests)


def test_auth_headers_for_pod_falls_back_when_no_template():
    config = Config(serverless_url="", mode="pod", pod_id=POD_ID, upstream_api_key="fixed-key")
    expected = "Bearer fixed-key"
    assert config.auth_headers_for_pod(POD_ID) == {"authorization": expected}
    assert config.auth_headers_for_pod("") == {"authorization": expected}


def test_auth_headers_for_pod_uses_template_when_pod_id_known():
    config = Config(
        serverless_url="", mode="pod", pod_id=POD_ID,
        upstream_api_key="stale", upstream_api_key_template="sk-{pod_id}",
    )
    assert config.auth_headers_for_pod("newpod") == {"authorization": "Bearer sk-newpod"}
    # No pod id known yet (e.g. discovery hasn't resolved a target): fall back.
    assert config.auth_headers_for_pod("") == {"authorization": "Bearer stale"}


async def test_pod_start_500_is_retried_until_success(make_app):
    calls = {"start": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST and request.url.path == START_PATH:
            calls["start"] += 1
            return httpx.Response(500 if calls["start"] == 1 else 200, json={})
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = _pod_app(make_app, handler, warmup_timeout_s=10.0, warmup_backoff_max_s=0.1)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 200
    assert calls["start"] == 2
    assert proxy.state.state is State.WARM


async def test_pod_idle_giveup_stops_pod_then_goes_cold(make_app):
    app, proxy, rec = _pod_app(
        make_app, pod_ok_handler, keepalive_interval_s=0.05, idle_giveup_s=0.1
    )
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time() - 10
    proxy.keepalive.start()
    await asyncio.sleep(0.3)
    await proxy.keepalive.stop()
    assert proxy.state.state is State.COLD
    stops = rest_calls(rec, STOP_PATH)
    assert len(stops) == 1
    assert stops[0].headers["authorization"] == f"Bearer {API_KEY}"


async def test_pod_stop_failure_keeps_state_and_retries_next_tick(make_app):
    mode = {"stop_ok": False}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST and request.url.path == STOP_PATH:
            return httpx.Response(200 if mode["stop_ok"] else 500, json={})
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = _pod_app(make_app, handler, keepalive_interval_s=0.05, idle_giveup_s=0.1)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time() - 10
    proxy.keepalive.start()

    # Stop keeps failing: state must NOT drop to COLD (the pod is still billing).
    await asyncio.sleep(0.25)
    assert proxy.state.state is State.WARM
    failed_stops = len(rest_calls(rec, STOP_PATH))
    assert failed_stops >= 2  # retried on later ticks

    # Stop succeeds: now the state transitions to COLD, and stops cease.
    mode["stop_ok"] = True
    await asyncio.sleep(0.25)
    await proxy.keepalive.stop()
    assert proxy.state.state is State.COLD
    assert len(rest_calls(rec, STOP_PATH)) == failed_stops + 1


async def test_status_includes_pod_mode_and_pod_id(make_app):
    app, proxy, rec = _pod_app(make_app, pod_ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/_status")
    body = r.json()
    assert body["mode"] == "pod"
    assert body["pod_id"] == POD_ID
    assert body["endpoint"] == f"https://{POD_HOST}"


def test_pod_url_override_wins_over_derived_url():
    config = Config(
        serverless_url="", mode="pod", pod_id=POD_ID,
        pod_url="https://custom.example.net",
    )
    assert config.upstream_url == "https://custom.example.net"
    assert config.warmup_url == "https://custom.example.net/v1/models"


def test_derived_pod_url_uses_pod_id_and_port():
    config = Config(serverless_url="", mode="pod", pod_id=POD_ID, pod_port=1234)
    assert config.upstream_url == f"https://{POD_ID}-1234.proxy.runpod.net"


def test_invalid_mode_raises_value_error():
    with pytest.raises(ValueError):
        Config(serverless_url="", mode="bogus")


async def test_shutdown_backend_stops_pinned_pod(make_app):
    """Stopping the proxy must stop the pod so the container does not leave it
    RUNNING (and billing) until the next session's idle give-up."""
    app, proxy, rec = _pod_app(make_app, pod_ok_handler)
    await proxy.shutdown_backend()
    assert len(rest_calls(rec, STOP_PATH)) == 1


async def test_shutdown_backend_stops_even_when_stop_fails(make_app):
    """A failed stop must not crash the shutdown path (the process exits
    regardless); it must simply be swallowed and logged."""
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST and request.url.path == STOP_PATH:
            return httpx.Response(500, json={"message": "boom"})
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = _pod_app(make_app, handler)
    await proxy.shutdown_backend()  # must not raise
    assert len(rest_calls(rec, STOP_PATH)) == 1


async def test_shutdown_backend_noop_in_serverless_mode(make_app):
    app, proxy, rec = make_app(ok_handler)
    await proxy.shutdown_backend()
    assert not rec.requests


@pytest.mark.parametrize("status_code", [404, 502, 503, 504])
async def test_pinned_pod_warmup_retries_through_runpod_edge_not_ready(make_app, status_code):
    """RunPod's edge answers 404/502/503/504 while the pod is still starting;
    warmup must keep retrying rather than declaring WARM on that response."""
    calls = {"probes": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={"id": POD_ID})
        calls["probes"] += 1
        if calls["probes"] < 2:
            return httpx.Response(status_code)
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = _pod_app(make_app, handler, warmup_timeout_s=5.0, warmup_backoff_max_s=0.05)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 200
    assert calls["probes"] >= 2
    assert proxy.state.state is State.WARM


async def test_pinned_pod_warmup_times_out_when_stuck_on_runpod_404(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={"id": POD_ID})
        return httpx.Response(404)

    app, proxy, rec = _pod_app(make_app, handler, warmup_timeout_s=0.15, warmup_backoff_max_s=0.02)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 503
    assert proxy.state.state is State.COLD


async def test_serverless_mode_warmup_still_accepts_any_status(make_app):
    """The pod-mode fix must not change serverless behavior: any response
    (even 4xx/5xx, e.g. an unconfigured upstream key) still means warm."""
    def handler(request: httpx.Request) -> httpx.Response:
        return httpx.Response(404, json={"error": "not found"})

    app, proxy, rec = make_app(handler, warmup_timeout_s=2.0, warmup_backoff_max_s=0.05)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 404  # forwarded, but the endpoint was declared warm
    assert proxy.state.state is State.WARM


async def test_pinned_pod_keepalive_detects_runpod_edge_not_ready_as_failure(make_app):
    """A pod-mode keepalive ping that gets RunPod's synthetic 404 (pod
    reclaimed/crashed underneath us) must count as a keepalive failure, not a
    false 'still warm'."""
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == REST_HOST:
            return httpx.Response(200, json={"id": POD_ID})
        return httpx.Response(404)

    app, proxy, rec = _pod_app(make_app, handler, keepalive_interval_s=0.05)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time()
    proxy.keepalive.start()
    await asyncio.sleep(0.3)
    await proxy.keepalive.stop()
    assert proxy.state.consecutive_keepalive_failures >= 3
    assert proxy.state.state is State.DEGRADED
