"""Keepalive loop: pings, idle give-up, degradation, re-warm."""
import asyncio
import time

import httpx

from proxy.state import State
from tests.conftest import ok_handler


async def test_keepalive_pings_warm_endpoint(make_app):
    app, proxy, rec = make_app(ok_handler, keepalive_interval_s=0.05)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time()
    proxy.keepalive.start()
    await asyncio.sleep(0.3)
    await proxy.keepalive.stop()
    assert len(rec.keepalive_calls) >= 1
    assert proxy.state.last_keepalive_at is not None
    assert proxy.state.consecutive_keepalive_failures == 0


async def test_keepalive_idle_giveup_sets_cold_and_stops_pinging(make_app):
    app, proxy, rec = make_app(ok_handler, keepalive_interval_s=0.05, idle_giveup_s=0.1)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time() - 10
    proxy.keepalive.start()
    await asyncio.sleep(0.25)
    await proxy.keepalive.stop()
    assert proxy.state.state is State.COLD
    assert len(rec.keepalive_calls) == 0


async def test_keepalive_noop_when_not_warm(make_app):
    app, proxy, rec = make_app(ok_handler, keepalive_interval_s=0.05)
    proxy.state.state = State.COLD
    proxy.keepalive.start()
    await asyncio.sleep(0.2)
    await proxy.keepalive.stop()
    assert rec.requests == []


async def test_three_consecutive_failures_degrade_then_real_request_rewarms(make_app):
    mode = {"down": True}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            if mode["down"]:
                raise httpx.ConnectError("down", request=request)
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = make_app(handler, keepalive_interval_s=0.05)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time()
    proxy.keepalive.start()
    await asyncio.sleep(0.35)
    await proxy.keepalive.stop()
    assert proxy.state.state is State.DEGRADED
    assert proxy.state.consecutive_keepalive_failures >= 3

    # Next real request re-warms (mock now healthy) and forwards.
    mode["down"] = False
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 200
    assert r.json() == {"upstream": True}
    assert proxy.state.state is State.WARM


async def test_degraded_rewarms_to_warm_without_real_traffic(make_app):
    mode = {"down": True}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            if mode["down"]:
                raise httpx.ConnectError("worker recycled", request=request)
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = make_app(handler, keepalive_interval_s=0.05)
    proxy.state.state = State.WARM
    proxy.state.last_real_traffic_at = time.time()
    proxy.keepalive.start()

    # Worker dies -> DEGRADED after 3 failed keepalives.
    await asyncio.sleep(0.35)
    assert proxy.state.state is State.DEGRADED

    # Worker is back: keepalives alone must re-warm to WARM (no real traffic).
    mode["down"] = False
    await asyncio.sleep(0.35)
    assert proxy.state.state is State.WARM
    assert proxy.state.consecutive_keepalive_failures == 0
    await proxy.keepalive.stop()
