"""Observability: X-Request-Id correlation, JSON log format, duration
histogram, and per-model request counters on /metrics."""
import json
import logging
import sys

import httpx
import pytest

from proxy.main import JsonLogFormatter, REQUEST_ID
from proxy.state import Histogram
from tests.conftest import ok_handler

RID_HEADER = "x-request-id"


async def test_request_id_generated_and_echoed(make_app, asgi):
    app, proxy, rec = make_app(ok_handler)
    async with asgi(app) as ac:
        r = await ac.post("/v1/chat/completions", json={})

    assert r.status_code == 200
    assert RID_HEADER in r.headers
    assert len(r.headers[RID_HEADER]) == 32  # uuid4().hex


async def test_request_id_echoed_on_error_responses(make_app, asgi):
    app, proxy, rec = make_app(ok_handler, proxy_api_key="sekret")
    async with asgi(app) as ac:
        r = await ac.get("/_status")  # no key -> 401

    assert r.status_code == 401
    assert len(r.headers[RID_HEADER]) == 32


async def test_client_supplied_request_id_honoured(make_app, asgi):
    app, proxy, rec = make_app(ok_handler)
    async with asgi(app) as ac:
        r1 = await ac.post("/v1/chat/completions", json={},
                           headers={RID_HEADER: "client-trace-1"})
        r2 = await ac.post("/v1/chat/completions", json={},
                           headers={RID_HEADER: "client-trace-2"})

    assert r1.headers[RID_HEADER] == "client-trace-1"
    assert r2.headers[RID_HEADER] == "client-trace-2"


@pytest.mark.parametrize("bad", ["", "   ", "x" * 129])
async def test_malformed_client_request_id_replaced(make_app, asgi, bad):
    app, proxy, rec = make_app(ok_handler)
    async with asgi(app) as ac:
        r = await ac.post("/v1/chat/completions", json={}, headers={RID_HEADER: bad})

    assert RID_HEADER in r.headers
    assert r.headers[RID_HEADER] != bad
    assert len(r.headers[RID_HEADER]) == 32


async def test_request_id_contextvar_isolated_between_requests(make_app, asgi):
    app, proxy, rec = make_app(ok_handler)
    async with asgi(app) as ac:
        # Two sequential requests get two distinct generated ids.
        ids = {
            (await ac.post("/v1/chat/completions", json={})).headers[RID_HEADER]
            for _ in range(2)
        }
    assert len(ids) == 2


def test_json_log_formatter_fields():
    record = logging.LogRecord("runpod-proxy", logging.INFO, __file__, 1,
                               "hello %s", ("world",), None)
    payload = json.loads(JsonLogFormatter().format(record))
    assert payload["msg"] == "hello world"
    assert payload["level"] == "INFO"
    assert payload["logger"] == "runpod-proxy"
    assert payload["request_id"] == "-"  # no request context
    assert payload["ts"].endswith("+00:00")
    assert "exc" not in payload


def test_json_log_formatter_request_id_and_exception():
    token = REQUEST_ID.set("trace-abc")
    try:
        try:
            raise ValueError("boom")
        except ValueError:
            record = logging.LogRecord("runpod-proxy", logging.ERROR, __file__, 1,
                                       "failed", None, sys.exc_info())
        payload = json.loads(JsonLogFormatter().format(record))
    finally:
        REQUEST_ID.reset(token)

    assert payload["request_id"] == "trace-abc"
    assert "ValueError: boom" in payload["exc"]


def test_histogram_cumulative_buckets():
    h = Histogram((0.1, 1.0))
    for value in (0.05, 0.5, 1.0, 30.0):
        h.observe(value)

    upper, counts = list(zip(*h.buckets()))
    assert upper == ("0.1", "1.0", "+Inf")
    assert counts == (1, 3, 4)  # cumulative, last = total
    assert h.count == 4
    assert h.sum == pytest.approx(31.55)


async def test_metrics_expose_histogram_and_per_model(make_app, asgi):
    app, proxy, rec = make_app(ok_handler,
                               model_name="model-a",
                               allowed_models=("model-a", "model-b"))
    async with asgi(app) as ac:
        await ac.post("/v1/chat/completions", json={})
        await ac.post("/v1/chat/completions", json={})
        text = (await ac.get("/metrics")).text

    assert "# TYPE runpod_proxy_request_duration_seconds histogram" in text
    assert 'runpod_proxy_request_duration_seconds_bucket{le="+Inf"} 2' in text
    assert "runpod_proxy_request_duration_seconds_count 2" in text
    assert "runpod_proxy_request_duration_seconds_sum" in text
    # Every finite bucket is 2 (mock upstream is sub-50ms), cumulative.
    assert 'runpod_proxy_request_duration_seconds_bucket{le="0.05"} 2' in text
    assert 'runpod_proxy_requests_by_model{model="model-a"} 2' in text
