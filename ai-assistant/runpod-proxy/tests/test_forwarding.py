"""Reverse-proxy forwarding behavior."""
import json

import httpx

from proxy.state import State


async def test_forwards_method_path_query_body_and_strips_hop_by_hop(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            return httpx.Response(200, json={})
        return httpx.Response(200, json={
            "method": request.method,
            "path": request.url.path,
            "query": dict(request.url.params),
            "headers": {k.lower(): v for k, v in request.headers.items()},
            "body": request.content.decode(),
        })

    app, proxy, rec = make_app(handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post(
            "/v1/chat/completions?stream=true&model=x",
            json={"prompt": "hi"},
            headers={"X-Api-Key": "secret", "Connection": "close"},
        )
    assert r.status_code == 200
    got = r.json()
    assert got["method"] == "POST"
    assert got["path"] == "/v1/chat/completions"
    assert got["query"] == {"stream": "true", "model": "x"}
    assert json.loads(got["body"]) == {"prompt": "hi"}
    assert got["headers"]["x-api-key"] == "secret"
    assert got["headers"]["content-type"] == "application/json"
    # client's hop-by-hop values must not be forwarded: host/content-length
    # are recomputed by httpx for the upstream target (required on the wire)
    assert got["headers"]["host"] == "ep.api.runpod.ai"
    assert int(got["headers"]["content-length"]) == len(got["body"])
    assert "transfer-encoding" not in got["headers"]


async def test_empty_path_maps_to_base_root(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"path": request.url.path})

    app, proxy, rec = make_app(handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 200
    assert r.json()["path"] == "/"


async def test_non_2xx_upstream_response_passthrough(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            return httpx.Response(200, json={})
        return httpx.Response(
            422, json={"detail": [{"msg": "bad request"}]}, headers={"x-upstream": "yes"},
        )

    app, proxy, rec = make_app(handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={})
    assert r.status_code == 422
    assert r.json() == {"detail": [{"msg": "bad request"}]}
    assert r.headers.get("x-upstream") == "yes"


async def test_sse_streaming_response_passthrough(make_app):
    payload = b'data: {"token": "a"}\n\ndata: [DONE]\n\n'

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            return httpx.Response(200, json={})
        return httpx.Response(200, content=payload, headers={"content-type": "text/event-stream"})

    app, proxy, rec = make_app(handler)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={"stream": True})
    assert r.status_code == 200
    assert r.headers["content-type"] == "text/event-stream"
    assert r.text == payload.decode()


async def test_runpod_api_key_injected_and_overrides_client_auth(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.path == "/v1/models":
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"auth": request.headers.get("authorization")})

    app, proxy, rec = make_app(handler, api_key="rp_live_test")
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/", json={}, headers={"Authorization": "Bearer client-own-key"})
    assert r.status_code == 200
    # client's own token is replaced by the RunPod key
    assert r.json()["auth"] == "Bearer rp_live_test"
    # warmup pings carry the key too
    assert rec.warmup_calls[0].headers.get("authorization") == "Bearer rp_live_test"


async def test_upstream_connection_error_returns_502(make_app):
    def handler(request: httpx.Request) -> httpx.Response:
        raise httpx.ConnectError("boom", request=request)

    app, proxy, rec = make_app(handler)
    proxy.state.state = State.WARM  # skip warmup; forward directly
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.get("/")
    assert r.status_code == 502
    assert r.json()["error"] == "upstream connection error"
