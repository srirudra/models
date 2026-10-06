//! GPU availability poller (spec section 10). WI-11.
//!
//! Polls the `RunPod` v2 catalog for the GPU types referenced by the model
//! catalogue and keeps the last-known values. The point is *staleness
//! visibility*: on any fetch failure the previous values are kept (not wiped)
//! and the age of the data plus the last error are surfaced in `/_status` and
//! `/metrics` (parity with `proxy/gpu_availability.py`).
//!
//! Deliberately non-fatal: a failed or crashing poll must never take the proxy
//! down, so the tick swallows every error and records it as `last_error`.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};
use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::health::ProbeClient;
use crate::state::now_secs;

/// Numeric encoding of `RunPod` availability spellings for Prometheus gauges;
/// unknown spellings map to `-1` (parity with `AVAILABILITY_LEVELS`).
#[must_use]
pub fn availability_level(name: &str) -> i64 {
    match name {
        "HIGH" => 3,
        "MEDIUM" => 2,
        "LOW" => 1,
        "NONE" => 0,
        _ => -1,
    }
}

/// One GPU type's last-known availability + per-datacenter breakdown.
#[derive(Clone, Debug)]
struct GpuEntry {
    availability: String,
    datacenters: Vec<(String, String)>,
}

/// Last-known poll results (kept across failures so the age can be surfaced).
#[derive(Default)]
struct PollState {
    last_success_at: Option<f64>,
    last_error: Option<String>,
    /// Ordered by GPU id (parity with Python's `sorted(gpu_info.items())`).
    gpu_info: BTreeMap<String, GpuEntry>,
}

/// The `/metrics` inputs: the data age and the sorted `(gpu_id, availability)`
/// pairs.
pub struct GpuMetricsView {
    /// Seconds since the last successful refresh (`None` until the first).
    pub age_s: Option<f64>,
    /// `(gpu_id, availability)` pairs sorted by id.
    pub gpus: Vec<(String, String)>,
}

/// Background GPU availability poller (spec section 10).
pub struct GpuAvailability {
    config: Arc<Config>,
    client: Arc<dyn ProbeClient>,
    inner: Arc<Mutex<PollState>>,
    stop_tx: watch::Sender<bool>,
    task: tokio::sync::Mutex<Option<JoinHandle<()>>>,
}

impl GpuAvailability {
    /// Build the poller over the shared config + HTTP client.
    pub fn new(config: Arc<Config>, client: Arc<dyn ProbeClient>) -> Self {
        let (stop_tx, _) = watch::channel(false);
        Self {
            config,
            client,
            inner: Arc::new(Mutex::new(PollState::default())),
            stop_tx,
            task: tokio::sync::Mutex::new(None),
        }
    }

    /// Whether the poller should run: a positive interval, pod mode, an API
    /// key, and at least one tracked GPU id (parity with `enabled`).
    #[must_use]
    pub fn enabled(&self) -> bool {
        self.config.gpu_availability_interval_s > 0.0
            && self.config.mode == "pod"
            && !self.config.api_key.is_empty()
            && !self.tracked_gpu_ids().is_empty()
    }

    /// GPU type ids to watch: the catalogue's (dedup, order-preserving), else
    /// the config-level ids (parity with `tracked_gpu_ids`).
    #[must_use]
    pub fn tracked_gpu_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self
            .config
            .catalogue()
            .models
            .iter()
            .flat_map(|spec| spec.gpus.iter().map(|g| g.id.clone()))
            .collect();
        if ids.is_empty() {
            ids.clone_from(&self.config.gpu_type_ids);
        }
        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for id in ids {
            if !id.is_empty() && seen.insert(id.clone()) {
                out.push(id);
            }
        }
        out
    }

    /// Start the poller (idempotent; a no-op when disabled). Fetches
    /// immediately, then every interval.
    pub async fn start(self: &Arc<Self>) {
        if !self.enabled() {
            return;
        }
        let mut task = self.task.lock().await;
        if task.is_some() {
            return;
        }
        let _ = self.stop_tx.send(false);
        let stop_rx = self.stop_tx.subscribe();
        let this = Arc::clone(self);
        let handle = tokio::spawn(async move { this.run(stop_rx).await });
        *task = Some(handle);
    }

    /// Stop the poller (idempotent).
    pub async fn stop(&self) {
        let _ = self.stop_tx.send(true);
        let mut task = self.task.lock().await;
        if let Some(handle) = task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// The interval loop: tick immediately, then every interval until stopped.
    async fn run(self: Arc<Self>, mut stop_rx: watch::Receiver<bool>) {
        let interval = Duration::from_secs_f64(self.config.gpu_availability_interval_s);
        loop {
            self.tick().await;
            tokio::select! {
                _ = stop_rx.changed() => {
                    if *stop_rx.borrow() {
                        return;
                    }
                }
                () = tokio::time::sleep(interval) => {}
            }
        }
    }

    /// Perform one poll, updating the last-known values or the last error.
    pub async fn tick(&self) {
        let tracked: std::collections::HashSet<String> =
            self.tracked_gpu_ids().into_iter().collect();
        let mut url = format!(
            "{}/catalog/gpus?include=AVAILABILITY&product=POD",
            self.config.availability_api_url
        );
        if !self.config.cloud_type.is_empty() {
            url.push_str("&cloud=");
            url.push_str(&self.config.cloud_type.to_uppercase());
        }
        let headers = vec![(
            "Authorization".to_string(),
            format!("Bearer {}", self.config.api_key),
        )];

        let result =
            tokio::time::timeout(Duration::from_secs(30), self.client.get(&url, &headers)).await;

        match Self::parse_result(result, &tracked) {
            Ok(gpu_info) => {
                let mut inner = self.inner.lock().unwrap();
                inner.gpu_info = gpu_info;
                inner.last_success_at = Some(now_secs());
                inner.last_error = None;
            }
            Err(detail) => {
                let mut inner = self.inner.lock().unwrap();
                inner.last_error = Some(detail);
            }
        }
    }

    /// Turn a probe outcome into the parsed GPU map, or an error detail string.
    fn parse_result(
        result: Result<
            Result<crate::health::ProbeResponse, crate::health::ProbeError>,
            tokio::time::error::Elapsed,
        >,
        tracked: &std::collections::HashSet<String>,
    ) -> Result<BTreeMap<String, GpuEntry>, String> {
        let response = match result {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => return Err(format!("ProbeError: {}", first_line(&e.0))),
            Err(_) => return Err("TimeoutError: gpu availability request timed out".to_string()),
        };
        if !(200..300).contains(&response.status) {
            return Err(format!("HttpStatusError: HTTP {}", response.status));
        }
        let data: Value = serde_json::from_str(&response.body)
            .map_err(|e| format!("JsonError: {}", first_line(&e.to_string())))?;
        let mut info = BTreeMap::new();
        if let Some(gpus) = data.get("gpus").and_then(Value::as_array) {
            for gpu in gpus {
                let Some(gpu_id) = gpu.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if !tracked.contains(gpu_id) {
                    continue;
                }
                let availability = gpu
                    .get("availability")
                    .and_then(Value::as_str)
                    .unwrap_or("UNKNOWN")
                    .to_string();
                let mut datacenters = Vec::new();
                if let Some(dcs) = gpu.get("dataCenters").and_then(Value::as_array) {
                    for dc in dcs {
                        if let Some(dc_id) = dc.get("id").and_then(Value::as_str) {
                            let dc_avail = dc
                                .get("availability")
                                .and_then(Value::as_str)
                                .unwrap_or("UNKNOWN")
                                .to_string();
                            datacenters.push((dc_id.to_string(), dc_avail));
                        }
                    }
                }
                info.insert(
                    gpu_id.to_string(),
                    GpuEntry {
                        availability,
                        datacenters,
                    },
                );
            }
        }
        Ok(info)
    }

    /// The `gpu_availability` block for `/_status` (parity with `status_view`).
    #[must_use]
    pub fn status_view(&self) -> Value {
        let inner = self.inner.lock().unwrap();
        let mut gpus = serde_json::Map::new();
        for (id, entry) in &inner.gpu_info {
            let dcs: Vec<Value> = entry
                .datacenters
                .iter()
                .map(|(dc_id, avail)| json!({ "id": dc_id, "availability": avail }))
                .collect();
            gpus.insert(
                id.clone(),
                json!({ "availability": entry.availability, "datacenters": dcs }),
            );
        }
        let mut models = serde_json::Map::new();
        let catalogue = self.config.catalogue();
        for spec in &catalogue.models {
            let mut per_gpu = serde_json::Map::new();
            for g in &spec.gpus {
                let avail = inner
                    .gpu_info
                    .get(&g.id)
                    .map_or(Value::Null, |e| Value::String(e.availability.clone()));
                per_gpu.insert(g.id.clone(), avail);
            }
            models.insert(spec.name.clone(), Value::Object(per_gpu));
        }
        json!({
            "updated_at": inner.last_success_at,
            "age_s": inner.last_success_at.map(|t| round1(now_secs() - t)),
            "last_error": inner.last_error,
            "gpus": Value::Object(gpus),
            "models": Value::Object(models),
        })
    }

    /// The `/metrics` inputs (parity with `metrics_view`).
    #[must_use]
    pub fn metrics_view(&self) -> GpuMetricsView {
        let inner = self.inner.lock().unwrap();
        let gpus = inner
            .gpu_info
            .iter()
            .map(|(id, entry)| (id.clone(), entry.availability.clone()))
            .collect();
        GpuMetricsView {
            age_s: inner.last_success_at.map(|t| round1(now_secs() - t)),
            gpus,
        }
    }
}

/// Round to 1 decimal (parity with Python `round(x, 1)` for the data age).
fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}

/// The first line of a message (parity with `str(exc).splitlines()[0]`).
fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap as Map;

    use crate::health::{ProbeError, ProbeResponse};
    use async_trait::async_trait;

    /// A probe client returning a scripted GET result.
    struct ScriptedClient {
        result: std::sync::Mutex<Result<ProbeResponse, ProbeError>>,
    }

    impl ScriptedClient {
        fn ok(body: &Value) -> Self {
            Self {
                result: std::sync::Mutex::new(Ok(ProbeResponse::json(200, body))),
            }
        }
        fn status(code: u16) -> Self {
            Self {
                result: std::sync::Mutex::new(Ok(ProbeResponse {
                    status: code,
                    body: String::new(),
                })),
            }
        }
        fn error(msg: &str) -> Self {
            Self {
                result: std::sync::Mutex::new(Err(ProbeError(msg.to_string()))),
            }
        }
    }

    #[async_trait]
    impl ProbeClient for ScriptedClient {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            self.result.lock().unwrap().clone()
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            self.result.lock().unwrap().clone()
        }
    }

    fn config(pairs: &[(&str, &str)]) -> Arc<Config> {
        let mut env: Map<String, String> = Map::new();
        for (k, v) in pairs {
            env.insert(k.to_string(), v.to_string());
        }
        Arc::new(Config::from_env_map(&env).unwrap())
    }

    fn pod_config(extra: &[(&str, &str)]) -> Arc<Config> {
        let mut pairs: Vec<(&str, &str)> = vec![
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_MODE", "pod"),
            ("RUNPOD_API_KEY", "k"),
            ("RUNPOD_POD_ID", "pod-1"),
            ("RUNPOD_GPU_TYPE_IDS", "NVIDIA A100 80GB PCIe"),
            ("GPU_AVAILABILITY_INTERVAL_S", "30"),
        ];
        pairs.extend_from_slice(extra);
        config(&pairs)
    }

    #[test]
    fn availability_level_maps_known_and_unknown() {
        assert_eq!(availability_level("HIGH"), 3);
        assert_eq!(availability_level("MEDIUM"), 2);
        assert_eq!(availability_level("LOW"), 1);
        assert_eq!(availability_level("NONE"), 0);
        assert_eq!(availability_level("WEIRD"), -1);
    }

    #[test]
    fn enabled_requires_pod_key_and_ids() {
        let poller =
            GpuAvailability::new(pod_config(&[]), Arc::new(ScriptedClient::ok(&json!({}))));
        assert!(poller.enabled());
        assert_eq!(poller.tracked_gpu_ids(), vec!["NVIDIA A100 80GB PCIe"]);

        // Serverless mode disables it.
        let sl = GpuAvailability::new(
            config(&[("RUNPOD_MODEL_NAME", "qwen"), ("POD_HEALTH_MODE", "any")]),
            Arc::new(ScriptedClient::ok(&json!({}))),
        );
        assert!(!sl.enabled());
    }

    #[tokio::test]
    async fn tick_stores_tracked_gpu_availability() {
        let body = json!({
            "gpus": [
                {"id": "NVIDIA A100 80GB PCIe", "availability": "HIGH",
                 "dataCenters": [{"id": "US-CA-1", "availability": "MEDIUM"}]},
                {"id": "OTHER", "availability": "LOW"}
            ]
        });
        let poller = GpuAvailability::new(pod_config(&[]), Arc::new(ScriptedClient::ok(&body)));
        poller.tick().await;
        let mv = poller.metrics_view();
        assert_eq!(
            mv.gpus,
            vec![("NVIDIA A100 80GB PCIe".to_string(), "HIGH".to_string())]
        );
        assert!(mv.age_s.is_some());
        let sv = poller.status_view();
        assert_eq!(sv["last_error"], Value::Null);
        assert_eq!(
            sv["gpus"]["NVIDIA A100 80GB PCIe"]["availability"],
            json!("HIGH")
        );
        // With only RUNPOD_MODEL_NAME set (no catalogue JSON), the catalogue
        // has no models, so the models block is empty (Python parity:
        // `{spec.name: ... for spec in catalogue.models}` over an empty list).
        assert_eq!(sv["models"], json!({}));
    }

    #[tokio::test]
    async fn tick_failure_keeps_last_known_values() {
        let body = json!({ "gpus": [
            {"id": "NVIDIA A100 80GB PCIe", "availability": "HIGH"}
        ]});
        let poller = Arc::new(GpuAvailability::new(
            pod_config(&[]),
            Arc::new(ScriptedClient::ok(&body)),
        ));
        poller.tick().await;

        // Swap in a failing client and tick again: values persist, error set.
        let failing = GpuAvailability {
            config: Arc::clone(&poller.config),
            client: Arc::new(ScriptedClient::status(500)),
            inner: Arc::clone(&poller.inner),
            stop_tx: watch::channel(false).0,
            task: tokio::sync::Mutex::new(None),
        };
        failing.tick().await;
        let mv = failing.metrics_view();
        assert_eq!(
            mv.gpus,
            vec![("NVIDIA A100 80GB PCIe".to_string(), "HIGH".to_string())]
        );
        let sv = failing.status_view();
        assert_eq!(sv["last_error"], json!("HttpStatusError: HTTP 500"));
    }

    #[tokio::test]
    async fn tick_transport_error_records_detail() {
        let poller = GpuAvailability::new(
            pod_config(&[]),
            Arc::new(ScriptedClient::error("connect: refused")),
        );
        poller.tick().await;
        let sv = poller.status_view();
        assert_eq!(sv["last_error"], json!("ProbeError: connect: refused"));
        assert_eq!(sv["age_s"], Value::Null);
    }
}
