"""GPU availability poller: /_status view, /metrics gauges, staleness.

The background poller is started by the app lifespan, so every test drives
the real lifespan (``app.router.lifespan_context``) rather than pinging an
app whose background tasks were never started.
"""
import asyncio

import httpx

from proxy.models_config import GpuSpec, ModelCatalogue, ModelSpec
from tests.conftest import ok_handler

AVAIL_PATH = "/v2/catalog/gpus"


def _availability_payload():
    return {"gpus": [
        {"id": "GPU-A", "availability": "HIGH", "dataCenters": [
            {"id": "US-TX-1", "availability": "HIGH"},
            {"id": "EU-DE-1", "availability": "NONE"},
        ]},
        {"id": "GPU-B", "availability": "LOW", "dataCenters": []},
        # Not tracked: must be dropped from the views.
        {"id": "GPU-UNTRACKED", "availability": "HIGH", "dataCenters": []},
    ]}


def _make(app_factory, **overrides):
    # Mutable flag so a test can flip the poll endpoint to 500 mid-run.
    state = {"fail": False, "calls": 0}

    def _handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == AVAIL_PATH:
            state["calls"] += 1
            if state["fail"]:
                return httpx.Response(500, json={"detail": "boom"})
            return httpx.Response(200, json=_availability_payload())
        return ok_handler(request)

    base = dict(
        mode="pod",
        api_key="mgmt",
        model_name="m",
        gpu_type_ids=("GPU-A", "GPU-B"),
        gpu_availability_interval_s=0.05,
    )
    base.update(overrides)
    app, proxy, recorder = app_factory(_handler, **base)
    return app, proxy, recorder, state


async def _request(app, path):
    transport = httpx.ASGITransport(app=app)
    async with httpx.AsyncClient(transport=transport, base_url="http://t") as c:
        return await c.get(path)


async def test_status_exposes_tracked_gpu_availability(make_app, asgi):
    app, proxy, recorder, state = _make(make_app)
    async with app.router.lifespan_context(app):
        await asyncio.sleep(0.2)  # first tick runs immediately at start
        status = (await _request(app, "/_status")).json()

    avail = status["gpu_availability"]
    assert avail["last_error"] is None
    assert avail["updated_at"] is not None
    assert avail["age_s"] is not None
    assert set(avail["gpus"]) == {"GPU-A", "GPU-B"}  # untracked dropped
    assert avail["gpus"]["GPU-A"]["availability"] == "HIGH"
    assert avail["gpus"]["GPU-A"]["datacenters"] == [
        {"id": "US-TX-1", "availability": "HIGH"},
        {"id": "EU-DE-1", "availability": "NONE"},
    ]
    assert avail["gpus"]["GPU-B"]["availability"] == "LOW"
    assert avail["models"] == {}  # no catalogue in this config

    # The poll hit the v2 catalog with the availability expansion.
    polls = [r for r in recorder.requests if r.url.path == AVAIL_PATH]
    assert polls
    assert polls[0].url.params["include"] == "AVAILABILITY"
    assert polls[0].url.params["product"] == "POD"
    assert polls[0].url.params["cloud"] == "SECURE"
    assert polls[0].headers["authorization"] == "Bearer mgmt"


async def test_metrics_expose_gpu_availability_gauges(make_app, asgi):
    app, proxy, recorder, state = _make(make_app)
    async with app.router.lifespan_context(app):
        await asyncio.sleep(0.2)
        metrics = (await _request(app, "/metrics")).text

    assert 'runpod_proxy_gpu_availability_level{gpu="GPU-A"} 3' in metrics
    assert 'runpod_proxy_gpu_availability_level{gpu="GPU-B"} 1' in metrics
    assert "GPU-UNTRACKED" not in metrics
    assert "runpod_proxy_gpu_availability_age_s " in metrics


async def test_disabled_in_serverless_mode(make_app, asgi):
    # Serverless mode has no pod stock to poll; also, pod mode without an
    # API key would fail the lifespan validation before the poller mattered.
    app, proxy, recorder, state = _make(make_app, mode="serverless")
    async with app.router.lifespan_context(app):
        await asyncio.sleep(0.1)
        status = (await _request(app, "/_status")).json()
        metrics = (await _request(app, "/metrics")).text

    assert "gpu_availability" not in status
    assert "runpod_proxy_gpu_availability" not in metrics
    assert state["calls"] == 0


async def test_disabled_when_interval_zero(make_app, asgi):
    app, proxy, recorder, state = _make(make_app, gpu_availability_interval_s=0.0)
    async with app.router.lifespan_context(app):
        await asyncio.sleep(0.1)
        status = (await _request(app, "/_status")).json()

    assert "gpu_availability" not in status
    assert state["calls"] == 0


async def test_failure_keeps_last_known_values(make_app, asgi):
    app, proxy, recorder, state = _make(make_app)
    async with app.router.lifespan_context(app):
        await asyncio.sleep(0.2)
        good = (await _request(app, "/_status")).json()["gpu_availability"]
        assert good["last_error"] is None
        assert good["gpus"]["GPU-A"]["availability"] == "HIGH"

        # From here on every poll fails; the poller must keep serving the
        # last-known values with a recorded error and a growing age.
        state["fail"] = True
        await asyncio.sleep(0.2)
        stale = (await _request(app, "/_status")).json()["gpu_availability"]

    assert stale["gpus"]["GPU-A"]["availability"] == "HIGH"
    assert stale["gpus"]["GPU-B"]["availability"] == "LOW"
    assert stale["last_error"]
    assert stale["updated_at"] == good["updated_at"]  # not refreshed
    assert stale["age_s"] >= good["age_s"]


async def test_catalogue_gpu_ids_are_tracked(make_app, asgi):
    catalogue = ModelCatalogue(models=(
        ModelSpec(
            name="model-a",
            templates=("tpl",),
            gpus=(GpuSpec(id="GPU-B", min_count=1, max_count=1),),
            port=8000,
            container_disk_gb=None,
            volume_gb=None,
            cloud_type=None,
        ),
    ))
    app, proxy, recorder, state = _make(make_app, catalogue=catalogue,
                                        gpu_type_ids=(), model_name="model-a")
    assert proxy.availability.tracked_gpu_ids == ("GPU-B",)
    async with app.router.lifespan_context(app):
        await asyncio.sleep(0.2)
        status = (await _request(app, "/_status")).json()

    avail = status["gpu_availability"]
    assert set(avail["gpus"]) == {"GPU-B"}
    assert avail["models"]["model-a"] == {"GPU-B": "LOW"}
