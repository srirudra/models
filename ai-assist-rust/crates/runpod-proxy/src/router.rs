//! Model resolution, leases, serialized switches (spec section 6.3). WI-10.
//!
//! WI-07 lands the pure `resolve` (allowlist + slug matching) and the request
//! lease (in-flight counter). The serialized model switch (drain, stop,
//! re-warm) lands in WI-10.

use std::sync::Arc;
use std::sync::atomic::{AtomicIsize, Ordering};

use tokio::sync::watch;

use crate::config::Config;
use crate::runpod_api::model_slug;

/// Per-request model selection and (later) discovery pod switching.
pub struct ModelRouter {
    config: Arc<Config>,
    /// The model the endpoint is currently serving.
    active_model: std::sync::Mutex<String>,
    /// In-flight request count (lock-free; the switch drain observes it).
    in_flight: Arc<AtomicIsize>,
    /// `true` when `in_flight == 0`. A draining model switch waits on this so
    /// it can proceed once the last in-flight request for the old model ends.
    idle_tx: Arc<watch::Sender<bool>>,
}

impl ModelRouter {
    /// Build a router over the shared config.
    pub fn new(config: Arc<Config>) -> Self {
        let default = config.default_model();
        let (idle_tx, _) = watch::channel(true);
        Self {
            config,
            active_model: std::sync::Mutex::new(default),
            in_flight: Arc::new(AtomicIsize::new(0)),
            idle_tx: Arc::new(idle_tx),
        }
    }

    /// The model the endpoint is currently serving.
    pub fn active_model(&self) -> String {
        self.active_model.lock().unwrap().clone()
    }

    /// Adopt a new active model (called by the proxy on a model switch).
    pub fn set_active_model(&self, model: &str) {
        *self.active_model.lock().unwrap() = model.to_string();
    }

    /// A receiver for the idle signal (`true` when no requests are in flight),
    /// used by the switch drain.
    pub fn subscribe_idle(&self) -> watch::Receiver<bool> {
        self.idle_tx.subscribe()
    }

    /// Resolve a requested model against the allowlist (spec section 6.3).
    ///
    /// Returns `(model, rejected)`: the canonical model to route to, and the
    /// requested model when it was rejected (allowlist miss). A request must
    /// never fail for omitting a model (falls back to the default).
    pub fn resolve(&self, requested: Option<&str>) -> (Option<String>, Option<String>) {
        let model = requested.map_or_else(|| self.config.default_model(), str::to_string);
        if !self.config.allowlist_configured() {
            return (Some(self.config.default_model()), None);
        }
        let wanted = model.to_lowercase();
        let wanted_slug = model_slug(&model);
        for allowed in self.config.effective_allowed_models() {
            if allowed.to_lowercase() == wanted || model_slug(&allowed) == wanted_slug {
                return (Some(allowed), None);
            }
        }
        (None, Some(model))
    }

    /// Acquire a request lease. The returned guard releases the lease exactly
    /// once on drop (or when the response stream ends).
    pub fn lease(&self) -> RequestLease {
        // Going 0 -> 1 clears the idle signal so a switch drain must wait.
        if self.in_flight.fetch_add(1, Ordering::SeqCst) == 0 {
            let _ = self.idle_tx.send(false);
        }
        RequestLease {
            counter: Arc::clone(&self.in_flight),
            idle_tx: Arc::clone(&self.idle_tx),
        }
    }

    /// The current in-flight request count (for tests / the switch drain).
    pub fn in_flight(&self) -> isize {
        self.in_flight.load(Ordering::SeqCst)
    }
}

/// A request lease: increments the in-flight count on creation and decrements
/// it exactly once on drop.
pub struct RequestLease {
    counter: Arc<AtomicIsize>,
    idle_tx: Arc<watch::Sender<bool>>,
}

impl Drop for RequestLease {
    fn drop(&mut self) {
        // Going 1 -> 0 raises the idle signal so a draining switch can proceed.
        if self.counter.fetch_sub(1, Ordering::SeqCst) == 1 {
            let _ = self.idle_tx.send(true);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn config(env: &[(&str, &str)]) -> Arc<Config> {
        let map: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(Config::from_env_map(&map).unwrap())
    }

    #[test]
    fn no_allowlist_returns_default() {
        let router = ModelRouter::new(config(&[("RUNPOD_MODEL_NAME", "qwen")]));
        let (model, rejected) = router.resolve(Some("anything"));
        assert_eq!(model.as_deref(), Some("qwen"));
        assert_eq!(rejected, None);
    }

    #[test]
    fn no_requested_falls_back_to_default() {
        let router = ModelRouter::new(config(&[("RUNPOD_MODEL_NAME", "qwen")]));
        let (model, rejected) = router.resolve(None);
        assert_eq!(model.as_deref(), Some("qwen"));
        assert_eq!(rejected, None);
    }

    #[test]
    fn allowlist_exact_match() {
        let router = ModelRouter::new(config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("RUNPOD_ALLOWED_MODELS", "qwen,llama"),
        ]));
        let (model, rejected) = router.resolve(Some("llama"));
        assert_eq!(model.as_deref(), Some("llama"));
        assert_eq!(rejected, None);
    }

    #[test]
    fn allowlist_case_insensitive_match() {
        let router = ModelRouter::new(config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("RUNPOD_ALLOWED_MODELS", "Qwen,LLaMA"),
        ]));
        let (model, rejected) = router.resolve(Some("llama"));
        assert_eq!(model.as_deref(), Some("LLaMA"));
        assert_eq!(rejected, None);
    }

    #[test]
    fn allowlist_slug_match() {
        let router = ModelRouter::new(config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("RUNPOD_ALLOWED_MODELS", "Qwen/Qwen3.8-27B"),
        ]));
        // A slug variant of the allowed model resolves to the canonical name.
        let (model, rejected) = router.resolve(Some("qwen-qwen3-8-27b"));
        assert_eq!(model.as_deref(), Some("Qwen/Qwen3.8-27B"));
        assert_eq!(rejected, None);
    }

    #[test]
    fn allowlist_miss_is_rejected() {
        let router = ModelRouter::new(config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("RUNPOD_ALLOWED_MODELS", "qwen,llama"),
        ]));
        let (model, rejected) = router.resolve(Some("gpt4"));
        assert_eq!(model, None);
        assert_eq!(rejected.as_deref(), Some("gpt4"));
    }

    #[test]
    fn lease_increments_and_decrements() {
        let router = ModelRouter::new(config(&[("RUNPOD_MODEL_NAME", "qwen")]));
        assert_eq!(router.in_flight(), 0);
        {
            let _lease = router.lease();
            assert_eq!(router.in_flight(), 1);
            let _lease2 = router.lease();
            assert_eq!(router.in_flight(), 2);
        }
        assert_eq!(router.in_flight(), 0);
    }

    #[test]
    fn active_model_defaults_to_default_model() {
        let router = ModelRouter::new(config(&[("RUNPOD_MODEL_NAME", "qwen")]));
        assert_eq!(router.active_model(), "qwen");
    }

    #[test]
    fn idle_signal_tracks_in_flight_transitions() {
        let router = ModelRouter::new(config(&[("RUNPOD_MODEL_NAME", "qwen")]));
        let mut idle = router.subscribe_idle();
        // Idle at rest.
        assert!(*idle.borrow_and_update());
        let lease = router.lease();
        // 0 -> 1 clears the idle signal.
        assert!(!*idle.borrow_and_update());
        let lease2 = router.lease();
        // 1 -> 2 does not re-signal (no change).
        assert!(!idle.has_changed().unwrap());
        drop(lease2);
        // 2 -> 1 stays busy.
        assert!(!*idle.borrow_and_update());
        drop(lease);
        // 1 -> 0 raises the idle signal.
        assert!(*idle.borrow_and_update());
    }

    #[test]
    fn set_active_model_updates_current() {
        let router = ModelRouter::new(config(&[("RUNPOD_MODEL_NAME", "qwen")]));
        router.set_active_model("llama");
        assert_eq!(router.active_model(), "llama");
    }
}
