"""/_status route behavior."""
import httpx

from tests.conftest import ok_handler


async def test_status_is_cold_before_any_request(make_app):
    app, proxy, rec = make_app(ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/_status")
    assert r.status_code == 200
    body = r.json()
    assert body["state"] == "COLD"
    assert body["endpoint"] == "https://ep.api.runpod.ai"
    assert body["consecutive_keepalive_failures"] == 0
    assert body["last_warmup_at"] is None
    assert body["last_keepalive_at"] is None
    assert body["last_real_traffic_at"] is None  # no billing anchor until real traffic
    assert body["uptime_s"] >= 0


async def test_status_reflects_warm_state(make_app):
    app, proxy, rec = make_app(ok_handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        await ac.post("/_warm")
        r = await ac.get("/_status")
    assert r.json()["state"] == "WARM"
    assert r.json()["last_warmup_at"] is not None
