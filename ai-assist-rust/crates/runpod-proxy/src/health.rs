//! Upstream health classification: any / model / completion (spec section 6.8a).
//! WI-06 (probe seam + classify); the discovery health loop reuses this in WI-09.
//!
//! A single source of truth for "is the endpoint actually serving the model
//! right now", shared by the warmup manager's final probe and (later) the
//! discovery lifecycle's health loop. The legacy behavior (mode "any") treats
//! any HTTP response as ready; "model" and "completion" exist because a model
//! server can answer its HTTP routes while its weights are still loading.

use async_trait::async_trait;
use serde_json::Value;

use crate::catalogue::py_str_repr;
use crate::runpod_api::model_slug;

/// The chat-completions route, relative to the model server's API prefix.
pub const COMPLETION_PATH: &str = "chat/completions";

/// `RunPod`'s edge/ingress proxy answers on the pod's behalf before the container
/// is ready (404 when nothing listens, 502/503/504 once the port is bound but
/// the app is still starting). These are synthetic "not ready" responses, not
/// answers from the model server, so pod-mode probes must not treat them as
/// healthy (spec section 6.5).
pub const POD_NOT_READY_STATUSES: [u16; 4] = [404, 502, 503, 504];

fn is_not_ready(status: u16) -> bool {
    POD_NOT_READY_STATUSES.contains(&status)
}

/// A minimal HTTP response for probe/health purposes.
#[derive(Debug, Clone)]
pub struct ProbeResponse {
    /// The HTTP status code.
    pub status: u16,
    /// The decoded response body (UTF-8).
    pub body: String,
}

impl ProbeResponse {
    /// Build a response with a status and a JSON-serializable body (tests).
    #[cfg(test)]
    pub fn json(status: u16, body: &Value) -> Self {
        Self {
            status,
            body: body.to_string(),
        }
    }

    /// Build a response with a status and a raw body (tests).
    #[cfg(test)]
    pub fn raw(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }

    /// Parse the body as JSON (if possible).
    pub fn json_value(&self) -> Option<Value> {
        serde_json::from_str(&self.body).ok()
    }
}

/// A probe request failed (transport error, timeout, connection refused).
/// The message carries the failure kind (parity with Python
/// `type(exc).__name__` in the completion-probe error).
#[derive(Debug, Clone, thiserror::Error)]
#[error("{0}")]
pub struct ProbeError(pub String);

/// Abstraction over the HTTP client used for warmup/keepalive probes. The
/// production impl wraps `reqwest::Client` (WI-07); tests use a scripted mock.
#[async_trait]
pub trait ProbeClient: Send + Sync {
    /// Issue a GET probe.
    async fn get(
        &self,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<ProbeResponse, ProbeError>;

    /// Issue a POST with a JSON body (used by the "completion" health mode).
    async fn post_json(
        &self,
        url: &str,
        body: &Value,
        headers: &[(String, String)],
        timeout: f64,
    ) -> Result<ProbeResponse, ProbeError>;
}

/// Derive the chat-completions URL from the warmup route. The warmup path
/// (default "v1/models") shares its prefix with the rest of the model server's
/// API, so "v1/models" -> "<base>/v1/chat/completions" and a bare "models" ->
/// "<base>/chat/completions".
pub fn completion_url(base_url: &str, warmup_path: &str) -> String {
    let parts: Vec<&str> = warmup_path
        .trim_matches('/')
        .split('/')
        .filter(|p| !p.is_empty())
        .collect();
    let prefix = parts[..parts.len().saturating_sub(1)].join("/");
    let base = base_url.trim_end_matches('/');
    if prefix.is_empty() {
        format!("{base}/{COMPLETION_PATH}")
    } else {
        format!("{base}/{prefix}/{COMPLETION_PATH}")
    }
}

/// Check that the target model is listed in a warmup-route (model list) response.
fn check_model_listed(response: &ProbeResponse, model: &str) -> (bool, String) {
    if is_not_ready(response.status) {
        return (false, format!("HTTP {} (not ready)", response.status));
    }
    if !(200..300).contains(&response.status) {
        return (false, format!("HTTP {}", response.status));
    }
    let Some(body) = response.json_value() else {
        return (false, "warmup response was not JSON".to_string());
    };
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        return (false, "warmup response has no model list".to_string());
    };
    let ids: Vec<String> = data
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    if model.is_empty() {
        return (!ids.is_empty(), format!("{} model(s) listed", ids.len()));
    }
    let wanted_cf = model.to_lowercase();
    let wanted_slug = model_slug(model);
    for id in &ids {
        if id.to_lowercase() == wanted_cf || model_slug(id) == wanted_slug {
            return (true, "model listed".to_string());
        }
    }
    (
        false,
        format!(
            "model {} not yet in warmup response ({} listed)",
            py_str_repr(model),
            ids.len()
        ),
    )
}

/// Functional smoke test: a 1-token completion that must actually succeed. This
/// is the only probe that guarantees the model is loadable and serving, because
/// it goes through the full inference path.
async fn check_completion(
    client: &dyn ProbeClient,
    base_url: &str,
    warmup_path: &str,
    model: &str,
    headers: &[(String, String)],
    timeout: f64,
) -> (bool, String) {
    let mut payload = serde_json::json!({
        "messages": [{"role": "user", "content": "ping"}],
        "max_tokens": 1,
        "stream": false,
        "temperature": 0,
    });
    if !model.is_empty() {
        payload["model"] = Value::String(model.to_string());
    }
    let url = completion_url(base_url, warmup_path);
    let response = match client.post_json(&url, &payload, headers, timeout).await {
        Ok(r) => r,
        Err(e) => return (false, format!("completion probe failed: {e}")),
    };
    if !(200..300).contains(&response.status) {
        return (false, format!("completion probe HTTP {}", response.status));
    }
    let Some(body) = response.json_value() else {
        return (false, "completion response was not JSON".to_string());
    };
    let has_choices = match body.get("choices") {
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
        Some(_) => true,
        None => false,
    };
    if has_choices {
        (true, "completion ok".to_string())
    } else {
        (false, "completion response had no choices".to_string())
    }
}

/// Classify a warmup-route response. Returns `(healthy, reason)`.
///
/// `reason` is a short loggable string; callers store it in `last_error` / the
/// warmup log so `/_status` shows why the endpoint is not ready yet.
// Faithful port of the Python `classify` signature (9 params); the probe seam
// is reused by the discovery health loop (WI-09), so the shape is kept stable.
#[allow(clippy::too_many_arguments)]
pub async fn classify(
    client: &dyn ProbeClient,
    mode: &str,
    pod_mode: bool,
    response: &ProbeResponse,
    base_url: &str,
    warmup_path: &str,
    model: &str,
    headers: &[(String, String)],
    timeout: f64,
) -> (bool, String) {
    match mode {
        "model" => check_model_listed(response, model),
        "completion" => {
            if pod_mode && is_not_ready(response.status) {
                return (false, format!("HTTP {} (not ready)", response.status));
            }
            check_completion(client, base_url, warmup_path, model, headers, timeout).await
        }
        // mode == "any" (legacy): any real HTTP response means the endpoint is
        // up. In serverless mode even a 404/502 is a genuine endpoint answer, so
        // only pod mode filters the RunPod edge synthetics.
        _ => {
            if pod_mode && is_not_ready(response.status) {
                (false, format!("HTTP {} (not ready)", response.status))
            } else {
                (true, format!("HTTP {}", response.status))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::sync::Mutex;

    /// A scripted probe client: returns queued responses in order.
    struct Scripted {
        responses: Mutex<VecDeque<Result<ProbeResponse, ProbeError>>>,
    }

    impl Scripted {
        fn new() -> Self {
            Self {
                responses: Mutex::new(VecDeque::new()),
            }
        }
        fn push(&self, r: Result<ProbeResponse, ProbeError>) {
            self.responses.lock().unwrap().push_back(r);
        }
    }

    #[async_trait]
    impl ProbeClient for Scripted {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ProbeError("no scripted response".into())))
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            self.responses
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(ProbeError("no scripted response".into())))
        }
    }

    fn ok(status: u16, body: &Value) -> ProbeResponse {
        ProbeResponse::json(status, body)
    }

    #[test]
    fn completion_url_strips_prefix() {
        assert_eq!(
            completion_url("http://u", "v1/models"),
            "http://u/v1/chat/completions"
        );
        assert_eq!(
            completion_url("http://u", "models"),
            "http://u/chat/completions"
        );
        assert_eq!(
            completion_url("http://u/", "/v1/models/"),
            "http://u/v1/chat/completions"
        );
    }

    #[tokio::test]
    async fn classify_any_mode_accepts_any_response_serverless() {
        let c = Scripted::new();
        // Serverless: even a 404 is a genuine answer.
        let (healthy, reason) = classify(
            &c,
            "any",
            false,
            &ok(404, &serde_json::json!({})),
            "http://u",
            "v1/models",
            "",
            &[],
            1.0,
        )
        .await;
        assert!(healthy);
        assert_eq!(reason, "HTTP 404");
    }

    #[tokio::test]
    async fn classify_any_mode_filters_edge_synthetics_in_pod_mode() {
        let c = Scripted::new();
        for status in [404, 502, 503, 504] {
            let (healthy, reason) = classify(
                &c,
                "any",
                true,
                &ok(status, &serde_json::json!({})),
                "http://u",
                "v1/models",
                "",
                &[],
                1.0,
            )
            .await;
            assert!(!healthy, "status {status} should be not ready");
            assert_eq!(reason, format!("HTTP {status} (not ready)"));
        }
    }

    #[tokio::test]
    async fn classify_model_mode_lists_model() {
        let c = Scripted::new();
        let body = serde_json::json!({"data": [{"id": "Qwen/Qwen3-8B"}, {"id": "other"}]});
        let (healthy, reason) = classify(
            &c,
            "model",
            false,
            &ok(200, &body),
            "http://u",
            "v1/models",
            "qwen/qwen3-8b",
            &[],
            1.0,
        )
        .await;
        assert!(healthy);
        assert_eq!(reason, "model listed");
    }

    #[tokio::test]
    async fn classify_model_mode_model_missing() {
        let c = Scripted::new();
        let body = serde_json::json!({"data": [{"id": "other"}]});
        let (healthy, reason) = classify(
            &c,
            "model",
            false,
            &ok(200, &body),
            "http://u",
            "v1/models",
            "qwen",
            &[],
            1.0,
        )
        .await;
        assert!(!healthy);
        assert!(reason.contains("not yet in warmup response"));
    }

    #[tokio::test]
    async fn classify_model_mode_not_json() {
        let c = Scripted::new();
        let (healthy, reason) = classify(
            &c,
            "model",
            false,
            &ProbeResponse::raw(200, "not json"),
            "http://u",
            "v1/models",
            "qwen",
            &[],
            1.0,
        )
        .await;
        assert!(!healthy);
        assert_eq!(reason, "warmup response was not JSON");
    }

    #[tokio::test]
    async fn classify_completion_mode_ok() {
        let c = Scripted::new();
        c.push(Ok(ProbeResponse::json(
            200,
            &serde_json::json!({"choices": [{"text": "x"}]}),
        )));
        let (healthy, reason) = classify(
            &c,
            "completion",
            false,
            &ok(200, &serde_json::json!({})),
            "http://u",
            "v1/models",
            "qwen",
            &[],
            1.0,
        )
        .await;
        assert!(healthy);
        assert_eq!(reason, "completion ok");
    }

    #[tokio::test]
    async fn classify_completion_mode_probe_error() {
        let c = Scripted::new();
        c.push(Err(ProbeError("TimeoutError".into())));
        let (healthy, reason) = classify(
            &c,
            "completion",
            false,
            &ok(200, &serde_json::json!({})),
            "http://u",
            "v1/models",
            "qwen",
            &[],
            1.0,
        )
        .await;
        assert!(!healthy);
        assert_eq!(reason, "completion probe failed: TimeoutError");
    }
}
