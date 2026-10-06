"""Catalogue hot-reload: POST /_reload re-reads and validates the catalogue
source, then swaps it in on the shared Config without disturbing the active
model or in-flight requests. Rejected reloads leave the running catalogue
untouched."""
import json

import httpx

from proxy.config import Config
from proxy.models_config import load_catalogue

REST = "rest.runpod.io"


def catalogue_file(tmp_path, models):
    path = tmp_path / "models.json"
    path.write_text(json.dumps({"models": [{"name": n, "templates": [f"tpl-{n}"]}
                                           for n in models]}))
    return str(path)


def make_catalogue_app(make_app, tmp_path, models, **overrides):
    """App whose Config was built from a real catalogue file, mirroring what
    from_env does at boot (the make_app fixture does not read the file)."""
    path = catalogue_file(tmp_path, models)
    return make_app(
        lambda request: httpx.Response(200, json={"ok": True}),
        catalogue_file=path,
        catalogue=load_catalogue(path=path),
        **overrides,
    )


async def post_model(client, model):
    return await client.post("/v1/chat/completions", json={"model": model})


async def test_reload_adds_model(make_app, tmp_path):
    app, proxy, _ = make_catalogue_app(make_app, tmp_path, ["model-a"])
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        assert (await post_model(client, "model-a")).status_code == 200
        path = catalogue_file(tmp_path, ["model-a", "model-b"])
        reloaded = await client.post("/_reload")
    assert reloaded.status_code == 200
    assert reloaded.json() == {
        "reloaded": True, "source": path, "models": ["model-a", "model-b"],
    }
    assert proxy.config.effective_allowed_models == ("model-a", "model-b")
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        assert (await post_model(client, "model-b")).status_code == 200


async def test_reload_invalid_file_keeps_old_catalogue(make_app, tmp_path):
    app, proxy, _ = make_catalogue_app(make_app, tmp_path, ["model-a"])
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        (tmp_path / "models.json").write_text("{not json")
        reloaded = await client.post("/_reload")
        assert reloaded.status_code == 400
        assert "not valid" in reloaded.json()["error"]
        # The running catalogue is untouched: old model still routes, the
        # never-added one still does not.
        assert (await post_model(client, "model-a")).status_code == 200
        assert (await post_model(client, "model-b")).status_code == 400
    assert proxy.config.effective_allowed_models == ("model-a",)


async def test_reload_empty_models_rejected(make_app, tmp_path):
    app, proxy, _ = make_catalogue_app(make_app, tmp_path, ["model-a"])
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        (tmp_path / "models.json").write_text(json.dumps({"models": []}))
        reloaded = await client.post("/_reload")
    assert reloaded.status_code == 400
    assert "no models" in reloaded.json()["error"]
    assert proxy.config.effective_allowed_models == ("model-a",)


async def test_reload_missing_source_rejected(make_app):
    app, _, _ = make_app(lambda request: httpx.Response(200, json={"ok": True}))
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        reloaded = await client.post("/_reload")
    assert reloaded.status_code == 400
    assert "no model catalogue source" in reloaded.json()["error"]


async def test_reload_dropping_default_model_rejected(make_app, tmp_path):
    app, proxy, _ = make_catalogue_app(
        make_app, tmp_path, ["model-a", "model-b"], model_name="model-a")
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        catalogue_file(tmp_path, ["model-b"])  # drops the default model-a
        reloaded = await client.post("/_reload")
        assert reloaded.status_code == 400
        assert "model-a" in reloaded.json()["error"]
        assert (await post_model(client, "model-a")).status_code == 200
    assert proxy.config.default_model == "model-a"


async def test_reload_removed_model_becomes_unroutable(make_app, tmp_path):
    """With no pinned default, the default model is the catalogue's first
    entry; a reload that reorders/drops models changes routing immediately."""
    app, proxy, _ = make_catalogue_app(make_app, tmp_path, ["model-a", "model-b"])
    assert proxy.config.default_model == "model-a"
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        catalogue_file(tmp_path, ["model-b", "model-c"])
        assert (await client.post("/_reload")).status_code == 200
        assert (await post_model(client, "model-a")).status_code == 400
        assert (await post_model(client, "model-b")).status_code == 200
        # No model field: falls back to the new default (first entry).
        response = await client.post("/v1/chat/completions", json={
            "messages": [{"role": "user", "content": "hi"}]})
    assert response.status_code == 200


async def test_reload_inline_catalogue(make_app):
    inline = json.dumps({"models": [{"name": "model-a", "templates": ["t"]}]})
    app, proxy, _ = make_app(
        lambda request: httpx.Response(200, json={"ok": True}),
        catalogue_inline=inline,
        catalogue=load_catalogue(inline=inline),
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        reloaded = await client.post("/_reload")
    assert reloaded.status_code == 200
    assert reloaded.json()["source"] == "inline"
    assert reloaded.json()["models"] == ["model-a"]
    # The Config method re-reads its stored inline source directly.
    assert proxy.config.reload_catalogue().names == ("model-a",)


async def test_reload_requires_proxy_key(make_app, tmp_path):
    app, _, _ = make_catalogue_app(
        make_app, tmp_path, ["model-a"], proxy_api_key="secret")
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        assert (await client.post("/_reload")).status_code == 401
        ok = await client.post("/_reload", headers={"x-proxy-key": "secret"})
    assert ok.status_code == 200


async def test_reload_discovery_routes_new_model(make_app, tmp_path):
    """In discovery mode the swapped catalogue drives pod matching: a newly
    added model routes to (and adopts) its own pod via the normal switch."""
    def pod(pid):
        return {"id": pid, "name": f"model-{pid}", "desiredStatus": "RUNNING",
                "image": f"model-{pid}:latest", "templateId": f"tpl-{pid}",
                "env": {}, "ports": ["8000/http"]}

    def handler(request):
        if request.url.host == REST:
            if request.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("a"), pod("b")])
            if request.url.path in ("/v1/pods/a", "/v1/pods/b"):
                return httpx.Response(200, json=pod(request.url.path.rsplit("/", 1)[-1]))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    path = catalogue_file(tmp_path, ["model-a"])
    app, proxy, _ = make_app(
        handler,
        mode="pod", api_key="management-key", allow_pod_create=False,
        pod_health_timeout_s=.3, pod_ready_timeout_s=.3,
        warmup_backoff_max_s=.05, warmup_timeout_s=1, keepalive_interval_s=.02,
        catalogue_file=path, catalogue=load_catalogue(path=path),
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://test"
    ) as client:
        assert (await post_model(client, "model-a")).status_code == 200
        assert proxy.target.pod_id == "a"
        # Not in the pre-reload catalogue: rejected without touching the pod.
        assert (await post_model(client, "model-b")).status_code == 400
        catalogue_file(tmp_path, ["model-a", "model-b"])
        assert (await client.post("/_reload")).status_code == 200
        assert (await post_model(client, "model-b")).status_code == 200
    assert proxy.target.pod_id == "b"


def test_config_from_env_stores_catalogue_source(monkeypatch):
    """from_env must record the source so reload can re-read it."""
    monkeypatch.delenv("RUNPOD_MODELS_FILE", raising=False)
    monkeypatch.setenv(
        "RUNPOD_MODELS_JSON",
        json.dumps({"models": [{"name": "m", "templates": ["t"]}]},
                   ),
    )
    config = Config.from_env()
    assert config.catalogue_inline
    assert config.catalogue.names == ("m",)
