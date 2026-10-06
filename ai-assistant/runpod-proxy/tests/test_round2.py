"""Round-2 enhancements: MAX_BODY_BYTES guard and PREWARM_TIMES scheduler."""
import time

import httpx

from proxy.prewarm import PrewarmScheduler, parse_prewarm_times
from proxy.state import EndpointState, State
from tests.conftest import ok_handler


# --- MAX_BODY_BYTES ---------------------------------------------------------


async def test_oversized_body_returns_413_without_upstream(make_app):
    app, proxy, rec = make_app(ok_handler, max_body_bytes=100)
    payload = {"model": "m", "content": "x" * 500}
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/v1/chat/completions", json=payload)
    assert r.status_code == 413
    assert r.json()["max_bytes"] == 100
    assert rec.requests == []  # nothing was forwarded or probed
    assert proxy.state.requests_failed == 1


async def test_body_under_limit_is_forwarded(make_app):
    app, proxy, rec = make_app(ok_handler, max_body_bytes=100_000)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post("/v1/chat/completions", json={"model": "m", "content": "x" * 5000})
    assert r.status_code == 200
    assert len(rec.non_warmup) == 1


async def test_zero_limit_means_unlimited(make_app):
    app, proxy, rec = make_app(ok_handler, max_body_bytes=0)
    big = b"z" * 300_000
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        r = await ac.post(
            "/v1/chat/completions", content=big,
            headers={"content-type": "application/json"},
        )
    assert r.status_code == 200


async def test_413_counted_in_metrics(make_app):
    app, proxy, rec = make_app(ok_handler, max_body_bytes=100)
    async with httpx.AsyncClient(transport=httpx.ASGITransport(app=app), base_url="http://t") as ac:
        await ac.post("/v1/chat/completions", json={"content": "x" * 500})
        values = {
            line.rpartition(" ")[0]: float(line.rpartition(" ")[2])
            for line in (await ac.get("/metrics")).text.splitlines()
            if line and not line.startswith("#")
        }
    assert values["runpod_proxy_requests_failed"] == 1


# --- PREWARM_TIMES ----------------------------------------------------------


def _now(hour, minute, second=0, year=2026, month=8, day=23):
    return time.struct_time((year, month, day, hour, minute, second, 6, 235, -1))


class _FakeWarmup:
    def __init__(self) -> None:
        self.calls = 0

    async def ensure_warm(self, force: bool = False) -> None:
        self.calls += 1


def _sched(times, state, tick_s=20.0):
    warmup = _FakeWarmup()
    return PrewarmScheduler(times, warmup, state, tick_s=tick_s), warmup


def test_parse_prewarm_times():
    assert parse_prewarm_times("08:30,13:00") == ((8, 30), (13, 0))
    assert parse_prewarm_times(" 08:30 , 13:00 , ") == ((8, 30), (13, 0))
    assert parse_prewarm_times("") == ()
    assert parse_prewarm_times("25:00,08:60,garbage,08:30") == ((8, 30),)


async def test_prewarm_fires_within_window_and_warms():
    state = EndpointState()
    assert state.state is State.COLD
    sched, warmup = _sched(((8, 30),), state)
    await sched._tick_once(_now(8, 30, 45))
    assert warmup.calls == 1
    # The window is 120s after the slot; outside it nothing happens.
    await sched._tick_once(_now(8, 34, 0))
    assert warmup.calls == 1


async def test_prewarm_fires_once_per_day():
    state = EndpointState()
    sched, warmup = _sched(((8, 30),), state)
    await sched._tick_once(_now(8, 30, 30))
    await sched._tick_once(_now(8, 31, 30))  # still in the window, same day
    assert warmup.calls == 1
    # A new day re-arms the slot.
    await sched._tick_once(_now(8, 30, 30, day=24))
    assert warmup.calls == 2


async def test_prewarm_handles_midnight_wrap():
    state = EndpointState()
    sched, warmup = _sched(((0, 0),), state)
    await sched._tick_once(_now(0, 1, 0))  # 60s after midnight
    assert warmup.calls == 1
    await sched._tick_once(_now(23, 59, 0, day=22))  # the day before: not due
    assert warmup.calls == 1


async def test_prewarm_skips_when_already_warm():
    state = EndpointState()
    state.state = State.WARM
    sched, warmup = _sched(((8, 30),), state)
    await sched._tick_once(_now(8, 30, 30))
    assert warmup.calls == 0


async def test_prewarm_failed_warmup_does_not_rerun_slot():
    class _Broken:
        async def ensure_warm(self, force: bool = False) -> None:
            raise RuntimeError("boom")

    state = EndpointState()
    sched = PrewarmScheduler(((8, 30),), _Broken(), state, tick_s=20.0)
    await sched._tick_once(_now(8, 30, 30))  # logs the failure
    await sched._tick_once(_now(8, 31, 0))   # slot stays dropped, no rerun
    assert state.state is not State.WARM


async def test_prewarm_scheduler_idle_when_no_times(make_app):
    app, proxy, rec = make_app(ok_handler)
    assert proxy.prewarm._times == ()
    proxy.prewarm.start()  # must be a no-op
    assert proxy.prewarm._task is None
    await proxy.prewarm.stop()


async def test_prewarm_scheduler_starts_and_stops_cleanly(make_app):
    app, proxy, rec = make_app(ok_handler, prewarm_times=((8, 30),))
    # Inject a fixed clock so the background task can never hit a real slot.
    proxy.prewarm._now_fn = lambda: _now(12, 0, 0)
    proxy.prewarm._tick_s = 0.05
    proxy.prewarm.start()
    assert proxy.prewarm._task is not None
    await proxy.prewarm.stop()
    assert proxy.prewarm._task is None
