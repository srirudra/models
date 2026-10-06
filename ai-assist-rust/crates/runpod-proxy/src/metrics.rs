//! Prometheus registry: exact metric names, labels, buckets (spec section 8.1). WI-11.
//!
//! Renders the `/metrics` text exposition (`text/plain; version=0.0.4`), a
//! byte-for-byte port of `proxy/main.py::metrics`. The exposition covers the
//! endpoint state gauge, the lifecycle/traffic counters, the active-model
//! gauge, per-model request counters, the request-duration histogram, and (in
//! pod mode with the poller enabled) the GPU availability gauges.

use serde_json::Value;

use crate::config::Config;
use crate::gpu_poller::{GpuAvailability, availability_level};
use crate::state::{EndpointState, State, py_float_str};

/// Render the `/metrics` exposition body (without the trailing content type).
#[must_use]
pub fn render(
    config: &Config,
    state: &EndpointState,
    active_model: &str,
    availability: &GpuAvailability,
) -> String {
    let mut lines: Vec<String> = vec![
        "# HELP runpod_proxy_state Current endpoint state (1 = active).".to_string(),
        "# TYPE runpod_proxy_state gauge".to_string(),
    ];
    for member in State::ALL {
        let value = i32::from(state.state == member);
        lines.push(format!(
            "runpod_proxy_state{{state=\"{}\"}} {value}",
            member.as_str()
        ));
    }

    // Lifecycle / traffic counters (insertion order from `metrics_view`).
    let values = state.metrics_view();
    if let Some(obj) = values.as_object() {
        for (name, value) in obj {
            let metric = format!("runpod_proxy_{name}");
            lines.push(format!("# TYPE {metric} {}", metric_type(name)));
            lines.push(format!("{metric} {}", number_str(value)));
        }
    }

    // Active-model gauge.
    let active = escape_label(active_model);
    lines.push("# TYPE runpod_proxy_active_model gauge".to_string());
    lines.push(format!("runpod_proxy_active_model{{model=\"{active}\"}} 1"));

    // Forwarded requests per resolved model (sorted by name).
    lines.push(
        "# HELP runpod_proxy_requests_by_model Forwarded requests per resolved model.".to_string(),
    );
    lines.push("# TYPE runpod_proxy_requests_by_model counter".to_string());
    let mut names: Vec<&String> = state.requests_by_model.keys().collect();
    names.sort();
    for name in names {
        let label = escape_label(name);
        let count = state.requests_by_model[name];
        lines.push(format!(
            "runpod_proxy_requests_by_model{{model=\"{label}\"}} {count}"
        ));
    }

    // Request-duration histogram.
    let hist = &state.request_duration;
    lines.push(
        "# HELP runpod_proxy_request_duration_seconds Seconds to upstream response headers \
         (forwarded requests)."
            .to_string(),
    );
    lines.push("# TYPE runpod_proxy_request_duration_seconds histogram".to_string());
    for (le, count) in hist.buckets() {
        lines.push(format!(
            "runpod_proxy_request_duration_seconds_bucket{{le=\"{le}\"}} {count}"
        ));
    }
    lines.push(format!(
        "runpod_proxy_request_duration_seconds_sum {:.6}",
        hist.sum()
    ));
    lines.push(format!(
        "runpod_proxy_request_duration_seconds_count {}",
        hist.count()
    ));

    // GPU availability gauges (pod mode, poller enabled).
    if config.mode == "pod" && availability.enabled() {
        let mv = availability.metrics_view();
        lines.push(
            "# HELP runpod_proxy_gpu_availability_age_s Seconds since the last successful GPU \
             availability refresh; grows while a fetch fails (last-known values are kept)."
                .to_string(),
        );
        lines.push("# TYPE runpod_proxy_gpu_availability_age_s gauge".to_string());
        if let Some(age) = mv.age_s {
            lines.push(format!(
                "runpod_proxy_gpu_availability_age_s {}",
                py_float_str(age)
            ));
        }
        lines.push(
            "# HELP runpod_proxy_gpu_availability_level Last-known RunPod availability per GPU \
             type (HIGH=3, MEDIUM=2, LOW=1, NONE=0, -1=unknown); absent until the first \
             successful refresh."
                .to_string(),
        );
        lines.push("# TYPE runpod_proxy_gpu_availability_level gauge".to_string());
        for (gpu_id, level) in mv.gpus {
            let label = escape_label(&gpu_id);
            lines.push(format!(
                "runpod_proxy_gpu_availability_level{{gpu=\"{label}\"}} {}",
                availability_level(&level)
            ));
        }
    }

    let mut body = lines.join("\n");
    body.push('\n');
    body
}

/// The Prometheus metric type for a `metrics_view` counter name.
fn metric_type(name: &str) -> &'static str {
    if name == "uptime_s" {
        "gauge"
    } else {
        "counter"
    }
}

/// Render a JSON number the way Python `f"{value}"` does: integers plain,
/// floats via `py_float_str` (a trailing `.0` on whole numbers).
fn number_str(value: &Value) -> String {
    if let Some(u) = value.as_u64() {
        u.to_string()
    } else if let Some(i) = value.as_i64() {
        i.to_string()
    } else if let Some(f) = value.as_f64() {
        py_float_str(f)
    } else {
        value.to_string()
    }
}

/// Escape a Prometheus label value (`\` -> `\\`, `"` -> `\"`, newline -> `\n`).
fn escape_label(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use crate::health::{ProbeClient, ProbeError, ProbeResponse};

    struct NullClient;

    #[async_trait::async_trait]
    impl ProbeClient for NullClient {
        async fn get(
            &self,
            _url: &str,
            _headers: &[(String, String)],
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &serde_json::json!({})))
        }
        async fn post_json(
            &self,
            _url: &str,
            _body: &Value,
            _headers: &[(String, String)],
            _timeout: f64,
        ) -> Result<ProbeResponse, ProbeError> {
            Ok(ProbeResponse::json(200, &serde_json::json!({})))
        }
    }

    fn config(pairs: &[(&str, &str)]) -> Arc<Config> {
        let mut env: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in pairs {
            env.insert(k.to_string(), v.to_string());
        }
        Arc::new(Config::from_env_map(&env).unwrap())
    }

    #[test]
    fn renders_state_gauge_and_counters() {
        let cfg = config(&[("RUNPOD_MODEL_NAME", "qwen"), ("POD_HEALTH_MODE", "any")]);
        let availability = GpuAvailability::new(Arc::clone(&cfg), Arc::new(NullClient));
        let mut state = EndpointState::new();
        state.state = State::Warm;
        state.warmups = 2;
        state.requests_total = 5;
        state.requests_warm_hit = 3;
        state.observe_request("qwen", 0.42);

        let out = render(&cfg, &state, "qwen", &availability);

        assert!(out.contains("runpod_proxy_state{state=\"WARM\"} 1"));
        assert!(out.contains("runpod_proxy_state{state=\"COLD\"} 0"));
        assert!(out.contains("# TYPE runpod_proxy_warmups counter"));
        assert!(out.contains("runpod_proxy_warmups 2"));
        assert!(out.contains("runpod_proxy_requests_cold_hit 2"));
        assert!(out.contains("# TYPE runpod_proxy_uptime_s gauge"));
        assert!(out.contains("runpod_proxy_active_model{model=\"qwen\"} 1"));
        assert!(out.contains("runpod_proxy_requests_by_model{model=\"qwen\"} 1"));
        assert!(out.contains("runpod_proxy_request_duration_seconds_count 1"));
        assert!(out.contains("runpod_proxy_request_duration_seconds_sum 0.420000"));
        assert!(out.ends_with('\n'));
        // Serverless: no GPU gauges.
        assert!(!out.contains("runpod_proxy_gpu_availability"));
    }

    #[test]
    fn escapes_label_special_characters() {
        assert_eq!(escape_label(r#"a\b"c"#), r#"a\\b\"c"#);
        assert_eq!(escape_label("a\nb"), "a\\nb");
    }
}
