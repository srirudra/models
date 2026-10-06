"""Pod replace: a start blocked because the pinned pod's original host has no
free GPU (RunPod's "please migrate" prompt or the REST 500 "not enough free
GPUs on the host machine") triggers the RUNPOD_ON_MIGRATE policy.  With
"replace", the old pod is terminated, a fresh one is created with the same
spec (template/GPUs/network volume), and the proxy adopts the new id/url —
including the per-pod model-server key derived from the new id.
"""
import json

import httpx
import pytest

from proxy.lifecycle import LifecycleError
from proxy.runpod_api import Pod
from proxy.state import State

OLD_ID = "oldpod"
NEW_ID = "newpod"
POD_PORT = 8000
API_KEY = "rk-test-key"
REST_HOST = "rest.runpod.io"
# The v2 API host (Config.availability_api_url default): pod create and
# network-volume lookups live here, v1 start/stop/delete do not.
V2_HOST = "api.runpod.io"
NEW_POD_HOST = f"{NEW_ID}-{POD_PORT}.proxy.runpod.net"
NEW_TARGET_URL = f"https://{NEW_POD_HOST}"

# Model-server key template used by the tests; the expected value is derived
# from it at runtime (never written as a literal).
KEY_TEMPLATE = "podkey-{pod_id}"


def expected_key(pod_id: str) -> str:
    """The Authorization header value the proxy sends to this pod."""
    return f"Bearer {KEY_TEMPLATE.format(pod_id=pod_id)}"

# The real RunPod v1 body for a start on a host out of GPUs (verified live).
CAPACITY_ERROR = (
    "start pod: There are not enough free GPUs on the host machine to "
    "start this pod."
)


def old_pod_spec():
    """The live v1 GET /pods/{id} shape: volume under networkVolumeId,
    gpuType/datacenter null."""
    return {
        "id": OLD_ID,
        "name": "tasteless_coffee_chinchilla",
        "desiredStatus": "EXITED",
        "image": "vllm/vllm-openai:latest",
        "templateId": "tmpl123",
        "env": {"VLLM_API_KEY": "sk-$RUNPOD_POD_ID"},
        "ports": ["8000/http"],
        "args": None,
        "gpuType": None,
        "gpuCount": 1,
        "containerDiskInGb": 5,
        "volumeId": None,
        "networkVolumeId": "vol123",
        "datacenter": None,
    }


def new_pod_spec(status="RUNNING"):
    spec = old_pod_spec()
    spec.update({"id": NEW_ID, "name": "fresh_pod", "desiredStatus": status})
    return spec


def make_handler(
    new_status: str = "RUNNING",
    create_status: int = 200,
    start_status: int = 500,
    start_body: dict | None = None,
):
    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == V2_HOST:
            path = request.url.path
            if path == "/v2/network-volumes/vol123":
                return httpx.Response(
                    200,
                    json={"id": "vol123", "size": 45, "dataCenter": "US-CA-3"},
                )
            if path == "/v2/pods" and request.method == "POST":
                if create_status != 200:
                    return httpx.Response(
                        create_status,
                        json={"detail": "no capacity", "status": create_status},
                    )
                return httpx.Response(201, json=new_pod_spec(new_status))
            return httpx.Response(404, json={"detail": "unexpected v2 call"})
        if request.url.host == REST_HOST:
            path = request.url.path
            if path == f"/v1/pods/{OLD_ID}":
                if request.method == "GET":
                    return httpx.Response(200, json=old_pod_spec())
                if request.method == "DELETE":
                    return httpx.Response(200, json={})
                return httpx.Response(405, json={"error": "unexpected"})
            if path == f"/v1/pods/{OLD_ID}/start":
                return httpx.Response(
                    start_status,
                    json=start_body
                    if start_body is not None
                    else {"error": CAPACITY_ERROR, "status": start_status},
                )
            if path == f"/v1/pods/{NEW_ID}" and request.method == "GET":
                return httpx.Response(200, json=new_pod_spec(new_status))
            return httpx.Response(404, json={"error": "unexpected rest call"})
        return httpx.Response(200, json={"upstream": True, "host": request.url.host})

    return handler


def _pod_app(make_app, handler, **overrides):
    base = dict(
        mode="pod", pod_id=OLD_ID, pod_port=POD_PORT, api_key=API_KEY,
        # The old pod's gpuType is null; replace falls back to the config.
        gpu_type_ids=("NVIDIA A40",),
    )
    base.update(overrides)
    return make_app(handler, **base)


def rest_calls(rec, method, path):
    return [
        r for r in rec.requests
        if r.url.host == REST_HOST and r.method == method and r.url.path == path
    ]


def v2_calls(rec, method, path):
    return [
        r for r in rec.requests
        if r.url.host == V2_HOST and r.method == method and r.url.path == path
    ]


def create_body(rec) -> dict:
    (create,) = v2_calls(rec, "POST", "/v2/pods")
    return json.loads(create.content)


# --- Pod.from_api volume mapping -------------------------------------------

def test_pod_from_api_maps_network_volume_to_volume_id():
    pod = Pod.from_api(old_pod_spec())
    assert pod.volume_id == "vol123"
    # A payload that only has the create-API spelling still maps.
    pod2 = Pod.from_api({"id": "x", "volumeId": "vol456"})
    assert pod2.volume_id == "vol456"
    # networkVolumeId wins when both are present (GET shape).
    pod3 = Pod.from_api({"id": "x", "volumeId": "a", "networkVolumeId": "b"})
    assert pod3.volume_id == "b"


# --- Detection: the real "not enough free GPUs" wording ---------------------

async def test_capacity_500_is_detected_as_migration(make_app):
    app, proxy, rec = _pod_app(make_app, make_handler())
    with pytest.raises(LifecycleError) as excinfo:
        await proxy.lifecycle.start(10.0)
    # Default policy is "fail": surface the RunPod wording plus the fix hint.
    message = str(excinfo.value)
    assert "not enough free GPUs" in message
    assert "RUNPOD_ON_MIGRATE=replace" in message
    # The pod was left untouched: no terminate, no create.
    assert not rest_calls(rec, "DELETE", f"/v1/pods/{OLD_ID}")
    assert not v2_calls(rec, "POST", "/v2/pods")


async def test_non_migration_500_body_is_reported(make_app):
    handler = make_handler(
        start_body={"error": "some other failure", "status": 500},
    )
    app, proxy, rec = _pod_app(make_app, handler)
    with pytest.raises(LifecycleError, match="some other failure"):
        await proxy.lifecycle.start(10.0)
    assert not rest_calls(rec, "DELETE", f"/v1/pods/{OLD_ID}")


# --- Replace policy ----------------------------------------------------------

async def test_replace_policy_adopts_fresh_pod_with_same_spec(make_app):
    app, proxy, rec = _pod_app(
        make_app, make_handler(),
        on_migrate="replace",
        gpu_type_ids=("NVIDIA A40",),
        upstream_api_key_template=KEY_TEMPLATE,
    )
    await proxy.lifecycle.start(10.0)

    # Old pod terminated, then a fresh pod created (in that order).
    deletes = rest_calls(rec, "DELETE", f"/v1/pods/{OLD_ID}")
    creates = v2_calls(rec, "POST", "/v2/pods")
    assert len(deletes) == 1
    assert len(creates) == 1
    assert rec.requests.index(deletes[0]) < rec.requests.index(creates[0])

    # Same spec, with the network volume re-attached (data carry-over).
    body = create_body(rec)
    assert body["templateId"] == "tmpl123"
    # v2: the network volume lives under mounts.network and the create is
    # pinned to the volume's own datacenter (fetched via GET /v2/...).
    assert body["mounts"] == {
        "network": [{"volumeId": "vol123", "path": "/workspace"}],
    }
    assert v2_calls(rec, "GET", "/v2/network-volumes/vol123")
    assert body["dataCenterIds"] == ["US-CA-3"]
    # v2: a single gpu object; config fallback for the old pod's null gpuType.
    assert body["gpu"] == {"id": "NVIDIA A40", "count": 1}
    assert body["cloud"] == "SECURE"
    assert "gpuTypeIds" not in body and "volumeId" not in body
    assert body["name"] == "tasteless_coffee_chinchilla"
    assert body["ports"] == ["8000/http"]
    assert body["env"] == {"VLLM_API_KEY": "sk-$RUNPOD_POD_ID"}

    # The proxy now points at the replacement, and the per-pod key follows
    # the new id.
    assert proxy.lifecycle.pod_id == NEW_ID
    assert proxy.target.pod_id == NEW_ID
    assert proxy.target.url == NEW_TARGET_URL
    assert proxy.state.pods_replaced == 1
    assert proxy.config.auth_headers_for_pod(proxy.target.pod_id) == {
        "authorization": expected_key(NEW_ID)
    }


async def test_replace_create_failure_keeps_old_id(make_app):
    app, proxy, rec = _pod_app(
        make_app, make_handler(create_status=500), on_migrate="replace",
    )
    with pytest.raises(LifecycleError, match="could not be created"):
        await proxy.lifecycle.start(10.0)
    # The original (already terminated) id is still what the proxy pins; the
    # operator recreates via the RunPod console. No half-adopted state.
    assert proxy.lifecycle.pod_id == OLD_ID
    assert proxy.target.pod_id == OLD_ID
    assert proxy.state.pods_replaced == 0


async def test_replace_adopts_before_waiting_for_running(make_app):
    """If the budget runs out while the replacement boots, the proxy must
    already target the replacement — not the deleted original (which would
    404 and look like "pod missing" while the new one bills in the
    background)."""
    app, proxy, rec = _pod_app(
        make_app, make_handler(new_status="EXITED"), on_migrate="replace",
    )
    with pytest.raises(LifecycleError, match="did not reach RUNNING"):
        await proxy.lifecycle.start(1.0)
    assert proxy.lifecycle.pod_id == NEW_ID
    assert proxy.target.pod_id == NEW_ID
    assert proxy.state.pods_replaced == 1
    # Subsequent status probes target the replacement, not the deleted id.
    assert rest_calls(rec, "GET", f"/v1/pods/{NEW_ID}")
    # ...and the replacement was only ever started on the old id.
    assert all(
        r.url.path != f"/v1/pods/{NEW_ID}/start" for r in rec.requests
    )


# --- End to end --------------------------------------------------------------

async def test_request_succeeds_after_replace(make_app):
    app, proxy, rec = _pod_app(
        make_app, make_handler(),
        on_migrate="replace",
        upstream_api_key_template=KEY_TEMPLATE,
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://t"
    ) as client:
        response = await client.post(
            "/v1/chat/completions", json={"model": "m", "messages": []},
        )
    assert response.status_code == 200
    assert proxy.state.state is State.WARM
    assert proxy.state.pods_replaced == 1

    # The forwarded request went to the REPLACEMENT pod, authenticated with
    # the key derived from its id.
    forwards = [r for r in rec.requests if r.url.host == NEW_POD_HOST]
    assert forwards
    assert forwards[-1].headers["authorization"] == expected_key(NEW_ID)


async def test_default_fail_policy_times_out_without_touching_pod(make_app):
    app, proxy, rec = _pod_app(
        make_app, make_handler(),
        warmup_timeout_s=1.0,
        warmup_backoff_max_s=0.1,
    )
    async with httpx.AsyncClient(
        transport=httpx.ASGITransport(app=app), base_url="http://t"
    ) as client:
        response = await client.post(
            "/v1/chat/completions", json={"model": "m", "messages": []},
        )
    assert response.status_code == 503
    assert "warmup timeout" in response.text
    assert proxy.state.state is State.COLD
    # Fail policy: the pod is left untouched for the operator to decide.
    assert not rest_calls(rec, "DELETE", f"/v1/pods/{OLD_ID}")
    assert not v2_calls(rec, "POST", "/v2/pods")


async def test_replace_retries_capacity_400_until_success(make_app):
    """A v2 create 400 whose body says 'no instances available' is a
    transient capacity shortage: the replace backs off and retries instead
    of surfacing a failure (the v1 flow had no retry at all and died on
    this exact 400)."""
    state = {"creates": 0}

    def handler(request: httpx.Request) -> httpx.Response:
        if request.url.host == V2_HOST:
            if request.url.path == "/v2/network-volumes/vol123":
                return httpx.Response(
                    200, json={"id": "vol123", "size": 45, "dataCenter": "US-CA-3"},
                )
            if request.url.path == "/v2/pods" and request.method == "POST":
                state["creates"] += 1
                if state["creates"] == 1:
                    return httpx.Response(
                        400,
                        json={"detail": "There are no longer any instances "
                                        "available with the requested "
                                        "specifications."},
                    )
                return httpx.Response(201, json=new_pod_spec())
            return httpx.Response(404, json={"detail": "unexpected v2 call"})
        if request.url.host == REST_HOST:
            path = request.url.path
            if path == f"/v1/pods/{OLD_ID}":
                if request.method == "GET":
                    return httpx.Response(200, json=old_pod_spec())
                if request.method == "DELETE":
                    return httpx.Response(200, json={})
                return httpx.Response(405, json={"error": "unexpected"})
            if path == f"/v1/pods/{OLD_ID}/start":
                return httpx.Response(500, json={"error": CAPACITY_ERROR})
            if path == f"/v1/pods/{NEW_ID}" and request.method == "GET":
                return httpx.Response(200, json=new_pod_spec())
            return httpx.Response(404, json={"error": "unexpected rest call"})
        return httpx.Response(200, json={"upstream": True, "host": request.url.host})

    app, proxy, rec = _pod_app(
        make_app, handler, on_migrate="replace", gpu_type_ids=("NVIDIA A40",),
    )
    await proxy.lifecycle.start(30.0)
    assert state["creates"] == 2  # first 400'd on capacity, retry succeeded
    assert len(v2_calls(rec, "POST", "/v2/pods")) == 2
    assert proxy.lifecycle.pod_id == NEW_ID
    assert proxy.state.pods_replaced == 1
