//! Proxy-key gate: constant-time comparison (spec section 8.2). WI-07.
//!
//! When `PROXY_API_KEY` is set, every route except `/_health` is gated. The
//! presented key is the `x-proxy-key` header, else the token after `Bearer` in
//! `Authorization` (case-insensitive prefix, trimmed). Comparison is
//! constant-time (timing-safe). Missing/mismatch -> `401 {"error":"unauthorized"}`.
//! When no proxy key is configured, all routes are open (documented risk: the
//! proxy is an open relay for the `RunPod` credit).

use std::sync::Arc;

use axum::Json;
use axum::extract::{Request, State};
use axum::http::{HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::Config;

/// The header carrying the proxy credential.
pub const PROXY_KEY_HEADER: &str = "x-proxy-key";

/// Paths that stay public even when a proxy key is configured (liveness only).
pub const PUBLIC_PATHS: &[&str] = &["/_health"];

/// Return `(key, source)` for the proxy credential the client presented.
/// `source` is `"x-proxy-key"`, `"authorization"`, or `""` when none.
pub fn presented_proxy_key(headers: &HeaderMap) -> (String, String) {
    let key = headers
        .get(PROXY_KEY_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    if !key.is_empty() {
        return (key, PROXY_KEY_HEADER.to_string());
    }
    let authorization = headers
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .unwrap_or_default()
        .to_string();
    let lower = authorization.to_ascii_lowercase();
    if let Some(token) = lower.strip_prefix("bearer ") {
        let token = token.trim().to_string();
        if !token.is_empty() {
            return (token, "authorization".to_string());
        }
    }
    (String::new(), String::new())
}

/// Constant-time byte comparison (timing-safe, parity with `hmac.compare_digest`).
///
/// A length mismatch returns `false` immediately, matching Python's
/// `hmac.compare_digest` (which also short-circuits on length).
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// True when the presented key matches the configured proxy key.
pub fn check_proxy_key(presented: &str, configured: &str) -> bool {
    !presented.is_empty() && constant_time_eq(presented.as_bytes(), configured.as_bytes())
}

/// How the client presented its proxy key, so the pipeline knows whether to
/// strip `Authorization` before forwarding (a proxy key sent via `Authorization`
/// must not leak to the model server as its API key).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProxyKeySource {
    /// Presented via the dedicated `x-proxy-key` header.
    Header,
    /// Presented via `Authorization: Bearer <key>`.
    Authorization,
}

impl ProxyKeySource {
    fn from_source(source: &str) -> Self {
        if source == "authorization" {
            Self::Authorization
        } else {
            Self::Header
        }
    }
}

/// The 401 body for an unauthorized request.
#[derive(Debug)]
pub struct Unauthorized;

impl IntoResponse for Unauthorized {
    fn into_response(self) -> Response {
        (
            StatusCode::UNAUTHORIZED,
            Json(serde_json::json!({ "error": "unauthorized" })),
        )
            .into_response()
    }
}

/// Gate every route behind `PROXY_API_KEY` when one is configured.
///
/// `/_health` and (when no key is set) all routes pass through. On success the
/// key's source is recorded in the request extensions for the pipeline.
pub async fn auth_middleware(
    State(config): State<Arc<Config>>,
    mut req: Request,
    next: Next,
) -> Result<Response, Unauthorized> {
    let path = req.uri().path();
    if config.proxy_api_key.is_empty() || PUBLIC_PATHS.contains(&path) {
        return Ok(next.run(req).await);
    }
    let (presented, source) = presented_proxy_key(req.headers());
    if !check_proxy_key(&presented, &config.proxy_api_key) {
        return Err(Unauthorized);
    }
    req.extensions_mut()
        .insert(ProxyKeySource::from_source(&source));
    Ok(next.run(req).await)
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::header::HeaderMap;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut m = HeaderMap::new();
        for (k, v) in pairs {
            m.insert(
                axum::http::HeaderName::from_bytes(k.as_bytes()).unwrap(),
                v.parse().unwrap(),
            );
        }
        m
    }

    #[test]
    fn presented_key_from_dedicated_header() {
        let (key, source) = presented_proxy_key(&headers(&[(PROXY_KEY_HEADER, "abc")]));
        assert_eq!(key, "abc");
        assert_eq!(source, "x-proxy-key");
    }

    #[test]
    fn presented_key_from_bearer_token() {
        let (key, source) =
            presented_proxy_key(&headers(&[("authorization", "Bearer   tok123  ")]));
        assert_eq!(key, "tok123");
        assert_eq!(source, "authorization");
    }

    #[test]
    fn bearer_prefix_is_case_insensitive() {
        let (key, source) = presented_proxy_key(&headers(&[("authorization", "bEaReR tok")]));
        assert_eq!(key, "tok");
        assert_eq!(source, "authorization");
    }

    #[test]
    fn dedicated_header_wins_over_bearer() {
        let (key, source) = presented_proxy_key(&headers(&[
            (PROXY_KEY_HEADER, "hdr"),
            ("authorization", "Bearer tok"),
        ]));
        assert_eq!(key, "hdr");
        assert_eq!(source, "x-proxy-key");
    }

    #[test]
    fn no_key_presented() {
        let (key, source) = presented_proxy_key(&headers(&[("authorization", "Basic abc")]));
        assert_eq!(key, "");
        assert_eq!(source, "");
    }

    #[test]
    fn constant_time_eq_matches() {
        assert!(constant_time_eq(b"secret", b"secret"));
        assert!(!constant_time_eq(b"secret", b"secrex"));
        assert!(!constant_time_eq(b"secret", b"short"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn check_proxy_key_requires_nonempty_match() {
        assert!(check_proxy_key("abc", "abc"));
        assert!(!check_proxy_key("", "abc"));
        assert!(!check_proxy_key("abc", ""));
        assert!(!check_proxy_key("abc", "abd"));
    }

    /// End-to-end: the middleware gates non-public routes behind the proxy key
    /// (401 without/wrong key, 200 with the key via header or bearer) and keeps
    /// `/_health` public.
    #[tokio::test]
    async fn auth_middleware_gates_routes() {
        use axum::Router;
        use axum::routing::get;
        use tower::ServiceExt;

        let mut env = std::collections::BTreeMap::new();
        env.insert("PROXY_API_KEY".to_string(), "secret".to_string());
        let config = Arc::new(Config::from_env_map(&env).unwrap());

        let app = Router::new()
            .route("/_health", get(|| async { "healthy" }))
            .route("/v1/models", get(|| async { "models" }))
            .layer(axum::middleware::from_fn_with_state(
                Arc::clone(&config),
                auth_middleware,
            ));

        let req = |uri: &str, headers: &[(&str, &str)]| {
            let mut builder = axum::http::Request::builder().uri(uri);
            for (k, v) in headers {
                builder = builder.header(*k, *v);
            }
            builder.body(axum::body::Body::empty()).unwrap()
        };

        // 401 without a key.
        let resp = app.clone().oneshot(req("/v1/models", &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 401 with a wrong key.
        let resp = app
            .clone()
            .oneshot(req("/v1/models", &[("x-proxy-key", "wrong")]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);

        // 200 with the correct key via the dedicated header.
        let resp = app
            .clone()
            .oneshot(req("/v1/models", &[(PROXY_KEY_HEADER, "secret")]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // 200 with the correct key via a bearer token.
        let resp = app
            .clone()
            .oneshot(req("/v1/models", &[("authorization", "Bearer secret")]))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);

        // /_health stays public (200 without a key).
        let resp = app.oneshot(req("/_health", &[])).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }
}
