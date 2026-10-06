//! Single-flight warmup: exactly one warmup chain per model, result settled
//! exactly once on every path (spec section 6.1). WI-06.
//!
//! Concurrent callers of [`WarmupManager::ensure_warm`] share a single warmup
//! attempt chain: the first caller starts it, the rest join the in-flight
//! future. The future is settled exactly once on every path (success, timeout,
//! pod-not-found, or an unexpected error), so no waiter can hang forever.

use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::watch;

use crate::config::Config;
use crate::health::{ProbeClient, classify};
use crate::lifecycle::{Lifecycle, LifecycleError};
use crate::state::{EndpointState, State, now_secs};
use crate::target::UpstreamTarget;

/// A warmup could not complete; the waiting clients get an HTTP 503.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct WarmupError(pub String);

/// Endpoint was not warm within `WARMUP_TIMEOUT_S`.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct WarmupTimeout(pub String);

impl From<WarmupTimeout> for WarmupError {
    fn from(e: WarmupTimeout) -> Self {
        WarmupError(e.0)
    }
}

type WarmupResult = Result<(), WarmupError>;

/// The outcome of a single warmup attempt (one probe round).
enum Attempt {
    /// The readiness probe classified the endpoint ready.
    Ready,
    /// The pinned pod no longer exists; fail fast (no backoff).
    PodNotFound(LifecycleError),
    /// Not ready yet (or a transient error); back off and retry.
    Retry,
}

/// Single-flight warmup manager (spec section 6.1).
#[derive(Clone)]
pub struct WarmupManager {
    config: Arc<Config>,
    client: Arc<dyn ProbeClient>,
    state: Arc<std::sync::Mutex<EndpointState>>,
    lifecycle: Arc<dyn Lifecycle>,
    target: Arc<std::sync::Mutex<UpstreamTarget>>,
    /// The in-flight warmup, if any. `None` value = pending, `Some` = settled.
    lock: Arc<tokio::sync::Mutex<Option<watch::Receiver<Option<WarmupResult>>>>>,
}

impl WarmupManager {
    /// Build a warmup manager over the shared state/target/lifecycle.
    pub fn new(
        config: Arc<Config>,
        client: Arc<dyn ProbeClient>,
        state: Arc<std::sync::Mutex<EndpointState>>,
        lifecycle: Arc<dyn Lifecycle>,
        target: Arc<std::sync::Mutex<UpstreamTarget>>,
    ) -> Self {
        Self {
            config,
            client,
            state,
            lifecycle,
            target,
            lock: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Wait until the endpoint is WARM, triggering a shared warmup if needed.
    ///
    /// Returns `Ok(())` when the endpoint is warm, or a [`WarmupError`] (e.g.
    /// [`WarmupTimeout`]) when the warmup could not complete.
    pub async fn ensure_warm(&self, force: bool) -> WarmupResult {
        // Fast path: already warm and not forced.
        {
            let s = self.state.lock().unwrap();
            if s.state == State::Warm && !force {
                return Ok(());
            }
        }
        let rx = {
            let mut guard = self.lock.lock().await;
            // Double-check under the lock (someone may have warmed it).
            {
                let s = self.state.lock().unwrap();
                if s.state == State::Warm && !force {
                    return Ok(());
                }
            }
            // Join an in-flight warmup if one is still pending; otherwise start
            // a new one.
            let needs_new = match &*guard {
                None => true,
                Some(rx) => rx.borrow().is_some(), // Some = already settled
            };
            if needs_new {
                let (tx, rx) = watch::channel(None);
                *guard = Some(rx);
                let this = self.clone();
                tokio::spawn(async move {
                    this.warm_up(tx).await;
                });
            }
            (*guard).clone().expect("in-flight receiver present")
        };
        // Wait for the warmup to settle (exactly once).
        let mut rx = rx;
        while rx.borrow_and_update().is_none() {
            if rx.changed().await.is_err() {
                // Sender dropped without settling (defensive; a bug in the
                // warmup task). Return state to COLD and fail the waiters.
                self.set_state(State::Cold);
                return Err(WarmupTimeout("warmup ended without result".into()).into());
            }
        }
        (*rx.borrow()).clone().expect("settled")
    }

    fn set_state(&self, state: State) {
        let mut s = self.state.lock().unwrap();
        s.state = state;
    }

    fn target_url(&self) -> String {
        self.target.lock().unwrap().url().to_string()
    }

    fn target_pod_id(&self) -> String {
        self.target.lock().unwrap().pod_id().to_string()
    }

    /// Drive the warmup attempt and settle the result exactly once.
    async fn warm_up(&self, tx: watch::Sender<Option<WarmupResult>>) {
        let result = self.warm_up_loop().await;
        let _ = tx.send(Some(result));
    }

    async fn warm_up_loop(&self) -> WarmupResult {
        self.set_state(State::Warming);
        let start = Instant::now();
        let deadline = start + Duration::from_secs_f64(self.config.warmup_timeout_s);
        let mut backoff: f64 = 1.0;
        let mut backend_started = false;
        loop {
            let remaining = deadline
                .saturating_duration_since(Instant::now())
                .as_secs_f64();
            if remaining <= 0.0 {
                tracing::error!(
                    timeout = self.config.warmup_timeout_s,
                    "warmup timed out; state -> COLD"
                );
                self.set_state(State::Cold);
                return Err(WarmupTimeout("endpoint warmup timeout".into()).into());
            }
            match self.warm_up_attempt(remaining, &mut backend_started).await {
                Attempt::Ready => {
                    tracing::info!("warmup: endpoint ready; state -> WARM");
                    let mut s = self.state.lock().unwrap();
                    s.state = State::Warm;
                    s.last_warmup_at = Some(now_secs());
                    s.consecutive_keepalive_failures = 0;
                    s.warmups += 1;
                    return Ok(());
                }
                Attempt::PodNotFound(err) => {
                    tracing::error!(%err, "warmup failed fast; state -> COLD");
                    self.set_state(State::Cold);
                    return Err(WarmupError(err.to_string()));
                }
                Attempt::Retry => {
                    let sleep = Duration::from_secs_f64(backoff.min(remaining.max(0.0)));
                    tokio::time::sleep(sleep).await;
                    backoff = (backoff * 2.0).min(self.config.warmup_backoff_max_s);
                }
            }
        }
    }

    /// One warmup attempt: start the backend (once), probe, classify.
    async fn warm_up_attempt(&self, remaining: f64, backend_started: &mut bool) -> Attempt {
        if !*backend_started {
            let budget = Duration::from_secs_f64(remaining);
            let started = tokio::time::timeout(budget, self.lifecycle.start(remaining)).await;
            match started {
                Ok(Ok(())) => *backend_started = true,
                Ok(Err(err @ LifecycleError::PodNotFound(_))) => {
                    return Attempt::PodNotFound(err);
                }
                Ok(Err(LifecycleError::General(msg))) => {
                    tracing::warn!(%msg, "warmup: backend start failed; retrying");
                    return Attempt::Retry;
                }
                Err(_) => {
                    tracing::warn!("warmup: backend start timed out; retrying");
                    return Attempt::Retry;
                }
            }
        }
        let url = self
            .target
            .lock()
            .unwrap()
            .warmup_url(&self.config.warmup_path);
        let pod_id = self.target_pod_id();
        let headers = self.config.auth_headers_for_pod(&pod_id);
        let probe = tokio::time::timeout(
            Duration::from_secs_f64(remaining),
            self.client.get(&url, &headers),
        )
        .await;
        let response = match probe {
            Ok(Ok(r)) => r,
            Ok(Err(e)) => {
                tracing::warn!(%e, "warmup: probe failed; retrying");
                return Attempt::Retry;
            }
            Err(_) => {
                tracing::warn!("warmup: probe timed out; retrying");
                return Attempt::Retry;
            }
        };
        let pod_mode = self.config.mode == "pod";
        let active = self.lifecycle.active_model();
        let model = if active.is_empty() {
            self.config.default_model()
        } else {
            active
        };
        let classify_timeout = 10.0f64.min(remaining.max(0.01));
        let (healthy, reason) = classify(
            self.client.as_ref(),
            &self.config.pod_health_mode,
            pod_mode,
            &response,
            &self.target_url(),
            &self.config.warmup_path,
            &model,
            &headers,
            classify_timeout,
        )
        .await;
        if healthy {
            Attempt::Ready
        } else {
            tracing::info!(%reason, "warmup: not ready yet; retrying");
            Attempt::Retry
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::health::{ProbeError, ProbeResponse};

    /// A probe client that always returns the same scripted response.
    struct FixedClient {
        response: Mutex<Result<ProbeResponse, ProbeError>>,
    }

    impl FixedClient {
        fn new(response: Result<ProbeResponse, ProbeError>) -> Self {
            Self {
                response: Mutex::new(response),
            }
        }
    }

    #[async_trait::async_trait]
    impl ProbeClient for FixedClient {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            self.response.lock().unwrap().clone()
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &serde_json::Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            self.response.lock().unwrap().clone()
        }
    }

    /// A lifecycle that counts start calls and returns scripted results.
    struct MockLifecycle {
        start_calls: AtomicUsize,
        start_result: Mutex<Result<(), LifecycleError>>,
        stop_result: Mutex<Result<(), LifecycleError>>,
    }

    impl MockLifecycle {
        fn new() -> Self {
            Self {
                start_calls: AtomicUsize::new(0),
                start_result: Mutex::new(Ok(())),
                stop_result: Mutex::new(Ok(())),
            }
        }
        fn set_start_result(&self, r: Result<(), LifecycleError>) {
            *self.start_result.lock().unwrap() = r;
        }
        fn start_count(&self) -> usize {
            self.start_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Lifecycle for MockLifecycle {
        async fn start(&self, _budget: f64) -> Result<(), LifecycleError> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            self.start_result.lock().unwrap().clone()
        }
        async fn stop(&self) -> Result<(), LifecycleError> {
            self.stop_result.lock().unwrap().clone()
        }
    }

    fn config(pairs: &[(&str, &str)]) -> Config {
        let mut env = BTreeMap::new();
        for (k, v) in pairs {
            env.insert(k.to_string(), v.to_string());
        }
        Config::from_env_map(&env).unwrap()
    }

    /// A "model" health config: ready when the probe lists the model.
    fn model_config(extra: &[(&str, &str)]) -> Config {
        let mut pairs: Vec<(&str, &str)> =
            vec![("RUNPOD_MODEL_NAME", "qwen"), ("POD_HEALTH_MODE", "model")];
        pairs.extend_from_slice(extra);
        config(&pairs)
    }

    fn ready_response() -> ProbeResponse {
        ProbeResponse::json(200, &serde_json::json!({"data": [{"id": "qwen"}]}))
    }

    fn not_ready_response() -> ProbeResponse {
        ProbeResponse::json(200, &serde_json::json!({"data": []}))
    }

    fn build(
        cfg: Config,
        client: Arc<FixedClient>,
        lifecycle: Arc<MockLifecycle>,
    ) -> (WarmupManager, Arc<std::sync::Mutex<EndpointState>>) {
        let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
        let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(
            "http://upstream",
            "",
        )));
        let manager = WarmupManager::new(
            Arc::new(cfg),
            client as Arc<dyn ProbeClient>,
            state.clone(),
            lifecycle as Arc<dyn Lifecycle>,
            target,
        );
        (manager, state)
    }

    #[tokio::test]
    async fn warmup_success_sets_warm() {
        let cfg = model_config(&[("WARMUP_TIMEOUT_S", "5")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (manager, state) = build(cfg, client.clone(), lifecycle.clone());

        assert!(manager.ensure_warm(false).await.is_ok());

        let s = state.lock().unwrap();
        assert_eq!(s.state, State::Warm);
        assert_eq!(s.warmups, 1);
        assert!(s.last_warmup_at.is_some());
        assert_eq!(s.consecutive_keepalive_failures, 0);
        assert_eq!(lifecycle.start_count(), 1);
    }

    #[tokio::test]
    async fn warmup_timeout_returns_cold() {
        let cfg = model_config(&[("WARMUP_TIMEOUT_S", "0.3")]);
        let client = Arc::new(FixedClient::new(Ok(not_ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (manager, state) = build(cfg, client, lifecycle);

        let err = manager.ensure_warm(false).await.unwrap_err();
        assert!(
            err.to_string().contains("timeout"),
            "expected timeout, got: {err}"
        );
        assert_eq!(state.lock().unwrap().state, State::Cold);
    }

    #[tokio::test]
    async fn warmup_pod_not_found_fails_fast() {
        let cfg = model_config(&[("WARMUP_TIMEOUT_S", "5")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        lifecycle.set_start_result(Err(LifecycleError::PodNotFound("pod-1".into())));
        let (manager, state) = build(cfg, client, lifecycle);

        let err = manager.ensure_warm(false).await.unwrap_err();
        assert!(
            err.to_string().contains("pod not found"),
            "expected pod-not-found, got: {err}"
        );
        assert_eq!(state.lock().unwrap().state, State::Cold);
    }

    #[tokio::test]
    async fn single_flight_shares_warmup() {
        // The probe is never ready, so the warmup runs to the (short) timeout.
        // All concurrent waiters must share ONE warmup chain (one start call).
        let cfg = model_config(&[("WARMUP_TIMEOUT_S", "0.4")]);
        let client = Arc::new(FixedClient::new(Ok(not_ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (manager, _state) = build(cfg, client, lifecycle.clone());

        let mut handles = Vec::new();
        for _ in 0..10 {
            let m = manager.clone();
            handles.push(tokio::spawn(async move { m.ensure_warm(false).await }));
        }
        for h in handles {
            assert!(h.await.unwrap().is_err());
        }
        // Exactly one warmup chain ran, despite 10 concurrent waiters.
        assert_eq!(lifecycle.start_count(), 1);
    }

    #[tokio::test]
    async fn timeout_wakes_all_waiters() {
        let cfg = model_config(&[("WARMUP_TIMEOUT_S", "0.3")]);
        let client = Arc::new(FixedClient::new(Ok(not_ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (manager, _state) = build(cfg, client, lifecycle);

        let mut handles = Vec::new();
        for _ in 0..5 {
            let m = manager.clone();
            handles.push(tokio::spawn(async move { m.ensure_warm(false).await }));
        }
        // Every waiter gets the timeout error (none hangs).
        for h in handles {
            let err = h.await.unwrap().unwrap_err();
            assert!(err.to_string().contains("timeout"), "got: {err}");
        }
    }

    #[tokio::test]
    async fn ensure_warm_fast_path_when_warm() {
        let cfg = model_config(&[("WARMUP_TIMEOUT_S", "5")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (manager, state) = build(cfg, client.clone(), lifecycle.clone());

        manager.ensure_warm(false).await.unwrap();
        // Second call: already warm, no new warmup (start count stays 1).
        manager.ensure_warm(false).await.unwrap();
        assert_eq!(lifecycle.start_count(), 1);
        assert_eq!(state.lock().unwrap().warmups, 1);
    }
}
