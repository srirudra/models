"""Small asynchronous client for the RunPod REST API."""

import logging
import re
from dataclasses import dataclass
from typing import Mapping, Sequence

import httpx

log = logging.getLogger("runpod-proxy.runpod_api")

RUNNING = "RUNNING"
EXITED = "EXITED"
TERMINATED = "TERMINATED"


class RunpodApiError(Exception):
    """A RunPod REST call failed (transport error or non-2xx)."""


class PodMigrationRequired(RunpodApiError):
    """RunPod says the pod is tied to a machine whose GPUs are no longer
    available — either its "please migrate your pod" prompt, or the REST
    variant ``"There are not enough free GPUs on the host machine to start
    this pod"``.

    A pinned pod "keeps its machine assignment and resumes onto the same
    host" (RunPod docs), so while that host is out of GPUs a plain start
    retry can never succeed.  Callers must not retry through this: either
    surface it (RUNPOD_ON_MIGRATE=fail) or terminate the pod and create a
    fresh one (RUNPOD_ON_MIGRATE=replace).  ``body`` carries the raw
    response body for diagnostics (the prompt format is undocumented — pod
    migration is beta, and the wording has changed over time).
    """

    def __init__(self, message: str, body: object = None) -> None:
        super().__init__(message)
        self.body = body


class RunpodCapacityError(RunpodApiError):
    """RunPod rejected a v2 pod create (HTTP 400) because the requested
    hardware has no capacity right now.

    The v2 API folds "no instances available with the requested
    specifications" into a plain 400 (schema violations are 422), so the
    body wording is the only discriminator.  Unlike a generic 400 this is
    transient: callers may retry (with backoff) or fall back to the next
    catalogue combination.
    """

    def __init__(self, message: str, body: object = None) -> None:
        super().__init__(message)
        self.body = body


# The prompt is beta and undocumented, and RunPod has changed its wording
# over time.  Known variants for the same condition (pod pinned to a host
# that no longer has a free GPU):
#   * console/API prompt: "please migrate your pod ..."
#   * REST v1 start 500:  "There are not enough free GPUs on the host
#                          machine to start this pod"
# Matching on stems keeps the detection tolerant to further rewordings.
MIGRATION_PATTERN = re.compile(r"migrat|not enough free gpus", re.IGNORECASE)

# v2 create 400s that mean "no capacity" rather than a bad request body.
# Verified wording (2026-08): "There are no longer any instances available
# with the requested specifications."
CAPACITY_PATTERN = re.compile(
    r"no longer any instances available|not enough (free gpus|capacity)"
    r"|no (more )?capacity|out of capacity",
    re.IGNORECASE,
)


@dataclass(frozen=True)
class Pod:
    id: str
    name: str
    desired_status: str
    image: str
    template_id: str | None
    env: Mapping[str, str]
    ports: tuple[str, ...]
    args: str = ""
    # Creation-spec fields, so a pod that must be replaced (e.g. the
    # "please migrate" prompt) can be recreated with identical hardware:
    # gpu_type is the RunPod gpuType id (e.g. "NVIDIA A40"); volume_id is a
    # network-volume id (data survives the replace), not a container disk.
    gpu_type: str = ""
    gpu_count: int = 1
    container_disk_gb: int | None = None
    volume_id: str = ""
    datacenter: str = ""

    @classmethod
    def from_api(cls, payload: dict) -> "Pod":
        disk = payload.get("containerDiskInGb")
        gpu_count = payload.get("gpuCount")
        return cls(
            id=payload.get("id") or "",
            name=payload.get("name") or "",
            desired_status=payload.get("desiredStatus") or "",
            image=payload.get("image") or "",
            template_id=payload.get("templateId"),
            env=dict(payload.get("env") or {}),
            ports=tuple(payload.get("ports") or ()),
            args=payload.get("args") or "",
            gpu_type=payload.get("gpuType") or "",
            gpu_count=int(gpu_count) if isinstance(gpu_count, int) else 1,
            container_disk_gb=int(disk) if isinstance(disk, int) else None,
            # v1 GET reports the attached network volume as "networkVolumeId"
            # (and "volumeId" stays null); "volumeId" is the create-API
            # spelling.  Reading both keeps a replace from silently dropping
            # the volume — i.e. the pod's on-disk data such as HF model
            # weights — so recreation doesn't re-download them.
            volume_id=(
                payload.get("networkVolumeId")
                or payload.get("volumeId")
                or ""
            ),
            datacenter=payload.get("datacenter") or "",
        )

    @classmethod
    def from_api_v2(cls, payload: dict) -> "Pod":
        """Parse a v2 pod object (POST /v2/pods, GET /v2/pods/{id}).

        v2 spellings win; v1 spellings are accepted as fallbacks so tests
        (and mixed v1/v2 responses) keep working: status vs desiredStatus,
        gpu{id,count} vs gpuType/gpuCount, dataCenterId vs datacenter,
        mounts.network vs networkVolumeId, disk vs containerDiskInGb.
        """
        gpu = payload.get("gpu")
        gpu_type = payload.get("gpuType") or ""
        gpu_count = payload.get("gpuCount")
        if isinstance(gpu, dict):
            gpu_type = gpu.get("id") or gpu_type
            if isinstance(gpu.get("count"), int):
                gpu_count = gpu["count"]
        mounts = payload.get("mounts")
        volume_id = ""
        if isinstance(mounts, dict):
            network = mounts.get("network")
            if isinstance(network, list) and network and isinstance(network[0], dict):
                volume_id = network[0].get("volumeId") or ""
        template = payload.get("template")
        template_id = payload.get("templateId")
        if isinstance(template, dict):
            template_id = template.get("id") or template_id
        elif isinstance(template, str) and template:
            template_id = template
        disk = payload.get("disk")
        if not isinstance(disk, int):
            disk = payload.get("containerDiskInGb")
        return cls(
            id=payload.get("id") or "",
            name=payload.get("name") or "",
            desired_status=payload.get("status") or payload.get("desiredStatus") or "",
            image=payload.get("image") or "",
            template_id=template_id,
            env=dict(payload.get("env") or {}),
            ports=tuple(payload.get("ports") or ()),
            args=payload.get("args") or "",
            gpu_type=gpu_type,
            gpu_count=int(gpu_count) if isinstance(gpu_count, int) else 1,
            container_disk_gb=int(disk) if isinstance(disk, int) else None,
            volume_id=(
                volume_id
                or payload.get("networkVolumeId")
                or payload.get("volumeId")
                or ""
            ),
            datacenter=payload.get("dataCenterId") or payload.get("datacenter") or "",
        )

    def http_ports(self) -> tuple[int, ...]:
        result = []
        for entry in self.ports:
            if not isinstance(entry, str):
                continue
            port, separator, protocol = entry.partition("/")
            if separator and protocol.casefold() == "http":
                try:
                    result.append(int(port))
                except ValueError:
                    pass
        return tuple(result)


@dataclass(frozen=True)
class Template:
    id: str
    name: str
    image_name: str
    env: Mapping[str, str]
    ports: tuple[str, ...]
    is_serverless: bool
    args: str = ""

    @classmethod
    def from_api(cls, payload: dict) -> "Template":
        return cls(
            id=payload.get("id") or "",
            name=payload.get("name") or "",
            image_name=payload.get("imageName") or "",
            env=dict(payload.get("env") or {}),
            ports=tuple(payload.get("ports") or ()),
            is_serverless=bool(payload.get("isServerless", False)),
            args=payload.get("args") or "",
        )


def model_slug(value: str) -> str:
    return re.sub(r"[^a-z0-9]+", "-", value.casefold()).strip("-")


def matches_model(*, name: str, image: str, env: Mapping[str, str], model: str, args: str = "") -> bool:
    wanted = model_slug(model)
    if not wanted:
        return False
    if wanted in model_slug(name) or wanted in model_slug(image):
        return True
    if args and wanted in model_slug(args):
        return True
    return any(
        wanted in model_slug(value)
        for value in env.values()
        if isinstance(value, str)
    )


def pod_matches_model(pod: Pod, model: str) -> bool:
    return matches_model(name=pod.name, image=pod.image, env=pod.env, model=model, args=pod.args)


def template_matches_model(template: Template, model: str) -> bool:
    return matches_model(
        name=template.name, image=template.image_name, env=template.env, model=model,
        args=template.args,
    )


class RunpodApi:
    def __init__(
        self, rest_url: str, v2_url: str, api_key: str, client: httpx.AsyncClient
    ) -> None:
        self._rest_url = rest_url.rstrip("/")
        # v2 API base (also used for pod create and network volumes).  The
        # v1 REST API's create schema predates network volumes (no
        # volumeId), which is why pod creation moved to v2.
        self._v2_url = v2_url.rstrip("/")
        self._headers = {"Authorization": f"Bearer {api_key}"}
        self._client = client

    @staticmethod
    def _migration_message(body: object) -> str:
        """The migration prompt from a RunPod response body, or ''."""
        if isinstance(body, dict):
            for key in ("message", "reason", "detail", "error", "statusMessage"):
                value = body.get(key)
                if isinstance(value, str) and MIGRATION_PATTERN.search(value):
                    return value
        return ""

    async def _request(
        self,
        method: str,
        path: str,
        operation: str,
        *,
        base: str | None = None,
        none_on_404: bool = False,
        check_migration: bool = False,
        capacity_400: bool = False,
        **kwargs,
    ) -> httpx.Response | None:
        try:
            response = await self._client.request(
                method, f"{(base or self._rest_url).rstrip('/')}{path}",
                headers=self._headers, **kwargs,
            )
        except (httpx.HTTPError, OSError) as exc:
            log.warning("%s failed (%s)", operation, type(exc).__name__)
            raise RunpodApiError(f"{operation} failed: {type(exc).__name__}") from exc
        if response.status_code == 404 and none_on_404:
            return None
        if not 200 <= response.status_code < 300:
            body: object = None
            detail = ""
            try:
                body = response.json()
                if isinstance(body, dict):
                    # v1 errors carry "message"; v2 FastAPI errors carry
                    # "detail".  Read whichever is present.
                    for key in ("message", "detail", "error", "reason"):
                        value = body.get(key)
                        if isinstance(value, str) and value.strip():
                            detail = f": {value.strip()}"
                            break
            except (ValueError, TypeError):
                pass
            if check_migration and (message := self._migration_message(body)):
                log.warning(
                    "%s returned HTTP %s with a migration prompt: %s",
                    operation, response.status_code, message,
                )
                raise PodMigrationRequired(
                    f"{operation} returned HTTP {response.status_code}: {message}",
                    body=body,
                )
            if (
                capacity_400
                and response.status_code == 400
                and CAPACITY_PATTERN.search(detail)
            ):
                log.warning("%s hit RunPod capacity: %s", operation, detail)
                raise RunpodCapacityError(
                    f"{operation} returned HTTP 400{detail}", body=body,
                )
            log.warning("%s returned HTTP %s", operation, response.status_code)
            raise RunpodApiError(
                f"{operation} returned HTTP {response.status_code}{detail}"
            )
        if check_migration:
            # The prompt is beta/undocumented: it may also arrive on a 2xx
            # start response, so scan those bodies too.
            try:
                body = response.json()
            except (ValueError, TypeError):
                body = None
            if message := self._migration_message(body):
                raise PodMigrationRequired(
                    f"{operation} returned HTTP {response.status_code}: {message}",
                    body=body,
                )
        return response

    @staticmethod
    def _array(response: httpx.Response, operation: str) -> list:
        try:
            body = response.json()
        except (ValueError, TypeError) as exc:
            raise RunpodApiError(f"{operation} returned invalid JSON") from exc
        if not isinstance(body, list):
            raise RunpodApiError(f"{operation} returned a non-array response")
        return body

    async def list_pods(self, desired_status: str | None = None) -> list[Pod]:
        params = {"desiredStatus": desired_status} if desired_status is not None else None
        response = await self._request("GET", "/pods", "list pods", params=params)
        return [Pod.from_api(item) for item in self._array(response, "list pods") if isinstance(item, dict)]

    async def get_pod(self, pod_id: str) -> Pod | None:
        operation = f"get pod {pod_id}"
        response = await self._request(
            "GET", f"/pods/{pod_id}", operation, none_on_404=True
        )
        if response is None:
            return None
        try:
            body = response.json()
        except (ValueError, TypeError) as exc:
            raise RunpodApiError(f"{operation} returned invalid JSON") from exc
        if not isinstance(body, dict):
            raise RunpodApiError(f"{operation} returned a non-object response")
        return Pod.from_api(body)

    async def start_pod(self, pod_id: str) -> None:
        # check_migration: a start blocked by the "please migrate" prompt
        # must surface as PodMigrationRequired, not a generic retriable error.
        await self._request(
            "POST", f"/pods/{pod_id}/start", f"start pod {pod_id}",
            check_migration=True,
        )

    async def stop_pod(self, pod_id: str) -> None:
        await self._request("POST", f"/pods/{pod_id}/stop", f"stop pod {pod_id}")

    async def delete_pod(self, pod_id: str) -> None:
        """Terminate a pod (RunPod's "delete").  Used by the
        RUNPOD_ON_MIGRATE=replace policy: a pod whose GPUs were stolen by
        another user cannot be started, and the beta auto-migration is
        unreliable, so terminating + creating a fresh pod is the clean path.
        """
        await self._request("DELETE", f"/pods/{pod_id}", f"delete pod {pod_id}")

    async def list_templates(
        self, *, include_public: bool = False, include_runpod: bool = False
    ) -> list[Template]:
        params = {}
        if include_public:
            params["includePublicTemplates"] = "true"
        if include_runpod:
            params["includeRunpodTemplates"] = "true"
        response = await self._request("GET", "/templates", "list templates", params=params or None)
        return [
            Template.from_api(item)
            for item in self._array(response, "list templates")
            if isinstance(item, dict)
        ]

    async def get_network_volume(self, volume_id: str) -> dict | None:
        """A v2 network volume by id, or None when it does not exist."""
        response = await self._request(
            "GET", f"/network-volumes/{volume_id}",
            f"get network volume {volume_id}",
            base=self._v2_url, none_on_404=True,
        )
        if response is None:
            return None
        try:
            body = response.json()
        except (ValueError, TypeError) as exc:
            raise RunpodApiError(
                f"get network volume {volume_id} returned invalid JSON"
            ) from exc
        return body if isinstance(body, dict) else None

    async def create_network_volume(
        self, *, name: str, size: int, datacenter: str,
    ) -> dict:
        """Create a v2 network volume (the v2 replacement of v1's
        volumeInGb) and return its object (id, size, dataCenter, ...)."""
        response = await self._request(
            "POST", "/network-volumes", f"create network volume {name}",
            base=self._v2_url,
            json={"name": name, "size": size, "dataCenter": datacenter},
        )
        try:
            body = response.json()
        except (ValueError, TypeError) as exc:
            raise RunpodApiError(
                "create network volume returned invalid JSON"
            ) from exc
        if not isinstance(body, dict):
            raise RunpodApiError("create network volume returned a non-object response")
        return body

    async def create_pod(
        self, *, name: str, template_id: str | None = None, image_name: str | None = None,
        gpu_type: str = "", gpu_count: int = 1,
        cloud_type: str = "SECURE",
        ports: Sequence[str] = (), env: Mapping[str, str] | None = None,
        container_disk_gb: int | None = None, volume_gb: int | None = None,
        volume_id: str | None = None,
        volume_mount_path: str = "/workspace",
        datacenter_ids: Sequence[str] = (),
    ) -> Pod:
        """Create a pod via the v2 API (POST /v2/pods).

        Pod creation lives on v2 because the v1 create schema has no
        volumeId (a v1 create with a network volume 400s).  v2 specifics:
        the GPU is a single {id, count} object (no priority list), the
        network volume goes under mounts.network and is datacenter-locked
        (this method pins the volume's own datacenter when the caller did
        not), and a capacity shortage arrives as a 400 whose body wording
        is matched into RunpodCapacityError (schema violations are 422).
        """
        if not template_id and not image_name:
            raise RunpodApiError("create pod requires template_id or image_name")
        if not gpu_type:
            raise RunpodApiError(
                "create pod requires gpu_type (the v2 API has no default "
                "GPU; set RUNPOD_GPU_TYPE_IDS or a catalogue gpus entry)"
            )
        datacenters = list(datacenter_ids)
        if volume_id and not datacenters:
            # A network volume cannot be attached to a pod in another
            # datacenter, so fetch the volume and pin its home DC.
            volume = await self.get_network_volume(volume_id)
            if volume is None:
                raise RunpodApiError(
                    f"network volume {volume_id} not found; it may have been "
                    "deleted with its pod"
                )
            if dc := volume.get("dataCenter"):
                datacenters = [dc]
        if volume_gb is not None:
            # A fresh volume needs a home datacenter before the pod exists.
            if not datacenters:
                raise RunpodApiError(
                    "volume_gb requires datacenter_ids: a new network "
                    "volume must be created in a specific datacenter"
                )
            volume = await self.create_network_volume(
                name=f"{name}-volume", size=volume_gb, datacenter=datacenters[0],
            )
            volume_id = volume.get("id") or None
            if not volume_id:
                raise RunpodApiError("create network volume returned no id")
        body: dict = {
            "name": name,
            "cloud": cloud_type.upper(),
            "gpu": {"id": gpu_type, "count": gpu_count},
            "templateId" if template_id else "image": template_id or image_name,
        }
        if datacenters:
            body["dataCenterIds"] = datacenters
        if ports:
            body["ports"] = list(ports)
        if env:
            body["env"] = dict(env)
        if container_disk_gb is not None:
            body["disk"] = container_disk_gb
        if volume_id:
            # Attach the (existing or freshly created) network volume so
            # its data — e.g. HF model weights — survives a
            # terminate-and-recreate.
            body["mounts"] = {
                "network": [{"volumeId": volume_id, "path": volume_mount_path}],
            }
        response = await self._request(
            "POST", "/pods", "create pod",
            base=self._v2_url, capacity_400=True, json=body,
        )
        try:
            payload = response.json()
        except (ValueError, TypeError) as exc:
            raise RunpodApiError("create pod returned invalid JSON") from exc
        if not isinstance(payload, dict):
            raise RunpodApiError("create pod returned a non-object response")
        return Pod.from_api_v2(payload)
