"""Shared fixtures: app factory with an injected mock upstream (hermetic, no network)."""
import json
from typing import Any, Callable, List

import httpx
import pytest

from proxy.config import Config
from proxy.main import create_app

WARMUP_PATH = "/v1/models"
BASE_URL = "https://ep.api.runpod.ai"


def _body(request: httpx.Request) -> Any:
    if not request.content:
        return None
    try:
        return json.loads(request.content)
    except ValueError:
        return request.content


class Recorder:
    """Records every upstream request; behavior driven by a swappable handler.

    Warmup and keepalive probes are both ``GET /v1/models``, so they are
    classified by the proxy state when the probe arrived: warmup probes run
    while WARMING, keepalive probes run while WARM/DEGRADED.
    """

    def __init__(self, handler: Callable[[httpx.Request], httpx.Response]) -> None:
        self.handler = handler
        self.requests: List[httpx.Request] = []
        self.state_at: List[str] = []
        self.state = None  # attached by make_app after the proxy exists

    def __call__(self, request: httpx.Request) -> httpx.Response:
        self.requests.append(request)
        self.state_at.append(self.state.state.value if self.state is not None else "")
        return self.handler(request)

    @property
    def non_warmup(self) -> List[httpx.Request]:
        return [r for r in self.requests if r.url.path != WARMUP_PATH]

    @property
    def warmup_calls(self) -> List[httpx.Request]:
        return [
            r for r, st in zip(self.requests, self.state_at)
            if r.url.path == WARMUP_PATH and st == "WARMING"
        ]

    @property
    def keepalive_calls(self) -> List[httpx.Request]:
        return [
            r for r, st in zip(self.requests, self.state_at)
            if r.url.path == WARMUP_PATH and st in ("WARM", "DEGRADED")
        ]


@pytest.fixture
async def make_app():
    clients: List[httpx.AsyncClient] = []

    def _make(handler, **config_overrides):
        recorder = Recorder(handler)
        client = httpx.AsyncClient(transport=httpx.MockTransport(recorder))
        base = dict(
            serverless_url=BASE_URL,
            warmup_path="v1/models",
            warmup_timeout_s=5.0,
            warmup_backoff_max_s=0.5,
            keepalive_interval_s=0.05,
            idle_giveup_s=300.0,
            request_timeout_s=5.0,
            # The production default is "model" (see proxy/health.py); the
            # generic 200-JSON test upstream has no model list, so tests keep
            # the legacy "any" bar unless a test opts into a stricter one.
            pod_health_mode="any",
        )
        base.update(config_overrides)
        app = create_app(Config(**base), client)
        clients.append(client)
        recorder.state = app.state.proxy.state
        return app, app.state.proxy, recorder

    yield _make

    for client in clients:
        await client.aclose()


@pytest.fixture
def asgi() -> Callable:
    def _make(app) -> httpx.AsyncClient:
        return httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://testserver")

    return _make


def ok_handler(request: httpx.Request) -> httpx.Response:
    """Default upstream: 200 JSON everywhere."""
    return httpx.Response(200, json={"upstream": True})
