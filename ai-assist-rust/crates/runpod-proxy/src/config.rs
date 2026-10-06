//! Environment configuration: exact parsing rules, derived values, boot
//! validation (spec section 5).
//!
//! Port of `proxy/config.py`. Parsing rules are a contract (spec section 5):
//! trim, casefold, `int(float(x))` truncation, URL trailing-slash stripping,
//! comma lists, bool spellings. `from_env_map` takes an explicit map so tests
//! are deterministic and parallel-safe; `from_env` reads the process
//! environment.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use arc_swap::ArcSwap;
use thiserror::Error;

use crate::catalogue::{
    ModelCatalogue, ModelConfigError, load_catalogue_ext, validate_reloaded_ext,
};
use crate::prewarm::parse_prewarm_times;

/// An environment variable could not be parsed.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ConfigError(String);

impl From<ModelConfigError> for ConfigError {
    fn from(e: ModelConfigError) -> Self {
        Self(e.0)
    }
}

/// Runtime configuration (spec section 5).
// Fields are the full env surface; individual consumers land in WI-06..WI-11.
#[allow(dead_code)]
#[derive(Debug)]
pub struct Config {
    pub serverless_url: String,
    pub api_key: String,
    pub upstream_api_key: String,
    pub port: i64,
    pub warmup_path: String,
    pub warmup_timeout_s: f64,
    pub warmup_backoff_max_s: f64,
    pub keepalive_interval_s: f64,
    pub idle_giveup_s: f64,
    pub request_timeout_s: f64,
    pub log_level: String,
    pub log_format: String,
    pub mode: String,
    pub pod_id: String,
    pub pod_port: i64,
    pub pod_url: String,
    pub rest_api_url: String,
    pub availability_api_url: String,
    pub gpu_availability_interval_s: f64,
    pub proxy_api_key: String,
    pub model_name: String,
    pub allowed_models: Vec<String>,
    pub model_switch_drain_s: f64,
    pub allow_pod_create: bool,
    pub gpu_type_ids: Vec<String>,
    pub gpu_type_priority: String,
    pub cloud_type: String,
    pub template_name: String,
    pub pod_revalidate_s: f64,
    pub pod_health_timeout_s: f64,
    pub pod_ready_timeout_s: f64,
    pub pod_health_mode: String,
    pub on_migrate: String,
    pub container_disk_gb: Option<i64>,
    pub volume_gb: Option<i64>,
    pub max_create_attempts: i64,
    pub pod_circuit_breaker_threshold: i64,
    pub pod_circuit_breaker_cooldown_s: f64,
    pub max_body_bytes: i64,
    /// N9 (D-07): bound on concurrent in-flight proxied requests. Saturation
    /// returns 503 + `Retry-After: 1` while established streams keep flowing.
    pub proxy_max_concurrent_requests: i64,
    pub prewarm_times: Vec<(u8, u8)>,
    pub upstream_api_key_template: String,
    /// True when a model allowlist is in effect (catalogue, explicit
    /// allowlist, or a single model name). Interior-mutable: `/_reload` may
    /// force it true after a successful catalogue swap.
    pub allowlist_configured: AtomicBool,
    /// The model catalogue, hot-swappable via `/_reload` (spec section 8.1).
    pub catalogue: ArcSwap<ModelCatalogue>,
    pub catalogue_file: String,
    pub catalogue_inline: String,
    /// Parser format for `catalogue_inline`: ".json" (`RUNPOD_MODELS_JSON`) or
    /// ".yaml" (`RUNPOD_MODELS_YAML`).
    pub catalogue_inline_ext: String,
}

fn env_str(env: &BTreeMap<String, String>, key: &str) -> String {
    env.get(key)
        .map(|v| v.trim().to_string())
        .unwrap_or_default()
}

fn env_str_or(env: &BTreeMap<String, String>, key: &str, default: &str) -> String {
    match env.get(key) {
        Some(v) => v.trim().to_string(),
        None => default.to_string(),
    }
}

/// URL field: trim + strip trailing slashes; default applied when unset
/// (parity with Python's `os.environ.get(name, default).strip().rstrip("/")`).
fn env_url(env: &BTreeMap<String, String>, key: &str, default: &str) -> String {
    let raw = env.get(key).map_or(default, std::string::String::as_str);
    raw.trim().trim_end_matches('/').to_string()
}

fn env_list(env: &BTreeMap<String, String>, key: &str) -> Vec<String> {
    env_str(env, key)
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

fn env_casefold_or(env: &BTreeMap<String, String>, key: &str, default: &str) -> String {
    match env.get(key) {
        Some(v) => v.trim().to_lowercase(),
        None => default.to_string(),
    }
}

fn float_env(env: &BTreeMap<String, String>, key: &str, default: f64) -> Result<f64, ConfigError> {
    let raw = env_str(env, key);
    if raw.is_empty() {
        return Ok(default);
    }
    raw.parse::<f64>()
        .map_err(|_| ConfigError(format!("{key} must be a number, got {raw:?}")))
}

/// Integer field: `int(float(raw))` — truncation, parity with Python.
fn int_env(env: &BTreeMap<String, String>, key: &str, default: i64) -> Result<i64, ConfigError> {
    // Both casts are intentional: defaults are small (no real precision loss),
    // and the f64->i64 truncation mirrors Python's `int(float(...))`
    // (e.g. "9000.9" -> 9000).
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    let value = float_env(env, key, default as f64)? as i64;
    Ok(value)
}

fn bool_env(env: &BTreeMap<String, String>, key: &str, default: bool) -> bool {
    let raw = env_str(env, key).to_lowercase();
    if raw.is_empty() {
        default
    } else {
        // Non-empty value: membership only — "maybe" is false even if the
        // default is true (parity with Python's `_bool_env`).
        ["1", "true", "yes", "on"].contains(&raw.as_str())
    }
}

fn optional_int_env(env: &BTreeMap<String, String>, key: &str) -> Result<Option<i64>, ConfigError> {
    let raw = env_str(env, key);
    if raw.is_empty() {
        Ok(None)
    } else {
        Ok(Some(int_env(env, key, 0)?))
    }
}

impl Config {
    /// Build a Config from an explicit environment map (deterministic; used
    /// by tests).
    pub fn from_env_map(env: &BTreeMap<String, String>) -> Result<Config, ConfigError> {
        let catalogue_file = env_str(env, "RUNPOD_MODELS_FILE");
        let catalogue_json = env_str(env, "RUNPOD_MODELS_JSON");
        let catalogue_yaml = env_str(env, "RUNPOD_MODELS_YAML");
        // Inline catalogue: RUNPOD_MODELS_JSON wins over RUNPOD_MODELS_YAML.
        let (catalogue_inline, catalogue_inline_ext) = if catalogue_json.is_empty() {
            (catalogue_yaml, ".yaml")
        } else {
            (catalogue_json, ".json")
        };
        let catalogue =
            load_catalogue_ext(&catalogue_file, &catalogue_inline, catalogue_inline_ext)?;

        let mut config = Config {
            serverless_url: env_url(env, "RUNPOD_SERVERLESS_URL", "https://api.runpod.io/v2"),
            api_key: env_str(env, "RUNPOD_API_KEY"),
            upstream_api_key: env_str(env, "RUNPOD_UPSTREAM_API_KEY"),
            port: int_env(env, "PORT", 8080)?,
            warmup_path: env_str_or(env, "WARMUP_PATH", "v1/models"),
            warmup_timeout_s: float_env(env, "WARMUP_TIMEOUT_S", 600.0)?,
            warmup_backoff_max_s: float_env(env, "WARMUP_BACKOFF_MAX_S", 15.0)?,
            keepalive_interval_s: float_env(env, "KEEPALIVE_INTERVAL_S", 25.0)?,
            idle_giveup_s: float_env(env, "IDLE_GIVEUP_S", 300.0)?,
            request_timeout_s: float_env(env, "REQUEST_TIMEOUT_S", 300.0)?,
            log_level: env_str_or(env, "LOG_LEVEL", "INFO"),
            log_format: env_casefold_or(env, "LOG_FORMAT", "text"),
            mode: env_str_or(env, "RUNPOD_MODE", "serverless"),
            pod_id: env_str(env, "RUNPOD_POD_ID"),
            pod_port: int_env(env, "RUNPOD_POD_PORT", 8000)?,
            pod_url: env_url(env, "RUNPOD_POD_URL", ""),
            rest_api_url: env_url(env, "RUNPOD_REST_API_URL", "https://rest.runpod.io/v1"),
            availability_api_url: env_url(
                env,
                "RUNPOD_AVAILABILITY_API_URL",
                "https://api.runpod.io/v2",
            ),
            gpu_availability_interval_s: float_env(env, "GPU_AVAILABILITY_INTERVAL_S", 300.0)?,
            proxy_api_key: env_str(env, "PROXY_API_KEY"),
            model_name: env_str(env, "RUNPOD_MODEL_NAME"),
            allowed_models: env_list(env, "RUNPOD_ALLOWED_MODELS"),
            model_switch_drain_s: float_env(env, "MODEL_SWITCH_DRAIN_S", 30.0)?,
            allow_pod_create: bool_env(env, "ALLOW_POD_CREATE", false),
            gpu_type_ids: env_list(env, "RUNPOD_GPU_TYPE_IDS"),
            gpu_type_priority: env_str_or(env, "RUNPOD_GPU_TYPE_PRIORITY", "availability"),
            cloud_type: env_str_or(env, "RUNPOD_CLOUD_TYPE", "SECURE"),
            template_name: env_str(env, "RUNPOD_TEMPLATE_NAME"),
            pod_revalidate_s: float_env(env, "POD_REVALIDATE_S", 300.0)?,
            pod_health_timeout_s: float_env(env, "POD_HEALTH_TIMEOUT_S", 180.0)?,
            pod_ready_timeout_s: float_env(env, "POD_READY_TIMEOUT_S", 120.0)?,
            pod_health_mode: env_casefold_or(env, "POD_HEALTH_MODE", "model"),
            on_migrate: env_casefold_or(env, "RUNPOD_ON_MIGRATE", "fail"),
            container_disk_gb: optional_int_env(env, "RUNPOD_CONTAINER_DISK_GB")?,
            volume_gb: optional_int_env(env, "RUNPOD_VOLUME_GB")?,
            max_create_attempts: int_env(env, "RUNPOD_MAX_CREATE_ATTEMPTS", 12)?,
            pod_circuit_breaker_threshold: int_env(env, "POD_CIRCUIT_BREAKER_THRESHOLD", 3)?,
            pod_circuit_breaker_cooldown_s: float_env(
                env,
                "POD_CIRCUIT_BREAKER_COOLDOWN_S",
                300.0,
            )?,
            max_body_bytes: int_env(env, "MAX_BODY_BYTES", 52_428_800)?,
            proxy_max_concurrent_requests: int_env(env, "PROXY_MAX_CONCURRENT_REQUESTS", 100)?,
            prewarm_times: parse_prewarm_times(&env_str(env, "PREWARM_TIMES")),
            upstream_api_key_template: env_str(env, "RUNPOD_UPSTREAM_API_KEY_TEMPLATE"),
            allowlist_configured: AtomicBool::new(
                !env_str(env, "RUNPOD_ALLOWED_MODELS").is_empty(),
            ),
            catalogue: ArcSwap::from_pointee(catalogue),
            catalogue_file,
            catalogue_inline,
            catalogue_inline_ext: catalogue_inline_ext.to_string(),
        };
        config.validate();
        Ok(config)
    }

    /// Build a Config from the process environment.
    pub fn from_env() -> Result<Config, ConfigError> {
        let env: BTreeMap<String, String> = std::env::vars().collect();
        Self::from_env_map(&env)
    }

    /// Post-init validation (port of `Config.__post_init__`): enum checks and
    /// the model-name-in-catalogue check; sets `allowlist_configured`.
    pub fn validate(&mut self) {
        assert!(
            ["pod", "serverless"].contains(&self.mode.as_str()),
            "RUNPOD_MODE must be one of ['pod', 'serverless'], got {:?}",
            self.mode
        );
        assert!(
            ["any", "completion", "model"].contains(&self.pod_health_mode.as_str()),
            "POD_HEALTH_MODE must be one of ['any', 'completion', 'model'], got {:?}",
            self.pod_health_mode
        );
        assert!(
            ["fail", "replace"].contains(&self.on_migrate.as_str()),
            "RUNPOD_ON_MIGRATE must be one of ['fail', 'replace'], got {:?}",
            self.on_migrate
        );
        if !self.catalogue().is_empty() {
            self.allowlist_configured.store(true, Ordering::Relaxed);
            if !self.model_name.is_empty() && self.catalogue().get(&self.model_name).is_none() {
                let names: Vec<String> = self.catalogue().names();
                panic!(
                    "RUNPOD_MODEL_NAME ({:?}) is not present in the model catalogue; known models: {:?}",
                    self.model_name, names
                );
            }
        }
        if !self.allowed_models.is_empty() {
            self.allowlist_configured.store(true, Ordering::Relaxed);
        }
    }

    /// The current model catalogue (a cheap `arc-swap` guard; deref for the
    /// `ModelCatalogue` API).
    #[must_use]
    pub fn catalogue(&self) -> arc_swap::Guard<Arc<ModelCatalogue>> {
        self.catalogue.load()
    }

    /// Whether a model allowlist is in effect.
    #[must_use]
    pub fn allowlist_configured(&self) -> bool {
        self.allowlist_configured.load(Ordering::Relaxed)
    }

    /// Hot-swap the catalogue after a successful `/_reload`; the allowlist is
    /// then in effect (port of `object.__setattr__(config, "catalogue", ...)`).
    pub fn set_catalogue(&self, catalogue: ModelCatalogue) {
        self.catalogue.store(Arc::new(catalogue));
        self.allowlist_configured.store(true, Ordering::Relaxed);
    }

    /// Boot-time checks (port of `main.py` lifespan startup).
    pub fn validate_boot(&self) -> Result<(), String> {
        if self.mode == "pod" && self.api_key.is_empty() {
            return Err("RUNPOD_API_KEY is required in pod mode".to_string());
        }
        if self.mode == "pod"
            && self.pod_id.is_empty()
            && self.model_name.is_empty()
            && self.allowed_models.is_empty()
        {
            return Err(
                "pod mode requires RUNPOD_POD_ID or RUNPOD_MODEL_NAME/RUNPOD_ALLOWED_MODELS"
                    .to_string(),
            );
        }
        if self.mode == "serverless" && self.serverless_url.is_empty() {
            return Err("RUNPOD_SERVERLESS_URL is required".to_string());
        }
        Ok(())
    }

    /// Re-read the catalogue source and validate it for a hot reload
    /// (port of `Config.reload_catalogue`).
    // Consumed by the `/_reload` handler (WI-11).
    #[allow(dead_code)]
    pub fn reload_catalogue(&self) -> Result<ModelCatalogue, ModelConfigError> {
        if self.catalogue_file.is_empty() && self.catalogue_inline.is_empty() {
            return Err(ModelConfigError::new(
                "no model catalogue source configured (set RUNPOD_MODELS_FILE, RUNPOD_MODELS_JSON, or RUNPOD_MODELS_YAML)",
            ));
        }
        validate_reloaded_ext(
            &self.catalogue_file,
            &self.catalogue_inline,
            &self.catalogue_inline_ext,
            &self.model_name,
        )
    }

    /// Upstream base URL (port of `Config.upstream_url`).
    pub fn upstream_url(&self) -> String {
        if self.mode == "pod" {
            if self.pod_url.is_empty() {
                if self.pod_id.is_empty() {
                    String::new()
                } else {
                    format!("https://{}-{}.proxy.runpod.net", self.pod_id, self.pod_port)
                }
            } else {
                self.pod_url.clone()
            }
        } else {
            self.serverless_url.clone()
        }
    }

    /// Warmup URL (port of `Config.warmup_url`).
    pub fn warmup_url(&self) -> String {
        let base = self.upstream_url();
        let path = self.warmup_path.trim_start_matches('/');
        if path.is_empty() {
            base
        } else {
            format!("{base}/{path}")
        }
    }

    /// True when pod discovery is active (port of `Config.discovery_enabled`).
    pub fn discovery_enabled(&self) -> bool {
        self.mode == "pod" && self.pod_id.is_empty() && !self.default_model().is_empty()
    }

    /// The model to discover/create a pod for (port of `Config.default_model`).
    pub fn default_model(&self) -> String {
        if !self.catalogue().is_empty() {
            if !self.model_name.is_empty() {
                if let Some(spec) = self.catalogue().get(&self.model_name) {
                    return spec.name.clone();
                }
            }
            return self
                .catalogue()
                .names()
                .into_iter()
                .next()
                .unwrap_or_default();
        }
        if self.model_name.is_empty() {
            self.allowed_models.first().cloned().unwrap_or_default()
        } else {
            self.model_name.clone()
        }
    }

    /// Effective allowlist (port of `Config.effective_allowed_models`).
    pub fn effective_allowed_models(&self) -> Vec<String> {
        if !self.catalogue().is_empty() {
            self.catalogue().names()
        } else if !self.allowed_models.is_empty() {
            self.allowed_models.clone()
        } else if !self.model_name.is_empty() {
            vec![self.model_name.clone()]
        } else {
            Vec::new()
        }
    }

    /// Key for the upstream `authorization` header (port of
    /// `Config.upstream_auth_key`).
    pub fn upstream_auth_key(&self) -> String {
        if !self.upstream_api_key.is_empty() {
            self.upstream_api_key.clone()
        } else if self.mode == "serverless" {
            self.api_key.clone()
        } else {
            String::new()
        }
    }

    /// Upstream auth headers (port of `Config.upstream_auth_headers`).
    pub fn upstream_auth_headers(&self) -> Vec<(String, String)> {
        let key = self.upstream_auth_key();
        if key.is_empty() {
            Vec::new()
        } else {
            vec![("authorization".to_string(), format!("Bearer {key}"))]
        }
    }

    /// Auth headers for a specific pod (port of `Config.auth_headers_for_pod`).
    ///
    /// Divergence: the Python template supports any `{field}` of the config;
    /// the Rust port supports `{pod_id}` (the only template used in practice).
    // Consumed by the pod proxy pipeline (WI-08).
    #[allow(dead_code)]
    pub fn auth_headers_for_pod(&self, pod_id: &str) -> Vec<(String, String)> {
        if !self.upstream_api_key_template.is_empty() && !pod_id.is_empty() {
            vec![(
                "authorization".to_string(),
                format!(
                    "Bearer {}",
                    self.upstream_api_key_template.replace("{pod_id}", pod_id)
                ),
            )]
        } else {
            self.upstream_auth_headers()
        }
    }
}

#[cfg(test)]
// Config values are parsed from decimal strings and compared against the same
// decimal literals, so strict equality is the correct assertion (both sides go
// through the identical f64 parse).
#[allow(clippy::float_cmp)]
mod tests {
    use super::*;

    const CATALOGUE: &str = r#"{
        "models": [
            {
                "name": "Qwen/Qwen3.8-27B-FP8",
                "templates": ["qwen3-vllm-fp8", "qwen3-vllm-a100"],
                "gpus": [
                    {"id": "NVIDIA H100 80GB HBM3", "min": 1, "max": 2},
                    {"id": "NVIDIA A100 80GB PCIe", "min": 2, "max": 4}
                ],
                "port": 8000,
                "container_disk_gb": 60,
                "volume_gb": 100,
                "cloud_type": "SECURE"
            },
            {
                "name": "meta/Llama-3-70B",
                "templates": ["llama3-vllm"]
            }
        ]
    }"#;

    fn env_of(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    fn config(pairs: &[(&str, &str)]) -> Config {
        Config::from_env_map(&env_of(pairs)).expect("config")
    }

    fn expect_config_err(env: &BTreeMap<String, String>) -> String {
        match Config::from_env_map(env) {
            Ok(_) => panic!("expected ConfigError, got Ok"),
            Err(e) => e.to_string(),
        }
    }

    struct TempFile(std::path::PathBuf);

    impl TempFile {
        fn new(name: &str, content: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "runpod-proxy-test-{}-{name}.tmp",
                std::process::id()
            ));
            std::fs::write(&path, content).expect("write temp file");
            Self(path)
        }

        fn path(&self) -> &str {
            self.0.to_str().expect("utf-8 path")
        }
    }

    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    // ------------------------------------------------------------------
    // Catalogue integration
    // ------------------------------------------------------------------

    #[test]
    fn catalogue_populates_fields() {
        let c = config(&[("RUNPOD_MODELS_JSON", CATALOGUE)]);
        assert!(!c.catalogue().is_empty());
        assert_eq!(c.catalogue().len(), 2);
        assert_eq!(
            c.catalogue().names(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );
        assert_eq!(c.default_model(), "Qwen/Qwen3.8-27B-FP8");
        assert_eq!(
            c.effective_allowed_models(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );
        assert!(c.allowlist_configured());
    }

    #[test]
    fn catalogue_from_inline_yaml() {
        const YAML: &str = "models:\n  - name: Qwen/Qwen3.8-27B-FP8\n    templates: [qwen3-vllm-fp8]\n  - name: meta/Llama-3-70B\n    templates: [llama3-vllm]\n";
        let c = config(&[("RUNPOD_MODELS_YAML", YAML)]);
        assert_eq!(
            c.catalogue().names(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );
        assert!(c.allowlist_configured());
        // Reload re-reads the inline YAML in YAML format.
        assert_eq!(c.reload_catalogue().expect("reload").len(), 2);
    }

    #[test]
    fn inline_json_wins_over_inline_yaml() {
        const YAML: &str = "models:\n  - name: only-yaml\n    templates: [t]\n";
        let c = config(&[
            ("RUNPOD_MODELS_JSON", CATALOGUE),
            ("RUNPOD_MODELS_YAML", YAML),
        ]);
        assert_eq!(
            c.catalogue().names(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );
    }

    #[test]
    fn inline_yaml_invalid_is_boot_error() {
        let m = expect_config_err(&env_of(&[("RUNPOD_MODELS_YAML", "{nope")]));
        assert!(
            m.contains("inline YAML is not valid YAML"),
            "unexpected error: {m}"
        );
    }

    #[test]
    #[should_panic(expected = "is not present in the model catalogue")]
    fn model_name_not_in_catalogue() {
        let _ = config(&[
            ("RUNPOD_MODELS_JSON", CATALOGUE),
            ("RUNPOD_MODEL_NAME", "does-not-exist"),
        ]);
    }

    #[test]
    fn model_name_resolves_to_canonical_spelling() {
        let c = config(&[
            ("RUNPOD_MODELS_JSON", CATALOGUE),
            ("RUNPOD_MODEL_NAME", "qwen/qwen3.8-27b-fp8"),
        ]);
        assert_eq!(c.default_model(), "Qwen/Qwen3.8-27B-FP8");
    }

    #[test]
    fn catalogue_from_file() {
        let file = TempFile::new("config-file.json", CATALOGUE);
        let c = config(&[("RUNPOD_MODELS_FILE", file.path())]);
        assert_eq!(c.catalogue().len(), 2);
        assert_eq!(c.catalogue_file, file.path());
    }

    #[test]
    fn catalogue_from_yaml_file() {
        let yaml = "models:\n  - name: m\n    templates: [t]\n";
        let file = TempFile::new("config-yaml.yaml", yaml);
        let c = config(&[("RUNPOD_MODELS_FILE", file.path())]);
        assert_eq!(c.catalogue().names(), vec!["m"]);
    }

    // ------------------------------------------------------------------
    // Backward compatibility (no catalogue)
    // ------------------------------------------------------------------

    #[test]
    fn backward_compat_allowed_models() {
        let c = config(&[("RUNPOD_ALLOWED_MODELS", "a, b, c")]);
        assert!(c.catalogue().is_empty());
        assert_eq!(c.allowed_models, vec!["a", "b", "c"]);
        assert_eq!(c.effective_allowed_models(), vec!["a", "b", "c"]);
        assert_eq!(c.default_model(), "a");
        assert!(c.allowlist_configured());
    }

    #[test]
    fn backward_compat_model_name() {
        let c = config(&[("RUNPOD_MODEL_NAME", "solo")]);
        assert_eq!(c.effective_allowed_models(), vec!["solo"]);
        assert_eq!(c.default_model(), "solo");
        // Only RUNPOD_MODEL_NAME is set: no allowlist, catalogue, or
        // RUNPOD_ALLOWED_MODELS, so allowlist_configured stays false (parity
        // with config.py __post_init__).
        assert!(!c.allowlist_configured());
    }

    #[test]
    fn backward_compat_empty() {
        let c = config(&[]);
        assert!(c.catalogue().is_empty());
        assert!(c.allowed_models.is_empty());
        assert!(c.model_name.is_empty());
        assert!(c.effective_allowed_models().is_empty());
        assert!(c.default_model().is_empty());
        assert!(!c.allowlist_configured());
    }

    // ------------------------------------------------------------------
    // Enum validation
    // ------------------------------------------------------------------

    #[test]
    #[should_panic(expected = "RUNPOD_MODE must be one of")]
    fn mode_rejects_unknown() {
        let _ = config(&[("RUNPOD_MODE", "hybrid")]);
    }

    #[test]
    #[should_panic(expected = "POD_HEALTH_MODE must be one of")]
    fn pod_health_mode_rejects_unknown() {
        let _ = config(&[("POD_HEALTH_MODE", "sometimes")]);
    }

    #[test]
    #[should_panic(expected = "RUNPOD_ON_MIGRATE must be one of")]
    fn on_migrate_rejects_unknown() {
        let _ = config(&[("RUNPOD_ON_MIGRATE", "maybe")]);
    }

    // ------------------------------------------------------------------
    // Boot checks
    // ------------------------------------------------------------------

    #[test]
    fn boot_pod_requires_api_key() {
        let c = config(&[("RUNPOD_MODE", "pod"), ("RUNPOD_POD_ID", "abc")]);
        assert_eq!(
            c.validate_boot().expect_err("boot"),
            "RUNPOD_API_KEY is required in pod mode"
        );
    }

    #[test]
    fn boot_pod_requires_pod_or_model() {
        let c = config(&[("RUNPOD_MODE", "pod"), ("RUNPOD_API_KEY", "k")]);
        assert_eq!(
            c.validate_boot().expect_err("boot"),
            "pod mode requires RUNPOD_POD_ID or RUNPOD_MODEL_NAME/RUNPOD_ALLOWED_MODELS"
        );
    }

    #[test]
    fn boot_serverless_requires_url() {
        let c = config(&[
            ("RUNPOD_MODE", "serverless"),
            ("RUNPOD_SERVERLESS_URL", "  "),
        ]);
        assert_eq!(
            c.validate_boot().expect_err("boot"),
            "RUNPOD_SERVERLESS_URL is required"
        );
    }

    #[test]
    fn boot_ok() {
        assert!(
            config(&[("RUNPOD_MODE", "serverless")])
                .validate_boot()
                .is_ok()
        );
        assert!(
            config(&[
                ("RUNPOD_MODE", "pod"),
                ("RUNPOD_API_KEY", "k"),
                ("RUNPOD_POD_ID", "abc")
            ])
            .validate_boot()
            .is_ok()
        );
    }

    // ------------------------------------------------------------------
    // Derived properties
    // ------------------------------------------------------------------

    #[test]
    fn upstream_url_pod() {
        let c = config(&[
            ("RUNPOD_MODE", "pod"),
            ("RUNPOD_POD_ID", "abc"),
            ("RUNPOD_POD_PORT", "9000"),
        ]);
        assert_eq!(c.upstream_url(), "https://abc-9000.proxy.runpod.net");
    }

    #[test]
    fn upstream_url_pod_custom_url() {
        let c = config(&[
            ("RUNPOD_MODE", "pod"),
            ("RUNPOD_POD_ID", "abc"),
            ("RUNPOD_POD_URL", "http://localhost:9000/"),
        ]);
        assert_eq!(c.upstream_url(), "http://localhost:9000");
    }

    #[test]
    fn upstream_url_pod_no_pod_id() {
        let c = config(&[("RUNPOD_MODE", "pod")]);
        assert_eq!(c.upstream_url(), "");
    }

    #[test]
    fn upstream_url_serverless() {
        let c = config(&[("RUNPOD_SERVERLESS_URL", "https://api.runpod.io/v2/")]);
        assert_eq!(c.upstream_url(), "https://api.runpod.io/v2");
    }

    #[test]
    fn warmup_url() {
        let c = config(&[
            ("RUNPOD_MODE", "pod"),
            ("RUNPOD_POD_ID", "abc"),
            ("WARMUP_PATH", "/v1/models"),
        ]);
        assert_eq!(
            c.warmup_url(),
            "https://abc-8000.proxy.runpod.net/v1/models"
        );
    }

    #[test]
    fn warmup_url_empty_path() {
        let c = config(&[
            ("RUNPOD_MODE", "pod"),
            ("RUNPOD_POD_ID", "abc"),
            ("WARMUP_PATH", ""),
        ]);
        assert_eq!(c.warmup_url(), "https://abc-8000.proxy.runpod.net");
    }

    #[test]
    fn discovery_enabled() {
        assert!(config(&[("RUNPOD_MODE", "pod"), ("RUNPOD_MODEL_NAME", "m")]).discovery_enabled());
        assert!(
            !config(&[
                ("RUNPOD_MODE", "pod"),
                ("RUNPOD_POD_ID", "abc"),
                ("RUNPOD_MODEL_NAME", "m")
            ])
            .discovery_enabled()
        );
        assert!(
            !config(&[("RUNPOD_MODE", "serverless"), ("RUNPOD_MODEL_NAME", "m")])
                .discovery_enabled()
        );
    }

    #[test]
    fn default_model_catalogue_first() {
        let c = config(&[("RUNPOD_MODELS_JSON", CATALOGUE)]);
        assert_eq!(c.default_model(), "Qwen/Qwen3.8-27B-FP8");
    }

    #[test]
    fn default_model_allowed_models_first() {
        let c = config(&[("RUNPOD_ALLOWED_MODELS", "x, y")]);
        assert_eq!(c.default_model(), "x");
    }

    #[test]
    fn upstream_auth_headers() {
        let c = config(&[("RUNPOD_API_KEY", "k")]);
        assert_eq!(
            c.upstream_auth_headers(),
            vec![("authorization".to_string(), "Bearer k".to_string())]
        );
    }

    #[test]
    fn upstream_auth_headers_upstream_key_wins() {
        let c = config(&[("RUNPOD_API_KEY", "k"), ("RUNPOD_UPSTREAM_API_KEY", "u")]);
        assert_eq!(
            c.upstream_auth_headers(),
            vec![("authorization".to_string(), "Bearer u".to_string())]
        );
    }

    #[test]
    fn upstream_auth_headers_pod_mode_no_key() {
        let c = config(&[("RUNPOD_MODE", "pod"), ("RUNPOD_POD_ID", "abc")]);
        assert!(c.upstream_auth_headers().is_empty());
    }

    #[test]
    fn auth_headers_for_pod_template() {
        let c = config(&[("RUNPOD_UPSTREAM_API_KEY_TEMPLATE", "sk-{pod_id}")]);
        assert_eq!(
            c.auth_headers_for_pod("abc"),
            vec![("authorization".to_string(), "Bearer sk-abc".to_string())]
        );
    }

    #[test]
    fn auth_headers_for_pod_falls_back() {
        let c = config(&[("RUNPOD_API_KEY", "k")]);
        assert_eq!(
            c.auth_headers_for_pod("abc"),
            vec![("authorization".to_string(), "Bearer k".to_string())]
        );
    }

    // ------------------------------------------------------------------
    // reload_catalogue
    // ------------------------------------------------------------------

    #[test]
    fn reload_no_source() {
        let c = config(&[]);
        assert_eq!(
            c.reload_catalogue().expect_err("reload").to_string(),
            "no model catalogue source configured (set RUNPOD_MODELS_FILE, RUNPOD_MODELS_JSON, or RUNPOD_MODELS_YAML)"
        );
    }

    #[test]
    fn reload_ok() {
        let c = config(&[("RUNPOD_MODELS_JSON", CATALOGUE)]);
        assert_eq!(c.reload_catalogue().expect("reload").len(), 2);
    }

    #[test]
    fn reload_empty_rejected() {
        let c = config(&[("RUNPOD_MODELS_JSON", r#"{"models": []}"#)]);
        assert_eq!(
            c.reload_catalogue().expect_err("reload").to_string(),
            "reloaded catalogue has no models; keeping the current catalogue"
        );
    }

    #[test]
    fn reload_drops_model_rejected() {
        let other = r#"{"models": [{"name": "other", "templates": ["t"]}]}"#;
        let m = crate::catalogue::validate_reloaded("", other, "Qwen/Qwen3.8-27B-FP8")
            .expect_err("reload");
        assert!(
            m.to_string().starts_with(
                "RUNPOD_MODEL_NAME ('Qwen/Qwen3.8-27B-FP8') is not present in the reloaded model catalogue; known models: ['other']"
            )
        );
    }

    // ------------------------------------------------------------------
    // Env parsing rules
    // ------------------------------------------------------------------

    #[test]
    fn port_trimmed() {
        let c = config(&[("PORT", " 9090 ")]);
        assert_eq!(c.port, 9090);
    }

    #[test]
    fn float_env_truncates_ints() {
        let c = config(&[("WARMUP_TIMEOUT_S", "12.7")]);
        assert_eq!(c.warmup_timeout_s, 12.7);
        let c = config(&[("RUNPOD_POD_PORT", "9000.9")]);
        assert_eq!(c.pod_port, 9000);
    }

    #[test]
    fn bool_env_spellings() {
        for v in ["1", "true", "True", "YES", "on", "On"] {
            assert!(config(&[("ALLOW_POD_CREATE", v)]).allow_pod_create, "{v}");
        }
        for v in ["0", "false", "no", "off", "", "maybe"] {
            assert!(!config(&[("ALLOW_POD_CREATE", v)]).allow_pod_create, "{v}");
        }
    }

    #[test]
    fn url_trailing_slash_stripped() {
        let c = config(&[("RUNPOD_SERVERLESS_URL", "https://x.io/v2///")]);
        assert_eq!(c.serverless_url, "https://x.io/v2");
    }

    #[test]
    fn invalid_float_is_error() {
        let m = expect_config_err(&env_of(&[("WARMUP_TIMEOUT_S", "abc")]));
        assert!(m.contains("WARMUP_TIMEOUT_S"), "message: {m}");
    }

    #[test]
    fn prewarm_times_parsed() {
        let c = config(&[("PREWARM_TIMES", "08:30, 22:15")]);
        assert_eq!(c.prewarm_times, vec![(8, 30), (22, 15)]);
    }

    #[test]
    fn optional_ints_default_none() {
        let c = config(&[]);
        assert_eq!(c.container_disk_gb, None);
        assert_eq!(c.volume_gb, None);
        let c = config(&[
            ("RUNPOD_CONTAINER_DISK_GB", "64"),
            ("RUNPOD_VOLUME_GB", "128"),
        ]);
        assert_eq!(c.container_disk_gb, Some(64));
        assert_eq!(c.volume_gb, Some(128));
    }

    #[test]
    fn defaults() {
        let c = config(&[]);
        assert_eq!(c.port, 8080);
        assert_eq!(c.mode, "serverless");
        assert_eq!(c.serverless_url, "https://api.runpod.io/v2");
        assert_eq!(c.warmup_path, "v1/models");
        assert_eq!(c.warmup_timeout_s, 600.0);
        assert_eq!(c.keepalive_interval_s, 25.0);
        assert_eq!(c.idle_giveup_s, 300.0);
        assert_eq!(c.request_timeout_s, 300.0);
        assert_eq!(c.log_level, "INFO");
        assert_eq!(c.log_format, "text");
        assert_eq!(c.pod_port, 8000);
        assert_eq!(c.rest_api_url, "https://rest.runpod.io/v1");
        assert_eq!(c.availability_api_url, "https://api.runpod.io/v2");
        assert_eq!(c.gpu_availability_interval_s, 300.0);
        assert_eq!(c.model_switch_drain_s, 30.0);
        assert!(!c.allow_pod_create);
        assert_eq!(c.gpu_type_priority, "availability");
        assert_eq!(c.cloud_type, "SECURE");
        assert_eq!(c.pod_revalidate_s, 300.0);
        assert_eq!(c.pod_health_timeout_s, 180.0);
        assert_eq!(c.pod_ready_timeout_s, 120.0);
        assert_eq!(c.pod_health_mode, "model");
        assert_eq!(c.on_migrate, "fail");
        assert_eq!(c.max_create_attempts, 12);
        assert_eq!(c.pod_circuit_breaker_threshold, 3);
        assert_eq!(c.pod_circuit_breaker_cooldown_s, 300.0);
        assert_eq!(c.max_body_bytes, 52_428_800);
        assert!(c.prewarm_times.is_empty());
        assert!(!c.allowlist_configured());
    }
}
