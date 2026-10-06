//! State machine COLD -> WARMING -> WARM -> (DEGRADED) -> COLD + counters
//! (spec section 4). WI-06.
//!
//! One global state per proxy process (the proxy serves one active model at a
//! time). The state is shared across the warmup task, the keepalive task, and
//! the request pipeline, so it lives behind `Arc<std::sync::Mutex<_>>`; the
//! critical sections are short field reads/writes (never held across an await).

use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// Backend endpoint states (spec section 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// Backend assumed down; let it idle (billing stopped).
    Cold,
    /// A shared warmup attempt is in progress.
    Warming,
    /// Backend confirmed answering.
    Warm,
    /// Backend may be down (3 consecutive probe failures); still probed to heal.
    Degraded,
}

impl State {
    /// All states in the canonical order (parity with Python `for m in State`).
    pub const ALL: [State; 4] = [State::Cold, State::Warming, State::Warm, State::Degraded];

    /// The wire/log spelling of the state (matches the Python `State.value`).
    pub fn as_str(self) -> &'static str {
        match self {
            State::Cold => "COLD",
            State::Warming => "WARMING",
            State::Warm => "WARM",
            State::Degraded => "DEGRADED",
        }
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Upper bounds (seconds) for the request-duration histogram, Prometheus-style
/// (cumulative, with an implicit +Inf bucket). Spans a warm-hit round trip
/// (~ms) through a cold start of a large pod (minutes).
pub const REQUEST_DURATION_BUCKETS: &[f64] =
    &[0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0];

/// Minimal Prometheus-style histogram: cumulative counts with an implicit
/// +Inf bucket. Single event loop, synchronous increments (no lock needed
/// beyond the one guarding the whole `EndpointState`).
#[derive(Debug, Clone)]
pub struct Histogram {
    upper: Vec<f64>,
    counts: Vec<u64>,
    sum: f64,
    count: u64,
}

impl Histogram {
    /// Build a histogram over the given upper bounds.
    pub fn new(buckets: &[f64]) -> Self {
        let upper = buckets.to_vec();
        let counts = vec![0u64; upper.len() + 1]; // last slot: +Inf
        Self {
            upper,
            counts,
            sum: 0.0,
            count: 0,
        }
    }

    /// Record an observation (seconds).
    pub fn observe(&mut self, value: f64) {
        self.sum += value;
        self.count += 1;
        for (index, &upper) in self.upper.iter().enumerate() {
            if value <= upper {
                self.counts[index] += 1;
                return;
            }
        }
        let last = self.counts.len() - 1;
        self.counts[last] += 1;
    }

    /// Cumulative `(le, count)` pairs; the last bound is the string `"+Inf"`.
    pub fn buckets(&self) -> Vec<(String, u64)> {
        let mut running = 0u64;
        let mut result = Vec::with_capacity(self.upper.len() + 1);
        for (upper, &count) in self.upper.iter().zip(self.counts.iter()) {
            running += count;
            result.push((py_float_str(*upper), running));
        }
        running += self.counts[self.counts.len() - 1];
        result.push(("+Inf".to_string(), running));
        result
    }

    /// Sum of all observations (seconds).
    pub fn sum(&self) -> f64 {
        self.sum
    }

    /// Number of observations.
    pub fn count(&self) -> u64 {
        self.count
    }
}

/// Python `str()` of a float: whole numbers keep a trailing `.0`
/// (`str(1.0) == "1.0"`), matching the reference metrics output.
pub(crate) fn py_float_str(value: f64) -> String {
    let s = format!("{value}");
    if s.contains('.') || s.contains('e') || s.contains("inf") || s.contains("nan") {
        s
    } else {
        format!("{s}.0")
    }
}

/// Wall-clock seconds since the Unix epoch (parity with Python `time.time()`).
pub fn now_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Round to 3 decimals (parity with Python `round(x, 3)` for uptime).
fn round3(value: f64) -> f64 {
    (value * 1000.0).round() / 1000.0
}

/// Mutable endpoint state + observability counters (spec section 4).
#[derive(Debug)]
pub struct EndpointState {
    pub state: State,
    pub last_warmup_at: Option<f64>,
    pub last_keepalive_at: Option<f64>,
    pub last_real_traffic_at: Option<f64>,
    pub last_success_at: Option<f64>,
    pub consecutive_keepalive_failures: u32,
    pub started_at: f64,
    pub warmups: u64,
    pub requests_total: u64,
    pub requests_warm_hit: u64,
    pub requests_failed: u64,
    pub keepalive_failures_total: u64,
    pub pod_starts: u64,
    pub pod_creates: u64,
    pub pods_replaced: u64,
    pub discoveries: u64,
    pub model_switches: u64,
    /// Per-forwarded-request duration (upstream send time), for /metrics.
    pub request_duration: Histogram,
    /// Per-model request counts, for /metrics.
    pub requests_by_model: HashMap<String, u64>,
}

impl Default for EndpointState {
    fn default() -> Self {
        Self {
            state: State::Cold,
            last_warmup_at: None,
            last_keepalive_at: None,
            last_real_traffic_at: None,
            last_success_at: None,
            consecutive_keepalive_failures: 0,
            started_at: now_secs(),
            warmups: 0,
            requests_total: 0,
            requests_warm_hit: 0,
            requests_failed: 0,
            keepalive_failures_total: 0,
            pod_starts: 0,
            pod_creates: 0,
            pods_replaced: 0,
            discoveries: 0,
            model_switches: 0,
            request_duration: Histogram::new(REQUEST_DURATION_BUCKETS),
            requests_by_model: HashMap::new(),
        }
    }
}

impl EndpointState {
    /// New state with a fresh start time.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a forwarded request's duration and per-model count.
    pub fn observe_request(&mut self, model: &str, seconds: f64) {
        self.request_duration.observe(seconds);
        *self.requests_by_model.entry(model.to_string()).or_insert(0) += 1;
    }

    /// The `/_status` view (spec section 8.1). Consumed by WI-11.
    pub fn status_view(&self, endpoint: &str) -> serde_json::Value {
        serde_json::json!({
            "state": self.state.as_str(),
            "endpoint": endpoint,
            "last_warmup_at": self.last_warmup_at,
            "last_keepalive_at": self.last_keepalive_at,
            "last_real_traffic_at": self.last_real_traffic_at,
            "consecutive_keepalive_failures": self.consecutive_keepalive_failures,
            "uptime_s": round3(now_secs() - self.started_at),
            "pod_starts": self.pod_starts,
            "pod_creates": self.pod_creates,
            "pods_replaced": self.pods_replaced,
            "discoveries": self.discoveries,
            "model_switches": self.model_switches,
        })
    }

    /// The `/metrics` counters view (spec section 8.1). Consumed by WI-11.
    pub fn metrics_view(&self) -> serde_json::Value {
        serde_json::json!({
            "warmups": self.warmups,
            "requests_total": self.requests_total,
            "requests_warm_hit": self.requests_warm_hit,
            "requests_cold_hit": self.requests_total - self.requests_warm_hit,
            "requests_failed": self.requests_failed,
            "keepalive_failures_total": self.keepalive_failures_total,
            "uptime_s": round3(now_secs() - self.started_at),
            "pod_starts": self.pod_starts,
            "pod_creates": self.pod_creates,
            "pods_replaced": self.pods_replaced,
            "discoveries": self.discoveries,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_as_str_matches_python_values() {
        assert_eq!(State::Cold.as_str(), "COLD");
        assert_eq!(State::Warming.as_str(), "WARMING");
        assert_eq!(State::Warm.as_str(), "WARM");
        assert_eq!(State::Degraded.as_str(), "DEGRADED");
    }

    #[test]
    fn default_state_is_cold_with_zero_counters() {
        let s = EndpointState::new();
        assert_eq!(s.state, State::Cold);
        assert_eq!(s.warmups, 0);
        assert_eq!(s.requests_total, 0);
        assert_eq!(s.consecutive_keepalive_failures, 0);
        assert!(s.last_warmup_at.is_none());
        assert!(s.last_real_traffic_at.is_none());
    }

    #[test]
    fn histogram_buckets_are_cumulative_with_inf() {
        let mut h = Histogram::new(&[1.0, 2.0]);
        h.observe(0.5);
        h.observe(1.5);
        h.observe(3.0);
        let b = h.buckets();
        // (le, cumulative count)
        assert_eq!(b[0], ("1.0".to_string(), 1));
        assert_eq!(b[1], ("2.0".to_string(), 2));
        assert_eq!(b[2], ("+Inf".to_string(), 3));
        assert_eq!(h.count(), 3);
    }

    #[test]
    fn histogram_all_in_inf_bucket() {
        let mut h = Histogram::new(&[1.0]);
        h.observe(5.0);
        let b = h.buckets();
        assert_eq!(b[0], ("1.0".to_string(), 0));
        assert_eq!(b[1], ("+Inf".to_string(), 1));
    }

    #[test]
    fn py_float_str_keeps_trailing_zero() {
        assert_eq!(py_float_str(1.0), "1.0");
        assert_eq!(py_float_str(0.05), "0.05");
        assert_eq!(py_float_str(0.1), "0.1");
        assert_eq!(py_float_str(120.0), "120.0");
    }

    #[test]
    fn observe_request_updates_histogram_and_model_count() {
        let mut s = EndpointState::new();
        s.observe_request("qwen", 0.2);
        s.observe_request("qwen", 0.3);
        s.observe_request("llama", 5.0);
        assert_eq!(s.request_duration.count(), 3);
        assert_eq!(s.requests_by_model["qwen"], 2);
        assert_eq!(s.requests_by_model["llama"], 1);
    }

    #[test]
    fn status_view_reports_state_and_uptime() {
        let s = EndpointState::new();
        let v = s.status_view("http://upstream");
        assert_eq!(v["state"], "COLD");
        assert_eq!(v["endpoint"], "http://upstream");
        assert!(v["uptime_s"].as_f64().unwrap() >= 0.0);
    }

    #[test]
    fn metrics_view_cold_hit_is_total_minus_warm() {
        let mut s = EndpointState::new();
        s.requests_total = 10;
        s.requests_warm_hit = 4;
        let v = s.metrics_view();
        assert_eq!(v["requests_cold_hit"], 6);
        assert_eq!(v["requests_total"], 10);
    }
}
