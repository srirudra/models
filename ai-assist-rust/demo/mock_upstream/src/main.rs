//! Mock upstream for local demos and load tests.
//!
//! Emulates the OpenAI-compatible surface a `RunPod` model server exposes so
//! the proxy can be exercised end-to-end without a real GPU backend:
//!
//! * `GET  /_health`             — liveness (`ok`).
//! * `GET  /v1/models`           — model list (satisfies the `model` warmup
//!   probe); the id echoes `MOCK_MODEL` (default `qwen`).
//! * `POST /v1/chat/completions` — a chat completion; `"stream": true` returns
//!   a real `text/event-stream` (several deltas + `[DONE]`), otherwise a
//!   single JSON body.
//!
//! Env knobs: `MOCK_PORT` (default 9000), `MOCK_MODEL` (default `qwen`),
//! `MOCK_STREAM_CHUNKS` (default 5), `MOCK_STREAM_DELAY_MS` (default 40).

use std::convert::Infallible;
use std::net::SocketAddr;
use std::time::Duration;

use axum::extract::State;
use axum::http::{StatusCode, header};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_core::Stream;
use serde_json::{Value, json};

#[derive(Clone)]
struct Cfg {
    model: String,
    chunks: usize,
    delay: Duration,
}

#[tokio::main]
async fn main() -> std::io::Result<()> {
    tracing_subscriber::fmt::init();
    let port: u16 = env_or("MOCK_PORT", "9000").parse().unwrap_or(9000);
    let cfg = Cfg {
        model: env_or("MOCK_MODEL", "qwen"),
        chunks: env_or("MOCK_STREAM_CHUNKS", "5").parse().unwrap_or(5),
        delay: Duration::from_millis(env_or("MOCK_STREAM_DELAY_MS", "40").parse().unwrap_or(40)),
    };
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let app = Router::new()
        .route("/_health", get(|| async { "ok" }))
        .route("/v1/models", get(models))
        .route("/v1/chat/completions", post(chat_completions))
        .with_state(cfg);
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "mock-upstream listening");
    axum::serve(listener, app).await?;
    Ok(())
}

fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// `GET /v1/models`: OpenAI-style model list; the warmup `model` probe checks
/// that the configured model id is present here.
async fn models(State(cfg): State<Cfg>) -> Json<Value> {
    Json(json!({
        "object": "list",
        "data": [{ "id": cfg.model, "object": "model", "owned_by": "mock" }],
    }))
}

/// `POST /v1/chat/completions`: a single JSON body, or a streamed
/// `text/event-stream` when the request asks for `"stream": true`.
async fn chat_completions(State(cfg): State<Cfg>, body: axum::body::Bytes) -> Response {
    let req: Value = serde_json::from_slice(&body).unwrap_or_else(|_| json!({}));
    let stream = req.get("stream").and_then(Value::as_bool).unwrap_or(false);
    let model = req
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or(&cfg.model)
        .to_string();

    if !stream {
        return Json(json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion",
            "model": model,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": "pong" },
                "finish_reason": "stop",
            }],
        }))
        .into_response();
    }

    let sse =
        Sse::new(completion_stream(model, cfg.chunks, cfg.delay)).keep_alive(KeepAlive::default());
    (StatusCode::OK, [(header::CACHE_CONTROL, "no-cache")], sse).into_response()
}

/// A minimal OpenAI-compatible streaming completion: a role delta, N content
/// deltas, a finish delta, then the terminal `[DONE]` sentinel.
fn completion_stream(
    model: String,
    chunks: usize,
    delay: Duration,
) -> impl Stream<Item = Result<Event, Infallible>> {
    async_stream::stream! {
        let base = |delta: Value, finish: Value| json!({
            "id": "chatcmpl-mock",
            "object": "chat.completion.chunk",
            "model": model,
            "choices": [{ "index": 0, "delta": delta, "finish_reason": finish }],
        });

        yield Ok(Event::default().data(base(json!({ "role": "assistant" }), Value::Null).to_string()));
        for i in 0..chunks {
            tokio::time::sleep(delay).await;
            let token = if i == 0 { "pong".to_string() } else { format!(" {i}") };
            yield Ok(Event::default().data(base(json!({ "content": token }), Value::Null).to_string()));
        }
        yield Ok(Event::default().data(base(json!({}), json!("stop")).to_string()));
        yield Ok(Event::default().data("[DONE]"));
    }
}
