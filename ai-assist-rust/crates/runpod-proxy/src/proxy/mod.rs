//! Request pipeline in normative order (spec section 8.4). WI-07.
//!
//! The `Proxy` owns the shared state and runs the 11-step pipeline for every
//! proxied request. The router lease and the N9 semaphore permit are held for
//! the duration of the response stream (released exactly once on stream end or
//! client disconnect, whichever drops the body first).

pub mod body;
pub mod headers;

use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, Request, State};
use axum::http::{HeaderMap, Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use futures_util::{Stream, StreamExt, TryStreamExt};
use serde_json::{Value, json};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

use crate::auth::ProxyKeySource;
use crate::config::Config;
use crate::health::POD_NOT_READY_STATUSES;
use crate::router::{ModelRouter, RequestLease};
use crate::state::{EndpointState, State as EndpointStateEnum, now_secs};
use crate::target::UpstreamTarget;
use crate::warmup::WarmupManager;

use self::body::{BodyRead, read_body};
use self::headers::{build_forward_headers, filter_response_headers, merge_pod_auth};
use crate::lifecycle::Lifecycle;
use crate::warmup::WarmupError;

/// The fixed connect timeout for upstream requests (spec section 8.4 step 9).
/// Set on the shared `reqwest::Client` (a `ClientBuilder`-level setting).
pub(crate) const CONNECT_TIMEOUT_S: u64 = 10;

/// The proxy: shared state + the request pipeline.
pub struct Proxy {
    pub(crate) config: Arc<Config>,
    pub(crate) state: Arc<std::sync::Mutex<EndpointState>>,
    pub(crate) target: Arc<std::sync::Mutex<UpstreamTarget>>,
    pub(crate) warmup: Arc<WarmupManager>,
    pub(crate) router: Arc<ModelRouter>,
    pub(crate) lifecycle: Arc<dyn Lifecycle>,
    pub(crate) availability: Arc<crate::gpu_poller::GpuAvailability>,
    client: reqwest::Client,
    semaphore: Arc<Semaphore>,
    /// Serializes model switches (spec section 6.3): one switch at a time, and
    /// a request never starts against a pod that is about to be swapped.
    switch_lock: tokio::sync::Mutex<()>,
}

impl Proxy {
    /// Build the proxy over the shared state/target/warmup/router.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        config: Arc<Config>,
        state: Arc<std::sync::Mutex<EndpointState>>,
        target: Arc<std::sync::Mutex<UpstreamTarget>>,
        warmup: Arc<WarmupManager>,
        router: Arc<ModelRouter>,
        lifecycle: Arc<dyn Lifecycle>,
        availability: Arc<crate::gpu_poller::GpuAvailability>,
        client: reqwest::Client,
    ) -> Self {
        let permits = usize::try_from(config.proxy_max_concurrent_requests.max(1)).unwrap_or(1);
        let semaphore = Arc::new(Semaphore::new(permits));
        Self {
            config,
            state,
            target,
            warmup,
            router,
            lifecycle,
            availability,
            client,
            semaphore,
            switch_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// Acquire a request lease, switching the active model first when needed
    /// (spec section 6.3). The switch (drain + stop + re-warm) runs under the
    /// switch lock; the lease is taken under the lock too so no request starts
    /// against a pod that is about to be swapped. Returns the warmup error
    /// when a switch's warmup fails (the caller maps it to 503).
    async fn acquire_lease(&self, model: &str) -> Result<RequestLease, WarmupError> {
        let _guard = self.switch_lock.lock().await;
        if self.lifecycle.supports_switch() && model != self.router.active_model() {
            self.switch(model).await?;
        }
        Ok(self.router.lease())
    }

    /// Switch the active model: best-effort drain, stop the old pod, reset the
    /// target, then discover/warm the new model (spec section 6.3). A failed
    /// stop is swallowed here (the discovery lifecycle records it for keepalive
    /// retries); a failed warmup propagates to the caller.
    async fn switch(&self, model: &str) -> Result<(), WarmupError> {
        if self.router.in_flight() > 0 {
            let drain = Duration::from_secs_f64(self.config.model_switch_drain_s);
            let mut idle = self.router.subscribe_idle();
            let _ = tokio::time::timeout(drain, async {
                while !*idle.borrow_and_update() {
                    if idle.changed().await.is_err() {
                        break;
                    }
                }
            })
            .await;
        }
        // A failed stop is recorded by the discovery lifecycle in its pending
        // set (retried by keepalive); do not block the switch on it.
        let _ = self.lifecycle.stop().await;
        self.target
            .lock()
            .unwrap()
            .set(&self.config.upstream_url(), "");
        {
            let mut state = self.state.lock().unwrap();
            state.state = EndpointStateEnum::Cold;
            state.model_switches += 1;
        }
        self.lifecycle.set_active_model(model);
        self.router.set_active_model(model);
        self.warmup.ensure_warm(false).await?;
        Ok(())
    }

    /// Mark a WARM pod-mode endpoint DEGRADED after a real failure.
    ///
    /// A dead or not-yet-serving pod can answer through `RunPod`'s edge proxy
    /// (synthetic 502/504) or drop the connection. Without this the next
    /// request would trust the WARM state and hit the same broken endpoint.
    fn note_upstream_degradation(&self, reason: &str) {
        if self.config.mode != "pod" {
            return;
        }
        let mut state = self.state.lock().unwrap();
        if state.state == EndpointStateEnum::Warm {
            state.state = EndpointStateEnum::Degraded;
            tracing::warn!("upstream degradation ({reason}); state -> DEGRADED");
        }
    }

    /// Steps 4-6: extract, resolve, and canonicalize the model. Returns the
    /// canonical model and the (possibly rewritten) body, or the rejected
    /// model name when it is not allowed (the caller builds the 400).
    fn resolve_model(
        &self,
        headers: &HeaderMap,
        body_bytes: &[u8],
    ) -> Result<(String, Vec<u8>), String> {
        let parsed_body = parse_body(headers, body_bytes);
        let (model, rejected) = self.router.resolve(parsed_body.requested.as_deref());
        if let Some(rejected) = rejected {
            return Err(rejected);
        }
        let model = model.expect("resolve returns a model when not rejected");
        let body_bytes = canonicalize_body(
            body_bytes,
            parsed_body.parsed.as_ref(),
            parsed_body.requested.as_deref(),
            &model,
            self.config.allowlist_configured(),
        );
        Ok((model, body_bytes))
    }

    /// Step 8: revalidate a stale WARM endpoint, then join any in-flight
    /// warmup (forwarding into a WARMING endpoint would 502). Returns the
    /// warmup error as a string when warmup failed.
    async fn ensure_warm_endpoint(&self, was_warm: bool) -> Option<String> {
        let err = if self.config.discovery_enabled()
            && was_warm
            && self.state.lock().unwrap().last_success_at.is_some()
            && self.state.lock().unwrap().last_success_at.unwrap()
                < now_secs() - self.config.pod_revalidate_s
        {
            self.warmup.ensure_warm(true).await.err()
        } else {
            let needs_warm = self.state.lock().unwrap().state != EndpointStateEnum::Warm;
            if needs_warm {
                self.warmup.ensure_warm(false).await.err()
            } else {
                None
            }
        };
        err.map(|e| e.to_string())
    }

    /// Steps 9-11: forward to the upstream, record success, and relay the
    /// response. The lease and permit are held for the duration of the stream.
    async fn forward_and_relay(
        &self,
        method: Method,
        full_url: String,
        headers: HeaderMap,
        body_bytes: Vec<u8>,
        model: &str,
        guards: StreamGuards,
    ) -> Response {
        let started = Instant::now();
        let upstream = match self
            .client
            .request(method, full_url)
            .timeout(Duration::from_secs_f64(self.config.request_timeout_s))
            .headers(headers)
            .body(body_bytes)
            .send()
            .await
        {
            Ok(resp) => resp,
            Err(err) => {
                self.state.lock().unwrap().requests_failed += 1;
                self.note_upstream_degradation(&err.to_string());
                return json_error(
                    StatusCode::BAD_GATEWAY,
                    json!({
                        "error": "upstream connection error",
                        "detail": "reqwest::Error"
                    }),
                );
            }
        };

        let status_code = upstream.status().as_u16();

        // `RunPod`'s edge answers synthetically (404/502/503/504) while a pod
        // is booting or has died: treat that as degradation, not success.
        if self.config.mode == "pod" && POD_NOT_READY_STATUSES.contains(&status_code) {
            self.note_upstream_degradation(&format!("HTTP {status_code} from pod edge"));
        }

        // 10. Record success (on response received, before streaming).
        {
            let mut state = self.state.lock().unwrap();
            state.last_success_at = Some(now_secs());
            state.observe_request(model, started.elapsed().as_secs_f64());
        }

        // 11. Relay: upstream status + filtered headers, streamed as it arrives.
        let resp_headers = filter_response_headers(upstream.headers());
        let inner = upstream.bytes_stream().map_err(axum::Error::new).boxed();
        let body = LeasedStream { inner, guards };
        let mut response = Response::new(Body::from_stream(body));
        *response.status_mut() =
            StatusCode::from_u16(status_code).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        *response.headers_mut() = resp_headers;
        response
    }
}

/// The router lease and the N9 semaphore permit, held for the duration of a
/// response stream. Both are released exactly once when the stream ends or
/// the client disconnects (whichever drops this first). The fields are never
/// read; they exist only for their Drop side effects.
#[allow(dead_code)]
struct StreamGuards {
    lease: RequestLease,
    permit: OwnedSemaphorePermit,
}

/// A response body that holds the request lease and the N9 semaphore permit
/// for the duration of the stream. Both are released exactly once when the
/// stream ends or the client disconnects (whichever drops this first).
struct LeasedStream {
    inner: Pin<Box<dyn Stream<Item = Result<Bytes, axum::Error>> + Send>>,
    // Held only for their Drop side effects (release the lease/permit when the
    // stream ends or the client disconnects); never read.
    #[allow(dead_code)]
    guards: StreamGuards,
}

impl Stream for LeasedStream {
    type Item = Result<Bytes, axum::Error>;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let this = self.get_mut();
        this.inner.as_mut().poll_next(cx)
    }
}

/// Build a JSON error response.
pub(crate) fn json_error(status: StatusCode, body: Value) -> Response {
    (status, Json(body)).into_response()
}

/// Build a JSON error response with a `Retry-After: 1` header (N9 saturation).
fn json_error_retry_after(status: StatusCode, body: Value) -> Response {
    (status, [(header::RETRY_AFTER, "1")], Json(body)).into_response()
}

/// Parse the request body for model extraction (spec section 8.4 step 4).
///
/// Bounded by design (N2): only JSON, only <= 2 MiB. Returns the requested
/// model (if the body is a JSON object with a string `"model"`) and the parsed
/// value (for canonicalization).
struct ParsedBody {
    requested: Option<String>,
    parsed: Option<Value>,
}

fn parse_body(headers: &HeaderMap, body: &[u8]) -> ParsedBody {
    if body.is_empty() {
        return ParsedBody {
            requested: None,
            parsed: None,
        };
    }
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or_default();
    if !content_type.to_lowercase().contains("json") {
        return ParsedBody {
            requested: None,
            parsed: None,
        };
    }
    if body.len() > 2 * 1024 * 1024 {
        return ParsedBody {
            requested: None,
            parsed: None,
        };
    }
    let parsed: Value = match serde_json::from_slice(body) {
        Ok(v) => v,
        Err(_) => {
            return ParsedBody {
                requested: None,
                parsed: None,
            };
        }
    };
    let requested = parsed
        .get("model")
        .and_then(|m| m.as_str())
        .map(str::to_string);
    ParsedBody {
        requested,
        parsed: Some(parsed),
    }
}

/// Rewrite the body's model field to the canonical model (spec section 8.4
/// step 6). Returns the original body when no rewrite is needed.
fn canonicalize_body(
    body: &[u8],
    parsed: Option<&Value>,
    requested: Option<&str>,
    model: &str,
    allowlist_configured: bool,
) -> Vec<u8> {
    if allowlist_configured && requested.is_some_and(|r| model != r) {
        if let Some(parsed) = parsed {
            if let Some(obj) = parsed.as_object() {
                let mut obj = obj.clone();
                obj.insert("model".to_string(), Value::String(model.to_string()));
                if let Ok(s) = serde_json::to_string(&Value::Object(obj)) {
                    return s.into_bytes();
                }
            }
        }
    }
    body.to_vec()
}

/// Catch-all proxy route (`/{*path}`): forwards any multi-segment path.
pub async fn proxy_request(
    State(proxy): State<Arc<Proxy>>,
    Path(path): Path<String>,
    req: Request,
) -> Response {
    run_pipeline(proxy, path, req).await
}

/// Root proxy route (`/`): parity with Starlette's `{path:path}`, which also
/// matches the bare root. Forwards with an empty upstream path.
pub async fn proxy_request_root(State(proxy): State<Arc<Proxy>>, req: Request) -> Response {
    run_pipeline(proxy, String::new(), req).await
}

/// The 11-step proxy pipeline (spec section 8.4, normative order).
async fn run_pipeline(proxy: Arc<Proxy>, path: String, req: Request) -> Response {
    let (parts, body) = req.into_parts();
    let method = parts.method.clone();
    let query = parts.uri.query().map(str::to_string);
    let key_source = parts.extensions.get::<ProxyKeySource>().copied();

    // 1. Record real traffic; classify warm vs cold hit.
    let was_warm = {
        let mut state = proxy.state.lock().unwrap();
        state.last_real_traffic_at = Some(now_secs());
        state.requests_total += 1;
        let warm = state.state == EndpointStateEnum::Warm;
        if warm {
            state.requests_warm_hit += 1;
        }
        warm
    };

    // 2. Build the forwarded headers (filter hop-by-hop, force identity, strip
    //    the proxy credential). The upstream auth headers are merged after
    //    warmup (step 8), when target.pod_id is resolved.
    let mut headers = build_forward_headers(&parts.headers, key_source);

    // 3. Read the body streaming with the cap.
    let body_bytes = match read_body(body, proxy.config.max_body_bytes).await {
        BodyRead::Ok(bytes) => bytes,
        BodyRead::TooLarge => {
            proxy.state.lock().unwrap().requests_failed += 1;
            return json_error(
                StatusCode::PAYLOAD_TOO_LARGE,
                json!({
                    "error": "request body too large",
                    "max_bytes": proxy.config.max_body_bytes
                }),
            );
        }
    };

    // 4-6. Extract, resolve, and canonicalize the model (bounded: JSON,
    //      <= 2 MiB). A rejected model is a 400.
    let (model, body_bytes) = match proxy.resolve_model(&parts.headers, &body_bytes) {
        Ok(v) => v,
        Err(rejected) => {
            proxy.state.lock().unwrap().requests_failed += 1;
            return json_error(
                StatusCode::BAD_REQUEST,
                json!({
                    "error": "model not allowed",
                    "model": rejected,
                    "allowed": proxy.config.effective_allowed_models()
                }),
            );
        }
    };

    // 7. Acquire the N9 concurrency permit (after 413/400, before any switch
    //    or warmup work), then the router lease — switching the active model
    //    first when the request targets a different one (discovery mode).
    let Ok(permit) = Arc::clone(&proxy.semaphore).try_acquire_owned() else {
        return json_error_retry_after(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": "too many concurrent requests" }),
        );
    };

    let lease = match proxy.acquire_lease(&model).await {
        Ok(lease) => lease,
        Err(err) => {
            proxy.state.lock().unwrap().requests_failed += 1;
            let state = proxy.state.lock().unwrap().state.as_str().to_string();
            return json_error(
                StatusCode::SERVICE_UNAVAILABLE,
                json!({ "error": err.to_string(), "state": state }),
            );
        }
    };

    // 8. Warmup: revalidate a stale WARM endpoint, then join any in-flight
    //    warmup (forwarding into a WARMING endpoint would 502).
    if let Some(err) = proxy.ensure_warm_endpoint(was_warm).await {
        proxy.state.lock().unwrap().requests_failed += 1;
        let state = proxy.state.lock().unwrap().state.as_str().to_string();
        return json_error(
            StatusCode::SERVICE_UNAVAILABLE,
            json!({ "error": err, "state": state }),
        );
    }

    // 9-11. Forward, record success, and relay. The upstream auth headers are
    //       merged now, after warmup resolved target.pod_id (a cold start's
    //       first request would otherwise use the static fallback key).
    let target = proxy.target.lock().unwrap().clone();
    let pod_auth = proxy.config.auth_headers_for_pod(target.pod_id());
    merge_pod_auth(&mut headers, &pod_auth);

    let target_url = if path.is_empty() {
        target.url().to_string()
    } else {
        format!("{}/{}", target.url(), path.trim_start_matches('/'))
    };
    let full_url = match query {
        Some(q) => format!("{target_url}?{q}"),
        None => target_url,
    };

    let guards = StreamGuards { lease, permit };
    proxy
        .forward_and_relay(method, full_url, headers, body_bytes, &model, guards)
        .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::health::{ProbeClient, ProbeError, ProbeResponse};
    use crate::lifecycle::serverless::ServerlessLifecycle;
    use std::collections::BTreeMap;

    /// A probe client that always reports the endpoint ready (so warmup
    /// succeeds immediately in tests).
    struct ReadyProbe;

    #[async_trait::async_trait]
    impl ProbeClient for ReadyProbe {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &json!({ "data": [] })))
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &json!({ "id": "ok" })))
        }
    }

    fn config(env: &[(&str, &str)]) -> Arc<Config> {
        let map: BTreeMap<String, String> = env
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        Arc::new(Config::from_env_map(&map).unwrap())
    }

    /// Build a `Proxy` whose upstream is `upstream_url` (point it at an
    /// httpmock server). The endpoint starts COLD; the first request warms it.
    fn make_proxy(config: Arc<Config>, upstream_url: &str) -> Arc<Proxy> {
        let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
        let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(upstream_url, "")));
        let client = Arc::new(ReadyProbe);
        let lifecycle: Arc<dyn crate::lifecycle::Lifecycle> = Arc::new(ServerlessLifecycle::new());
        let warmup = Arc::new(WarmupManager::new(
            Arc::clone(&config),
            client,
            Arc::clone(&state),
            Arc::clone(&lifecycle),
            Arc::clone(&target),
        ));
        let router = Arc::new(ModelRouter::new(Arc::clone(&config)));
        let availability = Arc::new(crate::gpu_poller::GpuAvailability::new(
            Arc::clone(&config),
            Arc::new(ReadyProbe),
        ));
        let http = crate::test_client();
        Arc::new(Proxy::new(
            config,
            state,
            target,
            warmup,
            router,
            lifecycle,
            availability,
            http,
        ))
    }

    fn build_request(method: &str, path: &str, headers: &[(&str, &str)], body: &[u8]) -> Request {
        let mut req = Request::new(Body::from(body.to_vec()));
        *req.method_mut() = method.parse().unwrap();
        *req.uri_mut() = path.parse().unwrap();
        for (k, v) in headers {
            req.headers_mut().insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        req
    }

    async fn read_response(response: Response) -> (StatusCode, HeaderMap, Vec<u8>) {
        let status = response.status();
        let headers = response.headers().clone();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, headers, body.to_vec())
    }

    #[tokio::test]
    async fn forwards_method_path_query_body() {
        let mock_server = httpmock::MockServer::start();
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions")
                .body(r#"{"model":"qwen","messages":[]}"#);
            then.status(200)
                .header("content-type", "application/json")
                .body(r#"{"upstream":true}"#);
        });

        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request(
            "POST",
            "/v1/chat/completions?model=qwen&stream=true",
            &[],
            br#"{"model":"qwen","messages":[]}"#,
        );
        let (status, _headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, br#"{"upstream":true}"#);
        mock.assert();
    }

    #[tokio::test]
    async fn strips_hop_by_hop_and_proxy_key() {
        let mock_server = httpmock::MockServer::start();
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::GET)
                .path("/v1/models")
                .header("x-api-key", "client-key")
                .header("accept-encoding", "identity");
            then.status(200).body("ok");
        });

        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request(
            "GET",
            "/v1/models",
            &[
                ("x-api-key", "client-key"),
                ("connection", "close"),
                ("x-proxy-key", "secret"),
            ],
            b"",
        );
        let (status, _headers, _body) =
            read_response(proxy_request(State(proxy), Path("v1/models".to_string()), req).await)
                .await;
        assert_eq!(status, StatusCode::OK);
        mock.assert();
    }

    #[tokio::test]
    async fn rejects_body_over_cap() {
        let mock_server = httpmock::MockServer::start();
        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("MAX_BODY_BYTES", "10"),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request("POST", "/v1/chat/completions", &[], b"0123456789ABCDEF");
        let (status, _headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "request body too large");
        assert_eq!(v["max_bytes"], 10);
    }

    #[tokio::test]
    async fn rejects_model_not_allowed() {
        let mock_server = httpmock::MockServer::start();
        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("RUNPOD_ALLOWED_MODELS", "qwen,llama"),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"gpt4"}"#,
        );
        let (status, _headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "model not allowed");
        assert_eq!(v["model"], "gpt4");
    }

    #[tokio::test]
    async fn connect_error_returns_502() {
        // Point the upstream at a closed port so the connect fails.
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", "http://127.0.0.1:1"),
            ("REQUEST_TIMEOUT_S", "2"),
        ]);
        let proxy = make_proxy(config, "http://127.0.0.1:1");

        let req = build_request("POST", "/v1/chat/completions", &[], b"{}");
        let (status, _headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_GATEWAY);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "upstream connection error");
    }

    #[tokio::test]
    async fn sse_passthrough_is_byte_exact() {
        let mock_server = httpmock::MockServer::start();
        let sse = "data: one\n\ndata: two\n\ndata: [DONE]\n\n";
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions");
            then.status(200)
                .header("content-type", "text/event-stream")
                .body(sse);
        });

        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"qwen","stream":true}"#,
        );
        let (status, headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            headers.get("content-type").and_then(|v| v.to_str().ok()),
            Some("text/event-stream")
        );
        assert_eq!(body, sse.as_bytes().to_vec());
        mock.assert();
    }

    #[tokio::test]
    async fn semaphore_saturation_returns_503_with_retry_after() {
        let mock_server = httpmock::MockServer::start();
        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("PROXY_MAX_CONCURRENT_REQUESTS", "1"),
        ]);
        let proxy = make_proxy(config, &url);

        // Hold the single permit, then a second request must be rejected.
        let _held = Arc::clone(&proxy.semaphore).try_acquire_owned().unwrap();
        let req = build_request("POST", "/v1/chat/completions", &[], b"{}");
        let (status, headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(
            headers
                .get(header::RETRY_AFTER)
                .and_then(|v| v.to_str().ok()),
            Some("1")
        );
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"], "too many concurrent requests");
    }

    #[tokio::test]
    async fn canonicalizes_model_variant_in_body() {
        let mock_server = httpmock::MockServer::start();
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions")
                .body(r#"{"model":"Qwen/Qwen3.8-27B","messages":[]}"#);
            then.status(200).body("ok");
        });

        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("RUNPOD_ALLOWED_MODELS", "Qwen/Qwen3.8-27B"),
        ]);
        let proxy = make_proxy(config, &url);

        // A slug variant of the allowed model is rewritten to the canonical name.
        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"qwen-qwen3-8-27b","messages":[]}"#,
        );
        let (status, _headers, _body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        mock.assert();
    }

    /// A rejected model is a 400 and must never reach the upstream backend.
    #[tokio::test]
    async fn disallowed_model_never_reaches_backend() {
        let mock_server = httpmock::MockServer::start();
        let url = mock_server.url("/");
        // A mock that would match the rejected model's request; it must never
        // be hit.
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions");
            then.status(200).body("should not be called");
        });
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("RUNPOD_ALLOWED_MODELS", "qwen,llama"),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"gpt4"}"#,
        );
        let (status, _headers, _body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        // The rejected model must not reach the upstream backend.
        mock.assert_hits(0);
    }

    /// A warmup that never becomes ready (model health mode, no model listed)
    /// runs to the timeout and the request gets a 503 carrying the state.
    #[tokio::test]
    async fn warmup_timeout_returns_503_with_state() {
        let mock_server = httpmock::MockServer::start();
        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "model"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("WARMUP_TIMEOUT_S", "0.5"),
        ]);
        let proxy = make_proxy(config, &url);

        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"qwen"}"#,
        );
        let (status, _headers, body) = read_response(
            proxy_request(State(proxy), Path("v1/chat/completions".to_string()), req).await,
        )
        .await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        let v: Value = serde_json::from_slice(&body).unwrap();
        assert!(v["error"].as_str().unwrap().contains("timeout"));
        assert_eq!(v["state"], "COLD");
    }

    /// A WARM pod-mode endpoint whose last success is older than
    /// `POD_REVALIDATE_S` forces a revalidation warmup on the next request.
    #[tokio::test]
    async fn stale_warm_endpoint_is_revalidated() {
        let mock_server = httpmock::MockServer::start();
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions");
            then.status(200).body("ok");
        });
        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODE", "pod"),
            ("RUNPOD_MODEL_NAME", "qwen"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
            ("POD_REVALIDATE_S", "10"),
        ]);
        let proxy = make_proxy(config, &url);

        // Simulate a WARM endpoint whose last success is older than the
        // revalidate window, so the next request must force a revalidation.
        {
            let mut s = proxy.state.lock().unwrap();
            s.state = EndpointStateEnum::Warm;
            s.last_success_at = Some(now_secs() - 100.0);
        }

        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"qwen"}"#,
        );
        let (status, _headers, _body) = read_response(
            proxy_request(
                State(Arc::clone(&proxy)),
                Path("v1/chat/completions".to_string()),
                req,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        // A forced revalidation warmup ran (warmups incremented from 0 to 1).
        assert_eq!(proxy.state.lock().unwrap().warmups, 1);
        mock.assert();
    }

    /// A lifecycle that supports model switching (like the discovery
    /// lifecycle) and counts `stop()` calls, so the switch path is observable.
    struct SwitchableLifecycle {
        stop_calls: Arc<std::sync::atomic::AtomicUsize>,
        active: Arc<std::sync::Mutex<String>>,
    }

    #[async_trait::async_trait]
    impl Lifecycle for SwitchableLifecycle {
        async fn start(&self, _budget: f64) -> Result<(), crate::lifecycle::LifecycleError> {
            Ok(())
        }
        async fn stop(&self) -> Result<(), crate::lifecycle::LifecycleError> {
            self.stop_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
        fn active_model(&self) -> String {
            self.active.lock().unwrap().clone()
        }
        fn supports_switch(&self) -> bool {
            true
        }
        fn set_active_model(&self, model: &str) {
            *self.active.lock().unwrap() = model.to_string();
        }
    }

    /// A request for an allowed model different from the active one triggers a
    /// serialized model switch: the old pod is stopped, the active model
    /// changes on both the lifecycle and the router, the switch counter
    /// increments, and the endpoint re-warms to WARM.
    #[tokio::test]
    async fn model_switch_drains_stops_and_rewarms() {
        let mock_server = httpmock::MockServer::start();
        let mock = mock_server.mock(|when, then| {
            when.method(httpmock::Method::POST)
                .path("/v1/chat/completions");
            then.status(200).body("ok");
        });
        let url = mock_server.url("/");
        let config = config(&[
            ("RUNPOD_MODEL_NAME", "model-a"),
            ("RUNPOD_ALLOWED_MODELS", "model-a,model-b"),
            ("POD_HEALTH_MODE", "any"),
            ("RUNPOD_SERVERLESS_URL", &url),
        ]);

        let stop_calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let active = Arc::new(std::sync::Mutex::new("model-a".to_string()));
        let lifecycle: Arc<dyn Lifecycle> = Arc::new(SwitchableLifecycle {
            stop_calls: Arc::clone(&stop_calls),
            active: Arc::clone(&active),
        });

        let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
        let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(&url, "")));
        let warmup = Arc::new(WarmupManager::new(
            Arc::clone(&config),
            Arc::new(ReadyProbe),
            Arc::clone(&state),
            Arc::clone(&lifecycle),
            Arc::clone(&target),
        ));
        let router = Arc::new(ModelRouter::new(Arc::clone(&config)));
        let availability = Arc::new(crate::gpu_poller::GpuAvailability::new(
            Arc::clone(&config),
            Arc::new(ReadyProbe),
        ));
        let proxy = Arc::new(Proxy::new(
            config,
            state,
            target,
            warmup,
            router,
            lifecycle,
            availability,
            crate::test_client(),
        ));

        // The endpoint is WARM, serving model-a.
        {
            let mut s = proxy.state.lock().unwrap();
            s.state = EndpointStateEnum::Warm;
        }

        // A request for model-b (allowed, but different from the active
        // model-a) triggers a serialized model switch.
        let req = build_request(
            "POST",
            "/v1/chat/completions",
            &[("content-type", "application/json")],
            br#"{"model":"model-b","messages":[]}"#,
        );
        let (status, _headers, _body) = read_response(
            proxy_request(
                State(Arc::clone(&proxy)),
                Path("v1/chat/completions".to_string()),
                req,
            )
            .await,
        )
        .await;
        assert_eq!(status, StatusCode::OK);

        // The switch ran: the old pod was stopped, the active model changed on
        // both the lifecycle and the router, and the switch counter incremented.
        assert_eq!(stop_calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(proxy.router.active_model(), "model-b");
        assert_eq!(*active.lock().unwrap(), "model-b");
        assert_eq!(proxy.state.lock().unwrap().model_switches, 1);
        // The endpoint is WARM again after the re-warm.
        assert_eq!(proxy.state.lock().unwrap().state, EndpointStateEnum::Warm);
        mock.assert();
    }
}
