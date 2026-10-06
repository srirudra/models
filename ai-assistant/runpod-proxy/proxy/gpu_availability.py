"""Background GPU availability poller.

Polls the RunPod v2 catalog
(``GET {availability_api_url}/catalog/gpus?include=AVAILABILITY&product=POD``)
for the GPU types referenced by the model catalogue and keeps the last-known
values.  The point is *staleness visibility*: on any fetch failure the
previous values are kept (not wiped) and the age of the data plus the last
error are surfaced in ``/_status`` and ``/metrics``.

Deliberately non-fatal: a failed or crashing poll must never take the proxy
down, so the tick swallows every exception and records it as ``last_error``.
"""
import asyncio
import logging
import time
from typing import Optional

import httpx

from .config import Config

log = logging.getLogger("runpod-proxy.gpu-availability")

# Numeric encoding for Prometheus gauges / alerting.  Unknown spellings are
# exposed as -1 rather than guessed.
AVAILABILITY_LEVELS = {"HIGH": 3, "MEDIUM": 2, "LOW": 1, "NONE": 0}


class GpuAvailability:
    def __init__(self, config: Config, client: httpx.AsyncClient) -> None:
        self._config = config
        self._client = client
        self._task: Optional[asyncio.Task] = None
        self._stop = asyncio.Event()
        # Last-known values: kept across failures so /_status can show the
        # age instead of a blank slate.
        self.last_success_at: float | None = None
        self.last_error: str | None = None
        self.gpu_info: dict[str, dict] = {}

    # ------------------------------------------------------------------ run

    @property
    def enabled(self) -> bool:
        c = self._config
        return (
            c.gpu_availability_interval_s > 0
            and c.mode == "pod"
            and bool(c.api_key)
            and bool(self.tracked_gpu_ids)
        )

    @property
    def tracked_gpu_ids(self) -> tuple[str, ...]:
        """GPU type ids to watch: the catalogue's, else config-level ids."""
        c = self._config
        ids = [g.id for spec in c.catalogue.models for g in spec.gpus]
        if not ids:
            ids = list(c.gpu_type_ids)
        seen: set[str] = set()
        out: list[str] = []
        for gpu_id in ids:
            if gpu_id and gpu_id not in seen:
                seen.add(gpu_id)
                out.append(gpu_id)
        return tuple(out)

    def start(self) -> None:
        if not self.enabled:
            return
        if self._task is None or self._task.done():
            self._stop.clear()
            self._task = asyncio.create_task(self._run(), name="runpod-gpu-availability")

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
        # Fetch immediately (the first /_status should not wait an interval),
        # then poll every interval.
        while not self._stop.is_set():
            await self._tick()
            try:
                await asyncio.wait_for(
                    self._stop.wait(), timeout=self._config.gpu_availability_interval_s
                )
            except asyncio.TimeoutError:
                pass

    async def _tick(self) -> None:
        c = self._config
        params: dict[str, str] = {"include": "AVAILABILITY", "product": "POD"}
        if c.cloud_type:
            params["cloud"] = c.cloud_type.upper()
        url = f"{c.availability_api_url}/catalog/gpus"
        try:
            response = await self._client.get(
                url,
                params=params,
                headers={"Authorization": f"Bearer {c.api_key}"},
                timeout=httpx.Timeout(30.0, connect=10.0),
            )
            response.raise_for_status()
            data = response.json()
            tracked = set(self.tracked_gpu_ids)
            info: dict[str, dict] = {}
            for gpu in data.get("gpus", []):
                gpu_id = gpu.get("id")
                if not isinstance(gpu_id, str) or gpu_id not in tracked:
                    continue
                datacenters = []
                for dc in gpu.get("dataCenters", []):
                    if isinstance(dc, dict) and dc.get("id"):
                        datacenters.append({
                            "id": dc["id"],
                            "availability": dc.get("availability", "UNKNOWN"),
                        })
                info[gpu_id] = {
                    "availability": gpu.get("availability", "UNKNOWN"),
                    "datacenters": datacenters,
                }
            self.gpu_info = info
            self.last_success_at = time.time()
            self.last_error = None
            log.debug(
                "gpu availability refreshed: %s",
                {k: v["availability"] for k, v in info.items()},
            )
        # Broad catch on purpose: this is an informational poller and a
        # malformed response, a rate limit, or a mock that raises on unknown
        # paths must never take the proxy down.  Last-known values persist.
        except Exception as exc:  # noqa: BLE001 - see above
            detail = str(exc).splitlines()[0] if str(exc) else ""
            self.last_error = f"{type(exc).__name__}" + (f": {detail}" if detail else "")
            log.warning("gpu availability refresh failed: %s", self.last_error)

    # ------------------------------------------------------------------ views

    def status_view(self) -> dict:
        """The ``gpu_availability`` block for ``/_status``."""
        c = self._config
        models = {
            spec.name: {g.id: self.gpu_info.get(g.id, {}).get("availability")
                        for g in spec.gpus}
            for spec in c.catalogue.models
        }
        return {
            "updated_at": self.last_success_at,
            "age_s": round(time.time() - self.last_success_at, 1)
            if self.last_success_at is not None else None,
            "last_error": self.last_error,
            "gpus": self.gpu_info,
            "models": models,
        }

    def metrics_view(self) -> dict:
        """Inputs for the /metrics gauges."""
        return {
            "age_s": round(time.time() - self.last_success_at, 1)
            if self.last_success_at is not None else None,
            "gpus": [(gpu_id, info["availability"])
                     for gpu_id, info in sorted(self.gpu_info.items())],
        }
