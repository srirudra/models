"""Hermetic tests for model-based pod discovery and its cost guardrails."""
import asyncio
import logging
import time

import httpx
import pytest

from proxy.state import State
from proxy.lifecycle import LifecycleError

REST = "rest.runpod.io"
# The v2 API host (Config.availability_api_url default).  Pod create and
# network-volume calls go here; v1 start/stop/delete/list stay on REST.
API = "api.runpod.io"
MODEL = "llama-3"


def pod(pid, status="RUNNING", name=MODEL, ports=("8000/http",)):
    return {
        "id": pid, "name": name, "desiredStatus": status,
        "image": f"{name}:latest", "templateId": "tpl",
        "env": {}, "ports": list(ports),
    }


def template(name=MODEL, serverless=False):
    return {
        "id": "tpl", "name": name, "imageName": f"{name}:latest",
        "env": {}, "ports": ["8000/http"], "isServerless": serverless,
    }


def discovery_app(make_app, handler, **overrides):
    config = dict(
        mode="pod", model_name=MODEL, api_key="management-key",
        allow_pod_create=True, pod_health_timeout_s=.3,
        pod_ready_timeout_s=.3, warmup_backoff_max_s=.05,
        warmup_timeout_s=1, keepalive_interval_s=.02,
        # v2 create requires a gpu id; discovery tests default to one.
        gpu_type_ids=("A",),
    )
    config.update(overrides)
    return make_app(handler, **config)


def rest(rec, method=None, path=None):
    return [
        r for r in rec.requests if r.url.host == REST
        and (method is None or r.method == method)
        and (path is None or r.url.path == path)
    ]


def api_calls(rec, method=None, path=None):
    return [
        r for r in rec.requests if r.url.host == API
        and (method is None or r.method == method)
        and (path is None or r.url.path == path)
    ]


async def test_forgets_created_pod_that_is_deleted_or_terminated(make_app):
    """When RunPod has deleted (404) or TERMINATED the pod the proxy created,
    its id must be forgotten: keeping it would make stop() target a dead pod,
    fail, and retry on every keepalive tick forever."""
    state = {"terminated": False}

    def handler(request):
        if request.url.host == API and request.url.path == "/v2/pods":
            if request.method == "POST":
                if state["terminated"]:
                    return httpx.Response(500, json={"detail": "no capacity"})
                return httpx.Response(201, json=pod("created", "RUNNING"))
        if request.url.host != REST:
            raise httpx.ConnectError("unhealthy", request=request)
        path = request.url.path
        if path == "/v1/templates":
            return httpx.Response(200, json=[template()])
        if path == "/v1/pods":
            return httpx.Response(200, json=[])
        if path == "/v1/pods/created":
            if state["terminated"]:
                return httpx.Response(404, json={})
            return httpx.Response(200, json=pod("created", "EXITED"))
        if path == "/v1/pods/created/stop":
            return httpx.Response(500, json={"message": "pod gone"})
        return httpx.Response(200, json={})

    app, _, rec = discovery_app(
        make_app, handler,
        pod_health_timeout_s=.01, pod_ready_timeout_s=.01,
        warmup_backoff_max_s=.01, warmup_timeout_s=.5,
    )
    assert (await request(app)).status_code == 503
    lifecycle = app.state.proxy.lifecycle
    assert lifecycle._created_pod_ids == {MODEL: "created"}

    # RunPod deletes the pod out-of-band.
    state["terminated"] = True
    stops_before = len(rest(rec, "POST", "/v1/pods/created/stop"))
    with pytest.raises(LifecycleError):
        await lifecycle.start()  # creation also fails -> no replacement recorded
    assert lifecycle._created_pod_ids == {}
    # stop() now has no live pod to target: no API call, no pending retries.
    await lifecycle.stop()
    assert len(rest(rec, "POST", "/v1/pods/created/stop")) == stops_before
    assert lifecycle.pending_stops == set()


async def request(app):
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        return await client.get("/")


async def test_failed_discovery_creates_at_most_one_pod(make_app):
    def handler(request):
        if request.url.host == API and request.url.path == "/v2/pods":
            if request.method == "POST":
                return httpx.Response(201, json=pod("created", "EXITED"))
        if request.url.host == REST:
            if request.url.path == "/v1/templates":
                return httpx.Response(200, json=[template()])
            if request.url.path == "/v1/pods/created":
                return httpx.Response(200, json=pod("created", "EXITED"))
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if request.method == "POST":
                return httpx.Response(200, json={})
        raise httpx.ConnectError("unhealthy", request=request)

    app, _, rec = discovery_app(
        make_app, handler, pod_health_timeout_s=.01, pod_ready_timeout_s=.01,
        warmup_backoff_max_s=.01, warmup_timeout_s=.5,
    )
    response = await request(app)
    assert response.status_code == 503
    with pytest.raises(LifecycleError):
        await app.state.proxy.lifecycle.start()
    assert len(api_calls(rec, "POST", "/v2/pods")) == 1


@pytest.mark.parametrize("initial", ["created", "resumed"])
async def test_unhealthy_pod_is_reclaimed(make_app, initial):
    calls = {"get": 0}

    def handler(request):
        if request.url.host == API and request.url.path == "/v2/pods":
            if request.method == "POST":
                return httpx.Response(201, json=pod("new", "EXITED"))
        if request.url.host != REST:
            raise httpx.ConnectError("unhealthy", request=request)
        path = request.url.path
        if path == "/v1/pods":
            if initial == "resumed":
                if request.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[])
                return httpx.Response(200, json=[pod("old", "EXITED")])
            return httpx.Response(200, json=[])
        if path == "/v1/templates":
            return httpx.Response(200, json=[template()])
        if path == "/v1/pods/old":
            return httpx.Response(200, json=pod("old", "EXITED"))
        if path in ("/v1/pods/old/start", "/v1/pods/new/start"):
            return httpx.Response(200)
        if path in ("/v1/pods/old/stop", "/v1/pods/new/stop"):
            return httpx.Response(200)
        if path == "/v1/pods/new":
            return httpx.Response(200, json=pod("new", "EXITED"))
        return httpx.Response(200, json=[])

    app, _, rec = discovery_app(make_app, handler, warmup_timeout_s=.8)
    response = await request(app)
    assert response.status_code == 503
    expected = "old" if initial == "resumed" else "new"
    assert rest(rec, "POST", f"/v1/pods/{expected}/stop")


async def test_created_pod_is_resumed_instead_of_created_again(make_app):
    state = {"created": False, "started": False, "healthy": False}

    def handler(request):
        if request.url.host == API and request.url.path == "/v2/pods":
            if request.method == "POST":
                state["created"] = True
                return httpx.Response(201, json=pod("one", "EXITED"))
        if request.url.host != REST:
            if state["healthy"]:
                return httpx.Response(200, json={"ok": True})
            raise httpx.ConnectError("unhealthy", request=request)
        path = request.url.path
        if path == "/v1/pods":
            return httpx.Response(200, json=[])
        if path == "/v1/templates":
            return httpx.Response(200, json=[template()])
        if path == "/v1/pods/one":
            return httpx.Response(200, json=pod("one", "RUNNING" if state["started"] else "EXITED"))
        if path == "/v1/pods/one/start":
            state["healthy"] = True
            state["started"] = True
            return httpx.Response(200)
        if path == "/v1/pods/one/stop":
            state["started"] = False
            return httpx.Response(200)
        return httpx.Response(200, json={})

    app, proxy, rec = discovery_app(make_app, handler, pod_health_timeout_s=.03,
                                    pod_ready_timeout_s=.03, warmup_timeout_s=.15)
    assert (await request(app)).status_code == 503
    assert rest(rec, "POST", "/v1/pods/one/stop")
    # A later lifecycle attempt sees the remembered pod and can resume it.
    state["healthy"] = True
    state["started"] = False
    await proxy.lifecycle.start()
    assert len(api_calls(rec, "POST", "/v2/pods")) == 1
    assert len(rest(rec, "POST", "/v1/pods/one/start")) == 1


async def test_existing_running_unhealthy_pod_is_not_stopped(make_app):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        raise httpx.ConnectError("unhealthy", request=request)

    app, _, rec = discovery_app(make_app, handler, allow_pod_create=False,
                                warmup_timeout_s=.12)
    assert (await request(app)).status_code == 503
    assert not rest(rec, "POST", "/v1/pods/live/stop")


async def test_healthy_running_pod_is_reused(make_app):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"model": MODEL})

    app, proxy, rec = discovery_app(make_app, handler)
    response = await request(app)
    assert response.status_code == 200
    assert proxy.target.pod_id == "live"
    assert not rest(rec, "POST", "/v1/pods/live/start")
    assert not api_calls(rec, "POST", "/v2/pods")


@pytest.mark.parametrize("status_code", [404, 502, 503, 504])
async def test_runpod_edge_not_ready_status_is_not_treated_as_healthy(make_app, status_code):
    """RunPod's own edge proxy answers with a synthetic 404 (nothing listening
    yet) or 502/503/504 (listening but not ready) before the container inside
    is actually serving. These must not be mistaken for a healthy pod — doing
    so would expose the endpoint before the model server can answer."""
    calls = {"pod_probes": 0}

    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        calls["pod_probes"] += 1
        if calls["pod_probes"] < 2:
            return httpx.Response(status_code)
        return httpx.Response(200, json={"model": MODEL})

    app, proxy, rec = discovery_app(
        make_app, handler, pod_health_timeout_s=2.0, warmup_backoff_max_s=0.02,
        warmup_timeout_s=5.0,
    )
    response = await request(app)
    assert response.status_code == 200
    assert calls["pod_probes"] >= 2
    assert proxy.target.pod_id == "live"


@pytest.mark.parametrize("status_code", [401, 403, 200])
async def test_real_upstream_response_codes_still_count_as_healthy(make_app, status_code):
    """Unlike RunPod's synthetic not-ready codes, a real response from the
    model server (even an auth error) means the pod itself is reachable."""
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(status_code, json={"model": MODEL})

    app, proxy, rec = discovery_app(make_app, handler)
    response = await request(app)
    assert response.status_code in (200, 401, 403)
    assert proxy.target.pod_id == "live"


async def test_pod_stuck_not_ready_times_out_as_unhealthy(make_app):
    """A pod that never clears RunPod's synthetic 404 within
    POD_HEALTH_TIMEOUT_S is correctly reported as discovery failure, not
    silently declared warm."""
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(404)

    app, proxy, rec = discovery_app(
        make_app, handler, allow_pod_create=False,
        pod_health_timeout_s=0.05, warmup_backoff_max_s=0.01, warmup_timeout_s=0.3,
    )
    response = await request(app)
    assert response.status_code == 503
    assert proxy.target.pod_id == ""



async def test_exited_pod_is_started_and_forwarded(make_app):
    status = {"value": "EXITED"}

    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                if request.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[])
                return httpx.Response(200, json=[pod("old", status["value"])])
            if request.url.path == "/v1/pods/old":
                return httpx.Response(200, json=pod("old", status["value"]))
            if request.url.path == "/v1/pods/old/start":
                status["value"] = "RUNNING"
                return httpx.Response(200)
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler, allow_pod_create=False)
    assert (await request(app)).status_code == 200
    assert len(rest(rec, "POST", "/v1/pods/old/start")) == 1


async def test_no_create_when_disabled(make_app):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            return httpx.Response(200, json=[])
        return httpx.Response(503)

    app, _, rec = discovery_app(make_app, handler, allow_pod_create=False,
                                warmup_timeout_s=.1)
    assert (await request(app)).status_code == 503
    assert not api_calls(rec, "POST", "/v2/pods")


async def test_template_create_forwards(make_app):
    def handler(request):
        if request.url.host == API and request.url.path == "/v2/pods":
            if request.method == "POST":
                return httpx.Response(201, json=pod("new", "RUNNING"))
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if request.url.path == "/v1/templates":
                return httpx.Response(200, json=[template()])
            if request.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod("new", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler)
    assert (await request(app)).status_code == 200
    assert len(api_calls(rec, "POST", "/v2/pods")) == 1


async def test_nonmatching_running_pod_is_ignored(make_app):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                if request.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[pod("wrong", name="other-model")])
                return httpx.Response(200, json=[pod("right", "EXITED")])
            if request.url.path == "/v1/pods/right/start":
                return httpx.Response(200)
            if request.url.path == "/v1/pods/right":
                return httpx.Response(200, json=pod("right", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, _ = discovery_app(make_app, handler, allow_pod_create=False)
    assert (await request(app)).status_code == 200
    assert proxy.target.pod_id == "right"


async def test_transient_start_failure_is_retried_not_abandoned(make_app):
    """Regression: RunPod's start endpoint can return a transient 5xx right
    after a pod was just stopped (backend still cleaning up). A single such
    failure must not make discovery give up on an otherwise perfectly
    reusable EXITED pod and fall through to a costly brand-new create."""
    calls = {"start": 0}

    def handler(req):
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                if req.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[])
                return httpx.Response(200, json=[pod("old", "EXITED")])
            if req.url.path == "/v1/pods/old/start":
                calls["start"] += 1
                if calls["start"] == 1:
                    return httpx.Response(500, json={"message": "transient"})
                return httpx.Response(200)
            if req.url.path == "/v1/pods/old":
                return httpx.Response(200, json=pod("old", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(
        make_app, handler, allow_pod_create=False,
        pod_ready_timeout_s=.2, warmup_backoff_max_s=.05,
        warmup_timeout_s=5,
    )
    assert (await request(app)).status_code == 200
    assert proxy.target.pod_id == "old"
    assert calls["start"] == 2
    assert not api_calls(rec, "POST", "/v2/pods")  # no new pod was created


async def test_start_failure_falls_through_to_next_exited_pod(make_app):
    started = set()

    def handler(req):
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                if req.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[])
                return httpx.Response(200, json=[pod("first", "EXITED"), pod("second", "EXITED")])
            if req.url.path == "/v1/pods/first/start":
                return httpx.Response(400, json={"message": "no GPU available"})
            if req.url.path == "/v1/pods/second/start":
                started.add("second")
                return httpx.Response(200)
            if req.url.path == "/v1/pods/second":
                return httpx.Response(200, json=pod("second", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler, allow_pod_create=False)
    assert (await request(app)).status_code == 200
    assert proxy.target.pod_id == "second"
    assert rest(rec, "POST", "/v1/pods/first/start")
    assert rest(rec, "POST", "/v1/pods/second/start")


async def test_serverless_template_is_not_selected(make_app):
    def handler(req):
        if req.url.host == API:
            if req.url.path == "/v2/pods":
                return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[
                    {**template(), "id": "serverless", "isServerless": True},
                    {**template(), "id": "persistent"},
                ])
            return httpx.Response(200, json=pod("new", "RUNNING"))
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler)
    assert (await request(app)).status_code == 200
    create = next(r for r in api_calls(rec, "POST", "/v2/pods"))
    assert b'"templateId":"persistent"' in create.content.replace(b" ", b"")


async def test_template_name_overrides_model_matching(make_app):
    bodies = []

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods" and req.method == "POST":
            bodies.append(req.content)
            return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[
                    {**template("named-template"), "id": "named"},
                    {**template(MODEL), "id": "model"},
                ])
            return httpx.Response(200, json=pod("new", "RUNNING"))
        return httpx.Response(200, json={"ok": True})

    app, _, _ = discovery_app(make_app, handler, template_name="named-template")
    assert (await request(app)).status_code == 200
    assert b'"templateId":"named"' in bodies[0].replace(b" ", b"")


async def test_discovery_is_single_flight_under_concurrency(make_app):
    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods" and req.method == "POST":
            return httpx.Response(201, json=pod("one", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[template()])
            return httpx.Response(200, json=pod("one", "RUNNING"))
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler)
    results = await asyncio.gather(*(request(app) for _ in range(5)))
    assert all(result.status_code == 200 for result in results)
    assert len(api_calls(rec, "POST", "/v2/pods")) == 1
    assert proxy.state.discoveries == 1


async def test_idle_giveup_stops_discovered_pod(make_app):
    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods" and req.method == "POST":
            return httpx.Response(201, json=pod("discovered", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[template()])
            if req.url.path == "/v1/pods/discovered":
                return httpx.Response(200, json=pod("discovered", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(
        make_app, handler, keepalive_interval_s=.02, idle_giveup_s=.05,
    )
    assert (await request(app)).status_code == 200
    proxy.state.last_real_traffic_at = time.time() - 10
    proxy.keepalive.start()
    await asyncio.sleep(.15)
    await proxy.keepalive.stop()
    assert proxy.state.state is State.COLD
    stops = rest(rec, "POST", "/v1/pods/discovered/stop")
    assert stops
    assert not rest(rec, "POST", "/v1/pods/configured/stop")


async def test_discovery_revalidation_window(make_app):
    def handler(req):
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(
        make_app, handler, allow_pod_create=False, pod_revalidate_s=.1,
    )
    assert (await request(app)).status_code == 200
    count = len(rest(rec, "GET", "/v1/pods"))
    assert (await request(app)).status_code == 200
    assert len(rest(rec, "GET", "/v1/pods")) == count
    proxy.state.last_success_at = time.time() - 1
    assert (await request(app)).status_code == 200
    assert len(rest(rec, "GET", "/v1/pods")) == count + 1


async def test_discovery_management_key_isolated_from_pod_requests(make_app):
    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods" and req.method == "POST":
            return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[template()])
            if req.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod("new", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler, upstream_api_key="upstream-key")
    assert (await request(app)).status_code == 200
    rest_requests = [r for r in rec.requests if r.url.host in (REST, API)]
    pod_requests = [r for r in rec.requests if r.url.host not in (REST, API)]
    assert rest_requests and all(
        r.headers["authorization"] == "Bearer management-key" for r in rest_requests
    )
    assert pod_requests and all(
        r.headers["authorization"] == "Bearer upstream-key" for r in pod_requests
    )
    assert all(
        r.headers["authorization"] != "Bearer management-key" for r in pod_requests
    )


async def test_discovery_derives_port_from_pod_ports(make_app):
    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods" and req.method == "POST":
            return httpx.Response(201, json=pod("ports", "RUNNING",
                                                ports=("9001/http", "22/tcp")))
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[template()])
            if req.url.path == "/v1/pods/ports":
                return httpx.Response(200, json=pod("ports", "RUNNING",
                                                    ports=("9001/http", "22/tcp")))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler, pod_port=8000)
    assert (await request(app)).status_code == 200
    assert proxy.target.url == "https://ports-9001.proxy.runpod.net"
    assert any(r.url.host == "ports-9001.proxy.runpod.net" for r in rec.requests)


async def test_lifecycle_validation_requires_discovery_selector(make_app):
    app, _, _ = make_app(lambda request: httpx.Response(200), mode="pod",
                          api_key="key", keepalive_interval_s=100)
    with pytest.raises(RuntimeError, match="RUNPOD_POD_ID or RUNPOD_MODEL_NAME"):
        async with app.router.lifespan_context(app):
            pass


async def test_warmup_log_includes_lifecycle_error_message(make_app, caplog):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            return httpx.Response(200, json=[])
        return httpx.Response(503)

    app, _, _ = discovery_app(make_app, handler, allow_pod_create=False,
                              warmup_timeout_s=.1)
    with caplog.at_level(logging.WARNING, logger="runpod-proxy.warmup"):
        assert (await request(app)).status_code == 503
    warnings = [r for r in caplog.records if r.name == "runpod-proxy.warmup"
                and "warmup attempt failed" in r.getMessage()]
    assert warnings
    joined = "\n".join(r.getMessage() for r in warnings)
    # The message text, not just the type name, must be present.
    assert "LifecycleError" in joined
    assert "no matching pod" in joined


async def test_disabled_create_logs_env_var_hint(make_app, caplog):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            return httpx.Response(200, json=[])
        return httpx.Response(503)

    app, _, _ = discovery_app(make_app, handler, allow_pod_create=False,
                              warmup_timeout_s=.1)
    with caplog.at_level(logging.INFO, logger="runpod-proxy.lifecycle"):
        assert (await request(app)).status_code == 503
    assert any("RUNPOD_ALLOW_POD_CREATE" in r.getMessage()
               for r in caplog.records if r.name == "runpod-proxy.lifecycle")


async def test_discovery_never_logs_pod_env_secrets(make_app, caplog):
    secret = "SUPER_SECRET_TOKEN_VALUE"

    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                if request.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[{
                        **pod("wrong", name="other-model"),
                        "env": {"API_TOKEN": secret},
                    }])
                return httpx.Response(200, json=[])
            return httpx.Response(200, json=[])
        return httpx.Response(503)

    app, _, _ = discovery_app(make_app, handler, allow_pod_create=False,
                              warmup_timeout_s=.1)
    caplog.set_level(logging.DEBUG)
    assert (await request(app)).status_code == 503
    for record in caplog.records:
        assert secret not in record.getMessage()
        assert secret not in repr(record.args)


async def test_circuit_breaker_opens_after_repeated_reclaims_and_fails_fast(make_app):
    """A crash-looping pod (e.g. bad launch args) would otherwise burn a full
    pod_health_timeout_s on every single client retry, forever. After enough
    consecutive reclaims, start() must fail immediately instead of repeating
    the doomed multi-minute cycle."""
    def handler(request):
        if request.url.host == REST:
            path = request.url.path
            if path == "/v1/pods":
                if request.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[])
                return httpx.Response(200, json=[pod("old", "EXITED")])
            if path == "/v1/pods/old":
                return httpx.Response(200, json=pod("old", "EXITED"))
            if path in ("/v1/pods/old/start", "/v1/pods/old/stop"):
                return httpx.Response(200)
            return httpx.Response(200, json={})
        raise httpx.ConnectError("unhealthy forever", request=request)

    app, proxy, rec = discovery_app(
        make_app, handler, allow_pod_create=False,
        pod_health_timeout_s=.05, pod_ready_timeout_s=.05,
        pod_circuit_breaker_threshold=2, pod_circuit_breaker_cooldown_s=100.0,
    )
    lifecycle = proxy.lifecycle

    for _ in range(2):
        with pytest.raises(LifecycleError):
            await lifecycle.start()

    stops_before = len(rest(rec, "POST", "/v1/pods/old/stop"))
    assert stops_before == 2  # one reclaim per failed attempt above

    with pytest.raises(LifecycleError, match="circuit breaker"):
        await lifecycle.start()
    # Fail-fast: the breaker must reject before making any further RunPod
    # calls (no additional resume/stop cycle).
    assert len(rest(rec, "POST", "/v1/pods/old/stop")) == stops_before
    assert proxy.lifecycle.circuit_breaker_open_s > 0


async def test_circuit_breaker_resets_after_a_healthy_pod(make_app):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"model": MODEL})

    app, proxy, rec = discovery_app(
        make_app, handler, pod_circuit_breaker_threshold=2,
    )
    await proxy.lifecycle.start()
    assert proxy.lifecycle.circuit_breaker_open_s == 0.0


async def test_first_cold_start_request_uses_template_derived_key_not_stale_fallback(make_app):
    """Regression: on a cold start's very first request, target.pod_id is
    still unresolved when the request handler starts, so the per-pod auth
    header must be computed AFTER ensure_warm() resolves it -- not before --
    or the forwarded request goes out with the stale static fallback key
    instead of the correct template-derived one."""
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"model": MODEL})

    app, proxy, rec = discovery_app(
        make_app, handler,
        upstream_api_key_template="sk-{pod_id}",
        upstream_api_key="stale-fallback-key",
    )
    response = await request(app)
    assert response.status_code == 200
    pod_requests = [r for r in rec.requests if r.url.host != REST]
    assert pod_requests
    assert all(r.headers["authorization"] == "Bearer sk-live" for r in pod_requests)


async def test_status_exposes_last_discovery_error(make_app):
    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        raise httpx.ConnectError("unhealthy", request=request)

    app, _, _ = discovery_app(make_app, handler, allow_pod_create=False,
                              warmup_timeout_s=.12)
    assert (await request(app)).status_code == 503
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        status = (await client.get("/_status")).json()
    assert status["last_discovery_error"] == "ConnectError"
