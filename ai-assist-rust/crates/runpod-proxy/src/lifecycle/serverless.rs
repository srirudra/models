//! Serverless (queue-based endpoint) lifecycle. WI-08.
//!
//! Serverless endpoints need no explicit start/stop: the `RunPod` queue handles
//! pod lifecycle. `start` and `stop` are no-ops, so the idle give-up transition
//! (spec section 4, rule 4) happens unconditionally in serverless mode.

use async_trait::async_trait;

use super::{Lifecycle, LifecycleError};

/// Serverless lifecycle: no explicit start/stop (the queue manages the pod).
#[derive(Debug, Default, Clone, Copy)]
pub struct ServerlessLifecycle;

impl ServerlessLifecycle {
    /// Build a serverless lifecycle.
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl Lifecycle for ServerlessLifecycle {
    async fn start(&self, _budget: f64) -> Result<(), LifecycleError> {
        Ok(())
    }

    async fn stop(&self) -> Result<(), LifecycleError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn serverless_start_stop_are_no_ops() {
        let lc = ServerlessLifecycle::new();
        assert!(lc.start(10.0).await.is_ok());
        assert!(lc.stop().await.is_ok());
        assert_eq!(lc.active_model(), "");
    }
}
