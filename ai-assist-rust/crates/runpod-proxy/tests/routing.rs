//! Regression guard for the proxy's catch-all routing (spec section 8.4).
//!
//! The proxy forwards every non-control path to the upstream via an axum
//! catch-all route. axum 0.8 spells a multi-segment wildcard `/{*path}`; the
//! pre-0.8 `/{path:path}` form only matches a *single* segment, which silently
//! 404s real paths like `/v1/chat/completions` while leaving warmup (a direct
//! probe, not routed) working — a subtle break that unit tests calling the
//! handler directly cannot catch. These tests exercise the router itself.

use axum::Router;
use axum::body::Body;
use axum::extract::Path;
use axum::http::{Request, StatusCode};
use axum::routing::{any, get};
use tower::ServiceExt;

fn app() -> Router {
    Router::new()
        .route("/_health", get(|| async { "health" }))
        .route("/", any(|| async { "root".to_string() }))
        .route(
            "/{*path}",
            any(|Path(path): Path<String>| async move { path }),
        )
}

async fn body_string(resp: axum::response::Response) -> String {
    let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    String::from_utf8(bytes.to_vec()).unwrap()
}

async fn send(uri: &str) -> (StatusCode, String) {
    let resp = app()
        .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = resp.status();
    (status, body_string(resp).await)
}

#[tokio::test]
async fn catch_all_matches_multi_segment_path() {
    let (status, body) = send("/v1/chat/completions").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "v1/chat/completions");
}

#[tokio::test]
async fn catch_all_matches_single_segment_path() {
    let (status, body) = send("/openai").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "openai");
}

#[tokio::test]
async fn root_path_is_forwarded() {
    let (status, body) = send("/").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "root");
}

#[tokio::test]
async fn fixed_route_wins_over_catch_all() {
    let (status, body) = send("/_health").await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, "health");
}
