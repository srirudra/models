//! Control-plane handlers: /_status, /_warm, /_reload (spec section 8.1). WI-11.
//!
//! Ports the admin endpoints from `proxy/main.py`: `/_status` (JSON view of
//! the endpoint state, mode, active model, discovery + GPU availability),
//! `/_warm` (warm the active model without switching), `/_reload` (hot-swap
//! the model catalogue from its configured source). All three sit behind the
//! same auth middleware as the proxy path; only `/_health` is public.

use std::sync::Arc;

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde_json::{Value, json};

use crate::metrics;
use crate::proxy::{Proxy, json_error};
use crate::state::now_secs;

/// `GET /_status`: the JSON status view (spec section 8.1).
pub async fn status(State(proxy): State<Arc<Proxy>>) -> Response {
    let endpoint = proxy.target.lock().unwrap().url().to_string();
    let mut view = proxy.state.lock().unwrap().status_view(&endpoint);
    let active_model = proxy.router.active_model();
    let obj = view
        .as_object_mut()
        .expect("status_view returns a JSON object");
    obj.insert("mode".to_string(), json!(proxy.config.mode));
    obj.insert("active_model".to_string(), json!(active_model));
    if proxy.config.mode == "pod" {
        let pod_id = proxy.target.lock().unwrap().pod_id().to_string();
        obj.insert("pod_id".to_string(), json!(pod_id));
        if proxy.config.discovery_enabled() {
            obj.insert("model".to_string(), json!(active_model));
        }
    }
    if let Some(dv) = proxy.lifecycle.discovery_view() {
        let last_error = if dv.last_error.is_empty() {
            Value::Null
        } else {
            json!(dv.last_error)
        };
        obj.insert("last_discovery_error".to_string(), last_error);
        let breaker = if dv.circuit_breaker_open_s == 0.0 {
            0.0
        } else {
            round1(dv.circuit_breaker_open_s)
        };
        obj.insert("circuit_breaker_open_s".to_string(), json!(breaker));
    }
    if proxy.config.mode == "pod" && proxy.availability.enabled() {
        obj.insert(
            "gpu_availability".to_string(),
            proxy.availability.status_view(),
        );
    }
    Json(view).into_response()
}

/// `GET /metrics`: the Prometheus text exposition (spec section 8.1).
pub async fn metrics(State(proxy): State<Arc<Proxy>>) -> Response {
    let active_model = proxy.router.active_model();
    let body = {
        let state = proxy.state.lock().unwrap();
        metrics::render(&proxy.config, &state, &active_model, &proxy.availability)
    };
    (
        StatusCode::OK,
        [("content-type", "text/plain; version=0.0.4")],
        body,
    )
        .into_response()
}

/// `POST /_warm`: warm the *active* model (no switch) and anchor the idle
/// window (spec section 8.1).
pub async fn warm(State(proxy): State<Arc<Proxy>>) -> Response {
    // Hold a lease for the warmup so a concurrent switch drains behind it.
    let lease = proxy.router.lease();
    let result = proxy.warmup.ensure_warm(false).await;
    drop(lease);

    if let Err(exc) = result {
        let state = proxy.state.lock().unwrap().state.as_str().to_string();
        let message = exc.to_string();
        let error = if message.is_empty() {
            "endpoint warmup failed".to_string()
        } else {
            message
        };
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": error, "state": state }),
        );
    }
    let state = {
        let mut s = proxy.state.lock().unwrap();
        s.last_real_traffic_at = Some(now_secs());
        s.state.as_str().to_string()
    };
    Json(json!({ "state": state })).into_response()
}

/// `POST /_reload`: hot-reload the model catalogue from its configured source
/// (spec section 8.1). A rejected reload leaves the running catalogue intact.
pub async fn reload(State(proxy): State<Arc<Proxy>>) -> Response {
    let catalogue = match proxy.config.reload_catalogue() {
        Ok(catalogue) => catalogue,
        Err(exc) => {
            return json_error(StatusCode::BAD_REQUEST, json!({ "error": exc.to_string() }));
        }
    };
    let names = catalogue.names();
    proxy.config.set_catalogue(catalogue);
    let source = if proxy.config.catalogue_file.is_empty() {
        "inline".to_string()
    } else {
        proxy.config.catalogue_file.clone()
    };
    tracing::info!(models = ?names, "model catalogue reloaded");
    Json(json!({ "reloaded": true, "source": source, "models": names })).into_response()
}

/// Round to 1 decimal (parity with Python `round(x, 1)`).
fn round1(value: f64) -> f64 {
    (value * 10.0).round() / 10.0
}
