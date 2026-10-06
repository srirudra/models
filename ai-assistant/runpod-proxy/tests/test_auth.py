"""PROXY_API_KEY gate: the proxy attaches your RunPod key, so it must not be open."""
import httpx

from tests.conftest import ok_handler

PROXY_KEY = "local-secret"


def auth_echo_handler(request: httpx.Request) -> httpx.Response:
    if request.url.path == "/v1/models":
        return httpx.Response(200, json={})
    return httpx.Response(200, json={"auth": request.headers.get("authorization"),
                                     "proxy_key": request.headers.get("x-proxy-key")})


async def test_requests_without_key_are_rejected(make_app):
    app, proxy, rec = make_app(ok_handler, proxy_api_key=PROXY_KEY)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        forwarded = await ac.post("/v1/chat/completions", json={})
        status = await ac.get("/_status")
        warm = await ac.post("/_warm")
        metrics = await ac.get("/metrics")

    assert [r.status_code for r in (forwarded, status, warm, metrics)] == [401, 401, 401, 401]
    assert rec.requests == []  # nothing reached the upstream


async def test_wrong_key_is_rejected(make_app):
    app, proxy, rec = make_app(ok_handler, proxy_api_key=PROXY_KEY)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/_status", headers={"x-proxy-key": "wrong"})
    assert r.status_code == 401


async def test_dedicated_header_and_bearer_token_both_authenticate(make_app):
    app, proxy, rec = make_app(ok_handler, proxy_api_key=PROXY_KEY)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        via_header = await ac.get("/_status", headers={"x-proxy-key": PROXY_KEY})
        via_bearer = await ac.get("/_status", headers={"Authorization": f"Bearer {PROXY_KEY}"})

    assert via_header.status_code == 200
    assert via_bearer.status_code == 200


async def test_health_route_stays_public_for_container_healthchecks(make_app):
    app, proxy, rec = make_app(ok_handler, proxy_api_key=PROXY_KEY)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/_health")
    assert r.status_code == 200
    assert r.json() == {"ok": True}


async def test_proxy_key_is_never_forwarded_upstream(make_app):
    """In pod mode client Authorization passes through - our key must not."""
    app, proxy, rec = make_app(
        auth_echo_handler, proxy_api_key=PROXY_KEY, mode="pod", pod_id="p", api_key="rk",
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        via_header = await ac.post("/", json={}, headers={"x-proxy-key": PROXY_KEY})
        via_bearer = await ac.post("/", json={}, headers={"Authorization": f"Bearer {PROXY_KEY}"})

    assert via_header.json() == {"auth": None, "proxy_key": None}
    assert via_bearer.json() == {"auth": None, "proxy_key": None}


async def test_client_auth_still_passes_through_when_no_proxy_key_configured(make_app):
    app, proxy, rec = make_app(auth_echo_handler, mode="pod", pod_id="p", api_key="rk")
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={}, headers={"Authorization": "Bearer model-server-key"})
    assert r.json()["auth"] == "Bearer model-server-key"


async def test_upstream_key_still_injected_when_proxy_key_used(make_app):
    app, proxy, rec = make_app(
        auth_echo_handler, proxy_api_key=PROXY_KEY, api_key="rp_live_key",
    )
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={}, headers={"Authorization": f"Bearer {PROXY_KEY}"})
    assert r.json()["auth"] == "Bearer rp_live_key"
