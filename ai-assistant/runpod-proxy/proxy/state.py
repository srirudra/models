"""Endpoint state machine: COLD -> WARMING -> WARM (-> DEGRADED / COLD)."""
import time
from dataclasses import dataclass, field
from enum import Enum


class State(str, Enum):
    COLD = "COLD"
    WARMING = "WARMING"
    WARM = "WARM"
    DEGRADED = "DEGRADED"


# Upper bounds (seconds) for the request-duration histogram, Prometheus-style
# (cumulative, with an implicit +Inf bucket).  Spans a warm-hit round trip
# (~ms) through a cold start of a large pod (minutes).
REQUEST_DURATION_BUCKETS = (
    0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0,
)


class Histogram:
    """Minimal Prometheus-style histogram.

    Single event loop, synchronous increments: no lock needed (the same
    guarantee every other counter in this module relies on).
    """

    def __init__(self, buckets: tuple[float, ...]) -> None:
        self._upper = tuple(buckets)
        self._counts = [0] * (len(self._upper) + 1)  # last slot: +Inf
        self.sum = 0.0
        self.count = 0

    def observe(self, value: float) -> None:
        self.sum += value
        self.count += 1
        for index, upper in enumerate(self._upper):
            if value <= upper:
                self._counts[index] += 1
                break
        else:
            self._counts[-1] += 1

    def buckets(self) -> list[tuple[str, int]]:
        """Cumulative (le, count) pairs, last bound the string "+Inf"."""
        running = 0
        result = []
        for upper, count in list(zip(self._upper, self._counts)) + [("+Inf", self._counts[-1])]:
            running += count
            result.append((str(upper), running))
        return result


@dataclass
class EndpointState:
    state: State = State.COLD
    last_warmup_at: float | None = None
    last_keepalive_at: float | None = None
    last_real_traffic_at: float | None = None
    last_success_at: float | None = None
    consecutive_keepalive_failures: int = 0
    started_at: float = field(default_factory=time.time)
    warmups: int = 0
    requests_total: int = 0
    requests_warm_hit: int = 0
    requests_failed: int = 0
    keepalive_failures_total: int = 0
    pod_starts: int = 0
    pod_creates: int = 0
    pods_replaced: int = 0
    discoveries: int = 0
    model_switches: int = 0
    # Observability: per-forwarded-request duration (upstream send time) and
    # per-model request counts, exposed via /metrics.
    request_duration: Histogram = field(
        default_factory=lambda: Histogram(REQUEST_DURATION_BUCKETS)
    )
    requests_by_model: dict[str, int] = field(default_factory=dict)

    def observe_request(self, model: str, seconds: float) -> None:
        self.request_duration.observe(seconds)
        self.requests_by_model[model] = self.requests_by_model.get(model, 0) + 1

    def status_view(self, endpoint: str) -> dict:
        return {
            "state": self.state.value,
            "endpoint": endpoint,
            "last_warmup_at": self.last_warmup_at,
            "last_keepalive_at": self.last_keepalive_at,
            "last_real_traffic_at": self.last_real_traffic_at,
            "consecutive_keepalive_failures": self.consecutive_keepalive_failures,
            "uptime_s": round(time.time() - self.started_at, 3),
            "pod_starts": self.pod_starts,
            "pod_creates": self.pod_creates,
            "pods_replaced": self.pods_replaced,
            "discoveries": self.discoveries,
            "model_switches": self.model_switches,
        }

    def metrics_view(self) -> dict:
        return {
            "warmups": self.warmups,
            "requests_total": self.requests_total,
            "requests_warm_hit": self.requests_warm_hit,
            "requests_cold_hit": self.requests_total - self.requests_warm_hit,
            "requests_failed": self.requests_failed,
            "keepalive_failures_total": self.keepalive_failures_total,
            "uptime_s": round(time.time() - self.started_at, 3),
            "pod_starts": self.pod_starts,
            "pod_creates": self.pod_creates,
            "pods_replaced": self.pods_replaced,
            "discoveries": self.discoveries,
        }
