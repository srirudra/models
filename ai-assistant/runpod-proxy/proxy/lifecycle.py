"""Backend lifecycle control: no-op for serverless, REST start/stop for pods."""
import asyncio
import logging
import time
from typing import Optional

import httpx

from .config import Config
from .health import POD_NOT_READY_STATUSES  # re-exported for warmup/keepalive
from .health import classify
from .runpod_api import (
    EXITED, RUNNING, TERMINATED, Pod, RunpodApi, RunpodApiError,
    PodMigrationRequired, RunpodCapacityError,
    model_slug, pod_matches_model, template_matches_model,
)
from .target import UpstreamTarget
from .state import EndpointState

log = logging.getLogger("runpod-proxy.lifecycle")


class LifecycleError(Exception):
    """A lifecycle start/stop call failed (transport error or non-2xx)."""


class PodNotFoundError(LifecycleError):
    """The pinned pod no longer exists on RunPod (GET /pods/{id} -> 404).

    A deleted pod's ID is never reused, so a warmup that hits this should
    fail fast instead of burning the whole budget on doomed starts.
    """


class Lifecycle:
    """Base lifecycle: serverless endpoints need no explicit start/stop.

    ``start`` accepts the warmup budget (seconds) left for the whole warmup;
    pod lifecycles spend their readiness polling on it rather than fixed
    caps, because a pod this proxy starts or creates can need minutes to
    finish loading weights even after its port starts answering.
    """

    async def start(self, budget: float | None = None) -> None:
        return None

    async def stop(self) -> None:
        return None


class ServerlessLifecycle(Lifecycle):
    """Serverless endpoints scale on demand; start/stop are no-ops."""


def _error_detail(body: object) -> str:
    """The RunPod error text from a response body, for logs and errors.

    v1 error bodies look like ``{"error": ..., "status": ...}``; v2-style
    ones use ``detail``/``title``.  Truncated so a verbose body cannot
    bloat a log line.  Returns "" when no recognizable message is present.
    """
    if isinstance(body, dict):
        for key in ("error", "message", "detail", "reason", "statusMessage"):
            value = body.get(key)
            if isinstance(value, str) and value.strip():
                return f": {value.strip()[:300]}"
    return ""


class PodLifecycle(Lifecycle):
    """Starts/stops a pinned persistent pod via the RunPod REST API."""

    def __init__(
        self, config: Config, client: httpx.AsyncClient,
        target: UpstreamTarget | None = None, state: EndpointState | None = None,
    ) -> None:
        self._config = config
        self._client = client
        self._target = target
        self._state = state
        self._api = RunpodApi(
            config.rest_api_url, config.availability_api_url,
            config.api_key, client,
        )
        # Mutable: the RUNPOD_ON_MIGRATE=replace policy swaps the pinned pod
        # for a fresh one with a new id/url; the rest of the proxy reads the
        # current pod through self.target / this attribute.
        self._pod_id = config.pod_id

    @property
    def pod_id(self) -> str:
        return self._pod_id

    @property
    def _headers(self) -> dict:
        return {"authorization": f"Bearer {self._config.api_key}"}

    @property
    def _pod_url(self) -> str:
        return f"{self._config.rest_api_url}/pods/{self._pod_id}"

    async def _desired_status(self) -> Optional[str]:
        """Current pod status, or None when it cannot be determined.

        Used to keep start/stop idempotent: RunPod rejects a start on an
        already-running pod, which would otherwise burn the whole warmup
        budget and 503 a perfectly healthy pod (e.g. after a proxy restart).
        """
        try:
            response = await self._client.get(self._pod_url, headers=self._headers)
        except (httpx.HTTPError, OSError) as exc:
            log.debug("pod status probe failed (%s); assuming unknown", type(exc).__name__)
            return None
        if response.status_code == 404:
            # A 404 on GET /pods/{id} means the pod itself is gone (deleted
            # on the RunPod side). Unlike a transport blip (unknown status,
            # retried by the caller), this can never resolve itself: RunPod
            # never reuses a deleted pod's id.
            raise PodNotFoundError(
                f"pinned pod {self._pod_id} no longer exists on RunPod "
                f"(GET /pods/{self._pod_id} -> 404) — point RUNPOD_POD_ID at a "
                "live pod, or unset it to enable pod discovery"
            )
        if not (200 <= response.status_code < 300):
            log.debug("pod status probe returned HTTP %s; assuming unknown", response.status_code)
            return None
        try:
            body = response.json()
        except ValueError:
            return None
        if not isinstance(body, dict):
            return None
        status = body.get("desiredStatus") or body.get("status")
        return status.upper() if isinstance(status, str) else None

    async def _post(self, action: str) -> None:
        url = f"{self._pod_url}/{action}"
        try:
            response = await self._client.post(url, headers=self._headers)
        except (httpx.HTTPError, OSError) as exc:
            log.warning("pod %s failed (%s)", action, type(exc).__name__)
            raise LifecycleError(f"pod {action} failed: {type(exc).__name__}") from exc
        if not (200 <= response.status_code < 300):
            body: object
            try:
                body = response.json()
            except (ValueError, TypeError):
                body = None
            detail = _error_detail(body)
            if message := RunpodApi._migration_message(body):
                raise PodMigrationRequired(
                    f"pod {action} returned HTTP {response.status_code}: {message}",
                    body=body,
                )
            log.warning(
                "pod %s returned HTTP %s%s", action, response.status_code, detail,
            )
            raise LifecycleError(
                f"pod {action} returned HTTP {response.status_code}{detail}",
            )
        # The prompt is beta/undocumented and may arrive on a 2xx body too.
        try:
            body = response.json()
        except (ValueError, TypeError):
            body = None
        if message := RunpodApi._migration_message(body):
            raise PodMigrationRequired(
                f"pod {action} returned HTTP {response.status_code}: {message}",
                body=body,
            )
        log.info("pod %s ok (HTTP %s)", action, response.status_code)

    async def start(self, budget: float | None = None) -> None:
        if await self._desired_status() == RUNNING:
            log.info("pod already running; skipping start")
            return
        try:
            await self._post("start")
        except PodMigrationRequired as exc:
            if self._config.on_migrate != "replace":
                raise LifecycleError(
                    f"pod {self._pod_id} cannot start: RunPod requires migration "
                    f"({exc}); set RUNPOD_ON_MIGRATE=replace to terminate it and "
                    "create a fresh pod instead"
                ) from exc
            await self._replace_pod(budget)

    async def _replace_pod(self, budget: float | None) -> None:
        """Terminate a pod blocked by the migration prompt and create a
        fresh one with the same spec, adopting the new id and URL.

        Network-volume data (volumeId) is re-attached and survives the
        replace; container-disk data does not.
        """
        log.warning(
            "pod %s requires migration; policy=replace: terminating and "
            "creating a fresh pod", self._pod_id,
        )
        old = None
        try:
            old = await self._api.get_pod(self._pod_id)
        except RunpodApiError as exc:
            log.warning("could not fetch pod %s before replace: %s", self._pod_id, exc)
        if old is None:
            raise LifecycleError(
                f"pod {self._pod_id} requires migration but its spec could not "
                "be fetched, so a replacement cannot be created"
            )
        try:
            await self._api.delete_pod(old.id)
            log.info("terminated pod %s", old.id)
        except RunpodApiError as exc:
            raise LifecycleError(
                f"pod {old.id} requires migration but could not be terminated: "
                f"{exc}"
            ) from exc
        # v2 create: the old pod's network volume (if any) is re-attached
        # and pins the volume's datacenter automatically; without a volume
        # the create is left unpinned so RunPod can place the pod wherever
        # the GPU type actually has capacity (pinning the stolen pod's own
        # datacenter is exactly what 400'd the v1 replace when that DC had
        # run out of the GPU type).  A capacity 400 is retried with
        # backoff inside the warmup budget instead of failing immediately.
        deadline = time.monotonic() + (
            budget if budget is not None else self._config.pod_ready_timeout_s
        )
        fresh = None
        attempt = 0
        while True:
            attempt += 1
            try:
                fresh = await self._api.create_pod(
                    name=old.name or self._config.model_name or "runpod-proxy-pod",
                    template_id=old.template_id,
                    image_name=None if old.template_id else old.image,
                    gpu_type=(
                        old.gpu_type
                        or (self._config.gpu_type_ids[0] if self._config.gpu_type_ids else "")
                    ),
                    gpu_count=old.gpu_count,
                    cloud_type=self._config.cloud_type,
                    ports=list(old.ports),
                    env=dict(old.env) if old.env else None,
                    container_disk_gb=old.container_disk_gb,
                    volume_id=old.volume_id or None,
                )
                break
            except RunpodCapacityError as exc:
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise LifecycleError(
                        f"pod {old.id} was replaced but the fresh pod could "
                        f"not be created within the budget (RunPod capacity): {exc}"
                    ) from exc
                delay = min(30.0, 2.0 * attempt)
                log.warning(
                    "replacement create hit RunPod capacity; retry %d in %.0fs: %s",
                    attempt, delay, exc,
                )
                await asyncio.sleep(min(delay, remaining))
            except RunpodApiError as exc:
                raise LifecycleError(
                    f"pod {old.id} was replaced but the fresh pod could not be "
                    f"created: {exc}"
                ) from exc
        # Adopt the new id/url immediately, before waiting for RUNNING: if
        # the budget runs out mid-boot, the next warmup must target the
        # replacement (an idempotent start on a booting pod eventually
        # succeeds), not the deleted original — which would 404 and look
        # like "pod missing" while the replacement bills in the background.
        self._pod_id = fresh.id
        if self._state is not None:
            self._state.pods_replaced += 1
        if self._target is not None:
            port = fresh.http_ports()
            self._target.set(
                f"https://{fresh.id}-{port[0] if port else self._config.pod_port}"
                ".proxy.runpod.net",
                fresh.id,
            )
        log.warning(
            "pod replaced: %s -> %s (update RUNPOD_POD_ID in the proxy env if "
            "this proxy restarts)", old.id, fresh.id,
        )
        if not await self._wait_running(fresh.id, budget):
            raise LifecycleError(
                f"replacement pod {fresh.id} did not reach RUNNING within the "
                "warmup budget"
            )

    async def _wait_running(self, pod_id: str, budget: float | None) -> bool:
        deadline = time.monotonic() + (
            budget if budget is not None else self._config.pod_ready_timeout_s
        )
        while True:
            try:
                pod = await self._api.get_pod(pod_id)
                if pod is None:
                    return False
                if pod.desired_status.upper() == RUNNING:
                    return True
            except RunpodApiError as exc:
                log.warning("replacement pod %s status probe failed: %s", pod_id, exc)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            await asyncio.sleep(min(1.0, remaining))

    async def stop(self) -> None:
        try:
            status = await self._desired_status()
        except PodNotFoundError:
            # A pod that no longer exists is already "stopped": nothing to do,
            # and this must not look like a failed stop (which would keep the
            # state and retry a doomed REST call on every tick).
            log.info("pod %s no longer exists; nothing to stop", self._pod_id)
            return
        if status == EXITED:
            log.info("pod already stopped; skipping stop")
            return
        await self._post("stop")


class DiscoveryPodLifecycle(Lifecycle):
    """Finds, resumes, or creates a pod matching the configured model."""

    def __init__(self, config: Config, client: httpx.AsyncClient,
                 target: UpstreamTarget, state: EndpointState) -> None:
        self._config = config
        self._target = target
        self._state = state
        self._api = RunpodApi(
            config.rest_api_url, config.availability_api_url,
            config.api_key, client,
        )
        self._client = client
        self._last_error = ""
        # Keep this across warmup retries.  In particular, a failed health
        # probe must not cause another billable create request.
        self._created_pod_ids: dict[str, str] = {}
        # EXITED pods this proxy itself resumed (start + probe).  Together
        # with _created_pod_ids they define ownership for stop(): a pod we
        # merely found RUNNING belongs to the operator (or another proxy
        # instance) and must never be stopped by us.
        self._started_pod_ids: set[str] = set()
        # Set when a migration-blocked pod was terminated under
        # RUNPOD_ON_MIGRATE=replace, so start() may fall through to a fresh
        # create instead of failing.
        self._migrate_replaced = False
        self._active_model = config.default_model
        self._pending_stops: set[str] = set()
        # Circuit breaker: a pod that repeatedly comes up but never becomes
        # healthy (e.g. a crash-looping model server) would otherwise burn a
        # full pod_health_timeout_s on every single client retry, forever.
        # After enough consecutive reclaims for a model, fail fast for a
        # cooldown period instead of starting another multi-minute cycle.
        self._reclaim_streak: dict[str, int] = {}
        self._circuit_open_until: dict[str, float] = {}
        # Cache of RunPod template name (casefolded) -> id.  Populated lazily
        # and only after a successful list_templates fetch, so a catalogue
        # deployment resolves template ids without repeating the HTTP call.
        self._template_name_to_id: dict[str, str] | None = None

    @property
    def active_model(self) -> str:
        return self._active_model

    @property
    def last_error(self) -> str:
        return self._last_error

    @property
    def circuit_breaker_open_s(self) -> float:
        """Seconds remaining until the circuit breaker for the active model
        closes again, or 0.0 if it is not open."""
        opens_at = self._circuit_open_until.get(self._active_model)
        if opens_at is None:
            return 0.0
        return max(0.0, opens_at - time.monotonic())

    @active_model.setter
    def active_model(self, value: str) -> None:
        self._active_model = value

    @property
    def pending_stops(self) -> set[str]:
        return self._pending_stops

    @staticmethod
    def _url(pod: Pod, port: int) -> str:
        ports = pod.http_ports()
        selected = port if port in ports else (ports[0] if ports else port)
        return f"https://{pod.id}-{selected}.proxy.runpod.net"

    def _spec(self):
        """The catalogue ModelSpec for the active model, or None."""
        return self._config.catalogue.get(self._active_model)

    def _port(self) -> int:
        spec = self._spec()
        if spec is not None and spec.port is not None:
            return spec.port
        return self._config.pod_port

    def _container_disk_gb(self) -> int | None:
        spec = self._spec()
        if spec is not None and spec.container_disk_gb is not None:
            return spec.container_disk_gb
        return self._config.container_disk_gb

    def _volume_gb(self) -> int | None:
        spec = self._spec()
        if spec is not None and spec.volume_gb is not None:
            return spec.volume_gb
        return self._config.volume_gb

    def _cloud_type(self) -> str:
        spec = self._spec()
        if spec is not None and spec.cloud_type is not None:
            return spec.cloud_type
        return self._config.cloud_type

    def _datacenter_ids(self) -> tuple[str, ...]:
        """Datacenter preference from the catalogue, if any.  v2 create
        accepts the list but has no dataCenterPriority field: RunPod picks
        whichever listed DC has capacity, so order is informational."""
        spec = self._spec()
        return spec.datacenters if spec is not None else ()

    async def _load_template_map(self) -> dict[str, str]:
        """Fetch templates and build a casefolded name -> id map."""
        templates = await self._api.list_templates()
        return {t.name.casefold(): t.id for t in templates}

    async def _template_ids_for_active_model(self) -> set[str]:
        """Resolve the active model's catalogue templates to template ids.

        Returns an empty set when there is no catalogue spec (avoiding an HTTP
        request on the discovery path of non-catalogue deployments) or when the
        templates call fails, in which case name/image/env matching is the only
        rule left.
        """
        spec = self._spec()
        if spec is None:
            return set()
        wanted = [name.casefold() for name in spec.templates]
        try:
            if self._template_name_to_id is None:
                self._template_name_to_id = await self._load_template_map()
            ids = {self._template_name_to_id[n] for n in wanted
                   if n in self._template_name_to_id}
            if any(n not in self._template_name_to_id for n in wanted):
                # A lookup missed; refresh once in case the account changed.
                self._template_name_to_id = await self._load_template_map()
                ids = {self._template_name_to_id[n] for n in wanted
                       if n in self._template_name_to_id}
            return ids
        except RunpodApiError as exc:
            self._last_error = str(exc)
            return set()

    def _match_reason(self, pod: Pod, template_ids: set[str]) -> str:
        """Return why a pod is a candidate ('' if it is not).

        Rule (a) template id is precise; rule (b) is the legacy slug heuristic.
        """
        if pod.template_id and pod.template_id in template_ids:
            return "template id"
        if pod_matches_model(pod, self._active_model):
            return "name/image/env"
        return ""

    def _select_create_templates(self, templates: list) -> list:
        """Ordered non-serverless templates to try creating from.

        Precedence: an explicit RUNPOD_TEMPLATE_NAME override, then the
        catalogue spec's templates in declaration order, then the legacy
        heuristic.
        """
        non_serverless = [t for t in templates if not t.is_serverless]
        if self._config.template_name:
            wanted = self._config.template_name.casefold()
            match = next((t for t in non_serverless if t.name.casefold() == wanted), None)
            return [match] if match else []
        spec = self._spec()
        if spec is not None:
            available = [t.name for t in non_serverless]
            resolved: list = []
            for name in spec.templates:
                match = next((t for t in non_serverless
                              if t.name.casefold() == name.casefold()), None)
                if match is None:
                    log.warning(
                        "discovery: template %r for model %r not found; available "
                        "non-serverless templates: %s",
                        name, self._active_model, available,
                    )
                    continue
                resolved.append(match)
            return resolved
        match = next((t for t in non_serverless
                      if template_matches_model(t, self._active_model)), None)
        return [match] if match else []

    async def _create_from_matrix(self, templates: list) -> Pod | None:
        """Attempt creation across templates x gpus x counts, cheapest first.

        Cost safety: the very first create_pod that returns a Pod records the
        created id, increments the counter, and stops the whole matrix.  A
        RunpodApiError provisioned nothing, so the next combination is tried.
        A hard cap bounds a pathological catalogue.
        """
        spec = self._spec()
        cap = self._config.max_create_attempts
        attempts = 0
        for template in templates:
            if spec is not None and spec.gpus:
                combos = [(gpu.id, count) for gpu in spec.gpus for count in gpu.counts()]
            else:
                combos = [(None, 1)]
            for gpu_id, count in combos:
                if attempts >= cap:
                    self._last_error = (
                        f"reached RUNPOD_MAX_CREATE_ATTEMPTS ({cap}) without a "
                        f"successful create"
                    )
                    log.warning(
                        "discovery: reached create attempt cap (%d) for model %r; "
                        "aborting matrix",
                        cap, self._active_model,
                    )
                    return None
                attempts += 1
                gpu_type_ids = (gpu_id,) if gpu_id else self._config.gpu_type_ids
                datacenter_ids = self._datacenter_ids()
                try:
                    log.info(
                        "discovery: creating pod from template %r for model %r "
                        "(gpu=%s count=%s datacenters=%s)",
                        template.name, self._active_model,
                        gpu_id or list(self._config.gpu_type_ids), count,
                        list(datacenter_ids) or "any",
                    )
                    pod = await self._api.create_pod(
                        name=model_slug(self._active_model),
                        template_id=template.id,
                        gpu_type=gpu_type_ids[0] if gpu_type_ids else "",
                        gpu_count=count,
                        cloud_type=self._cloud_type(),
                        container_disk_gb=self._container_disk_gb(),
                        volume_gb=self._volume_gb(),
                        datacenter_ids=datacenter_ids,
                    )
                except RunpodApiError as exc:
                    self._last_error = str(exc)
                    log.info(
                        "discovery: create attempt failed (template %r gpu=%s "
                        "count=%s): %s",
                        template.name, gpu_id, count, exc,
                    )
                    continue
                # A pod now exists and is billable: remember it and stop the
                # matrix before any other create can be attempted.
                self._created_pod_ids[self._active_model] = pod.id
                self._state.pod_creates += 1
                return pod
        return None

    def _log_nonmatching(self, pods: list[Pod], template_ids: set[str]) -> None:
        """DEBUG: show non-matching pods so an operator can see what was
        considered.  Only id/name/image/desiredStatus are logged; pod ``env``
        contents may hold secrets and must never reach a log record.
        """
        if not log.isEnabledFor(logging.DEBUG):
            return
        for pod in pods:
            if self._match_reason(pod, template_ids):
                continue
            log.debug(
                "discovery: non-matching pod id=%s name=%s image=%s desiredStatus=%s",
                pod.id, pod.name, pod.image, pod.desired_status,
            )

    async def _healthy(self, pod: Pod, deadline: float | None = None) -> bool:
        """Poll the pod's warmup route until it is serving the model.

        ``deadline`` (monotonic) is used for pods this proxy started or
        created: they get the full warmup budget, because a fresh pod can
        need minutes to load weights after its port starts answering.  A
        pod we merely found RUNNING is capped at pod_health_timeout_s
        (deadline=None) so one sick found pod cannot eat a whole warmup.
        """
        base = self._url(pod, self._port())
        path = self._config.warmup_path.lstrip("/")
        url = f"{base}/{path}" if path else base
        if deadline is None:
            deadline = time.monotonic() + self._config.pod_health_timeout_s
        backoff = 1.0
        while True:
            remaining = deadline - time.monotonic()
            try:
                response = await self._client.get(
                    url, headers=self._config.auth_headers_for_pod(pod.id),
                    timeout=min(10.0, max(0.01, remaining)),
                )
                healthy, reason = await classify(
                    self._client,
                    mode=self._config.pod_health_mode,
                    pod_mode=True,
                    response=response,
                    base_url=base,
                    warmup_path=self._config.warmup_path,
                    model=self._active_model,
                    headers=self._config.auth_headers_for_pod(pod.id),
                    timeout=min(10.0, max(0.01, remaining)),
                )
                if healthy:
                    return True
                self._last_error = reason
            except (httpx.HTTPError, OSError) as exc:
                self._last_error = type(exc).__name__
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return False
            await asyncio.sleep(min(backoff, remaining))
            backoff = min(backoff * 2, self._config.warmup_backoff_max_s)

    async def _ready(self, pod_id: str, deadline: float | None = None) -> Pod | None:
        if deadline is None:
            deadline = time.monotonic() + self._config.pod_ready_timeout_s
        while True:
            try:
                pod = await self._api.get_pod(pod_id)
                if pod and pod.desired_status.upper() == RUNNING:
                    return pod
            except RunpodApiError as exc:
                self._last_error = str(exc)
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return None
            await asyncio.sleep(min(1.0, remaining))

    async def _reclaim(self, pod_id: str) -> None:
        """Best-effort cleanup for a pod this lifecycle brought up."""
        log.warning("reclaiming unhealthy pod %s", pod_id)
        try:
            await self._api.stop_pod(pod_id)
        except RunpodApiError as exc:
            log.warning("failed to reclaim unhealthy pod %s (%s)", pod_id, exc)
        self._note_reclaim()

    def _note_reclaim(self) -> None:
        """Track consecutive reclaims for the active model and, once the
        threshold is hit, open the circuit breaker so further start() calls
        fail fast instead of repeating a doomed multi-minute boot cycle."""
        streak = self._reclaim_streak.get(self._active_model, 0) + 1
        self._reclaim_streak[self._active_model] = streak
        threshold = self._config.pod_circuit_breaker_threshold
        if threshold > 0 and streak >= threshold:
            cooldown = self._config.pod_circuit_breaker_cooldown_s
            self._circuit_open_until[self._active_model] = time.monotonic() + cooldown
            log.error(
                "circuit breaker open for model %r after %d consecutive "
                "reclaims (likely a crash-looping pod); failing fast for %.0fs",
                self._active_model, streak, cooldown,
            )

    def _note_healthy(self) -> None:
        """A pod became healthy: clear the failure streak for this model."""
        self._reclaim_streak.pop(self._active_model, None)
        self._circuit_open_until.pop(self._active_model, None)

    def _check_circuit_breaker(self) -> None:
        opens_at = self._circuit_open_until.get(self._active_model)
        if opens_at is None:
            return
        remaining = opens_at - time.monotonic()
        if remaining <= 0:
            # Cooldown elapsed: allow one more attempt: reset the streak so a
            # single fresh failure doesn't instantly reopen the breaker.
            self._circuit_open_until.pop(self._active_model, None)
            self._reclaim_streak.pop(self._active_model, None)
            return
        raise LifecycleError(
            f"circuit breaker open for model {self._active_model!r}: "
            f"{self._reclaim_streak.get(self._active_model, 0)} consecutive pod "
            f"failures; retry in {remaining:.0f}s (check the pod/template config)"
        )

    async def _reclaim_before_cancel(self, pod_id: str) -> None:
        """Finish cleanup even when the warmup's timeout is cancelling us."""
        task = asyncio.create_task(self._reclaim(pod_id))
        try:
            await asyncio.shield(task)
        except asyncio.CancelledError:
            await task
            raise

    async def _start_pod_with_retry(self, pod_id: str) -> None:
        """RunPod's start endpoint can return a transient 5xx immediately
        after a pod was just stopped (the backend is still cleaning up).
        A single such failure must not abandon an otherwise perfectly
        reusable pod and fall through to a costly brand-new create -- retry
        through it for a bounded budget instead.  A migration prompt is not
        retried: it can never be fixed by starting again.

        The retry window is pod_ready_timeout_s, not the warmup budget:
        burning the whole budget on start retries would starve the
        fall-through to the next candidate or a fresh create.
        """
        deadline = time.monotonic() + self._config.pod_ready_timeout_s
        backoff = 1.0
        while True:
            try:
                await self._api.start_pod(pod_id)
                return
            except PodMigrationRequired:
                # A start blocked by the "please migrate" prompt can never
                # succeed by retrying; let the caller's policy decide.
                raise
            except RunpodApiError as exc:
                self._last_error = str(exc)
                remaining = deadline - time.monotonic()
                if remaining <= 0:
                    raise
                log.warning(
                    "pod %s start failed (%s); retrying (%.1fs left)",
                    pod_id, exc, remaining,
                )
                await asyncio.sleep(min(backoff, remaining))
                backoff = min(backoff * 2, self._config.warmup_backoff_max_s)

    async def _resume_and_probe(self, pod: Pod, deadline: float | None = None) -> bool:
        """Resume an EXITED pod and return whether it became healthy."""
        started = False
        healthy = False
        try:
            await self._start_pod_with_retry(pod.id)
            started = True
            self._started_pod_ids.add(pod.id)
            self._state.pod_starts += 1
            ready = await self._ready(pod.id, deadline)
            if ready and await self._healthy(ready, deadline):
                self._target.set(self._url(ready, self._port()), ready.id)
                self._state.discoveries += 1
                healthy = True
                self._note_healthy()
                return healthy
        except PodMigrationRequired as exc:
            self._last_error = str(exc)
            if self._config.on_migrate != "replace":
                raise LifecycleError(
                    f"pod {pod.id} cannot start: RunPod requires migration "
                    f"({exc}); set RUNPOD_ON_MIGRATE=replace to terminate it "
                    "and create a fresh pod instead"
                ) from exc
            log.warning(
                "discovery: pod %s requires migration; policy=replace: "
                "terminating it", pod.id,
            )
            await self._terminate_pod(pod.id)
            self._migrate_replaced = True
            return False
        except RunpodApiError as exc:
            self._last_error = str(exc)
        finally:
            if started and not healthy:
                # This also runs when the warmup-level timeout cancels us.
                # A successful start is otherwise a billing leak.
                await self._reclaim_before_cancel(pod.id)
        return False

    async def _terminate_pod(self, pod_id: str) -> None:
        """Best-effort DELETE of a pod blocked by the migration prompt.

        Terminating keeps it from being re-discovered (as EXITED) on the
        next attempt and starting it again.  Network-volume data survives
        via volumeId when a replacement pod is created.
        """
        try:
            await self._api.delete_pod(pod_id)
            log.info("discovery: terminated pod %s", pod_id)
        except RunpodApiError as exc:
            log.warning("discovery: failed to terminate pod %s (%s)", pod_id, exc)
        self._created_pod_ids = {
            model: pid for model, pid in self._created_pod_ids.items()
            if pid != pod_id
        }
        self._started_pod_ids.discard(pod_id)
        if self._target.pod_id == pod_id:
            self._target.set("", "")

    async def start(self, budget: float | None = None) -> None:
        self._check_circuit_breaker()
        self._migrate_replaced = False
        # Pods this proxy starts or created get the full remaining warmup
        # budget to reach RUNNING and become healthy (big weight loads can
        # take minutes after the port starts answering); pods we merely
        # found RUNNING keep the pod_health_timeout_s fast-fail cap.
        deadline = time.monotonic() + (
            budget if budget is not None else self._config.warmup_timeout_s
        )
        tried = 0
        # A created pod remains the sole candidate for this lifecycle.  It is
        # deliberately remembered even after reclamation, so a retry resumes
        # it rather than provisioning a second pod.
        created_pod_id = self._created_pod_ids.get(self._active_model)
        if created_pod_id:
            try:
                pod = await self._api.get_pod(created_pod_id)
            except RunpodApiError as exc:
                self._last_error = str(exc)
                raise LifecycleError(
                    f"pod discovery failed: {self._last_error}"
                ) from exc
            if pod and pod.desired_status.upper() != TERMINATED:
                tried += 1
                log.info(
                    "discovery: resuming previously-created pod %s (%s), desiredStatus=%s",
                    pod.id, pod.name, pod.desired_status,
                )
                if pod.desired_status.upper() == RUNNING:
                    if await self._healthy(pod):
                        log.info("discovery: created pod %s (%s) healthy", pod.id, pod.name)
                        self._target.set(self._url(pod, self._port()), pod.id)
                        self._state.discoveries += 1
                        self._note_healthy()
                        return
                    log.info("discovery: created pod %s (%s) unhealthy", pod.id, pod.name)
                    await self._reclaim_before_cancel(pod.id)
                elif await self._resume_and_probe(pod, deadline):
                    log.info("discovery: created pod %s (%s) resumed and healthy", pod.id, pod.name)
                    return
                else:
                    log.info("discovery: created pod %s (%s) resume failed", pod.id, pod.name)
                if self._migrate_replaced:
                    # The old pod was terminated under RUNPOD_ON_MIGRATE=
                    # replace; continue discovery so a fresh pod can be
                    # provisioned (its id was already forgotten).
                    log.info(
                        "discovery: created pod %s was replaced for migration; "
                        "continuing discovery", pod.id,
                    )
                else:
                    raise LifecycleError(
                        f"pod discovery failed after {tried} candidates: "
                        f"{self._last_error or 'created pod is unhealthy'}"
                    )
            else:
                # The created pod was deleted or terminated out-of-band.
                # Forget its id so stop() and later retries can't target a
                # dead pod (which would fail and be retried every tick).
                log.info("discovery: created pod %s deleted or terminated; forgetting",
                         created_pod_id)
                self._created_pod_ids.pop(self._active_model, None)

        # Precise template-id matching (rule a) needs the account's templates,
        # but only when a catalogue spec exists — non-catalogue deployments
        # must not gain an extra HTTP request here.
        template_ids = await self._template_ids_for_active_model()

        try:
            running = await self._api.list_pods(RUNNING)
        except RunpodApiError as exc:
            self._last_error = str(exc)
            running = []
        matched_running = [p for p in running if self._match_reason(p, template_ids)]
        log.info(
            "discovery: %d running pod(s), %d match model %r",
            len(running), len(matched_running), self._active_model,
        )
        self._log_nonmatching(running, template_ids)
        for pod in running:
            reason = self._match_reason(pod, template_ids)
            if not reason:
                continue
            tried += 1
            log.info("discovery: pod %s matched by %s", pod.id, reason)
            if await self._healthy(pod):
                log.info("discovery: running pod %s (%s) healthy", pod.id, pod.name)
                self._target.set(self._url(pod, self._port()), pod.id)
                self._state.discoveries += 1
                self._note_healthy()
                return
            log.info("discovery: running pod %s (%s) unhealthy", pod.id, pod.name)

        try:
            exited = await self._api.list_pods(EXITED)
        except RunpodApiError as exc:
            self._last_error = str(exc)
            exited = []
        matched_exited = [p for p in exited if self._match_reason(p, template_ids)]
        log.info(
            "discovery: %d exited pod(s), %d match model %r",
            len(exited), len(matched_exited), self._active_model,
        )
        self._log_nonmatching(exited, template_ids)
        for pod in exited:
            reason = self._match_reason(pod, template_ids)
            if not reason:
                continue
            tried += 1
            log.info("discovery: pod %s matched by %s", pod.id, reason)
            if await self._resume_and_probe(pod, deadline):
                log.info("discovery: exited pod %s (%s) resumed and healthy", pod.id, pod.name)
                return
            log.info("discovery: exited pod %s (%s) resume failed", pod.id, pod.name)

        if not self._config.allow_pod_create:
            log.warning(
                "discovery: pod creation disabled (set RUNPOD_ALLOW_POD_CREATE=true to "
                "allow creating a pod for model %r)",
                self._active_model,
            )
        if self._config.allow_pod_create:
            try:
                templates = await self._api.list_templates()
            except RunpodApiError as exc:
                self._last_error = str(exc)
                templates = None
            if templates is not None:
                resolved = self._select_create_templates(templates)
                if resolved:
                    pod = await self._create_from_matrix(resolved)
                    if pod is not None:
                        healthy = False
                        try:
                            ready = await self._ready(pod.id, deadline)
                            if ready and await self._healthy(ready, deadline):
                                self._target.set(self._url(ready, self._port()), ready.id)
                                self._state.discoveries += 1
                                healthy = True
                                self._note_healthy()
                                return
                        finally:
                            if not healthy:
                                # Cleanup must survive cancellation by the
                                # warmup timeout.
                                await self._reclaim_before_cancel(pod.id)
                else:
                    log.info(
                        "discovery: no non-serverless template matched model %r",
                        self._active_model,
                    )
        raise LifecycleError(f"pod discovery failed after {tried} candidates: {self._last_error or 'no matching pod'}")

    async def stop(self) -> None:
        pod_id = self._target.pod_id or self._created_pod_ids.get(self._active_model)
        if not pod_id:
            return
        owned = pod_id in self._created_pod_ids.values() or pod_id in self._started_pod_ids
        if not owned:
            # Cost-safety invariant: a pod we merely found RUNNING belongs to
            # the operator (or another proxy instance).  Stopping it on idle
            # give-up or shutdown would kill someone else's billable
            # workload, so leave it alone.
            log.warning(
                "not stopping pod %s: it was discovered, not started or "
                "created by this proxy", pod_id,
            )
            return
        try:
            await self._api.stop_pod(pod_id)
        except RunpodApiError as exc:
            self._pending_stops.add(pod_id)
            raise LifecycleError(str(exc)) from exc
        self._pending_stops.discard(pod_id)

    async def retry_pending_stops(self) -> None:
        for pod_id in tuple(self._pending_stops):
            try:
                await self._api.stop_pod(pod_id)
            except RunpodApiError:
                continue
            self._pending_stops.discard(pod_id)
