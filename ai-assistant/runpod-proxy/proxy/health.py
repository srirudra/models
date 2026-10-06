"""Readiness classification for warmup probes (POD_HEALTH_MODE).

A single source of truth for "is the endpoint actually serving the model
right now", shared by the discovery lifecycle's health loop and the
warmup manager's final probe.  The legacy behavior (mode="any") treats any
HTTP response as ready; "model" and "completion" exist because a model
server can answer its HTTP routes while its weights are still loading —
declaring WARM then exposes the first real request to a multi-minute stall.
"""
import logging

import httpx

log = logging.getLogger("runpod-proxy.health")

COMPLETION_PATH = "chat/completions"

# RunPod's own edge/ingress proxy (the "https://<pod>-<port>.proxy.runpod.net"
# layer) answers on the pod's behalf before the container inside is ready:
# 404 when nothing is listening on the port yet, and 502/503/504 once the
# port is bound but the app hasn't finished starting. These are synthetic
# "not ready" responses, not answers from the model server, so pod-mode
# health/warmup probes must not treat them as healthy — doing so declares the
# pod ready while it is still booting/loading weights, and the first real
# request then gets a confusing 404/502 from the same edge layer instead of a
# clean warmup wait.
POD_NOT_READY_STATUSES = frozenset({404, 502, 503, 504})


def _is_not_ready(status_code: int) -> bool:
    return status_code in POD_NOT_READY_STATUSES


def completion_url(base_url: str, warmup_path: str) -> str:
    """Derive the chat-completions URL from the warmup route.

    The warmup path (default "v1/models") shares its prefix with the rest of
    the model server's API, so "v1/models" -> "<base>/v1/chat/completions"
    and a bare "models" -> "<base>/chat/completions".
    """
    parts = [p for p in warmup_path.strip("/").split("/") if p]
    prefix = "/".join(parts[:-1])
    return f"{base_url.rstrip('/')}/{prefix}/{COMPLETION_PATH}" if prefix \
        else f"{base_url.rstrip('/')}/{COMPLETION_PATH}"


def _check_model_listed(response: httpx.Response, model: str) -> tuple[bool, str]:
    if _is_not_ready(response.status_code):
        return False, f"HTTP {response.status_code} (not ready)"
    if not (200 <= response.status_code < 300):
        return False, f"HTTP {response.status_code}"
    try:
        body = response.json()
    except ValueError:
        return False, "warmup response was not JSON"
    data = body.get("data") if isinstance(body, dict) else None
    if not isinstance(data, list):
        return False, "warmup response has no model list"
    ids = [m.get("id") for m in data if isinstance(m, dict) and isinstance(m.get("id"), str)]
    if not model:
        return bool(ids), f"{len(ids)} model(s) listed"
    wanted = {model.casefold(), _slug(model)}
    if any(i.casefold() in wanted or _slug(i) in wanted for i in ids):
        return True, "model listed"
    return False, f"model {model!r} not yet in warmup response ({len(ids)} listed)"


def _slug(value: str) -> str:
    import re
    return re.sub(r"[^a-z0-9]+", "-", value.casefold()).strip("-")


async def _check_completion(
    client: httpx.AsyncClient,
    base_url: str,
    warmup_path: str,
    model: str,
    headers: dict,
    timeout: float,
) -> tuple[bool, str]:
    """Functional smoke test: a 1-token completion that must actually succeed.

    This is the only probe that guarantees the model is loadable and
    serving, because it goes through the full inference path.
    """
    payload: dict = {
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": False,
        "temperature": 0,
    }
    if model:
        payload["model"] = model
    try:
        response = await client.post(
            completion_url(base_url, warmup_path),
            json=payload, headers=headers, timeout=max(0.01, timeout),
        )
    except (httpx.HTTPError, OSError) as exc:
        return False, f"completion probe failed: {type(exc).__name__}"
    if not (200 <= response.status_code < 300):
        return False, f"completion probe HTTP {response.status_code}"
    try:
        body = response.json()
    except ValueError:
        return False, "completion response was not JSON"
    if isinstance(body, dict) and body.get("choices"):
        return True, "completion ok"
    return False, "completion response had no choices"


async def classify(
    client: httpx.AsyncClient,
    *,
    mode: str,
    pod_mode: bool,
    response: httpx.Response,
    base_url: str,
    warmup_path: str,
    model: str,
    headers: dict,
    timeout: float,
) -> tuple[bool, str]:
    """Classify a warmup-route response.  Returns (healthy, reason).

    ``reason`` is a short loggable string; callers store it in
    ``last_error`` / the warmup log so ``/_status`` shows why the endpoint
    is not ready yet.
    """
    if mode == "model":
        return _check_model_listed(response, model)
    if mode == "completion":
        if pod_mode and _is_not_ready(response.status_code):
            return False, f"HTTP {response.status_code} (not ready)"
        return await _check_completion(
            client, base_url, warmup_path, model, headers, timeout
        )
    # mode == "any" (legacy): any real HTTP response means the endpoint is
    # up.  In serverless mode even a 404/502 is a genuine endpoint answer,
    # so only pod mode filters the RunPod edge synthetics.
    if pod_mode and _is_not_ready(response.status_code):
        return False, f"HTTP {response.status_code} (not ready)"
    return True, f"HTTP {response.status_code}"
