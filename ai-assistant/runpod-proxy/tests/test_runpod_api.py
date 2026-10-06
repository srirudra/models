import json

import httpx
import pytest

from proxy.runpod_api import (
    EXITED,
    Pod,
    RunpodApi,
    RunpodApiError,
    RunpodCapacityError,
    Template,
    matches_model,
    model_slug,
    pod_matches_model,
    template_matches_model,
)


def api(handler):
    client = httpx.AsyncClient(
        transport=httpx.MockTransport(handler), base_url="https://unused"
    )
    return (
        RunpodApi("https://rest.example/v1/", "https://api.example/v2/", "secret", client),
        client,
    )


@pytest.mark.parametrize("method", ["start_pod", "stop_pod"])
async def test_actions_and_auth(method):
    seen = []

    def handler(request):
        seen.append(request)
        return httpx.Response(200)

    client_api, client = api(handler)
    await getattr(client_api, method)("pod")
    await client.aclose()
    assert seen[0].headers["authorization"] == "Bearer secret"


async def test_list_pods_and_query():
    def handler(request):
        assert request.url.params["desiredStatus"] == EXITED
        assert request.headers["Authorization"] == "Bearer secret"
        return httpx.Response(200, json=[{"id": "p", "image": "i", "ports": ["8000/http"]}])

    client_api, client = api(handler)
    pods = await client_api.list_pods(EXITED)
    await client.aclose()
    assert pods[0].http_ports() == (8000,)


async def test_empty_and_non_array_list():
    for body, expected in [([], []), ({"data": []}, RunpodApiError)]:
        client_api, client = api(lambda request, body=body: httpx.Response(200, json=body))
        if expected is RunpodApiError:
            with pytest.raises(expected):
                await client_api.list_pods()
        else:
            assert await client_api.list_pods() == expected
        await client.aclose()


def test_parsing_and_model_matching():
    pod = Pod.from_api(
        {
            "id": "p",
            "name": "qwen-qwen3-32b",
            "image": "vllm/vllm-openai:qwen-qwen3-32b",
            "env": {"MODEL_NAME": "Qwen/Qwen3-32B"},
        }
    )
    template = Template.from_api({"id": "t", "name": "n", "imageName": "image"})
    assert pod.template_id is None and pod.env["MODEL_NAME"] == "Qwen/Qwen3-32B"
    assert pod.ports == ()
    assert template.image_name == "image" and template.env == {} and template.ports == ()
    assert not template.is_serverless
    assert model_slug("  Qwen_Qwen3.32b ") == "qwen-qwen3-32b"
    assert pod_matches_model(pod, "Qwen/Qwen3-32B")
    assert matches_model(
        name="unrelated",
        image="vllm/vllm-openai:qwen-qwen3-32b",
        env={},
        model="Qwen/Qwen3-32B",
    )
    assert matches_model(
        name="unrelated",
        image="",
        env={"MODEL_NAME": "Qwen/Qwen3-32B"},
        model="Qwen/Qwen3-32B",
    )
    assert template_matches_model(template, "image")
    assert not matches_model(name="llama-3-70b", image="", env={}, model="Qwen/Qwen3-32B")
    assert not matches_model(name="", image="", env={}, model=" ")


def test_args_matching_finds_pods_not_named_after_the_model():
    """A pod's dockerArgs literally carries the model tag vLLM was launched
    with (e.g. "Qwen/Qwen3-32B --port 8000 ..."), which is a more reliable
    signal than pod naming for pods not created by this proxy's own matrix
    (e.g. legacy/manually-created pods) -- catching them avoids spinning up
    a duplicate, billable pod when one already exists for the same model."""
    pod = Pod.from_api(
        {
            "id": "legacy",
            "name": "some-unrelated-pod-name",
            "image": "vllm/vllm-openai:latest",
            "env": {},
            "args": "Qwen/Qwen3-32B --port 8000 --tensor-parallel-size 2",
        }
    )
    assert pod_matches_model(pod, "Qwen/Qwen3-32B")
    template = Template.from_api(
        {"id": "t", "name": "unrelated", "imageName": "vllm/vllm-openai:latest",
         "args": "Qwen/Qwen3-32B --port 8000"}
    )
    assert template_matches_model(template, "Qwen/Qwen3-32B")
    assert not pod_matches_model(pod, "Qwen/Qwen2-7B")


async def test_get_pod_errors_and_404():
    for status in (404, 400, 500):
        client_api, client = api(lambda request, status=status: httpx.Response(status, json={"message": "bad"}))
        if status == 404:
            assert await client_api.get_pod("p") is None
        else:
            with pytest.raises(RunpodApiError, match="bad"):
                await client_api.get_pod("p")
        await client.aclose()


@pytest.mark.parametrize("body", ["not json", []])
async def test_get_pod_invalid_response(body):
    client_api, client = api(
        lambda request, body=body: httpx.Response(
            200, content=body if isinstance(body, str) else json.dumps(body)
        )
    )
    with pytest.raises(RunpodApiError, match="returned (invalid JSON|a non-object)"):
        await client_api.get_pod("p")
    await client.aclose()


async def test_get_pod_transport_error():
    client_api, client = api(lambda request: (_ for _ in ()).throw(httpx.ConnectError("x")))
    with pytest.raises(RunpodApiError, match="get pod p failed"):
        await client_api.get_pod("p")
    await client.aclose()


@pytest.mark.parametrize("method", ["start_pod", "stop_pod"])
async def test_action_error_message(method):
    client_api, client = api(lambda request: httpx.Response(400, json={"message": "no GPU available"}))
    with pytest.raises(RunpodApiError, match="no GPU available"):
        await getattr(client_api, method)("p")
    await client.aclose()


V2_POD = {
    "id": "p", "name": "n", "status": "RUNNING", "image": "i",
    "gpu": {"id": "a", "count": 1}, "dataCenterId": "US-TX-3",
}


async def test_create_pod_body_and_parse():
    """Pod create is a v2 call: single gpu object, no priority lists, and
    the response is parsed with the v2 field spellings."""
    def handler(request):
        assert request.url.host == "api.example"
        assert request.url.path == "/v2/pods"
        body = json.loads(request.content)
        assert body["templateId"] == "t"
        assert body["gpu"] == {"id": "a", "count": 1}
        assert body["cloud"] == "SECURE"
        assert "imageName" not in body and "gpuTypeIds" not in body
        assert "gpuTypePriority" not in body and "env" not in body and "ports" not in body
        return httpx.Response(201, json=V2_POD)

    client_api, client = api(handler)
    pod = await client_api.create_pod(name="n", template_id="t", gpu_type="a")
    assert pod.id == "p" and pod.gpu_type == "a" and pod.datacenter == "US-TX-3"
    assert pod.desired_status == "RUNNING"
    with pytest.raises(RunpodApiError, match="template_id or image_name"):
        await client_api.create_pod(name="n")
    with pytest.raises(RunpodApiError, match="requires gpu_type"):
        await client_api.create_pod(name="n", template_id="t")
    await client.aclose()


async def test_create_pod_image_body():
    def handler(request):
        body = json.loads(request.content)
        assert body["image"] == "img"
        assert "templateId" not in body
        return httpx.Response(201, json=V2_POD)

    client_api, client = api(handler)
    assert (await client_api.create_pod(name="n", image_name="img", gpu_type="a")).id == "p"
    await client.aclose()


async def test_create_pod_datacenter_field():
    """Catalogue datacenters reach RunPod as dataCenterIds; v2 has no
    dataCenterPriority field, and an unpinned create omits dataCenterIds."""
    seen = []

    def handler(request):
        seen.append(json.loads(request.content))
        return httpx.Response(201, json=V2_POD)

    client_api, client = api(handler)
    pod = await client_api.create_pod(
        name="n", template_id="t", gpu_type="a",
        datacenter_ids=["US-TX-3", "US-KS-3"],
    )
    await client_api.create_pod(name="n", template_id="t", gpu_type="a")
    await client.aclose()
    assert seen[0]["dataCenterIds"] == ["US-TX-3", "US-KS-3"]
    assert "dataCenterPriority" not in seen[0]
    assert pod.datacenter == "US-TX-3"
    assert "dataCenterIds" not in seen[1]


async def test_create_pod_attaches_existing_volume_and_pins_its_datacenter():
    """A replace re-attaches the old pod's network volume under
    mounts.network and pins the volume's own datacenter (a network volume
    is DC-locked)."""
    calls = []

    def handler(request):
        calls.append((request.method, request.url.path))
        if request.url.path == "/v2/network-volumes/vol1":
            return httpx.Response(
                200, json={"id": "vol1", "size": 45, "dataCenter": "US-CA-3"},
            )
        body = json.loads(request.content)
        assert body["mounts"] == {"network": [{"volumeId": "vol1", "path": "/workspace"}]}
        assert body["dataCenterIds"] == ["US-CA-3"]
        return httpx.Response(201, json={**V2_POD,
                                         "mounts": {"network": [{"volumeId": "vol1"}]}})

    client_api, client = api(handler)
    pod = await client_api.create_pod(name="n", template_id="t", gpu_type="a",
                                      volume_id="vol1")
    await client.aclose()
    assert pod.volume_id == "vol1"
    assert ("GET", "/v2/network-volumes/vol1") in calls
    assert ("POST", "/v2/pods") in calls


async def test_create_pod_volume_without_its_datacenter_is_an_error():
    def handler(request):
        return httpx.Response(404, json={"detail": "volume not found"})

    client_api, client = api(handler)
    with pytest.raises(RunpodApiError, match="network volume vol404 not found"):
        await client_api.create_pod(name="n", template_id="t", gpu_type="a",
                                    volume_id="vol404")
    await client.aclose()


async def test_create_pod_fresh_volume_needs_a_datacenter():
    def handler(request):
        if request.url.path == "/v2/network-volumes" and request.method == "POST":
            body = json.loads(request.content)
            assert body == {"name": "n-volume", "size": 45, "dataCenter": "US-CA-3"}
            return httpx.Response(201, json={"id": "volnew", "size": 45,
                                             "dataCenter": "US-CA-3"})
        body = json.loads(request.content)
        assert body["mounts"] == {"network": [{"volumeId": "volnew", "path": "/workspace"}]}
        return httpx.Response(201, json=V2_POD)

    client_api, client = api(handler)
    pod = await client_api.create_pod(
        name="n", template_id="t", gpu_type="a", volume_gb=45,
        datacenter_ids=["US-CA-3"],
    )
    await client.aclose()
    assert pod.volume_id == ""  # the response fixture carries no mounts

    client_api, client = api(lambda r: httpx.Response(200))
    with pytest.raises(RunpodApiError, match="requires datacenter_ids"):
        await client_api.create_pod(name="n", template_id="t", gpu_type="a",
                                    volume_gb=45)
    await client.aclose()


async def test_create_pod_capacity_400_raises_capacity_error():
    """The v2 'no instances available' 400 is classified as transient
    capacity, not a schema error callers would retry-forever on."""
    client_api, client = api(lambda r: httpx.Response(
        400,
        json={"detail": "There are no longer any instances available with "
                        "the requested specifications.",
              "status": 400, "title": "Bad Request"},
    ))
    with pytest.raises(RunpodCapacityError, match="no longer any instances available"):
        await client_api.create_pod(name="n", template_id="t", gpu_type="a")
    await client.aclose()


async def test_create_pod_other_400_stays_generic():
    client_api, client = api(lambda r: httpx.Response(
        400, json={"detail": "gpu.id must be a known GPU id"},
    ))
    with pytest.raises(RunpodApiError) as excinfo:
        await client_api.create_pod(name="n", template_id="t", gpu_type="a")
    assert not isinstance(excinfo.value, RunpodCapacityError)
    assert "gpu.id must be a known GPU id" in str(excinfo.value)
    await client.aclose()


async def test_get_network_volume_and_create():
    def handler(request):
        if request.url.path == "/v2/network-volumes/v9":
            return httpx.Response(
                200, json={"id": "v9", "size": 45, "dataCenter": "US-CA-3"},
            )
        assert request.url.path == "/v2/network-volumes"
        return httpx.Response(201, json={"id": "v10", "size": 10,
                                         "dataCenter": "US-CA-3"})

    client_api, client = api(handler)
    vol = await client_api.get_network_volume("v9")
    assert vol["dataCenter"] == "US-CA-3"
    created = await client_api.create_network_volume(
        name="v", size=10, datacenter="US-CA-3")
    assert created["id"] == "v10"
    await client.aclose()

    client_api, client = api(lambda r: httpx.Response(
        404, json={"detail": "volume not found"}))
    assert await client_api.get_network_volume("gone") is None
    await client.aclose()


async def test_templates_flags_and_transport_error():
    def handler(request):
        assert request.url.params["includePublicTemplates"] == "true"
        assert request.url.params["includeRunpodTemplates"] == "true"
        return httpx.Response(200, json=[{"id": "t", "imageName": "i"}])

    client_api, client = api(handler)
    assert (await client_api.list_templates(include_public=True, include_runpod=True))[0].image_name == "i"
    await client.aclose()

    client_api, client = api(lambda request: (_ for _ in ()).throw(httpx.ConnectError("x")))
    with pytest.raises(RunpodApiError):
        await client_api.list_pods()
    await client.aclose()
