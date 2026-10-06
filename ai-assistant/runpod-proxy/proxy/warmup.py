"""Single-flight warmup: concurrent waiters share one warmup attempt chain."""
import asyncio
import logging
import time
from typing import Optional

import httpx

from .config import Config
from .health import classify
from .lifecycle import Lifecycle, LifecycleError, PodNotFoundError
from .state import EndpointState, State
from .target import UpstreamTarget

log = logging.getLogger("runpod-proxy.warmup")


class WarmupError(Exception):
    """A warmup could not complete; the waiting clients get an HTTP 503."""


class WarmupTimeout(WarmupError):
    """Endpoint was not warm within WARMUP_TIMEOUT_S."""


class WarmupManager:
    def __init__(
        self,
        config: Config,
        client: httpx.AsyncClient,
        state: EndpointState,
        lifecycle: Lifecycle,
        target: UpstreamTarget,
    ) -> None:
        self._config = config
        self._client = client
        self._state = state
        self._lifecycle = lifecycle
        self._target = target
        self._lock = asyncio.Lock()
        self._in_flight: Optional[asyncio.Future] = None

    async def ensure_warm(self, force: bool = False) -> None:
        """Wait until the endpoint is WARM, triggering a shared warmup if needed."""
        if self._state.state is State.WARM and not force:
            return
        async with self._lock:
            if self._state.state is State.WARM and not force:
                return
            if self._in_flight is None or self._in_flight.done():
                self._in_flight = asyncio.get_running_loop().create_future()
                asyncio.create_task(self._warm_up(self._in_flight))
            future = self._in_flight
        await future

    async def _warm_up(self, future: "asyncio.Future") -> None:
        """Drive the warmup loop, always resolving ``future`` exactly once.

        Every exit path must settle the future: concurrent callers of
        ``ensure_warm`` are awaiting it, and an unsettled future would hang
        them forever (and every later request, which joins the same future).
        """
        try:
            await self._warm_up_loop(future)
        except BaseException as exc:  # noqa: BLE001 - must never leak; see docstring
            log.exception("warmup aborted unexpectedly (%s); state -> COLD", type(exc).__name__)
            self._state.state = State.COLD
            if not future.done():
                future.set_exception(WarmupTimeout(f"warmup aborted: {type(exc).__name__}"))
            if isinstance(exc, asyncio.CancelledError):
                raise
        finally:
            if not future.done():  # defensive: no path may leave waiters hanging
                self._state.state = State.COLD
                future.set_exception(WarmupTimeout("warmup ended without result"))

    async def _warm_up_loop(self, future: "asyncio.Future") -> None:
        self._state.state = State.WARMING
        start = time.monotonic()
        deadline = start + self._config.warmup_timeout_s
        backoff = 1.0
        backend_started = False  # lifecycle.start() must succeed once per warmup
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                log.error("warmup timed out after %.1fs; state -> COLD", self._config.warmup_timeout_s)
                self._state.state = State.COLD
                future.set_exception(WarmupTimeout("endpoint warmup timeout"))
                return
            try:
                if not backend_started:
                    # The pod gets the whole remaining warmup budget to
                    # start, reach RUNNING and load its weights: a fresh pod
                    # can need minutes even after its port starts answering.
                    await asyncio.wait_for(self._lifecycle.start(remaining), timeout=remaining)
                    backend_started = True
                # GET (e.g. /v1/models) is the canonical health route: on the real
                # serverless gateway it is reliably routed to ready workers, while
                # POST probes can sit in the LB queue and hang.
                response = await asyncio.wait_for(
                    self._client.get(
                        self._target.warmup_url(self._config.warmup_path),
                        headers=self._config.auth_headers_for_pod(self._target.pod_id),
                    ),
                    timeout=remaining,
                )
                # Readiness is classified by POD_HEALTH_MODE (see
                # proxy/health.py): "any" (legacy) accepts any real HTTP
                # response; "model" requires the target model in the warmup
                # route's JSON; "completion" requires a real 1-token chat
                # completion to succeed.  In pod mode the RunPod edge proxy's
                # synthetic 404/502/503/504 (pod still booting) always count
                # as "not ready".  A model server can answer its HTTP routes
                # while its weights are still loading — declaring WARM then
                # would expose the endpoint before it can actually serve.
                pod_mode = self._config.mode == "pod"
                healthy, reason = await classify(
                    self._client,
                    mode=self._config.pod_health_mode if pod_mode else "any",
                    pod_mode=pod_mode,
                    response=response,
                    base_url=self._target.url,
                    warmup_path=self._config.warmup_path,
                    model=(getattr(self._lifecycle, "active_model", "")
                           or self._config.default_model),
                    headers=self._config.auth_headers_for_pod(self._target.pod_id),
                    timeout=min(10.0, max(0.01, remaining)),
                )
                if not healthy:
                    log.info(
                        "warmup: not ready yet (%s) — %.0fs elapsed, %.0fs budget left; retrying",
                        reason, time.monotonic() - start, deadline - time.monotonic(),
                    )
                    raise httpx.HTTPStatusError(
                        f"not ready: {reason}",
                        request=response.request, response=response,
                    )
                log.info("warmup: endpoint ready (%s); state -> WARM", reason)
                self._state.state = State.WARM
                self._state.last_warmup_at = time.time()
                self._state.consecutive_keepalive_failures = 0
                self._state.warmups += 1
                future.set_result(None)
                return
            except PodNotFoundError as exc:
                # The pinned pod was deleted: its id is never reused, so no
                # amount of retrying can succeed. Fail fast (state back to
                # COLD) with the reason instead of burning the whole budget.
                log.error("warmup failed fast: %s", exc)
                self._state.state = State.COLD
                future.set_exception(WarmupError(str(exc)))
                return
            except (LifecycleError, httpx.HTTPError, asyncio.TimeoutError, OSError) as exc:
                log.warning("warmup attempt failed (%s: %s); backoff %.1fs", type(exc).__name__, exc, backoff)
                await asyncio.sleep(min(backoff, max(remaining, 0.0)))
                backoff = min(backoff * 2, self._config.warmup_backoff_max_s)
