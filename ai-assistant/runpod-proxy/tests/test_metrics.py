"""/metrics Prometheus exposition."""
import httpx

from proxy.state import State
from tests.conftest import ok_handler


def _parse(text: str) -> dict:
    values = {}
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        name, _, value = line.rpartition(" ")
        values[name] = float(value)
    return values


async def test_metrics_exposes_state_and_counters(make_app):
    app, proxy, rec = make_app(ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        await ac.post("/v1/chat/completions", json={})  # cold: triggers a warmup
        await ac.post("/v1/chat/completions", json={})  # warm hit
        r = await ac.get("/metrics")

    assert r.status_code == 200
    assert r.headers["content-type"].startswith("text/plain")
    values = _parse(r.text)
    assert values['runpod_proxy_state{state="WARM"}'] == 1
    assert values['runpod_proxy_state{state="COLD"}'] == 0
    assert values["runpod_proxy_warmups"] == 1
    assert values["runpod_proxy_requests_total"] == 2
    assert values["runpod_proxy_requests_warm_hit"] == 1
    assert values["runpod_proxy_requests_cold_hit"] == 1
    assert values["runpod_proxy_requests_failed"] == 0


async def test_metrics_counts_failed_requests(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("boom", request=request)

    app, proxy, rec = make_app(handler)
    proxy.state.state = State.WARM  # skip warmup; fail on forward
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        assert (await ac.get("/")).status_code == 502
        r = await ac.get("/metrics")

    values = _parse(r.text)
    assert values["runpod_proxy_requests_failed"] == 1


async def test_metrics_counts_keepalive_failures(make_app):
    import asyncio
    import time

    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("down", request=request)

    app, proxy, rec = make_app(handler, keepalive_interval_s=0.05)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time()
    proxy.keepalive.start()
    await asyncio.sleep(0.25)
    await proxy.keepalive.stop()

    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        values = _parse((await ac.get("/metrics")).text)

    assert values["runpod_proxy_keepalive_failures_total"] >= 3
    assert values['runpod_proxy_state{state="DEGRADED"}'] == 1
