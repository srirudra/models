//! `RunPod` REST client (v1 + v2) with typed errors (spec section 9). WI-08.
//!
//! Port of `proxy/runpod_api.py`. A single shared `reqwest::Client`
//! (connection pooling) backs every call. The v1 base (`RUNPOD_REST_URL`)
//! serves read/start/stop/delete; the v2 base (`RUNPOD_AVAILABILITY_URL`)
//! serves pod creation and network volumes (the v1 create schema predates
//! network volumes).

use std::collections::BTreeMap;
use std::sync::LazyLock;

use axum::http::Method;
use regex::Regex;
use serde_json::{Map, Value, json};

/// Pod status: running.
pub const RUNNING: &str = "RUNNING";
/// Pod status: exited (stopped).
pub const EXITED: &str = "EXITED";
/// Pod status: terminated (deleted).
pub const TERMINATED: &str = "TERMINATED";

/// A `RunPod` REST call failed (transport error or non-2xx), or `RunPod` signalled
/// a pod-migration requirement / a v2 capacity shortage.
#[derive(Debug, thiserror::Error)]
pub enum RunpodApiError {
    /// A transport error or a non-2xx response that is neither a migration
    /// prompt nor a capacity shortage.
    #[error("{0}")]
    General(String),
    /// `RunPod` says the pod is tied to a machine whose GPUs are no longer
    /// available — the "please migrate your pod" prompt, or the REST variant
    /// "There are not enough free GPUs on the host machine to start this pod".
    /// A pinned pod keeps its machine assignment, so a plain start retry can
    /// never succeed: callers must surface it (`RUNPOD_ON_MIGRATE=fail`) or
    /// terminate + recreate (`RUNPOD_ON_MIGRATE=replace`). `body` carries the
    /// raw response for diagnostics (the prompt is beta/undocumented).
    #[error("{message}")]
    Migration {
        /// The formatted error message (operation + HTTP status + prompt).
        message: String,
        /// The raw response body, for diagnostics.
        body: Option<Value>,
    },
    /// `RunPod` rejected a v2 pod create (HTTP 400) because the requested
    /// hardware has no capacity right now. Transient: callers may retry with
    /// backoff or fall back to the next catalogue combination.
    #[error("{message}")]
    Capacity {
        /// The formatted error message (operation + HTTP 400 + detail).
        message: String,
        /// The raw response body, for diagnostics.
        body: Option<Value>,
    },
}

/// The migration prompt is beta and undocumented, and `RunPod` has changed its
/// wording over time. Known variants for the same condition (pod pinned to a
/// host that no longer has a free GPU):
///
/// * console/API prompt: "please migrate your pod ..."
/// * REST v1 start 500: "There are not enough free GPUs on the host machine
///   to start this pod"
///
/// Matching on stems keeps the detection tolerant to further rewordings.
static MIGRATION_PATTERN: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)migrat|not enough free gpus").unwrap());

/// v2 create 400s that mean "no capacity" rather than a bad request body.
/// Verified wording (2026-08): "There are no longer any instances available
/// with the requested specifications."
static CAPACITY_PATTERN: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)no longer any instances available|not enough (free gpus|capacity)|no (more )?capacity|out of capacity",
    )
    .unwrap()
});

/// A `RunPod` pod (creation-spec fields included, so a pod that must be
/// replaced can be recreated with identical hardware).
#[derive(Debug, Clone, PartialEq)]
pub struct Pod {
    pub id: String,
    pub name: String,
    pub desired_status: String,
    pub image: String,
    pub template_id: Option<String>,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub args: String,
    /// The `RunPod` gpuType id (e.g. "NVIDIA A40").
    pub gpu_type: String,
    pub gpu_count: i64,
    pub container_disk_gb: Option<i64>,
    /// A network-volume id (data survives a replace), not a container disk.
    pub volume_id: String,
    pub datacenter: String,
}

impl Pod {
    /// Parse a v1 pod object (GET /pods, GET /pods/{id}).
    pub fn from_api(payload: &Value) -> Self {
        let obj = payload.as_object().expect("pod payload is an object");
        let disk = obj.get("containerDiskInGb").and_then(Value::as_i64);
        let gpu_count = obj.get("gpuCount").and_then(Value::as_i64).unwrap_or(1);
        Self {
            id: str_field(obj, "id"),
            name: str_field(obj, "name"),
            desired_status: str_field(obj, "desiredStatus"),
            image: str_field(obj, "image"),
            template_id: opt_str_field(obj, "templateId"),
            env: str_map_field(obj, "env"),
            ports: str_list_field(obj, "ports"),
            args: str_field(obj, "args"),
            gpu_type: str_field(obj, "gpuType"),
            gpu_count,
            container_disk_gb: disk,
            // v1 GET reports the attached network volume as "networkVolumeId"
            // (and "volumeId" stays null); "volumeId" is the create-API
            // spelling. Reading both keeps a replace from silently dropping
            // the volume — i.e. the pod's on-disk data such as HF model
            // weights — so recreation doesn't re-download them.
            volume_id: first_nonempty(&[
                opt_str_field(obj, "networkVolumeId"),
                opt_str_field(obj, "volumeId"),
            ]),
            datacenter: str_field(obj, "datacenter"),
        }
    }

    /// Parse a v2 pod object (POST /v2/pods, GET /v2/pods/{id}).
    ///
    /// v2 spellings win; v1 spellings are accepted as fallbacks so tests (and
    /// mixed v1/v2 responses) keep working: status vs desiredStatus,
    /// gpu{id,count} vs gpuType/gpuCount, dataCenterId vs datacenter,
    /// mounts.network vs networkVolumeId, disk vs containerDiskInGb.
    pub fn from_api_v2(payload: &Value) -> Self {
        let obj = payload.as_object().expect("pod payload is an object");
        let mut gpu_type = str_field(obj, "gpuType");
        let mut gpu_count = obj.get("gpuCount").and_then(Value::as_i64);
        if let Some(gpu) = obj.get("gpu").and_then(|v| v.as_object()) {
            if let Some(id) = gpu.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    gpu_type = id.to_string();
                }
            }
            if let Some(count) = gpu.get("count").and_then(Value::as_i64) {
                gpu_count = Some(count);
            }
        }
        let mut volume_id = String::new();
        if let Some(mounts) = obj.get("mounts").and_then(|v| v.as_object()) {
            if let Some(network) = mounts.get("network").and_then(|v| v.as_array()) {
                if let Some(first) = network.first().and_then(|v| v.as_object()) {
                    volume_id = first
                        .get("volumeId")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                }
            }
        }
        let mut template_id = opt_str_field(obj, "templateId");
        if let Some(template) = obj.get("template") {
            if let Some(tid) = template
                .as_object()
                .and_then(|t| t.get("id"))
                .and_then(|v| v.as_str())
            {
                if !tid.is_empty() {
                    template_id = Some(tid.to_string());
                }
            } else if let Some(tid) = template.as_str() {
                if !tid.is_empty() {
                    template_id = Some(tid.to_string());
                }
            }
        }
        let disk = obj
            .get("disk")
            .and_then(Value::as_i64)
            .or_else(|| obj.get("containerDiskInGb").and_then(Value::as_i64));
        let desired_status = obj
            .get("status")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| obj.get("desiredStatus").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        Self {
            id: str_field(obj, "id"),
            name: str_field(obj, "name"),
            desired_status,
            image: str_field(obj, "image"),
            template_id,
            env: str_map_field(obj, "env"),
            ports: str_list_field(obj, "ports"),
            args: str_field(obj, "args"),
            gpu_type,
            gpu_count: gpu_count.unwrap_or(1),
            container_disk_gb: disk,
            volume_id: first_nonempty(&[
                Some(volume_id),
                opt_str_field(obj, "networkVolumeId"),
                opt_str_field(obj, "volumeId"),
            ]),
            datacenter: first_nonempty(&[
                opt_str_field(obj, "dataCenterId"),
                opt_str_field(obj, "datacenter"),
            ]),
        }
    }

    /// The HTTP ports (entries whose protocol is `http`, parsed to int).
    pub fn http_ports(&self) -> Vec<u16> {
        let mut result = Vec::new();
        for entry in &self.ports {
            let Some((port, protocol)) = entry.split_once('/') else {
                continue;
            };
            if protocol.eq_ignore_ascii_case("http") {
                if let Ok(p) = port.parse::<u16>() {
                    result.push(p);
                }
            }
        }
        result
    }
}

/// A `RunPod` template.
#[derive(Debug, Clone, PartialEq)]
pub struct Template {
    pub id: String,
    pub name: String,
    pub image_name: String,
    pub env: BTreeMap<String, String>,
    pub ports: Vec<String>,
    pub is_serverless: bool,
    pub args: String,
}

impl Template {
    /// Parse a template object (GET /templates).
    pub fn from_api(payload: &Value) -> Self {
        let obj = payload.as_object().expect("template payload is an object");
        Self {
            id: str_field(obj, "id"),
            name: str_field(obj, "name"),
            image_name: str_field(obj, "imageName"),
            env: str_map_field(obj, "env"),
            ports: str_list_field(obj, "ports"),
            is_serverless: obj
                .get("isServerless")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            args: str_field(obj, "args"),
        }
    }
}

/// Normalize a model name to its slug (spec section 7.1).
///
/// Port of `proxy/runpod_api.py::model_slug`: casefold, replace every run of
/// non-alphanumeric characters with a single `-`, strip leading/trailing `-`.
///
/// Divergence: Python `casefold()` may expand some non-ASCII characters
/// (e.g. `ß` -> `ss`); Rust `to_lowercase()` does not. Identical for ASCII,
/// which covers all realistic model names.
pub fn model_slug(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.to_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
        } else if !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_matches('-').to_string()
}

/// Whether a pod/template (name, image, env, args) matches a model slug.
pub fn matches_model(
    name: &str,
    image: &str,
    env: &BTreeMap<String, String>,
    model: &str,
    args: &str,
) -> bool {
    let wanted = model_slug(model);
    if wanted.is_empty() {
        return false;
    }
    if model_slug(name).contains(&wanted) || model_slug(image).contains(&wanted) {
        return true;
    }
    if !args.is_empty() && model_slug(args).contains(&wanted) {
        return true;
    }
    env.values()
        .any(|value| model_slug(value).contains(&wanted))
}

/// Whether a pod matches a model.
pub fn pod_matches_model(pod: &Pod, model: &str) -> bool {
    matches_model(&pod.name, &pod.image, &pod.env, model, &pod.args)
}

/// Whether a template matches a model.
pub fn template_matches_model(template: &Template, model: &str) -> bool {
    matches_model(
        &template.name,
        &template.image_name,
        &template.env,
        model,
        &template.args,
    )
}

/// The `RunPod` REST client.
#[derive(Clone)]
pub struct RunpodApi {
    rest_url: String,
    v2_url: String,
    api_key: String,
    client: reqwest::Client,
}

/// Options for a single REST call.
#[derive(Default)]
// Four independent orthogonal flags (API base, 404 handling, migration and
// capacity detection); not a state machine, so bools are the right shape.
#[allow(clippy::struct_excessive_bools)]
struct RequestOpts {
    /// Use the v2 base (`RUNPOD_AVAILABILITY_URL`) instead of the v1 base.
    base_v2: bool,
    /// Return `None` (not an error) on HTTP 404.
    none_on_404: bool,
    /// Scan the body for a migration prompt (2xx and non-2xx).
    check_migration: bool,
    /// Treat a capacity-worded 400 as `RunpodApiError::Capacity`.
    capacity_400: bool,
    /// Query parameters.
    params: Option<Vec<(String, String)>>,
    /// JSON request body.
    json: Option<Value>,
}

impl RunpodApi {
    /// Build a client over a shared `reqwest::Client`.
    pub fn new(rest_url: &str, v2_url: &str, api_key: &str, client: reqwest::Client) -> Self {
        Self {
            rest_url: rest_url.trim_end_matches('/').to_string(),
            v2_url: v2_url.trim_end_matches('/').to_string(),
            api_key: api_key.to_string(),
            client,
        }
    }

    /// The migration prompt from a `RunPod` response body, or `None`.
    pub fn migration_message(body: Option<&Value>) -> Option<String> {
        let obj = body?.as_object()?;
        for key in ["message", "reason", "detail", "error", "statusMessage"] {
            if let Some(value) = obj.get(key).and_then(|v| v.as_str()) {
                if MIGRATION_PATTERN.is_match(value) {
                    return Some(value.to_string());
                }
            }
        }
        None
    }

    /// Perform a REST call, returning the response body bytes (or `None` for a
    /// 404 with `none_on_404`). Non-2xx responses are turned into typed errors
    /// (migration / capacity / general) before the body is returned.
    async fn request(
        &self,
        method: Method,
        path: &str,
        operation: &str,
        opts: RequestOpts,
    ) -> Result<Option<Vec<u8>>, RunpodApiError> {
        let base = if opts.base_v2 {
            &self.v2_url
        } else {
            &self.rest_url
        };
        let url = format!(
            "{}/{}",
            base.trim_end_matches('/'),
            path.trim_start_matches('/')
        );
        let mut req = self.client.request(method, &url);
        req = req.header("Authorization", format!("Bearer {}", self.api_key));
        if let Some(params) = &opts.params {
            req = req.query(params);
        }
        if let Some(json) = &opts.json {
            req = req.json(json);
        }
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%operation, error = %e, "runpod api request failed");
                return Err(RunpodApiError::General(format!("{operation} failed: {e}")));
            }
        };
        let status = response.status().as_u16();
        if status == 404 && opts.none_on_404 {
            return Ok(None);
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| RunpodApiError::General(format!("{operation} failed to read body: {e}")))?
            .to_vec();
        let body: Option<Value> = if bytes.is_empty() {
            None
        } else {
            serde_json::from_slice(&bytes).ok()
        };
        if !(200..300).contains(&status) {
            let detail = error_detail(body.as_ref());
            if opts.check_migration {
                if let Some(message) = Self::migration_message(body.as_ref()) {
                    tracing::warn!(%operation, status, %message, "runpod api migration prompt");
                    return Err(RunpodApiError::Migration {
                        message: format!("{operation} returned HTTP {status}: {message}"),
                        body,
                    });
                }
            }
            if opts.capacity_400 && status == 400 {
                if let Some(d) = &detail {
                    if CAPACITY_PATTERN.is_match(d) {
                        tracing::warn!(%operation, %d, "runpod api capacity");
                        return Err(RunpodApiError::Capacity {
                            message: format!("{operation} returned HTTP 400: {d}"),
                            body,
                        });
                    }
                }
            }
            tracing::warn!(%operation, status, "runpod api non-2xx");
            let detail_str = detail.map(|d| format!(": {d}")).unwrap_or_default();
            return Err(RunpodApiError::General(format!(
                "{operation} returned HTTP {status}{detail_str}"
            )));
        }
        if opts.check_migration {
            // The prompt is beta/undocumented: it may also arrive on a 2xx
            // start response, so scan those bodies too.
            if let Some(message) = Self::migration_message(body.as_ref()) {
                return Err(RunpodApiError::Migration {
                    message: format!("{operation} returned HTTP {status}: {message}"),
                    body,
                });
            }
        }
        Ok(Some(bytes))
    }

    fn array(bytes: &[u8], operation: &str) -> Result<Vec<Value>, RunpodApiError> {
        let body: Value = serde_json::from_slice(bytes)
            .map_err(|_| RunpodApiError::General(format!("{operation} returned invalid JSON")))?;
        let items = body
            .as_array()
            .ok_or_else(|| {
                RunpodApiError::General(format!("{operation} returned a non-array response"))
            })?
            .clone();
        Ok(items)
    }

    /// Parse a JSON object body, or an error.
    fn object(bytes: &[u8], operation: &str) -> Result<Value, RunpodApiError> {
        let body: Value = serde_json::from_slice(bytes)
            .map_err(|_| RunpodApiError::General(format!("{operation} returned invalid JSON")))?;
        if !body.is_object() {
            return Err(RunpodApiError::General(format!(
                "{operation} returned a non-object response"
            )));
        }
        Ok(body)
    }

    /// List pods, optionally filtered by desired status.
    pub async fn list_pods(
        &self,
        desired_status: Option<&str>,
    ) -> Result<Vec<Pod>, RunpodApiError> {
        let params = desired_status.map(|s| vec![("desiredStatus".to_string(), s.to_string())]);
        let bytes = self
            .request(
                Method::GET,
                "/pods",
                "list pods",
                RequestOpts {
                    params,
                    ..Default::default()
                },
            )
            .await?
            .expect("list pods is not none_on_404");
        let items = Self::array(&bytes, "list pods")?;
        Ok(items
            .into_iter()
            .filter(Value::is_object)
            .map(|v| Pod::from_api(&v))
            .collect())
    }

    /// Get a pod by id, or `None` when it does not exist (404).
    pub async fn get_pod(&self, pod_id: &str) -> Result<Option<Pod>, RunpodApiError> {
        let operation = format!("get pod {pod_id}");
        let bytes = self
            .request(
                Method::GET,
                &format!("/pods/{pod_id}"),
                &operation,
                RequestOpts {
                    none_on_404: true,
                    ..Default::default()
                },
            )
            .await?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let body = Self::object(&bytes, &operation)?;
        Ok(Some(Pod::from_api(&body)))
    }

    /// Start a pod. A start blocked by the "please migrate" prompt surfaces as
    /// `RunpodApiError::Migration`, not a generic retriable error.
    pub async fn start_pod(&self, pod_id: &str) -> Result<(), RunpodApiError> {
        self.request(
            Method::POST,
            &format!("/pods/{pod_id}/start"),
            &format!("start pod {pod_id}"),
            RequestOpts {
                check_migration: true,
                ..Default::default()
            },
        )
        .await?;
        Ok(())
    }

    /// Stop a pod.
    pub async fn stop_pod(&self, pod_id: &str) -> Result<(), RunpodApiError> {
        self.request(
            Method::POST,
            &format!("/pods/{pod_id}/stop"),
            &format!("stop pod {pod_id}"),
            RequestOpts::default(),
        )
        .await?;
        Ok(())
    }

    /// Terminate a pod (`RunPod`'s "delete"). Used by the
    /// `RUNPOD_ON_MIGRATE=replace` policy: a pod whose GPUs were stolen by
    /// another user cannot be started, and the beta auto-migration is
    /// unreliable, so terminating + creating a fresh pod is the clean path.
    pub async fn delete_pod(&self, pod_id: &str) -> Result<(), RunpodApiError> {
        self.request(
            Method::DELETE,
            &format!("/pods/{pod_id}"),
            &format!("delete pod {pod_id}"),
            RequestOpts::default(),
        )
        .await?;
        Ok(())
    }

    /// List templates, optionally including public/`RunPod` templates.
    pub async fn list_templates(
        &self,
        include_public: bool,
        include_runpod: bool,
    ) -> Result<Vec<Template>, RunpodApiError> {
        let mut params = Vec::new();
        if include_public {
            params.push(("includePublicTemplates".to_string(), "true".to_string()));
        }
        if include_runpod {
            params.push(("includeRunpodTemplates".to_string(), "true".to_string()));
        }
        let bytes = self
            .request(
                Method::GET,
                "/templates",
                "list templates",
                RequestOpts {
                    params: if params.is_empty() {
                        None
                    } else {
                        Some(params)
                    },
                    ..Default::default()
                },
            )
            .await?
            .expect("list templates is not none_on_404");
        let items = Self::array(&bytes, "list templates")?;
        Ok(items
            .into_iter()
            .filter(Value::is_object)
            .map(|v| Template::from_api(&v))
            .collect())
    }

    /// A v2 network volume by id, or `None` when it does not exist.
    pub async fn get_network_volume(
        &self,
        volume_id: &str,
    ) -> Result<Option<Value>, RunpodApiError> {
        let operation = format!("get network volume {volume_id}");
        let bytes = self
            .request(
                Method::GET,
                &format!("/network-volumes/{volume_id}"),
                &operation,
                RequestOpts {
                    base_v2: true,
                    none_on_404: true,
                    ..Default::default()
                },
            )
            .await?;
        let Some(bytes) = bytes else {
            return Ok(None);
        };
        let body = Self::object(&bytes, &operation)?;
        Ok(Some(body))
    }

    /// Create a v2 network volume (the v2 replacement of v1's `volumeInGb`)
    /// and return its object (id, size, dataCenter, ...).
    pub async fn create_network_volume(
        &self,
        name: &str,
        size: i64,
        datacenter: &str,
    ) -> Result<Value, RunpodApiError> {
        let body = json!({
            "name": name,
            "size": size,
            "dataCenter": datacenter,
        });
        let bytes = self
            .request(
                Method::POST,
                "/network-volumes",
                &format!("create network volume {name}"),
                RequestOpts {
                    base_v2: true,
                    json: Some(body),
                    ..Default::default()
                },
            )
            .await?
            .expect("create network volume is not none_on_404");
        Self::object(&bytes, "create network volume")
    }

    /// Build the v2 create-pod JSON body from a spec.
    fn build_create_pod_body(
        spec: &CreatePodSpec,
        datacenters: &[String],
        volume_id: Option<&str>,
    ) -> Map<String, Value> {
        let mut body = Map::new();
        body.insert("name".into(), json!(spec.name));
        body.insert("cloud".into(), json!(spec.cloud_type.to_uppercase()));
        body.insert(
            "gpu".into(),
            json!({ "id": spec.gpu_type, "count": spec.gpu_count }),
        );
        match &spec.template_id {
            Some(tid) => body.insert("templateId".into(), json!(tid)),
            None => body.insert(
                "image".into(),
                json!(spec.image_name.clone().unwrap_or_default()),
            ),
        };
        if !datacenters.is_empty() {
            body.insert("dataCenterIds".into(), json!(datacenters));
        }
        if !spec.ports.is_empty() {
            body.insert("ports".into(), json!(spec.ports));
        }
        if let Some(env) = &spec.env {
            body.insert("env".into(), json!(env));
        }
        if let Some(disk) = spec.container_disk_gb {
            body.insert("disk".into(), json!(disk));
        }
        if let Some(vol) = volume_id {
            // Attach the (existing or freshly created) network volume so its
            // data — e.g. HF model weights — survives a terminate-and-recreate.
            body.insert(
                "mounts".into(),
                json!({ "network": [{ "volumeId": vol, "path": spec.volume_mount_path }] }),
            );
        }
        body
    }

    /// Create a pod via the v2 API (POST /v2/pods).
    ///
    /// Pod creation lives on v2 because the v1 create schema has no
    /// `volumeId` (a v1 create with a network volume 400s). v2 specifics: the
    /// GPU is a single `{id, count}` object (no priority list), the network
    /// volume goes under `mounts.network` and is datacenter-locked (this
    /// method pins the volume's own datacenter when the caller did not), and a
    /// capacity shortage arrives as a 400 whose body wording is matched into
    /// `RunpodApiError::Capacity` (schema violations are 422).
    pub async fn create_pod(&self, spec: &CreatePodSpec) -> Result<Pod, RunpodApiError> {
        if spec.template_id.is_none() && spec.image_name.is_none() {
            return Err(RunpodApiError::General(
                "create pod requires template_id or image_name".into(),
            ));
        }
        if spec.gpu_type.is_empty() {
            return Err(RunpodApiError::General(
                "create pod requires gpu_type (the v2 API has no default GPU; set \
                 RUNPOD_GPU_TYPE_IDS or a catalogue gpus entry)"
                    .into(),
            ));
        }
        let mut datacenters = spec.datacenter_ids.clone();
        if let Some(volume_id) = &spec.volume_id {
            if !volume_id.is_empty() && datacenters.is_empty() {
                // A network volume cannot be attached to a pod in another
                // datacenter, so fetch the volume and pin its home DC.
                let volume = self.get_network_volume(volume_id).await?;
                let Some(volume) = volume else {
                    return Err(RunpodApiError::General(format!(
                        "network volume {volume_id} not found; it may have been \
                         deleted with its pod"
                    )));
                };
                if let Some(dc) = volume.get("dataCenter").and_then(|v| v.as_str()) {
                    if !dc.is_empty() {
                        datacenters = vec![dc.to_string()];
                    }
                }
            }
        }
        let mut volume_id = spec.volume_id.clone().filter(|v| !v.is_empty());
        if let Some(volume_gb) = spec.volume_gb {
            // A fresh volume needs a home datacenter before the pod exists.
            if datacenters.is_empty() {
                return Err(RunpodApiError::General(
                    "volume_gb requires datacenter_ids: a new network volume must \
                     be created in a specific datacenter"
                        .into(),
                ));
            }
            let volume = self
                .create_network_volume(&format!("{}-volume", spec.name), volume_gb, &datacenters[0])
                .await?;
            volume_id = volume
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            if volume_id.is_none() {
                return Err(RunpodApiError::General(
                    "create network volume returned no id".into(),
                ));
            }
        }

        let body = Self::build_create_pod_body(spec, &datacenters, volume_id.as_deref());
        let bytes = self
            .request(
                Method::POST,
                "/pods",
                "create pod",
                RequestOpts {
                    base_v2: true,
                    capacity_400: true,
                    json: Some(Value::Object(body)),
                    ..Default::default()
                },
            )
            .await?
            .expect("create pod is not none_on_404");
        let payload = Self::object(&bytes, "create pod")?;
        Ok(Pod::from_api_v2(&payload))
    }
}

/// The creation spec for a v2 pod (POST /v2/pods).
#[derive(Debug, Clone, Default)]
pub struct CreatePodSpec {
    pub name: String,
    pub template_id: Option<String>,
    pub image_name: Option<String>,
    pub gpu_type: String,
    pub gpu_count: i64,
    pub cloud_type: String,
    pub ports: Vec<String>,
    pub env: Option<BTreeMap<String, String>>,
    pub container_disk_gb: Option<i64>,
    pub volume_gb: Option<i64>,
    pub volume_id: Option<String>,
    pub volume_mount_path: String,
    pub datacenter_ids: Vec<String>,
}

/// The `RunPod` error text from a response body, for logs and errors.
///
/// v1 error bodies look like `{"error": ..., "status": ...}`; v2-style ones
/// use `detail`/`title`. Returns `None` when no recognizable message is
/// present.
fn error_detail(body: Option<&Value>) -> Option<String> {
    let obj = body?.as_object()?;
    for key in ["message", "detail", "error", "reason"] {
        if let Some(value) = obj.get(key).and_then(|v| v.as_str()) {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

/// A string field, or `""` when missing/non-string.
fn str_field(obj: &Map<String, Value>, key: &str) -> String {
    obj.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// An optional string field, or `None` when missing/non-string/empty.
fn opt_str_field(obj: &Map<String, Value>, key: &str) -> Option<String> {
    obj.get(key)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// A string-map field, or an empty map.
fn str_map_field(obj: &Map<String, Value>, key: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    if let Some(map) = obj.get(key).and_then(|v| v.as_object()) {
        for (k, v) in map {
            if let Some(s) = v.as_str() {
                out.insert(k.clone(), s.to_string());
            }
        }
    }
    out
}

/// A string-list field, or an empty list.
fn str_list_field(obj: &Map<String, Value>, key: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Some(list) = obj.get(key).and_then(|v| v.as_array()) {
        for item in list {
            if let Some(s) = item.as_str() {
                out.push(s.to_string());
            }
        }
    }
    out
}

/// The first non-empty string in the list, or `""`.
fn first_nonempty(candidates: &[Option<String>]) -> String {
    candidates
        .iter()
        .find_map(|c| c.as_ref().filter(|s| !s.is_empty()).cloned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use httpmock::prelude::*;

    #[test]
    fn slug_lowercases_and_keeps_alphanumerics() {
        assert_eq!(model_slug("Qwen/Qwen3.8-27B-FP8"), "qwen-qwen3-8-27b-fp8");
        assert_eq!(model_slug("meta/Llama-3-70B"), "meta-llama-3-70b");
    }

    #[test]
    fn slug_collapses_non_alphanumeric_runs() {
        assert_eq!(model_slug("a__b--c  d"), "a-b-c-d");
        assert_eq!(model_slug("!!hello!!"), "hello");
    }

    #[test]
    fn slug_strips_edge_dashes() {
        assert_eq!(model_slug("-leading"), "leading");
        assert_eq!(model_slug("trailing-"), "trailing");
        assert_eq!(model_slug("---"), "");
    }

    #[test]
    fn slug_empty_and_blank() {
        assert_eq!(model_slug(""), "");
        assert_eq!(model_slug("   "), "");
    }

    #[test]
    fn pod_from_api_v1_reads_network_volume_and_gpu() {
        let payload = json!({
            "id": "pod-1",
            "name": "my-pod",
            "desiredStatus": "RUNNING",
            "image": "img:latest",
            "templateId": "tpl-1",
            "env": {"MODEL": "qwen"},
            "ports": ["8000/http"],
            "args": "--model qwen",
            "gpuType": "NVIDIA A40",
            "gpuCount": 2,
            "containerDiskInGb": 100,
            "networkVolumeId": "vol-1",
            "volumeId": null,
            "datacenter": "US_WEST"
        });
        let pod = Pod::from_api(&payload);
        assert_eq!(pod.id, "pod-1");
        assert_eq!(pod.desired_status, "RUNNING");
        assert_eq!(pod.template_id.as_deref(), Some("tpl-1"));
        assert_eq!(pod.gpu_type, "NVIDIA A40");
        assert_eq!(pod.gpu_count, 2);
        assert_eq!(pod.container_disk_gb, Some(100));
        // networkVolumeId wins over the null volumeId.
        assert_eq!(pod.volume_id, "vol-1");
        assert_eq!(pod.datacenter, "US_WEST");
        assert_eq!(pod.http_ports(), vec![8000]);
    }

    #[test]
    fn pod_from_api_v2_prefers_v2_spellings() {
        let payload = json!({
            "id": "pod-2",
            "name": "my-pod",
            "status": "RUNNING",
            "image": "img:latest",
            "gpu": {"id": "NVIDIA H100", "count": 4},
            "disk": 200,
            "mounts": {"network": [{"volumeId": "vol-2", "path": "/workspace"}]},
            "template": {"id": "tpl-2"},
            "dataCenterId": "US_EAST"
        });
        let pod = Pod::from_api_v2(&payload);
        assert_eq!(pod.desired_status, "RUNNING");
        assert_eq!(pod.gpu_type, "NVIDIA H100");
        assert_eq!(pod.gpu_count, 4);
        assert_eq!(pod.container_disk_gb, Some(200));
        assert_eq!(pod.volume_id, "vol-2");
        assert_eq!(pod.template_id.as_deref(), Some("tpl-2"));
        assert_eq!(pod.datacenter, "US_EAST");
    }

    #[test]
    fn pod_from_api_v2_falls_back_to_v1_spellings() {
        let payload = json!({
            "id": "pod-3",
            "desiredStatus": "EXITED",
            "gpuType": "NVIDIA A100",
            "gpuCount": 1,
            "containerDiskInGb": 50,
            "volumeId": "vol-3",
            "datacenter": "EU_CENTRAL"
        });
        let pod = Pod::from_api_v2(&payload);
        assert_eq!(pod.desired_status, "EXITED");
        assert_eq!(pod.gpu_type, "NVIDIA A100");
        assert_eq!(pod.gpu_count, 1);
        assert_eq!(pod.container_disk_gb, Some(50));
        assert_eq!(pod.volume_id, "vol-3");
        assert_eq!(pod.datacenter, "EU_CENTRAL");
    }

    #[test]
    fn http_ports_filters_http_protocol() {
        let payload = json!({
            "ports": ["8000/http", "9000/tcp", "bad/http", "8080/HTTP"]
        });
        let pod = Pod::from_api(&payload);
        assert_eq!(pod.http_ports(), vec![8000, 8080]);
    }

    #[test]
    fn template_from_api_parses_fields() {
        let payload = json!({
            "id": "tpl-1",
            "name": "qwen-tpl",
            "imageName": "img:latest",
            "env": {"MODEL": "qwen"},
            "ports": ["8000/http"],
            "isServerless": true,
            "args": "--model qwen"
        });
        let t = Template::from_api(&payload);
        assert_eq!(t.id, "tpl-1");
        assert_eq!(t.image_name, "img:latest");
        assert!(t.is_serverless);
        assert_eq!(t.args, "--model qwen");
    }

    #[test]
    fn matches_model_checks_name_image_env_args() {
        let env = BTreeMap::new();
        assert!(matches_model(
            "qwen-qwen3-8-pod",
            "",
            &env,
            "Qwen/Qwen3.8",
            ""
        ));
        assert!(matches_model("", "qwen-image", &env, "qwen", ""));
        let mut env2 = BTreeMap::new();
        env2.insert("MODEL".into(), "qwen".into());
        assert!(matches_model("", "", &env2, "qwen", ""));
        assert!(matches_model("", "", &env, "qwen", "--model qwen"));
        assert!(!matches_model("llama-pod", "", &env, "qwen", ""));
        assert!(!matches_model("anything", "", &env, "", ""));
    }

    #[test]
    fn pod_and_template_matchers_delegate() {
        let pod = Pod::from_api(&json!({"name": "qwen-pod", "image": ""}));
        assert!(pod_matches_model(&pod, "qwen"));
        let t = Template::from_api(&json!({"name": "", "imageName": "qwen-img"}));
        assert!(template_matches_model(&t, "qwen"));
    }

    fn api(server: &MockServer) -> RunpodApi {
        RunpodApi::new(
            &server.url("/v1"),
            &server.url("/v2"),
            "test-key",
            crate::test_client(),
        )
    }

    #[tokio::test]
    async fn get_pod_200_returns_pod() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(200)
                .json_body(json!({"id": "pod-1", "desiredStatus": "RUNNING"}));
        });
        let pod = api(&server).get_pod("pod-1").await.unwrap();
        assert_eq!(pod.unwrap().id, "pod-1");
    }

    #[tokio::test]
    async fn get_pod_404_returns_none() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/missing");
            then.status(404);
        });
        let pod = api(&server).get_pod("missing").await.unwrap();
        assert!(pod.is_none());
    }

    #[tokio::test]
    async fn get_pod_500_is_general_error_with_detail() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET).path("/v1/pods/pod-1");
            then.status(500).json_body(json!({"message": "boom"}));
        });
        let err = api(&server).get_pod("pod-1").await.unwrap_err();
        match err {
            RunpodApiError::General(msg) => {
                assert!(msg.contains("HTTP 500"), "{msg}");
                assert!(msg.contains("boom"), "{msg}");
            }
            other => panic!("expected General, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn list_pods_filters_by_status_and_parses_array() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(GET)
                .path("/v1/pods")
                .query_param("desiredStatus", "RUNNING");
            then.status(200)
                .json_body(json!([{"id": "a"}, {"id": "b"}, "not-an-object"]));
        });
        let pods = api(&server).list_pods(Some("RUNNING")).await.unwrap();
        assert_eq!(pods.len(), 2);
        assert_eq!(pods[0].id, "a");
    }

    #[tokio::test]
    async fn start_pod_migration_on_500() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/start");
            then.status(500)
                .json_body(json!({"message": "There are not enough free GPUs on the host machine to start this pod"}));
        });
        let err = api(&server).start_pod("pod-1").await.unwrap_err();
        match err {
            RunpodApiError::Migration { message, .. } => {
                assert!(message.contains("not enough free GPUs"), "{message}");
            }
            other => panic!("expected Migration, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn start_pod_migration_on_2xx_body() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v1/pods/pod-1/start");
            then.status(200)
                .json_body(json!({"statusMessage": "please migrate your pod to a new machine"}));
        });
        let err = api(&server).start_pod("pod-1").await.unwrap_err();
        assert!(matches!(err, RunpodApiError::Migration { .. }));
    }

    #[tokio::test]
    async fn create_pod_capacity_400() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v2/pods");
            then.status(400)
                .json_body(json!({"detail": "There are no longer any instances available with the requested specifications."}));
        });
        let spec = CreatePodSpec {
            name: "p".into(),
            image_name: Some("img".into()),
            gpu_type: "NVIDIA A40".into(),
            ..Default::default()
        };
        let err = api(&server).create_pod(&spec).await.unwrap_err();
        assert!(matches!(err, RunpodApiError::Capacity { .. }));
    }

    #[tokio::test]
    async fn create_pod_requires_gpu_type() {
        let server = MockServer::start();
        let spec = CreatePodSpec {
            name: "p".into(),
            image_name: Some("img".into()),
            ..Default::default()
        };
        let err = api(&server).create_pod(&spec).await.unwrap_err();
        assert!(matches!(err, RunpodApiError::General(_)));
    }

    #[tokio::test]
    async fn create_pod_success_parses_v2() {
        let server = MockServer::start();
        server.mock(|when, then| {
            when.method(POST).path("/v2/pods");
            then.status(200)
                .json_body(json!({"id": "fresh-1", "status": "STARTING", "gpu": {"id": "NVIDIA A40", "count": 1}}));
        });
        let spec = CreatePodSpec {
            name: "p".into(),
            image_name: Some("img".into()),
            gpu_type: "NVIDIA A40".into(),
            ..Default::default()
        };
        let pod = api(&server).create_pod(&spec).await.unwrap();
        assert_eq!(pod.id, "fresh-1");
        assert_eq!(pod.gpu_type, "NVIDIA A40");
    }

    #[tokio::test]
    async fn transport_error_is_general() {
        // Point at a closed port to force a transport error.
        let api = RunpodApi::new(
            "http://127.0.0.1:1/v1",
            "http://127.0.0.1:1/v2",
            "k",
            crate::test_client(),
        );
        let err = api.get_pod("x").await.unwrap_err();
        assert!(matches!(err, RunpodApiError::General(_)));
    }
}
