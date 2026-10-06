//! Model catalogue: JSON/YAML, strict validation, hot reload (spec section 7.2).
//!
//! Port of `proxy/models_config.py`. Error message strings are a wire-visible
//! contract (spec section 7.2) and match the Python reference exactly,
//! including Python `repr()` formatting of offending values.

use std::collections::{HashMap, HashSet};
use std::fmt::Write;

use serde_json::Value;
use thiserror::Error;
use yaml_rust2::{Yaml, YamlLoader};

use crate::runpod_api::model_slug;

/// A model catalogue config is missing, unreadable, or invalid.
#[derive(Debug, Error, PartialEq, Eq)]
#[error("{0}")]
pub struct ModelConfigError(pub(crate) String);

impl ModelConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

/// One GPU preference for a model (spec section 7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuSpec {
    pub id: String,
    pub min_count: i64,
    pub max_count: i64,
}

impl GpuSpec {
    /// Inclusive range of acceptable GPU counts.
    // Consumed by pod creation (WI-08).
    #[allow(dead_code)]
    pub fn counts(&self) -> Vec<i64> {
        (self.min_count..=self.max_count).collect()
    }
}

/// One model entry in the catalogue (spec section 7.2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelSpec {
    pub name: String,
    pub templates: Vec<String>,
    pub gpus: Vec<GpuSpec>,
    pub port: Option<i64>,
    pub container_disk_gb: Option<i64>,
    pub volume_gb: Option<i64>,
    pub cloud_type: Option<String>,
    /// Ordered `RunPod` datacenter ids the pod may be created in; the declared
    /// order is informational only (the v2 create API has no priority field).
    pub datacenters: Vec<String>,
    pub datacenter_priority: String,
}

/// The validated model catalogue.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModelCatalogue {
    pub models: Vec<ModelSpec>,
}

impl ModelCatalogue {
    /// Catalogue with no models.
    pub fn new() -> Self {
        Self::default()
    }

    /// Canonical model names, in catalogue order.
    pub fn names(&self) -> Vec<String> {
        self.models.iter().map(|spec| spec.name.clone()).collect()
    }

    /// Resolve a requested name by slug equality (spec section 7.1).
    pub fn get(&self, name: &str) -> Option<&ModelSpec> {
        let wanted = model_slug(name);
        if wanted.is_empty() {
            return None;
        }
        self.models
            .iter()
            .find(|spec| model_slug(&spec.name) == wanted)
    }

    /// True when the catalogue has at least one model.
    pub fn is_empty(&self) -> bool {
        self.models.is_empty()
    }

    /// Number of models.
    // Consumed by `/_status` (WI-11).
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.models.len()
    }
}

const MODEL_KEYS: &[&str] = &[
    "name",
    "templates",
    "gpus",
    "port",
    "container_disk_gb",
    "volume_gb",
    "cloud_type",
    "datacenters",
    "datacenter_priority",
];
const GPU_KEYS: &[&str] = &["id", "min", "max"];
const DATACENTER_PRIORITIES: &[&str] = &["availability", "custom"];

/// Python `repr()` of a JSON value, for error messages (contract: spec 7.2).
pub(crate) fn py_repr(value: &Value) -> String {
    match value {
        Value::Null => "None".to_string(),
        Value::Bool(true) => "True".to_string(),
        Value::Bool(false) => "False".to_string(),
        Value::Number(n) => {
            // is_i64/is_u64 check the stored representation, so a JSON `1.0`
            // (float) is rendered `1.0` like Python, not `1`.
            if n.is_i64() {
                n.as_i64().expect("is_i64").to_string()
            } else if n.is_u64() {
                n.as_u64().expect("is_u64").to_string()
            } else {
                py_float_repr(n.as_f64().expect("is_f64"))
            }
        }
        Value::String(s) => py_str_repr(s),
        Value::Array(items) => {
            let inner: Vec<String> = items.iter().map(py_repr).collect();
            format!("[{}]", inner.join(", "))
        }
        Value::Object(map) => {
            let inner: Vec<String> = map
                .iter()
                .map(|(k, v)| format!("{}: {}", py_str_repr(k), py_repr(v)))
                .collect();
            format!("{{{}}}", inner.join(", "))
        }
    }
}

fn py_float_repr(f: f64) -> String {
    if f.fract() == 0.0 && f.abs() < 1e16 {
        format!("{f}.0")
    } else {
        format!("{f}")
    }
}

/// Python `repr()` of a string: single-quoted, double-quoted only when the
/// value contains a single quote and no double quote.
pub(crate) fn py_str_repr(s: &str) -> String {
    let quote = if s.contains('\'') && !s.contains('"') {
        '"'
    } else {
        '\''
    };
    let mut out = String::with_capacity(s.len() + 2);
    out.push(quote);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            c if c == quote => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\x{:02x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push(quote);
    out
}

fn py_list_repr(items: &[&str]) -> String {
    let inner: Vec<String> = items.iter().map(|s| py_str_repr(s)).collect();
    format!("[{}]", inner.join(", "))
}

/// JSON number that is an integer (bools are a separate variant, so they are
/// naturally excluded — parity with Python's `isinstance(v, int) and not
/// isinstance(v, bool)`).
///
/// `is_i64`/`is_u64` check the stored representation, so a JSON `1.0` (a
/// float, like Python's `1.0`) is NOT an integer — `as_i64()` alone would
/// wrongly accept it.
fn as_int(value: &Value) -> Option<i64> {
    match value {
        Value::Number(n) if n.is_i64() => n.as_i64(),
        Value::Number(n) if n.is_u64() => n.as_u64().and_then(|u| i64::try_from(u).ok()),
        _ => None,
    }
}

fn positive_int(value: &Value, label: &str, where_: &str) -> Result<i64, ModelConfigError> {
    match as_int(value) {
        Some(i) if i >= 1 => Ok(i),
        _ => Err(ModelConfigError::new(format!(
            "{where_}: {label} ({}) must be a positive integer",
            py_repr(value)
        ))),
    }
}

fn parse_gpu(
    entry: &Value,
    model_index: usize,
    name: &str,
    gpu_index: usize,
) -> Result<GpuSpec, ModelConfigError> {
    let where_ = format!("models[{model_index}] ({})", py_str_repr(name));
    let Value::Object(map) = entry else {
        return Err(ModelConfigError::new(format!(
            "{where_}: 'gpus[{gpu_index}]' must be an object"
        )));
    };
    let extra: Vec<&str> = map
        .keys()
        .filter(|k| !GPU_KEYS.contains(&k.as_str()))
        .map(std::string::String::as_str)
        .collect();
    if !extra.is_empty() {
        let mut sorted = extra;
        sorted.sort_unstable();
        return Err(ModelConfigError::new(format!(
            "{where_}: 'gpus[{gpu_index}]' has unknown key(s) {}",
            py_list_repr(&sorted)
        )));
    }
    let id = match map.get("id") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => {
            return Err(ModelConfigError::new(format!(
                "{where_}: 'gpus[{gpu_index}].id' must be a non-empty string"
            )));
        }
    };
    let min_count = match map.get("min") {
        None => 1,
        Some(v) => match as_int(v) {
            Some(i) => i,
            None => {
                return Err(ModelConfigError::new(format!(
                    "{where_}: 'gpus[{gpu_index}].min' ({}) must be an integer",
                    py_repr(v)
                )));
            }
        },
    };
    if min_count < 1 {
        return Err(ModelConfigError::new(format!(
            "{where_}: 'gpus[{gpu_index}].min' ({min_count}) must be >= 1"
        )));
    }
    let max_count = match map.get("max") {
        None => min_count,
        Some(v) => match as_int(v) {
            Some(i) => i,
            None => {
                return Err(ModelConfigError::new(format!(
                    "{where_}: 'gpus[{gpu_index}].max' ({}) must be an integer",
                    py_repr(v)
                )));
            }
        },
    };
    if max_count < min_count {
        return Err(ModelConfigError::new(format!(
            "{where_}: 'gpus[{gpu_index}].max' ({max_count}) must be >= 'min' ({min_count})"
        )));
    }
    Ok(GpuSpec {
        id,
        min_count,
        max_count,
    })
}

fn parse_templates(
    map: &serde_json::Map<String, Value>,
    where_: &str,
) -> Result<Vec<String>, ModelConfigError> {
    let templates = match map.get("templates") {
        Some(Value::Array(arr)) if !arr.is_empty() => arr,
        _ => {
            return Err(ModelConfigError::new(format!(
                "{where_}: 'templates' must be a non-empty list"
            )));
        }
    };
    let mut template_names = Vec::with_capacity(templates.len());
    for (t_index, template) in templates.iter().enumerate() {
        match template {
            Value::String(s) if !s.trim().is_empty() => template_names.push(s.clone()),
            _ => {
                return Err(ModelConfigError::new(format!(
                    "{where_}: 'templates[{t_index}]' must be a non-empty string"
                )));
            }
        }
    }
    Ok(template_names)
}

fn parse_optional_positive_int(
    map: &serde_json::Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<Option<i64>, ModelConfigError> {
    match map.get(key) {
        Some(v) => Ok(Some(positive_int(v, &format!("'{key}'"), where_)?)),
        None => Ok(None),
    }
}

fn parse_optional_nonempty_string(
    map: &serde_json::Map<String, Value>,
    key: &str,
    where_: &str,
) -> Result<Option<String>, ModelConfigError> {
    match map.get(key) {
        Some(Value::String(s)) if !s.trim().is_empty() => Ok(Some(s.clone())),
        Some(_) => Err(ModelConfigError::new(format!(
            "{where_}: '{key}' must be a non-empty string"
        ))),
        None => Ok(None),
    }
}

fn parse_datacenters(
    map: &serde_json::Map<String, Value>,
    where_: &str,
) -> Result<Vec<String>, ModelConfigError> {
    let empty_dcs = Value::Array(Vec::new());
    let dc_raw = map.get("datacenters").unwrap_or(&empty_dcs);
    let Value::Array(dc_arr) = dc_raw else {
        return Err(ModelConfigError::new(format!(
            "{where_}: 'datacenters' must be a list"
        )));
    };
    let mut datacenters = Vec::with_capacity(dc_arr.len());
    let mut seen_datacenters: HashSet<String> = HashSet::new();
    for (d_index, datacenter) in dc_arr.iter().enumerate() {
        let s = match datacenter {
            Value::String(s) if !s.trim().is_empty() => s.clone(),
            _ => {
                return Err(ModelConfigError::new(format!(
                    "{where_}: 'datacenters[{d_index}]' must be a non-empty string"
                )));
            }
        };
        if !seen_datacenters.insert(s.clone()) {
            return Err(ModelConfigError::new(format!(
                "{where_}: duplicate datacenter {} in 'datacenters'",
                py_str_repr(&s)
            )));
        }
        datacenters.push(s);
    }
    Ok(datacenters)
}

fn parse_datacenter_priority(
    map: &serde_json::Map<String, Value>,
    where_: &str,
) -> Result<String, ModelConfigError> {
    let datacenter_priority = match map.get("datacenter_priority") {
        None => "availability".to_string(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => {
            return Err(ModelConfigError::new(format!(
                "{where_}: 'datacenter_priority' ({}) must be one of ['availability', 'custom']",
                py_repr(other)
            )));
        }
    };
    if !DATACENTER_PRIORITIES.contains(&datacenter_priority.as_str()) {
        return Err(ModelConfigError::new(format!(
            "{where_}: 'datacenter_priority' ({}) must be one of ['availability', 'custom']",
            py_str_repr(&datacenter_priority)
        )));
    }
    Ok(datacenter_priority)
}

fn parse_model(entry: &Value, index: usize) -> Result<ModelSpec, ModelConfigError> {
    let Value::Object(map) = entry else {
        return Err(ModelConfigError::new(format!(
            "models[{index}] must be an object"
        )));
    };
    let name = match map.get("name") {
        Some(Value::String(s)) if !s.trim().is_empty() => s.clone(),
        _ => {
            return Err(ModelConfigError::new(format!(
                "models[{index}]: 'name' must be a non-empty string"
            )));
        }
    };
    let where_ = format!("models[{index}] ({})", py_str_repr(&name));
    let extra: Vec<&str> = map
        .keys()
        .filter(|k| !MODEL_KEYS.contains(&k.as_str()))
        .map(std::string::String::as_str)
        .collect();
    if !extra.is_empty() {
        let mut sorted = extra;
        sorted.sort_unstable();
        return Err(ModelConfigError::new(format!(
            "{where_}: unknown key(s) {}",
            py_list_repr(&sorted)
        )));
    }

    let template_names = parse_templates(map, &where_)?;

    let empty_gpus = Value::Array(Vec::new());
    let gpus_raw = map.get("gpus").unwrap_or(&empty_gpus);
    let Value::Array(gpus_arr) = gpus_raw else {
        return Err(ModelConfigError::new(format!(
            "{where_}: 'gpus' must be a list"
        )));
    };
    let mut gpus = Vec::with_capacity(gpus_arr.len());
    let mut seen_gpu_ids: HashSet<String> = HashSet::new();
    for (g_index, gpu_entry) in gpus_arr.iter().enumerate() {
        let gpu = parse_gpu(gpu_entry, index, &name, g_index)?;
        let slug = gpu.id.trim().to_lowercase();
        if !seen_gpu_ids.insert(slug) {
            return Err(ModelConfigError::new(format!(
                "{where_}: duplicate gpu id {} in 'gpus[{g_index}]'",
                py_str_repr(&gpu.id)
            )));
        }
        gpus.push(gpu);
    }

    let port = parse_optional_positive_int(map, "port", &where_)?;
    let container_disk_gb = parse_optional_positive_int(map, "container_disk_gb", &where_)?;
    let volume_gb = parse_optional_positive_int(map, "volume_gb", &where_)?;
    let cloud_type = parse_optional_nonempty_string(map, "cloud_type", &where_)?;
    let datacenters = parse_datacenters(map, &where_)?;
    let datacenter_priority = parse_datacenter_priority(map, &where_)?;

    Ok(ModelSpec {
        name,
        templates: template_names,
        gpus,
        port,
        container_disk_gb,
        volume_gb,
        cloud_type,
        datacenters,
        datacenter_priority,
    })
}

fn build_catalogue(data: &Value) -> Result<ModelCatalogue, ModelConfigError> {
    let raw_models: &Vec<Value> = match data {
        Value::Object(map) => match map.get("models") {
            Some(Value::Array(arr)) => arr,
            _ => {
                return Err(ModelConfigError::new(
                    "top-level object must contain a 'models' list",
                ));
            }
        },
        Value::Array(arr) => arr,
        _ => {
            return Err(ModelConfigError::new(
                "catalogue must be an object with a 'models' list or a bare array",
            ));
        }
    };

    let mut specs = Vec::with_capacity(raw_models.len());
    let mut seen_slugs: HashMap<String, String> = HashMap::new();
    for (index, entry) in raw_models.iter().enumerate() {
        let spec = parse_model(entry, index)?;
        let slug = model_slug(&spec.name);
        if let Some(first) = seen_slugs.get(&slug) {
            return Err(ModelConfigError::new(format!(
                "models[{index}] ({}): duplicate model name; collides with {} (compared by slug '{}')",
                py_str_repr(&spec.name),
                py_str_repr(first),
                slug
            )));
        }
        seen_slugs.insert(slug, spec.name.clone());
        specs.push(spec);
    }
    Ok(ModelCatalogue { models: specs })
}

/// YAML 1.1 plain-scalar resolution (`PyYAML` parity): null and boolean words.
///
/// Divergence: yaml-rust2 does not expose whether a scalar was quoted, so
/// quoted `"on"`/`"yes"`/... are also resolved as booleans (`PyYAML` would keep
/// them as strings). Stricter than Python; matches the tested behavior for
/// plain scalars.
const YAML_NULLS: &[&str] = &["~", "null", "Null", "NULL", ""];
const YAML_TRUES: &[&str] = &[
    "y", "Y", "yes", "Yes", "YES", "true", "True", "TRUE", "on", "On", "ON",
];
const YAML_FALSES: &[&str] = &[
    "n", "N", "no", "No", "NO", "false", "False", "FALSE", "off", "Off", "OFF",
];

fn yaml_scalar_string(s: &str) -> Value {
    if YAML_NULLS.contains(&s) {
        Value::Null
    } else if YAML_TRUES.contains(&s) {
        Value::Bool(true)
    } else if YAML_FALSES.contains(&s) {
        Value::Bool(false)
    } else {
        Value::String(s.to_string())
    }
}

fn yaml_to_value(node: &Yaml) -> Value {
    match node {
        Yaml::Boolean(b) => Value::Bool(*b),
        Yaml::Integer(i) => Value::Number((*i).into()),
        Yaml::Real(s) => {
            // Non-finite reals (`.inf`, `.nan`) have no JSON representation;
            // they become null (documented divergence from PyYAML).
            match s.parse::<f64>() {
                Ok(f) if f.is_finite() => {
                    serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number)
                }
                _ => Value::Null,
            }
        }
        Yaml::String(s) => yaml_scalar_string(s),
        Yaml::Array(items) => Value::Array(items.iter().map(yaml_to_value).collect()),
        Yaml::Hash(pairs) => {
            let mut map = serde_json::Map::new();
            for (k, v) in pairs {
                // Non-string keys (e.g. `1:`) are rendered Python-style; the
                // validation then rejects them as unknown keys.
                let key = match k {
                    Yaml::String(s) => s.clone(),
                    other => py_repr(&yaml_to_value(other)),
                };
                map.insert(key, yaml_to_value(v));
            }
            Value::Object(map)
        }
        // Aliases are not fully supported by yaml-rust2; treat as null.
        Yaml::Null | Yaml::Alias(_) | Yaml::BadValue => Value::Null,
    }
}

fn parse_yaml(raw: &str) -> Result<Value, String> {
    let docs = YamlLoader::load_from_str(raw).map_err(|e| e.to_string())?;
    let doc = match docs.len() {
        0 => Yaml::Null,
        1 => docs.into_iter().next().expect("len == 1"),
        _ => return Err("expected a single document in the stream, but found another".to_string()),
    };
    Ok(yaml_to_value(&doc))
}

fn parse_json(raw: &str) -> Result<Value, String> {
    serde_json::from_str(raw).map_err(|e| e.to_string())
}

/// Parse catalogue text in the format dictated by the file extension
/// (port of `models_config.py::_parse_document`).
fn parse_document(raw: &str, source: &str, ext: &str) -> Result<Value, ModelConfigError> {
    let ext = ext.to_lowercase();
    match ext.as_str() {
        ".yaml" | ".yml" => parse_yaml(raw).map_err(|e| {
            ModelConfigError::new(format!("model catalogue {source} is not valid YAML: {e}"))
        }),
        ".json" => parse_json(raw).map_err(|e| {
            ModelConfigError::new(format!("model catalogue {source} is not valid JSON: {e}"))
        }),
        _ => match parse_json(raw) {
            Ok(value) => Ok(value),
            Err(_) => parse_yaml(raw).map_err(|e| {
                ModelConfigError::new(format!(
                    "model catalogue {source} is not valid JSON or YAML: {e}"
                ))
            }),
        },
    }
}

/// Load and validate a catalogue from a file path and/or inline JSON
/// (port of `models_config.py::load_catalogue`). Path wins over inline;
/// with neither, an empty catalogue.
#[cfg(test)]
pub fn load_catalogue(path: &str, inline: &str) -> Result<ModelCatalogue, ModelConfigError> {
    load_catalogue_ext(path, inline, ".json")
}

/// Like [`load_catalogue`] but the inline string is parsed in the format named
/// by `inline_ext` (".json" for `RUNPOD_MODELS_JSON`, ".yaml"/".yml" for
/// `RUNPOD_MODELS_YAML`). The file branch still dispatches on its own extension.
pub fn load_catalogue_ext(
    path: &str,
    inline: &str,
    inline_ext: &str,
) -> Result<ModelCatalogue, ModelConfigError> {
    if !path.is_empty() {
        let raw = std::fs::read_to_string(path).map_err(|e| {
            ModelConfigError::new(format!(
                "could not read model catalogue file {}: {e}",
                py_str_repr(path)
            ))
        })?;
        let source = format!("file {}", py_str_repr(path));
        let ext = std::path::Path::new(path)
            .extension()
            .map(|e| format!(".{}", e.to_string_lossy()))
            .unwrap_or_default();
        let data = parse_document(&raw, &source, &ext)?;
        build_catalogue(&data)
    } else if !inline.is_empty() {
        let source = match inline_ext.to_lowercase().as_str() {
            ".yaml" | ".yml" => "inline YAML",
            _ => "inline JSON",
        };
        let data = parse_document(inline, source, inline_ext)?;
        build_catalogue(&data)
    } else {
        Ok(ModelCatalogue::new())
    }
}

/// Re-read and validate a catalogue for a hot reload
/// (port of `models_config.py::validate_reloaded`).
#[cfg(test)]
pub fn validate_reloaded(
    path: &str,
    inline: &str,
    model_name: &str,
) -> Result<ModelCatalogue, ModelConfigError> {
    validate_reloaded_ext(path, inline, ".json", model_name)
}

/// Like [`validate_reloaded`] but the inline string is parsed in the format
/// named by `inline_ext` (".json" or ".yaml"/".yml").
pub fn validate_reloaded_ext(
    path: &str,
    inline: &str,
    inline_ext: &str,
    model_name: &str,
) -> Result<ModelCatalogue, ModelConfigError> {
    let catalogue = load_catalogue_ext(path, inline, inline_ext)?;
    if catalogue.is_empty() {
        return Err(ModelConfigError::new(
            "reloaded catalogue has no models; keeping the current catalogue",
        ));
    }
    if !model_name.is_empty() && catalogue.get(model_name).is_none() {
        let names: Vec<String> = catalogue.names().iter().map(|s| py_str_repr(s)).collect();
        return Err(ModelConfigError::new(format!(
            "RUNPOD_MODEL_NAME ({}) is not present in the reloaded model catalogue; known models: [{}]",
            py_str_repr(model_name),
            names.join(", ")
        )));
    }
    Ok(catalogue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const CATALOGUE_JSON: &str = r#"{
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

    fn catalogue() -> Value {
        serde_json::from_str(CATALOGUE_JSON).expect("valid test catalogue")
    }

    const YAML_CATALOGUE: &str = r#"
models:
  - name: Qwen/Qwen3.8-27B-FP8
    templates: [qwen3-vllm-fp8, qwen3-vllm-a100]
    gpus:
      - {id: "NVIDIA H100 80GB HBM3", min: 1, max: 2}
      - {id: "NVIDIA A100 80GB PCIe", min: 2, max: 4}
    port: 8000
    container_disk_gb: 60
    volume_gb: 100
    cloud_type: SECURE
  - name: meta/Llama-3-70B
    templates: [llama3-vllm]
"#;

    /// Unique temp file, removed on drop (tests run in parallel threads).
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

    fn write_json(name: &str, data: &Value) -> TempFile {
        TempFile::new(name, &serde_json::to_string(data).expect("serialize"))
    }

    fn msg(err: &ModelConfigError) -> String {
        err.to_string()
    }

    fn expect_err(result: Result<ModelCatalogue, ModelConfigError>) -> String {
        match result {
            Ok(_) => panic!("expected ModelConfigError, got Ok"),
            Err(e) => msg(&e),
        }
    }

    // ------------------------------------------------------------------
    // Happy paths
    // ------------------------------------------------------------------

    #[test]
    fn happy_path_from_file() {
        let file = write_json("happy-file", &catalogue());
        let cat = load_catalogue(file.path(), "").expect("load");
        assert!(!cat.is_empty());
        assert_eq!(cat.len(), 2);
        assert_eq!(
            cat.names(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );

        let qwen = cat.get("Qwen/Qwen3.8-27B-FP8").expect("qwen");
        assert_eq!(qwen.templates, vec!["qwen3-vllm-fp8", "qwen3-vllm-a100"]);
        assert_eq!(
            qwen.gpus.iter().map(|g| g.id.clone()).collect::<Vec<_>>(),
            vec!["NVIDIA H100 80GB HBM3", "NVIDIA A100 80GB PCIe"]
        );
        assert_eq!(qwen.gpus[0].counts(), vec![1, 2]);
        assert_eq!(qwen.gpus[1].counts(), vec![2, 3, 4]);
        assert_eq!(qwen.port, Some(8000));
        assert_eq!(qwen.container_disk_gb, Some(60));
        assert_eq!(qwen.volume_gb, Some(100));
        assert_eq!(qwen.cloud_type.as_deref(), Some("SECURE"));
    }

    #[test]
    fn max_defaults_to_min() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "gpu", "min": 3}]}]});
        let cat = load_catalogue("", &data.to_string()).expect("load");
        let gpu = &cat.get("m").expect("m").gpus[0];
        assert_eq!(gpu.min_count, 3);
        assert_eq!(gpu.max_count, 3);
        assert_eq!(gpu.counts(), vec![3]);
    }

    #[test]
    fn min_defaults_to_one() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "gpu"}]}]});
        let cat = load_catalogue("", &data.to_string()).expect("load");
        assert_eq!(cat.get("m").expect("m").gpus[0].counts(), vec![1]);
    }

    #[test]
    fn happy_path_inline() {
        let cat = load_catalogue("", CATALOGUE_JSON).expect("load");
        assert_eq!(
            cat.names(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );
    }

    #[test]
    fn bare_array_top_level() {
        let data = &catalogue()["models"];
        let cat = load_catalogue("", &data.to_string()).expect("load");
        assert_eq!(
            cat.names(),
            vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
        );
    }

    #[test]
    fn path_wins_over_inline() {
        let file = write_json(
            "path-wins",
            &json!({"models": [{"name": "fromfile", "templates": ["t"]}]}),
        );
        let cat = load_catalogue(file.path(), CATALOGUE_JSON).expect("load");
        assert_eq!(cat.names(), vec!["fromfile"]);
    }

    #[test]
    fn empty_catalogue_when_neither() {
        let cat = load_catalogue("", "").expect("load");
        assert!(cat.is_empty());
        assert_eq!(cat.len(), 0);
        assert!(cat.names().is_empty());
    }

    // ------------------------------------------------------------------
    // YAML support
    // ------------------------------------------------------------------

    #[test]
    fn happy_path_from_yaml_file() {
        for ext in [".yaml", ".yml"] {
            let file = TempFile::new(&format!("happy-yaml-{ext}"), YAML_CATALOGUE);
            let cat = load_catalogue(file.path(), "").expect("load");
            assert!(!cat.is_empty());
            assert_eq!(cat.len(), 2);
            assert_eq!(
                cat.names(),
                vec!["Qwen/Qwen3.8-27B-FP8", "meta/Llama-3-70B"]
            );

            let qwen = cat.get("Qwen/Qwen3.8-27B-FP8").expect("qwen");
            assert_eq!(qwen.templates, vec!["qwen3-vllm-fp8", "qwen3-vllm-a100"]);
            assert_eq!(
                qwen.gpus.iter().map(|g| g.id.clone()).collect::<Vec<_>>(),
                vec!["NVIDIA H100 80GB HBM3", "NVIDIA A100 80GB PCIe"]
            );
            assert_eq!(qwen.gpus[0].counts(), vec![1, 2]);
            assert_eq!(qwen.gpus[1].counts(), vec![2, 3, 4]);
            assert_eq!(qwen.port, Some(8000));
            assert_eq!(qwen.container_disk_gb, Some(60));
            assert_eq!(qwen.volume_gb, Some(100));
            assert_eq!(qwen.cloud_type.as_deref(), Some("SECURE"));
        }
    }

    #[test]
    fn bad_yaml() {
        let file = TempFile::new("bad-yaml", "models:\n  - name: [unclosed\n");
        let m = expect_err(load_catalogue(file.path(), ""));
        assert!(m.contains("YAML"), "message: {m}");
    }

    #[test]
    fn empty_yaml_file_rejected() {
        let file = TempFile::new("empty-yaml", "");
        let m = expect_err(load_catalogue(file.path(), ""));
        assert!(m.contains("models"), "message: {m}");
    }

    #[test]
    fn unknown_extension_parses_yaml() {
        let file = TempFile::new("no-ext", "models:\n  - name: m\n    templates: [t]\n");
        let cat = load_catalogue(file.path(), "").expect("load");
        assert_eq!(cat.names(), vec!["m"]);
    }

    #[test]
    fn unknown_extension_prefers_json() {
        let data = json!({"models": [{"name": "m", "templates": ["t"]}]});
        let file = write_json("no-ext-json", &data);
        let cat = load_catalogue(file.path(), "").expect("load");
        assert_eq!(cat.names(), vec!["m"]);
    }

    #[test]
    fn unknown_extension_bad_content() {
        let file = TempFile::new("no-ext-bad", "models: [ {name: m\n");
        let m = expect_err(load_catalogue(file.path(), ""));
        assert!(m.contains("JSON or YAML"), "message: {m}");
    }

    #[test]
    fn yaml_bool_name_rejected() {
        // YAML 1.1 parses unquoted "on" as a boolean; validation must reject
        // it with a clear name error rather than crash downstream.
        let file = TempFile::new(
            "yaml-bool-name",
            "models:\n  - name: on\n    templates: [t]\n",
        );
        let m = expect_err(load_catalogue(file.path(), ""));
        assert!(m.contains("'name'"), "message: {m}");
    }

    // ------------------------------------------------------------------
    // get() resolution
    // ------------------------------------------------------------------

    #[test]
    fn get_exact_case_and_slug() {
        let cat = load_catalogue("", CATALOGUE_JSON).expect("load");
        assert_eq!(
            cat.get("Qwen/Qwen3.8-27B-FP8").expect("exact").name,
            "Qwen/Qwen3.8-27B-FP8"
        );
        assert_eq!(
            cat.get("qwen/qwen3.8-27b-fp8").expect("lower").name,
            "Qwen/Qwen3.8-27B-FP8"
        );
        assert_eq!(
            cat.get("qwen-qwen3-8-27b-fp8").expect("slug").name,
            "Qwen/Qwen3.8-27B-FP8"
        );
        assert!(cat.get("does-not-exist").is_none());
        assert!(cat.get("").is_none());
    }

    // ------------------------------------------------------------------
    // Validation failures
    // ------------------------------------------------------------------

    #[test]
    fn missing_file() {
        let m = expect_err(load_catalogue("/no/such/file/here.json", ""));
        assert!(m.to_lowercase().contains("read"), "message: {m}");
    }

    #[test]
    fn bad_json() {
        let m = expect_err(load_catalogue("", "{not valid json"));
        assert!(m.contains("JSON"), "message: {m}");
    }

    #[test]
    fn top_level_wrong_type() {
        expect_err(load_catalogue("", "42"));
    }

    #[test]
    fn top_level_object_without_models_list() {
        let m = expect_err(load_catalogue("", &json!({"model": []}).to_string()));
        assert!(m.contains("models"), "message: {m}");
    }

    #[test]
    fn model_not_object() {
        let m = expect_err(load_catalogue("", &json!({"models": ["nope"]}).to_string()));
        assert!(m.contains("models[0]"), "message: {m}");
    }

    #[test]
    fn name_missing() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"templates": ["t"]}]}).to_string(),
        ));
        assert!(m.contains("'name'"), "message: {m}");
    }

    #[test]
    fn name_blank() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "  ", "templates": ["t"]}]}).to_string(),
        ));
        assert!(m.contains("'name'"), "message: {m}");
    }

    #[test]
    fn duplicate_model_by_slug() {
        let data = json!({"models": [
            {"name": "Qwen/Qwen3-32B", "templates": ["t"]},
            {"name": "qwen-qwen3-32b", "templates": ["t"]},
        ]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert!(m.to_lowercase().contains("duplicate"), "message: {m}");
    }

    #[test]
    fn templates_missing() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m"}]}).to_string(),
        ));
        assert!(m.contains("'templates'"), "message: {m}");
    }

    #[test]
    fn templates_not_list() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": "t"}]}).to_string(),
        ));
        assert!(m.contains("'templates'"), "message: {m}");
    }

    #[test]
    fn templates_empty() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": []}]}).to_string(),
        ));
        assert!(m.contains("'templates'"), "message: {m}");
    }

    #[test]
    fn templates_blank_entry() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": [" "]}]}).to_string(),
        ));
        assert!(m.contains("templates[0]"), "message: {m}");
    }

    #[test]
    fn unknown_key_at_model_level() {
        let data = json!({"models": [{"name": "m", "template": ["t"], "templates": ["t"]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert!(m.contains("unknown key"), "message: {m}");
        assert!(m.contains("template"), "message: {m}");
    }

    #[test]
    fn unknown_key_at_gpu_level() {
        let data =
            json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "mn": 1}]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert!(m.contains("unknown key"), "message: {m}");
    }

    #[test]
    fn gpus_not_list() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": ["t"], "gpus": {}}]}).to_string(),
        ));
        assert!(m.contains("'gpus'"), "message: {m}");
    }

    #[test]
    fn gpu_not_object() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": ["t"], "gpus": ["g"]}]}).to_string(),
        ));
        assert!(m.contains("gpus[0]"), "message: {m}");
    }

    #[test]
    fn gpu_id_missing() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"min": 1}]}]})
                .to_string(),
        ));
        assert!(m.contains("gpus[0].id"), "message: {m}");
    }

    #[test]
    fn min_less_than_one() {
        let m = expect_err(
            load_catalogue("", &json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": 0}]}]}).to_string()),
        );
        assert!(m.contains("gpus[0].min"), "message: {m}");
    }

    #[test]
    fn max_less_than_min() {
        let data = json!({"models": [{"name": "Qwen/Qwen3.8-27B-FP8", "templates": ["t"], "gpus": [{"id": "g", "min": 2, "max": 1}]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert!(m.contains("gpus[0].max"), "message: {m}");
        assert!(m.contains("min"), "message: {m}");
    }

    #[test]
    fn bool_rejected_as_int() {
        // JSON true -> bool; must be rejected as an integer for min.
        let m = expect_err(
            load_catalogue("", &json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": true}]}]}).to_string()),
        );
        assert!(m.contains("gpus[0].min"), "message: {m}");
    }

    #[test]
    fn duplicate_gpu_id() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g"}, {"id": "g"}]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert!(m.contains("duplicate gpu id"), "message: {m}");
    }

    #[test]
    fn port_not_positive_int() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": ["t"], "port": 0}]}).to_string(),
        ));
        assert!(m.contains("'port'"), "message: {m}");
    }

    #[test]
    fn container_disk_not_positive_int() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": ["t"], "container_disk_gb": -5}]})
                .to_string(),
        ));
        assert!(m.contains("'container_disk_gb'"), "message: {m}");
    }

    #[test]
    fn volume_not_positive_int() {
        let m = expect_err(load_catalogue(
            "",
            &json!({"models": [{"name": "m", "templates": ["t"], "volume_gb": "big"}]}).to_string(),
        ));
        assert!(m.contains("'volume_gb'"), "message: {m}");
    }

    // ------------------------------------------------------------------
    // Exact error-message contract (spec 7.2)
    // ------------------------------------------------------------------

    #[test]
    fn exact_message_min_string() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": "2"}]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(m, "models[0] ('m'): 'gpus[0].min' ('2') must be an integer");
    }

    #[test]
    fn exact_message_min_null() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": null}]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(
            m,
            "models[0] ('m'): 'gpus[0].min' (None) must be an integer"
        );
    }

    #[test]
    fn exact_message_port_null() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "port": null}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(
            m,
            "models[0] ('m'): 'port' (None) must be a positive integer"
        );
    }

    #[test]
    fn exact_message_max_below_min() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "gpus": [{"id": "g", "min": 4, "max": 2}]}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(m, "models[0] ('m'): 'gpus[0].max' (2) must be >= 'min' (4)");
    }

    #[test]
    fn exact_message_unknown_keys_sorted() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "zz": 1, "aa": 2}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(m, "models[0] ('m'): unknown key(s) ['aa', 'zz']");
    }

    #[test]
    fn exact_message_duplicate_model() {
        let data = json!({"models": [
            {"name": "A/B", "templates": ["t"]},
            {"name": "a-b", "templates": ["t"]},
        ]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(
            m,
            "models[1] ('a-b'): duplicate model name; collides with 'A/B' (compared by slug 'a-b')"
        );
    }

    #[test]
    fn exact_message_datacenter_priority_int() {
        let data = json!({"models": [{"name": "m", "templates": ["t"], "datacenter_priority": 5}]});
        let m = expect_err(load_catalogue("", &data.to_string()));
        assert_eq!(
            m,
            "models[0] ('m'): 'datacenter_priority' (5) must be one of ['availability', 'custom']"
        );
    }

    #[test]
    fn exact_message_inline_bad_json() {
        let m = expect_err(load_catalogue("", "{nope"));
        assert!(
            m.starts_with("model catalogue inline JSON is not valid JSON: "),
            "message: {m}"
        );
    }

    #[test]
    fn exact_message_file_source() {
        let file = TempFile::new("file-source", "{nope");
        let m = expect_err(load_catalogue(file.path(), ""));
        // TempFile uses a `.tmp` extension, so the parser tries JSON then YAML
        // and reports "JSON or YAML" (parity with models_config.py
        // _parse_document for non-.json/.yaml extensions).
        assert!(
            m.starts_with(&format!(
                "model catalogue file {} is not valid JSON or YAML: ",
                py_str_repr(file.path())
            )),
            "message: {m}"
        );
    }

    // ------------------------------------------------------------------
    // Datacentre pinning
    // ------------------------------------------------------------------

    fn one_model(extra: Value) -> String {
        let mut model = serde_json::Map::new();
        model.insert("name".into(), json!("m"));
        model.insert("templates".into(), json!(["t"]));
        if let Value::Object(extra) = extra {
            for (k, v) in extra {
                model.insert(k, v);
            }
        }
        json!({"models": [model]}).to_string()
    }

    #[test]
    fn datacenters_parsed() {
        let cat = load_catalogue(
            "",
            &one_model(
                json!({"datacenters": ["US-TX-3", "US-KS-3"], "datacenter_priority": "custom"}),
            ),
        )
        .expect("load");
        let spec = cat.get("m").expect("m");
        assert_eq!(spec.datacenters, vec!["US-TX-3", "US-KS-3"]);
        assert_eq!(spec.datacenter_priority, "custom");
    }

    #[test]
    fn datacenters_default_to_any() {
        let cat = load_catalogue("", &one_model(json!({}))).expect("load");
        let spec = cat.get("m").expect("m");
        assert!(spec.datacenters.is_empty());
        assert_eq!(spec.datacenter_priority, "availability");
    }

    #[test]
    fn datacenters_reject_not_a_list() {
        let m = expect_err(load_catalogue(
            "",
            &one_model(json!({"datacenters": "not-a-list"})),
        ));
        assert!(m.contains("'datacenters' must be a list"), "message: {m}");
    }

    #[test]
    fn datacenters_reject_empty_entry() {
        let m = expect_err(load_catalogue(
            "",
            &one_model(json!({"datacenters": ["US-TX-3", ""]})),
        ));
        assert!(m.contains("non-empty string"), "message: {m}");
    }

    #[test]
    fn datacenters_reject_duplicate() {
        let m = expect_err(load_catalogue(
            "",
            &one_model(json!({"datacenters": ["US-TX-3", "US-TX-3"]})),
        ));
        assert!(m.contains("duplicate datacenter"), "message: {m}");
    }

    #[test]
    fn datacenter_priority_rejects_unknown() {
        let m = expect_err(load_catalogue(
            "",
            &one_model(json!({"datacenter_priority": "nearest"})),
        ));
        assert!(m.contains("datacenter_priority"), "message: {m}");
    }

    // ------------------------------------------------------------------
    // py_repr
    // ------------------------------------------------------------------

    #[test]
    fn py_repr_scalars() {
        assert_eq!(py_repr(&Value::Null), "None");
        assert_eq!(py_repr(&json!(true)), "True");
        assert_eq!(py_repr(&json!(false)), "False");
        assert_eq!(py_repr(&json!(42)), "42");
        assert_eq!(py_repr(&json!(-7)), "-7");
        assert_eq!(py_repr(&json!(1.0)), "1.0");
        assert_eq!(py_repr(&json!(1.5)), "1.5");
        assert_eq!(py_repr(&json!("a")), "'a'");
        assert_eq!(py_repr(&json!("a'b")), "\"a'b\"");
        assert_eq!(py_repr(&json!("a\"b")), "'a\"b'");
        assert_eq!(py_repr(&json!("a\\b")), "'a\\\\b'");
        assert_eq!(py_repr(&json!("a\nb")), "'a\\nb'");
    }

    #[test]
    fn py_repr_composite() {
        assert_eq!(py_repr(&json!([1, "x", null])), "[1, 'x', None]");
        assert_eq!(py_repr(&json!([])), "[]");
    }

    // ------------------------------------------------------------------
    // validate_reloaded
    // ------------------------------------------------------------------

    #[test]
    fn reload_ok() {
        let cat = validate_reloaded("", CATALOGUE_JSON, "").expect("reload");
        assert_eq!(cat.len(), 2);
    }

    #[test]
    fn reload_empty_rejected() {
        let m = expect_err(validate_reloaded(
            "",
            &json!({"models": []}).to_string(),
            "",
        ));
        assert_eq!(
            m,
            "reloaded catalogue has no models; keeping the current catalogue"
        );
    }

    #[test]
    fn reload_drops_default_model_rejected() {
        let m = expect_err(validate_reloaded(
            "",
            &json!({"models": [{"name": "other", "templates": ["t"]}]}).to_string(),
            "Qwen/Qwen3.8-27B-FP8",
        ));
        assert!(
            m.starts_with("RUNPOD_MODEL_NAME ('Qwen/Qwen3.8-27B-FP8') is not present in the reloaded model catalogue; known models: ['other']"),
            "message: {m}"
        );
    }
}
