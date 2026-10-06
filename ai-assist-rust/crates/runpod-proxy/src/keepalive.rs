//! Keepalive probe loop + idle give-up (spec section 6.2). WI-06.
//!
//! While the endpoint is WARM or DEGRADED and has seen real traffic, the
//! keepalive loop probes the warmup route on an interval: a healthy probe
//! heals DEGRADED back to WARM and resets the failure counter; three
//! consecutive unhealthy probes mark the endpoint DEGRADED. If the endpoint
//! has been idle (no real traffic) for longer than `IDLE_GIVEUP_S`, the
//! backend is stopped to stop the billing clock (cost safety, spec section 11).
//!
//! The loop is shared across the app via `Arc<KeepaliveLoop>` (it is not
//! `Clone` because it owns a `watch::Sender`). `tick` is exposed so tests can
//! drive a single iteration without the interval timer.

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::watch;
use tokio::task::JoinHandle;

use crate::config::Config;
use crate::health::{ProbeClient, classify};
use crate::lifecycle::Lifecycle;
use crate::state::{EndpointState, State, now_secs};
use crate::target::UpstreamTarget;

/// The keepalive probe loop (spec section 6.2).
pub struct KeepaliveLoop {
    config: Arc<Config>,
    client: Arc<dyn ProbeClient>,
    state: Arc<std::sync::Mutex<EndpointState>>,
    lifecycle: Arc<dyn Lifecycle>,
    target: Arc<std::sync::Mutex<UpstreamTarget>>,
    /// Stop signal: `true` asks the loop to exit.
    stop_tx: watch::Sender<bool>,
    /// The running loop task, if any.
    task: Arc<tokio::sync::Mutex<Option<JoinHandle<()>>>>,
}

impl KeepaliveLoop {
    /// Build a keepalive loop over the shared state/target/lifecycle.
    pub fn new(
        config: Arc<Config>,
        client: Arc<dyn ProbeClient>,
        state: Arc<std::sync::Mutex<EndpointState>>,
        lifecycle: Arc<dyn Lifecycle>,
        target: Arc<std::sync::Mutex<UpstreamTarget>>,
    ) -> Self {
        let (stop_tx, _) = watch::channel(false);
        Self {
            config,
            client,
            state,
            lifecycle,
            target,
            stop_tx,
            task: Arc::new(tokio::sync::Mutex::new(None)),
        }
    }

    /// Start the loop (idempotent: a second call is a no-op).
    pub async fn start(&self) {
        let mut task = self.task.lock().await;
        if task.is_some() {
            return;
        }
        // Reset the stop flag in case a previous stop() set it.
        let _ = self.stop_tx.send(false);
        let stop_rx = self.stop_tx.subscribe();
        let config = self.config.clone();
        let client = self.client.clone();
        let state = self.state.clone();
        let lifecycle = self.lifecycle.clone();
        let target = self.target.clone();
        let handle = tokio::spawn(async move {
            run_loop(config, client, state, lifecycle, target, stop_rx).await;
        });
        *task = Some(handle);
    }

    /// Stop the loop (idempotent). Signals the loop and aborts the task.
    pub async fn stop(&self) {
        let _ = self.stop_tx.send(true);
        let mut task = self.task.lock().await;
        if let Some(handle) = task.take() {
            handle.abort();
            let _ = handle.await;
        }
    }

    /// Whether the loop task is currently running (tests).
    #[cfg(test)]
    pub async fn is_running(&self) -> bool {
        self.task.lock().await.is_some()
    }

    /// Run one keepalive iteration (exposed for tests; the loop calls this on
    /// each interval tick).
    #[cfg(test)]
    pub async fn tick(&self) {
        tick_inner(
            &self.config,
            self.client.as_ref(),
            &self.state,
            self.lifecycle.as_ref(),
            &self.target,
        )
        .await;
    }
}

/// The interval loop: tick every `KEEPALIVE_INTERVAL_S` until stopped.
async fn run_loop(
    config: Arc<Config>,
    client: Arc<dyn ProbeClient>,
    state: Arc<std::sync::Mutex<EndpointState>>,
    lifecycle: Arc<dyn Lifecycle>,
    target: Arc<std::sync::Mutex<UpstreamTarget>>,
    mut stop_rx: watch::Receiver<bool>,
) {
    let mut interval = tokio::time::interval(Duration::from_secs_f64(config.keepalive_interval_s));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            _ = stop_rx.changed() => {
                if *stop_rx.borrow() {
                    tracing::info!("keepalive: stop requested; exiting loop");
                    return;
                }
            }
            _ = interval.tick() => {
                tick_inner(&config, client.as_ref(), &state, lifecycle.as_ref(), &target)
                    .await;
            }
        }
    }
}

/// One keepalive iteration (spec section 6.2).
async fn tick_inner(
    config: &Config,
    client: &dyn ProbeClient,
    state: &std::sync::Mutex<EndpointState>,
    lifecycle: &dyn Lifecycle,
    target: &std::sync::Mutex<UpstreamTarget>,
) {
    // Cost safety: retry any pending backend stops (spec section 11).
    lifecycle.retry_pending_stops().await;

    // Only probe when WARM/DEGRADED and there has been real traffic.
    let idle_for = {
        let s = state.lock().unwrap();
        match s.state {
            State::Warm | State::Degraded => s.last_real_traffic_at.map(|last| now_secs() - last),
            _ => return,
        }
    };
    let Some(idle_for) = idle_for else {
        return;
    };

    // Idle give-up: stop the backend if it has been idle too long.
    if idle_for > config.idle_giveup_s {
        tracing::info!(idle = idle_for, "keepalive: idle give-up; stopping backend");
        match lifecycle.stop().await {
            Ok(()) => {
                let mut s = state.lock().unwrap();
                s.state = State::Cold;
                tracing::info!("keepalive: backend stopped; state -> COLD");
            }
            Err(e) => {
                tracing::warn!(%e, "keepalive: stop failed; will retry next tick");
            }
        }
        return;
    }

    // Probe the warmup route and classify.
    let (url, pod_id, base_url) = {
        let t = target.lock().unwrap();
        (
            t.warmup_url(&config.warmup_path),
            t.pod_id().to_string(),
            t.url().to_string(),
        )
    };
    let headers = config.auth_headers_for_pod(&pod_id);
    let probe = tokio::time::timeout(
        Duration::from_secs_f64(config.keepalive_interval_s),
        client.get(&url, &headers),
    )
    .await;
    let response = match probe {
        Ok(Ok(r)) => r,
        Ok(Err(e)) => {
            record_failure(state, &format!("probe error: {e}"));
            return;
        }
        Err(_) => {
            record_failure(state, "probe timeout");
            return;
        }
    };
    let pod_mode = config.mode == "pod";
    let active = lifecycle.active_model();
    let model = if active.is_empty() {
        config.default_model()
    } else {
        active
    };
    let (healthy, reason) = classify(
        client,
        &config.pod_health_mode,
        pod_mode,
        &response,
        &base_url,
        &config.warmup_path,
        &model,
        &headers,
        10.0,
    )
    .await;
    if healthy {
        let mut s = state.lock().unwrap();
        s.last_keepalive_at = Some(now_secs());
        if s.state == State::Degraded {
            tracing::info!("keepalive: endpoint healed; state -> WARM");
            s.state = State::Warm;
        }
        s.consecutive_keepalive_failures = 0;
    } else {
        record_failure(state, &reason);
    }
}

/// Record an unhealthy probe: bump the failure counters; 3 consecutive
/// failures mark the endpoint DEGRADED.
fn record_failure(state: &std::sync::Mutex<EndpointState>, reason: &str) {
    let mut s = state.lock().unwrap();
    s.consecutive_keepalive_failures += 1;
    s.keepalive_failures_total += 1;
    s.last_keepalive_at = Some(now_secs());
    if s.consecutive_keepalive_failures >= 3 && s.state != State::Degraded {
        tracing::info!(
            failures = s.consecutive_keepalive_failures,
            %reason,
            "keepalive: 3 consecutive failures; state -> DEGRADED"
        );
        s.state = State::Degraded;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::health::{ProbeError, ProbeResponse};
    use crate::lifecycle::LifecycleError;

    /// A probe client that always returns the same scripted response.
    struct FixedClient {
        response: Mutex<Result<ProbeResponse, ProbeError>>,
        calls: AtomicUsize,
    }

    impl FixedClient {
        fn new(response: Result<ProbeResponse, ProbeError>) -> Self {
            Self {
                response: Mutex::new(response),
                calls: AtomicUsize::new(0),
            }
        }
        fn call_count(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl ProbeClient for FixedClient {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response.lock().unwrap().clone()
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &serde_json::Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.response.lock().unwrap().clone()
        }
    }

    /// A lifecycle that counts start/stop calls and returns scripted results.
    struct MockLifecycle {
        start_calls: AtomicUsize,
        stop_calls: AtomicUsize,
        start_result: Mutex<Result<(), LifecycleError>>,
        stop_result: Mutex<Result<(), LifecycleError>>,
    }

    impl MockLifecycle {
        fn new() -> Self {
            Self {
                start_calls: AtomicUsize::new(0),
                stop_calls: AtomicUsize::new(0),
                start_result: Mutex::new(Ok(())),
                stop_result: Mutex::new(Ok(())),
            }
        }
        fn set_stop_result(&self, r: Result<(), LifecycleError>) {
            *self.stop_result.lock().unwrap() = r;
        }
        fn stop_count(&self) -> usize {
            self.stop_calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl Lifecycle for MockLifecycle {
        async fn start(&self, _budget: f64) -> Result<(), LifecycleError> {
            self.start_calls.fetch_add(1, Ordering::SeqCst);
            self.start_result.lock().unwrap().clone()
        }
        async fn stop(&self) -> Result<(), LifecycleError> {
            self.stop_calls.fetch_add(1, Ordering::SeqCst);
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
    ) -> (KeepaliveLoop, Arc<std::sync::Mutex<EndpointState>>) {
        let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
        let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(
            "http://upstream",
            "",
        )));
        let loop_ = KeepaliveLoop::new(
            Arc::new(cfg),
            client as Arc<dyn ProbeClient>,
            state.clone(),
            lifecycle as Arc<dyn Lifecycle>,
            target,
        );
        (loop_, state)
    }

    #[tokio::test]
    async fn tick_cold_no_ping() {
        let cfg = model_config(&[]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, state) = build(cfg, client.clone(), lifecycle);

        // State is COLD (default): no probe.
        loop_.tick().await;

        assert_eq!(client.call_count(), 0);
        assert_eq!(state.lock().unwrap().state, State::Cold);
    }

    #[tokio::test]
    async fn tick_warm_no_traffic_no_ping() {
        let cfg = model_config(&[]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, state) = build(cfg, client.clone(), lifecycle);

        {
            let mut s = state.lock().unwrap();
            s.state = State::Warm;
            // last_real_traffic_at is None: no real traffic yet.
        }

        loop_.tick().await;

        assert_eq!(client.call_count(), 0);
        assert_eq!(state.lock().unwrap().state, State::Warm);
    }

    #[tokio::test]
    async fn tick_idle_giveup_stops() {
        let cfg = model_config(&[("IDLE_GIVEUP_S", "10")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, state) = build(cfg, client.clone(), lifecycle.clone());

        {
            let mut s = state.lock().unwrap();
            s.state = State::Warm;
            s.last_real_traffic_at = Some(now_secs() - 20.0); // idle 20s > 10s
        }

        loop_.tick().await;

        assert_eq!(lifecycle.stop_count(), 1);
        assert_eq!(state.lock().unwrap().state, State::Cold);
        // Stopped before probing.
        assert_eq!(client.call_count(), 0);
    }

    #[tokio::test]
    async fn tick_idle_giveup_stop_fails() {
        let cfg = model_config(&[("IDLE_GIVEUP_S", "10")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        lifecycle.set_stop_result(Err(LifecycleError::general("stop failed")));
        let (loop_, state) = build(cfg, client, lifecycle.clone());

        {
            let mut s = state.lock().unwrap();
            s.state = State::Warm;
            s.last_real_traffic_at = Some(now_secs() - 20.0);
        }

        loop_.tick().await;

        assert_eq!(lifecycle.stop_count(), 1);
        // Stop failed, so state stays WARM (retried next tick).
        assert_eq!(state.lock().unwrap().state, State::Warm);
    }

    #[tokio::test]
    async fn tick_success_resets_failures() {
        let cfg = model_config(&[("IDLE_GIVEUP_S", "300")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, state) = build(cfg, client.clone(), lifecycle);

        {
            let mut s = state.lock().unwrap();
            s.state = State::Warm;
            s.last_real_traffic_at = Some(now_secs()); // recent traffic
            s.consecutive_keepalive_failures = 2;
        }

        loop_.tick().await;

        let s = state.lock().unwrap();
        assert_eq!(s.state, State::Warm);
        assert_eq!(s.consecutive_keepalive_failures, 0);
        assert!(s.last_keepalive_at.is_some());
        assert_eq!(client.call_count(), 1);
    }

    #[tokio::test]
    async fn tick_degraded_heals() {
        let cfg = model_config(&[("IDLE_GIVEUP_S", "300")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, state) = build(cfg, client, lifecycle);

        {
            let mut s = state.lock().unwrap();
            s.state = State::Degraded;
            s.last_real_traffic_at = Some(now_secs());
            s.consecutive_keepalive_failures = 3;
        }

        loop_.tick().await;

        let s = state.lock().unwrap();
        assert_eq!(s.state, State::Warm);
        assert_eq!(s.consecutive_keepalive_failures, 0);
    }

    #[tokio::test]
    async fn tick_three_failures_degraded() {
        let cfg = model_config(&[("IDLE_GIVEUP_S", "300")]);
        let client = Arc::new(FixedClient::new(Ok(not_ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, state) = build(cfg, client, lifecycle);

        {
            let mut s = state.lock().unwrap();
            s.state = State::Warm;
            s.last_real_traffic_at = Some(now_secs());
        }

        loop_.tick().await;
        loop_.tick().await;
        assert_eq!(state.lock().unwrap().state, State::Warm); // 2 failures
        loop_.tick().await;
        let s = state.lock().unwrap();
        assert_eq!(s.state, State::Degraded);
        assert_eq!(s.consecutive_keepalive_failures, 3);
        assert_eq!(s.keepalive_failures_total, 3);
    }

    #[tokio::test]
    async fn start_stop_lifecycle() {
        let cfg = model_config(&[("KEEPALIVE_INTERVAL_S", "0.05")]);
        let client = Arc::new(FixedClient::new(Ok(ready_response())));
        let lifecycle = Arc::new(MockLifecycle::new());
        let (loop_, _state) = build(cfg, client, lifecycle);

        assert!(!loop_.is_running().await);
        loop_.start().await;
        assert!(loop_.is_running().await);
        loop_.start().await; // idempotent
        assert!(loop_.is_running().await);
        loop_.stop().await;
        assert!(!loop_.is_running().await);
        loop_.stop().await; // idempotent
        assert!(!loop_.is_running().await);
    }
}
