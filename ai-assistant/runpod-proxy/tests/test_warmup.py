"""Warmup behavior: single-flight, timeout, re-warm."""
import asyncio

import httpx

from proxy.state import State
from tests.conftest import ok_handler


async def test_first_request_warms_once_then_forwards(make_app):
    app, proxy, rec = make_app(ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={"prompt": "hello"})
    assert r.status_code == 200
    assert r.json() == {"upstream": True}
    assert len(rec.warmup_calls) == 1
    assert proxy.state.state is State.WARM
    assert proxy.state.last_warmup_at is not None


async def test_subsequent_requests_do_not_rewarm(make_app):
    app, proxy, rec = make_app(ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r1 = await ac.post("/", json={})
        r2 = await ac.post("/", json={})
        r3 = await ac.get("/some/path")
    assert (r1.status_code, r2.status_code, r3.status_code) == (200, 200, 200)
    assert len(rec.warmup_calls) == 1


async def test_warmup_timeout_returns_503_and_resets_to_cold(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("connection refused", request=request)

    app, proxy, rec = make_app(handler, warmup_timeout_s=0.3, warmup_backoff_max_s=0.1)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 503
    assert "warmup timeout" in r.json()["error"]
    assert proxy.state.state is State.COLD


async def test_concurrent_requests_share_single_warmup(make_app):
    async def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            await asyncio.sleep(0.3)
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"upstream": True})

    app, proxy, rec = make_app(handler, warmup_timeout_s=10.0)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        results = await asyncio.gather(ac.get("/"), ac.get("/"), ac.get("/"))
    assert [r.status_code for r in results] == [200, 200, 200]
    assert len(rec.warmup_calls) == 1
    assert proxy.state.state is State.WARM


async def test_warm_endpoint_triggers_warmup(make_app):
    app, proxy, rec = make_app(ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/_warm")
    assert r.status_code == 200
    assert r.json() == {"state": "WARM"}
    assert len(rec.warmup_calls) == 1


async def test_warm_endpoint_timeout_503(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("nope", request=request)

    app, proxy, rec = make_app(handler, warmup_timeout_s=0.2, warmup_backoff_max_s=0.1)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/_warm")
    assert r.status_code == 503
    assert proxy.state.state is State.COLD


async def test_request_arriving_during_warmup_joins_it_instead_of_502(make_app):
    """A request that lands while the state is WARMING must wait for the
    single-flight warmup, not be forwarded into a worker that is not up yet."""
    seen = {"probe": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            seen["probe"] += 1
            if seen["probe"] <= 2:
                raise httpx.ConnectError("worker not up yet", request=request)
            return httpx.Response(200, json={})
        # The worker answers real traffic only once the warmup probe succeeded.
        if seen["probe"] <= 2:
            raise httpx.ConnectError("worker not up yet", request=request)
        return httpx.Response(200, json={"answered": True})

    app, proxy, _ = make_app(
        handler, warmup_timeout_s=10.0, warmup_backoff_max_s=0.1
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://t"
    ) as ac:
        first = asyncio.create_task(ac.get("/"))
        # Let the first request kick off the shared warmup.
        for _ in range(500):
            if proxy.state.state is State.WARMING:
                break
            await asyncio.sleep(0.01)
        assert proxy.state.state is State.WARMING
        second = await ac.get("/")
        first_response = await first

    assert first_response.status_code == 200
    assert second.status_code == 200
    assert second.json() == {"answered": True}
