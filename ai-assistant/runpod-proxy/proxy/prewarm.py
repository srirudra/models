"""Scheduled pre-warm: wake the active model at configured local times.

Use case: a personal assistant whose model pod stops billing after idle.
Setting ``PREWARM_TIMES=08:30`` makes the proxy warm the active model (which
in discovery mode discovers or creates its pod) shortly before the workday,
so the first real request is a warm hit instead of a cold start.
"""
import asyncio
import logging
import time
from typing import Optional

from .state import State

log = logging.getLogger("runpod-proxy.prewarm")


def parse_prewarm_times(raw: str) -> tuple[tuple[int, int], ...]:
    """Parse "HH:MM,HH:MM" into ((hour, minute), ...); invalid parts are dropped."""
    result = []
    for part in raw.split(","):
        part = part.strip()
        if not part:
            continue
        try:
            hh, mm = part.split(":")
            hour, minute = int(hh), int(mm)
        except ValueError:
            log.warning("prewarm: ignoring invalid time %r (expected HH:MM)", part)
            continue
        if not (0 <= hour <= 23 and 0 <= minute <= 59):
            log.warning("prewarm: ignoring out-of-range time %r", part)
            continue
        result.append((hour, minute))
    return tuple(result)


class PrewarmScheduler:
    """Fires one warmup per configured local time per calendar day.

    A tick considers a time due when the current time is within
    ``max(120s, 2 * tick_s)`` *after* it (a missed tick never fires late by
    more than that window). Only the active model is warmed — the scheduler
    never triggers a model switch. ``now_fn`` is injectable for tests.
    """

    def __init__(self, times: tuple[tuple[int, int], ...], warmup, state,
                 tick_s: float = 20.0, now_fn=time.localtime) -> None:
        self._times = times
        self._warmup = warmup
        self._state = state
        self._tick_s = tick_s
        self._now_fn = now_fn
        self._task: Optional[asyncio.Task] = None
        self._stop = asyncio.Event()
        # (day, time-index) pairs already fired; a process restart re-fires a
        # still-in-window slot, which is harmless (warm stays warm).
        self._fired: set[tuple[str, int]] = set()

    def start(self) -> None:
        if not self._times:
            return
        if self._task is None or self._task.done():
            self._stop.clear()
            self._task = asyncio.create_task(self._run(), name="runpod-prewarm")
            log.info(
                "prewarm: scheduled for local times %s",
                ", ".join(f"{h:02d}:{m:02d}" for h, m in self._times),
            )

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
                await asyncio.wait_for(self._stop.wait(), timeout=self._tick_s)
            except asyncio.TimeoutError:
                pass
            if self._stop.is_set():
                return
            await self._tick_once()

    async def _tick_once(self, now: Optional[time.struct_time] = None) -> None:
        if now is None:
            now = self._now_fn()
        day = time.strftime("%Y-%m-%d", now)
        now_s = now.tm_hour * 3600 + now.tm_min * 60 + now.tm_sec
        window = max(120.0, 2 * self._tick_s)
        for index, (hour, minute) in enumerate(self._times):
            target_s = hour * 3600 + minute * 60
            # Seconds since the target today (modulo handles the midnight wrap).
            if (now_s - target_s) % 86400 >= window:
                continue
            if (day, index) in self._fired:
                continue
            self._fired = {(d, i) for d, i in self._fired if d == day}
            self._fired.add((day, index))
            if self._state.state is State.WARM:
                log.info("prewarm: %02d:%02d slot — already warm; nothing to do", hour, minute)
                continue
            log.info("prewarm: %02d:%02d slot — warming the active model", hour, minute)
            try:
                await self._warmup.ensure_warm()
            except Exception:
                # One failed slot is logged and dropped; the next day retries.
                log.warning("prewarm: scheduled warmup failed; dropping this slot",
                            exc_info=True)
