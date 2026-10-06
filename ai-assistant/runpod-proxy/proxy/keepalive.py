"""Background keepalive: pings a warm endpoint while the user is active,
stops after IDLE_GIVEUP_S of no real traffic so billing can stop."""
import asyncio
import logging
import time
from typing import Optional

import httpx

from .config import Config
from .lifecycle import Lifecycle, LifecycleError, POD_NOT_READY_STATUSES
from .state import EndpointState, State
from .target import UpstreamTarget

log = logging.getLogger("runpod-proxy.keepalive")


class KeepaliveLoop:
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
        self._task: Optional[asyncio.Task] = None
        self._stop = asyncio.Event()

    def start(self) -> None:
        if self._task is None or self._task.done():
            self._stop.clear()
            self._task = asyncio.create_task(self._run(), name="runpod-keepalive")

    async def stop(self) -> None:
        self._stop.set()
        task, self._task = self._task, None
        if task is not None:
            task.cancel()
            try:
                await task
            except (asyncio.CancelledError, Exception):
                pass

    async def _run(self) -> None:
        while not self._stop.is_set():
            try:
                await asyncio.wait_for(self._stop.wait(), timeout=self._config.keepalive_interval_s)
            except asyncio.TimeoutError:
                pass
            if self._stop.is_set():
                return
            await self._tick()

    async def _tick(self) -> None:
        state = self._state
        retry = getattr(self._lifecycle, "retry_pending_stops", None)
        if retry is not None:
            await retry()
        # Ping in DEGRADED too so a transient failure can recover to WARM;
        # COLD means "let the endpoint idle" (billing stop) — no pinging.
        if state.state not in (State.WARM, State.DEGRADED):
            return
        if state.last_real_traffic_at is None:
            return  # no real traffic yet: nothing to keep alive
        idle_for = time.time() - state.last_real_traffic_at
        if idle_for > self._config.idle_giveup_s:
            try:
                # Stop the backend first (pod mode: REST stop; serverless: no-op).
                # If the stop fails, stay in the current state so the next tick
                # retries — a silently still-billing pod is worse than one more ping.
                await self._lifecycle.stop()
            except LifecycleError as exc:
                log.warning("keepalive: backend stop failed (%s); will retry next tick", exc)
                return
            state.state = State.COLD
            log.info(
                "keepalive: idle for %.1fs > IDLE_GIVEUP_S %.1fs; state -> COLD (letting endpoint go idle)",
                idle_for, self._config.idle_giveup_s,
            )
            return
        timeout = max(1.0, min(self._config.keepalive_interval_s, 30.0))
        try:
            response = await asyncio.wait_for(
                self._client.get(
                    self._target.warmup_url(self._config.warmup_path),
                    headers=self._config.auth_headers_for_pod(self._target.pod_id),
                ),
                timeout=timeout,
            )
            # In pod mode, RunPod's edge proxy can start answering with a
            # synthetic 404/502/503/504 if the pod was reclaimed/crashed
            # underneath us; treat that like a transport failure so DEGRADED
            # is detected instead of a false "keepalive ok".
            if self._config.mode == "pod" and response.status_code in POD_NOT_READY_STATUSES:
                raise httpx.HTTPStatusError(
                    f"pod not ready: HTTP {response.status_code}",
                    request=response.request, response=response,
                )
            if state.state is State.DEGRADED:
                state.state = State.WARM
                log.info("keepalive: endpoint recovered; state -> WARM")
            state.consecutive_keepalive_failures = 0
            state.last_keepalive_at = time.time()
            log.debug("keepalive ok (upstream %s)", response.status_code)
        except (httpx.HTTPError, asyncio.TimeoutError, OSError) as exc:
            state.consecutive_keepalive_failures += 1
            state.keepalive_failures_total += 1
            log.warning("keepalive failed (%s), %d consecutive", type(exc).__name__, state.consecutive_keepalive_failures)
            if state.consecutive_keepalive_failures >= 3:
                state.state = State.DEGRADED
                log.warning("keepalive: 3 consecutive failures; state -> DEGRADED")
