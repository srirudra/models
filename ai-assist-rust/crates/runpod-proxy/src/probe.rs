//! Production `ProbeClient` over `reqwest::Client` (spec section 8.4). WI-07.
//!
//! A single shared `reqwest::Client` (connection pooling) backs both the
//! warmup/keepalive probes and the request forwarding.

use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

use crate::health::{ProbeClient, ProbeError, ProbeResponse};

/// Production `ProbeClient` over a shared `reqwest::Client`.
pub struct ReqwestProbeClient {
    client: reqwest::Client,
}

impl ReqwestProbeClient {
    /// Build a probe client over a shared `reqwest::Client`.
    pub fn new(client: reqwest::Client) -> Self {
        Self { client }
    }
}

/// Map a `reqwest` error to the probe error taxonomy (timeout vs connect).
fn map_reqwest_error(e: &reqwest::Error) -> ProbeError {
    if e.is_timeout() {
        ProbeError(format!("timeout: {e}"))
    } else {
        ProbeError(format!("connect: {e}"))
    }
}

#[async_trait]
impl ProbeClient for ReqwestProbeClient {
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<ProbeResponse, ProbeError> {
        let mut req = self.client.get(url);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| map_reqwest_error(&e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(ProbeResponse { status, body })
    }

    async fn post_json(
        &self,
        url: &str,
        body: &Value,
        headers: &[(String, String)],
        timeout: f64,
    ) -> Result<ProbeResponse, ProbeError> {
        let mut req = self
            .client
            .post(url)
            .timeout(Duration::from_secs_f64(timeout))
            .header("content-type", "application/json")
            .body(body.to_string());
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let resp = req.send().await.map_err(|e| map_reqwest_error(&e))?;
        let status = resp.status().as_u16();
        let body = resp.text().await.unwrap_or_default();
        Ok(ProbeResponse { status, body })
    }
}
