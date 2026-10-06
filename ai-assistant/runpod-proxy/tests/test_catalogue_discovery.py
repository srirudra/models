"""Discovery tests for catalogue-driven pod matching and creation."""
import json
import logging

import httpx
import pytest

from proxy.lifecycle import LifecycleError
from proxy.models_config import load_catalogue
from tests.test_discovery import REST, API, api_calls, discovery_app, pod, request, rest


def catalogue(models):
    return load_catalogue(inline=json.dumps({"models": models}))


def tpl(tid, name, serverless=False):
    return {
        "id": tid, "name": name, "imageName": f"{name}:latest",
        "env": {}, "ports": ["8000/http"], "isServerless": serverless,
    }


def creates(rec):
    return [json.loads(r.content) for r in api_calls(rec, "POST", "/v2/pods")]


async def test_running_pod_matched_by_template_id(make_app):
    """A pod whose templateId matches the catalogue is selected even when its
    name/image/env would fail the slug heuristic."""
    model = "Qwen/Qwen3.8-27B-FP8"
    cat = catalogue([{"name": model, "templates": ["qwen-template"]}])
    running = {**pod("qwen-pod", name="Qwen3.8-27B-FP8"), "templateId": "tpl-qwen"}

    def handler(req):
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("tpl-qwen", "qwen-template")])
            if req.url.path == "/v1/pods":
                if req.url.params.get("desiredStatus") == "RUNNING":
                    return httpx.Response(200, json=[running])
                return httpx.Response(200, json=[])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, _ = discovery_app(make_app, handler, model_name=model, catalogue=cat)
    assert (await request(app)).status_code == 200
    assert proxy.target.pod_id == "qwen-pod"


async def test_templates_tried_in_declaration_order(make_app, caplog):
    """The second template is used when the first name is absent, and a
    WARNING names the missing template."""
    model = "meta/Llama-3-70B"
    cat = catalogue([{
        "name": model, "templates": ["missing-template", "present-template"],
    }])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("present", "present-template")])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod("new", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    with caplog.at_level(logging.WARNING, logger="runpod-proxy.lifecycle"):
        app, _, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat)
        assert (await request(app)).status_code == 200
    bodies = creates(rec)
    assert len(bodies) == 1
    assert bodies[0]["templateId"] == "present"
    assert any("missing-template" in r.getMessage()
               for r in caplog.records if r.name == "runpod-proxy.lifecycle")


async def test_gpu_matrix_is_walked_in_order(make_app):
    """gpus [(A,1-2),(B,2-2)] yield attempts (A,1),(A,2),(B,2) cheapest first."""
    model = "meta/Llama-3-70B"
    cat = catalogue([{
        "name": model, "templates": ["tpl-name"],
        "gpus": [
            {"id": "A", "min": 1, "max": 2},
            {"id": "B", "min": 2, "max": 2},
        ],
    }])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(400, json={"detail": "no GPU available"})
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("tpl", "tpl-name")])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat)
    assert (await request(app)).status_code == 503
    with pytest.raises(LifecycleError):
        await app.state.proxy.lifecycle.start()
    bodies = creates(rec)
    sequence = [(b["templateId"], b["gpu"]["id"], b["gpu"]["count"]) for b in bodies]
    assert sequence == [
        ("tpl", "A", 1),
        ("tpl", "A", 2),
        ("tpl", "B", 2),
        ("tpl", "A", 1),
        ("tpl", "A", 2),
        ("tpl", "B", 2),
    ]


async def test_first_created_pod_stops_the_matrix(make_app):
    """The first create_pod that returns a pod ends the matrix; a later failed
    health probe never provisions again, and a second start() resumes it."""
    model = "meta/Llama-3-70B"
    cat = catalogue([{
        "name": model, "templates": ["tpl-name"],
        "gpus": [{"id": "A", "min": 1, "max": 2}],
    }])
    state = {"create_calls": 0, "healthy": False}

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            state["create_calls"] += 1
            if state["create_calls"] == 1:
                return httpx.Response(400, json={"detail": "no GPU available"})
            return httpx.Response(201, json=pod("created", "RUNNING"))
        if req.url.host != REST:
            if state["healthy"]:
                return httpx.Response(200, json={"ok": True})
            raise httpx.ConnectError("unhealthy", request=req)
        path = req.url.path
        if path == "/v1/templates":
            return httpx.Response(200, json=[tpl("tpl", "tpl-name")])
        if path == "/v1/pods":
            return httpx.Response(200, json=[])
        if path == "/v1/pods/created":
            return httpx.Response(200, json=pod("created", "RUNNING"))
        if path == "/v1/pods/created/stop":
            return httpx.Response(200)
        return httpx.Response(200, json={})

    app, proxy, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat,
                                    pod_health_timeout_s=.02, pod_ready_timeout_s=.02,
                                    warmup_timeout_s=.2)
    # First warmup: one failed attempt then a success, then an unhealthy probe.
    assert (await request(app)).status_code == 503
    # Exactly failures (1) + 1 successful create.
    assert len(api_calls(rec, "POST", "/v2/pods")) == 2
    assert rest(rec, "POST", "/v1/pods/created/stop")
    # A subsequent start resumes the remembered pod rather than creating again.
    state["healthy"] = True
    await proxy.lifecycle.start()
    assert len(api_calls(rec, "POST", "/v2/pods")) == 2
    assert proxy.target.pod_id == "created"


async def test_create_succeeds_on_first_combo_skips_remaining_matrix(make_app):
    """When the very first combo (A,1) succeeds, the matrix stops immediately:
    exactly one create is issued for the first combination even though many
    remaining combinations (two templates x five gpu combos) exist, and the
    remembered pod is resumed on a later start() instead of created again."""
    model = "meta/Llama-3-70B"
    cat = catalogue([{
        "name": model, "templates": ["tpl-one", "tpl-two"],
        "gpus": [
            {"id": "A", "min": 1, "max": 3},
            {"id": "B", "min": 1, "max": 2},
        ],
    }])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(201, json=pod("created", "RUNNING"))
        if req.url.host != REST:
            return httpx.Response(200, json={"ok": True})
        path = req.url.path
        if path == "/v1/templates":
            return httpx.Response(200, json=[
                tpl("tpl1", "tpl-one"), tpl("tpl2", "tpl-two"),
            ])
        if path == "/v1/pods":
            return httpx.Response(200, json=[])
        if path == "/v1/pods/created":
            return httpx.Response(200, json=pod("created", "RUNNING"))
        if path == "/v1/pods/created/stop":
            return httpx.Response(200)
        return httpx.Response(200, json={})

    app, proxy, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat,
                                    pod_health_timeout_s=.2, pod_ready_timeout_s=.2,
                                    warmup_timeout_s=.5)
    # The first combo succeeds and is healthy: warmup resolves the target.
    assert (await request(app)).status_code == 200
    assert proxy.target.pod_id == "created"
    # Exactly one create despite 2 templates x 5 gpu combos remaining.
    posts = api_calls(rec, "POST", "/v2/pods")
    assert len(posts) == 1
    body = json.loads(posts[0].content)
    assert body["templateId"] == "tpl1"
    assert body["gpu"] == {"id": "A", "count": 1}
    assert proxy.state.pod_creates == 1
    # The created pod id was recorded: a later start() resumes it rather than
    # provisioning another billable pod.
    await proxy.lifecycle.start()
    assert len(api_calls(rec, "POST", "/v2/pods")) == 1
    assert proxy.state.pod_creates == 1
    assert proxy.target.pod_id == "created"


async def test_max_create_attempts_caps_the_matrix(make_app):
    model = "meta/Llama-3-70B"
    cat = catalogue([{
        "name": model, "templates": ["tpl-name"],
        "gpus": [{"id": "A", "min": 1, "max": 3}],
    }])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(400, json={"detail": "no GPU available"})
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("tpl", "tpl-name")])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat,
                                    max_create_attempts=2, warmup_timeout_s=.2)
    assert (await request(app)).status_code == 503
    assert len(api_calls(rec, "POST", "/v2/pods")) == 2
    assert "RUNPOD_MAX_CREATE_ATTEMPTS" in proxy.lifecycle.last_error


async def test_per_model_port_override_used_for_probe_and_url(make_app):
    model = "llama-3"
    cat = catalogue([{"name": model, "templates": ["tpl-name"], "port": 9000}])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(201, json=pod(
                "new", "RUNNING", ports=("8000/http", "9000/http")))
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("tpl", "tpl-name")])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod(
                    "new", "RUNNING", ports=("8000/http", "9000/http")))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat)
    assert (await request(app)).status_code == 200
    assert proxy.target.url == "https://new-9000.proxy.runpod.net"
    assert any(r.url.host == "new-9000.proxy.runpod.net" for r in rec.requests)


async def test_template_name_overrides_catalogue(make_app):
    model = "meta/Llama-3-70B"
    cat = catalogue([{"name": model, "templates": ["catalogue-tpl"]}])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[
                    tpl("named", "named-template"),
                    tpl("cat", "catalogue-tpl"),
                ])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod("new", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat,
                                template_name="named-template")
    assert (await request(app)).status_code == 200
    bodies = creates(rec)
    assert bodies[0]["templateId"] == "named"


async def test_no_catalogue_does_not_list_templates_in_matching(make_app):
    """A non-catalogue deployment must not gain an extra templates request on
    the running/exited matching path."""
    def handler(req):
        if req.url.host == REST:
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[pod("live")])
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler)
    assert (await request(app)).status_code == 200
    assert proxy.target.pod_id == "live"
    assert not rest(rec, "GET", "/v1/templates")


async def test_datacenters_sent_on_create(make_app):
    """Catalogue datacenters reach the create body as dataCenterIds.  The
    v2 API has no priority field, so the declared order is informational and
    datacenter_priority is accepted but not sent."""
    model = "meta/Llama-3-70B"
    cat = catalogue([{
        "name": model, "templates": ["tpl-name"],
        "datacenters": ["US-TX-3", "US-KS-3"],
        "datacenter_priority": "custom",
    }])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("tpl", "tpl-name")])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod("new", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, proxy, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat)
    assert (await request(app)).status_code == 200
    bodies = creates(rec)
    assert len(bodies) == 1
    assert bodies[0]["dataCenterIds"] == ["US-TX-3", "US-KS-3"]
    assert "dataCenterPriority" not in bodies[0]


async def test_no_datacenters_omits_the_fields(make_app):
    """Without catalogue datacenters the create body must not pin any
    datacenter (RunPod's own defaults apply)."""
    model = "meta/Llama-3-70B"
    cat = catalogue([{"name": model, "templates": ["tpl-name"]}])

    def handler(req):
        if req.url.host == API and req.url.path == "/v2/pods":
            return httpx.Response(201, json=pod("new", "RUNNING"))
        if req.url.host == REST:
            if req.url.path == "/v1/templates":
                return httpx.Response(200, json=[tpl("tpl", "tpl-name")])
            if req.url.path == "/v1/pods":
                return httpx.Response(200, json=[])
            if req.url.path == "/v1/pods/new":
                return httpx.Response(200, json=pod("new", "RUNNING"))
            return httpx.Response(200, json={})
        return httpx.Response(200, json={"ok": True})

    app, _, rec = discovery_app(make_app, handler, model_name=model, catalogue=cat)
    assert (await request(app)).status_code == 200
    bodies = creates(rec)
    assert len(bodies) == 1
    assert "dataCenterIds" not in bodies[0]
