//! Pinned pod lifecycle incl. `RUNPOD_ON_MIGRATE=replace` (spec section 6.7).
//! WI-08.
//!
//! Port of `proxy/lifecycle.py::PodLifecycle`. Starts/stops a pinned
//! persistent pod via the `RunPod` REST API. A raw `reqwest::Client` backs
//! the status probe and the start/stop POSTs (with their own migration
//! detection); the `RunpodApi` client backs get/delete/create. The pinned pod
//! id is mutable: the `replace` policy swaps it for a fresh pod.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::Value;

use super::{Lifecycle, LifecycleError};
use crate::config::Config;
use crate::runpod_api::{CreatePodSpec, EXITED, Pod, RUNNING, RunpodApi, RunpodApiError};
use crate::state::EndpointState;
use crate::target::UpstreamTarget;

/// A start/stop POST failed. `Migration` is the "please migrate" prompt, which
/// the `replace` policy turns into a terminate-and-recreate.
#[derive(Debug)]
enum PostError {
    /// `RunPod` signalled a pod-migration requirement.
    Migration(String),
    /// A transport error or a non-2xx response (not a migration prompt).
    General(String),
}

impl std::fmt::Display for PostError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PostError::Migration(m) | PostError::General(m) => write!(f, "{m}"),
        }
    }
}

/// Starts/stops a pinned persistent pod via the `RunPod` REST API.
pub struct PodLifecycle {
    config: Arc<Config>,
    api: RunpodApi,
    client: reqwest::Client,
    target: Arc<Mutex<UpstreamTarget>>,
    state: Arc<Mutex<EndpointState>>,
    /// Mutable: the `replace` policy swaps the pinned pod for a fresh one.
    pod_id: Mutex<String>,
}

impl PodLifecycle {
    /// Build a pinned lifecycle over a shared client and endpoint state.
    pub fn new(
        config: Arc<Config>,
        client: reqwest::Client,
        target: Arc<Mutex<UpstreamTarget>>,
        state: Arc<Mutex<EndpointState>>,
    ) -> Self {
        let pod_id = config.pod_id.clone();
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            client.clone(),
        );
        Self {
            config,
            api,
            client,
            target,
            state,
            pod_id: Mutex::new(pod_id),
        }
    }

    /// The current pinned pod id (may have been swapped by a replace).
    pub fn pod_id(&self) -> String {
        self.pod_id.lock().expect("pod_id lock").clone()
    }

    /// `GET /pods/{id}` URL for the current pod.
    fn pod_url(&self) -> String {
        let base = self.config.rest_api_url.trim_end_matches('/');
        let pod_id = self.pod_id.lock().expect("pod_id lock").clone();
        format!("{base}/pods/{pod_id}")
    }

    /// Current pod status, or `None` when it cannot be determined. A 404 means
    /// the pod is gone (deleted on the `RunPod` side) and is surfaced as
    /// `PodNotFound` so a warmup fails fast instead of burning the budget.
    async fn desired_status(&self) -> Result<Option<String>, LifecycleError> {
        let url = self.pod_url();
        let api_key = self.config.api_key.clone();
        let Ok(response) = self.client.get(&url).bearer_auth(&api_key).send().await else {
            return Ok(None);
        };
        let status = response.status().as_u16();
        if status == 404 {
            let pod_id = self.pod_id.lock().expect("pod_id lock").clone();
            return Err(LifecycleError::PodNotFound(format!(
                "pinned pod {pod_id} no longer exists on RunPod \
                 (GET /pods/{pod_id} -> 404) — point RUNPOD_POD_ID at a live \
                 pod, or unset it to enable pod discovery"
            )));
        }
        if !(200..300).contains(&status) {
            return Ok(None);
        }
        let body: Value = match response.json().await {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let Some(obj) = body.as_object() else {
            return Ok(None);
        };
        let raw = obj
            .get("desiredStatus")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .or_else(|| obj.get("status").and_then(Value::as_str));
        Ok(raw.map(str::to_uppercase))
    }

    /// `POST /pods/{id}/{action}` (start/stop). A migration prompt is surfaced
    /// as `PostError::Migration`; anything else as `General`.
    async fn post(&self, action: &str) -> Result<(), PostError> {
        let url = format!("{}/{}", self.pod_url(), action);
        let api_key = self.config.api_key.clone();
        let response = match self.client.post(&url).bearer_auth(&api_key).send().await {
            Ok(r) => r,
            Err(e) => return Err(PostError::General(format!("pod {action} failed: {e}"))),
        };
        let status = response.status().as_u16();
        if !(200..300).contains(&status) {
            let body = response.json::<Value>().await.ok();
            if let Some(message) = RunpodApi::migration_message(body.as_ref()) {
                return Err(PostError::Migration(format!(
                    "pod {action} returned HTTP {status}: {message}"
                )));
            }
            let detail = error_detail(body.as_ref());
            return Err(PostError::General(format!(
                "pod {action} returned HTTP {status}{detail}"
            )));
        }
        // The prompt is beta/undocumented and may arrive on a 2xx body too.
        let body = response.json::<Value>().await.ok();
        if let Some(message) = RunpodApi::migration_message(body.as_ref()) {
            return Err(PostError::Migration(format!(
                "pod {action} returned HTTP {status}: {message}"
            )));
        }
        Ok(())
    }

    /// Terminate a pod blocked by the migration prompt and create a fresh one
    /// with the same spec, adopting the new id and URL.
    async fn replace_pod(&self, budget: f64) -> Result<(), LifecycleError> {
        let old_id = self.pod_id.lock().expect("pod_id lock").clone();
        tracing::warn!(
            %old_id,
            "pod requires migration; policy=replace: terminating and creating a fresh pod"
        );
        let old = match self.api.get_pod(&old_id).await {
            Ok(pod) => pod,
            Err(e) => {
                tracing::warn!(%old_id, %e, "could not fetch pod before replace");
                None
            }
        };
        let Some(old) = old else {
            return Err(LifecycleError::general(format!(
                "pod {old_id} requires migration but its spec could not be \
                 fetched, so a replacement cannot be created"
            )));
        };
        if let Err(e) = self.api.delete_pod(&old.id).await {
            return Err(LifecycleError::general(format!(
                "pod {} requires migration but could not be terminated: {e}",
                old.id
            )));
        }
        tracing::info!(%old_id, "terminated pod");
        let fresh = self.create_pod_with_backoff(&old, budget).await?;
        // Adopt the new id/url immediately, before waiting for RUNNING: if the
        // budget runs out mid-boot, the next warmup must target the
        // replacement, not the deleted original.
        let fresh_id = fresh.id.clone();
        self.pod_id
            .lock()
            .expect("pod_id lock")
            .clone_from(&fresh_id);
        self.state.lock().expect("state lock").pods_replaced += 1;
        let port = fresh.http_ports();
        let port_str = port
            .first()
            .map_or_else(|| self.config.pod_port.to_string(), ToString::to_string);
        let url = format!("https://{fresh_id}-{port_str}.proxy.runpod.net");
        self.target
            .lock()
            .expect("target lock")
            .set(&url, &fresh_id);
        tracing::warn!(old = %old.id, new = %fresh_id, "pod replaced");
        if !self.wait_running(&fresh_id, budget).await {
            return Err(LifecycleError::general(format!(
                "replacement pod {fresh_id} did not reach RUNNING within the warmup budget"
            )));
        }
        Ok(())
    }

    /// Create the replacement pod, retrying a v2 capacity 400 with backoff
    /// inside the warmup budget.
    async fn create_pod_with_backoff(&self, old: &Pod, budget: f64) -> Result<Pod, LifecycleError> {
        let spec = CreatePodSpec {
            name: if old.name.is_empty() {
                if self.config.model_name.is_empty() {
                    "runpod-proxy-pod".to_string()
                } else {
                    self.config.model_name.clone()
                }
            } else {
                old.name.clone()
            },
            template_id: old.template_id.clone(),
            image_name: if old.template_id.is_some() {
                None
            } else {
                Some(old.image.clone())
            },
            gpu_type: if old.gpu_type.is_empty() {
                self.config
                    .gpu_type_ids
                    .first()
                    .cloned()
                    .unwrap_or_default()
            } else {
                old.gpu_type.clone()
            },
            gpu_count: old.gpu_count,
            cloud_type: self.config.cloud_type.clone(),
            ports: old.ports.clone(),
            env: if old.env.is_empty() {
                None
            } else {
                Some(old.env.clone())
            },
            container_disk_gb: old.container_disk_gb,
            volume_gb: None,
            volume_id: if old.volume_id.is_empty() {
                None
            } else {
                Some(old.volume_id.clone())
            },
            volume_mount_path: "/workspace".to_string(),
            datacenter_ids: Vec::new(),
        };
        let deadline = Instant::now() + Duration::from_secs_f64(budget);
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.api.create_pod(&spec).await {
                Ok(pod) => return Ok(pod),
                Err(RunpodApiError::Capacity { message, .. }) => {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(LifecycleError::general(format!(
                            "pod {} was replaced but the fresh pod could not be \
                             created within the budget (RunPod capacity): {message}",
                            old.id
                        )));
                    }
                    let delay = Duration::from_secs_f64(30.0)
                        .min(Duration::from_secs_f64(2.0 * f64::from(attempt)));
                    tracing::warn!(attempt, "replacement create hit RunPod capacity; retrying");
                    tokio::time::sleep(delay.min(remaining)).await;
                }
                Err(e) => {
                    return Err(LifecycleError::general(format!(
                        "pod {} was replaced but the fresh pod could not be \
                         created: {e}",
                        old.id
                    )));
                }
            }
        }
    }

    /// Poll `GET /pods/{id}` until the pod is RUNNING or the budget is spent.
    async fn wait_running(&self, pod_id: &str, budget: f64) -> bool {
        let deadline = Instant::now() + Duration::from_secs_f64(budget);
        loop {
            match self.api.get_pod(pod_id).await {
                Ok(Some(pod)) => {
                    if pod.desired_status.to_uppercase() == RUNNING {
                        return true;
                    }
                }
                Ok(None) => return false,
                Err(e) => {
                    tracing::warn!(%pod_id, %e, "replacement pod status probe failed");
                }
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return false;
            }
            tokio::time::sleep(Duration::from_secs_f64(1.0).min(remaining)).await;
        }
    }
}

/// The `RunPod` error text from a response body, for logs and errors.
/// Returns `": {value}"` (truncated to 300 chars) or `""`.
fn error_detail(body: Option<&Value>) -> String {
    let Some(obj) = body.and_then(Value::as_object) else {
        return String::new();
    };
    for key in ["error", "message", "detail", "reason", "statusMessage"] {
        if let Some(value) = obj.get(key).and_then(Value::as_str) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                let truncated: String = trimmed.chars().take(300).collect();
                return format!(": {truncated}");
            }
        }
    }
    String::new()
}

#[async_trait]
impl Lifecycle for PodLifecycle {
    async fn start(&self, budget: f64) -> Result<(), LifecycleError> {
        if self.desired_status().await? == Some(RUNNING.to_string()) {
            tracing::info!("pod already running; skipping start");
            return Ok(());
        }
        match self.post("start").await {
            Ok(()) => Ok(()),
            Err(PostError::Migration(msg)) => {
                if self.config.on_migrate != "replace" {
                    let pod_id = self.pod_id.lock().expect("pod_id lock").clone();
                    return Err(LifecycleError::general(format!(
                        "pod {pod_id} cannot start: RunPod requires migration \
                         ({msg}); set RUNPOD_ON_MIGRATE=replace to terminate it \
                         and create a fresh pod instead"
                    )));
                }
                self.replace_pod(budget).await
            }
            Err(PostError::General(msg)) => Err(LifecycleError::general(msg)),
        }
    }

    async fn stop(&self) -> Result<(), LifecycleError> {
        let status = match self.desired_status().await {
            Ok(s) => s,
            Err(LifecycleError::PodNotFound(_)) => {
                tracing::info!(pod = %self.pod_id(), "pod no longer exists; nothing to stop");
                return Ok(());
            }
            Err(e) => return Err(e),
        };
        if status == Some(EXITED.to_string()) {
            tracing::info!("pod already stopped; skipping stop");
            return Ok(());
        }
        self.post("stop")
            .await
            .map_err(|e| LifecycleError::general(e.to_string()))
    }

    fn active_model(&self) -> String {
        self.config.model_name.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    use httpmock::prelude::*;
    use serde_json::json;

    /// Build a pinned lifecycle pointed at the mock server, plus the shared
    /// target/state so tests can assert on the adopted pod.
    fn lifecycle(
        server: &MockServer,
        extra: &[(&str, &str)],
    ) -> (
        PodLifecycle,
        Arc<Mutex<UpstreamTarget>>,
        Arc<Mutex<EndpointState>>,
    ) {
        let mut env = BTreeMap::new();
        env.insert("RUNPOD_MODE".into(), "pod".into());
        env.insert("RUNPOD_API_KEY".into(), "test-key".into());
        env.insert("RUNPOD_POD_ID".into(), "pod-1".into());
        env.insert("RUNPOD_REST_API_URL".into(), server.url("/v1"));
        env.insert("RUNPOD_AVAILABILITY_API_URL".into(), server.url("/v2"));
        for (k, v) in extra {
            env.insert((*k).to_string(), (*v).to_string());
        }
        let cfg = Arc::new(Config::from_env_map(&env).unwrap());
        let target = Arc::new(Mutex::new(UpstreamTarget::new("http://initial", "pod-1")));
        let state = Arc::new(Mutex::new(EndpointState::new()));
        let lc = PodLifecycle::new(
            cfg,
            crate::test_client(),
            Arc::clone(&target),
            Arc::clone(&state),
        );
        (lc, target, state)
    }

    #[tokio::test]
    async fn start_is_noop_when_already_running() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200)
                .json_body(json!({"id": "pod-1", "desiredStatus": "RUNNING"}));
        });
        let start_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/start");
            then.status(200);
        });
        let (lc, _t, _s) = lifecycle(&server, &[]);
        assert!(lc.start(60.0).await.is_ok());
        assert_eq!(start_mock.hits(), 0);
    }

    #[tokio::test]
    async fn start_posts_start_when_not_running() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200)
                .json_body(json!({"id": "pod-1", "desiredStatus": "EXITED"}));
        });
        let start_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/start");
            then.status(200);
        });
        let (lc, _t, _s) = lifecycle(&server, &[]);
        assert!(lc.start(60.0).await.is_ok());
        assert_eq!(start_mock.hits(), 1);
    }

    #[tokio::test]
    async fn start_404_is_pod_not_found() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(404);
        });
        let (lc, _t, _s) = lifecycle(&server, &[]);
        let err = lc.start(60.0).await.unwrap_err();
        assert!(matches!(err, LifecycleError::PodNotFound(_)), "{err:?}");
    }

    #[tokio::test]
    async fn start_migration_fails_when_on_migrate_fail() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200)
                .json_body(json!({"id": "pod-1", "desiredStatus": "EXITED"}));
        });
        server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/start");
            then.status(500)
                .json_body(json!({"message": "please migrate your pod"}));
        });
        let (lc, _t, _s) = lifecycle(&server, &[("RUNPOD_ON_MIGRATE", "fail")]);
        let err = lc.start(60.0).await.unwrap_err();
        match err {
            LifecycleError::General(msg) => {
                assert!(msg.contains("requires migration"), "{msg}");
                assert!(msg.contains("RUNPOD_ON_MIGRATE=replace"), "{msg}");
            }
            other @ LifecycleError::PodNotFound(_) => panic!("expected General, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn stop_is_noop_when_exited() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200)
                .json_body(json!({"id": "pod-1", "desiredStatus": "EXITED"}));
        });
        let stop_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/stop");
            then.status(200);
        });
        let (lc, _t, _s) = lifecycle(&server, &[]);
        assert!(lc.stop().await.is_ok());
        assert_eq!(stop_mock.hits(), 0);
    }

    #[tokio::test]
    async fn stop_404_is_ok() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(404);
        });
        let (lc, _t, _s) = lifecycle(&server, &[]);
        assert!(lc.stop().await.is_ok());
    }

    #[tokio::test]
    async fn stop_posts_stop_when_running() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200)
                .json_body(json!({"id": "pod-1", "desiredStatus": "RUNNING"}));
        });
        let stop_mock = server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/stop");
            then.status(200);
        });
        let (lc, _t, _s) = lifecycle(&server, &[]);
        assert!(lc.stop().await.is_ok());
        assert_eq!(stop_mock.hits(), 1);
    }

    #[test]
    fn active_model_returns_model_name() {
        let server = MockServer::start();
        let (lc, _t, _s) = lifecycle(&server, &[("RUNPOD_MODEL_NAME", "qwen")]);
        assert_eq!(lc.active_model(), "qwen");
    }

    #[tokio::test]
    async fn replace_pod_adopts_new_id_and_url() {
        let server = MockServer::start();
        // The status probe and get_pod both hit GET /v1/pods/pod-1.
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200).json_body(json!({
                "id": "pod-1",
                "name": "my-pod",
                "desiredStatus": "EXITED",
                "image": "my-image",
                "templateId": "tmpl-1",
                "gpuType": "NVIDIA A40",
                "gpuCount": 1,
                "ports": ["8000/http"],
                "env": {"MODEL": "qwen"},
                "containerDiskInGb": 50,
                "datacenter": "uswest"
            }));
        });
        // Start is blocked by the migration prompt.
        server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/start");
            then.status(500)
                .json_body(json!({"message": "please migrate your pod"}));
        });
        // Terminate the old pod.
        server.mock(|when, then| {
            when.method(DELETE).path("/v1/pods/pod-1");
            then.status(200);
        });
        // Create the fresh pod (v2).
        server.mock(|when, then| {
            when.method(POST).path("/v2/pods");
            then.status(200).json_body(json!({
                "id": "pod-2",
                "status": "STARTING",
                "ports": ["8000/http"],
                "gpu": {"id": "NVIDIA A40", "count": 1}
            }));
        });
        // The replacement reaches RUNNING.
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-2");
            then.status(200)
                .json_body(json!({"id": "pod-2", "desiredStatus": "RUNNING"}));
        });
        let (lc, target, state) = lifecycle(
            &server,
            &[
                ("RUNPOD_ON_MIGRATE", "replace"),
                ("RUNPOD_MODEL_NAME", "qwen"),
                ("RUNPOD_GPU_TYPE_IDS", "NVIDIA A40"),
                ("RUNPOD_CLOUD_TYPE", "SECURE"),
                ("RUNPOD_POD_PORT", "8000"),
            ],
        );
        assert!(lc.start(60.0).await.is_ok());
        assert_eq!(lc.pod_id(), "pod-2");
        let t = target.lock().unwrap();
        assert_eq!(t.pod_id(), "pod-2");
        assert_eq!(t.url(), "https://pod-2-8000.proxy.runpod.net");
        assert_eq!(state.lock().unwrap().pods_replaced, 1);
    }
}
