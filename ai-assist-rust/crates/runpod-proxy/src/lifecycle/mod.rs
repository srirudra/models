//! Pod lifecycle strategies: serverless, pinned, discovery (WI-08/WI-09).
//!
//! The `Lifecycle` trait is the seam between the warmup/keepalive (WI-06) and
//! the concrete pod-management strategies (serverless WI-08, pinned WI-08,
//! discovery WI-09). WI-06 defines the trait + the trivial serverless impl;
//! the pinned/discovery strategies land in WI-08/WI-09.

pub mod discovery;
pub mod pinned;
pub mod serverless;

use async_trait::async_trait;

/// A lifecycle start/stop call failed, or the pinned pod no longer exists.
#[derive(Debug, Clone, thiserror::Error)]
pub enum LifecycleError {
    /// A transport error or non-2xx response during start/stop.
    #[error("{0}")]
    General(String),
    /// The pinned pod no longer exists on `RunPod` (GET /pods/{id} -> 404). A
    /// deleted pod's ID is never reused, so a warmup that hits this should fail
    /// fast instead of burning the whole budget.
    #[error("pod not found: {0}")]
    PodNotFound(String),
}

impl LifecycleError {
    /// Build a general (non-fatal) lifecycle error.
    pub fn general(msg: impl Into<String>) -> Self {
        Self::General(msg.into())
    }
}

/// The seam between warmup/keepalive and the concrete pod-management strategy.
#[async_trait]
pub trait Lifecycle: Send + Sync {
    /// Start the backend, given the remaining warmup budget (seconds). Must
    /// succeed once per warmup before readiness probes begin.
    async fn start(&self, budget: f64) -> Result<(), LifecycleError>;

    /// Stop the backend (pod mode: REST stop; serverless: no-op).
    async fn stop(&self) -> Result<(), LifecycleError>;

    /// Retry any pending backend stops (cost safety, spec section 11). Default
    /// no-op for strategies without pending stops.
    async fn retry_pending_stops(&self) {}

    /// The model currently active on the backend (default empty).
    fn active_model(&self) -> String {
        String::new()
    }

    /// True when this lifecycle can switch the active model per request
    /// (discovery pod mode). Pinned/serverless serve one fixed model, so a
    /// model switch is a no-op for them (parity with Python's
    /// `isinstance(lifecycle, DiscoveryPodLifecycle)` check in the router).
    fn supports_switch(&self) -> bool {
        false
    }

    /// Set the active model (discovery lifecycle only; default no-op).
    fn set_active_model(&self, _model: &str) {}

    /// The discovery status (last discovery error + circuit-breaker seconds)
    /// for `/_status`. `None` for non-discovery strategies (parity with the
    /// Python `isinstance(lifecycle, DiscoveryPodLifecycle)` guard).
    fn discovery_view(&self) -> Option<DiscoveryView> {
        None
    }
}

/// The discovery lifecycle's `/_status` fields.
pub struct DiscoveryView {
    /// The last discovery error (empty string when there is none).
    pub last_error: String,
    /// Seconds remaining on the active model's create circuit breaker.
    pub circuit_breaker_open_s: f64,
}
