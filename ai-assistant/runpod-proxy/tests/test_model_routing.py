"""Hermetic tests for request model selection and pod switching."""
import asyncio
import json

import httpx

from proxy.state import State

REST = "rest.runpod.io"
MODEL = "model-a"


def pod(pid, status="RUNNING", name=MODEL, ports=("8000/http",)):
    return {"id": pid, "name": name, "desiredStatus": status,
            "image": f"{name}:latest", "templateId": "tpl", "env": {},
            "ports": list(ports)}


def discovery_app(make_app, handler, **overrides):
    config = dict(mode="pod", model_name=MODEL, api_key="management-key",
                  allow_pod_create=False, pod_health_timeout_s=.3,
                  pod_ready_timeout_s=.3, warmup_backoff_max_s=.05,
                  warmup_timeout_s=1, keepalive_interval_s=.02)
    config.update(overrides)
    return make_app(handler, **config)


def rest(rec, method=None, path=None):
    return [r for r in rec.requests if r.url.host == REST
            and (method is None or r.method == method)
            and (path is None or r.url.path == path)]


async def call(app, method="POST", body=None):
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        return await client.request(method, "/v1/chat/completions", json=body) if body is not None else await client.get("/v1/models")


def model_handler(request):
    if request.url.host == REST:
        if request.url.path == "/v1/pods":
            wanted = request.url.params.get("desiredStatus")
            return httpx.Response(200, json=[pod("a", name="model-a"), pod("b", name="model-b")])
        if request.url.path in ("/v1/pods/a", "/v1/pods/b"):
            pid = request.url.path.split("/")[-1]
            return httpx.Response(200, json=pod(pid, name=f"model-{pid}"))
        return httpx.Response(200, json={})
    return httpx.Response(200, json={"ok": True})


async def test_body_model_selects_pod(make_app):
    app, proxy, _ = discovery_app(
        make_app, model_handler, model_name="", allowed_models=("model-a", "model-b")
    )
    response = await call(app, body={"model": "model-b"})
    assert response.status_code == 200
    assert proxy.target.pod_id == "b"


async def test_disallowed_model_does_not_call_upstream(make_app):
    app, _, recorder = discovery_app(make_app, model_handler, allowed_models=("model-a",))
    response = await call(app, body={"model": "nope"})
    assert response.status_code == 400
    assert response.json() == {"error": "model not allowed", "model": "nope", "allowed": ["model-a"]}
    assert not recorder.requests


async def test_slug_matching_uses_canonical_model(make_app):
    def handler(request):
        if request.url.host == REST and request.url.path == "/v1/pods":
            return httpx.Response(200, json=[pod("q", name="Qwen/Qwen3-32B")])
        if request.url.host == REST:
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})
    app, proxy, _ = discovery_app(
        make_app, handler, model_name="", allowed_models=("Qwen/Qwen3-32B",),
        allow_pod_create=False,
    )
    response = await call(app, body={"model": "qwen/qwen3-32b"})
    assert response.status_code == 200
    assert proxy.lifecycle.active_model == "Qwen/Qwen3-32B"


async def test_bad_or_missing_body_uses_default(make_app):
    app, proxy, recorder = discovery_app(
        make_app, model_handler, model_name="model-a", allowed_models=(),
    )
    assert (await call(app)).status_code == 200
    assert proxy.lifecycle.active_model == "model-a"
    assert (await call(app, body={"other": 1})).status_code == 200
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        malformed = await client.post(
            "/v1/chat/completions", content=b"{not json",
            headers={"content-type": "application/json"},
        )
    assert malformed.status_code == 200
    assert all(r.url.host != REST or "model-b" not in r.content.decode() for r in recorder.requests)


async def test_switch_adopts_new_model_without_stopping_discovered_old(make_app):
    """Spec §11.4: a pod found already RUNNING (not started or created by this
    process) is never stopped — not even by a model switch — because it may
    belong to the operator or another proxy instance. The switch still adopts
    the new model's pod and forwards traffic to it."""
    app, proxy, recorder = discovery_app(
        make_app, model_handler, model_name="", allowed_models=("model-a", "model-b")
    )
    assert (await call(app, body={"model": "model-a"})).status_code == 200
    assert (await call(app, body={"model": "model-b"})).status_code == 200
    assert not rest(recorder, "POST", "/v1/pods/a/stop")
    assert proxy.target.pod_id == "b"


async def test_same_model_does_not_switch(make_app):
    app, _, recorder = discovery_app(make_app, model_handler, model_name="model-a")
    assert (await call(app, body={"model": "anything"})).status_code == 200
    before = len(rest(recorder, "GET", "/v1/pods"))
    assert (await call(app, body={"model": "model-a"})).status_code == 200
    assert len(rest(recorder, "GET", "/v1/pods")) == before
    assert not rest(recorder, "POST", "/v1/pods/a/stop")


async def test_concurrent_new_model_is_single_flight(make_app):
    app, proxy, recorder = discovery_app(
        make_app, model_handler, model_name="", allowed_models=("model-a", "model-b")
    )
    await call(app, body={"model": "model-a"})
    responses = await asyncio.gather(*(call(app, body={"model": "model-b"}) for _ in range(3)))
    assert all(r.status_code == 200 for r in responses)
    # One shared switch (single flight): exactly two discoveries (model-a,
    # then model-b) and one pod list per discovery. The discovered pod-a is
    # never stopped by the switch (spec §11.4).
    assert len(rest(recorder, "POST", "/v1/pods/a/stop")) == 0
    assert len(rest(recorder, "GET", "/v1/pods")) == 2
    assert proxy.state.discoveries == 2


async def test_serverless_allowlist_and_warm_default(make_app):
    app, _, recorder = make_app(
        lambda request: httpx.Response(200, json={"ok": True}),
        allowed_models=("model-a",), model_name="model-a",
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://test") as client:
        bad = await client.post("/v1/chat/completions", json={"model": "model-b"})
        warm = await client.post("/_warm")
    assert bad.status_code == 400
    assert warm.status_code == 200
    assert len(recorder.non_warmup) == 0


async def test_forwarded_body_uses_canonical_model_name(make_app):
    """The router accepts case/slug variants, but the pod's model server only
    knows the canonical allowlist spelling, so the forwarded body must carry it."""
    seen = {}

    def handler(request):
        if request.url.host == REST and request.url.path == "/v1/pods":
            return httpx.Response(200, json=[pod("q", name="Qwen/Qwen3-32B")])
        if request.url.host == REST:
            return httpx.Response(200, json={})
        if request.url.path == "/v1/models":  # warmup probe
            return httpx.Response(200, json={"models": []})
        # Real traffic only: record the forwarded body.
        seen["body"] = json.loads(request.content.decode())
        return httpx.Response(200, json={"ok": True})

    app, _, _ = discovery_app(
        make_app, handler, model_name="", allowed_models=("Qwen/Qwen3-32B",),
        allow_pod_create=False,
    )
    response = await call(
        app,
        body={"model": "qwen/qwen3-32b",
              "messages": [{"role": "user", "content": "hi"}]},
    )
    assert response.status_code == 200
    assert seen["body"]["model"] == "Qwen/Qwen3-32B"
    # The rest of the body is forwarded untouched.
    assert seen["body"]["messages"] == [{"role": "user", "content": "hi"}]


async def test_switch_warmup_timeout_returns_503_not_500(make_app):
    """A model switch whose warmup times out (no pod for the new model) must
    surface a 503 warmup-timeout response, not an unhandled 500."""

    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("a", name="model-a")])
            return httpx.Response(200, json={})
        if request.url.host == "a-8000.proxy.runpod.net":
            return httpx.Response(200, json={"ok": True})
        raise httpx.ConnectError("no worker for model-b", request=request)

    app, proxy, _ = discovery_app(
        make_app, handler, model_name="",
        allowed_models=("model-a", "model-b"),
        warmup_timeout_s=0.3, warmup_backoff_max_s=0.05,
    )
    assert (await call(app, body={"model": "model-a"})).status_code == 200
    response = await call(app, body={"model": "model-b"})
    assert response.status_code == 503
    assert response.json()["error"] == "endpoint warmup timeout"
    assert proxy.state.state is State.COLD
    assert proxy.state.requests_failed >= 1


async def test_warm_endpoint_keeps_active_model(make_app):
    """POST /_warm must warm the model traffic is currently on, not the
    default — otherwise an operator prewarm swaps the active pod."""
    app, proxy, recorder = discovery_app(
        make_app, model_handler, model_name="",
        allowed_models=("model-a", "model-b"),
    )
    assert (await call(app, body={"model": "model-b"})).status_code == 200
    assert proxy.router.active_model == "model-b"
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        warm = await client.post("/_warm")
    assert warm.status_code == 200
    assert proxy.router.active_model == "model-b"
    assert proxy.target.pod_id == "b"
    # No switch happened: the active pod was never stopped.
    assert not rest(recorder, "POST", "/v1/pods/b/stop")


async def test_failed_switch_stop_is_retried(make_app):
    """Spec §11.3: a failed stop on a model switch is recorded and retried by
    the keepalive loop. Only a pod this proxy itself started or created may be
    stopped (§11.4), so pod-a here is found EXITED and resumed by the proxy."""
    attempts = []

    def handler(request):
        if request.url.host == REST and request.url.path == "/v1/pods":
            if request.url.params.get("desiredStatus") == "EXITED":
                return httpx.Response(
                    200, json=[pod("a", name="model-a", status="EXITED")])
            return httpx.Response(200, json=[pod("b", name="model-b")])
        if request.url.host == REST and request.url.path == "/v1/pods/a":
            return httpx.Response(200, json=pod("a", name="model-a"))
        if request.url.host == REST and request.url.path == "/v1/pods/a/stop":
            attempts.append(1)
            return httpx.Response(500 if len(attempts) == 1 else 200)
        if request.url.host == REST:
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, _ = discovery_app(
        make_app, handler, model_name="", allowed_models=("model-a", "model-b")
    )
    assert (await call(app, body={"model": "model-a"})).status_code == 200
    assert proxy.target.pod_id == "a"
    assert proxy.state.pod_starts == 1  # the proxy itself resumed pod-a
    assert (await call(app, body={"model": "model-b"})).status_code == 200
    assert len(attempts) == 1  # the switch's stop failed once
    await proxy.keepalive._tick()
    assert len(attempts) == 2


# ---------------------------------------------------------------------------
# Concurrency: model-switch drain must observe in-flight completion lock-free.
# These tests exercise ModelRouter directly with lightweight fakes.
# ---------------------------------------------------------------------------
import time

from proxy.config import Config
from proxy.lifecycle import DiscoveryPodLifecycle
from proxy.router import ModelRouter
from proxy.state import EndpointState
from proxy.target import UpstreamTarget


class _FakeDiscoveryLifecycle(DiscoveryPodLifecycle):
    """DiscoveryPodLifecycle subclass with no network; records stop() times."""

    def __init__(self) -> None:  # noqa: D401 - deliberately bypass base __init__
        self._active_model = "model-a"
        self.stops: list[float] = []

    async def start(self) -> None:
        return None

    async def stop(self) -> None:
        self.stops.append(time.monotonic())


class _FakeWarmup:
    """Stand-in warmup whose ensure_warm can be gated on an event."""

    def __init__(self, gate: "asyncio.Event | None" = None) -> None:
        self.gate = gate
        self.calls = 0

    async def ensure_warm(self, force: bool = False) -> None:
        self.calls += 1
        if self.gate is not None:
            await self.gate.wait()


def _make_router(warmup=None, **cfg):
    kwargs = dict(serverless_url="http://upstream", model_name="model-a")
    kwargs.update(cfg)
    config = Config(**kwargs)
    state = EndpointState()
    target = UpstreamTarget(config.upstream_url, "")
    lifecycle = _FakeDiscoveryLifecycle()
    warmup = warmup or _FakeWarmup()
    router = ModelRouter(config, state, target, lifecycle, warmup)
    return router, lifecycle, warmup, state


async def test_drain_observes_completion_promptly():
    router, lifecycle, _warmup, _state = _make_router(model_switch_drain_s=5.0)
    released_at: dict[str, float] = {}

    async def hold_a():
        async with router.request("model-a"):
            await asyncio.sleep(0.05)
        released_at["a"] = time.monotonic()

    task_a = asyncio.create_task(hold_a())
    await asyncio.sleep(0)  # let request A acquire its lease first

    start = time.monotonic()
    async with router.request("model-b"):
        pass
    elapsed = time.monotonic() - start

    await task_a
    assert lifecycle.stops, "switch must stop the previous pod"
    # (a) stop happened only after the model-A lease was released
    assert lifecycle.stops[0] >= released_at["a"]
    # (b) the switch completed far inside the 5s drain budget
    assert elapsed < 1.0, f"switch took {elapsed:.2f}s; drain did not observe completion"
    assert router.active_model == "model-b"


async def test_lease_release_not_blocked_by_in_progress_switch():
    gate = asyncio.Event()
    warmup = _FakeWarmup(gate=gate)
    router, lifecycle, _warmup, _state = _make_router(warmup=warmup, model_switch_drain_s=5.0)

    # Hold a model-A lease open.
    lease_a = router.request("model-a")
    await lease_a.__aenter__()

    # Start a model-B request; its ensure_warm blocks on the gate.
    switch_task = asyncio.create_task(_drive(router.request("model-b")))
    await asyncio.sleep(0.05)  # let the switch enter its drain wait

    # Releasing the model-A lease must not block on the switch's lock.
    release_task = asyncio.create_task(lease_a.__aexit__(None, None, None))
    await asyncio.wait_for(release_task, timeout=1.0)

    # The switch is now stuck inside ensure_warm (gate not yet set).
    await asyncio.sleep(0.02)
    assert warmup.calls == 1
    assert not switch_task.done()

    gate.set()
    await asyncio.wait_for(switch_task, timeout=1.0)
    assert router.active_model == "model-b"


async def test_drain_timeout_still_proceeds():
    router, lifecycle, _warmup, _state = _make_router(model_switch_drain_s=0.05)

    lease_a = router.request("model-a")
    await lease_a.__aenter__()  # held longer than the drain budget

    start = time.monotonic()
    async with router.request("model-b"):
        pass
    elapsed = time.monotonic() - start

    assert lifecycle.stops, "switch must proceed after the drain budget expires"
    assert router.active_model == "model-b"
    # It waited out (roughly) the drain budget rather than hanging forever.
    assert 0.03 <= elapsed < 1.0

    await lease_a.__aexit__(None, None, None)


async def _drive(cm):
    """Enter and exit an async context manager to completion."""
    async with cm:
        pass
