//! `RunPod` warm proxy (Rust) — entry point.
//!
//! Bootstrap: config -> app -> run. This scaffold serves the liveness probe
//! only; the full pipeline lands in later work items (see plan.md).

mod auth;
mod catalogue;
mod config;
mod gpu_poller;
mod health;
mod keepalive;
mod lifecycle;
mod metrics;
mod prewarm;
mod probe;
mod proxy;
mod request_id;
mod router;
mod runpod_api;
mod state;
mod status;
mod target;
mod warmup;

use std::net::SocketAddr;
use std::sync::Arc;

use config::Config;
use health::ProbeClient;
use keepalive::KeepaliveLoop;
use lifecycle::{
    Lifecycle, discovery::DiscoveryPodLifecycle, pinned::PodLifecycle,
    serverless::ServerlessLifecycle,
};
use probe::ReqwestProbeClient;
use proxy::{CONNECT_TIMEOUT_S, Proxy, proxy_request, proxy_request_root};
use router::ModelRouter;
use runpod_api::RunpodApi;
use state::EndpointState;
use target::UpstreamTarget;
use warmup::WarmupManager;

/// Test-only reqwest client that bypasses any ambient HTTP(S) proxy so the
/// httpmock servers (bound to 127.0.0.1) are reached directly. Production
/// clients keep normal proxy behavior.
#[cfg(test)]
pub(crate) fn test_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .build()
        .expect("test reqwest client builds")
}

/// Serve the proxy until SIGINT/SIGTERM.
#[tokio::main]
#[allow(clippy::too_many_lines)]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();

    // Fail fast on invalid configuration (N7), mirroring the Python lifespan
    // boot checks.
    let config = Config::from_env().map_err(|e| anyhow::anyhow!("{e}"))?;
    config.validate_boot().map_err(anyhow::Error::msg)?;

    let port = config.port;
    if !(1..=65535).contains(&port) {
        anyhow::bail!("PORT must be between 1 and 65535, got {port}");
    }
    let addr = SocketAddr::from((
        [0, 0, 0, 0],
        u16::try_from(port).expect("port range-checked 1..=65535"),
    ));

    tracing::info!(
        mode = %config.mode,
        upstream = %config.upstream_url(),
        warmup = %config.warmup_url(),
        default_model = %config.default_model(),
        discovery = config.discovery_enabled(),
        allowlist = ?config.effective_allowed_models(),
        upstream_auth = !config.upstream_auth_headers().is_empty(),
        "runpod-proxy starting"
    );

    // Build the shared pipeline (WI-07). The keepalive loop and the proxy
    // handler share the same state/target/lifecycle so warmup, keepalive, and
    // forwarding all observe one endpoint.
    let config = Arc::new(config);
    let state = Arc::new(std::sync::Mutex::new(EndpointState::new()));
    let target = Arc::new(std::sync::Mutex::new(UpstreamTarget::new(
        &config.upstream_url(),
        "",
    )));
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(CONNECT_TIMEOUT_S))
        .build()
        .expect("reqwest client builds");
    let probe_client: Arc<dyn ProbeClient> = Arc::new(ReqwestProbeClient::new(client.clone()));
    // Select the lifecycle strategy (mirrors main.py): pod mode with a pinned
    // pod id uses the pinned REST lifecycle; pod mode with no pod id and a
    // default model uses the discovery lifecycle; everything else is serverless
    // (no-op start/stop).
    let lifecycle: Arc<dyn Lifecycle> = if config.mode == "pod" && !config.pod_id.is_empty() {
        Arc::new(PodLifecycle::new(
            Arc::clone(&config),
            client.clone(),
            Arc::clone(&target),
            Arc::clone(&state),
        ))
    } else if config.discovery_enabled() {
        let api = RunpodApi::new(
            &config.rest_api_url,
            &config.availability_api_url,
            &config.api_key,
            client.clone(),
        );
        Arc::new(DiscoveryPodLifecycle::new(
            Arc::clone(&config),
            api,
            Arc::clone(&probe_client),
            Arc::clone(&target),
            Arc::clone(&state),
        ))
    } else {
        Arc::new(ServerlessLifecycle::new())
    };
    let warmup = Arc::new(WarmupManager::new(
        Arc::clone(&config),
        Arc::clone(&probe_client),
        Arc::clone(&state),
        Arc::clone(&lifecycle),
        Arc::clone(&target),
    ));
    let router = Arc::new(ModelRouter::new(Arc::clone(&config)));
    let availability = Arc::new(gpu_poller::GpuAvailability::new(
        Arc::clone(&config),
        Arc::clone(&probe_client),
    ));
    let proxy = Arc::new(Proxy::new(
        Arc::clone(&config),
        Arc::clone(&state),
        Arc::clone(&target),
        Arc::clone(&warmup),
        Arc::clone(&router),
        Arc::clone(&lifecycle),
        Arc::clone(&availability),
        client,
    ));

    // Start the background loops (idempotent; each no-ops when disabled):
    // keepalive probes, scheduled prewarm, and the GPU availability poller.
    let keepalive = KeepaliveLoop::new(
        Arc::clone(&config),
        Arc::clone(&probe_client),
        Arc::clone(&state),
        Arc::clone(&lifecycle),
        Arc::clone(&target),
    );
    keepalive.start().await;
    let prewarm = prewarm::PrewarmScheduler::new(
        config.prewarm_times.clone(),
        Arc::clone(&warmup),
        Arc::clone(&state),
    );
    prewarm.start().await;
    availability.start().await;

    // Routes: liveness probe (public) + control-plane + catch-all proxy. The
    // request-id layer is outermost so every response (including 401s) carries
    // an id; the auth layer sits inside it and guards everything but /_health.
    let app: axum::Router<Arc<Proxy>> = axum::Router::new()
        .route("/_health", axum::routing::get(health_ok))
        .route("/_status", axum::routing::get(status::status))
        .route("/metrics", axum::routing::get(status::metrics))
        .route("/_warm", axum::routing::post(status::warm))
        .route("/_reload", axum::routing::post(status::reload))
        .route("/", axum::routing::any(proxy_request_root))
        .route("/{*path}", axum::routing::any(proxy_request));
    let app = app
        .with_state(Arc::clone(&proxy))
        .layer(axum::middleware::from_fn_with_state(
            Arc::clone(&config),
            auth::auth_middleware,
        ))
        .layer(axum::middleware::from_fn(request_id::request_id_middleware));

    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!(%addr, "runpod-proxy listening");
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // Graceful shutdown (parity with the Python lifespan `finally`): stop the
    // background loops, then best-effort stop the pod-mode backend so a
    // discovered/pinned pod does not keep billing after the container exits.
    prewarm.stop().await;
    keepalive.stop().await;
    availability.stop().await;
    if config.mode == "pod" {
        match lifecycle.stop().await {
            Ok(()) => tracing::info!("shutdown: pod-mode backend stopped"),
            Err(e) => {
                tracing::warn!(%e, "shutdown: could not stop pod-mode backend (pod may keep billing)");
            }
        }
    }
    Ok(())
}

/// Liveness probe: `{"ok":true}` while the process is up.
async fn health_ok() -> axum::Json<serde_json::Value> {
    axum::Json(serde_json::json!({ "ok": true }))
}

/// Resolve when the OS asks us to stop: Ctrl+C on every platform, plus
/// SIGTERM on Unix (what container orchestrators send on shutdown). Without
/// the SIGTERM arm a Linux deployment would be killed instead of draining.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };

    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => {
                tracing::warn!(%e, "could not install SIGTERM handler; Ctrl+C only");
                std::future::pending::<()>().await;
            }
        }
    };

    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
    tracing::info!("shutdown signal received");
}
