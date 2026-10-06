"""FastAPI app: reverse proxy to a RunPod serverless endpoint with
warmup-on-first-request and activity-based keepalive."""
import hmac
import json
import logging
import time
import uuid
from contextlib import asynccontextmanager
from contextvars import ContextVar
from datetime import datetime, timezone
from typing import AsyncIterator

import httpx
from fastapi import FastAPI, Request
from fastapi.responses import JSONResponse, PlainTextResponse, StreamingResponse
from starlette.datastructures import Headers

from .config import Config
from .health import POD_NOT_READY_STATUSES
from .models_config import ModelConfigError
from .gpu_availability import AVAILABILITY_LEVELS, GpuAvailability
from .keepalive import KeepaliveLoop
from .lifecycle import DiscoveryPodLifecycle, PodLifecycle, ServerlessLifecycle
from .prewarm import PrewarmScheduler
from .state import EndpointState, State
from .warmup import WarmupError, WarmupManager
from .target import UpstreamTarget
from .router import ModelRouter

log = logging.getLogger("runpod-proxy")

HOP_BY_HOP = frozenset({
    "connection", "keep-alive", "proxy-authenticate", "proxy-authorization",
    "te", "trailer", "transfer-encoding", "upgrade", "host", "content-length",
})

PROXY_KEY_HEADER = "x-proxy-key"
REQUEST_ID_HEADER = "x-request-id"
CONNECT_TIMEOUT_S = 10.0
# Liveness only (no state, no config): safe to leave unauthenticated so
# container healthchecks work without embedding the proxy key.
PUBLIC_PATHS = frozenset({"/_health"})

# Current request's correlation id, set by the request_id middleware.  The
# RequestIdFilter copies it onto every log record so text and JSON logs can
# be correlated per request; "-" outside a request.
REQUEST_ID: ContextVar[str] = ContextVar("runpod_proxy_request_id", default="-")


class RequestIdFilter(logging.Filter):
    def filter(self, record: logging.LogRecord) -> bool:
        record.request_id = REQUEST_ID.get()
        return True


class JsonLogFormatter(logging.Formatter):
    """One JSON object per line (LOG_FORMAT=json) for log shippers."""

    def format(self, record: logging.LogRecord) -> str:
        request_id = getattr(record, "request_id", None)
        if request_id is None:
            # Records that never passed through the RequestIdFilter (e.g. in
            # unit tests) still pick up the ambient correlation id.
            request_id = REQUEST_ID.get()
        payload = {
            "ts": datetime.fromtimestamp(record.created, tz=timezone.utc).isoformat(),
            "level": record.levelname,
            "logger": record.name,
            "request_id": request_id,
            "msg": record.getMessage(),
        }
        if record.exc_info:
            payload["exc"] = self.formatException(record.exc_info)
        return json.dumps(payload, ensure_ascii=False)


class RequestIdMiddleware:
    """Pure-ASGI correlation-id middleware (deliberately NOT @app.middleware).

    BaseHTTPMiddleware (what @app.middleware uses) cannot add headers to a
    response produced by an inner middleware that returns early without
    calling call_next — e.g. the 401 from require_proxy_key — because the
    response.start message was already sent.  Wrapping `send` directly works
    for every response the app produces.
    """

    def __init__(self, app) -> None:
        self.app = app

    async def __call__(self, scope, receive, send) -> None:
        if scope["type"] != "http":
            await self.app(scope, receive, send)
            return
        # Honour a well-formed client-supplied id (so callers can correlate
        # their own logs with ours); otherwise generate one.
        incoming = Headers(scope=scope).get(REQUEST_ID_HEADER, "").strip()
        rid = incoming if 0 < len(incoming) <= 128 else uuid.uuid4().hex
        token = REQUEST_ID.set(rid)

        async def send_with_id(message) -> None:
            if message["type"] == "http.response.start":
                headers = [
                    (k, v) for k, v in message["headers"]
                    if k.lower() != REQUEST_ID_HEADER.encode()
                ]
                headers.append((REQUEST_ID_HEADER.encode(), rid.encode()))
                message["headers"] = headers
            await send(message)

        try:
            await self.app(scope, receive, send_with_id)
        finally:
            REQUEST_ID.reset(token)


def filter_hop_by_hop(headers) -> dict:
    return {k: v for k, v in headers.items() if k.lower() not in HOP_BY_HOP}


async def _read_body(request: Request, limit: int) -> tuple[bytes, bool]:
    """Read the request body, enforcing ``limit`` bytes (<=0 means unlimited).

    Reads in a stream instead of ``request.body()`` so an oversized upload is
    rejected with 413 rather than buffered into memory (an unauthenticated
    OOM vector on a proxy that sits in front of a billable endpoint).
    """
    if limit <= 0:
        return await request.body(), False
    chunks: list[bytes] = []
    total = 0
    async for chunk in request.stream():
        total += len(chunk)
        if total > limit:
            return b"", True
        chunks.append(chunk)
    return b"".join(chunks), False


def _presented_proxy_key(headers) -> tuple[str, str]:
    """Return (key, source) for the proxy credential the client presented."""
    key = headers.get(PROXY_KEY_HEADER, "").strip()
    if key:
        return key, PROXY_KEY_HEADER
    authorization = headers.get("authorization", "").strip()
    if authorization.lower().startswith("bearer "):
        return authorization[7:].strip(), "authorization"
    return "", ""


class Proxy:
    """Wires config, state, warmup and keepalive together."""

    def __init__(self, config: Config, client: httpx.AsyncClient) -> None:
        self.config = config
        self.client = client
        self.state = EndpointState()
        self.target = UpstreamTarget(config.upstream_url, config.pod_id)
        self.lifecycle = (
            DiscoveryPodLifecycle(config, client, self.target, self.state)
            if config.discovery_enabled else
            PodLifecycle(config, client, self.target, self.state)
            if config.mode == "pod" else ServerlessLifecycle()
        )
        self.warmup = WarmupManager(config, client, self.state, self.lifecycle, self.target)
        self.router = ModelRouter(config, self.state, self.target, self.lifecycle, self.warmup)
        self.keepalive = KeepaliveLoop(config, client, self.state, self.lifecycle, self.target)
        self.prewarm = PrewarmScheduler(config.prewarm_times, self.warmup, self.state)
        self.availability = GpuAvailability(config, client)

    def note_upstream_degradation(self, reason: str) -> None:
        """Mark a WARM pod-mode endpoint DEGRADED after a real failure.

        A dead or not-yet-serving pod can answer through RunPod's edge proxy
        (synthetic 502/504) or drop the connection entirely.  Without this
        the next request would trust the WARM state (and the POD_REVALIDATE_S
        window) and hit the same broken endpoint.  DEGRADED forces the next
        request through ensure_warm(), which re-discovers/re-warms instead.
        """
        if self.config.mode != "pod" or self.state.state is not State.WARM:
            return
        self.state.state = State.DEGRADED
        log.warning("upstream degradation (%s); state -> DEGRADED", reason)

    async def shutdown_backend(self) -> None:
        """Best-effort stop of the pod-mode backend on graceful shutdown.

        Without this, stopping the proxy container leaves a discovered or
        pinned pod RUNNING (and billing) until the next container session's
        idle give-up stops it.
        """
        if self.config.mode != "pod":
            return
        try:
            await self.lifecycle.stop()
            log.info("shutdown: pod-mode backend stopped")
        except Exception:
            log.exception("shutdown: could not stop pod-mode backend (pod may keep billing)")


def create_app(config: Config, client: httpx.AsyncClient) -> FastAPI:
    proxy = Proxy(config, client)

    @asynccontextmanager
    async def lifespan(_app: FastAPI) -> AsyncIterator[None]:
        if proxy.config.mode == "pod":
            if not proxy.config.api_key:
                raise RuntimeError("RUNPOD_API_KEY is required in pod mode")
            if not proxy.config.pod_id and not proxy.config.default_model:
                raise RuntimeError("pod mode requires RUNPOD_POD_ID or RUNPOD_MODEL_NAME/RUNPOD_ALLOWED_MODELS")
        elif not proxy.config.serverless_url:
            raise RuntimeError("RUNPOD_SERVERLESS_URL is required")
        proxy.keepalive.start()
        proxy.prewarm.start()
        proxy.availability.start()
        log.info(
            "runpod-proxy started (mode=%s, endpoint=%s, keepalive every %.0fs, idle giveup %.0fs)",
            proxy.config.mode, proxy.target.url,
            proxy.config.keepalive_interval_s, proxy.config.idle_giveup_s,
        )
        try:
            yield
        finally:
            await proxy.prewarm.stop()
            await proxy.keepalive.stop()
            await proxy.availability.stop()
            await proxy.shutdown_backend()
            await client.aclose()

    app = FastAPI(title="runpod-proxy", lifespan=lifespan)
    app.state.proxy = proxy

    @app.middleware("http")
    async def require_proxy_key(request: Request, call_next):
        """Gate every route behind PROXY_API_KEY when one is configured.

        Without this the proxy is an open relay for your RunPod credit: it
        attaches your RunPod key to whatever it forwards.
        """
        if not proxy.config.proxy_api_key or request.url.path in PUBLIC_PATHS:
            return await call_next(request)
        presented, source = _presented_proxy_key(request.headers)
        if not (presented and hmac.compare_digest(presented, proxy.config.proxy_api_key)):
            return JSONResponse({"error": "unauthorized"}, status_code=401)
        request.state.proxy_key_source = source
        return await call_next(request)

    # Registered last: in this Starlette version @app.middleware also goes
    # through add_middleware (insert at index 0), so the LAST-registered
    # middleware is outermost.  request_id must sit outside
    # require_proxy_key so the id is available (and echoed) for auth
    # failures too.
    app.add_middleware(RequestIdMiddleware)

    @app.get("/_health")
    async def health():
        return JSONResponse({"ok": True})

    @app.get("/_status")
    async def status():
        view = proxy.state.status_view(proxy.target.url)
        view["mode"] = proxy.config.mode
        view["active_model"] = proxy.router.active_model
        if proxy.config.mode == "pod":
            view["pod_id"] = proxy.target.pod_id
            if proxy.config.discovery_enabled:
                view["model"] = proxy.router.active_model
        if isinstance(proxy.lifecycle, DiscoveryPodLifecycle):
            view["last_discovery_error"] = proxy.lifecycle.last_error or None
            breaker_s = proxy.lifecycle.circuit_breaker_open_s
            view["circuit_breaker_open_s"] = round(breaker_s, 1) if breaker_s else 0.0
        if proxy.config.mode == "pod" and proxy.availability.enabled:
            # Last-known RunPod GPU availability with explicit staleness
            # (age_s / last_error); poll failures keep the old values.
            view["gpu_availability"] = proxy.availability.status_view()
        return JSONResponse(view)

    @app.get("/metrics")
    async def metrics():
        """Prometheus text exposition of lifecycle and traffic counters."""
        state = proxy.state
        values = state.metrics_view()
        lines = [
            "# HELP runpod_proxy_state Current endpoint state (1 = active).",
            "# TYPE runpod_proxy_state gauge",
        ]
        for member in State:
            lines.append(
                f'runpod_proxy_state{{state="{member.value}"}} '
                f"{1 if state.state is member else 0}"
            )
        metric_types = {
            "warmups": "counter",
            "requests_total": "counter",
            "requests_warm_hit": "counter",
            "requests_cold_hit": "counter",
            "requests_failed": "counter",
            "keepalive_failures_total": "counter",
            "pod_starts": "counter",
            "pod_creates": "counter",
            "pods_replaced": "counter",
            "discoveries": "counter",
            "model_switches": "counter",
            "uptime_s": "gauge",
        }
        for name, value in values.items():
            metric = f"runpod_proxy_{name}"
            lines.append(f"# TYPE {metric} {metric_types.get(name, 'gauge')}")
            lines.append(f"{metric} {value}")
        active = proxy.router.active_model.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")
        lines.extend([
            '# TYPE runpod_proxy_active_model gauge',
            f'runpod_proxy_active_model{{model="{active}"}} 1',
        ])

        # Forwarded requests per resolved model.  NEW metric name on purpose:
        # the existing runpod_proxy_requests_total has no labels, and
        # Prometheus labels are per-family, so reusing that name with a
        # {model=...} label would break the old exposition.
        lines.extend([
            "# HELP runpod_proxy_requests_by_model Forwarded requests per resolved model.",
            "# TYPE runpod_proxy_requests_by_model counter",
        ])
        for name in sorted(state.requests_by_model):
            label = name.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")
            lines.append(f'runpod_proxy_requests_by_model{{model="{label}"}} '
                         f"{state.requests_by_model[name]}")

        # Request-duration histogram: seconds from receiving the client
        # request to the upstream's response headers, forwarded requests
        # only.  Standard Prometheus histogram text format (cumulative
        # buckets, last bound "+Inf").
        hist = state.request_duration
        lines.extend([
            "# HELP runpod_proxy_request_duration_seconds Seconds to upstream response headers (forwarded requests).",
            "# TYPE runpod_proxy_request_duration_seconds histogram",
        ])
        for le, count in hist.buckets():
            lines.append(f'runpod_proxy_request_duration_seconds_bucket{{le="{le}"}} {count}')
        lines.append(f"runpod_proxy_request_duration_seconds_sum {hist.sum:.6f}")
        lines.append(f"runpod_proxy_request_duration_seconds_count {hist.count}")

        if proxy.config.mode == "pod" and proxy.availability.enabled:
            mv = proxy.availability.metrics_view()
            lines.extend([
                "# HELP runpod_proxy_gpu_availability_age_s Seconds since the last successful GPU availability refresh; grows while a fetch fails (last-known values are kept).",
                "# TYPE runpod_proxy_gpu_availability_age_s gauge",
            ])
            if mv["age_s"] is not None:
                lines.append(f"runpod_proxy_gpu_availability_age_s {mv['age_s']}")
            lines.extend([
                "# HELP runpod_proxy_gpu_availability_level Last-known RunPod availability per GPU type (HIGH=3, MEDIUM=2, LOW=1, NONE=0, -1=unknown); absent until the first successful refresh.",
                "# TYPE runpod_proxy_gpu_availability_level gauge",
            ])
            for gpu_id, level in mv["gpus"]:
                label = gpu_id.replace("\\", "\\\\").replace('"', '\\"').replace("\n", "\\n")
                lines.append(
                    f'runpod_proxy_gpu_availability_level{{gpu="{label}"}} '
                    f"{AVAILABILITY_LEVELS.get(level, -1)}"
                )

        return PlainTextResponse("\n".join(lines) + "\n", media_type="text/plain; version=0.0.4")

    @app.post("/_warm")
    async def warm():
        try:
            # Warm the *active* model: using the default here would trigger an
            # unwanted pod switch away from whatever model traffic is on.
            async with proxy.router.request(proxy.router.active_model):
                await proxy.warmup.ensure_warm()
        except WarmupError as exc:
            return JSONResponse(
                {"error": str(exc) or "endpoint warmup failed", "state": proxy.state.state.value},
                status_code=503,
            )
        # An explicit prewarm anchors the idle window: the endpoint stays warm
        # for IDLE_GIVEUP_S so the user can start working right away.
        proxy.state.last_real_traffic_at = time.time()
        return JSONResponse({"state": proxy.state.state.value})

    @app.post("/_reload")
    async def reload():
        """Hot-reload the model catalogue from its configured source.

        The new catalogue is fully validated first; only then is it swapped in
        on the shared Config (every component reads the catalogue through it),
        so a rejected reload leaves the running catalogue untouched. The swap
        does not touch the active model, pod state, or in-flight requests: a
        model dropped from the new catalogue simply stops being routable (400),
        and the default model becomes the new catalogue's first entry.
        """
        config = proxy.config
        try:
            catalogue = config.reload_catalogue()
        except ModelConfigError as exc:
            return JSONResponse({"error": str(exc)}, status_code=400)
        # Single event loop, synchronous write: no reader can observe a
        # partially-applied swap (spec N: single worker).
        object.__setattr__(config, "catalogue", catalogue)
        object.__setattr__(config, "allowlist_configured", True)
        log.info("model catalogue reloaded: %s", list(catalogue.names))
        return JSONResponse({
            "reloaded": True,
            "source": config.catalogue_file or "inline",
            "models": list(catalogue.names),
        })

    @app.api_route("/{path:path}", methods=["GET", "POST", "PUT", "DELETE", "PATCH", "HEAD", "OPTIONS"])
    async def proxy_request(path: str, request: Request):
        state = proxy.state
        state.last_real_traffic_at = time.time()
        state.requests_total += 1
        was_warm = state.state is State.WARM
        if was_warm:
            state.requests_warm_hit += 1
        headers = filter_hop_by_hop(request.headers)
        headers["accept-encoding"] = "identity"  # keep upstream response pass-through simple
        # The proxy credential is ours, never the upstream's: strip it so it
        # cannot leak to a pod's model server (which sees client Authorization
        # when no RUNPOD_UPSTREAM_API_KEY is configured).
        headers.pop(PROXY_KEY_HEADER, None)
        if getattr(request.state, "proxy_key_source", "") == "authorization":
            headers.pop("authorization", None)
        body, too_large = await _read_body(request, proxy.config.max_body_bytes)
        if too_large:
            state.requests_failed += 1
            return JSONResponse(
                {"error": "request body too large",
                 "max_bytes": proxy.config.max_body_bytes},
                status_code=413,
            )
        requested = None
        if body and "json" in request.headers.get("content-type", "").casefold() and len(body) <= 2 * 1024 * 1024:
            try:
                parsed = json.loads(body)
                if isinstance(parsed, dict) and isinstance(parsed.get("model"), str):
                    requested = parsed["model"]
            except (ValueError, TypeError):
                pass
        model, rejected = proxy.router.resolve(requested)
        if rejected is not None:
            state.requests_failed += 1
            return JSONResponse({
                "error": "model not allowed", "model": rejected,
                "allowed": list(proxy.config.effective_allowed_models),
            }, status_code=400)
        # The pod's model server (vLLM) only knows the canonical allowlist
        # spelling, so forward that even when the client used a case/slug
        # variant that the router accepted.
        if (proxy.config.allowlist_configured and requested is not None
                and model != requested):
            parsed["model"] = model
            body = json.dumps(parsed, separators=(",", ":"),
                              ensure_ascii=False).encode("utf-8")

        lease = proxy.router.request(model)
        try:
            await lease.__aenter__()
        except WarmupError as exc:
            # A model switch runs its warmup inside __aenter__ (via _switch);
            # surface the failure like any other warmup failure.
            state.requests_failed += 1
            return JSONResponse(
                {"error": str(exc) or "endpoint warmup failed", "state": state.state.value},
                status_code=503,
            )
        try:
            if (proxy.config.discovery_enabled and was_warm and state.last_success_at is not None
                    and time.time() - state.last_success_at > proxy.config.pod_revalidate_s):
                await proxy.warmup.ensure_warm(force=True)
            # Join the in-flight warmup when one is running: forwarding into a
            # WARMING endpoint would hit a worker that is not up yet (502).
            if state.state is not State.WARM:
                await proxy.warmup.ensure_warm()
        except WarmupError as exc:
            await lease.__aexit__(None, None, None)
            state.requests_failed += 1
            return JSONResponse(
                {"error": str(exc) or "endpoint warmup failed", "state": state.state.value},
                status_code=503,
            )

        target = f"{proxy.target.url}/{path}"
        # Computed only now, after ensure_warm() has resolved target.pod_id:
        # doing this earlier (before warmup) would use the static fallback
        # key on a cold start's first request, even when
        # RUNPOD_UPSTREAM_API_KEY_TEMPLATE is configured, because the real
        # pod id isn't known yet.
        pod_auth = proxy.config.auth_headers_for_pod(proxy.target.pod_id)
        if pod_auth:
            headers.update(pod_auth)
        started = time.monotonic()
        query = [(k, v) for k, v in request.query_params.multi_items()] or None
        req = proxy.client.build_request(
            request.method,
            target,
            params=query,
            headers=headers,
            content=body,
            # A bare float would apply to every timeout category and silently
            # drop the client's short connect timeout, so a black-holed
            # upstream would hang for the full request budget.
            timeout=httpx.Timeout(proxy.config.request_timeout_s, connect=CONNECT_TIMEOUT_S),
        )
        try:
            upstream = await proxy.client.send(req, stream=True)
        except (httpx.HTTPError, OSError) as exc:
            await lease.__aexit__(None, None, None)
            state.requests_failed += 1
            log.error("forward %s /%s failed: %s", request.method, path, type(exc).__name__)
            proxy.note_upstream_degradation(type(exc).__name__)
            return JSONResponse(
                {"error": "upstream connection error", "detail": type(exc).__name__},
                status_code=502,
            )

        status_code = upstream.status_code
        # RunPod's edge proxy answers synthetically (404/502/503/504) while a
        # pod is booting or has died: treat that as degradation, not success,
        # so the next request re-warms instead of re-hitting the edge.
        if proxy.config.mode == "pod" and status_code in POD_NOT_READY_STATUSES:
            proxy.note_upstream_degradation(f"HTTP {status_code} from pod edge")
        state.last_success_at = time.time()
        # Histogram/counter measure time-to-headers: the part of a request
        # the proxy can actually influence (body streaming is pass-through).
        state.observe_request(model, time.monotonic() - started)
        resp_headers = filter_hop_by_hop(upstream.headers)
        log.info(
            "forwarded %s /%s -> %s (%.0fms)",
            request.method, path, status_code, (time.monotonic() - started) * 1000,
        )

        async def stream_body() -> AsyncIterator[bytes]:
            try:
                async for chunk in upstream.aiter_bytes():
                    yield chunk
            finally:
                await upstream.aclose()
                await lease.__aexit__(None, None, None)

        return StreamingResponse(stream_body(), status_code=status_code, headers=resp_headers)

    return app


def create_production_app() -> FastAPI:
    config = Config.from_env()
    level = getattr(logging, config.log_level.upper(), logging.INFO)
    if config.log_format == "json":
        # One JSON object per line: shippable to Loki/ELK/Splunk with a
        # line-based file tail.
        handler = logging.StreamHandler()
        handler.setFormatter(JsonLogFormatter())
    else:
        handler = logging.StreamHandler()
        handler.setFormatter(
            logging.Formatter("%(asctime)s %(levelname)s %(name)s [%(request_id)s] %(message)s")
        )
    # The filter MUST sit on the handler, not a logger: records propagated
    # from child loggers (httpx, uvicorn, ...) skip ancestor loggers'
    # filters, so a logger-level filter would leave request_id unset and the
    # %(request_id)s format would crash every such record.
    handler.addFilter(RequestIdFilter())
    logging.basicConfig(level=level, handlers=[handler])
    client = httpx.AsyncClient(timeout=httpx.Timeout(config.request_timeout_s, connect=CONNECT_TIMEOUT_S))
    return create_app(config, client)


app = create_production_app()
